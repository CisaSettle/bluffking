//! Wallet primitives and the runtime baseline for migrated game writers.
//!
//! One `wallets` row per (user, game); every balance change is one
//! `wallet_ledger` row written in the same transaction, so a balance always
//! equals the sum of that wallet's ledger deltas. Poker chips (`users.chips`)
//! are a different economy and never pass through here.
//! Startup hydrates migrated games once. Their room projection and idle reads
//! share each entry and their accepted writes keep ordered ledger persistence.
//! Legacy domains retain durable idle reads and their active-seat projections.

use std::collections::HashMap;
use std::sync::{
    atomic::{AtomicBool, AtomicI64, Ordering},
    Arc, Mutex, OnceLock,
};

use futures::{
    future::{BoxFuture, Shared},
    FutureExt,
};

use chrono::{DateTime, Utc};
use md5::{Digest, Md5};
use serde::Serialize;
use sqlx::{PgConnection, PgPool, Row};
use uuid::Uuid;

/// The game's initial session chip grant. Table admission may choose its own size.
#[derive(Debug, Clone, Copy)]
pub struct Policy {
    pub game: i16,
    pub grant: i64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Wallet {
    pub balance: i64,
    pub hands_played: i32,
    pub hands_won: i32,
    pub net_won: i64,
    pub refills: i32,
}

// One canonical database pool owns this runtime. Retaining its options Arc
// prevents pointer reuse; a second pool is rejected instead of mixing wallets.
struct BaselineState {
    options: Arc<sqlx::postgres::PgConnectOptions>,
    ready: AtomicBool,
    loader: tokio::sync::Mutex<()>,
}
fn baseline_state(pool: &PgPool) -> sqlx::Result<&'static BaselineState> {
    static STATE: OnceLock<BaselineState> = OnceLock::new();
    let options = pool.connect_options();
    let state = STATE.get_or_init(|| BaselineState {
        options: options.clone(),
        ready: AtomicBool::new(false),
        loader: tokio::sync::Mutex::new(()),
    });
    if !Arc::ptr_eq(&state.options, &options) {
        return Err(sqlx::Error::Protocol(
            "wallet baseline belongs to another pool".into(),
        ));
    }
    Ok(state)
}

const MAX_WALLETS: usize = 2_000_000;
const MAX_INITIALIZATIONS: usize = 1024;
const WALLET_PAGE: i64 = 1000;
type InitialGrant = Shared<BoxFuture<'static, Result<(), String>>>;
fn initializations() -> &'static Mutex<HashMap<(i16, Uuid), InitialGrant>> {
    static PENDING: OnceLock<Mutex<HashMap<(i16, Uuid), InitialGrant>>> = OnceLock::new();
    PENDING.get_or_init(Default::default)
}

// Baseline and seated projection are the same entry, updated under one lock.
// Leaving releases only the room binding, never the user's balance/counters.
struct MemoryWallet {
    room: Option<String>,
    seated: bool,
    wallet: Wallet,
    failed: bool,
}
fn memory() -> &'static dashmap::DashMap<(i16, Uuid), MemoryWallet> {
    static CACHE: OnceLock<dashmap::DashMap<(i16, Uuid), MemoryWallet>> = OnceLock::new();
    CACHE.get_or_init(Default::default)
}
pub fn ready(pool: &PgPool) -> bool {
    baseline_state(pool).is_ok_and(|state| state.ready.load(Ordering::Acquire))
}

/// Startup-only keyset hydration of migrated games. Their normal runtime reads
/// never refresh from SQL; recovery explicitly reconciles any replayed receipts.
/// Exceeding the memory ceiling fails readiness rather than truncating balances.
pub async fn hydrate(pool: &PgPool) -> sqlx::Result<()> {
    let baseline = baseline_state(pool)?;
    let _loader = baseline.loader.lock().await;
    if baseline.ready.load(Ordering::Acquire) {
        return Ok(());
    }
    if !memory().is_empty() {
        return Err(sqlx::Error::Protocol(
            "wallet hydration must precede runtime activity".into(),
        ));
    }
    let mut tx = pool.begin().await?;
    sqlx::query("SET TRANSACTION ISOLATION LEVEL REPEATABLE READ, READ ONLY")
        .execute(&mut *tx)
        .await?;
    let mut after: Option<(Uuid, i16)> = None;
    let mut loaded = Vec::new();
    loop {
        let rows = sqlx::query(
            "SELECT user_id,game,balance,hands_played,hands_won,net_won,refills FROM wallets \
             WHERE ($1::uuid IS NULL OR (user_id,game)>($1,$2)) AND game=ANY($4) \
             ORDER BY user_id,game LIMIT $3",
        )
        .bind(after.map(|(user, _)| user))
        .bind(after.map(|(_, game)| game))
        .bind(WALLET_PAGE)
        .bind(MEMORY_GAMES)
        .fetch_all(&mut *tx)
        .await?;
        let count = rows.len();
        if loaded.len() + count > MAX_WALLETS {
            return Err(sqlx::Error::Protocol(
                "wallet baseline capacity exceeded".into(),
            ));
        }
        for row in rows {
            let user: Uuid = row.get("user_id");
            let game: i16 = row.get("game");
            after = Some((user, game));
            if memory_authoritative(game) {
                loaded.push(((game, user), wallet_from(&row)));
            }
        }
        if count < WALLET_PAGE as usize {
            break;
        }
    }
    tx.commit().await?;
    for (key, wallet) in loaded {
        memory().insert(
            key,
            MemoryWallet {
                room: None,
                seated: false,
                wallet,
                failed: false,
            },
        );
    }
    baseline.ready.store(true, Ordering::Release);
    Ok(())
}

/// Only these domains publish every wallet counter through their room actor
/// and persist all mutations through its ordered checkpoint writer. Other
/// domains keep their existing active-seat projection and durable idle reads
/// until their complete writer lifecycle is migrated.
const MEMORY_GAMES: &[i16] = &[
    crate::game_catalog::DDZ,
    crate::game_catalog::NIUNIU,
    crate::game_catalog::SANGONG,
    crate::game_catalog::BLACKJACK,
    crate::game_catalog::XIANGQI,
];
fn memory_authoritative(game: i16) -> bool {
    MEMORY_GAMES.contains(&game)
}

pub(crate) fn projected(game: i16, user: Uuid) -> Option<Wallet> {
    memory()
        .get(&(game, user))
        .filter(|v| {
            !v.failed
                && (v.room.is_some()
                    || (crate::realtime_mode::enabled() && memory_authoritative(game)))
        })
        .map(|v| v.wallet.clone())
}
/// A balance projection still requires a complete, non-quarantined baseline.
pub(crate) fn projected_balance(game: i16, user: Uuid) -> Option<i64> {
    projected(game, user).map(|wallet| wallet.balance)
}

pub(crate) fn projected_room(game: i16, user: Uuid) -> Option<String> {
    memory()
        .get(&(game, user))
        .filter(|v| v.seated)
        .and_then(|v| v.room.clone())
}
/// The room a projection belongs to, seated or still awaiting its leave commit.
pub(crate) fn projection_room(game: i16, user: Uuid) -> Option<String> {
    memory().get(&(game, user)).and_then(|v| v.room.clone())
}
pub(crate) fn activate(game: i16, user: Uuid, room: &str, wallet: Wallet) {
    let mut entry = memory()
        .entry((game, user))
        .or_insert_with(|| MemoryWallet {
            room: None,
            seated: false,
            wallet: wallet.clone(),
            failed: false,
        });
    if entry.room.is_none() {
        entry.room = Some(room.to_owned());
        entry.seated = true;
        entry.wallet = wallet;
    }
}
/// Roll back a room session reset whose attach command was never accepted.
/// The caller first verifies its exact membership epoch under the hub binding
/// lock, so this cannot overwrite a later claim or accepted game projection.
pub(crate) fn cancel_admission(game: i16, user: Uuid, room: &str, previous: Wallet) {
    if let Some(mut entry) = memory().get_mut(&(game, user)) {
        if entry.room.is_none() || entry.room.as_deref() == Some(room) {
            entry.wallet = previous;
        }
    }
    retire(game, user, room);
}

/// An idle baseline does not reserve a seat or exclude another room.
pub(crate) fn held(game: i16, user: Uuid) -> bool {
    memory()
        .get(&(game, user))
        .is_some_and(|v| v.room.is_some())
}
pub(crate) fn reserved_elsewhere(game: i16, user: Uuid, room: &str) -> bool {
    memory()
        .get(&(game, user))
        .is_some_and(|v| v.room.as_deref().is_some_and(|held| held != room))
}
pub(crate) fn depart(game: i16, user: Uuid, room: &str) {
    if let Some(mut entry) = memory().get_mut(&(game, user)) {
        if entry.room.as_deref() == Some(room) {
            entry.seated = false;
        }
    }
}
pub(crate) fn retire(game: i16, user: Uuid, room: &str) {
    if memory_authoritative(game) {
        if let Some(mut entry) = memory().get_mut(&(game, user)) {
            if entry.room.as_deref() == Some(room) {
                entry.room = None;
                entry.seated = false;
            }
        }
    } else {
        memory().remove_if(&(game, user), |_, entry| {
            entry.room.as_deref() == Some(room)
        });
    }
}
/// A failed accepted membership invalidates every later optimistic projection
/// for that same wallet, including a successor room waiting on its receipt.
/// Values are retained for recovery; normal transient retries never call this.
pub(crate) fn quarantine_user(game: i16, user: Uuid) {
    if let Some(mut entry) = memory().get_mut(&(game, user)) {
        entry.failed = true;
    }
}

pub(crate) fn project_play(
    game: i16,
    user: Uuid,
    balance: i64,
    delta: i64,
    completed: bool,
    won: bool,
) {
    if let Some(mut entry) = memory().get_mut(&(game, user)) {
        entry.wallet.balance = balance;
        entry.wallet.net_won += delta;
        entry.wallet.hands_played += i32::from(completed);
        entry.wallet.hands_won += i32::from(completed && won);
    }
}
pub(crate) fn project_session_balance(game: i16, user: Uuid, balance: i64) {
    if let Some(mut entry) = memory().get_mut(&(game, user)) {
        entry.wallet.balance = balance;
    }
}
pub(crate) fn project_refill(game: i16, user: Uuid, balance: i64) {
    if let Some(mut entry) = memory().get_mut(&(game, user)) {
        entry.wallet.balance = balance;
        entry.wallet.refills += 1;
    }
}

/// Background admission/settlement writers must order themselves after the
/// initial grant. This barrier belongs off the player request/action path.
pub(crate) async fn await_initial_grant(game: i16, user: Uuid) -> sqlx::Result<()> {
    let pending = initializations()
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .get(&(game, user))
        .cloned();
    if let Some(pending) = pending {
        pending.await.map_err(sqlx::Error::Protocol)?;
    }
    if memory()
        .get(&(game, user))
        .is_some_and(|entry| entry.failed)
    {
        return Err(sqlx::Error::Protocol("wallet initialization failed".into()));
    }
    Ok(())
}

/// The caller holds the room checkpoint row lock in this transaction. Its
/// accepted purchase UUID is the receipt key, including lost-ACK retries.
// Each argument is one ledger column of the refill receipt.
#[allow(clippy::too_many_arguments)]
pub(crate) async fn refill_snapshot_on(
    conn: &mut PgConnection,
    game: i16,
    user: Uuid,
    before: i64,
    after: i64,
    code: &str,
    revision: i64,
    receipt: Uuid,
) -> sqlx::Result<()> {
    // Serialize receipts for a user before checking idempotency. Room codes and
    // revision counters are reusable; the accepted command's UUID is not.
    let balance = lock(conn, game, user)
        .await?
        .ok_or(sqlx::Error::RowNotFound)?;
    let exists: bool = sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM wallet_ledger WHERE game=$1 AND user_id=$2 AND kind='refill' AND details->>'buy_in_id'=$3)")
        .bind(game).bind(user).bind(receipt.to_string()).fetch_one(&mut *conn).await?;
    if exists {
        return Ok(());
    }
    if balance != before || after <= before {
        return Err(sqlx::Error::Protocol("buy-in wallet changed".into()));
    }
    sqlx::query("UPDATE wallets SET balance=$3,refills=refills+1,updated_at=now() WHERE game=$1 AND user_id=$2")
        .bind(game).bind(user).bind(after).execute(&mut *conn).await?;
    sqlx::query("INSERT INTO wallet_ledger(user_id,game,kind,delta,balance_after,room_code,revision,details) VALUES($1,$2,'refill',$3,$4,$5,$6,$7)")
        .bind(user).bind(game).bind(after-before).bind(after).bind(code).bind(revision)
        .bind(serde_json::json!({"buy_in_id": receipt})).execute(&mut *conn).await?;
    Ok(())
}

fn wallet_from(row: &sqlx::postgres::PgRow) -> Wallet {
    Wallet {
        balance: row.get("balance"),
        hands_played: row.get("hands_played"),
        hands_won: row.get("hands_won"),
        net_won: row.get("net_won"),
        refills: row.get("refills"),
    }
}

/// Migrated games read their complete runtime baseline. A known user's first
/// wallet is a real accepted grant: enqueue the atomic wallet/ledger write
/// before exposing it. Legacy idle reads and first grants remain durable until
/// every writer in that domain can publish a coherent full-wallet projection.
pub async fn ensure(pool: &PgPool, policy: Policy, user_id: Uuid) -> sqlx::Result<Wallet> {
    if !crate::realtime_mode::enabled() || !memory_authoritative(policy.game) {
        if held(policy.game, user_id) {
            if let Some(wallet) = projected(policy.game, user_id) {
                return Ok(wallet);
            }
        }
        // Legacy transaction callers require a committed first grant before
        // locking or creating this wallet themselves. No optimistic idle
        // baseline may hide their independently committed mutations.
        return ensure_durable(pool, policy, user_id).await;
    }
    let baseline = baseline_state(pool)?;
    if !baseline.ready.load(Ordering::Acquire) {
        return Err(sqlx::Error::Protocol("wallet baseline not ready".into()));
    }
    let player = crate::player_directory::lookup(pool, user_id)?.ok_or(sqlx::Error::RowNotFound)?;
    if player.deleted_at.is_some() {
        return Err(sqlx::Error::RowNotFound);
    }
    if let Some(entry) = memory().get(&(policy.game, user_id)) {
        if entry.failed {
            return Err(sqlx::Error::Protocol("wallet initialization failed".into()));
        }
        return Ok(entry.wallet.clone());
    }
    if policy.grant < 0 || !crate::write_behind::accepting() {
        return Err(sqlx::Error::Protocol(
            "wallet initialization unavailable".into(),
        ));
    }
    let mut pending = initializations().lock().unwrap_or_else(|e| e.into_inner());
    // Another first-use request can have published while this one took the lock.
    if let Some(entry) = memory().get(&(policy.game, user_id)) {
        if entry.failed {
            return Err(sqlx::Error::Protocol("wallet initialization failed".into()));
        }
        return Ok(entry.wallet.clone());
    }
    if pending.len() >= MAX_INITIALIZATIONS || memory().len() >= MAX_WALLETS {
        return Err(sqlx::Error::Protocol(
            "wallet initialization capacity reached".into(),
        ));
    }
    let wallet = Wallet {
        balance: policy.grant,
        hands_played: 0,
        hands_won: 0,
        net_won: 0,
        refills: 0,
    };
    let receipt = Arc::new(AtomicI64::new(0));
    let writer_pool = pool.clone();
    let mut writer = crate::write_behind::Ordered::new(false, move |_old, next| {
        let (pool, receipt) = (writer_pool.clone(), receipt.clone());
        async move {
            match persist_initial_grant(&pool, policy, user_id, &receipt).await {
                Ok(()) => Ok(next),
                Err(error) => {
                    if matches!(error, sqlx::Error::RowNotFound)
                        || matches!(&error, sqlx::Error::Protocol(message) if message == "wallet_initialization_conflict")
                    {
                        if let Some(mut entry) = memory().get_mut(&(policy.game, user_id)) {
                            entry.failed = true;
                        }
                        return Err(crate::write_behind::fenced("wallet initialization refused"));
                    }
                    Err(error.to_string())
                }
            }
        }
    });
    writer.enqueue(true).map_err(sqlx::Error::Protocol)?;
    let durable = writer.durable().boxed().shared();
    memory().insert(
        (policy.game, user_id),
        MemoryWallet {
            room: None,
            seated: false,
            wallet: wallet.clone(),
            failed: false,
        },
    );
    pending.insert((policy.game, user_id), durable.clone());
    drop(pending);
    // Dropping the sender does not discard its accepted item: Ordered retains
    // and retries that item, and the global shutdown drain tracks its worker.
    drop(writer);
    tokio::spawn(async move {
        if durable.await.is_err() {
            // The worker may finish before the accepting task publishes the
            // entry. This continuation is spawned only after publication.
            if let Some(mut entry) = memory().get_mut(&(policy.game, user_id)) {
                entry.failed = true;
            }
        }
        initializations()
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .remove(&(policy.game, user_id));
    });
    Ok(wallet)
}

async fn persist_initial_grant(
    pool: &PgPool,
    policy: Policy,
    user: Uuid,
    receipt: &AtomicI64,
) -> sqlx::Result<()> {
    static WRITERS: tokio::sync::Semaphore = tokio::sync::Semaphore::const_new(4);
    let _permit = WRITERS
        .acquire()
        .await
        .map_err(|_| sqlx::Error::PoolClosed)?;
    let mut tx = pool.begin().await?;
    crate::db::lock_active_users(&mut tx, &[user]).await?;
    let inserted = sqlx::query(
        "INSERT INTO wallets(user_id,game,balance) VALUES($1,$2,$3) ON CONFLICT(user_id,game) DO NOTHING",
    ).bind(user).bind(policy.game).bind(policy.grant).execute(&mut *tx).await?.rows_affected();
    if inserted == 1 {
        let id: i64 = sqlx::query_scalar(
            "INSERT INTO wallet_ledger(user_id,game,kind,delta,balance_after) VALUES($1,$2,'grant',$3,$3) RETURNING id",
        ).bind(user).bind(policy.game).bind(policy.grant).fetch_one(&mut *tx).await?;
        // Retain the exact receipt before COMMIT: a lost acknowledgement can
        // then be proven by one primary-key lookup without replaying the grant.
        receipt.store(id, Ordering::Release);
    } else {
        let id = receipt.load(Ordering::Acquire);
        let committed = if id > 0 {
            sqlx::query_scalar::<_, bool>(
                "SELECT EXISTS(SELECT 1 FROM wallet_ledger WHERE id=$1 AND user_id=$2 AND game=$3 AND kind='grant' AND delta=$4 AND balance_after=$4)",
            ).bind(id).bind(user).bind(policy.game).bind(policy.grant).fetch_one(&mut *tx).await?
        } else {
            false
        };
        if !committed {
            return Err(sqlx::Error::Protocol(
                "wallet_initialization_conflict".into(),
            ));
        }
    }
    tx.commit().await?;
    drop(_permit);
    crate::card_ledger_memory::retain_publish_grant(pool, policy.game, user).await;
    Ok(())
}

/// Recovery can settle an interrupted hand after startup hydration. Refresh
/// its complete wallet once after those durable writes and before activating
/// the restored actor. Ordinary requests and actions never call this helper.
pub(crate) async fn recover_wallet(
    pool: &PgPool,
    policy: Policy,
    user: Uuid,
    room: &str,
) -> sqlx::Result<Wallet> {
    if !memory().contains_key(&(policy.game, user)) {
        // Missing startup rows use the same initializer as runtime admission;
        // never race a second first-grant INSERT against its accepted receipt.
        ensure(pool, policy, user).await?;
    }
    let pending = initializations()
        .lock()
        .unwrap_or_else(|error| error.into_inner())
        .get(&(policy.game, user))
        .cloned();
    if let Some(pending) = pending {
        pending.await.map_err(sqlx::Error::Protocol)?;
    }
    let wallet = ensure_durable(pool, policy, user).await?;
    if crate::realtime_mode::enabled() && memory_authoritative(policy.game) {
        let mut entry = memory()
            .entry((policy.game, user))
            .or_insert_with(|| MemoryWallet {
                room: None,
                seated: false,
                wallet: wallet.clone(),
                failed: false,
            });
        if entry.room.as_deref().is_some_and(|held| held != room) {
            return Err(sqlx::Error::Protocol(
                "wallet already owned during recovery".into(),
            ));
        }
        entry.wallet = wallet.clone();
        entry.failed = false;
    }
    Ok(wallet)
}

/// Creates the wallet with its grant on first use; idempotent afterwards.
async fn ensure_durable(pool: &PgPool, policy: Policy, user_id: Uuid) -> sqlx::Result<Wallet> {
    // Profile reads must not start a write transaction for an existing wallet.
    // Read fresh committed balances; keep first-use grant + ledger atomic below.
    let started = std::time::Instant::now();
    let mut conn = pool.acquire().await?;
    let acquired = std::time::Instant::now();
    let row = sqlx::query(
        "SELECT balance, hands_played, hands_won, net_won, refills \
         FROM wallets WHERE user_id = $1 AND game = $2",
    )
    .bind(user_id)
    .bind(policy.game)
    .fetch_optional(&mut *conn)
    .await?;
    tracing::debug!(target: "game_latency", game=policy.game, stage="wallet_read",
        acquire_ms=(acquired-started).as_secs_f64()*1000.0,
        query_ms=acquired.elapsed().as_secs_f64()*1000.0);
    drop(conn);
    if let Some(row) = row {
        // A previous first-grant COMMIT acknowledgement may have been lost.
        // This committed row is its retry path; reconcile the real grant
        // receipts even though this attempt does not create another wallet.
        crate::card_ledger_memory::retain_publish_grant(pool, policy.game, user_id).await;
        return Ok(wallet_from(&row));
    }
    let _beans_mutation = (policy.game == crate::game_catalog::BEANS)
        .then(|| crate::beans_read_memory::mutation(pool, [user_id]));
    let mut tx = pool.begin().await?;
    let inserted = sqlx::query(
        "INSERT INTO wallets (user_id, game, balance) VALUES ($1, $2, $3) \
         ON CONFLICT (user_id, game) DO NOTHING",
    )
    .bind(user_id)
    .bind(policy.game)
    .bind(policy.grant)
    .execute(&mut *tx)
    .await?
    .rows_affected();
    if inserted == 1 {
        sqlx::query(
            "INSERT INTO wallet_ledger (user_id, game, kind, delta, balance_after) \
             VALUES ($1, $2, 'grant', $3, $3)",
        )
        .bind(user_id)
        .bind(policy.game)
        .bind(policy.grant)
        .execute(&mut *tx)
        .await?;
    }
    let row = sqlx::query(
        "SELECT balance, hands_played, hands_won, net_won, refills \
         FROM wallets WHERE user_id = $1 AND game = $2",
    )
    .bind(user_id)
    .bind(policy.game)
    .fetch_one(&mut *tx)
    .await?;
    let committed = tx.commit().await;
    if policy.game == crate::game_catalog::BEANS {
        crate::beans_read_memory::invalidate_user(pool, user_id);
    }
    committed?;
    crate::card_ledger_memory::retain_publish_grant(pool, policy.game, user_id).await;
    Ok(wallet_from(&row))
}

/// Start a new, independent session under the same transaction as admission.
/// The wallet is the durable backing for the current session's
/// stack. A grant adjustment preserves its ledger invariant without counting
/// the reset as winnings, a played hand, or a refill. Reconnects never call this.
pub(crate) async fn start_session_on(
    conn: &mut PgConnection,
    policy: Policy,
    user: Uuid,
    room: Option<&str>,
) -> sqlx::Result<i64> {
    // Existing wallets need only one round trip. Lock and retain the latest
    // balance before the reset so the receipt includes any preceding writer.
    // The missing-row path below still arbitrates concurrent first inserts.
    let written = sqlx::query(
        "WITH previous AS MATERIALIZED (SELECT user_id,balance FROM wallets \
           WHERE user_id=$1 AND game=$2 FOR UPDATE), \
         reset AS (UPDATE wallets AS w SET balance=$3,updated_at=now() FROM previous \
           WHERE w.user_id=previous.user_id AND w.game=$2 \
           RETURNING w.user_id,previous.balance AS before) \
         INSERT INTO wallet_ledger(user_id,game,kind,delta,balance_after,room_code) \
         SELECT user_id,$2,'grant',$3-before,$3,$4 FROM reset",
    )
    .bind(user)
    .bind(policy.game)
    .bind(policy.grant)
    .bind(room)
    .execute(&mut *conn)
    .await?
    .rows_affected();
    if written == 1 {
        return Ok(policy.grant);
    }
    // Create the wallet at 0 when missing, or lock the existing row, and read
    // its balance in one round trip: the no-op ON CONFLICT update takes the
    // same row lock `lock` would and returns the latest committed balance.
    let before: i64 = sqlx::query_scalar(
        "INSERT INTO wallets(user_id,game,balance) VALUES($1,$2,0) \
         ON CONFLICT(user_id,game) DO UPDATE SET balance=wallets.balance RETURNING balance",
    )
    .bind(user)
    .bind(policy.game)
    .fetch_one(&mut *conn)
    .await?;
    // The reset and its ledger row in one statement; the row is locked above,
    // so the update always matches and exactly one ledger row is written.
    let written = sqlx::query(
        "WITH reset AS (UPDATE wallets SET balance=$3,updated_at=now() WHERE game=$2 AND user_id=$1 RETURNING user_id) \
         INSERT INTO wallet_ledger(user_id,game,kind,delta,balance_after,room_code) \
         SELECT user_id,$2,'grant',$4,$3,$5 FROM reset",
    )
    .bind(user)
    .bind(policy.game)
    .bind(policy.grant)
    .bind(policy.grant - before)
    .bind(room)
    .execute(conn)
    .await?
    .rows_affected();
    if written != 1 {
        return Err(sqlx::Error::RowNotFound);
    }
    Ok(policy.grant)
}

pub(crate) fn room_policy(name: &str) -> Option<Policy> {
    match name {
        "ddz" => Some(crate::ddz::store::POLICY),
        "niuniu" => Some(crate::niuniu::store::POLICY),
        "sangong" => Some(crate::sangong::store::POLICY),
        "blackjack" => Some(crate::blackjack::store::POLICY),
        "xiangqi" => Some(crate::xiangqi::store::POLICY),
        _ => None,
    }
}

pub(crate) fn session_policy(
    name: &str,
    state: &serde_json::Value,
) -> sqlx::Result<Option<Policy>> {
    let Some(mut policy) = room_policy(name) else {
        return Ok(None);
    };
    if matches!(name, "ddz" | "niuniu" | "sangong" | "blackjack") {
        let opts = &state["opts"];
        policy.grant = opts["initial_antes"]
            .as_u64()
            .and_then(|n| opts["ante"].as_u64().and_then(|ante| n.checked_mul(ante)))
            .and_then(|n| i64::try_from(n).ok())
            .ok_or_else(|| sqlx::Error::Protocol("missing session buy-in".into()))?;
    }
    Ok(Some(policy))
}

pub async fn balances(
    pool: &PgPool,
    game: i16,
    user_ids: &[Uuid],
) -> sqlx::Result<HashMap<Uuid, i64>> {
    if user_ids.is_empty() {
        return Ok(HashMap::new());
    }
    if crate::realtime_mode::enabled() && memory_authoritative(game) {
        if !ready(pool) {
            return Err(sqlx::Error::Protocol("wallet baseline not ready".into()));
        }
        let mut balances = HashMap::with_capacity(user_ids.len());
        for user in user_ids {
            if let Some(entry) = memory().get(&(game, *user)) {
                if entry.failed {
                    return Err(sqlx::Error::Protocol("wallet initialization failed".into()));
                }
                balances.insert(*user, entry.wallet.balance);
            }
        }
        return Ok(balances);
    }
    let rows =
        sqlx::query("SELECT user_id, balance FROM wallets WHERE game = $1 AND user_id = ANY($2)")
            .bind(game)
            .bind(user_ids)
            .fetch_all(pool)
            .await?;
    Ok(rows
        .into_iter()
        .map(|r| (r.get::<Uuid, _>("user_id"), r.get::<i64, _>("balance")))
        .collect())
}

/// Locks the wallet row for the rest of the transaction and returns its
/// balance. `None` when the user has no wallet for this game.
pub async fn lock(conn: &mut PgConnection, game: i16, user_id: Uuid) -> sqlx::Result<Option<i64>> {
    sqlx::query_scalar("SELECT balance FROM wallets WHERE user_id = $1 AND game = $2 FOR UPDATE")
        .bind(user_id)
        .bind(game)
        .fetch_optional(conn)
        .await
}

/// One user's share of one settled play (a hand, or a kwx checkpoint revision).
#[derive(Debug, Clone)]
pub struct Play<'a> {
    pub record_id: Uuid,
    pub delta: i64,
    /// Counts one hand played (and one won when `won`) on the wallet.
    pub completes_hand: bool,
    pub won: bool,
    pub room_code: Option<&'a str>,
    pub revision: Option<i64>,
    pub details: Option<serde_json::Value>,
    pub hand_completed: Option<bool>,
    pub has_bots: Option<bool>,
}

impl<'a> Play<'a> {
    /// A whole hand settled at once (zjh / ddz): counts as played, won when
    /// the delta is positive.
    pub fn hand(record_id: Uuid, delta: i64, room_code: &'a str) -> Self {
        Play {
            record_id,
            delta,
            completes_hand: true,
            won: delta > 0,
            room_code: Some(room_code),
            revision: None,
            details: None,
            hand_completed: None,
            has_bots: None,
        }
    }
}

/// Applies one play to the wallet and writes its ledger row. Returns the
/// balance after. A delta that drives the balance negative fails the CHECK
/// constraint, which rolls back the caller's transaction.
pub async fn settle(
    conn: &mut PgConnection,
    game: i16,
    user_id: Uuid,
    play: &Play<'_>,
) -> sqlx::Result<i64> {
    // The ledger consumes the exact updated balance in the same statement.
    // This removes a network round trip per player without moving either write
    // outside the caller's settlement transaction or changing wallet lock order.
    sqlx::query_scalar(
        "WITH updated AS (UPDATE wallets \
            SET balance = balance + $3, \
                net_won = net_won + $3, \
                hands_played = hands_played + $4, \
                hands_won = hands_won + $5, \
                updated_at = now() \
          WHERE user_id = $1 AND game = $2 \
      RETURNING balance) \
         INSERT INTO wallet_ledger \
            (user_id, game, kind, record_id, room_code, revision, delta, balance_after, \
             details, hand_completed, has_bots) \
         SELECT $1, $2, 'play', $6, $7, $8, $3, balance, $9, $10, $11 FROM updated \
         RETURNING balance_after",
    )
    .bind(user_id)
    .bind(game)
    .bind(play.delta)
    .bind(i32::from(play.completes_hand))
    .bind(i32::from(play.completes_hand && play.won))
    .bind(play.record_id)
    .bind(play.room_code)
    .bind(play.revision)
    .bind(&play.details)
    .bind(play.hand_completed)
    .bind(play.has_bots)
    .fetch_one(conn)
    .await
}

/// Balances written for an already-settled record (idempotent retry path).
pub async fn settled_balances(
    conn: &mut PgConnection,
    game: i16,
    record_id: Uuid,
) -> sqlx::Result<HashMap<Uuid, i64>> {
    let rows = sqlx::query(
        "SELECT user_id, balance_after FROM wallet_ledger WHERE game = $1 AND record_id = $2",
    )
    .bind(game)
    .bind(record_id)
    .fetch_all(conn)
    .await?;
    Ok(rows
        .into_iter()
        .map(|r| (r.get("user_id"), r.get("balance_after")))
        .collect())
}

/// 卡五星 has no per-hand id: a settled checkpoint revision is identified by
/// `md5(room_code:revision)`, the same expression migration 0102 used to
/// backfill historical rows.
pub fn kwx_record_id(room_code: &str, revision: i64) -> Uuid {
    let digest = Md5::digest(format!("{room_code}:{revision}").as_bytes());
    Uuid::from_bytes(digest.into())
}

#[derive(Debug, Clone, Serialize)]
pub struct LedgerRow {
    pub id: i64,
    pub kind: String,
    pub record_id: Option<Uuid>,
    pub room_code: Option<String>,
    pub revision: Option<i64>,
    pub delta: i64,
    pub balance_after: i64,
    pub details: Option<serde_json::Value>,
    pub hand_completed: Option<bool>,
    pub has_bots: Option<bool>,
    pub created_at: DateTime<Utc>,
}

pub async fn ledger(
    pool: &PgPool,
    game: i16,
    user_id: Uuid,
    limit: i64,
    offset: i64,
) -> sqlx::Result<Vec<LedgerRow>> {
    let rows = sqlx::query(
        "SELECT id, kind, record_id, room_code, revision, delta, balance_after, details, \
                hand_completed, has_bots, created_at \
           FROM wallet_ledger \
          WHERE user_id = $1 AND game = $2 AND kind = 'play' \
          ORDER BY id DESC \
          LIMIT $3 OFFSET $4",
    )
    .bind(user_id)
    .bind(game)
    .bind(crate::pagination::limit(Some(limit)))
    .bind(crate::pagination::offset(Some(offset)))
    .fetch_all(pool)
    .await?;
    Ok(rows
        .into_iter()
        .map(|r| LedgerRow {
            id: r.get("id"),
            kind: r.get("kind"),
            record_id: r.get("record_id"),
            room_code: r.get("room_code"),
            revision: r.get("revision"),
            delta: r.get("delta"),
            balance_after: r.get("balance_after"),
            details: r.get("details"),
            hand_completed: r.get("hand_completed"),
            has_bots: r.get("has_bots"),
            created_at: r.get("created_at"),
        })
        .collect())
}
