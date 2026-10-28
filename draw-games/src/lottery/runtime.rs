//! Like card-room actors: one owner, memory commands, frozen FIFO writes.
//! A normal stop drains Ordered; hard recovery restores only its committed prefix.
use super::{
    handlers::Error,
    rules::{self, Entry},
    store::{self, NumberBuy, PeriodRow, Purchase, TicketRow},
    Game,
};
use crate::{
    beans_read_memory::{self, PendingEffect},
    write_behind::Ordered,
};
use chrono::{DateTime, Utc};
use rand::{rngs::OsRng, RngCore};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use sqlx::{PgPool, Row};
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
#[path = "runtime/socket.rs"]
pub mod socket;
const MAX_ROWS: usize = 2_000_000;
const MAX_BYTES: usize = 512 * 1024 * 1024;
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
}
#[derive(Clone)]
pub(super) struct Notice {
    pub(super) owner: Option<Uuid>,
    pub(super) game: String,
    pub(super) revision: u64,
}
#[derive(Default)]
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
}

struct State {
    data: Mutex<Option<Data>>,
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
                    writer: Mutex::new(None),
                    loader: tokio::sync::Mutex::new(()),
                    commands: tokio::sync::Mutex::new(()),
                    changed: tokio::sync::broadcast::channel(128).0,
                    queued: AtomicUsize::new(0),
                    accepted: Mutex::new(VecDeque::new()),
                    floor: AtomicU64::new(0),
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
fn check_owner() -> Result<(), Error> {
    if !crate::write_behind::accepting()
        || (crate::runtime_owner::configured() && !crate::runtime_owner::active())
    {
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
        self.receipts.clear();
        // Compaction re-serializes the whole working set under the data lock;
        // the idle tick calls this every second, so only pay when a row left.
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
            if let Some(offer) = &event.offer {
                self.bytes = self
                    .bytes
                    .saturating_sub(self.offers.get(&user).map_or(0, resident))
                    .saturating_add(resident(offer));
                self.offers.insert(user, offer.clone());
            }
            if event.claim.is_some() {
                if let Some(offer) = self.offers.remove(&user) {
                    self.bytes = self.bytes.saturating_sub(resident(&offer));
                }
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
            .by_period
            .get(&(game.as_str().into(), period))
            .into_iter()
            .flatten()
            .filter_map(|id| self.tickets.get(id))
            .filter(|t| t.user == user)
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
) -> Result<(), Error> {
    check_owner()?;
    let bytes = serde_json::to_vec(&event)
        .map_err(|_| Error::InvalidEntry)?
        .len()
        .saturating_mul(4);
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
    });
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
    let _ = state.changed.send(Notice {
        owner: event.owner,
        game: event.game,
        revision: data.revision,
    });
    Ok(())
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
    let writer = Ordered::new(None::<Arc<Job>>, move |_old, next: Option<Arc<Job>>| {
        let (pool, state) = (worker_pool.clone(), worker_state.clone());
        async move {
            if let Some(job) = &next {
                let grants = persistence::persist(&pool, &job.event).await.map_err(|e| {
                    let error = format!(
                        "lottery event={} game={} kind={}: {e}",
                        job.event.id, job.event.game, job.event.kind
                    );
                    if persistence::is_permanent(&e) {
                        crate::write_behind::fenced(&error)
                    } else {
                        error
                    }
                })?;
                accepted(&state).retain(|queued| !Arc::ptr_eq(queued, job));
                if let Some(owner) = job.event.owner {
                    if job.event.effect.is_some() {
                        beans_read_memory::effect_committed(&pool, owner, job.event.id)
                    }
                }
                {
                    let mut inner = state.data.lock().unwrap_or_else(|e| e.into_inner());
                    if let Some(data) = inner.as_mut() {
                        for (id, grant) in grants {
                            if let Some(mut t) = data.tickets.get(&id).cloned() {
                                t.card.entry["reward_grant"] = json!(grant);
                                data.insert(t);
                            }
                        }
                        data.durable += 1;
                        let _ = state.changed.send(Notice {
                            owner: job.event.owner,
                            game: job.event.game.clone(),
                            revision: data.revision,
                        });
                    }
                }
                if let Some(work) = &job.work {
                    work.complete()
                }
                state.queued.fetch_sub(job.bytes, Ordering::AcqRel);
            }
            Ok(next)
        }
    });
    *state.writer.lock().unwrap_or_else(|e| e.into_inner()) = Some(writer);
    *state.data.lock().unwrap_or_else(|e| e.into_inner()) = Some(data);
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

fn accepted(state: &State) -> std::sync::MutexGuard<'_, VecDeque<Arc<Job>>> {
    state.accepted.lock().unwrap_or_else(|e| e.into_inner())
}

/// A permanent persist error fences the writer: nothing queued can commit, so
/// memory no longer matches SQL. Drop the ring and every accepted-but-unwritten
/// job (their bean effects reload from SQL, their purge holds release) and let
/// the run loop hydrate again. Accepted purchases that never committed vanish;
/// none is charged twice.
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
    state.floor.store(revision + 1, Ordering::Release);
    let abandoned: Vec<_> = accepted(&state).drain(..).collect();
    for job in &abandoned {
        if let (Some(owner), Some(_)) = (job.event.owner, &job.event.effect) {
            beans_read_memory::effect_abandoned(pool, owner, job.event.id);
        }
        if let Some(work) = &job.work {
            work.complete()
        }
        state.queued.fetch_sub(job.bytes, Ordering::AcqRel);
    }
    tracing::error!(
        abandoned = abandoned.len(),
        revision,
        "lottery_writer_fenced_rehydrating"
    );
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
    admit(pool, state, data, event, 0)
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
    let wallet = beans_read_memory::ensure(pool, user)?;
    let available = beans_read_memory::available(pool, user, wallet.balance, false, None)?;
    let state = state(pool);
    let mut lock = state.data.lock().unwrap_or_else(|e| e.into_inner());
    let data = lock.as_mut().ok_or_else(unavailable)?;
    let now = Utc::now();
    data.presence
        .retain(|_, at| now.signed_duration_since(*at).num_seconds() < 30);
    if data.presence.len() < 4096 || data.presence.contains_key(&(game.as_str().into(), user)) {
        data.presence.insert((game.as_str().into(), user), now);
    }
    let fairness = if game == Game::Scratch {
        Some(offer(pool, &state, data, user)?)
    } else {
        ensure_period(pool, &state, data, game)?;
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
        json!({"game":game.as_str(),"runtime_epoch":crate::server_performance::process_instance_id(),"revision":data.revision,"durable_revision":data.durable,"now_ms":super::now_ms(),"balance":wallet.balance,"available_balance":available,"unit":game.unit(),"period":period,"my_tickets":my,"last_draw":null,"recent":[],"crowd":{"present":present,"members":members.iter().map(|((_,u),at)|json!({"user_id":u,"name":name(pool,*u),"seen_at_ms":at.timestamp_millis()})).collect::<Vec<_>>()},"feed":[],"fairness_offer":fairness}),
    )
}

// SQL history and replay are read outside the synchronous runtime lock. The
// command gate prevents eviction or rollover between preflight and acceptance.
async fn durable(pool: &PgPool) -> Result<(), Error> {
    let state = state(pool);
    let receipt = state
        .writer
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .as_ref()
        .ok_or_else(unavailable)?
        .durable();
    tokio::time::timeout(Duration::from_secs(2), receipt)
        .await
        .map_err(|_| unavailable())?
        .map_err(|_| unavailable())
}

async fn wallet_ready(pool: &PgPool, user: Uuid) -> Result<(), Error> {
    beans_read_memory::ensure(pool, user)?;
    beans_read_memory::initial_grant_ready(pool, user).await?;
    Ok(())
}

async fn prepare_scratch(pool: &PgPool, user: Uuid, ticket: Option<Uuid>) -> Result<(), Error> {
    durable(pool).await?;
    let mut conn = pool.acquire().await?;
    let mut cards = super::scratch::my_cards(&mut conn, user).await?;
    if let Some(id) = ticket {
        if !cards.iter().any(|c| c.id == id) {
            if let Some(row) = sqlx::query(&format!("SELECT {} FROM lottery_tickets WHERE id=$1 AND user_id=$2 AND game='scratch' AND cost=100 AND entry->'reward_version'='1'::jsonb", store::TICKET_COLUMNS))
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
    let state = state(pool);
    let mut lock = state.data.lock().unwrap_or_else(|e| e.into_inner());
    let data = lock.as_mut().ok_or_else(unavailable)?;
    // No accepted writes remain, so every evicted card/receipt is queryable.
    let retained = data
        .tickets
        .values()
        .filter(|t| t.game != "scratch")
        .map(resident)
        .sum::<usize>()
        + data.periods.values().map(resident).sum::<usize>();
    let incoming = cards
        .iter()
        .map(|card| {
            resident(&OwnedTicket {
                user,
                game: "scratch".into(),
                card: card.clone(),
            })
        })
        .sum::<usize>();
    if retained.saturating_add(incoming).saturating_add(4096) > MAX_BYTES {
        return Err(unavailable());
    }
    data.tickets.retain(|_, t| t.game != "scratch");
    // Drop the evicted cards' index entries too: `insert` re-indexes any id
    // missing from `tickets`, so stale ids would duplicate on every view.
    data.by_user.retain(|(_, g), _| g != "scratch");
    data.by_period.retain(|(g, _), _| g != "scratch");
    data.recent.remove("scratch");
    data.receipts.clear();
    data.offers.clear();
    for card in cards {
        data.insert(OwnedTicket {
            user,
            game: "scratch".into(),
            card,
        });
    }
    if let Some(row) = offer {
        data.offers.insert(
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
    data.compact();
    if data.bytes > MAX_BYTES {
        return Err(unavailable());
    }
    Ok(())
}

pub async fn view(pool: &PgPool, user: Uuid, game: Game) -> Result<Value, Error> {
    let state = state(pool);
    let command = state.commands.lock().await;
    if game == Game::Scratch {
        // The scratch view admits a fairness offer whose persistence locks the
        // wallet row; a user whose wallet row is not durable yet would wedge
        // the single ordered writer with RowNotFound retries.
        wallet_ready(pool, user).await?;
        prepare_scratch(pool, user, None).await?;
    }
    let mut view = current_view(pool, user, game)?;
    drop(command);
    // A state push must not expose an accepted purchase before its commit either.
    durable(pool).await?;
    store::decorate_history(pool, user, game, &mut view).await?;
    Ok(view)
}

pub async fn buy_numbers(
    pool: &PgPool,
    user: Uuid,
    game: Game,
    body: &NumberBuy,
) -> Result<Purchase, Error> {
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
    let command = state.commands.lock().await;
    let replay = store::replay_request(
        &mut *pool.acquire().await?,
        game,
        user,
        body.request_id,
        &hash,
    )
    .await?;
    let purchase = if let Some(p) = replay {
        p
    } else {
        wallet_ready(pool, user).await?;
        accept_numbers(pool, user, game, body)?
    };
    drop(command);
    durable(pool).await?;
    Ok(purchase)
}

pub async fn buy_scratch(
    pool: &PgPool,
    user: Uuid,
    body: &super::scratch::ScratchBuy,
) -> Result<Purchase, Error> {
    player(pool, user)?;
    let mut identity = json!({"game":"scratch","cost":body.cost,"count":body.count,"fairness":{"offer_id":body.fairness.offer_id,"commitment":body.fairness.commitment,"client_seed":body.fairness.client_seed}});
    if body.all_in {
        identity["all_in"] = json!(true);
        identity["expected_balance"] = json!(body.expected_balance);
    }
    let hash = store::body_sha256(&identity);
    let state = state(pool);
    let command = state.commands.lock().await;
    prepare_scratch(pool, user, None).await?;
    let replay = store::replay_request(
        &mut *pool.acquire().await?,
        Game::Scratch,
        user,
        body.request_id,
        &hash,
    )
    .await?;
    let purchase = if let Some(p) = replay {
        p
    } else {
        wallet_ready(pool, user).await?;
        accept_scratch(pool, user, body)?
    };
    drop(command);
    durable(pool).await?;
    Ok(purchase)
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
    let result = accept_reveal(pool, user, ticket, index)?;
    drop(command);
    durable(pool).await?;
    Ok(result)
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
) -> Result<Purchase, Error> {
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
        return Ok(p);
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
    if plan.commitment != body.proof_commitment {
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
        balance: -charged,
        net: -charged,
        played: 0,
        won: 0,
        ledger_delta: -charged,
        day: None,
    });
    let purchased = event
        .cards
        .iter()
        .map(|t| store::ticket_json(game, &t.card))
        .collect();
    admit(pool, &state, data, event, charged)?;
    Ok(Purchase {
        purchased,
        charged,
        balance_before: before,
        balance_after: receipt.after,
    })
}

fn scratch_reward(card: &TicketRow) -> Result<(i64, bool), Error> {
    let reward: Option<draw_games::scratch::Reward> =
        serde_json::from_value(card.entry["reward"].clone()).map_err(|_| Error::InvalidEntry)?;
    Ok(match reward {
        Some(draw_games::scratch::Reward::Beans { amount }) => (amount, false),
        Some(draw_games::scratch::Reward::Membership { tier, days })
            if tier == "max" && days == 30 =>
        {
            (0, true)
        }
        Some(_) => return Err(Error::InvalidEntry),
        None => (0, false),
    })
}
fn accept_scratch(
    pool: &PgPool,
    user: Uuid,
    body: &super::scratch::ScratchBuy,
) -> Result<Purchase, Error> {
    player(pool, user)?;
    if body.request_id.is_nil()
        || body.cost != 100
        || (!body.all_in && ![1, 5, 10].contains(&body.count))
        || (body.all_in && (body.count == 0 || body.expected_balance.is_none()))
        || (!body.all_in && body.expected_balance.is_some())
    {
        return Err(Error::InvalidEntry);
    }
    if body.count > draw_games::scratch::MAX_BATCH_CARDS {
        return Err(Error::TooManyTickets);
    }
    let batch = body.count > 1 || body.all_in;
    let mut identity = json!({"game":"scratch","cost":body.cost,"count":body.count,"fairness":{"offer_id":body.fairness.offer_id,"commitment":body.fairness.commitment,"client_seed":body.fairness.client_seed}});
    if body.all_in {
        identity["all_in"] = json!(true);
        identity["expected_balance"] = json!(body.expected_balance)
    }
    let hash = store::body_sha256(&identity);
    let state = state(pool);
    let mut lock = state.data.lock().unwrap_or_else(|e| e.into_inner());
    let data = lock.as_mut().ok_or_else(unavailable)?;
    if let Some(p) = data.replay(user, Game::Scratch, body.request_id, &hash)? {
        return Ok(p);
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
    let rewards = draw_games::scratch::rewards(&reveal, body.cost, body.count)
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
    let mut paid = 0i64;
    for (i, card) in generated.into_iter().enumerate() {
        let entry = json!({"cost":body.cost,"lucky":card.lucky,"numbers":card.numbers,"fairness":{"kind":"scratch","reveal":reveal,"cost":body.cost,"count":body.count,"index":i,"numbers":card.numbers,"lucky":card.lucky,"prize":card.prize,"hits":card.hits},"reward_version":1,"reward":rewards[i]});
        let ticket = TicketRow {
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
            let (bonus, max) = scratch_reward(&ticket)?;
            event.bonus = event.bonus.checked_add(bonus).ok_or(Error::InvalidEntry)?;
            if max {
                event.max_cards.push(ticket.id)
            }
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
        balance: delta + event.bonus,
        net: delta,
        played: i32::from(batch),
        won: i32::from(batch && paid > 0),
        ledger_delta: delta,
        day: None,
    });
    let purchased = event
        .cards
        .iter()
        .map(|t| store::ticket_json(Game::Scratch, &t.card))
        .collect();
    admit(pool, &state, data, event, charged)?;
    Ok(Purchase {
        purchased,
        charged,
        balance_before: before,
        balance_after: after,
    })
}

fn accept_reveal(
    pool: &PgPool,
    user: Uuid,
    ticket: Uuid,
    index: Option<usize>,
) -> Result<(Value, i64), Error> {
    player(pool, user)?;
    reveal_card(pool, user, ticket, index, false)
}
fn reveal_card(
    pool: &PgPool,
    user: Uuid,
    ticket: Uuid,
    index: Option<usize>,
    expiry: bool,
) -> Result<(Value, i64), Error> {
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
                && t.card.entry["reward_version"] == 1
        })
        .cloned()
        .ok_or(Error::TicketNotFound)?;
    if owned.card.settled_at.is_some() {
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
        return Ok((store::ticket_json(Game::Scratch, &owned.card), before));
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
        let (bonus, max) = scratch_reward(&owned.card)?;
        event.bonus = bonus;
        if max {
            event.max_cards.push(ticket)
        }
        event.effect = Some(PendingEffect {
            balance: paid + bonus,
            net: paid,
            played: 1,
            won: i32::from(paid > 0),
            ledger_delta: paid,
            day: None,
        });
        after = before
            .checked_add(paid)
            .and_then(|v| v.checked_add(bonus))
            .ok_or(Error::InvalidEntry)?;
    }
    let wire = store::ticket_json(Game::Scratch, &owned.card);
    event.cards.push(owned);
    admit(pool, &state, data, event, 0)?;
    Ok((wire, after))
}

pub async fn run(pool: PgPool) {
    let mut scratch_expiry_due = std::time::Instant::now();
    let mut retention_due = std::time::Instant::now() + Duration::from_secs(3600);
    // A fence that recurs soon after rehydrating backs off 2s..64s instead of
    // reloading the ring and abandoning other users' work every second.
    let (mut fences, mut last_fence) = (0u32, std::time::Instant::now());
    let mut hydrate_after = std::time::Instant::now();
    let mut tick = tokio::time::interval(Duration::from_secs(1));
    tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    loop {
        tick.tick().await;
        if !crate::write_behind::accepting() {
            break;
        }
        if crate::runtime_owner::configured()
            && (!crate::runtime_owner::active() || !crate::runtime_owner::recovery_ready(&pool))
        {
            continue;
        }
        if !beans_read_memory::ready(&pool) || !crate::player_directory::ready(&pool) {
            continue;
        }
        if discard_fenced(&pool).await {
            let now = std::time::Instant::now();
            fences = if now.duration_since(last_fence) < Duration::from_secs(300) {
                fences.saturating_add(1)
            } else {
                1
            };
            last_fence = now;
            hydrate_after = now + Duration::from_secs(1 << fences.min(6));
        }
        if !ready(&pool) {
            if std::time::Instant::now() < hydrate_after {
                continue;
            }
            let started = std::time::Instant::now();
            let result = hydrate(&pool).await;
            tracing::info!(
                stage = "lottery::runtime",
                elapsed_ms = started.elapsed().as_millis() as u64,
                success = result.is_ok(),
                "runtime_memory_hydration_finished"
            );
            if let Err(error) = result {
                tracing::warn!(%error, "lottery_hydration_retry");
                continue;
            }
        }
        {
            let state = state(&pool);
            let _command = state.commands.lock().await;
            if let Err(error) = recover_slots(&pool).await {
                tracing::warn!(?error, "lottery_ring_recovery_waiting");
                continue;
            }
        }
        if std::time::Instant::now() >= retention_due {
            let state = state(&pool);
            let mut lock = state.data.lock().unwrap_or_else(|e| e.into_inner());
            if let Some(data) = lock.as_mut() {
                let event = Event::new(Game::Ssq, None, "retention", Uuid::new_v4());
                if admit(&pool, &state, data, event, 0).is_ok() {
                    retention_due = std::time::Instant::now() + Duration::from_secs(3600);
                }
            }
        }
        for game in Game::DRAWN {
            if let Err(error) = advance(&pool, game).await {
                tracing::warn!(
                    ?error,
                    game = game.as_str(),
                    "lottery_memory_advance_waiting"
                );
            }
        }
        for _ in 0..20 {
            match settle(&pool) {
                Ok(true) => {}
                Ok(false) => break,
                Err(error) => {
                    tracing::warn!(?error, "lottery_memory_settlement_waiting");
                    break;
                }
            }
        }
        if std::time::Instant::now() >= scratch_expiry_due {
            scratch_expiry_due = std::time::Instant::now() + Duration::from_secs(60);
            let expired = sqlx::query("SELECT id,user_id FROM lottery_tickets WHERE game='scratch' AND cost=100 AND entry->'reward_version'='1'::jsonb AND settled_at IS NULL AND bought_at < now()-interval '30 days' ORDER BY bought_at,id LIMIT 20").fetch_all(&pool).await;
            if let Ok(rows) = expired {
                for row in rows {
                    let (user, id) = (row.get("user_id"), row.get("id"));
                    let state = state(&pool);
                    let _command = state.commands.lock().await;
                    if prepare_scratch(&pool, user, Some(id)).await.is_ok() {
                        let _ = reveal_card(&pool, user, id, None, true);
                    }
                }
            }
        }
    }
}
async fn recover_slots(pool: &PgPool) -> Result<(), Error> {
    let state = state(pool);
    let keys = {
        let mut lock = state.data.lock().unwrap_or_else(|e| e.into_inner());
        let data = lock.as_mut().ok_or_else(unavailable)?;
        if data.revision != data.durable {
            return Ok(());
        }
        data.retire_durable_periods();
        data.periods.keys().cloned().collect::<Vec<_>>()
    };
    let recovered = persistence::hydrate_missing(pool, &keys).await?;
    let mut lock = state.data.lock().unwrap_or_else(|e| e.into_inner());
    let data = lock.as_mut().ok_or_else(unavailable)?;
    if data.bytes.saturating_add(recovered.bytes) > MAX_BYTES {
        return Err(unavailable());
    }
    if !recovered.periods.is_empty() || !recovered.tickets.is_empty() {
        data.periods.extend(recovered.periods);
        for ticket in recovered.tickets.into_values() {
            data.insert(ticket);
        }
        data.compact();
    }
    tracing::debug!(
        periods = data.periods.len(),
        tickets = data.tickets.len(),
        bytes = data.bytes,
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
            .find(|p| p.draw_at_ms <= super::now_ms())
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
    admit(pool, &state, data, event, 0)
}
fn settle(pool: &PgPool) -> Result<bool, Error> {
    let state = state(pool);
    let mut lock = state.data.lock().unwrap_or_else(|e| e.into_inner());
    let data = lock.as_mut().ok_or_else(unavailable)?;
    let periods = data
        .periods
        .iter()
        .filter(|(_, r)| r.status == "drawn")
        .map(|(k, r)| (k.clone(), r.clone()))
        .collect::<Vec<_>>();
    for ((g, period), mut row) in periods {
        let game = Game::parse(&g).ok_or(Error::InvalidEntry)?;
        let pending = data
            .by_period
            .get(&(g.clone(), period))
            .into_iter()
            .flatten()
            .filter_map(|id| data.tickets.get(id))
            .filter(|t| t.card.settled_at.is_none())
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
            let mut event = Event::new(
                game,
                Some(owner),
                "settle",
                super::draw_credit_id(game, period, owner),
            );
            event.before = before;
            let mut paid = 0i64;
            for mut t in pending.iter().filter(|t| t.user == owner).cloned() {
                let entry = Entry::from_stored(game, &t.card.entry).ok_or(Error::InvalidEntry)?;
                let (prize, tier) = entry
                    .prize(
                        i64::from(t.card.multiplier),
                        row.numbers.as_deref().ok_or(Error::InvalidEntry)?,
                        row.bonus,
                        &row.bonus_numbers,
                    )
                    .ok_or(Error::InvalidEntry)?;
                paid = paid.checked_add(prize).ok_or(Error::InvalidEntry)?;
                if entry.awards_max(prize, tier) {
                    event.max_cards.push(t.card.id)
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
            event.effect = Some(PendingEffect {
                balance: paid,
                net: paid,
                played: 1,
                won: i32::from(paid > 0),
                ledger_delta: paid,
                day: None,
            });
            admit(pool, &state, data, event, 0)?;
            return Ok(true);
        }
    }
    Ok(false)
}
fn forget(data: &mut Data, user: Uuid) {
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
