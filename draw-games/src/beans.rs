//! 金豆 (gold beans): the platform-wide wallet behind every bean game.
//!
//! One bean is priced like a US$1 casino chip (no currency is ever shown).
//! Public quick-join tables, friend and card rooms, casino/casual games, the
//! lottery center and tournaments all settle in the same wallet. Every user
//! starts with [`GRANT`] beans and may claim an extra [`DAILY_REFILL_TO`] per
//! day; registration, referrals, feedback and tournament prizes add more.
//! Beans have no monetary value and cannot be cashed out; the only transfer
//! is a seated player's fee-bearing gift to another human at the same public
//! Hold'em table ([`crate::bean_gift`]).
//!
//! Accounting model: the wallet balance always INCLUDES the stack a player has
//! on a public table. Each completed public-table hand settles its per-player
//! delta into the wallet as one posting under the hand's record id
//! (idempotent per `(record_id, user)`, [`crate::bean_ledger`] rule 3), so the
//! invariant `balance >= stack on table` holds between hands. Sitting down
//! requires `balance >= buy-in`; an explicit re-buy after a bust requires the
//! same (the bust stack is 0). Mid-hand top-ups reserve only wallet headroom
//! above the durable hand-start stack and earlier pending top-ups; they apply
//! at the next hand boundary without changing the current pot or wallet.
//!
//! Reserving, releasing and settling beans for a game goes through
//! [`crate::bean_ledger`], which also documents the ledger row format and
//! the restart argument.

use crate::db_writer::{PgConnection, PgPool};
use serde::Serialize;
use uuid::Uuid;

use crate::bean_ledger::{self, Amount, Leg, Reason, Transfer, Wallet};
use crate::game_catalog::BEANS as GAME;

/// First-use grant; existing balances are preserved.
pub const GRANT: i64 = 200;
/// Keeps payouts and JSON integers bounded even for the largest solo multiplier.
pub const MAX_ALL_IN: i64 = 1_000_000_000_000;
/// Namespace of the server-derived receipt ids of client-requested solo plays.
const SOLO_RECORD_NS: Uuid = Uuid::from_u128(0x8d2f_6c41_0b7e_4a93_9e15_c3a8_57d0_50e1);
/// The shared wallet ledger keys play receipts by `(game, record_id, user)`
/// only, and room/hand/lottery settlements use ids clients can see. A solo
/// play's receipt id is therefore derived from its game, owner and request
/// id and never equals a client-supplied id.
pub fn solo_record_id(game: &str, user: Uuid, request: Uuid) -> Uuid {
    Uuid::new_v5(
        &SOLO_RECORD_NS,
        format!("{game}:{user}:{request}").as_bytes(),
    )
}
/// Extra beans ADDED by the once-per-Singapore-day claim (not a refill-to
/// target).
/// 200 = one public-table buy-in, so a day's claim is enough to play
/// (owner 2026-11-12: 「每天送200…最少代入是200，每天领完就可以玩」).
pub const DAILY_REFILL_TO: i64 = 200;
/// Public beans table shape. One stake keeps the thin player pool together.
pub const STAKE_KEY: &str = "1/2";
pub const SB: u32 = 1;
pub const BB: u32 = 2;
/// Public tables play 1/2/4 (owner 2026-11-12) — a forced straddle of
/// 2 × BB on every hand with three or more dealt seats — plus a 1-bean ante
/// per dealt seat.
pub const STRADDLE_BB: u8 = 2;
pub const ANTE: u32 = 1;
/// Owner 2026-11-11: new public tables also run 血战鱿鱼 (full squid,
/// penalty in BB, default extra tokens) and a timed Bomb Pot (each dealt seat
/// posts this many BB every 5-8 minutes). Same defaults as the friend-room
/// setup form. Running sessions keep the rules they were created with.
pub const SQUID_PENALTY_BB: u8 = 5;
pub const SQUID_EXTRA_TOKENS: u8 = 3;
pub const BOMB_POT_BB: u64 = 5;
pub const BOMB_POT_INTERVAL_MINUTES: [u16; 2] = [5, 8];

/// Rule keys every new public beans session adds to its `session_config`.
/// Bomb Pot needs durable runtime checkpoints (as friend-room create does).
pub fn public_rule_config() -> serde_json::Map<String, serde_json::Value> {
    let mut rules = serde_json::Map::new();
    rules.insert("squid_penalty_bb".into(), SQUID_PENALTY_BB.into());
    rules.insert("squid_mode".into(), "full".into());
    rules.insert("squid_extra_tokens".into(), SQUID_EXTRA_TOKENS.into());
    rules.insert("squid_funding".into(), "stack".into());
    rules.insert("bomb_pot_bb".into(), BOMB_POT_BB.into());
    rules.insert(
        "bomb_pot_interval_minutes".into(),
        serde_json::json!(BOMB_POT_INTERVAL_MINUTES),
    );
    rules
}
/// 100 BB.
pub const BUY_IN: u32 = 200;

/// A departed member must bring back the same stack in this room generation.
/// A NULL stack (legacy departures that recorded none, terminal departures)
/// is no resumable stack: the member re-enters exactly like a new one, with a
/// fresh default buy-in. Malformed or oversized stored stacks stay rejected.
pub(crate) fn reentry_chips(
    value: Option<&serde_json::Value>,
) -> Result<Option<crate::runtime_checkpoint::StandingChips>, &'static str> {
    let Some(value) = value else {
        return Ok(None);
    };
    let saved: crate::runtime_checkpoint::StandingChips =
        serde_json::from_value(value.clone()).map_err(|_| "room_unavailable")?;
    if saved
        .stack
        .checked_add(saved.pending_chips)
        .is_none_or(|total| total > u64::from(u32::MAX))
    {
        return Err("room_unavailable");
    }
    Ok(Some(saved))
}

/// The fewest beans `saved` needs to sit back down when a fresh seat costs
/// `fresh` and the whole wallet, beans reserved by other games included, is
/// `wallet_total`. A settled departure whose whole wallet can no longer cover
/// its stack may return with the whole wallet once that covers a fresh
/// buy-in: it keeps nothing back, so this is no rathole (owner 2026-11-12:
/// leaving a table with 2,484 and then holding 1,358 blocked that table for
/// its 24 hours). Beans held by another game must be released first, so
/// `available` then equals `wallet_total`.
pub(crate) fn reentry_required(
    saved: &crate::runtime_checkpoint::StandingChips,
    fresh: u64,
    wallet_total: i64,
) -> u64 {
    let total = saved.stack + saved.pending_chips;
    if reentry_reducible(saved) && wallet_total < total as i64 {
        total.min(fresh).max(wallet_total.max(0) as u64)
    } else {
        total
    }
}

/// `saved` re-entering with `available` beans: unchanged when they cover it.
/// Otherwise the old stack was cashed out and the seat is funded afresh with
/// every available bean: a new `occupancy` (the re-entry's `joined_at`) and
/// `buyin_total`, which is how standings book a cash-out and new buy-in.
/// `None` when even [`reentry_required`] is not covered.
pub(crate) fn reentry_stack(
    saved: crate::runtime_checkpoint::StandingChips,
    available: i64,
    wallet_total: i64,
    fresh: u64,
    joined_at: chrono::DateTime<chrono::Utc>,
) -> Option<crate::runtime_checkpoint::StandingChips> {
    let available = u64::try_from(available).ok()?;
    if available < reentry_required(&saved, fresh, wallet_total) {
        return None;
    }
    if available >= saved.stack + saved.pending_chips {
        return Some(saved);
    }
    Some(crate::runtime_checkpoint::StandingChips {
        stack: available,
        buyin_total: available,
        pending_chips: 0,
        occupancy: Some(joined_at),
        pending_beans_hand: None,
        season_day: None,
    })
}

/// Only a settled departure without pending chips may come back shorter.
fn reentry_reducible(saved: &crate::runtime_checkpoint::StandingChips) -> bool {
    saved.pending_beans_hand.is_none() && saved.pending_chips == 0
}
pub const SEATS: i16 = 9;
/// New public tables run for 24 hours from the first deal.
pub const PUBLIC_TABLE_DURATION_SECS: i64 = 86_400;
/// Paid pack, priced at the bean anchor (owner 2026-11-12: 「按照一美金=1金豆改」):
/// 14 beans for US$13.99. A `BILLING_BEANS_PACK_USD_CENTS` override must keep
/// the same 1 bean ≈ US$1 ratio, so change both together.
pub const PACK_BEANS: i64 = 14;
pub const PACK_USD_CENTS_DEFAULT: i64 = 1_399;
/// The `daily_usage` feature of the once-per-Singapore-day claim.
pub(crate) const DAILY_FEATURE: &str = "beans_daily_refill";

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
    bean_ledger::ensure(pool, user).await
}

/// Call with the shared wallet locked, before a new debit. Poker admission
/// takes the same lock, so a stack cannot be spent in two games at once.
/// Whether `user` holds a seat in a gold-bean dealer room (百家乐, 押大小).
pub async fn dealer_seated(conn: &mut PgConnection, user: Uuid) -> sqlx::Result<bool> {
    sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM game_room_checkpoint_members m JOIN game_room_checkpoints c ON c.game=m.game AND c.code=m.code WHERE m.game=ANY($2) AND m.beans AND m.user_id=$1)")
        .bind(user).bind(dealer_games()).fetch_one(conn).await
}

/// The registered dealer-room games.
fn dealer_games() -> Vec<&'static str> {
    crate::games::ids(|entry| entry.family == crate::games::Family::Dealer)
}

pub async fn spendable(pool: &PgPool, conn: &mut PgConnection, user: Uuid) -> sqlx::Result<bool> {
    // Memory admission is visible before its SQL membership is persisted. All
    // debit callers hold beans_read_memory::Mutation before BEGIN, excluding
    // another admission until their transaction and publication are complete.
    let directory = crate::live_directory::memory_snapshot(pool)?;
    let seated = directory.memberships_for(user).any(|(room, member)| {
        member.left_at.is_none()
            && room.ended_at.is_none()
            && room
                .session_config
                .as_ref()
                .is_some_and(|c| c["beans"] == true)
    });
    Ok(!seated
        && !dealer_seated(conn, user).await?
        && !holds::outstanding(user)
        && !crate::local_runtime::beans_held(user).map_err(sqlx::Error::Io)?)
}

/// One affordability rule for every SQL bean debit: numeric reservations held
/// by the single-owner runtime and its checkpoint authority.
pub async fn available_on(
    pool: &PgPool,
    conn: &mut PgConnection,
    user: Uuid,
    balance: i64,
    excluding_dealer: Option<(&str, &str)>,
) -> sqlx::Result<i64> {
    if crate::local_runtime::enabled() {
        let rooms: Vec<(String, String)> = sqlx::query_as(
            "SELECT game,code FROM game_room_checkpoint_members WHERE game=ANY($2) AND beans AND user_id=$1",
        )
        .bind(user)
        .bind(dealer_games())
        .fetch_all(&mut *conn)
        .await?;
        if rooms.iter().any(|(game, code)| {
            crate::local_runtime::owned_by_another_backend(crate::release_handoff::rooms::id(
                game, code,
            ))
        }) {
            return Err(sqlx::Error::Protocol(
                "beans read memory unavailable".into(),
            ));
        }
    }
    crate::beans_read_memory::available(pool, user, balance, true, excluding_dealer)
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
    /// Forced straddle amount in beans (0 = none).
    pub straddle: u32,
    /// Per-seat ante in beans (0 = none).
    pub ante: u32,
    /// 血战鱿鱼 penalty per share in beans (0 = no squid).
    pub squid_penalty: u32,
    /// Per-seat Bomb Pot contribution in beans (0 = no Bomb Pot).
    pub bomb_pot: u32,
    pub daily_refill_to: i64,
    pub initial_grant: i64,
    /// True when today's free refill has not been claimed yet regardless of the current balance.
    pub daily_available: bool,
    pub pack_beans: i64,
    pub pack_usd_cents: i64,
    /// False while the pack is not for sale; the H5 hides the buy button.
    pub pack_available: bool,
}

pub async fn profile(pool: &PgPool, user: Uuid) -> sqlx::Result<Profile> {
    let _publication_observation =
        crate::beans_read_memory::observe_profile_publication(pool, user);
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
        // A balance a settlement left negative reserves nothing.
        reserved_balance: (w.balance - available_balance).max(0),
        net_won: w.net_won,
        hands_played: w.hands_played,
        hands_won: w.hands_won,
        buy_in: BUY_IN,
        stake: STAKE_KEY,
        straddle: BB * u32::from(STRADDLE_BB),
        ante: ANTE,
        squid_penalty: BB * u32::from(SQUID_PENALTY_BB),
        bomb_pot: BB * BOMB_POT_BB as u32,
        daily_refill_to: DAILY_REFILL_TO,
        initial_grant: crate::auth::registration_reward::BEANS,
        daily_available,
        pack_beans: PACK_BEANS,
        pack_usd_cents: pack_usd_cents(),
        pack_available: pack_enabled(),
    })
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
    crate::beans_read_memory::ensure(pool, user)?;
    crate::beans_read_memory::initial_grant_ready(pool, user).await?;
    let beans_mutation = crate::beans_read_memory::daily_mutation(pool, user);
    ensure(pool, user).await?;
    let mut tx = pool.begin().await?;
    let before = bean_ledger::lock(&mut tx, GAME, user)
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
    // Keyed by the Singapore day the database just claimed.
    let posted = bean_ledger::credit(
        &mut tx,
        &Transfer::one(Leg::new(
            user,
            Reason::DailyRefill,
            day,
            Amount::Credit(granted),
            serde_json::json!({ "day": day }),
        )),
    )
    .await?;
    let after = posted.of(user)?.balance_after;
    tx.commit().await?;
    if !posted.applied() {
        // The ledger already holds today's claim; only the day counter was missing.
        beans_mutation.publish_daily(day, before);
        return Ok(DailyOutcome::AlreadyClaimed { balance: before });
    }
    bean_ledger::project_session_balance(GAME, user, after);
    beans_mutation.publish_daily(day, after);
    Ok(DailyOutcome::Granted {
        granted,
        balance: after,
    })
}

/// One wallet effect, also used by the accepted-memory overlay and replay.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PokerEffect {
    pub delta: i64,
    pub played: i32,
    pub won: i32,
    pub squid_delta: i64,
}

impl PokerEffect {
    pub fn hand(delta: i64) -> Self {
        Self {
            delta,
            played: 1,
            won: i32::from(delta > 0),
            squid_delta: 0,
        }
    }
}

pub fn hand_effects(hand: &crate::persist::HandRecord) -> sqlx::Result<Vec<(Uuid, PokerEffect)>> {
    let mut effects = std::collections::BTreeMap::new();
    for player in &hand.players {
        if effects
            .insert(
                player.user_id,
                PokerEffect {
                    delta: i64::from(player.delta),
                    played: 1,
                    won: i32::from(player.delta > 0),
                    squid_delta: 0,
                },
            )
            .is_some()
        {
            return Err(sqlx::Error::Protocol("duplicate poker wallet owner".into()));
        }
    }
    if !hand.squid_rounds.is_empty() {
        if hand.squid_rounds.iter().any(|round| {
            Some(round.session_id) != hand.session_id
                || round.total_amount != round.loser_stack_debit
        }) {
            return Err(sqlx::Error::Protocol(
                "invalid bean squid hand settlement".into(),
            ));
        }
        for (user, delta) in crate::squid_ledger::bean_deltas(&hand.squid_rounds)? {
            let effect = effects.entry(user).or_insert(PokerEffect {
                delta: 0,
                played: 0,
                won: 0,
                squid_delta: 0,
            });
            effect.delta = effect
                .delta
                .checked_add(delta)
                .ok_or_else(|| sqlx::Error::Protocol("squid delta overflow".into()))?;
            effect.squid_delta = delta;
        }
    }
    Ok(effects.into_iter().collect())
}

/// Owner 2026-11-11: a game settlement whose loss exceeds the wallet is posted
/// in full, the balance goes negative, play continues and operators are told.
/// Every bean settlement writer reports a negative result here. `wallets` is
/// how many wallets the record left below zero.
pub(crate) fn alert_negative_settlement(record: Uuid, user: Option<Uuid>, wallets: i64) {
    let user = user.map(|u| u.to_string()).unwrap_or_else(|| "-".into());
    tracing::error!(
        alert_error = "Gold-bean settlement left a wallet negative: the loss exceeds the wallet",
        alert_cause = "the stake was not fully backed by the wallet; the result was settled in full and the balance is now below zero / 下注或桌上筹码没有被钱包余额完全覆盖；本局已按全额结算，余额现为负数",
        alert_action = "AI team finds what spent this wallet below its stake; Loong decides whether to correct the negative balance / AI 排查该钱包为何低于下注额，是否修正负余额由 Loong 决定",
        "[bean_settlement_negative_balance] record={record} user={user} wallets={wallets}"
    );
}

/// Settles one completed public-table hand into every human's beans wallet
/// on the caller's transaction (the hand write), as one
/// [`crate::bean_ledger`] posting: every participant or nobody, never
/// clamped. The caller only invokes this when the hand row was newly
/// inserted, so receipts already present under the hand id are a conflict.
pub async fn settle_hand_on(
    conn: &mut PgConnection,
    hand_id: Uuid,
    session: Option<Uuid>,
    players: &[(Uuid, PokerEffect)],
    has_bots: bool,
) -> sqlx::Result<()> {
    use crate::bean_ledger::{Entry, Kind, Posted, Posting};
    let posting = Posting {
        game: "poker",
        kind: Kind::Hand,
        record_id: hand_id,
        room_code: None,
        session,
        has_bots,
        meta: serde_json::json!({ "table": "public" }),
        entries: players
            .iter()
            .map(|(user, effect)| Entry {
                user: *user,
                delta: effect.delta,
                completes_hand: effect.played > 0,
                won: effect.won > 0,
                meta: Some(serde_json::json!({ "squid_delta": effect.squid_delta })),
            })
            .collect(),
    };
    match bean_ledger::settle(conn, &posting).await? {
        Posted::Applied(receipts) => {
            for receipt in receipts {
                bean_ledger::project_session_balance(GAME, receipt.user, receipt.balance_after);
            }
            Ok(())
        }
        Posted::Replayed => Err(bean_ledger::refuse(
            &posting,
            bean_ledger::RECEIPT_CONFLICT,
            "receipts exist for a newly written hand",
        )),
    }
}

/// Seated projections are published under the wallet row lock, inside the
/// writing transaction, so they stay ordered with every later writer. When
/// that transaction does not verifiably commit (an early error, a rollback
/// or an unknown COMMIT outcome) the tentative value must not stay visible.
/// Arm each user right after an in-transaction projection and call
/// [`ProjectionFence::finish`] with the commit result. Dropped armed, it
/// fences each still-seated projection ([`bean_ledger::quarantine_user`]): no
/// reload, because neither room-accepted effects nor post-commit delta
/// publishers are ordered by the wallet row lock. Fenced readers use durable
/// balances until the seat retires; money and the ledger are never touched.
#[must_use]
#[derive(Default)]
pub struct ProjectionFence {
    users: Vec<Uuid>,
}

impl ProjectionFence {
    pub fn arm(&mut self, user: Uuid) {
        self.users.push(user);
    }

    /// A verified commit made the in-transaction projections committed state.
    pub fn finish(mut self, committed: bool) {
        if committed {
            self.users.clear();
        }
    }
}

impl Drop for ProjectionFence {
    fn drop(&mut self) {
        for user in self.users.drain(..) {
            if bean_ledger::held(GAME, user) {
                bean_ledger::quarantine_user(GAME, user);
                tracing::error!(user = %user, "beans_projection_fenced_uncommitted");
            }
        }
    }
}

/// [`ensure`] on a caller-owned connection: a missing wallet gets the
/// first-use grant and its ledger row ([`bean_ledger::grant_on`]), so a
/// purchase that arrives before the first profile read never swallows it.
pub async fn ensure_on(conn: &mut PgConnection, user: Uuid) -> sqlx::Result<()> {
    bean_ledger::grant_on(conn, user).await.map(|_| ())
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
    let reason = match reward_kind {
        "champion" => Reason::TournamentChampion,
        "runner_up" => Reason::TournamentRunnerUp,
        "third" => Reason::TournamentThird,
        "participation" => Reason::TournamentParticipation,
        "final_table" => Reason::TournamentFinalTable,
        _ => {
            return Err(sqlx::Error::Protocol(
                "invalid tournament bean award".into(),
            ))
        }
    };
    if !(1..=1_000_000).contains(&amount) {
        return Err(sqlx::Error::Protocol(
            "invalid tournament bean award".into(),
        ));
    }
    let posted = bean_ledger::credit(
        conn,
        &Transfer::one(Leg::new(
            user,
            reason,
            reward,
            Amount::Credit(amount),
            serde_json::json!({"reward_kind":reward_kind,"edition_id":edition,"tournament_reward_id":reward}),
        )),
    )
    .await?;
    Ok(posted.of(user)?.balance_after)
}

/// Credits a paid pack. Caller holds the purchase row (status flip) in the
/// same transaction and arms a [`ProjectionFence`] for `user` on success.
pub async fn credit_purchase_on(
    conn: &mut PgConnection,
    user: Uuid,
    purchase_id: Uuid,
    beans: i64,
) -> sqlx::Result<i64> {
    let posted = bean_ledger::credit(
        conn,
        &Transfer::one(Leg::new(
            user,
            Reason::Purchase,
            purchase_id,
            Amount::Credit(beans),
            serde_json::json!({ "purchase_id": purchase_id }),
        )),
    )
    .await?;
    let after = posted.of(user)?.balance_after;
    bean_ledger::project_session_balance(GAME, user, after);
    Ok(after)
}

/// Debits refunded beans, never below zero (beans already lost at the table
/// stay lost; the refund is the operator's decision, not a cash-out). The
/// caller computes `beans` from the refunded amount (partial refunds debit
/// in proportion); `refund_id` is the provider's refund id and the ledger
/// idempotency key. The caller arms a [`ProjectionFence`] for `user` on
/// success. Returns the beans actually debited, clamped to the balance; the
/// ledger row records both the requested and the applied amount.
pub async fn debit_refund_on(
    pool: &PgPool,
    conn: &mut PgConnection,
    user: Uuid,
    purchase_id: Uuid,
    refund_id: &str,
    beans: i64,
) -> sqlx::Result<i64> {
    bean_ledger::lock(conn, GAME, user)
        .await?
        .ok_or(sqlx::Error::RowNotFound)?;
    if !spendable(pool, conn, user).await? {
        return Err(sqlx::Error::Protocol(
            "beans_in_use: retry refund after settlement".into(),
        ));
    }
    let posted = bean_ledger::credit(
        conn,
        &Transfer::one(Leg::new(
            user,
            Reason::Refund,
            refund_id,
            Amount::Reclaim(beans),
            serde_json::json!({ "purchase_id": purchase_id, "refund_id": refund_id }),
        )),
    )
    .await?;
    let receipt = posted.of(user)?;
    bean_ledger::project_session_balance(GAME, user, receipt.balance_after);
    Ok(-receipt.delta)
}

#[derive(Debug, Clone, Copy, Default, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum LeaderboardSort {
    #[default]
    Profit,
    Balance,
}

#[derive(Debug, Clone, Serialize)]
pub struct LeaderboardRow {
    pub balance: i64,
    pub user_id: Uuid,
    pub display_name: String,
    pub net_won: i64,
    pub hands_played: i32,
}

/// Fixed size of each lobby gold-bean ranking; there is no pagination.
pub const LEADERBOARD_LIMIT: usize = 20;

/// Top [`LEADERBOARD_LIMIT`] active players by lifetime profit or current
/// total wallet balance, served from the in-memory wallet read model.
pub async fn leaderboard(
    pool: &PgPool,
    sort: LeaderboardSort,
) -> sqlx::Result<Vec<LeaderboardRow>> {
    crate::beans_read_memory::leaderboard(pool, sort).await
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
        /// Dealt users who left and were folded out of this hand: their
        /// result is the chips they had committed, already their `amounts`
        /// entry, so it is no longer unknown (`outstanding_unless`).
        fixed: HashSet<Uuid>,
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
                fixed: HashSet::new(),
                settling: false,
                alive: Arc::downgrade(alive),
                _local_holds: local_holds,
            },
        );
        Ok(())
    }

    /// The live hand of `session` that `user` was dealt into and that has
    /// not started settling.
    pub(crate) fn active_hand(session: Uuid, user: Uuid) -> Option<Uuid> {
        open_hands().iter().find_map(|(id, hand)| {
            (hand.session == session
                && !hand.settling
                && hand.users.contains(&user)
                && hand.alive.strong_count() > 0)
                .then_some(*id)
        })
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
        let mut open = open_hands();
        open.retain(|_, h| h.alive.strong_count() > 0);
        open.values().any(|h| h.users.contains(&user))
    }

    /// Owner 2026-11-12 (「强制退房 就可以立马让他退」): `user` left `hand` and
    /// was folded out of it, so its result is exactly `loss` (the chips they
    /// had committed). The hold keeps reserving that loss until the hand's
    /// settlement commits, but memory admission no longer waits for it.
    pub(crate) fn fix_departed_loss(hand: Uuid, user: Uuid, loss: i64) {
        if let Some(open) = open_hands().get_mut(&hand) {
            if let Some(amount) = open.amounts.get_mut(&user) {
                *amount = (*amount).min(loss.max(0));
                open.fixed.insert(user);
            }
        }
    }

    /// Memory poker admission may ignore results already included in its
    /// locked wallet snapshot, and a folded leaver's result whose loss the
    /// hold already reserves (`fix_departed_loss`). Durable-wallet spenders
    /// must use `outstanding`. The predicate must not acquire the wallet lock
    /// (wallet -> holds order).
    pub(crate) fn outstanding_unless(user: Uuid, mut accounted: impl FnMut(Uuid) -> bool) -> bool {
        let mut open = open_hands();
        open.retain(|_, h| h.alive.strong_count() > 0);
        open.iter().any(|(hand, h)| {
            h.users.contains(&user) && !h.fixed.contains(&user) && !accounted(*hand)
        })
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
