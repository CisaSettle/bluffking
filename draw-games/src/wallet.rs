//! Gold-bean wallet row primitives and the live balance projection of the
//! seats that play on beans.
//!
//! One `wallets` row per user (game 10); every balance change is one
//! `wallet_ledger` row written in the same transaction, so a balance always
//! equals the sum of that wallet's ledger deltas. Poker chips (`users.chips`)
//! and room-scoped points never pass through here.

use crate::db_writer::{PgConnection, PgPool};
use std::sync::OnceLock;

use serde::Serialize;
use sqlx::Row;
use uuid::Uuid;

/// The wallet's game code and its first-use grant.
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

// The projection of a seated wallet, updated under one lock. It exists only
// while a room holds the seat; leaving removes it.
struct MemoryWallet {
    room: Option<String>,
    wallet: Wallet,
    failed: bool,
}
fn memory() -> &'static dashmap::DashMap<(i16, Uuid), MemoryWallet> {
    static CACHE: OnceLock<dashmap::DashMap<(i16, Uuid), MemoryWallet>> = OnceLock::new();
    CACHE.get_or_init(Default::default)
}

pub(crate) fn projected(game: i16, user: Uuid) -> Option<Wallet> {
    memory()
        .get(&(game, user))
        .filter(|v| !v.failed && v.room.is_some())
        .map(|v| v.wallet.clone())
}
pub(crate) fn activate(game: i16, user: Uuid, room: &str, wallet: Wallet) {
    let mut entry = memory()
        .entry((game, user))
        .or_insert_with(|| MemoryWallet {
            room: None,
            wallet: wallet.clone(),
            failed: false,
        });
    if entry.room.is_none() {
        entry.room = Some(room.to_owned());
        entry.wallet = wallet;
    }
}
/// An idle baseline does not reserve a seat or exclude another room.
pub(crate) fn held(game: i16, user: Uuid) -> bool {
    memory()
        .get(&(game, user))
        .is_some_and(|v| v.room.is_some())
}
pub(crate) fn retire(game: i16, user: Uuid, room: &str) {
    memory().remove_if(&(game, user), |_, entry| {
        entry.room.as_deref() == Some(room)
    });
}
/// A failed accepted membership invalidates every later optimistic projection
/// for that same wallet, including a successor room waiting on its receipt.
/// Values are retained for recovery; normal transient retries never call this.
pub(crate) fn quarantine_user(game: i16, user: Uuid) {
    if let Some(mut entry) = memory().get_mut(&(game, user)) {
        entry.failed = true;
    }
}

pub(crate) fn project_session_balance(game: i16, user: Uuid, balance: i64) {
    if let Some(mut entry) = memory().get_mut(&(game, user)) {
        entry.wallet.balance = balance;
    }
}
/// Publish a newly committed external credit without replacing accepted room
/// effects. Call once on the fresh-commit path, never on receipt replay.
pub(crate) fn project_external_credit(game: i16, user: Uuid, amount: i64) {
    if let Some(mut entry) = memory().get_mut(&(game, user)) {
        if let Some(balance) = entry.wallet.balance.checked_add(amount) {
            entry.wallet.balance = balance;
        } else {
            // The durable credit already succeeded. Fail this display/admission
            // projection closed rather than wrapping or reporting a failed grant.
            entry.failed = true;
            tracing::error!(game, "wallet_external_credit_projection_overflow");
        }
    }
}
/// Bean room settlements must use the same atomic delta update as external
/// credits, rather than clone a balance and later overwrite intervening grants.
pub(crate) fn project_bean_settlement(user: Uuid, delta: i64, completed: bool, won: bool) {
    if let Some(mut entry) = memory().get_mut(&(crate::game_catalog::BEANS, user)) {
        if let Some(balance) = entry.wallet.balance.checked_add(delta) {
            entry.wallet.balance = balance;
            entry.wallet.net_won += delta;
            entry.wallet.hands_played += i32::from(completed);
            entry.wallet.hands_won += i32::from(completed && won);
        } else {
            entry.failed = true;
            tracing::error!("wallet_bean_settlement_projection_overflow");
        }
    }
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

/// A seated wallet reads its room projection; otherwise the committed row,
/// created with its first-use grant when missing.
pub async fn ensure(pool: &PgPool, policy: Policy, user_id: Uuid) -> sqlx::Result<Wallet> {
    if held(policy.game, user_id) {
        if let Some(wallet) = projected(policy.game, user_id) {
            return Ok(wallet);
        }
    }
    ensure_durable(pool, policy, user_id).await
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
        return Ok(wallet_from(&row));
    }
    let _beans_mutation = crate::beans_read_memory::mutation(pool, [user_id]);
    let mut tx = pool.begin().await?;
    // The one first-use bean grant; an erased account gets none and the
    // read below answers RowNotFound.
    crate::bean_ledger::grant_on(&mut tx, user_id).await?;
    let row = sqlx::query(
        "SELECT balance, hands_played, hands_won, net_won, refills \
         FROM wallets WHERE user_id = $1 AND game = $2",
    )
    .bind(user_id)
    .bind(policy.game)
    .fetch_one(&mut *tx)
    .await?;
    let committed = tx.commit().await;
    crate::beans_read_memory::invalidate_user(pool, user_id);
    committed?;
    Ok(wallet_from(&row))
}

/// Locks the wallet row for the rest of the transaction and returns its
/// balance. `None` when the user has no wallet for this game.
pub async fn lock(conn: &mut PgConnection, game: i16, user_id: Uuid) -> sqlx::Result<Option<i64>> {
    sqlx::query_scalar("SELECT balance FROM wallets WHERE user_id = $1 AND game = $2 FOR UPDATE")
        .bind(user_id)
        .bind(game)
        .fetch_optional(conn.writer())
        .await
}

/// A settlement refused because one of its play receipt keys
/// `(game, record_id, user)` was already taken: no retry can succeed.
pub(crate) const PLAY_RECEIPT_CONFLICT: &str = "play_receipt_id_conflict";
