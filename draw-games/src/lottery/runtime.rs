//! Like card-room actors: one owner, memory commands, frozen FIFO writes.
//! A normal stop drains Ordered; hard recovery restores only its committed prefix.
use super::{
    handlers::Error,
    rules::{self, Entry},
    store::{self, NumberBuy, PeriodRow, Purchase, TicketRow},
    Game,
};
use crate::db_writer::PgPool;
use crate::request_timing::{lottery_span, LotteryStage};
use crate::{
    beans_read_memory::{self, PendingEffect},
    write_behind::Ordered,
};
use chrono::{DateTime, Utc};
use rand::{rngs::OsRng, RngCore};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use sqlx::Row;
use std::{
    collections::{BTreeMap, HashMap, VecDeque},
    sync::{
        atomic::{AtomicU64, AtomicUsize, Ordering},
        Arc, Mutex, OnceLock, Weak,
    },
    time::Duration,
};
use uuid::Uuid;
#[path = "runtime/persistence.rs"]
mod persistence;
#[path = "runtime/published.rs"]
mod published;
#[path = "runtime/socket.rs"]
pub mod socket;
const MAX_ROWS: usize = 2_000_000;
// Accepted and published projections each get half the former 512 MiB budget.
const MAX_BYTES: usize = 256 * 1024 * 1024;
const MAX_QUEUED_BYTES: usize = 64 * 1024 * 1024;
const PAGE: i64 = 500;
const PERIOD_SLOTS: usize = 3;

#[derive(Clone, Serialize, Deserialize)]
struct OwnedTicket {
    user: Uuid,
    game: String,
    card: TicketRow,
}
#[derive(Clone, Serialize, Deserialize)]
struct Receipt {
    game: String,
    record: Uuid,
    hash: String,
    tickets: Vec<Uuid>,
    charged: i64,
    before: i64,
    after: i64,
}
#[derive(Clone, Serialize, Deserialize)]
struct Offer {
    user: Uuid,
    id: Uuid,
    seed: String,
    fresh: bool,
    previous: Option<Uuid>,
}
#[derive(Clone, Serialize, Deserialize)]
struct Winner {
    user: Option<Uuid>,
    name: String,
    tier: i16,
    prize: i64,
}
#[derive(Clone, Serialize, Deserialize)]
struct Event {
    id: Uuid,
    game: String,
    owner: Option<Uuid>,
    at: DateTime<Utc>,
    kind: String,
    cards: Vec<OwnedTicket>,
    period: Option<PeriodRow>,
    receipt: Option<Receipt>,
    effect: Option<PendingEffect>,
    before: i64,
    bonus: i64,
    max_cards: Vec<Uuid>,
    offer: Option<Offer>,
    claim: Option<Uuid>,
}
impl Event {
    fn new(game: Game, owner: Option<Uuid>, kind: &str, id: Uuid) -> Self {
        Self {
            id,
            game: game.as_str().into(),
            owner,
            at: Utc::now(),
            kind: kind.into(),
            cards: vec![],
            period: None,
            receipt: None,
            effect: None,
            before: 0,
            bonus: 0,
            max_cards: vec![],
            offer: None,
            claim: None,
        }
    }
}
struct Job {
    event: Event,
    work: Option<crate::account_purge::RequiredWork>,
    bytes: usize,
    committed: tokio::sync::watch::Sender<Option<Arc<Vec<OwnedTicket>>>>,
}

struct CommitReceipt(tokio::sync::watch::Receiver<Option<Arc<Vec<OwnedTicket>>>>);
impl CommitReceipt {
    async fn wait(mut self) -> Result<Arc<Vec<OwnedTicket>>, Error> {
        tokio::time::timeout(Duration::from_secs(2), async {
            loop {
                if let Some(cards) = self.0.borrow_and_update().clone() {
                    return Ok(cards);
                }
                self.0.changed().await.map_err(|_| unavailable())?;
            }
        })
        .await
        .map_err(|_| unavailable())?
    }
    fn complete() -> Self {
        Self(tokio::sync::watch::channel(Some(Arc::new(Vec::new()))).1)
    }
}

fn pending_receipt(state: &State, id: Uuid) -> CommitReceipt {
    accepted(state)
        .iter()
        .find(|job| job.event.id == id)
        .map(|job| CommitReceipt(job.committed.subscribe()))
        .unwrap_or_else(CommitReceipt::complete)
}

#[derive(Clone)]
pub(super) struct Notice {
    pub(super) owner: Option<Uuid>,
    pub(super) game: String,
    pub(super) revision: u64,
}
#[derive(Clone, Default)]
struct Data {
    revision: u64,
    durable: u64,
    bytes: usize,
    periods: BTreeMap<(String, i64), PeriodRow>,
    tickets: HashMap<Uuid, OwnedTicket>,
    by_user: HashMap<(Uuid, String), Vec<Uuid>>,
    by_period: HashMap<(String, i64), Vec<Uuid>>,
    recent: HashMap<String, VecDeque<Uuid>>,
    receipts: HashMap<(Uuid, Uuid), Receipt>,
    offers: HashMap<Uuid, Offer>,
    winners: HashMap<(String, i64), Vec<Winner>>,
    presence: HashMap<(String, Uuid), DateTime<Utc>>,
    scratch_owner: Option<Uuid>,
}

struct State {
    data: Mutex<Option<Data>>,
    published: Mutex<Option<published::Published>>,
    writer: Mutex<Option<Ordered<Option<Arc<Job>>>>>,
    loader: tokio::sync::Mutex<()>,
    commands: tokio::sync::Mutex<()>,
    changed: tokio::sync::broadcast::Sender<Notice>,
    queued: AtomicUsize,
    /// Jobs published to the writer and not yet committed, in FIFO order.
    accepted: Mutex<VecDeque<Arc<Job>>>,
    /// Revision a rehydrated ring starts from, so clients on the same
    /// runtime_epoch never discard its views as older than what they hold.
    floor: AtomicU64,
    /// The writer's terminal error for the event it stopped at.
    refused: Mutex<Option<String>>,
    /// Accepted jobs that do not depend on a refused event, waiting to be
    /// handed to the next writer by [`hydrate`].
    resume: Mutex<Vec<Arc<Job>>>,
    /// Wakes [`run`]: a write committed (purchase, period, draw or settlement,
    /// so a slot may be free or settlement may continue) or was refused.
    wake: tokio::sync::Notify,
    /// The last retention pass deleted a full batch: run the next one soon.
    retention_more: std::sync::atomic::AtomicBool,
}
fn state(pool: &PgPool) -> Arc<State> {
    type Pools = HashMap<usize, (Weak<sqlx::postgres::PgConnectOptions>, Arc<State>)>;
    static POOLS: OnceLock<Mutex<Pools>> = OnceLock::new();
    let mut pools = POOLS
        .get_or_init(Default::default)
        .lock()
        .unwrap_or_else(|e| e.into_inner());
    pools.retain(|_, (w, _)| w.strong_count() > 0);
    pools
        .entry(crate::live_membership::pool_id(pool))
        .or_insert_with(|| {
            (
                Arc::downgrade(&pool.connect_options()),
                Arc::new(State {
                    data: Mutex::new(None),
                    published: Mutex::new(None),
                    writer: Mutex::new(None),
                    loader: tokio::sync::Mutex::new(()),
                    commands: tokio::sync::Mutex::new(()),
                    changed: tokio::sync::broadcast::channel(128).0,
                    queued: AtomicUsize::new(0),
                    accepted: Mutex::new(VecDeque::new()),
                    floor: AtomicU64::new(0),
                    refused: Mutex::new(None),
                    resume: Mutex::new(Vec::new()),
                    wake: tokio::sync::Notify::new(),
                    retention_more: std::sync::atomic::AtomicBool::new(false),
                }),
            )
        })
        .1
        .clone()
}
fn unavailable() -> Error {
    Error::Db(sqlx::Error::Protocol("lottery runtime unavailable".into()))
}
fn db_unavailable() -> sqlx::Error {
    sqlx::Error::Protocol("lottery runtime unavailable".into())
}
pub fn ready(pool: &PgPool) -> bool {
    state(pool)
        .data
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .is_some()
}
/// The lottery exists only on the single realtime owner: a process that is
/// not (or not yet) that owner accepts nothing and serves no view.
fn check_owner() -> Result<(), Error> {
    if !crate::write_behind::accepting() || !crate::realtime_mode::enabled() {
        return Err(unavailable());
    }
    Ok(())
}
fn player(pool: &PgPool, user: Uuid) -> Result<(), Error> {
    if crate::player_directory::lookup(pool, user)?.is_some_and(|p| p.active()) {
        Ok(())
    } else {
        Err(unavailable())
    }
}
fn name(pool: &PgPool, user: Uuid) -> String {
    crate::player_directory::lookup(pool, user)
        .ok()
        .flatten()
        .map(|p| store::public_name(user, p.display_name.clone()))
        .unwrap_or_else(|| crate::display_name::fallback_display_name(user))
}
fn resident<T: Serialize>(value: &T) -> usize {
    serde_json::to_vec(value)
        .expect("runtime value serializes")
        .len()
        .saturating_mul(4)
        .saturating_add(256)
}
impl Data {
    fn compact(&mut self) {
        self.by_user.retain(|_, ids| {
            ids.retain(|id| self.tickets.contains_key(id));
            !ids.is_empty()
        });
        self.by_period.retain(|_, ids| {
            ids.retain(|id| self.tickets.contains_key(id));
            !ids.is_empty()
        });
        self.recent.retain(|_, ids| {
            ids.retain(|id| self.tickets.contains_key(id));
            !ids.is_empty()
        });
        self.winners.retain(|key, _| self.periods.contains_key(key));
        self.bytes = persistence::weight(self).expect("runtime values serialize");
    }

    fn retire_durable_periods(&mut self) {
        if self.revision != self.durable {
            return;
        }
        let before = (self.periods.len(), self.tickets.len(), self.receipts.len());
        self.periods.retain(|_, p| p.status != "settled");
        self.tickets.retain(|_, t| {
            t.game == "scratch" || self.periods.contains_key(&(t.game.clone(), t.card.period))
        });
        // Scratch replay windows are retired only by serialized cold preparation.
        self.receipts.retain(|_, receipt| receipt.game == "scratch");
        // Compaction re-serializes the whole working set under the data lock;
        // the runtime loop calls this on every commit, so only pay when a row left.
        if before != (self.periods.len(), self.tickets.len(), self.receipts.len()) {
            self.compact();
        }
    }
    fn insert(&mut self, t: OwnedTicket) {
        let id = t.card.id;
        if !self.tickets.contains_key(&id) {
            self.by_user
                .entry((t.user, t.game.clone()))
                .or_default()
                .push(id);
            self.by_period
                .entry((t.game.clone(), t.card.period))
                .or_default()
                .push(id);
            let recent = self.recent.entry(t.game.clone()).or_default();
            recent.push_front(id);
            recent.truncate(100);
        }
        self.bytes = self
            .bytes
            .saturating_sub(self.tickets.get(&id).map_or(0, resident))
            .saturating_add(resident(&t));
        self.tickets.insert(id, t);
    }
    fn apply(&mut self, event: &Event) {
        if let Some(row) = &event.period {
            let key = (event.game.clone(), row.period);
            self.bytes = self
                .bytes
                .saturating_sub(self.periods.get(&key).map_or(0, resident))
                .saturating_add(resident(row));
            self.periods.insert(key, row.clone());
        }
        for t in &event.cards {
            self.insert(t.clone());
        }
        if let (Some(user), Some(receipt)) = (event.owner, &event.receipt) {
            let key = (user, receipt.record);
            self.bytes = self
                .bytes
                .saturating_sub(self.receipts.get(&key).map_or(0, resident))
                .saturating_add(resident(receipt));
            self.receipts.insert(key, receipt.clone());
        }
        if let Some(user) = event.owner {
            if event.claim.is_some() {
                if let Some(offer) = self.offers.remove(&user) {
                    self.bytes = self.bytes.saturating_sub(resident(&offer));
                }
            }
            if let Some(offer) = &event.offer {
                self.bytes = self
                    .bytes
                    .saturating_sub(self.offers.get(&user).map_or(0, resident))
                    .saturating_add(resident(offer));
                self.offers.insert(user, offer.clone());
            }
        }
        if event.kind == "settle" {
            let prize = event.cards.iter().map(|t| t.card.prize.unwrap_or(0)).sum();
            if prize > 0 {
                let user = event.owner;
                let tier = event
                    .cards
                    .iter()
                    .filter_map(|t| t.card.tier.filter(|_| t.card.prize.unwrap_or(0) > 0))
                    .collect::<Vec<_>>();
                let tier = if event.game == "happy8" {
                    tier.into_iter().max()
                } else {
                    tier.into_iter().min()
                }
                .unwrap_or(0);
                if let Some(period) = event.cards.first().map(|t| t.card.period) {
                    let winner = Winner {
                        user,
                        name: String::new(),
                        tier,
                        prize,
                    };
                    self.bytes = self.bytes.saturating_add(resident(&winner));
                    self.winners
                        .entry((event.game.clone(), period))
                        .or_default()
                        .push(winner);
                }
            }
        }
        self.revision += 1;
    }
    fn own(&self, user: Uuid, game: Game, period: i64) -> Vec<&TicketRow> {
        let mut rows = self
            .by_user
            .get(&(user, game.as_str().into()))
            .into_iter()
            .flatten()
            .filter_map(|id| self.tickets.get(id))
            .filter(|t| t.card.period == period)
            .map(|t| &t.card)
            .collect::<Vec<_>>();
        rows.sort_by(|a, b| b.bought_at.cmp(&a.bought_at).then(a.id.cmp(&b.id)));
        rows
    }
    fn replay(
        &self,
        user: Uuid,
        game: Game,
        request: Uuid,
        hash: &str,
    ) -> Result<Option<Purchase>, Error> {
        for record in [
            super::purchase_id(user, request),
            super::scratch_batch_id(user, request),
        ] {
            if let Some(r) = self.receipts.get(&(user, record)) {
                if r.game != game.as_str() || r.hash != hash {
                    return Err(Error::RequestConflict);
                }
                let mut cards = vec![];
                for id in &r.tickets {
                    let t = self.tickets.get(id).ok_or(Error::RequestConflict)?;
                    cards.push(store::ticket_json(game, &t.card));
                }
                return Ok(Some(Purchase {
                    purchased: cards,
                    charged: r.charged,
                    balance_before: r.before,
                    balance_after: r.after,
                }));
            }
        }
        Ok(None)
    }
}
fn admit(
    pool: &PgPool,
    state: &Arc<State>,
    data: &mut Data,
    event: Event,
    spend: i64,
) -> Result<CommitReceipt, Error> {
    check_owner()?;
    if event.kind != "settle" {
        let mut parked = parked();
        let now = std::time::Instant::now();
        parked.players.retain(|_, until| *until > now);
        if event
            .owner
            .is_some_and(|owner| parked.players.contains_key(&owner))
            || event
                .cards
                .iter()
                .any(|t| parked.cards.contains(&t.card.id))
        {
            return Err(unavailable());
        }
    }
    let bytes = serde_json::to_vec(&event)
        .map_err(|_| Error::InvalidEntry)?
        .len()
        .saturating_mul(4)
        .saturating_add((event.cards.len() + 4).saturating_mul(256));
    // Keep half the capacity for payouts, grants and finishing accepted work.
    if matches!(event.kind.as_str(), "purchase" | "scratch_batch")
        && data.bytes.saturating_add(bytes) > MAX_BYTES / 2
    {
        return Err(unavailable());
    }
    if (event.kind != "retention"
        && (data.tickets.len() + event.cards.len() + data.receipts.len() + data.periods.len()
            > MAX_ROWS
            || data.bytes.saturating_add(bytes) > MAX_BYTES))
        || state.queued.load(Ordering::Acquire).saturating_add(bytes) > MAX_QUEUED_BYTES
    {
        return Err(unavailable());
    }
    // Reserve read-side space before accepting a write. Pending events retain
    // their own payload until publication, so they cannot overfill the view.
    if state
        .published
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .as_ref()
        .is_some_and(|p| {
            p.data
                .bytes
                .saturating_add(state.queued.load(Ordering::Acquire))
                .saturating_add(bytes)
                > MAX_BYTES
        })
    {
        return Err(unavailable());
    }
    let mut writer = state.writer.lock().unwrap_or_else(|e| e.into_inner());
    let slot = writer
        .as_mut()
        .ok_or_else(unavailable)?
        .try_reserve()
        .map_err(|_| unavailable())?;
    let users: Vec<_> = event.owner.into_iter().collect();
    let work = if users.is_empty() {
        None
    } else {
        Some(crate::account_purge::retain_required_work(
            pool,
            &users,
            || {
                for u in &users {
                    if crate::player_directory::lookup(pool, *u)?.is_none() {
                        return Err(db_unavailable());
                    }
                }
                Ok(())
            },
        )?)
    };
    let job = Arc::new(Job {
        event: event.clone(),
        work,
        bytes,
        committed: tokio::sync::watch::channel(None).0,
    });
    let receipt = CommitReceipt(job.committed.subscribe());
    state.queued.fetch_add(bytes, Ordering::AcqRel);
    if let (Some(owner), Some(effect)) = (event.owner, event.effect.clone()) {
        if let Err(e) = beans_read_memory::accept_effect(
            pool,
            owner,
            event.id,
            effect,
            spend,
            event.before,
            || {
                accepted(state).push_back(job.clone());
                slot.publish(Some(job.clone()));
                Ok(())
            },
        ) {
            state.queued.fetch_sub(bytes, Ordering::AcqRel);
            if let Some(work) = &job.work {
                work.complete()
            }
            return Err(
                if matches!(&e,sqlx::Error::Protocol(s) if s=="lottery_insufficient_balance") {
                    Error::InsufficientBalance
                } else {
                    Error::Db(e)
                },
            );
        }
    } else {
        accepted(state).push_back(job.clone());
        slot.publish(Some(job));
    }
    data.apply(&event);
    if event.kind == "retention" {
        data.bytes = persistence::weight(data).expect("runtime values serialize");
    }
    Ok(receipt)
}

pub async fn hydrate(pool: &PgPool) -> sqlx::Result<()> {
    let state = state(pool);
    let _loader = state.loader.lock().await;
    if state
        .data
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .is_some()
    {
        return Ok(());
    }
    let mut data = persistence::hydrate(pool).await?;
    data.bytes = persistence::weight(&data)?;
    if data.bytes > MAX_BYTES {
        return Err(db_unavailable());
    }
    let floor = state.floor.load(Ordering::Acquire);
    (data.revision, data.durable) = (floor, floor);
    tracing::info!(
        periods = data.periods.len(),
        tickets = data.tickets.len(),
        bytes = data.bytes,
        period_slots = PERIOD_SLOTS,
        "lottery_ring_hydrated"
    );
    let (worker_pool, worker_state) = (pool.clone(), state.clone());
    let worker_conn = Arc::new(tokio::sync::Mutex::new(None::<WriterConnection>));
    let writer = Ordered::new(None::<Arc<Job>>, move |_old, next: Option<Arc<Job>>| {
        let (pool, state) = (worker_pool.clone(), worker_state.clone());
        let worker_conn = worker_conn.clone();
        async move {
            if let Some(job) = &next {
                let mut held = worker_conn.lock().await;
                let grants = persist_on_held(&pool, &mut held, &job.event)
                    .await
                    .map_err(|e| {
                        let error = format!(
                            "lottery event={} game={} kind={}: {e}",
                            job.event.id, job.event.game, job.event.kind
                        );
                        if persistence::is_permanent(&e) {
                            *state.refused.lock().unwrap_or_else(|e| e.into_inner()) =
                                Some(error.clone());
                            state.wake.notify_one();
                            crate::write_behind::fenced(&error)
                        } else {
                            error
                        }
                    })?;
                {
                    // Same lock order as acceptance/cold preparation. Lottery
                    // reads take only published -> bean authority, never data.
                    let mut inner = state.data.lock().unwrap_or_else(|e| e.into_inner());
                    let mut public = state.published.lock().unwrap_or_else(|e| e.into_inner());
                    if let (Some(data), Some(public)) = (inner.as_mut(), public.as_mut()) {
                        for (id, grant) in &grants {
                            if let Some(mut t) = data.tickets.get(id).cloned() {
                                t.card.entry["reward_grant"] = json!(grant);
                                data.insert(t);
                            }
                        }
                        data.durable += 1;
                        public.commit(&job.event, &grants);
                        if let Some(owner) = job.event.owner {
                            if job.event.effect.is_some() {
                                beans_read_memory::effect_committed(&pool, owner, job.event.id);
                            }
                        }
                        let _ = state.changed.send(Notice {
                            owner: job.event.owner,
                            game: job.event.game.clone(),
                            revision: public.data.revision,
                        });
                    }
                    // Replay may stop waiting only after the projection is visible.
                    let mut cards = job.event.cards.clone();
                    for (id, grant) in &grants {
                        if let Some(card) = cards.iter_mut().find(|t| t.card.id == *id) {
                            card.card.entry["reward_grant"] = json!(grant);
                        }
                    }
                    job.committed.send_replace(Some(Arc::new(cards)));
                    accepted(&state).retain(|queued| !Arc::ptr_eq(queued, job));
                }
                if let Some(work) = &job.work {
                    work.complete()
                }
                state.queued.fetch_sub(job.bytes, Ordering::AcqRel);
                state.wake.notify_one();
            }
            Ok(next)
        }
    });
    // Installed and resumed while holding the data lock, which every command
    // and every commit needs first, so none can see the ring between the two.
    let mut installed = state.data.lock().unwrap_or_else(|e| e.into_inner());
    *state.published.lock().unwrap_or_else(|e| e.into_inner()) =
        Some(published::Published::new(data.clone()));
    let data = installed.insert(data);
    let mut writer = writer;
    // Accepted events the refused one did not touch continue on this writer,
    // in their accepted order, on top of the committed state just loaded.
    let resumed: Vec<_> =
        std::mem::take(&mut *state.resume.lock().unwrap_or_else(|e| e.into_inner()));
    let mut lost = 0usize;
    for job in resumed {
        // Not gated on the writer's health: a transient error of an earlier
        // resumed write must not cost the ones queued behind it.
        if writer.enqueue(Some(job.clone())).is_ok() {
            data.apply(&job.event);
            accepted(&state).push_back(job);
        } else {
            abandon(pool, &state, &job);
            lost += 1;
        }
    }
    if lost > 0 {
        // The fresh queue holds more than a writer can ever have had accepted.
        tracing::error!(lost, "lottery_resume_queue_full");
    }
    *state.writer.lock().unwrap_or_else(|e| e.into_inner()) = Some(writer);
    drop(installed);
    if floor > 0 {
        for game in Game::ALL {
            let _ = state.changed.send(Notice {
                owner: None,
                game: game.as_str().into(),
                revision: floor,
            });
        }
    }
    Ok(())
}

/// The ordered writer's own pooled connection, kept between writes.
///
/// Perf (2026-10-30): purchases are rare, so a fresh `pool.begin()` per write
/// usually landed on a backend that had never prepared these statements nor
/// compiled the ticket/ledger triggers (prod `POST /api/lottery/buy` 19.5 ms
/// server time, most of it the durable write it waits for). The writer runs
/// one job at a time, every few minutes at least (period rows), so one held
/// connection stays warm. Any failed write drops it (a retry starts on a
/// fresh one), and it is closed one pool connection lifetime after it was
/// taken.
struct WriterConnection {
    conn: crate::db_writer::PoolConnection<sqlx::Postgres>,
    since: std::time::Instant,
}

/// Below this pool size the writer does not keep a connection between writes:
/// a tiny pool (`DB_MAX_CONNECTIONS` 1-3) must not lose one to an idle writer.
/// There each write closes its connection afterwards (see `persist_on_held`).
const HOLD_MIN_POOL_CONNECTIONS: u32 = 4;

async fn persist_on_held(
    pool: &PgPool,
    held: &mut Option<WriterConnection>,
    event: &Event,
) -> sqlx::Result<Vec<(Uuid, crate::game_rewards::Grant)>> {
    if event.kind == "retention" {
        // Runs on its own pool transactions; taking the writer connection too
        // would only cost the pool a second one meanwhile.
        if persistence::retention(pool).await? {
            state(pool).retention_more.store(true, Ordering::Release);
        }
        return Ok(vec![]);
    }
    let hold = pool.options().get_max_connections() >= HOLD_MIN_POOL_CONNECTIONS;
    if held
        .as_ref()
        .is_some_and(|held| held.since.elapsed() >= crate::db::CONNECTION_MAX_LIFETIME)
    {
        *held = None;
    }
    // Taken out for the write: only a successful write puts it back, so an
    // error or a cancelled write never leaves a connection in doubt held.
    // It never returns to the shared pool either: a write that failed between
    // `BEGIN` and the transaction guard (e.g. a cancelled `SET LOCAL` in the
    // opening batch) could leave it inside an aborted transaction, so every
    // drop (error, cancellation, panic, retirement) closes it instead.
    let mut writer = match held.take() {
        Some(writer) => writer,
        None => {
            let since = std::time::Instant::now();
            let mut conn = pool.acquire().await?;
            conn.close_on_drop();
            WriterConnection { conn, since }
        }
    };
    let result = persistence::persist(pool, &mut writer.conn, event).await;
    if result.is_ok() && hold {
        *held = Some(writer);
    }
    result
}

fn accepted(state: &State) -> std::sync::MutexGuard<'_, VecDeque<Arc<Job>>> {
    state.accepted.lock().unwrap_or_else(|e| e.into_inner())
}

/// Ends an accepted job that will never be written: its bean effect leaves
/// memory (the wallet reloads from SQL), its purge hold releases, and whoever
/// waits for its commit gets "unavailable".
fn abandon(pool: &PgPool, state: &State, job: &Job) {
    if let (Some(owner), Some(_)) = (job.event.owner, &job.event.effect) {
        beans_read_memory::effect_abandoned(pool, owner, job.event.id);
    }
    if let Some(work) = &job.work {
        work.complete()
    }
    state.queued.fetch_sub(job.bytes, Ordering::AcqRel);
}

/// What an event touches: its owner and the periods whose row or tickets it
/// carries. Two events sharing neither are independent of each other.
fn touches(event: &Event) -> (Option<Uuid>, Vec<(String, i64)>) {
    let mut periods: Vec<(String, i64)> = event
        .period
        .iter()
        .map(|row| (event.game.clone(), row.period))
        .collect();
    for ticket in event.cards.iter().filter(|t| t.card.period != 0) {
        let key = (ticket.game.clone(), ticket.card.period);
        if !periods.contains(&key) {
            periods.push(key);
        }
    }
    (event.owner, periods)
}

/// Periods and settlements whose own event the database refused in this
/// process. The runtime does not produce their events again (they would be
/// refused again); the next start retries them once.
#[derive(Default)]
struct Parked {
    /// A period whose creation, draw or closing was refused: nothing more is
    /// sold, drawn or settled in it.
    periods: std::collections::HashSet<(String, i64)>,
    /// A user's settlement of a period that was refused or cannot be priced:
    /// those tickets stay unsettled and unpaid.
    settlements: std::collections::HashSet<(String, i64, Uuid)>,
    /// A scratch card whose reveal was refused: it keeps its saved state and
    /// neither the player nor the expiry sweep reveals it again.
    cards: std::collections::HashSet<Uuid>,
    /// A player whose own request was refused, and until when further
    /// requests of that player are declined: a refusal that repeats must not
    /// stop the writer for everyone again and again.
    players: HashMap<Uuid, std::time::Instant>,
}
/// How long a player's requests are declined after one of them was refused.
const REFUSED_PLAYER_HOLD: Duration = Duration::from_secs(300);
fn parked() -> std::sync::MutexGuard<'static, Parked> {
    static PARKED: OnceLock<Mutex<Parked>> = OnceLock::new();
    PARKED
        .get_or_init(Default::default)
        .lock()
        .unwrap_or_else(|e| e.into_inner())
}

/// The database refused ONE accepted event for a reason retrying cannot change
/// (`persistence::is_permanent`), which stops the ordered writer at it. That
/// event is dropped and reported, and with it only the accepted events that
/// were built on it: later events of the same player and of the same period,
/// transitively. Every other accepted event is kept and resumes, in order, on
/// a fresh writer ([`hydrate`]) over memory reloaded from SQL, so memory and
/// durable state agree again without anyone else losing accepted work.
///
/// A dropped event was never written: nothing is charged or paid twice. The
/// player of a dropped purchase or reveal gets "unavailable" for that request
/// and sees the balance and tickets from before it, and further lottery
/// requests of that player are declined for five minutes; a dropped
/// settlement leaves its tickets unsettled (see [`Parked`]).
async fn discard_fenced(pool: &PgPool) -> bool {
    let state = state(pool);
    let fenced = || {
        state
            .writer
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .as_ref()
            .is_some_and(Ordered::is_closed)
    };
    if !fenced() {
        return false;
    }
    let _command = state.commands.lock().await;
    let _loader = state.loader.lock().await;
    if !fenced() {
        return false;
    }
    // The worker has exited, so no queued job can still be committing.
    *state.writer.lock().unwrap_or_else(|e| e.into_inner()) = None;
    let revision = state
        .data
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .take()
        .map_or(0, |data| data.revision);
    *state.published.lock().unwrap_or_else(|e| e.into_inner()) = None;
    state.floor.store(revision + 1, Ordering::Release);
    let mut queue: VecDeque<_> = accepted(&state).drain(..).collect();
    let reason = state
        .refused
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .take()
        .unwrap_or_default();
    // The writer commits in FIFO order: the head is the refused event.
    let refused = queue.pop_front();
    let (mut owners, mut periods) = (Vec::new(), Vec::new());
    let mut dropped = 0usize;
    if let Some(job) = &refused {
        let (owner, touched) = touches(&job.event);
        owners.extend(owner);
        periods.extend(touched);
        let period = job
            .event
            .period
            .as_ref()
            .map(|row| row.period)
            .or_else(|| job.event.cards.first().map(|t| t.card.period));
        if let Some(period) = period.filter(|period| *period != 0) {
            match (job.event.kind.as_str(), job.event.owner) {
                ("settle", Some(owner)) => {
                    parked()
                        .settlements
                        .insert((job.event.game.clone(), period, owner));
                }
                ("draw" | "period", _) => {
                    parked().periods.insert((job.event.game.clone(), period));
                }
                _ => {}
            }
        }
        if job.event.kind != "settle" {
            let mut parked = parked();
            if job.event.kind == "reveal" {
                parked
                    .cards
                    .extend(job.event.cards.iter().map(|t| t.card.id));
            }
            if let Some(owner) = job.event.owner {
                parked
                    .players
                    .insert(owner, std::time::Instant::now() + REFUSED_PLAYER_HOLD);
            }
        }
        tracing::error!(
            alert_error = "Lottery event refused by the database and dropped",
            alert_cause = "one accepted lottery event can never be written (constraint, receipt or plan mismatch); it was dropped before anything of it was saved, its player's balance and tickets are back to the saved state, and all other lottery work continues / 一条已受理的彩票事件无法落库（约束、收据或开奖计划不一致），已在写入前丢弃，该玩家的余额与彩票恢复为已保存状态，其余彩票业务照常进行",
            alert_action = "AI team reads the refusal reason, checks that player's wallet, tickets and the period row, and settles a parked settlement or period by hand; Loong action: none / AI 根据拒绝原因核查该玩家钱包、彩票与期次，必要时手工结算被搁置的结算或期次，Loong 无需操作",
            "[lottery_event_refused] event={} kind={} game={} user={} period={} reason={reason}",
            job.event.id,
            job.event.kind,
            job.event.game,
            job.event.owner.map(|u| u.to_string()).unwrap_or_else(|| "-".into()),
            period.unwrap_or(0),
        );
        abandon(pool, &state, job);
    }
    let mut kept = Vec::new();
    for job in queue {
        let (owner, touched) = touches(&job.event);
        if owner.is_some_and(|owner| owners.contains(&owner))
            || touched.iter().any(|key| periods.contains(key))
        {
            owners.extend(owner);
            periods.extend(touched);
            abandon(pool, &state, &job);
            dropped += 1;
        } else {
            kept.push(job);
        }
    }
    tracing::error!(
        dropped_dependents = dropped,
        resumed = kept.len(),
        revision,
        "lottery_writer_fenced_rehydrating"
    );
    state
        .resume
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .extend(kept);
    true
}

fn ensure_period(
    pool: &PgPool,
    state: &Arc<State>,
    data: &mut Data,
    game: Game,
) -> Result<(), Error> {
    let selling = game
        .schedule()
        .ok_or(Error::InvalidEntry)?
        .period_at(super::now_ms());
    if data
        .periods
        .contains_key(&(game.as_str().into(), selling.id))
    {
        return Ok(());
    }
    if super::now_ms() >= selling.closes_ms {
        return Err(Error::SalesClosed);
    }
    // Its creation was refused: nothing can be sold in it.
    if parked()
        .periods
        .contains(&(game.as_str().into(), selling.id))
    {
        return Err(Error::SalesClosed);
    }
    if data
        .periods
        .range((game.as_str().into(), i64::MIN)..=(game.as_str().into(), i64::MAX))
        .count()
        >= PERIOD_SLOTS
    {
        return Err(unavailable());
    }
    let plan = draw_games::lottery::Plan::new(game, selling.id).map_err(|_| Error::InvalidEntry)?;
    let row = PeriodRow {
        period: selling.id,
        status: "open".into(),
        numbers: None,
        bonus: None,
        bonus_numbers: vec![],
        drawn_at: None,
        settled_at: None,
        tickets: 0,
        stakes: 0,
        players: 0,
        sales: 0,
        paid: 0,
        winners: 0,
        proof_plan: Some(json!(plan)),
        draw_proof: None,
    };
    let mut event = Event::new(
        game,
        None,
        "period",
        super::record_id(&format!("period:{}:{}", game.as_str(), selling.id)),
    );
    event.period = Some(row);
    admit(pool, state, data, event, 0).map(|_| ())
}
fn offer(
    pool: &PgPool,
    state: &Arc<State>,
    data: &mut Data,
    user: Uuid,
) -> Result<draw_games::proof::Commitment, Error> {
    if !data.offers.get(&user).is_some_and(|offer| offer.fresh) {
        let mut seed = [0u8; 32];
        OsRng.fill_bytes(&mut seed);
        let offer = Offer {
            user,
            id: Uuid::new_v4(),
            seed: hex::encode(seed),
            fresh: true,
            previous: data.offers.get(&user).map(|o| o.id),
        };
        let mut event = Event::new(Game::Scratch, Some(user), "offer", offer.id);
        event.offer = Some(offer);
        admit(pool, state, data, event, 0)?;
    }
    let o = data.offers.get(&user).ok_or_else(unavailable)?;
    draw_games::proof::commit("scratch", &o.id.to_string(), &o.seed)
        .map_err(|_| Error::InvalidEntry)
}

fn current_view(pool: &PgPool, user: Uuid, game: Game) -> Result<Value, Error> {
    player(pool, user)?;
    beans_read_memory::ensure(pool, user)?;
    let state = state(pool);
    let mut lock = state.published.lock().unwrap_or_else(|e| e.into_inner());
    let public = lock.as_mut().ok_or_else(unavailable)?;
    if game == Game::Scratch && !public.touch(user) {
        return Err(Error::PreparationRequired);
    }
    let (balance, available) = beans_read_memory::lottery_read_balance(pool, user)?;
    let data = &mut public.data;
    let now = Utc::now();
    data.presence
        .retain(|_, at| now.signed_duration_since(*at).num_seconds() < 30);
    if data.presence.len() < 4096 || data.presence.contains_key(&(game.as_str().into(), user)) {
        data.presence.insert((game.as_str().into(), user), now);
    }
    let fairness = if game == Game::Scratch {
        data.offers
            .get(&user)
            .filter(|offer| offer.fresh)
            .map(|offer| {
                draw_games::proof::commit("scratch", &offer.id.to_string(), &offer.seed)
                    .map_err(|_| Error::InvalidEntry)
            })
            .transpose()?
            .ok_or(Error::PreparationRequired)
            .map(Some)?
    } else {
        None
    };
    let (period, my) = if let Some(schedule) = game.schedule() {
        let selling = schedule.period_at(super::now_ms());
        let row = data.periods.get(&(game.as_str().into(), selling.id));
        let open = row.is_some_and(|r| r.status == "open" && r.proof_plan.is_some())
            && super::now_ms() < selling.closes_ms;
        (
            store::period_json(&selling, row, open),
            data.own(user, game, selling.id)
                .iter()
                .map(|t| store::ticket_json(game, t))
                .collect::<Vec<_>>(),
        )
    } else {
        let ids = data
            .by_user
            .get(&(user, game.as_str().into()))
            .cloned()
            .unwrap_or_default();
        let latest = ids
            .iter()
            .filter_map(|id| data.tickets.get(id))
            .max_by_key(|t| (t.card.bought_at, t.card.id))
            .map(|t| t.card.purchase_id);
        (
            Value::Null,
            ids.iter()
                .filter_map(|id| data.tickets.get(id))
                .filter(|t| t.card.settled_at.is_none() || Some(t.card.purchase_id) == latest)
                .map(|t| store::ticket_json(game, &t.card))
                .collect(),
        )
    };
    let mut members: Vec<_> = data
        .presence
        .iter()
        .filter(|((g, _), _)| g == game.as_str())
        .collect();
    members.sort_by_key(|(_, at)| std::cmp::Reverse(**at));
    let present = members.len();
    members.truncate(12);
    Ok(
        json!({"game":game.as_str(),"runtime_epoch":crate::server_performance::process_instance_id(),"revision":data.revision,"durable_revision":data.durable,"now_ms":super::now_ms(),"balance":balance,"available_balance":available,"unit":game.unit(),"period":period,"my_tickets":my,"last_draw":null,"recent":[],"crowd":{"present":present,"members":members.iter().map(|((_,u),at)|json!({"user_id":u,"name":name(pool,*u),"seen_at_ms":at.timestamp_millis()})).collect::<Vec<_>>()},"feed":[],"fairness_offer":fairness}),
    )
}

async fn wallet_ready(pool: &PgPool, user: Uuid) -> Result<(), Error> {
    beans_read_memory::ensure(pool, user)?;
    beans_read_memory::initial_grant_ready(pool, user).await?;
    Ok(())
}

/// [`wallet_ready`] when the purchase preflight statement already read
/// whether the wallet row exists (`wallet_durable`).
async fn wallet_ready_after(pool: &PgPool, user: Uuid, wallet_durable: bool) -> Result<(), Error> {
    beans_read_memory::ensure(pool, user)?;
    if !wallet_durable {
        beans_read_memory::initial_grant_durable(pool).await?;
    }
    Ok(())
}

async fn prepare_scratch(pool: &PgPool, user: Uuid, ticket: Option<Uuid>) -> Result<(), Error> {
    let state = state(pool);
    {
        let lock = state.data.lock().unwrap_or_else(|e| e.into_inner());
        let data = lock.as_ref().ok_or_else(unavailable)?;
        let mut public = state.published.lock().unwrap_or_else(|e| e.into_inner());
        // Retain only a small replay window in the command working set. Its
        // replacement happens under commands, before the next SQL replay
        // lookup, so a concurrent commit cannot erase an in-flight replay.
        let scratch_receipts = data
            .receipts
            .values()
            .filter(|r| r.game == "scratch")
            .count();
        if data.scratch_owner == Some(user)
            && scratch_receipts < 8
            && ticket.is_none_or(|id| data.tickets.contains_key(&id))
            && public.as_mut().is_some_and(|p| p.touch(user))
        {
            return Ok(());
        }
    }
    // Replace only the scratch working set. Capture its last accepted job,
    // not the global writer tail; later/unrelated work cannot extend this wait.
    let pending = accepted(&state)
        .iter()
        .rev()
        .find(|job| job.event.game == "scratch")
        .map(|job| CommitReceipt(job.committed.subscribe()));
    if let Some(receipt) = pending {
        receipt.wait().await?;
    }
    let mut conn = pool.acquire().await?;
    let mut cards = super::scratch::my_cards(&mut conn, user).await?;
    if let Some(id) = ticket {
        if !cards.iter().any(|c| c.id == id) {
            if let Some(row) = sqlx::query(&format!("SELECT {} FROM lottery_tickets WHERE id=$1 AND user_id=$2 AND game='scratch' AND cost=100 AND {}", store::TICKET_COLUMNS, super::scratch::REWARD_VERSION_SQL))
                .bind(id).bind(user).fetch_optional(&mut *conn).await? {
                cards.push(TicketRow::from_row(&row));
            }
        }
    }
    let offer = sqlx::query(
        "SELECT id,server_seed FROM draw_commitments WHERE user_id=$1 AND game='scratch'",
    )
    .bind(user)
    .fetch_optional(&mut *conn)
    .await?;
    let mut loaded = Data::default();
    for card in cards {
        loaded.insert(OwnedTicket {
            user,
            game: "scratch".into(),
            card,
        });
    }
    if let Some(row) = offer {
        loaded.offers.insert(
            user,
            Offer {
                user,
                id: row.get("id"),
                seed: row.get("server_seed"),
                fresh: true,
                previous: None,
            },
        );
    }
    loaded.bytes = persistence::weight(&loaded)?;
    let mut lock = state.data.lock().unwrap_or_else(|e| e.into_inner());
    let data = lock.as_mut().ok_or_else(unavailable)?;
    // Scratch jobs are durable; unrelated drawn-game jobs/receipts stay intact.
    // Admit BOTH replacements before changing either working state or marker.
    let retained = data
        .tickets
        .values()
        .filter(|t| t.game != "scratch")
        .map(resident)
        .sum::<usize>()
        + data.periods.values().map(resident).sum::<usize>()
        + data
            .receipts
            .values()
            .filter(|r| r.game != "scratch")
            .map(resident)
            .sum::<usize>()
        + data.winners.values().flatten().map(resident).sum::<usize>();
    if retained.saturating_add(loaded.bytes) > MAX_BYTES {
        return Err(unavailable());
    }
    let mut public = state.published.lock().unwrap_or_else(|e| e.into_inner());
    public.as_mut().ok_or_else(unavailable)?.load_scratch(
        &loaded,
        user,
        state.queued.load(Ordering::Acquire),
    )?;
    data.tickets.retain(|_, t| t.game != "scratch");
    data.by_user.retain(|(_, g), _| g != "scratch");
    data.by_period.retain(|(g, _), _| g != "scratch");
    data.recent.remove("scratch");
    data.receipts.retain(|_, receipt| receipt.game != "scratch");
    data.offers = loaded.offers;
    for card in loaded.tickets.into_values() {
        data.insert(card);
    }
    data.bytes = retained + loaded.bytes;
    data.scratch_owner = Some(user);
    Ok(())
}

/// Only committed memory, including on WebSocket sync/heartbeat. Cold scratch
/// hydration and fairness commitment creation are explicit prepare commands.
pub async fn view(pool: &PgPool, user: Uuid, game: Game) -> Result<Value, Error> {
    current_view(pool, user, game)
}

pub async fn prepare(pool: &PgPool, user: Uuid, game: Game) -> Result<Value, Error> {
    player(pool, user)?;
    let state = state(pool);
    let command = state.commands.lock().await;
    wallet_ready(pool, user).await?;
    let receipt = if game == Game::Scratch {
        prepare_scratch(pool, user, None).await?;
        let mut lock = state.data.lock().unwrap_or_else(|e| e.into_inner());
        let data = lock.as_mut().ok_or_else(unavailable)?;
        let offer = offer(pool, &state, data, user)?;
        pending_receipt(
            &state,
            Uuid::parse_str(&offer.draw_id).map_err(|_| Error::InvalidEntry)?,
        )
    } else {
        let mut lock = state.data.lock().unwrap_or_else(|e| e.into_inner());
        let data = lock.as_mut().ok_or_else(unavailable)?;
        ensure_period(pool, &state, data, game)?;
        // Period creation is driven by the background owner as well.
        let selling = game
            .schedule()
            .ok_or(Error::InvalidEntry)?
            .period_at(super::now_ms());
        pending_receipt(
            &state,
            super::record_id(&format!("period:{}:{}", game.as_str(), selling.id)),
        )
    };
    drop(command);
    receipt.wait().await?;
    current_view(pool, user, game)
}

pub async fn buy_numbers(
    pool: &PgPool,
    user: Uuid,
    game: Game,
    body: &NumberBuy,
) -> Result<Purchase, Error> {
    let validation = lottery_span(LotteryStage::Validate);
    player(pool, user)?;
    let entries = body
        .tickets
        .iter()
        .map(|v| Entry::parse(game, v).map_err(|_| Error::InvalidEntry))
        .collect::<Result<Vec<_>, _>>()?;
    let mut identity = json!({"game":game.as_str(),"period":body.period,"multiplier":body.multiplier,"tickets":entries.iter().map(Entry::to_value).collect::<Vec<_>>(),"proof_commitment":body.proof_commitment});
    if body.all_in {
        identity["all_in"] = json!(true);
        identity["expected_balance"] = json!(body.expected_balance);
    }
    let hash = store::body_sha256(&identity);
    let state = state(pool);
    drop(validation);
    let command = {
        let _span = lottery_span(LotteryStage::CommandWait);
        state.commands.lock().await
    };
    let (replay, wallet) = {
        let mut conn = {
            let _span = lottery_span(LotteryStage::ReplayPool);
            pool.acquire().await?
        };
        let _span = lottery_span(LotteryStage::ReplayQuery);
        store::replay_request_and_wallet(&mut conn, game, user, body.request_id, &hash).await?
    };
    let (purchase, receipt) = if let Some(p) = replay {
        (
            p,
            pending_receipt(&state, super::purchase_id(user, body.request_id)),
        )
    } else {
        {
            let _span = lottery_span(LotteryStage::WalletReady);
            wallet_ready_after(pool, user, wallet).await?;
        }
        let _span = lottery_span(LotteryStage::Accept);
        accept_numbers(pool, user, game, body)?
    };
    drop(command);
    {
        let _span = lottery_span(LotteryStage::CommitWait);
        receipt.wait().await?;
    }
    Ok(purchase)
}

fn committed_purchase(mut purchase: Purchase, cards: &[OwnedTicket]) -> Purchase {
    for wire in &mut purchase.purchased {
        if let Some(id) = wire["id"].as_str().and_then(|id| Uuid::parse_str(id).ok()) {
            if let Some(ticket) = cards.iter().find(|t| t.card.id == id) {
                *wire = store::ticket_json(Game::Scratch, &ticket.card);
            }
        }
    }
    purchase
}

/// Pair wallet visibility with lottery publication for compact purchase replies.
pub(super) fn balance(pool: &PgPool, user: Uuid) -> Result<(i64, i64), Error> {
    let state = state(pool);
    let public = state.published.lock().unwrap_or_else(|e| e.into_inner());
    public.as_ref().ok_or_else(unavailable)?;
    Ok(beans_read_memory::lottery_read_balance(pool, user)?)
}

pub async fn buy_scratch(
    pool: &PgPool,
    user: Uuid,
    body: &super::scratch::ScratchBuy,
) -> Result<Purchase, Error> {
    let validation = lottery_span(LotteryStage::Validate);
    player(pool, user)?;
    // The same normalised identity the purchase stored (reward_version
    // included), so a retried request replays instead of conflicting.
    let hash = store::body_sha256(&body.identity());
    let state = state(pool);
    drop(validation);
    let command = {
        let _span = lottery_span(LotteryStage::CommandWait);
        state.commands.lock().await
    };
    {
        let _span = lottery_span(LotteryStage::ScratchPrepare);
        prepare_scratch(pool, user, None).await?;
    }
    let (replay, wallet) = {
        let mut conn = {
            let _span = lottery_span(LotteryStage::ReplayPool);
            pool.acquire().await?
        };
        let _span = lottery_span(LotteryStage::ReplayQuery);
        store::replay_request_and_wallet(&mut conn, Game::Scratch, user, body.request_id, &hash)
            .await?
    };
    let (purchase, receipt) = if let Some(p) = replay {
        let id = if body.count > 1 || body.all_in {
            super::scratch_batch_id(user, body.request_id)
        } else {
            super::purchase_id(user, body.request_id)
        };
        (p, pending_receipt(&state, id))
    } else {
        {
            let _span = lottery_span(LotteryStage::WalletReady);
            wallet_ready_after(pool, user, wallet).await?;
        }
        let _span = lottery_span(LotteryStage::Accept);
        accept_scratch(pool, user, body)?
    };
    drop(command);
    let cards = {
        let _span = lottery_span(LotteryStage::CommitWait);
        receipt.wait().await?
    };
    let _span = lottery_span(LotteryStage::Reply);
    Ok(committed_purchase(purchase, &cards))
}

pub async fn reveal(
    pool: &PgPool,
    user: Uuid,
    ticket: Uuid,
    index: Option<usize>,
) -> Result<(Value, i64), Error> {
    player(pool, user)?;
    let state = state(pool);
    let command = state.commands.lock().await;
    prepare_scratch(pool, user, Some(ticket)).await?;
    wallet_ready(pool, user).await?;
    let (wire, balance, receipt) = accept_reveal(pool, user, ticket, index)?;
    drop(command);
    let cards = receipt.wait().await?;
    let wire = cards
        .iter()
        .find(|t| t.card.id == ticket)
        .map(|t| store::ticket_json(Game::Scratch, &t.card))
        .unwrap_or(wire);
    Ok((wire, balance))
}

pub async fn refill(pool: &PgPool, user: Uuid) -> Result<i64, Error> {
    match crate::beans::claim_daily(pool, user).await? {
        crate::beans::DailyOutcome::Granted { balance, .. } => Ok(balance),
        crate::beans::DailyOutcome::AlreadyClaimed { .. } => Err(Error::DailyAlreadyClaimed),
    }
}

fn accept_numbers(
    pool: &PgPool,
    user: Uuid,
    game: Game,
    body: &NumberBuy,
) -> Result<(Purchase, CommitReceipt), Error> {
    player(pool, user)?;
    if body.request_id.is_nil()
        || body.multiplier != 1
        || body.tickets.is_empty()
        || body.tickets.len() > rules::MAX_TICKETS_PER_PURCHASE
        || (body.all_in && body.expected_balance.is_none())
        || (!body.all_in && body.expected_balance.is_some())
    {
        return Err(Error::InvalidEntry);
    }
    let entries = body
        .tickets
        .iter()
        .map(|v| Entry::parse(game, v).map_err(|_| Error::InvalidEntry))
        .collect::<Result<Vec<_>, _>>()?;
    let mut normalized = json!({"game":game.as_str(),"period":body.period,"multiplier":body.multiplier,"tickets":entries.iter().map(Entry::to_value).collect::<Vec<_>>(),"proof_commitment":body.proof_commitment});
    if body.all_in {
        normalized["all_in"] = json!(true);
        normalized["expected_balance"] = json!(body.expected_balance)
    }
    let hash = store::body_sha256(&normalized);
    let state = state(pool);
    let mut lock = state.data.lock().unwrap_or_else(|e| e.into_inner());
    let data = lock.as_mut().ok_or_else(unavailable)?;
    if let Some(p) = data.replay(user, game, body.request_id, &hash)? {
        let id = super::purchase_id(user, body.request_id);
        return Ok((p, pending_receipt(&state, id)));
    }
    ensure_period(pool, &state, data, game)?;
    let selling = game
        .schedule()
        .ok_or(Error::InvalidEntry)?
        .period_at(super::now_ms());
    if selling.id != body.period || super::now_ms() >= selling.closes_ms {
        return Err(Error::SalesClosed);
    }
    let mut row = data
        .periods
        .get(&(game.as_str().into(), selling.id))
        .cloned()
        .ok_or(Error::SalesClosed)?;
    if row.status != "open" {
        return Err(Error::SalesClosed);
    }
    let plan: draw_games::lottery::Plan =
        serde_json::from_value(row.proof_plan.clone().ok_or(Error::FairnessRequired)?)
            .map_err(|_| Error::InvalidEntry)?;
    plan.validate().map_err(|_| Error::InvalidEntry)?;
    if (plan.protocol != draw_games::lottery::PROTOCOL
        && plan.protocol != draw_games::lottery::V2_PROTOCOL)
        || plan.commitment != body.proof_commitment
    {
        return Err(Error::RequestConflict);
    }
    let before = beans_read_memory::ensure(pool, user)?.balance;
    let available = beans_read_memory::available(pool, user, before, false, None)?;
    let old = data.own(user, game, selling.id);
    let used = old
        .iter()
        .map(|t| i64::from(t.stakes) * i64::from(t.multiplier))
        .sum::<i64>();
    if old.len() + entries.len() > rules::MAX_TICKETS_PER_PERIOD as usize {
        return Err(Error::TooManyTickets);
    }
    let stakes = entries.iter().map(Entry::stakes).collect::<Vec<_>>();
    if body.all_in && stakes.iter().any(|s| *s != 1) {
        return Err(Error::InvalidEntry);
    }
    let multipliers = if body.all_in {
        if body.expected_balance != Some(available) {
            return Err(Error::RequestConflict);
        }
        let copies = available / game.unit();
        if copies <= 0 {
            return Err(Error::InsufficientBalance);
        }
        let groups = entries.len() as i64;
        if groups != copies.min(rules::MAX_ALL_IN_GROUPS as i64) {
            return Err(Error::InvalidEntry);
        }
        (0..groups)
            .map(|i| {
                let n = copies / groups + i64::from(i < copies % groups);
                if !(1..=rules::MAX_ALL_IN_MULTIPLIER).contains(&n) {
                    return Err(Error::TooManyTickets);
                }
                i16::try_from(n).map_err(|_| Error::TooManyTickets)
            })
            .collect::<Result<Vec<_>, _>>()?
    } else {
        vec![1; entries.len()]
    };
    let effective = stakes
        .iter()
        .zip(&multipliers)
        .map(|(s, m)| s.checked_mul(i64::from(*m)).ok_or(Error::InvalidEntry))
        .collect::<Result<Vec<_>, _>>()?;
    let total = effective
        .iter()
        .try_fold(0i64, |a, b| a.checked_add(*b).ok_or(Error::InvalidEntry))?;
    if !body.all_in
        && used
            .checked_add(total)
            .is_none_or(|n| n > rules::MAX_STAKES_PER_PERIOD)
    {
        return Err(Error::TooManyTickets);
    }
    let charged = total.checked_mul(game.unit()).ok_or(Error::InvalidEntry)?;
    if charged > available {
        return Err(Error::InsufficientBalance);
    }
    let total_i32 = i32::try_from(total).map_err(|_| Error::TooManyTickets)?;
    row.stakes = row
        .stakes
        .checked_add(total_i32)
        .ok_or(Error::TooManyTickets)?;
    row.sales = row
        .sales
        .checked_add(charged)
        .ok_or(Error::TooManyTickets)?;
    row.tickets = row
        .tickets
        .checked_add(entries.len() as i32)
        .ok_or(Error::TooManyTickets)?;
    row.players += i32::from(old.is_empty());
    let id = super::purchase_id(user, body.request_id);
    let mut event = Event::new(game, Some(user), "purchase", id);
    event.before = before;
    for ((entry, stakes), multiplier) in entries.iter().zip(stakes).zip(multipliers) {
        let card = TicketRow {
            id: Uuid::new_v4(),
            purchase_id: id,
            period: selling.id,
            bought_at: event.at,
            entry: entry.to_value(),
            stakes: stakes as i32,
            multiplier,
            cost: i64::from(multiplier) * stakes * game.unit(),
            revealed: None,
            prize: None,
            tier: None,
            settled_at: None,
            proof_plan: row.proof_plan.clone(),
        };
        event.cards.push(OwnedTicket {
            user,
            game: game.as_str().into(),
            card,
        })
    }
    let receipt = Receipt {
        game: game.as_str().into(),
        record: id,
        hash,
        tickets: event.cards.iter().map(|t| t.card.id).collect(),
        charged,
        before,
        after: before - charged,
    };
    event.period = Some(row);
    event.receipt = Some(receipt.clone());
    event.effect = Some(PendingEffect {
        delta: -charged,
        bonus: 0,
        played: 0,
        won: 0,
    });
    let purchased = event
        .cards
        .iter()
        .map(|t| store::ticket_json(game, &t.card))
        .collect();
    let commit = admit(pool, &state, data, event, charged)?;
    Ok((
        Purchase {
            purchased,
            charged,
            balance_before: before,
            balance_after: receipt.after,
        },
        commit,
    ))
}

/// Beans for a settling card's service reward, recorded on the card so a
/// converted version-1 Max reward stays auditable. No membership is granted.
fn scratch_reward(card: &mut TicketRow) -> Result<i64, Error> {
    let amount = super::scratch::reward_beans(card)?;
    if amount > 0 {
        card.entry["reward_beans"] = json!(amount);
    }
    Ok(amount)
}
fn accept_scratch(
    pool: &PgPool,
    user: Uuid,
    body: &super::scratch::ScratchBuy,
) -> Result<(Purchase, CommitReceipt), Error> {
    player(pool, user)?;
    if body.request_id.is_nil()
        || body.cost != 100
        || (!body.all_in && ![1, 5, 10].contains(&body.count))
        || (body.all_in && (body.count == 0 || body.expected_balance.is_none()))
        || (!body.all_in && body.expected_balance.is_some())
    {
        return Err(Error::InvalidEntry);
    }
    let reward_version = body.reward_version()?;
    if body.count > draw_games::scratch::MAX_BATCH_CARDS {
        return Err(Error::TooManyTickets);
    }
    let batch = body.count > 1 || body.all_in;
    let hash = store::body_sha256(&body.identity());
    let state = state(pool);
    let mut lock = state.data.lock().unwrap_or_else(|e| e.into_inner());
    let data = lock.as_mut().ok_or_else(unavailable)?;
    if let Some(p) = data.replay(user, Game::Scratch, body.request_id, &hash)? {
        let id = if body.count > 1 || body.all_in {
            super::scratch_batch_id(user, body.request_id)
        } else {
            super::purchase_id(user, body.request_id)
        };
        return Ok((p, pending_receipt(&state, id)));
    }
    let before = beans_read_memory::ensure(pool, user)?.balance;
    let available = beans_read_memory::available(pool, user, before, false, None)?;
    if body.all_in {
        if body.expected_balance != Some(available) {
            return Err(Error::RequestConflict);
        }
        if available / body.cost != body.count as i64 {
            return Err(Error::InvalidEntry);
        }
    }
    if !batch
        && data
            .own(user, Game::Scratch, 0)
            .iter()
            .filter(|t| t.settled_at.is_none())
            .count()
            >= 50
    {
        return Err(Error::TooManyTickets);
    }
    let charged = body
        .cost
        .checked_mul(body.count as i64)
        .ok_or(Error::InvalidEntry)?;
    if charged > available {
        return Err(Error::InsufficientBalance);
    }
    let old = data
        .offers
        .get(&user)
        .cloned()
        .ok_or(Error::RequestConflict)?;
    if !old.fresh {
        return Err(Error::RequestConflict);
    }
    draw_games::proof::seed_bytes(&body.fairness.client_seed).map_err(|_| Error::InvalidEntry)?;
    let commitment = draw_games::proof::commit("scratch", &old.id.to_string(), &old.seed)
        .map_err(|_| Error::InvalidEntry)?;
    if old.id != body.fairness.offer_id || commitment.commitment != body.fairness.commitment {
        return Err(Error::RequestConflict);
    }
    let reveal = draw_games::proof::Reveal {
        commitment,
        server_seed: old.seed,
        client_seed: body.fairness.client_seed.clone(),
    };
    let generated = draw_games::scratch::generate(&reveal, body.cost, body.count)
        .map_err(|_| Error::InvalidEntry)?;
    let rewards = draw_games::scratch::rewards(&reveal, body.cost, body.count, reward_version)
        .map_err(|_| Error::InvalidEntry)?;
    let id = if batch {
        super::scratch_batch_id(user, body.request_id)
    } else {
        super::purchase_id(user, body.request_id)
    };
    let mut event = Event::new(
        Game::Scratch,
        Some(user),
        if batch { "scratch_batch" } else { "purchase" },
        id,
    );
    event.before = before;
    event.claim = Some(old.id);
    let mut seed = [0u8; 32];
    OsRng.fill_bytes(&mut seed);
    // Freeze the next offer in the same transaction as consuming the old one.
    event.offer = Some(Offer {
        user,
        id: Uuid::new_v4(),
        seed: hex::encode(seed),
        fresh: true,
        previous: None,
    });
    let mut paid = 0i64;
    for (i, card) in generated.into_iter().enumerate() {
        let entry = json!({"cost":body.cost,"lucky":card.lucky,"numbers":card.numbers,"fairness":{"kind":"scratch","reveal":reveal,"cost":body.cost,"count":body.count,"index":i,"numbers":card.numbers,"lucky":card.lucky,"prize":card.prize,"hits":card.hits},"reward_version":reward_version,"reward":rewards[i]});
        let mut ticket = TicketRow {
            id: Uuid::new_v4(),
            purchase_id: id,
            period: 0,
            bought_at: event.at,
            entry,
            stakes: 1,
            multiplier: 1,
            cost: 100,
            revealed: Some(vec![batch; 6]),
            prize: batch.then_some(card.prize),
            tier: batch.then_some(card.hits),
            settled_at: batch.then_some(event.at),
            proof_plan: None,
        };
        if batch {
            paid = paid.checked_add(card.prize).ok_or(Error::InvalidEntry)?;
            let bonus = scratch_reward(&mut ticket)?;
            event.bonus = event.bonus.checked_add(bonus).ok_or(Error::InvalidEntry)?;
        }
        event.cards.push(OwnedTicket {
            user,
            game: "scratch".into(),
            card: ticket,
        });
    }
    let delta = paid.checked_sub(charged).ok_or(Error::InvalidEntry)?;
    let after = before
        .checked_add(delta)
        .and_then(|v| v.checked_add(event.bonus))
        .ok_or(Error::InvalidEntry)?;
    event.receipt = Some(Receipt {
        game: "scratch".into(),
        record: id,
        hash,
        tickets: event.cards.iter().map(|t| t.card.id).collect(),
        charged,
        before,
        after,
    });
    event.effect = Some(PendingEffect {
        delta,
        bonus: event.bonus,
        played: i32::from(batch),
        won: i32::from(batch && paid > 0),
    });
    let purchased = event
        .cards
        .iter()
        .map(|t| store::ticket_json(Game::Scratch, &t.card))
        .collect();
    let receipt = admit(pool, &state, data, event, charged)?;
    Ok((
        Purchase {
            purchased,
            charged,
            balance_before: before,
            balance_after: after,
        },
        receipt,
    ))
}

fn accept_reveal(
    pool: &PgPool,
    user: Uuid,
    ticket: Uuid,
    index: Option<usize>,
) -> Result<(Value, i64, CommitReceipt), Error> {
    player(pool, user)?;
    reveal_card(pool, user, ticket, index, false)
}
fn reveal_card(
    pool: &PgPool,
    user: Uuid,
    ticket: Uuid,
    index: Option<usize>,
    expiry: bool,
) -> Result<(Value, i64, CommitReceipt), Error> {
    if index.is_some_and(|i| i >= 6) {
        return Err(Error::InvalidEntry);
    }
    let state = state(pool);
    let mut lock = state.data.lock().unwrap_or_else(|e| e.into_inner());
    let data = lock.as_mut().ok_or_else(unavailable)?;
    let mut owned = data
        .tickets
        .get(&ticket)
        .filter(|t| {
            t.user == user
                && t.game == "scratch"
                && t.card.cost == 100
                && matches!(t.card.entry["reward_version"].as_u64(), Some(1 | 2))
        })
        .cloned()
        .ok_or(Error::TicketNotFound)?;
    if owned.card.settled_at.is_some() {
        // A retry may see an accepted final reveal before it commits. Await
        // that same frozen result instead of acknowledging settlement early.
        if let Some(job) = accepted(&state)
            .iter()
            .find(|job| job.event.id == super::scratch_credit_id(ticket))
        {
            let balance = job
                .event
                .before
                .checked_add(job.event.effect.as_ref().map_or(0, |e| e.delta + e.bonus))
                .ok_or(Error::InvalidEntry)?;
            return Ok((
                store::ticket_json(Game::Scratch, &owned.card),
                balance,
                CommitReceipt(job.committed.subscribe()),
            ));
        }
        return Err(Error::AlreadySettled);
    }
    let before = if expiry {
        beans_read_memory::settlement_balance(pool, user)?
    } else {
        beans_read_memory::ensure(pool, user)?.balance
    };
    let mut shown = owned
        .card
        .revealed
        .clone()
        .unwrap_or_else(|| vec![false; 6]);
    shown.resize(6, false);
    if let Some(i) = index {
        shown[i] = true
    } else {
        shown.fill(true)
    }
    if owned.card.revealed.as_ref() == Some(&shown) {
        let id = super::record_id(&format!(
            "reveal:{ticket}:{}",
            shown
                .iter()
                .enumerate()
                .filter(|(_, v)| **v)
                .map(|(i, _)| 1u8 << i)
                .sum::<u8>()
        ));
        return Ok((
            store::ticket_json(Game::Scratch, &owned.card),
            before,
            pending_receipt(&state, id),
        ));
    }
    let all = shown.iter().all(|v| *v);
    let id = if all {
        super::scratch_credit_id(ticket)
    } else {
        super::record_id(&format!(
            "reveal:{ticket}:{}",
            shown
                .iter()
                .enumerate()
                .filter(|(_, v)| **v)
                .map(|(i, _)| 1u8 << i)
                .sum::<u8>()
        ))
    };
    let mut event = Event::new(Game::Scratch, Some(user), "reveal", id);
    event.before = before;
    owned.card.revealed = Some(shown);
    let mut after = before;
    if all {
        let numbers: [i16; 6] = serde_json::from_value(owned.card.entry["numbers"].clone())
            .map_err(|_| Error::InvalidEntry)?;
        let lucky = owned.card.entry["lucky"]
            .as_i64()
            .ok_or(Error::InvalidEntry)? as i16;
        let (paid, tier) =
            draw_games::scratch::payout(100, lucky, &numbers).map_err(|_| Error::InvalidEntry)?;
        owned.card.prize = Some(paid);
        owned.card.tier = Some(tier);
        owned.card.settled_at = Some(event.at);
        let bonus = scratch_reward(&mut owned.card)?;
        event.bonus = bonus;
        event.effect = Some(PendingEffect {
            delta: paid,
            bonus,
            played: 1,
            won: i32::from(paid > 0),
        });
        after = before
            .checked_add(paid)
            .and_then(|v| v.checked_add(bonus))
            .ok_or(Error::InvalidEntry)?;
    }
    let wire = store::ticket_json(Game::Scratch, &owned.card);
    event.cards.push(owned);
    let receipt = admit(pool, &state, data, event, 0)?;
    Ok((wire, after, receipt))
}

/// Settlement events kept in the writer queue at once (it holds 64), so a
/// large draw pays out continuously without taking purchases' room.
const SETTLE_IN_FLIGHT: usize = 16;
/// Settlement events decided between two yields to other tasks.
const SETTLE_BATCH: usize = 20;
/// Expired scratch cards revealed per sweep query.
const SCRATCH_BATCH: usize = 20;
/// Least wait before an expiry sweep repeats while cards it found remain
/// unsettled in SQL (a failed reveal, or one whose write is still queued).
const SCRATCH_RETRY: Duration = Duration::from_secs(60);
const DAY: Duration = Duration::from_secs(24 * 3600);

/// Retry delay after `failures` consecutive passes left due work unfinished:
/// 0.5 s doubling to 5 s (a pending beacon is fetched at most every 5 s anyway).
fn backoff(failures: u32) -> Duration {
    Duration::from_millis(500u64 << failures.saturating_sub(1).min(4)).min(Duration::from_secs(5))
}

/// What the loop keeps between passes.
struct Schedule {
    /// The first fence rehydrates at once. One that recurs within five
    /// minutes backs off 1s..16s instead of reloading the ring again and again.
    fences: u32,
    last_fence: std::time::Instant,
    hydrate_after: std::time::Instant,
    /// A failed slot recovery holds back draws and settlement until it is retried.
    recover_after: Option<std::time::Instant>,
    /// Games whose unsettled periods in SQL may not all fit their memory
    /// slots. Set by every hydration; a game leaves once a recovery query
    /// returns fewer periods than it had free slots.
    backlog: Vec<Game>,
    /// Stranded periods found at this start, reopened one at a time.
    stranded: Option<VecDeque<(String, i64)>>,
    stranded_after: std::time::Instant,
    retention_due: std::time::Instant,
    scratch_due: std::time::Instant,
    failures: u32,
}

/// Event driven: each pass does the work that is due, then the loop sleeps
/// until the next known deadline (next period opening or draw, next scratch
/// expiry, next retention, a retry) or until a write commits or is refused
/// (purchase, period change, draw, settlement: a slot may be free, settlement
/// may continue). Nothing is polled while idle; readiness gates are awaited.
pub async fn run(pool: PgPool) {
    let state = state(&pool);
    let now = std::time::Instant::now();
    let mut schedule = Schedule {
        fences: 0,
        last_fence: now,
        hydrate_after: now,
        recover_after: None,
        backlog: Game::DRAWN.to_vec(),
        stranded: None,
        stranded_after: now,
        retention_due: now + Duration::from_secs(3600),
        scratch_due: now,
        failures: 0,
    };
    loop {
        if !gates_ready(&pool).await {
            break;
        }
        let next = pass(&pool, &mut schedule).await;
        if !crate::write_behind::accepting() {
            break;
        }
        // `notify_one` keeps a permit, so a commit during the pass is not lost.
        tokio::select! {
            _ = state.wake.notified() => {}
            _ = tokio::time::sleep_until(next.into()) => {}
            _ = crate::write_behind::shutdown_started() => {}
        }
    }
}

/// Waits until the lottery may run here: this process is the realtime owner,
/// startup recovery finished, and the wallet and player directory are loaded.
/// False once the process stops accepting (release handoff draining).
async fn gates_ready(pool: &PgPool) -> bool {
    let mut wait = Duration::from_millis(20);
    loop {
        if !crate::write_behind::accepting() {
            return false;
        }
        let configured = crate::runtime_owner::configured();
        if crate::realtime_mode::enabled()
            && (!configured || crate::runtime_owner::recovery_ready(pool))
            && beans_read_memory::ready(pool)
            && crate::player_directory::ready(pool)
        {
            return true;
        }
        if configured && !crate::runtime_owner::active() {
            // Standby: woken when this process takes ownership.
            tokio::select! {
                _ = crate::runtime_owner::wait_active() => {}
                _ = crate::write_behind::shutdown_started() => {}
            }
        } else if !configured && !crate::realtime_mode::enabled() {
            // No single owner configured: the lottery never runs here.
            crate::write_behind::shutdown_started().await;
        } else if configured && !crate::runtime_owner::recovery_ready(pool) {
            tokio::select! {
                _ = crate::runtime_owner::wait_recovery_ready(pool) => {}
                _ = crate::write_behind::shutdown_started() => {}
            }
        } else {
            // Wallet or directory still loading (development mode without a
            // host owner, or lagging the recovery barrier).
            tokio::select! {
                _ = tokio::time::sleep(wait) => {}
                _ = crate::write_behind::shutdown_started() => {}
            }
            wait = (wait * 2).min(Duration::from_secs(1));
        }
    }
}

/// One pass over everything due; returns when the loop needs to look again.
async fn pass(pool: &PgPool, s: &mut Schedule) -> std::time::Instant {
    let state = state(pool);
    let now = std::time::Instant::now();
    if discard_fenced(pool).await {
        s.fences = if now.duration_since(s.last_fence) < Duration::from_secs(300) {
            s.fences.saturating_add(1)
        } else {
            1
        };
        s.last_fence = now;
        s.hydrate_after = now
            + match s.fences {
                0 | 1 => Duration::ZERO,
                n => Duration::from_secs(1 << (n - 2).min(4)),
            };
    } else if state
        .refused
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .is_some()
    {
        // The writer refused an event and is about to stop; discard it then.
        return now + Duration::from_millis(20);
    }
    if !ready(pool) {
        if now < s.hydrate_after {
            return s.hydrate_after;
        }
        let started = std::time::Instant::now();
        let result = hydrate(pool).await;
        tracing::info!(
            stage = "lottery::runtime",
            elapsed_ms = started.elapsed().as_millis() as u64,
            success = result.is_ok(),
            "runtime_memory_hydration_finished"
        );
        if let Err(error) = result {
            tracing::warn!(%error, "lottery_hydration_retry");
            s.failures = s.failures.saturating_add(1);
            s.hydrate_after = std::time::Instant::now() + backoff(s.failures);
            return s.hydrate_after;
        }
        s.backlog = Game::DRAWN.to_vec();
        s.recover_after = None;
    }
    if s.recover_after.is_some_and(|after| now < after) {
        return s.recover_after.unwrap_or(now);
    }
    {
        let _command = state.commands.lock().await;
        if let Err(error) = recover_slots(pool, &mut s.backlog).await {
            tracing::warn!(?error, "lottery_ring_recovery_waiting");
            s.failures = s.failures.saturating_add(1);
            let after = std::time::Instant::now() + backoff(s.failures);
            s.recover_after = Some(after);
            return after;
        }
        s.recover_after = None;
    }
    let mut retry = false;
    let mut again = false;
    if s.stranded.is_none() && now >= s.stranded_after {
        match persistence::stranded(pool).await {
            Ok((periods, unusable)) => {
                if !periods.is_empty() || unusable > 0 {
                    tracing::error!(
                        alert_error = "Lottery tickets were left unsettled in periods already marked settled",
                        alert_cause = "a period was marked settled without these tickets (a second backend wrote the same database); the runtime now settles them from the stored draw, once / 有期次在这些彩票未结算时被标记为已结算（另一个后端写了同一数据库），运行时将按已存开奖结果补结算一次",
                        alert_action = "AI team checks the later lottery_stranded_period_reopened lines and anything counted as unusable (a period without stored plan or draw, a ticket whose period row is gone) and settles those by hand; Loong action: none / AI 核对后续 lottery_stranded_period_reopened 日志，并手工处理无法自动结算的期次或彩票，Loong 无需操作",
                        "[lottery_stranded_tickets] periods={} unusable={unusable}",
                        periods.len()
                    );
                }
                s.stranded = Some(periods.into());
            }
            Err(error) => {
                tracing::warn!(%error, "lottery_stranded_scan_retry");
                s.stranded_after = std::time::Instant::now() + Duration::from_secs(60);
            }
        }
    }
    if let Some(queue) = s.stranded.as_mut().filter(|queue| !queue.is_empty()) {
        // One per pass; with none fitting, a freed slot (a commit) wakes the loop.
        match reopen_stranded(pool, queue).await {
            Ok(reopened) => again |= reopened,
            Err(error) => {
                tracing::warn!(?error, "lottery_stranded_reopen_waiting");
                retry = true;
            }
        }
    }
    if state.retention_more.swap(false, Ordering::AcqRel) {
        s.retention_due = s.retention_due.min(now + Duration::from_secs(60));
    }
    if now >= s.retention_due {
        let mut lock = state.data.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(data) = lock.as_mut() {
            let event = Event::new(Game::Ssq, None, "retention", Uuid::new_v4());
            s.retention_due = std::time::Instant::now()
                + if admit(pool, &state, data, event, 0).is_ok() {
                    DAY
                } else {
                    // The writer is busy or failing: never a past-due deadline.
                    backoff(s.failures.saturating_add(1))
                };
        }
    }
    for game in Game::DRAWN {
        if let Err(error) = advance(pool, game).await {
            tracing::warn!(
                ?error,
                game = game.as_str(),
                "lottery_memory_advance_waiting"
            );
            retry = true;
        }
    }
    retry |= settle_all(pool).await;
    if std::time::Instant::now() >= s.scratch_due {
        s.scratch_due = std::time::Instant::now() + expire_scratch(pool).await;
    }
    // Draw work from memory: the next period boundary, the next draw, and a
    // draw that is due but still waits (its beacon is not out yet).
    let (next_ms, due) = drawn_deadline(pool);
    retry |= due;
    let now = std::time::Instant::now();
    let mut next = (now
        + Duration::from_millis(next_ms.saturating_sub(super::now_ms()).max(0) as u64 + 1))
    .min(s.retention_due)
    .min(s.scratch_due);
    if s.stranded.is_none() {
        next = next.min(s.stranded_after);
    }
    if retry {
        s.failures = s.failures.saturating_add(1);
        next = next.min(now + backoff(s.failures));
    } else {
        s.failures = 0;
    }
    if again {
        next = now;
    }
    next
}

/// Settles drawn periods until none is left, `SETTLE_BATCH` events between
/// yields. Pauses while `SETTLE_IN_FLIGHT` writes are queued: each commit
/// wakes the loop to continue. True when work is left that only a retry can
/// finish (an error, or an owner whose wallet or directory row was unreadable).
async fn settle_all(pool: &PgPool) -> bool {
    let state = state(pool);
    let mut settled = 0usize;
    loop {
        if accepted(&state).len() >= SETTLE_IN_FLIGHT {
            return false;
        }
        match settle(pool) {
            Ok(true) => {
                settled += 1;
                if settled % SETTLE_BATCH == 0 {
                    tokio::task::yield_now().await;
                }
            }
            Ok(false) => {
                let lock = state.data.lock().unwrap_or_else(|e| e.into_inner());
                return lock.as_ref().is_some_and(|data| {
                    data.periods
                        .iter()
                        .any(|(key, r)| r.status == "drawn" && !parked().periods.contains(key))
                });
            }
            Err(error) => {
                tracing::warn!(?error, "lottery_memory_settlement_waiting");
                return true;
            }
        }
    }
}

/// The next draw-game deadline (wall-clock ms: the next period boundary or
/// an earlier open period's draw) and whether a draw is already due.
fn drawn_deadline(pool: &PgPool) -> (i64, bool) {
    let now = super::now_ms();
    let mut next = Game::DRAWN
        .iter()
        .filter_map(|game| game.schedule())
        .map(|schedule| schedule.period_at(now).draw_ms)
        .min()
        .unwrap_or(now + 60_000);
    let mut due = false;
    let state = state(pool);
    let lock = state.data.lock().unwrap_or_else(|e| e.into_inner());
    if let Some(data) = lock.as_ref() {
        for (key, row) in data.periods.iter().filter(|(_, r)| r.status == "open") {
            let Some(plan) = row
                .proof_plan
                .clone()
                .and_then(|p| serde_json::from_value::<draw_games::lottery::Plan>(p).ok())
            else {
                continue;
            };
            if plan.draw_at_ms > now {
                next = next.min(plan.draw_at_ms);
            } else if !parked().periods.contains(key) {
                due = true;
            }
        }
    }
    (next, due)
}

/// Reveals scratch cards 30 days after purchase; returns the wait until the
/// next sweep: none while full batches keep coming, else until the oldest
/// unsettled card expires (at most a day, at least `SCRATCH_RETRY`).
async fn expire_scratch(pool: &PgPool) -> Duration {
    let skip: Vec<Uuid> = parked().cards.iter().copied().collect();
    let rows = match sqlx::query(&format!("SELECT id,user_id FROM lottery_tickets WHERE game='scratch' AND cost=100 AND {} AND settled_at IS NULL AND bought_at < now()-interval '30 days' AND NOT (id=ANY($1)) ORDER BY bought_at,id LIMIT {SCRATCH_BATCH}", super::scratch::REWARD_VERSION_SQL)).bind(&skip).fetch_all(pool).await {
        Ok(rows) => rows,
        Err(error) => {
            tracing::warn!(%error, "lottery_scratch_expiry_retry");
            return SCRATCH_RETRY;
        }
    };
    let mut revealed = 0usize;
    let mut last = None;
    for row in &rows {
        let (user, id): (Uuid, Uuid) = (row.get("user_id"), row.get("id"));
        if parked().cards.contains(&id) {
            continue;
        }
        let state = state(pool);
        let _command = state.commands.lock().await;
        if prepare_scratch(pool, user, Some(id)).await.is_ok() {
            if let Ok((_, _, receipt)) = reveal_card(pool, user, id, None, true) {
                revealed += 1;
                last = Some(receipt);
            }
        }
    }
    if rows.len() == SCRATCH_BATCH && revealed == SCRATCH_BATCH {
        // Re-sweep once the batch is durable, so SQL no longer returns it.
        if let Some(receipt) = last {
            if receipt.wait().await.is_ok() {
                return Duration::ZERO;
            }
        }
        return SCRATCH_RETRY;
    }
    let skip: Vec<Uuid> = parked().cards.iter().copied().collect();
    match sqlx::query_scalar::<_, Option<DateTime<Utc>>>(&format!("SELECT min(bought_at) FROM lottery_tickets WHERE game='scratch' AND cost=100 AND {} AND settled_at IS NULL AND NOT (id=ANY($1))", super::scratch::REWARD_VERSION_SQL)).bind(&skip).fetch_one(pool).await {
        // A card bought from now on expires after a full 30 days.
        Ok(oldest) => (oldest.unwrap_or_else(Utc::now) + chrono::Duration::days(30) - Utc::now())
            .to_std()
            .unwrap_or_default()
            .clamp(SCRATCH_RETRY, DAY),
        Err(error) => {
            tracing::warn!(%error, "lottery_scratch_expiry_retry");
            SCRATCH_RETRY
        }
    }
}
/// Reopens the next stranded period for settlement when its game has a slot
/// to spare beside the period on sale; the ordinary settlement loop then pays
/// its unsettled tickets and marks it settled again. Exactly once: a ticket
/// is settled in the same transaction as its credit, and only unsettled
/// tickets are ever paid. True when a period left the queue.
async fn reopen_stranded(
    pool: &PgPool,
    queue: &mut VecDeque<(String, i64)>,
) -> Result<bool, Error> {
    let state = state(pool);
    let _command = state.commands.lock().await;
    // The first queued period whose game has room; one game's full slots
    // never hold back another game's recovery.
    let next = {
        let lock = state.data.lock().unwrap_or_else(|e| e.into_inner());
        let data = lock.as_ref().ok_or_else(unavailable)?;
        queue.retain(|key| !data.periods.contains_key(key));
        queue
            .iter()
            .position(|(game, _)| {
                data.periods
                    .range((game.clone(), i64::MIN)..=(game.clone(), i64::MAX))
                    .count()
                    + 2
                    <= PERIOD_SLOTS
            })
            .and_then(|index| queue.remove(index))
    };
    let Some((game, period)) = next else {
        return Ok(false);
    };
    // Taken off the queue for good: a period that cannot be loaded or does
    // not fit is reported once, not reloaded under the command lock forever.
    let reopened = match persistence::load_stranded(pool, &game, period).await {
        Ok(Some(reopened)) => reopened,
        Ok(None) => return Ok(true),
        Err(error) => {
            tracing::error!(%error, game, period, "lottery_stranded_period_unusable");
            return Ok(true);
        }
    };
    let mut lock = state.data.lock().unwrap_or_else(|e| e.into_inner());
    let data = lock.as_mut().ok_or_else(unavailable)?;
    let mut public = state.published.lock().unwrap_or_else(|e| e.into_inner());
    let public = public.as_mut().ok_or_else(unavailable)?;
    if data.bytes.saturating_add(reopened.bytes) > MAX_BYTES
        || public.data.bytes.saturating_add(reopened.bytes) > MAX_BYTES
    {
        tracing::error!(game, period, "lottery_stranded_period_unusable");
        return Ok(true);
    }
    let unsettled = reopened
        .tickets
        .values()
        .filter(|t| t.card.settled_at.is_none())
        .count();
    public.data.periods.extend(reopened.periods.clone());
    for ticket in reopened.tickets.values() {
        public.data.insert(ticket.clone());
    }
    public.data.compact();
    data.periods.extend(reopened.periods);
    for ticket in reopened.tickets.into_values() {
        data.insert(ticket);
    }
    data.compact();
    tracing::info!(game, period, unsettled, "lottery_stranded_period_reopened");
    Ok(true)
}

/// Retires durable settled periods, then refills free slots from SQL, but
/// only for `backlog` games: elsewhere every unsettled period is in memory.
async fn recover_slots(pool: &PgPool, backlog: &mut Vec<Game>) -> Result<(), Error> {
    let state = state(pool);
    let (keys, wanted) = {
        let mut lock = state.data.lock().unwrap_or_else(|e| e.into_inner());
        let data = lock.as_mut().ok_or_else(unavailable)?;
        if data.revision != data.durable {
            return Ok(());
        }
        data.retire_durable_periods();
        if let Some(public) = state
            .published
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .as_mut()
        {
            public.data.retire_durable_periods();
        }
        let wanted = backlog
            .iter()
            .map(|game| {
                let held = data
                    .periods
                    .range((game.as_str().into(), i64::MIN)..=(game.as_str().into(), i64::MAX))
                    .count();
                (*game, PERIOD_SLOTS.saturating_sub(held))
            })
            .filter(|(_, free)| *free > 0)
            .collect::<Vec<_>>();
        if wanted.is_empty() {
            return Ok(());
        }
        (data.periods.keys().cloned().collect::<Vec<_>>(), wanted)
    };
    let games = wanted.iter().map(|(game, _)| *game).collect::<Vec<_>>();
    let recovered = persistence::hydrate_missing(pool, &keys, &games).await?;
    // A game given fewer periods than it had free slots has none left in SQL
    // outside memory: new periods are only ever created by this runtime.
    let drained = wanted
        .iter()
        .filter(|(game, free)| {
            recovered
                .periods
                .keys()
                .filter(|(g, _)| g == game.as_str())
                .count()
                < *free
        })
        .map(|(game, _)| *game)
        .collect::<Vec<_>>();
    let mut lock = state.data.lock().unwrap_or_else(|e| e.into_inner());
    let data = lock.as_mut().ok_or_else(unavailable)?;
    if data.bytes.saturating_add(recovered.bytes) > MAX_BYTES {
        return Err(unavailable());
    }
    if !recovered.periods.is_empty() || !recovered.tickets.is_empty() {
        let mut public = state.published.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(public) = public.as_mut() {
            if public.data.bytes.saturating_add(recovered.bytes) > MAX_BYTES {
                return Err(unavailable());
            }
            public.data.periods.extend(recovered.periods.clone());
            for ticket in recovered.tickets.values() {
                public.data.insert(ticket.clone());
            }
            public.data.compact();
        }
        data.periods.extend(recovered.periods);
        for ticket in recovered.tickets.into_values() {
            data.insert(ticket);
        }
        data.compact();
    }
    backlog.retain(|game| !drained.contains(game));
    tracing::debug!(
        periods = data.periods.len(),
        tickets = data.tickets.len(),
        bytes = data.bytes,
        backlog = backlog.len(),
        "lottery_ring_size"
    );
    Ok(())
}

async fn advance(pool: &PgPool, game: Game) -> Result<(), Error> {
    let state = state(pool);
    let plan = {
        let _command = state.commands.lock().await;
        let mut lock = state.data.lock().unwrap_or_else(|e| e.into_inner());
        let data = lock.as_mut().ok_or_else(unavailable)?;
        if data
            .periods
            .range((game.as_str().into(), i64::MIN)..=(game.as_str().into(), i64::MAX))
            .count()
            < PERIOD_SLOTS
        {
            match ensure_period(pool, &state, data, game) {
                Ok(()) | Err(Error::SalesClosed) => {}
                Err(e) => return Err(e),
            }
        }
        data.periods
            .iter()
            .filter(|((g, _), r)| g == game.as_str() && r.status == "open")
            .filter_map(|(_, r)| {
                r.proof_plan
                    .clone()
                    .and_then(|p| serde_json::from_value::<draw_games::lottery::Plan>(p).ok())
            })
            .find(|p| {
                p.draw_at_ms <= super::now_ms()
                    && !parked().periods.contains(&(game.as_str().into(), p.period))
            })
    };
    let Some(plan) = plan else { return Ok(()) };
    let Some(beacon) = super::beacon::fetch(plan.beacon_round)
        .await
        .map_err(Error::Proof)?
    else {
        return Ok(());
    };
    let proof = draw_games::lottery::DrawProof::new(plan.clone(), beacon).map_err(Error::Proof)?;
    let _command = state.commands.lock().await;
    let mut lock = state.data.lock().unwrap_or_else(|e| e.into_inner());
    let data = lock.as_mut().ok_or_else(unavailable)?;
    let Some(mut row) = data
        .periods
        .get(&(game.as_str().into(), plan.period))
        .filter(|r| r.status == "open" && r.proof_plan.as_ref() == Some(&json!(plan)))
        .cloned()
    else {
        return Ok(());
    };
    row.status = "drawn".into();
    row.numbers = Some(proof.numbers.clone());
    row.bonus = proof.bonus;
    row.bonus_numbers = proof.bonus_numbers.clone();
    row.drawn_at = Some(Utc::now());
    row.draw_proof = Some(json!(proof));
    let mut event = Event::new(
        game,
        None,
        "draw",
        super::record_id(&format!("draw:{}:{}", game.as_str(), plan.period)),
    );
    event.period = Some(row);
    admit(pool, &state, data, event, 0).map(|_| ())
}
fn settle(pool: &PgPool) -> Result<bool, Error> {
    let state = state(pool);
    let mut lock = state.data.lock().unwrap_or_else(|e| e.into_inner());
    let data = lock.as_mut().ok_or_else(unavailable)?;
    let periods = data
        .periods
        .iter()
        .filter(|(key, r)| r.status == "drawn" && !parked().periods.contains(*key))
        .map(|(k, r)| (k.clone(), r.clone()))
        .collect::<Vec<_>>();
    for ((g, period), mut row) in periods {
        let game = Game::parse(&g).ok_or(Error::InvalidEntry)?;
        // The period's plan fixes which prize table applies (see `prize_scale`).
        let protocol = super::prize_protocol(row.proof_plan.as_ref());
        let pending = data
            .by_period
            .get(&(g.clone(), period))
            .into_iter()
            .flatten()
            .filter_map(|id| data.tickets.get(id))
            .filter(|t| t.card.settled_at.is_none())
            .filter(|t| !parked().settlements.contains(&(g.clone(), period, t.user)))
            .cloned()
            .collect::<Vec<_>>();
        if pending.is_empty() {
            row.status = "settled".into();
            row.settled_at = Some(Utc::now());
            let mut event = Event::new(
                game,
                None,
                "period",
                super::record_id(&format!("settled:{g}:{period}")),
            );
            event.period = Some(row);
            admit(pool, &state, data, event, 0)?;
            return Ok(true);
        }
        let mut owners = std::collections::HashSet::new();
        for owner in pending.iter().map(|t| t.user).filter(|u| owners.insert(*u)) {
            match crate::player_directory::lookup(pool, owner) {
                Ok(Some(p)) if p.deleted_at.is_none_or(|at| at.timestamp() != 0) => {}
                Ok(_) => {
                    forget(data, owner);
                    return Ok(true);
                }
                Err(_) => continue,
            }
            let Ok(before) = beans_read_memory::settlement_balance(pool, owner) else {
                continue;
            };
            // One credit per (game, period, user). A user who already has
            // settled tickets in this period (only in a reopened stranded
            // period, see `persistence::stranded`) gets a receipt of its own
            // for the remaining ones, derived from the first of them.
            let mine = || {
                data.by_period
                    .get(&(g.clone(), period))
                    .into_iter()
                    .flatten()
                    .filter_map(|id| data.tickets.get(id))
                    .filter(move |t| t.user == owner)
            };
            let id = if mine().any(|t| t.card.settled_at.is_some()) {
                let first = mine()
                    .filter(|t| t.card.settled_at.is_none())
                    .map(|t| t.card.id)
                    .min()
                    .ok_or(Error::InvalidEntry)?;
                super::record_id(&format!("draw:{g}:{period}:{owner}:{first}"))
            } else {
                super::draw_credit_id(game, period, owner)
            };
            // A ticket that cannot be priced must not stop every other
            // settlement: this user's credit is skipped and reported.
            let priced = (|| -> Result<Event, Error> {
                let mut row = row.clone();
                let mut event = Event::new(game, Some(owner), "settle", id);
                event.before = before;
                let mut paid = 0i64;
                for mut t in pending.iter().filter(|t| t.user == owner).cloned() {
                    let entry =
                        Entry::from_stored(game, &t.card.entry).ok_or(Error::InvalidEntry)?;
                    let (prize, tier) = entry
                        .prize(
                            &protocol,
                            i64::from(t.card.multiplier),
                            row.numbers.as_deref().ok_or(Error::InvalidEntry)?,
                            row.bonus,
                            &row.bonus_numbers,
                        )
                        .ok_or(Error::InvalidEntry)?;
                    paid = paid.checked_add(prize).ok_or(Error::InvalidEntry)?;
                    let bonus = entry
                        .jackpot_bonus(&protocol, i64::from(t.card.multiplier), prize, tier)
                        .ok_or(Error::InvalidEntry)?;
                    if bonus > 0 {
                        // Credited by the writer as its own non-play ledger row,
                        // next to (never inside) the prize the proof recomputes.
                        t.card.entry["jackpot_bonus"] = json!(bonus);
                        event.bonus = event.bonus.checked_add(bonus).ok_or(Error::InvalidEntry)?;
                    }
                    t.card.prize = Some(prize);
                    t.card.tier = Some(tier);
                    t.card.settled_at = Some(event.at);
                    event.cards.push(t)
                }
                row.paid = row.paid.checked_add(paid).ok_or(Error::InvalidEntry)?;
                row.winners = row
                    .winners
                    .checked_add(i32::from(paid > 0))
                    .ok_or(Error::InvalidEntry)?;
                event.period = Some(row);
                paid.checked_add(event.bonus).ok_or(Error::InvalidEntry)?;
                event.effect = Some(PendingEffect {
                    delta: paid,
                    bonus: event.bonus,
                    played: 1,
                    won: i32::from(paid > 0),
                });
                Ok(event)
            })();
            let event = match priced {
                Ok(event) => event,
                Err(error) => {
                    parked().settlements.insert((g.clone(), period, owner));
                    tracing::error!(
                        alert_error = "Lottery tickets could not be settled: their stored entry cannot be priced",
                        alert_cause = "a stored ticket entry or its period's draw does not parse for the prize table; these tickets stay unsettled and unpaid while every other settlement continues / 已存彩票内容或该期开奖无法按奖表计算，这些彩票保持未结算，其他结算照常进行",
                        alert_action = "AI team inspects the user's unsettled tickets of that period and settles them by hand; Loong action: none / AI 核查该用户该期未结算彩票并手工结算，Loong 无需操作",
                        "[lottery_settlement_unpriceable] game={g} period={period} user={owner} error={error:?}"
                    );
                    return Ok(true);
                }
            };
            admit(pool, &state, data, event, 0)?;
            return Ok(true);
        }
    }
    Ok(false)
}
fn forget(data: &mut Data, user: Uuid) {
    if data.scratch_owner == Some(user) {
        data.scratch_owner = None;
    }
    data.tickets.retain(|_, t| t.user != user);
    data.receipts.retain(|(u, _), _| *u != user);
    data.offers.remove(&user);
    data.by_user.retain(|(u, _), _| *u != user);
    for ids in data.by_period.values_mut() {
        ids.retain(|id| data.tickets.contains_key(id))
    }
    for ids in data.recent.values_mut() {
        ids.retain(|id| data.tickets.contains_key(id))
    }
    data.bytes = persistence::weight(data).expect("runtime values serialize");
}

pub(crate) fn remove_user(pool: &PgPool, user: Uuid) {
    let state = state(pool);
    let mut lock = state.data.lock().unwrap_or_else(|e| e.into_inner());
    if let Some(data) = lock.as_mut() {
        if data.scratch_owner == Some(user) {
            data.scratch_owner = None;
        }
        data.tickets.retain(|_, t| t.user != user);
        data.receipts.retain(|(u, _), _| *u != user);
        data.offers.remove(&user);
        data.by_user.retain(|(u, _), _| *u != user);
        for ids in data.by_period.values_mut() {
            ids.retain(|id| data.tickets.contains_key(id));
        }
        for ids in data.recent.values_mut() {
            ids.retain(|id| data.tickets.contains_key(id));
        }
        for winners in data.winners.values_mut() {
            for winner in winners.iter_mut().filter(|w| w.user == Some(user)) {
                winner.user = None;
                winner.name = crate::display_name::fallback_display_name(user)
            }
        }
        data.presence.retain(|(_, u), _| *u != user);
        data.revision += 1;
        data.durable += 1; // Purge is already durable; it enqueues no lottery job.
        if let Some(public) = state
            .published
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .as_mut()
        {
            public.remove_user(user);
            public.data.revision += 1;
            public.data.durable += 1;
        }
        data.compact();
        for game in Game::ALL {
            let _ = state.changed.send(Notice {
                owner: Some(user),
                game: game.as_str().into(),
                revision: data.revision,
            });
        }
    }
}
