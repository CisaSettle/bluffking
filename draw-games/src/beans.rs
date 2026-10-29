//! 金豆 (gold beans): the platform-wide practice wallet behind public tables.
//!
//! Beans are shared by public quick-join tables and the lottery center;
//! friend rooms and solo practice keep their own chip models. Every user
//! starts with [`GRANT`] beans, may claim an extra [`DAILY_REFILL_TO`] per day
//! and, only while [`pack_enabled`] is on, may buy a [`PACK_BEANS`] pack.
//! Beans have no monetary value, cannot be
//! cashed out, transferred or gifted.
//!
//! Accounting model: the wallet balance always INCLUDES the stack a player has
//! on a public table. Each completed public-table hand settles its per-player
//! delta into the wallet (idempotent on `(game, hand_id, user_id)`), so the
//! invariant `balance >= stack on table` holds between hands. Sitting down
//! requires `balance >= buy-in`; an explicit re-buy after a bust requires the
//! same (the bust stack is 0). Mid-hand top-ups reserve only wallet headroom
//! above the durable hand-start stack and earlier pending top-ups; they apply
//! at the next hand boundary without changing the current pot or wallet.

use serde::Serialize;
use sqlx::{PgConnection, PgPool};
use uuid::Uuid;

use crate::game_catalog::BEANS as GAME;
use crate::wallet::{self, Policy, Wallet};

/// First-use grant (owner: 默认每人一万金豆).
pub const GRANT: i64 = 10_000;
/// Extra beans ADDED by the once-per-Singapore-day claim (not a refill-to
/// target; owner 2026-10-29: 10,000 → 2,000).
pub const DAILY_REFILL_TO: i64 = 2_000;
/// Public beans table shape. One stake keeps the thin player pool together.
pub const STAKE_KEY: &str = "10/20";
pub const SB: u32 = 10;
pub const BB: u32 = 20;
pub const BUY_IN: u32 = 2_000;
pub const SEATS: i16 = 6;
/// Paid pack (owner anchor ¥100 ≈ US$13.99 for 10,000).
pub const PACK_BEANS: i64 = 10_000;
pub const PACK_USD_CENTS_DEFAULT: i64 = 1_399;
const DAILY_FEATURE: &str = "beans_daily_refill";

pub const POLICY: Policy = Policy {
    game: GAME,
    grant: GRANT,
};

pub fn pack_usd_cents() -> i64 {
    std::env::var("BILLING_BEANS_PACK_USD_CENTS")
        .ok()
        .and_then(|v| v.trim().parse::<i64>().ok())
        .filter(|v| *v > 0)
        .unwrap_or(PACK_USD_CENTS_DEFAULT)
}

/// Whether the paid pack may be sold at all. Fail-closed (owner 2026-10-13):
/// the purchase entry stays off until counsel clears selling beans; set
/// `BILLING_BEANS_PACK_ENABLED=1` to reopen it. The daily free refill, the
/// Stripe webhook (fulfilment and refunds of already-paid packs) and the
/// public tables themselves are unaffected.
pub fn pack_enabled() -> bool {
    std::env::var("BILLING_BEANS_PACK_ENABLED")
        .map(|v| matches!(v.trim(), "1" | "true" | "yes"))
        .unwrap_or(false)
}

/// Creates the wallet with its grant on first use; idempotent afterwards.
pub async fn ensure(pool: &PgPool, user: Uuid) -> sqlx::Result<Wallet> {
    wallet::ensure(pool, POLICY, user).await
}

/// Call with the shared wallet locked, before a new debit. Poker admission
/// takes the same lock, so a stack cannot be spent in two games at once.
pub async fn sicbo_seated(conn: &mut PgConnection, user: Uuid) -> sqlx::Result<bool> {
    sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM game_room_checkpoint_members m JOIN game_room_checkpoints c ON c.game=m.game AND c.code=m.code WHERE m.game='sicbo' AND m.user_id=$1)")
        .bind(user).fetch_one(conn).await
}

pub async fn spendable(pool: &PgPool, conn: &mut PgConnection, user: Uuid) -> sqlx::Result<bool> {
    // Memory admission is visible before its SQL membership is persisted. All
    // debit callers hold beans_read_memory::Mutation before BEGIN, excluding
    // another admission until their transaction and publication are complete.
    let seated = if crate::realtime_mode::enabled() {
        let directory = crate::live_directory::memory_snapshot(pool)?;
        let occupied = directory.memberships_for(user).any(|(room, member)| {
            member.left_at.is_none()
                && room.ended_at.is_none()
                && directory
                    .public_tables
                    .iter()
                    .any(|table| table.beans && table.session_id == Some(room.id))
        });
        occupied
    } else {
        sqlx::query_scalar(
        "SELECT EXISTS(SELECT 1 FROM session_users su JOIN game_sessions gs ON gs.id=su.session_id
         JOIN public_tables pt ON pt.session_id=gs.id
         WHERE su.user_id=$1 AND su.left_at IS NULL AND gs.ended_at IS NULL AND pt.beans)",
    )
    .bind(user)
    .fetch_one(&mut *conn)
    .await?
    };
    Ok(!seated
        && !sicbo_seated(conn, user).await?
        && !holds::outstanding(user)
        && !crate::local_runtime::beans_held(user).map_err(sqlx::Error::Io)?)
}

/// One affordability rule for every SQL bean debit. The old overlapping-owner
/// mode keeps its exclusive-wallet guard; numeric reservations require the
/// current single-owner runtime and its checkpoint authority.
pub async fn available_on(
    pool: &PgPool,
    conn: &mut PgConnection,
    user: Uuid,
    balance: i64,
    excluding_sicbo: Option<&str>,
) -> sqlx::Result<i64> {
    if !crate::realtime_mode::enabled() {
        if excluding_sicbo.is_some() {
            let poker_seated: bool = sqlx::query_scalar(
                "SELECT EXISTS(SELECT 1 FROM session_users su JOIN game_sessions gs ON gs.id=su.session_id JOIN public_tables pt ON pt.session_id=gs.id WHERE su.user_id=$1 AND su.left_at IS NULL AND gs.ended_at IS NULL AND pt.beans)",
            ).bind(user).fetch_one(&mut *conn).await?;
            return Ok(
                if poker_seated
                    || holds::outstanding(user)
                    || crate::local_runtime::beans_held(user).map_err(sqlx::Error::Io)?
                {
                    0
                } else {
                    balance
                },
            );
        }
        return Ok(if spendable(pool, conn, user).await? {
            balance
        } else {
            0
        });
    }
    if crate::local_runtime::enabled() {
        let code: Option<String> = sqlx::query_scalar(
            "SELECT code FROM game_room_checkpoint_members WHERE game='sicbo' AND user_id=$1",
        )
        .bind(user)
        .fetch_optional(&mut *conn)
        .await?;
        if code.is_some_and(|code| {
            crate::local_runtime::owned_by_another_backend(crate::release_handoff::rooms::id(
                "sicbo", &code,
            ))
        }) {
            return Err(sqlx::Error::Protocol(
                "beans read memory unavailable".into(),
            ));
        }
    }
    crate::beans_read_memory::available(pool, user, balance, true, excluding_sicbo)
}

#[derive(Debug, Clone, Serialize)]
pub struct Profile {
    pub balance: i64,
    pub available_balance: i64,
    pub reserved_balance: i64,
    pub net_won: i64,
    pub hands_played: i32,
    pub hands_won: i32,
    pub buy_in: u32,
    pub stake: &'static str,
    pub daily_refill_to: i64,
    /// True when today's free refill has not been claimed yet regardless of the current balance.
    pub daily_available: bool,
    pub pack_beans: i64,
    pub pack_usd_cents: i64,
    /// False while the pack is not for sale; the H5 hides the buy button.
    pub pack_available: bool,
}

pub async fn profile(pool: &PgPool, user: Uuid) -> sqlx::Result<Profile> {
    let (w, daily_available) = crate::beans_read_memory::read_after_publication(pool, || {
        crate::beans_read_memory::profile(pool, user)
    })
    .await?;
    let available_balance =
        crate::beans_read_memory::available(pool, user, w.balance, false, None)?;
    let _phase = crate::request_timing::http_phase(crate::request_timing::HttpPhase::Processing)
        .operation("beans_profile_assemble");
    Ok(Profile {
        balance: w.balance,
        available_balance,
        reserved_balance: w.balance - available_balance,
        net_won: w.net_won,
        hands_played: w.hands_played,
        hands_won: w.hands_won,
        buy_in: BUY_IN,
        stake: STAKE_KEY,
        daily_refill_to: DAILY_REFILL_TO,
        daily_available,
        pack_beans: PACK_BEANS,
        pack_usd_cents: pack_usd_cents(),
        pack_available: pack_enabled(),
    })
}

pub async fn balance(pool: &PgPool, user: Uuid) -> sqlx::Result<i64> {
    Ok(ensure(pool, user).await?.balance)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DailyOutcome {
    /// An additional daily grant was credited.
    Granted { granted: i64, balance: i64 },
    /// Already claimed in this Singapore day.
    AlreadyClaimed { balance: i64 },
}

/// One extra [`DAILY_REFILL_TO`] grant per Singapore calendar day. The day counter,
/// the wallet update and the ledger row commit together.
pub async fn claim_daily(pool: &PgPool, user: Uuid) -> sqlx::Result<DailyOutcome> {
    let _daily = crate::beans_read_memory::lock_daily(pool, user).await?;
    if crate::beans_read_memory::daily_claimed(
        pool,
        user,
        crate::lottery::sgt_day(chrono::Utc::now()),
    ) {
        return Ok(DailyOutcome::AlreadyClaimed {
            balance: crate::beans_read_memory::ensure(pool, user)?.balance,
        });
    }
    if crate::realtime_mode::enabled() {
        crate::beans_read_memory::ensure(pool, user)?;
        crate::beans_read_memory::initial_grant_ready(pool, user).await?;
    }
    let beans_mutation = crate::beans_read_memory::daily_mutation(pool, user);
    ensure(pool, user).await?;
    let mut tx = pool.begin().await?;
    let before = wallet::lock(&mut tx, GAME, user)
        .await?
        .ok_or(sqlx::Error::RowNotFound)?;
    let (consumed, day): (Option<chrono::NaiveDate>, chrono::NaiveDate) = sqlx::query_as(
        "WITH claim AS (INSERT INTO daily_usage (scope,subject_key, feature, usage_date, count) \
         VALUES ('feature_user',$1::uuid::text, $2, (now() AT TIME ZONE 'Asia/Singapore')::date, 1) \
         ON CONFLICT (scope, subject_key, feature, usage_date) DO UPDATE \
         SET count = daily_usage.count + 1 WHERE daily_usage.count < 1 RETURNING usage_date) \
         SELECT (SELECT usage_date FROM claim), (now() AT TIME ZONE 'Asia/Singapore')::date",
    )
    .bind(user)
    .bind(DAILY_FEATURE)
    .fetch_one(&mut *tx)
    .await?;
    if consumed.is_none() {
        tx.rollback().await?;
        beans_mutation.publish_daily(day, before);
        return Ok(DailyOutcome::AlreadyClaimed { balance: before });
    }
    let granted = DAILY_REFILL_TO;
    let after = before
        .checked_add(granted)
        .ok_or_else(|| sqlx::Error::Protocol("beans balance overflow".into()))?;
    sqlx::query(
        "UPDATE wallets SET balance = $3, updated_at = now() WHERE user_id = $1 AND game = $2",
    )
    .bind(user)
    .bind(GAME)
    .bind(after)
    .execute(&mut *tx)
    .await?;
    sqlx::query(
        "INSERT INTO wallet_ledger (user_id, game, kind, delta, balance_after, details) \
         VALUES ($1, $2, 'bonus', $3, $4, $5)",
    )
    .bind(user)
    .bind(GAME)
    .bind(granted)
    .bind(after)
    .bind(serde_json::json!({ "reason": "daily_refill" }))
    .execute(&mut *tx)
    .await?;
    tx.commit().await?;
    wallet::project_session_balance(GAME, user, after);
    beans_mutation.publish_daily(day, after);
    Ok(DailyOutcome::Granted {
        granted,
        balance: after,
    })
}

/// Settles one completed public-table hand into every human's beans wallet
/// on the caller's transaction (the hand write). Idempotency comes from the
/// `(game, record_id, user_id)` unique ledger index: the caller only invokes
/// this when the hand row was newly inserted. A delta that would drive a
/// wallet negative is clamped to the balance and logged; the invariant
/// `balance >= stack` makes that unreachable in normal play, and clamping
/// keeps the hand persist (and the table) alive instead of aborting it.
pub async fn settle_hand_on(
    conn: &mut PgConnection,
    hand_id: Uuid,
    players: &[(Uuid, i64)],
    has_bots: bool,
    memory_accepted: bool,
) -> sqlx::Result<()> {
    // Lock wallets in user-id order, like every other multi-wallet settlement,
    // so overlapping settlements can never deadlock on lock order.
    let mut ordered: Vec<&(Uuid, i64)> = players.iter().collect();
    ordered.sort_unstable_by_key(|(user, _)| *user);
    for (user, delta) in ordered {
        if memory_accepted {
            // The startup-ready authority can grant a first wallet before its
            // separate grant writer commits. This idempotent operation keeps
            // that grant and this hand's receipt ordered under the wallet lock.
            ensure_on(conn, *user).await?;
        }
        let Some(balance) = wallet::lock(conn, GAME, *user).await? else {
            tracing::warn!(hand_id = %hand_id, user = %user, "beans_settle_no_wallet");
            continue;
        };
        if memory_accepted && balance.checked_add(*delta).is_none_or(|after| after < 0) {
            return Err(sqlx::Error::Protocol(
                "accepted poker settlement conflicts with durable wallet".into(),
            ));
        }
        let applied = if balance + *delta < 0 {
            tracing::error!(
                hand_id = %hand_id, user = %user, balance, delta,
                "beans_settle_clamped_negative"
            );
            -balance
        } else {
            *delta
        };
        let play = wallet::Play {
            record_id: hand_id,
            delta: applied,
            completes_hand: true,
            won: applied > 0,
            room_code: None,
            revision: None,
            details: Some(serde_json::json!({ "table": "public" })),
            hand_completed: Some(true),
            has_bots: Some(has_bots),
        };
        let after = wallet::settle(conn, GAME, *user, &play).await?;
        wallet::project_session_balance(GAME, *user, after);
    }
    Ok(())
}

/// [`ensure`] on a caller-owned connection: a missing wallet gets the
/// first-use grant and its ledger row, so a purchase that arrives before the
/// first profile read never swallows the grant.
pub async fn ensure_on(conn: &mut PgConnection, user: Uuid) -> sqlx::Result<()> {
    let inserted = sqlx::query(
        "INSERT INTO wallets (user_id, game, balance) VALUES ($1, $2, $3) ON CONFLICT (user_id, game) DO NOTHING",
    )
    .bind(user)
    .bind(GAME)
    .bind(GRANT)
    .execute(&mut *conn)
    .await?
    .rows_affected();
    if inserted == 1 {
        sqlx::query("INSERT INTO wallet_ledger (user_id, game, kind, delta, balance_after) VALUES ($1, $2, 'grant', $3, $3)")
            .bind(user)
            .bind(GAME)
            .bind(GRANT)
            .execute(&mut *conn)
            .await?;
    }
    Ok(())
}

/// Credits a tournament reward on its outbox transaction. The caller holds the
/// user and entry locks and marks the reward granted in the same transaction.
/// Project the returned balance only after that transaction commits.
pub(crate) async fn credit_tournament_on(
    conn: &mut PgConnection,
    user: Uuid,
    edition: Uuid,
    reward: Uuid,
    reward_kind: &str,
    amount: i64,
) -> sqlx::Result<i64> {
    if !(1..=1_000_000).contains(&amount)
        || !matches!(
            reward_kind,
            "champion" | "runner_up" | "third" | "participation"
        )
    {
        return Err(sqlx::Error::Protocol(
            "invalid tournament bean award".into(),
        ));
    }
    ensure_on(conn, user).await?;
    let before = wallet::lock(conn, GAME, user)
        .await?
        .ok_or(sqlx::Error::RowNotFound)?;
    let after = before
        .checked_add(amount)
        .ok_or_else(|| sqlx::Error::Protocol("beans balance overflow".into()))?;
    sqlx::query("UPDATE wallets SET balance=$3,updated_at=now() WHERE user_id=$1 AND game=$2")
        .bind(user)
        .bind(GAME)
        .bind(after)
        .execute(&mut *conn)
        .await?;
    sqlx::query("INSERT INTO wallet_ledger(user_id,game,kind,delta,balance_after,details) VALUES($1,$2,'bonus',$3,$4,$5)")
        .bind(user).bind(GAME).bind(amount).bind(after)
        .bind(serde_json::json!({"reason":format!("tournament_{reward_kind}"),"reward_kind":reward_kind,"edition_id":edition,"tournament_reward_id":reward}))
        .execute(&mut *conn).await?;
    Ok(after)
}

/// Credits a paid pack. Caller holds the purchase row (status flip) in the
/// same transaction.
pub async fn credit_purchase_on(
    conn: &mut PgConnection,
    user: Uuid,
    purchase_id: Uuid,
    beans: i64,
) -> sqlx::Result<i64> {
    ensure_on(&mut *conn, user).await?;
    let before = wallet::lock(conn, GAME, user)
        .await?
        .ok_or(sqlx::Error::RowNotFound)?;
    let after = before + beans;
    sqlx::query("UPDATE wallets SET balance = $3, refills = refills + 1, updated_at = now() WHERE user_id = $1 AND game = $2")
        .bind(user)
        .bind(GAME)
        .bind(after)
        .execute(&mut *conn)
        .await?;
    sqlx::query(
        "INSERT INTO wallet_ledger (user_id, game, kind, delta, balance_after, details) \
         VALUES ($1, $2, 'purchase', $3, $4, $5)",
    )
    .bind(user)
    .bind(GAME)
    .bind(beans)
    .bind(after)
    .bind(serde_json::json!({ "purchase_id": purchase_id }))
    .execute(&mut *conn)
    .await?;
    wallet::project_session_balance(GAME, user, after);
    Ok(after)
}

/// Debits refunded beans, never below zero (beans already lost at the table
/// stay lost; the refund is the operator's decision, not a cash-out). The
/// caller computes `beans` from the refunded amount (partial refunds debit
/// in proportion) and records the provider refund id for idempotency.
pub async fn debit_refund_on(
    pool: &PgPool,
    conn: &mut PgConnection,
    user: Uuid,
    purchase_id: Uuid,
    beans: i64,
) -> sqlx::Result<i64> {
    let before = wallet::lock(conn, GAME, user)
        .await?
        .ok_or(sqlx::Error::RowNotFound)?;
    if !spendable(pool, conn, user).await? {
        return Err(sqlx::Error::Protocol(
            "beans_in_use: retry refund after settlement".into(),
        ));
    }
    let debit = beans.min(before).max(0);
    let after = before - debit;
    sqlx::query(
        "UPDATE wallets SET balance = $3, updated_at = now() WHERE user_id = $1 AND game = $2",
    )
    .bind(user)
    .bind(GAME)
    .bind(after)
    .execute(&mut *conn)
    .await?;
    sqlx::query(
        "INSERT INTO wallet_ledger (user_id, game, kind, delta, balance_after, details) \
         VALUES ($1, $2, 'refund', $3, $4, $5)",
    )
    .bind(user)
    .bind(GAME)
    .bind(-debit)
    .bind(after)
    .bind(serde_json::json!({ "purchase_id": purchase_id, "requested": beans }))
    .execute(&mut *conn)
    .await?;
    wallet::project_session_balance(GAME, user, after);
    Ok(after)
}

#[derive(Debug, Clone, Serialize)]
pub struct LeaderboardRow {
    pub user_id: Uuid,
    pub display_name: String,
    pub net_won: i64,
    pub hands_played: i32,
}

/// Top beans winners (lifetime net). Only QA identities are excluded: this
/// is a player-facing board, not a growth metric, so staff accounts that play
/// rank like everyone else.
pub async fn leaderboard(
    pool: &PgPool,
    limit: i64,
    offset: i64,
) -> sqlx::Result<Vec<LeaderboardRow>> {
    crate::beans_read_memory::leaderboard(pool, limit, offset)
}

/// Unsettled public-hand liabilities, held per process: the runtime owner
/// runs every live table, and admission waits for ownership on handover.
///
/// A player still owes a dealt hand after leaving the table. Accepted poker
/// settlements update the memory wallet immediately, while other games and
/// refunds still debit the durable wallet. Retain this hold until COMMIT so
/// those callers cannot spend a database balance missing an accepted loss.
pub mod holds {
    use std::collections::{HashMap, HashSet};
    use std::sync::atomic::AtomicUsize;
    use std::sync::{Arc, LazyLock, Mutex, MutexGuard, Weak};
    use uuid::Uuid;

    struct OpenHand {
        session: Uuid,
        users: HashSet<Uuid>,
        amounts: HashMap<Uuid, i64>,
        /// Completed; its wallet settlement is queued.
        settling: bool,
        /// The table's unsettled counter: the session actor and every queued
        /// settlement hold it, so a dead handle means nothing can settle.
        alive: Weak<AtomicUsize>,
        /// Shared local OS locks live exactly as long as this hand's hold.
        _local_holds: Vec<std::fs::File>,
    }

    static OPEN: LazyLock<Mutex<HashMap<Uuid, OpenHand>>> = LazyLock::new(Default::default);

    fn open_hands() -> MutexGuard<'static, HashMap<Uuid, OpenHand>> {
        OPEN.lock().unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    /// A hand was dealt to `users`. The table runs one hand at a time, so an
    /// earlier hand of the same table that never reached settlement has ended.
    pub fn open(
        session: Uuid,
        hand: Uuid,
        amounts: impl IntoIterator<Item = (Uuid, i64)>,
        alive: &Arc<AtomicUsize>,
    ) -> std::io::Result<()> {
        let amounts: HashMap<Uuid, i64> = amounts.into_iter().collect();
        let users: HashSet<Uuid> = amounts.keys().copied().collect();
        // Acquire every user's guard before publishing the hand. Partial
        // acquisition drops its guards on error; callers must not deal.
        let local_holds = users
            .iter()
            .map(|user| crate::local_runtime::hold_beans(*user))
            .collect::<std::io::Result<Vec<_>>>()?
            .into_iter()
            .flatten()
            .collect();
        let mut open = open_hands();
        open.retain(|_, h| h.alive.strong_count() > 0 && (h.session != session || h.settling));
        open.insert(
            hand,
            OpenHand {
                session,
                users,
                amounts,
                settling: false,
                alive: Arc::downgrade(alive),
                _local_holds: local_holds,
            },
        );
        Ok(())
    }

    /// The hand completed and its settlement is queued.
    pub fn settling(hand: Uuid) {
        if let Some(open) = open_hands().get_mut(&hand) {
            open.settling = true;
        }
    }

    /// The hand's wallet settlement committed, or it ended without one.
    pub fn resolve(hand: Uuid) {
        open_hands().remove(&hand);
    }

    /// Whether `user` was dealt into a beans hand that has not settled yet.
    pub fn outstanding(user: Uuid) -> bool {
        outstanding_unless(user, |_| false)
    }

    /// Memory poker admission may ignore results already included in its
    /// locked wallet snapshot. Durable-wallet spenders must use `outstanding`.
    /// The predicate must not acquire the wallet lock (wallet -> holds order).
    pub(crate) fn outstanding_unless(user: Uuid, mut accounted: impl FnMut(Uuid) -> bool) -> bool {
        let mut open = open_hands();
        open.retain(|_, h| h.alive.strong_count() > 0);
        open.iter()
            .any(|(hand, h)| h.users.contains(&user) && !accounted(*hand))
    }

    pub fn amounts(user: Uuid) -> HashMap<Uuid, i64> {
        let mut open = open_hands();
        open.retain(|_, h| h.alive.strong_count() > 0);
        let mut amounts = HashMap::<Uuid, i64>::new();
        for hand in open.values() {
            if let Some(amount) = hand.amounts.get(&user) {
                let entry = amounts.entry(hand.session).or_default();
                *entry = (*entry).max(*amount);
            }
        }
        amounts
    }
}
