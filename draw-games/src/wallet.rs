//! Practice-chip wallet primitives and their in-memory seated projection.
//!
//! One `wallets` row per (user, game); every balance change is one
//! `wallet_ledger` row written in the same transaction, so a balance always
//! equals the sum of that wallet's ledger deltas. Poker chips (`users.chips`)
//! are a different economy and never pass through here.

use std::collections::HashMap;

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

#[derive(Debug, Clone, Serialize)]
pub struct Wallet {
    pub balance: i64,
    pub hands_played: i32,
    pub hands_won: i32,
    pub net_won: i64,
    pub refills: i32,
}

// Authoritative projection for seated players. It is installed before play,
// updated by the room owner, and retired only after the leave checkpoint commits.
struct MemoryWallet {
    room: String,
    seated: bool,
    wallet: Wallet,
}
fn memory() -> &'static dashmap::DashMap<(i16, Uuid), MemoryWallet> {
    static CACHE: std::sync::OnceLock<dashmap::DashMap<(i16, Uuid), MemoryWallet>> =
        std::sync::OnceLock::new();
    CACHE.get_or_init(Default::default)
}
pub(crate) fn projected(game: i16, user: Uuid) -> Option<Wallet> {
    memory().get(&(game, user)).map(|v| v.wallet.clone())
}
pub(crate) fn projected_room(game: i16, user: Uuid) -> Option<String> {
    memory()
        .get(&(game, user))
        .filter(|v| v.seated)
        .map(|v| v.room.clone())
}
/// The room a projection belongs to, seated or still waiting on its leave checkpoint.
pub(crate) fn projection_room(game: i16, user: Uuid) -> Option<String> {
    memory().get(&(game, user)).map(|v| v.room.clone())
}
pub(crate) fn activate(game: i16, user: Uuid, room: &str, wallet: Wallet) {
    memory()
        .entry((game, user))
        .or_insert_with(|| MemoryWallet {
            room: room.to_owned(),
            seated: true,
            wallet,
        });
}
/// Whether any room still holds `user`'s session wallet of `game`.
pub(crate) fn held(game: i16, user: Uuid) -> bool {
    memory().contains_key(&(game, user))
}
pub(crate) fn reserved_elsewhere(game: i16, user: Uuid, room: &str) -> bool {
    memory().get(&(game, user)).is_some_and(|v| v.room != room)
}
pub(crate) fn depart(game: i16, user: Uuid, room: &str) {
    if let Some(mut entry) = memory().get_mut(&(game, user)) {
        if entry.room == room {
            entry.seated = false;
        }
    }
}
pub(crate) fn retire(game: i16, user: Uuid, room: &str) {
    memory().remove_if(&(game, user), |_, v| v.room == room);
}
/// Retires every projection a room installed, whoever it belonged to.
pub(crate) fn retire_room(game: i16, room: &str) {
    memory().retain(|(g, _), v| *g != game || v.room != room);
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

/// Creates the wallet with its grant on first use; idempotent afterwards.
pub async fn ensure(pool: &PgPool, policy: Policy, user_id: Uuid) -> sqlx::Result<Wallet> {
    if let Some(wallet) = projected(policy.game, user_id) {
        return Ok(wallet);
    }
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
        return Ok(wallet_from(&row));
    }
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
    tx.commit().await?;
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

pub async fn balances(
    pool: &PgPool,
    game: i16,
    user_ids: &[Uuid],
) -> sqlx::Result<HashMap<Uuid, i64>> {
    if user_ids.is_empty() {
        return Ok(HashMap::new());
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
) -> sqlx::Result<Vec<LedgerRow>> {
    let rows = sqlx::query(
        "SELECT id, kind, record_id, room_code, revision, delta, balance_after, details, \
                hand_completed, has_bots, created_at \
           FROM wallet_ledger \
          WHERE user_id = $1 AND game = $2 AND kind = 'play' \
          ORDER BY id DESC \
          LIMIT $3",
    )
    .bind(user_id)
    .bind(game)
    .bind(limit)
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
