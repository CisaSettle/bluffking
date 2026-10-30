//! Complete gold-bean authority. Poker settlements publish at queue admission;
//! other money writers fence and refresh only the affected user's baseline.
//! First-use grants are accepted in memory and persisted by an ordered worker.

use crate::request_timing::{http_phase, HttpPhase};
use crate::{beans, wallet::Wallet};
use chrono::NaiveDate;
use futures::TryStreamExt;
use serde_json::Value;
use sqlx::{PgPool, Row};
use std::collections::{BTreeSet, HashMap, HashSet, VecDeque};
use std::ops::Bound::{Excluded, Unbounded};
use std::sync::{Arc, Mutex, OnceLock, Weak};
use std::time::Duration;
use uuid::Uuid;

#[path = "beans_pending_effects.rs"]
mod pending_effects;
#[path = "beans_reservations.rs"]
mod reservations;
pub(crate) use pending_effects::{
    accept_effect, daily_claimed, effect_abandoned, effect_committed, settlement_balance,
    PendingEffect,
};
pub(crate) use reservations::release_poker_session;
pub use reservations::{
    available, reserve_poker_stack, sicbo_accepted, sicbo_committed, sync_poker_checkpoint,
};

const PAGE: i64 = 500;
const MAX_USERS: usize = 1_000_000;
const MAX_ROWS: usize = 2_000_000;
const MAX_BYTES: usize = 512 * 1024 * 1024;
const MAX_POKER_HANDS: usize = 131_072;
const RECENT_POKER_ACKS: usize = 4096;
const DAILY: &str = "beans_daily_refill";
const READ_PUBLICATION_WAIT: Duration = Duration::from_secs(2);
static READ_WAITERS: tokio::sync::Semaphore = tokio::sync::Semaphore::const_new(256);
type Rank = (std::cmp::Reverse<i64>, std::cmp::Reverse<i32>, Uuid);
#[derive(Default)]
struct User {
    wallet: Option<Wallet>,
    eligible: bool,
    qa_dirty: bool,
    days: HashSet<NaiveDate>,
    receipts: HashMap<Uuid, Arc<Value>>,
    latest: HashMap<&'static str, Recent>,
    available: bool,
    /// Hidden after account deletion while an already dealt hand still owns
    /// accounting obligations. This wallet cannot be read or admitted again.
    retired: bool,
    generation: u64,
    bytes: usize,
    /// Accepted poker deltas not yet acknowledged by their durable writer.
    poker: HashMap<Uuid, i64>,
    effects: HashMap<Uuid, PendingEffect>,
    effect_hold: Option<std::fs::File>,
}
#[derive(Clone)]
struct PokerReceipt {
    players: Vec<(Uuid, i64)>,
    committed: bool,
}
#[derive(Clone)]
pub struct Recent {
    pub record: Uuid,
    pub balance: i64,
    pub details: Arc<Value>,
    bytes: usize,
}
#[derive(Default)]
struct Inner {
    reservations: reservations::Reservations,
    ready: bool,
    epoch: u64,
    active_writes: usize,
    active_users: HashMap<Uuid, usize>,
    users: HashMap<Uuid, User>,
    ranks: BTreeSet<Rank>,
    pending: BTreeSet<Uuid>,
    count: usize,
    bytes: usize,
    writer: Option<crate::write_behind::Ordered<Option<Uuid>>>,
    poker: HashMap<Uuid, PokerReceipt>,
    poker_acknowledged: VecDeque<Uuid>,
}
#[derive(Default)]
struct State {
    inner: Mutex<Inner>,
    loader: tokio::sync::Mutex<()>,
    wake: tokio::sync::Notify,
    published: Arc<tokio::sync::Notify>,
    daily_claims: Mutex<HashMap<Uuid, Weak<tokio::sync::Mutex<()>>>>,
}
fn state(pool: &PgPool) -> Arc<State> {
    type Pools = HashMap<usize, (Weak<sqlx::postgres::PgConnectOptions>, Arc<State>)>;
    static POOLS: OnceLock<Mutex<Pools>> = OnceLock::new();
    let mut pools = POOLS
        .get_or_init(Default::default)
        .lock()
        .unwrap_or_else(|e| e.into_inner());
    pools.retain(|_, (options, _)| options.strong_count() > 0);
    pools
        .entry(crate::live_membership::pool_id(pool))
        .or_insert_with(|| (Arc::downgrade(&pool.connect_options()), Arc::default()))
        .1
        .clone()
}
/// Held before BEGIN until the transaction and its publication scope end.
/// Drop also runs when an HTTP future is cancelled during COMMIT. The loader
/// waits for server-side transactions before taking its replacement snapshot.
pub struct Mutation {
    pool: PgPool,
    users: Vec<Uuid>,
    fence: bool,
    daily_generation: Option<(Uuid, u64)>,
    daily_published: bool,
}
pub fn mutation(pool: &PgPool, users: impl IntoIterator<Item = Uuid>) -> Mutation {
    begin_mutation(pool, users, true)
}
fn begin_mutation(pool: &PgPool, users: impl IntoIterator<Item = Uuid>, fence: bool) -> Mutation {
    let state = state(pool);
    {
        let mut inner = state.inner.lock().unwrap_or_else(|e| e.into_inner());
        inner.epoch = inner.epoch.wrapping_add(1);
        inner.active_writes += 1;
    }
    let mut guard = Mutation {
        pool: pool.clone(),
        users: Vec::new(),
        fence,
        daily_generation: None,
        daily_published: false,
    };
    for user in users {
        guard.watch(user);
    }
    guard
}

/// Both daily HTTP endpoints share this bounded, per-user publication order.
/// Other wallet writers keep their normal transaction and generation fences.
pub async fn lock_daily(
    pool: &PgPool,
    user: Uuid,
) -> sqlx::Result<tokio::sync::OwnedMutexGuard<()>> {
    let state = state(pool);
    let lock = {
        let mut claims = state.daily_claims.lock().unwrap_or_else(|e| e.into_inner());
        claims.retain(|_, lock| lock.strong_count() != 0);
        if let Some(lock) = claims.get(&user).and_then(Weak::upgrade) {
            lock
        } else {
            if claims.len() >= 1024 {
                return Err(unavailable());
            }
            let lock = Arc::new(tokio::sync::Mutex::new(()));
            claims.insert(user, Arc::downgrade(&lock));
            lock
        }
    };
    tokio::time::timeout(Duration::from_secs(5), lock.lock_owned())
        .await
        .map_err(|_| sqlx::Error::PoolTimedOut)
}

/// A daily transaction changes only balance and today's entitlement. Remember
/// whether the remaining projection was complete before fencing it, atomically
/// with registering this writer; an overlapping writer prevents publication.
pub fn daily_mutation(pool: &PgPool, user: Uuid) -> Mutation {
    let state = state(pool);
    let mut inner = state.inner.lock().unwrap_or_else(|e| e.into_inner());
    let readable = inner.ready
        && !inner.active_users.contains_key(&user)
        && inner.users.get(&user).is_some_and(|u| {
            u.available && u.wallet.is_some() && u.poker.is_empty() && u.effects.is_empty()
        });
    inner.active_writes += 1;
    *inner.active_users.entry(user).or_default() += 1;
    invalidate_locked(&mut inner, user);
    let daily_generation = readable.then_some((user, inner.epoch));
    state.wake.notify_one();
    Mutation {
        pool: pool.clone(),
        users: vec![user],
        fence: true,
        daily_generation,
        daily_published: false,
    }
}
impl Mutation {
    /// Call only after acknowledged COMMIT, or an acknowledged no-op rollback.
    /// The database supplies the exact Singapore day, including midnight races.
    pub fn publish_daily(mut self, day: NaiveDate, balance: i64) {
        let Some((id, generation)) = self.daily_generation else {
            return;
        };
        let state = state(&self.pool);
        let mut inner = state.inner.lock().unwrap_or_else(|e| e.into_inner());
        if !inner.ready || inner.active_users.get(&id) != Some(&1) {
            return;
        }
        let Some(user) = inner.users.get(&id).filter(|u| u.generation == generation) else {
            return;
        };
        let added = usize::from(!user.days.contains(&day));
        if inner.count + added > MAX_ROWS || inner.bytes + added * 64 > MAX_BYTES {
            return;
        }
        let user = inner.users.get_mut(&id).expect("verified daily projection");
        user.wallet.as_mut().expect("complete daily wallet").balance = balance;
        user.days.insert(day);
        user.bytes += added * 64;
        user.available = true;
        let key = rank(id, user);
        inner.count += added;
        inner.bytes += added * 64;
        if let Some(key) = key {
            inner.ranks.insert(key);
        }
        inner.pending.remove(&id);
        self.daily_published = true;
        state.published.notify_waiters();
    }
    /// Register an owner discovered by a transaction before its first wallet write.
    pub fn watch(&mut self, user: Uuid) {
        if self.users.contains(&user) {
            return;
        }
        self.users.push(user);
        let state = state(&self.pool);
        {
            let mut inner = state.inner.lock().unwrap_or_else(|e| e.into_inner());
            *inner.active_users.entry(user).or_default() += 1;
            inner.epoch = inner.epoch.wrapping_add(1);
            let epoch = inner.epoch;
            if let Some(current) = inner.users.get_mut(&user) {
                current.generation = epoch;
            }
        }
        if self.fence {
            invalidate_user(&self.pool, user);
        }
    }
}
impl Drop for Mutation {
    fn drop(&mut self) {
        let state = state(&self.pool);
        // Keep the users active until every cancellation fence has been installed.
        for user in &self.users {
            if self.daily_published {
                continue;
            } else if self.fence {
                invalidate_user(&self.pool, *user);
            } else {
                initial_saved(&state, *user);
            }
        }
        let mut inner = state.inner.lock().unwrap_or_else(|e| e.into_inner());
        inner.epoch = inner.epoch.wrapping_add(1);
        let epoch = inner.epoch;
        for user in &self.users {
            if let Some(current) = inner.users.get_mut(user) {
                current.generation = epoch;
            }
            if let Some(count) = inner.active_users.get_mut(user) {
                *count -= 1;
                if *count == 0 {
                    inner.active_users.remove(user);
                }
            }
        }
        inner.active_writes -= 1;
        state.wake.notify_one();
    }
}
pub fn is_unavailable(error: &sqlx::Error) -> bool {
    matches!(error, sqlx::Error::Protocol(message) if message == "beans read memory unavailable")
}
fn unavailable() -> sqlx::Error {
    sqlx::Error::Protocol("beans read memory unavailable".into())
}

/// An acknowledged money write can precede its complete read projection.
/// Wait only for the existing background publisher: GETs never query SQL or
/// serve the fenced balance. Cancellation releases the bounded waiter slot;
/// overload, startup failure and a stalled publisher retain the usual 503.
pub async fn read_after_publication<T>(
    pool: &PgPool,
    mut read: impl FnMut() -> sqlx::Result<T>,
) -> sqlx::Result<T> {
    match read() {
        Err(error) if is_unavailable(&error) => {}
        result => return result,
    }
    let state = {
        let _phase = http_phase(HttpPhase::Processing).operation("beans_state_lookup");
        state(pool)
    };
    let ready = {
        let inner = {
            let _phase =
                http_phase(HttpPhase::Processing).operation("beans_publication_ready_mutex");
            state.inner.lock().unwrap_or_else(|e| e.into_inner())
        };
        let _phase = http_phase(HttpPhase::Processing).operation("beans_publication_ready_read");
        inner.ready
    };
    if !ready {
        return Err(unavailable());
    }
    let _slot = READ_WAITERS.try_acquire().map_err(|_| unavailable())?;
    let deadline = tokio::time::Instant::now() + READ_PUBLICATION_WAIT;
    loop {
        if tokio::time::Instant::now() >= deadline {
            return Err(unavailable());
        }
        let published = state.published.notified();
        tokio::pin!(published);
        published.as_mut().enable();
        match read() {
            Err(error) if is_unavailable(&error) => {}
            result => return result,
        }
        state.wake.notify_one();
        {
            // Only the notification wait: read attempts have their own scopes.
            // Includes executor resumption; does not establish publisher CPU/SQL time.
            let _phase = http_phase(HttpPhase::Processing).operation("beans_publication_wait");
            tokio::time::timeout_at(deadline, published)
                .await
                .map_err(|_| unavailable())?;
        }
    }
}
fn rank(id: Uuid, user: &User) -> Option<Rank> {
    user.wallet
        .as_ref()
        .filter(|w| user.available && user.eligible && w.hands_played > 0)
        .map(|w| {
            (
                std::cmp::Reverse(w.net_won),
                std::cmp::Reverse(w.hands_played),
                id,
            )
        })
}
fn wallet(row: &sqlx::postgres::PgRow) -> Wallet {
    Wallet {
        balance: row.get("balance"),
        net_won: row.get("net_won"),
        hands_played: row.get("hands_played"),
        hands_won: row.get("hands_won"),
        refills: row.get("refills"),
    }
}
fn count(user: &User) -> usize {
    user.days.len() + user.receipts.len() + user.latest.len() + user.effects.len() + 1
}

fn apply_poker(wallet: &mut Wallet, delta: i64) -> sqlx::Result<()> {
    apply_poker_totals(wallet, delta, 1, i32::from(delta > 0))
}

fn apply_poker_totals(wallet: &mut Wallet, delta: i64, played: i32, won: i32) -> sqlx::Result<()> {
    let balance = wallet
        .balance
        .checked_add(delta)
        .filter(|balance| *balance >= 0)
        .ok_or_else(|| {
            sqlx::Error::Protocol("accepted poker balance overflow or overdraft".into())
        })?;
    let net_won = wallet.net_won.checked_add(delta).ok_or_else(unavailable)?;
    let hands_played = wallet
        .hands_played
        .checked_add(played)
        .ok_or_else(unavailable)?;
    let hands_won = wallet.hands_won.checked_add(won).ok_or_else(unavailable)?;
    wallet.balance = balance;
    wallet.net_won = net_won;
    wallet.hands_played = hands_played;
    wallet.hands_won = hands_won;
    Ok(())
}

/// Validate every wallet before admitting the immutable hand write. The closure
/// only publishes to the already-reserved FIFO; it must never await or reenter
/// this module. The worker cannot see a half-published set of player balances.
pub fn accept_poker_hand_with(
    pool: &PgPool,
    hand: &crate::persist::HandRecord,
    checkpoint: Option<&crate::runtime_checkpoint::RuntimeCheckpoint>,
    enqueue: impl FnOnce() -> sqlx::Result<()>,
) -> sqlx::Result<()> {
    let state = state(pool);
    let mut inner = state.inner.lock().unwrap_or_else(|e| e.into_inner());
    if !inner.ready || hand.players.is_empty() || hand.players.len() > 10 {
        return Err(unavailable());
    }
    let players: Vec<_> = hand
        .players
        .iter()
        .map(|p| (p.user_id, i64::from(p.delta)))
        .collect();
    if let Some(existing) = inner.poker.get(&hand.id) {
        if existing.players != players {
            return Err(sqlx::Error::Protocol(
                "poker settlement identity changed".into(),
            ));
        }
        return enqueue();
    }
    if inner.poker.len() >= MAX_POKER_HANDS {
        return Err(unavailable());
    }
    let mut updated = Vec::with_capacity(players.len());
    let mut unique = HashSet::new();
    for (id, delta) in &players {
        if !unique.insert(*id) {
            return Err(sqlx::Error::Protocol("duplicate poker wallet owner".into()));
        }
        let user = inner.users.get(id).ok_or_else(unavailable)?;
        let mut wallet = user.wallet.clone().ok_or_else(unavailable)?;
        apply_poker(&mut wallet, *delta)?;
        updated.push((*id, *delta, wallet));
    }
    enqueue()?;
    if let (Some(session), Some(checkpoint)) = (hand.session_id, checkpoint) {
        inner.reservations.checkpoint(session, checkpoint);
    }
    inner.epoch = inner.epoch.wrapping_add(1);
    let generation = inner.epoch;
    for (id, delta, wallet) in updated {
        if let Some(key) = inner.users.get(&id).and_then(|user| rank(id, user)) {
            inner.ranks.remove(&key);
        }
        let user = inner.users.get_mut(&id).expect("validated poker wallet");
        user.wallet = Some(wallet);
        user.poker.insert(hand.id, delta);
        user.generation = generation;
        if let Some(key) = rank(id, user) {
            inner.ranks.insert(key);
        }
    }
    inner.poker.insert(
        hand.id,
        PokerReceipt {
            players,
            committed: false,
        },
    );
    state.published.notify_waiters();
    Ok(())
}

pub fn poker_accepted(pool: &PgPool, hand: Uuid) -> bool {
    state(pool)
        .inner
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .poker
        .contains_key(&hand)
}

/// A durable acknowledgement removes only this receipt's overlay. It never
/// replaces an already newer accepted wallet with a database balance snapshot.
pub fn poker_committed(pool: &PgPool, hand: Uuid) {
    let state = state(pool);
    let mut inner = state.inner.lock().unwrap_or_else(|e| e.into_inner());
    let Some(receipt) = inner.poker.get_mut(&hand) else {
        return;
    };
    if receipt.committed {
        return;
    }
    receipt.committed = true;
    let players = receipt.players.clone();
    inner.epoch = inner.epoch.wrapping_add(1);
    let generation = inner.epoch;
    for (id, _) in players {
        if let Some(user) = inner.users.get_mut(&id) {
            user.poker.remove(&hand);
            user.generation = generation;
        }
    }
    inner.poker_acknowledged.push_back(hand);
    if inner.poker_acknowledged.len() > RECENT_POKER_ACKS {
        if let Some(old) = inner.poker_acknowledged.pop_front() {
            inner.poker.remove(&old);
        }
    }
    state.published.notify_waiters();
}

/// Run after the hand's cross-game liability hold is released. A deleted
/// account's hidden wallet survives only while another dealt hand needs it.
pub fn poker_released(pool: &PgPool, hand: Uuid) {
    let state = state(pool);
    let retired: Vec<_> = {
        let inner = state.inner.lock().unwrap_or_else(|e| e.into_inner());
        inner
            .poker
            .get(&hand)
            .into_iter()
            .flat_map(|receipt| &receipt.players)
            .filter_map(|(id, _)| inner.users.get(id).filter(|user| user.retired).map(|_| *id))
            .collect()
    };
    for id in retired {
        remove_user(pool, id);
    }
}
fn add_ledger(
    user: &mut User,
    record: Option<Uuid>,
    balance: i64,
    details: Value,
) -> sqlx::Result<()> {
    let Some(record) = record else {
        return Ok(());
    };
    let bytes = details
        .to_string()
        .len()
        .saturating_mul(2)
        .saturating_add(256);
    if details["purpose"] == "casual_entry" {
        if user.receipts.contains_key(&record) {
            return Err(unavailable());
        }
        if user.bytes.saturating_add(bytes) > MAX_BYTES || count(user) >= MAX_ROWS {
            return Err(unavailable());
        }
        user.bytes += bytes;
        user.receipts.insert(record, Arc::new(details));
    } else {
        let game = if details["purpose"] == "claw_grab" {
            "claw"
        } else {
            "slots"
        };
        let previous = user.latest.get(game).map_or(0, |row| row.bytes);
        let next = user.bytes.saturating_sub(previous).saturating_add(bytes);
        if next > MAX_BYTES {
            return Err(unavailable());
        }
        user.bytes = next;
        user.latest.insert(
            game,
            Recent {
                record,
                balance,
                details: Arc::new(details),
                bytes,
            },
        );
    }
    Ok(())
}
fn replace(inner: &mut Inner, id: Uuid, user: User) -> sqlx::Result<()> {
    let old = inner.users.get(&id);
    let rows = inner.count - old.map_or(0, count) + count(&user);
    let bytes = inner.bytes - old.map_or(0, |v| v.bytes) + user.bytes;
    if rows > MAX_ROWS || bytes > MAX_BYTES || (old.is_none() && inner.users.len() >= MAX_USERS) {
        return Err(unavailable());
    }
    if let Some(key) = old.and_then(|v| rank(id, v)) {
        inner.ranks.remove(&key);
    }
    if let Some(key) = rank(id, &user) {
        inner.ranks.insert(key);
    }
    inner.users.insert(id, user);
    inner.count = rows;
    inner.bytes = bytes;
    Ok(())
}
pub fn ready(pool: &PgPool) -> bool {
    state(pool)
        .inner
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .ready
}

pub async fn hydrate(pool: &PgPool) -> sqlx::Result<()> {
    let state = state(pool);
    let _loader = state.loader.lock().await;
    let epoch = {
        let inner = state.inner.lock().unwrap_or_else(|e| e.into_inner());
        if inner.ready {
            return Ok(());
        }
        if inner.active_writes != 0 {
            return Err(unavailable());
        }
        inner.epoch
    };
    let mut loaded = tokio::time::timeout(Duration::from_secs(60), async {
        crate::live_directory::wait_for_open_transactions(pool).await?;
        let mut tx = crate::live_directory::begin_read_snapshot(pool).await?;
        let mut loaded = Inner::default();
        // One streamed statement per source (one plan, one scan) in the snapshot.
        let query = format!("SELECT w.user_id,w.balance,w.net_won,w.hands_played,w.hands_won,w.refills,({}) AS eligible FROM wallets w JOIN users u ON u.id=w.user_id WHERE w.game=$1 AND u.deleted_at IS DISTINCT FROM 'epoch'::timestamptz ORDER BY w.user_id", crate::internal_accounts::not_qa_flags("u."));
        {
            let mut rows = sqlx::query(&query).bind(crate::game_catalog::BEANS).fetch(&mut *tx);
            while let Some(row) = rows.try_next().await? {
                let id = row.get("user_id");
                replace(&mut loaded, id, User { wallet: Some(wallet(&row)), eligible: row.get("eligible"), available: true, bytes: 512, ..User::default() })?;
            }
        }
        {
            let mut rows = sqlx::query_as::<_, (String, NaiveDate)>("SELECT subject_key,usage_date FROM daily_usage WHERE scope='feature_user' AND feature=$1 ORDER BY subject_key,usage_date")
                .bind(DAILY).fetch(&mut *tx);
            while let Some((id, day)) = rows.try_next().await? {
                let id = Uuid::parse_str(&id).map_err(|_| unavailable())?;
                if let Some(user) = loaded.users.get_mut(&id) {
                    if user.days.insert(day) { user.bytes += 64; loaded.bytes += 64; loaded.count += 1; }
                }
                if loaded.count > MAX_ROWS || loaded.bytes > MAX_BYTES { return Err(unavailable()); }
            }
        }
        {
            let mut rows = sqlx::query_as::<_, (i64, Uuid, Option<Uuid>, i64, Value)>("SELECT id,user_id,record_id,balance_after,details FROM wallet_ledger WHERE game=$1 AND (details->>'purpose'='casual_entry' OR (kind='play' AND (details->>'purpose'='claw_grab' OR details->>'game'='slots'))) AND id>0 ORDER BY id")
                .bind(crate::game_catalog::BEANS).fetch(&mut *tx);
            while let Some((_, owner, record, balance, details)) = rows.try_next().await? {
                if let Some(user) = loaded.users.get_mut(&owner) {
                    let before = user.bytes;
                    let previous_count = count(user);
                    add_ledger(user, record, balance, details)?;
                    loaded.bytes = loaded.bytes - before + user.bytes;
                    loaded.count = loaded.count - previous_count + count(user);
                }
                if loaded.count > MAX_ROWS || loaded.bytes > MAX_BYTES { return Err(unavailable()); }
            }
        }
        let rooms: Vec<(String, Value)> = sqlx::query_as("SELECT code,state FROM game_room_checkpoints WHERE game='sicbo'")
            .fetch_all(&mut *tx).await?;
        for (code, snapshot) in rooms { loaded.reservations.sicbo_commit(&code, &snapshot); }
        tx.commit().await?;
        Ok::<_, sqlx::Error>(loaded)
    }).await.map_err(|_| sqlx::Error::PoolTimedOut)??;
    {
        let mut inner = state.inner.lock().unwrap_or_else(|e| e.into_inner());
        if inner.epoch != epoch || inner.active_writes != 0 {
            return Err(unavailable());
        }
        let pool = pool.clone();
        let weak = Arc::downgrade(&state);
        loaded.writer = Some(crate::write_behind::Ordered::new(None, move |_, next| {
            let pool = pool.clone();
            let weak = weak.clone();
            async move {
                if let Some(user) = next {
                    let _mutation = begin_mutation(&pool, [user], false);
                    tokio::time::timeout(Duration::from_secs(10), async {
                        let mut tx = pool.begin().await.map_err(|e| e.to_string())?;
                        match crate::db::lock_game_participants(&mut tx, &[user]).await {
                            Ok(()) => {}
                            Err(sqlx::Error::RowNotFound) => return Ok(()),
                            Err(error) => return Err(error.to_string()),
                        }
                        beans::ensure_on(&mut tx, user)
                            .await
                            .map_err(|e| e.to_string())?;
                        tx.commit().await.map_err(|e| e.to_string())?;
                        Ok::<_, String>(())
                    })
                    .await
                    .map_err(|_| "beans initial grant timed out".to_string())??;
                    if let Some(state) = weak.upgrade() {
                        initial_saved(&state, user);
                    }
                }
                Ok(None)
            }
        }));
        loaded.ready = true;
        loaded.epoch = epoch;
        *inner = loaded;
    }
    let worker_pool = pool.clone();
    let worker_state = state.clone();
    tokio::spawn(async move {
        refresh_loop(worker_pool, worker_state).await;
    });
    Ok(())
}
fn invalidate_state(state: &State, id: Uuid) {
    let mut inner = state.inner.lock().unwrap_or_else(|e| e.into_inner());
    invalidate_locked(&mut inner, id);
    state.wake.notify_one();
}
fn invalidate_locked(inner: &mut Inner, id: Uuid) {
    inner.epoch = inner.epoch.wrapping_add(1);
    if !inner.ready {
        return;
    }
    if let Some(key) = inner.users.get(&id).and_then(|user| rank(id, user)) {
        inner.ranks.remove(&key);
    }
    if !inner.users.contains_key(&id)
        && replace(
            inner,
            id,
            User {
                bytes: 512,
                ..User::default()
            },
        )
        .is_err()
    {
        inner.ready = false;
        return;
    }
    let generation = inner.epoch;
    let user = inner.users.get_mut(&id).expect("beans user installed");
    user.generation = generation;
    user.available = false;
    user.retired = false;
    inner.pending.insert(id);
}
fn initial_saved(state: &State, id: Uuid) {
    let mut inner = state.inner.lock().unwrap_or_else(|e| e.into_inner());
    if inner.ready && inner.users.get(&id).is_some_and(|user| !user.retired) {
        // The accepted grant stays readable while its durable row is confirmed.
        inner.pending.insert(id);
        state.wake.notify_one();
    }
}
/// Invoke after COMMIT completes (also on an unknown acknowledgement), never
/// publish tentative values from inside the transaction.
pub fn invalidate_user(pool: &PgPool, id: Uuid) {
    match crate::player_directory::lookup(pool, id) {
        Ok(Some(player)) if !player.active() => remove_user(pool, id),
        Ok(None) => remove_user(pool, id),
        _ => invalidate_state(&state(pool), id),
    }
}
/// Telemetry producers call this only after committing a bkqa: marker.
/// Repeated observations do not refresh an already excluded or fenced user.
pub fn qa_marker_written(pool: &PgPool, id: Uuid) {
    let player = match crate::player_directory::lookup(pool, id) {
        Ok(Some(player)) => player,
        Ok(None) => return,
        Err(_) => {
            invalidate_state(&state(pool), id);
            return;
        }
    };
    if !player.active()
        || !(player.is_guest
            || (player.email.starts_with("tg-") && player.email.ends_with("@telegram.invalid")))
    {
        return;
    }
    let state = state(pool);
    let needed = {
        let mut inner = state.inner.lock().unwrap_or_else(|e| e.into_inner());
        if !inner.ready {
            inner.epoch = inner.epoch.wrapping_add(1);
            return;
        }
        inner.users.get_mut(&id).is_some_and(|user| {
            if user.eligible && !user.qa_dirty {
                user.qa_dirty = true;
                true
            } else {
                false
            }
        })
    };
    if needed {
        invalidate_state(&state, id);
    }
}
/// A newly committed account has no bean wallet yet; its first GET can grant
/// immediately. Never use this hook for an existing account or on login.
pub fn register_user(pool: &PgPool, id: Uuid) {
    let state = state(pool);
    let mut inner = state.inner.lock().unwrap_or_else(|e| e.into_inner());
    inner.epoch = inner.epoch.wrapping_add(1);
    if !inner.ready
        || inner
            .users
            .get(&id)
            .is_some_and(|user| user.wallet.is_some())
    {
        return;
    }
    let generation = inner.epoch;
    if replace(
        &mut inner,
        id,
        User {
            available: true,
            generation,
            bytes: 512,
            ..User::default()
        },
    )
    .is_err()
    {
        inner.ready = false;
        return;
    }
    inner.pending.remove(&id);
    state.published.notify_waiters();
}
pub fn remove_user(pool: &PgPool, id: Uuid) {
    let state = state(pool);
    let mut inner = state.inner.lock().unwrap_or_else(|e| e.into_inner());
    inner.epoch = inner.epoch.wrapping_add(1);
    if inner
        .users
        .get(&id)
        .is_some_and(|user| !user.poker.is_empty() || !user.effects.is_empty())
        || crate::beans::holds::outstanding(id)
    {
        if let Some(key) = inner.users.get(&id).and_then(|user| rank(id, user)) {
            inner.ranks.remove(&key);
        }
        let generation = inner.epoch;
        if let Some(user) = inner.users.get_mut(&id) {
            user.available = false;
            user.eligible = false;
            user.retired = true;
            user.generation = generation;
        }
        inner.pending.remove(&id);
        state.published.notify_waiters();
        return;
    }
    inner.reservations.remove_user(id);
    if let Some(user) = inner.users.remove(&id) {
        if let Some(key) = rank(id, &user) {
            inner.ranks.remove(&key);
        }
        inner.count -= count(&user);
        inner.bytes -= user.bytes;
    }
    inner.pending.remove(&id);
    state.published.notify_waiters();
}
async fn read_user(
    pool: &PgPool,
    id: Uuid,
    poker: HashMap<Uuid, i64>,
    effects: HashMap<Uuid, PendingEffect>,
) -> sqlx::Result<User> {
    tokio::time::timeout(Duration::from_secs(10), async {
        crate::live_directory::wait_for_open_transactions(pool).await?;
        let mut tx = crate::live_directory::begin_read_snapshot(pool).await?;
        let query = format!("SELECT w.balance,w.net_won,w.hands_played,w.hands_won,w.refills,({}) AS eligible FROM wallets w JOIN users u ON u.id=w.user_id WHERE w.game=$1 AND w.user_id=$2 AND u.deleted_at IS DISTINCT FROM 'epoch'::timestamptz", crate::internal_accounts::not_qa_flags("u."));
        let row = sqlx::query(&query).bind(crate::game_catalog::BEANS).bind(id).fetch_optional(&mut *tx).await?;
        let mut user = User { wallet: row.as_ref().map(wallet), eligible: row.as_ref().is_some_and(|r| r.get("eligible")), available: true, bytes: 512, ..User::default() };
        let mut after = None::<NaiveDate>;
        loop {
            let rows: Vec<NaiveDate> = sqlx::query_scalar("SELECT usage_date FROM daily_usage WHERE scope='feature_user' AND subject_key=$1 AND feature=$2 AND ($3::date IS NULL OR usage_date>$3) ORDER BY usage_date LIMIT $4")
                .bind(id.to_string()).bind(DAILY).bind(after).bind(PAGE).fetch_all(&mut *tx).await?;
            if rows.is_empty() { break; }
            after = rows.last().copied();
            for day in rows { user.days.insert(day); user.bytes += 64; }
            if count(&user) > MAX_ROWS || user.bytes > MAX_BYTES { return Err(unavailable()); }
        }
        let mut after = 0i64;
        loop {
            let rows: Vec<(i64, Option<Uuid>, i64, Value)> = sqlx::query_as("SELECT id,record_id,balance_after,details FROM wallet_ledger WHERE user_id=$1 AND game=$2 AND (details->>'purpose'='casual_entry' OR (kind='play' AND (details->>'purpose'='claw_grab' OR details->>'game'='slots'))) AND id>$3 ORDER BY id LIMIT $4")
                .bind(id).bind(crate::game_catalog::BEANS).bind(after).bind(PAGE).fetch_all(&mut *tx).await?;
            if rows.is_empty() { break; }
            for (cursor, record, balance, details) in rows { after = cursor; add_ledger(&mut user, record, balance, details)?; }
            if count(&user) > MAX_ROWS || user.bytes > MAX_BYTES { return Err(unavailable()); }
        }
        // The wallet and receipt membership come from this same MVCC snapshot.
        // A lost acknowledgement may already be present in SQL; overlay only
        // the accepted hands that this snapshot has not committed yet.
        if !poker.is_empty() {
            let ids: Vec<_> = poker.keys().copied().collect();
            let applied: Vec<(Uuid, i64)> = sqlx::query_as("SELECT record_id,delta FROM wallet_ledger WHERE game=$1 AND user_id=$2 AND kind='play' AND record_id=ANY($3)")
                .bind(crate::game_catalog::BEANS).bind(id).bind(&ids).fetch_all(&mut *tx).await?;
            let mut seen = HashSet::new();
            for (hand, delta) in applied {
                if poker.get(&hand) != Some(&delta) || !seen.insert(hand) {
                    return Err(sqlx::Error::Protocol("poker receipt does not match accepted settlement".into()));
                }
            }
            // HashMap order is not acceptance order. Validate the aggregate
            // against the snapshot once: a loss may depend on an accepted win
            // that happens to appear later in this iteration.
            let (mut delta_total, mut played, mut won) = (0i64, 0i32, 0i32);
            for (hand, delta) in &poker {
                if !seen.contains(hand) {
                    delta_total = delta_total.checked_add(*delta).ok_or_else(unavailable)?;
                    played = played.checked_add(1).ok_or_else(unavailable)?;
                    won = won.checked_add(i32::from(*delta > 0)).ok_or_else(unavailable)?;
                }
            }
            let wallet = user.wallet.as_mut().ok_or_else(unavailable)?;
            apply_poker_totals(wallet, delta_total, played, won)?;
            user.poker = poker;
        }
        if !effects.is_empty() {
            let ids: Vec<_> = effects.keys().copied().collect();
            let applied: Vec<(Uuid,i64)> = sqlx::query_as("SELECT record_id,delta FROM wallet_ledger WHERE game=$1 AND user_id=$2 AND record_id=ANY($3)")
                .bind(crate::game_catalog::BEANS).bind(id).bind(ids).fetch_all(&mut *tx).await?;
            let mut seen = HashSet::new();
            for (record,delta) in applied {
                if effects.get(&record).is_none_or(|e|e.ledger_delta!=delta) || !seen.insert(record) { return Err(unavailable()); }
            }
            let mut total = PendingEffect {balance:0,net:0,played:0,won:0,ledger_delta:0,day:None};
            for (record,effect) in &effects {
                if let Some(day)=effect.day {user.days.insert(day);}
                if !seen.contains(record) {
                    total.balance=total.balance.checked_add(effect.balance).ok_or_else(unavailable)?;
                    total.net=total.net.checked_add(effect.net).ok_or_else(unavailable)?;
                    total.played=total.played.checked_add(effect.played).ok_or_else(unavailable)?;
                    total.won=total.won.checked_add(effect.won).ok_or_else(unavailable)?;
                }
            }
            total.apply(user.wallet.as_mut().ok_or_else(unavailable)?)?;
            user.bytes += effects.len()*256;
            user.effects=effects;
        }
        tx.commit().await?;
        Ok::<_, sqlx::Error>(user)
    }).await.map_err(|_| sqlx::Error::PoolTimedOut)?
}
async fn refresh_loop(pool: PgPool, state: Arc<State>) {
    let mut after = None::<Uuid>;
    let mut failures = 0u32;
    loop {
        if pool.is_closed() {
            return;
        }
        let (empty, batch): (bool, Vec<_>) = {
            let inner = state.inner.lock().unwrap_or_else(|e| e.into_inner());
            (
                inner.pending.is_empty(),
                inner
                    .pending
                    .range((after.map_or(Unbounded, Excluded), Unbounded))
                    .filter_map(|id| {
                        if inner.active_users.contains_key(id) {
                            None
                        } else {
                            inner
                                .users
                                .get(id)
                                .filter(|user| !user.retired)
                                .map(|user| {
                                    (
                                        *id,
                                        user.generation,
                                        user.poker.clone(),
                                        user.effects.clone(),
                                    )
                                })
                        }
                    })
                    .take(64)
                    .collect(),
            )
        };
        if batch.is_empty() {
            // The wake that ended the last backoff may be for a user before
            // the cursor: rescan from the start before waiting again.
            if after.take().is_some() {
                continue;
            }
            if empty {
                state.wake.notified().await;
            } else {
                // Pending users are still inside a write; Mutation::drop wakes
                // us as soon as it ends, so a fenced GET does not wait a tick.
                let _ =
                    tokio::time::timeout(Duration::from_millis(100), state.wake.notified()).await;
            }
            continue;
        }
        after = batch.last().map(|(id, _, _, _)| *id);
        let mut failed = false;
        for (id, generation, poker, effects) in batch {
            let _loader = state.loader.lock().await;
            let result = read_user(&pool, id, poker, effects).await;
            match result {
                Ok(mut user) => {
                    let mut inner = state.inner.lock().unwrap_or_else(|e| e.into_inner());
                    if inner.active_users.contains_key(&id)
                        || !inner
                            .users
                            .get(&id)
                            .is_some_and(|current| current.generation == generation)
                    {
                        continue;
                    }
                    if let Some(current) = inner.users.get_mut(&id) {
                        user.effect_hold = current.effect_hold.take();
                    }
                    user.generation = generation;
                    if replace(&mut inner, id, user).is_ok() {
                        inner.pending.remove(&id);
                        state.published.notify_waiters();
                    } else {
                        failed = true;
                    }
                }
                Err(_) => {
                    failed = true;
                    failures = failures.saturating_add(1);
                    if failures.is_power_of_two() {
                        tracing::warn!(failures, "beans_read_refresh_waiting");
                    }
                }
            }
        }
        // Back off between batches; a new write or a waiting read cuts it
        // short unless a load or install just failed, so wakes cannot spin.
        if failed {
            tokio::time::sleep(Duration::from_millis(100)).await;
        } else {
            let _ = tokio::time::timeout(Duration::from_millis(100), state.wake.notified()).await;
        }
    }
}
fn active(pool: &PgPool, user: Uuid) -> sqlx::Result<()> {
    match crate::player_directory::lookup(pool, user)? {
        Some(player) if player.active() => Ok(()),
        _ => Err(sqlx::Error::RowNotFound),
    }
}
pub fn ensure(pool: &PgPool, id: Uuid) -> sqlx::Result<Wallet> {
    {
        let _phase = http_phase(HttpPhase::Processing).operation("beans_player_lookup");
        active(pool, id)?;
    }
    let state = {
        let _phase = http_phase(HttpPhase::Processing).operation("beans_state_lookup");
        state(pool)
    };
    let mut inner = {
        let _phase = http_phase(HttpPhase::Processing).operation("beans_ensure_mutex");
        state.inner.lock().unwrap_or_else(|e| e.into_inner())
    };
    let _phase = http_phase(HttpPhase::Processing).operation("beans_ensure_read");
    if !inner.ready {
        return Err(unavailable());
    }
    if let Some(user) = inner.users.get(&id) {
        if !user.available {
            return Err(unavailable());
        }
        if let Some(wallet) = &user.wallet {
            return Ok(wallet.clone());
        }
    }
    if !crate::write_behind::accepting()
        || inner.users.len() >= MAX_USERS
        || inner.count >= MAX_ROWS
        || inner.bytes + 512 > MAX_BYTES
    {
        return Err(unavailable());
    }
    let wallet = Wallet {
        balance: beans::GRANT,
        net_won: 0,
        hands_played: 0,
        hands_won: 0,
        refills: 0,
    };
    inner.epoch = inner.epoch.wrapping_add(1);
    let generation = inner.epoch;
    let Inner {
        users,
        writer,
        count,
        bytes,
        ..
    } = &mut *inner;
    let permit = writer
        .as_mut()
        .ok_or_else(unavailable)?
        .try_reserve()
        .map_err(|_| unavailable())?;
    if let Some(user) = users.get_mut(&id) {
        user.wallet = Some(wallet.clone());
        user.generation = generation;
    } else {
        users.insert(
            id,
            User {
                wallet: Some(wallet.clone()),
                available: true,
                generation,
                bytes: 512,
                ..User::default()
            },
        );
        *count += 1;
        *bytes += 512;
    }
    permit.publish(Some(id));
    Ok(wallet)
}
/// Keep the balance snapshot stable through a public-poker seat admission.
/// The callback updates memory only and must not await or reenter this module.
/// Seating does not debit the wallet: it already includes the table stack.
/// The second callback argument reports dealt hands whose result is not yet
/// included in this balance. Accepted results need no database-commit wait.
pub(crate) fn with_admission_balance<R>(
    pool: &PgPool,
    id: Uuid,
    accept: impl FnOnce(i64, bool) -> sqlx::Result<R>,
) -> sqlx::Result<R> {
    ensure(pool, id)?;
    let state = state(pool);
    let inner = state.inner.lock().unwrap_or_else(|e| e.into_inner());
    if !inner.ready || inner.active_users.contains_key(&id) {
        return Err(unavailable());
    }
    let user = inner
        .users
        .get(&id)
        .filter(|user| user.available)
        .ok_or_else(unavailable)?;
    let balance = user.wallet.as_ref().ok_or_else(unavailable)?.balance;
    let directory = crate::live_directory::memory_snapshot_for_user(pool, id)?;
    let reserved = reservations::reserved(&inner, &directory, id, false, None, None)?;
    let unaccounted =
        crate::beans::holds::outstanding_unless(id, |hand| inner.poker.contains_key(&hand));
    accept(balance.saturating_sub(reserved).max(0), unaccounted)
}

pub fn profile(pool: &PgPool, id: Uuid) -> sqlx::Result<(Wallet, bool)> {
    ensure(pool, id)?;
    let day = (chrono::Utc::now() + chrono::Duration::hours(8)).date_naive();
    let state = {
        let _phase = http_phase(HttpPhase::Processing).operation("beans_state_lookup");
        state(pool)
    };
    let inner = {
        let _phase = http_phase(HttpPhase::Processing).operation("beans_profile_mutex");
        state.inner.lock().unwrap_or_else(|e| e.into_inner())
    };
    let _phase = http_phase(HttpPhase::Processing).operation("beans_profile_read");
    let user = inner
        .users
        .get(&id)
        .filter(|v| inner.ready && v.available)
        .ok_or_else(unavailable)?;
    Ok((
        user.wallet.as_ref().ok_or_else(unavailable)?.clone(),
        !user.days.contains(&day),
    ))
}
pub fn receipt(
    pool: &PgPool,
    id: Uuid,
    record: Uuid,
    game: &str,
) -> sqlx::Result<Option<(i64, Arc<Value>)>> {
    active(pool, id)?;
    let state = state(pool);
    let inner = state.inner.lock().unwrap_or_else(|e| e.into_inner());
    if !inner.ready {
        return Err(unavailable());
    }
    let Some(user) = inner.users.get(&id) else {
        return Ok(None);
    };
    if !user.available {
        return Err(unavailable());
    }
    Ok(user.wallet.as_ref().and_then(|wallet| {
        user.receipts
            .get(&record)
            .filter(|value| value["game"] == game)
            .map(|details| (wallet.balance, details.clone()))
    }))
}
pub fn leaderboard(
    pool: &PgPool,
    limit: i64,
    offset: i64,
) -> sqlx::Result<Vec<beans::LeaderboardRow>> {
    let state = state(pool);
    let inner = state.inner.lock().unwrap_or_else(|e| e.into_inner());
    if !inner.ready
        || inner
            .pending
            .iter()
            .any(|id| inner.users.get(id).is_some_and(|user| !user.available))
    {
        return Err(unavailable());
    }
    let mut rows = Vec::new();
    let mut skipped = 0usize;
    let limit = crate::pagination::limit(Some(limit)) as usize;
    let offset = crate::pagination::offset(Some(offset)) as usize;
    for (net, hands, id) in &inner.ranks {
        let Some(player) = crate::player_directory::lookup(pool, *id)? else {
            continue;
        };
        if !player.active() {
            continue;
        }
        if skipped < offset {
            skipped += 1;
            continue;
        }
        rows.push(beans::LeaderboardRow {
            user_id: *id,
            display_name: crate::content_moderation::public_name_or_fallback(
                player
                    .display_name
                    .as_deref()
                    .map(str::trim)
                    .filter(|s| !s.is_empty()),
                *id,
            ),
            net_won: net.0,
            hands_played: hands.0,
        });
        if rows.len() == limit {
            break;
        }
    }
    Ok(rows)
}

/// Current wallet plus the newest committed cabinet result, scoped to its owner.
pub fn latest(pool: &PgPool, id: Uuid, game: &str) -> sqlx::Result<(i64, Option<Recent>)> {
    ensure(pool, id)?;
    let state = state(pool);
    let inner = state.inner.lock().unwrap_or_else(|e| e.into_inner());
    let user = inner
        .users
        .get(&id)
        .filter(|v| inner.ready && v.available)
        .ok_or_else(unavailable)?;
    Ok((
        user.wallet.as_ref().ok_or_else(unavailable)?.balance,
        user.latest.get(game).cloned(),
    ))
}

/// Socket refresh notification; waiting performs no SQL.
pub(crate) fn changed(pool: &PgPool) -> tokio::sync::futures::OwnedNotified {
    state(pool).published.clone().notified_owned()
}

/// Socket consumers compare only their own wallet generation after a wakeup.
pub(crate) fn user_generation(pool: &PgPool, user: Uuid) -> Option<u64> {
    state(pool)
        .inner
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .users
        .get(&user)
        .map(|u| u.generation)
}

/// Do not enqueue lottery work ahead of a new user's initial wallet grant.
pub(crate) async fn initial_grant_ready(pool: &PgPool, user: Uuid) -> sqlx::Result<()> {
    if sqlx::query_scalar::<_, bool>(
        "SELECT EXISTS(SELECT 1 FROM wallets WHERE user_id=$1 AND game=$2)",
    )
    .bind(user)
    .bind(crate::game_catalog::BEANS)
    .fetch_one(pool)
    .await?
    {
        return Ok(());
    }
    initial_grant_durable(pool).await
}

/// The rest of [`initial_grant_ready`] for a caller that already read the
/// wallet row's absence (in its own statement): wait until every accepted
/// beans write, including this user's initial grant, is committed.
pub(crate) async fn initial_grant_durable(pool: &PgPool) -> sqlx::Result<()> {
    let state = state(pool);
    let receipt = state
        .inner
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .writer
        .as_ref()
        .ok_or_else(unavailable)?
        .durable();
    tokio::time::timeout(Duration::from_secs(2), receipt)
        .await
        .map_err(|_| unavailable())?
        .map_err(|_| unavailable())?;
    Ok(())
}
