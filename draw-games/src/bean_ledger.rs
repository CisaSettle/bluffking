//! The 金豆 (gold bean) ledger: the one interface every game uses to reserve,
//! release and settle beans, and the one format of a settlement ledger row.
//!
//! [`crate::beans`] keeps the product policy (daily refill, pack,
//! profile); this module also holds the wallet row primitives ([`ensure`],
//! [`lock`]) and the live balance projection of seated wallets
//! ([`projected`]). Money movement of a game goes through this module only.
//!
//! # Model
//!
//! * The balance is `wallets(user, game = 10).balance`. It INCLUDES every
//!   stack the player has on a bean table; sitting down debits nothing.
//! * A **reservation** ([`reserve`]) earmarks part of the balance for a seat.
//!   Spendable beans are always `balance - reservations`
//!   (`beans_read_memory::available` / [`available_on`]); nobody computes `balance - stack`.
//! * A **posting** ([`settle`]) applies one settled unit of play (a hand, a
//!   round, a match) to every participant at once: balance update and ledger
//!   row for all entries in the caller's transaction, or nothing.
//!
//! # Conservation rules (enforced here, not per game)
//!
//! 1. Every balance change has exactly one ledger row in the same
//!    transaction, with `balance_after` read from the updated wallet row.
//!    Schema 0124 enforces the same at COMMIT for every bean writer.
//! 2. A posting never clamps and never skips a participant: every human
//!    entry is posted in full. Between humans what the losers pay is exactly
//!    what the winners receive, but bots have no wallets, so beans a human
//!    wins from a bot are created and beans lost to a bot are destroyed; a
//!    posting is zero-sum only when every participant is human. A missing wallet gets its
//!    idempotent first-use grant first, unless its account is erased or
//!    claimed by erasure (wallet and ledger are gone): that share is dropped. A debit larger than the balance is
//!    posted in full (owner 2026-11-11: 「不够余额是负数就行了 让结算继续
//!    游戏继续，报警给我即可」): the balance goes negative, the settlement and
//!    the game continue, and `[bean_settlement_negative_balance]` alerts the
//!    owner.
//!    `balance >= reserved stack >= possible loss` makes that rare. A
//!    negative balance is zero spendable beans: every admission, top-up,
//!    stake, gift, exchange and entry fee still refuses on available beans.
//! 3. A posting is idempotent on `(record_id, user)`: replaying identical
//!    entries is [`Posted::Replayed`]; anything else found under those keys
//!    is [`RECEIPT_CONFLICT`].
//! 4. A posting that can never commit ([`terminal_reason`]: a receipt
//!    conflict or a malformed posting) is alerted and terminal for its
//!    room: the writer is fenced instead of retrying forever, the hand
//!    stays unposted at the last durable checkpoint, and no wallet moves.
//!
//! # Recovery (restart, crash, disconnect)
//!
//! Reservations are deliberately NOT a second durable table. Their only
//! source of truth is the state that also restores the seat itself:
//! `game_room_checkpoints` (card and match rooms, 押大小) and the live
//! directory's session members with their runtime checkpoint (hold'em).
//! `beans_read_memory::hydrate` rebuilds every reservation from those rows
//! and the balances from `wallets`, in one snapshot. This is sound because:
//!
//! * a settlement commits atomically with the state that carries the
//!   resulting stacks (hand row + wallet deltas + departed chips + runtime
//!   checkpoint for hold'em; checkpoint revision + posting for rooms), so
//!   durable stacks and durable balances are always the same prefix of play;
//! * anything only accepted in memory (a result waiting for its writer, a
//!   top-up, an admission) is simply absent after a crash: the hand is void
//!   at its last checkpoint and its wallet delta was never written;
//! * a credit that is accepted but not yet committed is not spendable
//!   outside the room that produced it (`beans_reservations::reserved`,
//!   `beans_card_rooms::accept_card_snapshot`), and an accepted lottery
//!   prize is not spendable at all until its row commits, so no durable
//!   admission can have been funded by winnings a crash then voids;
//! * a disconnect changes nothing: the seat, its checkpoint and its
//!   reservation stay until the room itself releases them.
//!
//! One narrow residue is detected rather than prevented: a hold'em seat
//! released in memory whose departure write is still queued can fund another
//! admission; a crash in exactly that window restores both seats. Rule 2
//! still moves every bean exactly once, and [`reconcile`] reports such
//! over-reserved wallets at startup.
//!
//! # Ledger row format (`details.v = 2`)
//!
//! Every settlement row written here is `kind = 'play'`, `record_id` set,
//! `hand_completed` and `has_bots` always non-null, and
//!
//! ```json
//! {"v":2, "game":"<catalog slug>", "kind":"hand|round|match|solo|draw",
//!  "unit":"beans", "room":"<code>"|null, "session":"<uuid>"|null,
//!  "meta":{ ...game specific... }}
//! ```
//!
//! Lottery postings are kind `draw` under the lottery game's slug, one per
//! accepted event (`meta.op` = `purchase`, `scratch_batch`, `reveal` or
//! `settle`): cost and ticket prizes. A jackpot or card bonus is credited
//! beside it as a non-play row keyed by the same event id.
//!
//! `room_code` (column and `room`) is the join code of a code-addressed
//! room; session-addressed hold'em tables leave it null and set `session`.
//! `meta` of a solo or dealer-room row is the game's own receipt
//! ([`receipt`] serves it). A bonus a game pays beside its play row (押大小
//! jackpot, 2048 reward) is a [`credit`] with the game's slug. Posted rows
//! are immutable (schema 0129): a later step is a new row. The
//! database accepts no other row format (schema 0132).
//!
//! # Non-play rows ([`transfer`], [`credit`], [`grant_on`])
//!
//! Everything that is not a settled unit of play (first-use grant, daily
//! claim, welcome and referral rewards, staff credits, redeem codes, the
//! exchange, gifts, purchases and refunds, tournament fees and prizes,
//! jackpots) is one [`Transfer`] of one or more [`Leg`]s. Each row is
//! `kind = grant|bonus|purchase|refund` (fixed by its [`Reason`]),
//! `record_id` NULL, `transfer_key = "<reason>:<natural id>"` (unique per
//! account, schema 0131) and
//!
//! ```json
//! {"v":2, "kind":"transfer", "reason":"<Reason code>", "unit":"beans",
//!  "game":"<catalog slug>",            // only for a game's bonus rows
//!  "meta":{ ...reason specific... }}
//! ```
//!
//! A spend ([`Amount::Spend`]) is refused unless the AVAILABLE beans cover
//! it; a credit always applies, also to a negative wallet.

use crate::db_writer::{PgConnection, PgPool};
use crate::game_catalog::BEANS as GAME;
use serde::Serialize;
use serde_json::{json, Value};
use sqlx::Row;
use std::collections::HashMap;
use std::sync::{Mutex, OnceLock};
use std::time::Duration;
use uuid::Uuid;

/// Version of the settlement `details` object this module writes.
pub const DETAILS_VERSION: i64 = 2;
/// A receipt key of the posting holds something else.
pub(crate) const RECEIPT_CONFLICT: &str = "play_receipt_id_conflict";
/// The posting itself is malformed (duplicate participant, overflow).
pub(crate) const INVALID: &str = "bean_posting_invalid";
const MAX_ENTRIES: usize = 64;

/// What one posting settles.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Kind {
    Hand,
    Round,
    Match,
    Solo,
    Draw,
}

impl Kind {
    pub const fn as_str(self) -> &'static str {
        match self {
            Kind::Hand => "hand",
            Kind::Round => "round",
            Kind::Match => "match",
            Kind::Solo => "solo",
            Kind::Draw => "draw",
        }
    }
}

/// One participant's share of a posting.
#[derive(Debug, Clone, PartialEq)]
pub struct Entry {
    pub user: Uuid,
    pub delta: i64,
    /// Counts one hand played (and one won when `won`) on the wallet.
    pub completes_hand: bool,
    pub won: bool,
    /// Per-participant keys merged over the posting's `meta`.
    pub meta: Option<Value>,
}

impl Entry {
    /// A whole hand or match: played, won when the delta is positive.
    pub fn played(user: Uuid, delta: i64) -> Self {
        Entry {
            user,
            delta,
            completes_hand: true,
            won: delta > 0,
            meta: None,
        }
    }
}

/// One settled unit of play for all of its human participants.
#[derive(Debug, Clone)]
pub struct Posting<'a> {
    /// Catalog slug (`poker`, `squid`, `zjh`, `niuniu`, ...).
    pub game: &'a str,
    pub kind: Kind,
    /// Receipt id, unique per settled unit; with the user it is the
    /// idempotency key.
    pub record_id: Uuid,
    pub room_code: Option<&'a str>,
    pub session: Option<Uuid>,
    pub has_bots: bool,
    /// Game-specific facts shared by every entry.
    pub meta: Value,
    pub entries: Vec<Entry>,
}

/// One written ledger row.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Receipt {
    pub user: Uuid,
    pub delta: i64,
    pub balance_after: i64,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Posted {
    /// Wallets and ledger rows were written in the caller's transaction.
    Applied(Vec<Receipt>),
    /// Every entry was already posted with the same delta; nothing changed.
    Replayed,
}

fn protocol(reason: &'static str) -> sqlx::Error {
    sqlx::Error::Protocol(reason.into())
}

/// The reason a settlement error can never succeed on retry. Callers stop
/// their writer (fence the room) instead of retrying it.
pub(crate) fn terminal_reason(error: &sqlx::Error) -> Option<&'static str> {
    match error {
        sqlx::Error::Protocol(m) if m == RECEIPT_CONFLICT => Some(RECEIPT_CONFLICT),
        sqlx::Error::Protocol(m) if m == INVALID => Some(INVALID),
        _ => None,
    }
}

/// The ordered room writer's error for a terminal settlement failure.
pub(crate) fn fenced(error: &sqlx::Error) -> Option<String> {
    terminal_reason(error).map(crate::write_behind::fenced)
}

/// The settlement `details` object of one entry.
pub fn details(posting: &Posting<'_>, entry: &Entry) -> Value {
    let mut meta = match &posting.meta {
        Value::Object(map) => map.clone(),
        _ => serde_json::Map::new(),
    };
    if let Some(Value::Object(own)) = &entry.meta {
        meta.extend(own.clone());
    }
    json!({
        "v": DETAILS_VERSION,
        "game": posting.game,
        "kind": posting.kind.as_str(),
        "unit": "beans",
        "room": posting.room_code,
        "session": posting.session,
        "meta": meta,
    })
}

/// The catalog slug a bean ledger row belongs to.
pub fn game_slug(details: &Value) -> Option<&str> {
    details.get("game").and_then(Value::as_str)
}

/// Game-specific facts of a row.
pub fn meta(details: &Value) -> &Value {
    &details["meta"]
}

/// The game's own receipt of a row, as its API serves it: `meta` plus the
/// `game` and `unit` of the row.
pub fn receipt(details: &Value) -> Value {
    let mut receipt = match &details["meta"] {
        Value::Object(map) => map.clone(),
        _ => serde_json::Map::new(),
    };
    receipt.insert("game".into(), details["game"].clone());
    receipt.insert("unit".into(), details["unit"].clone());
    Value::Object(receipt)
}

/// [`receipt`] in SQL, over a ledger row's `details` column.
pub const RECEIPT_SQL: &str =
    "(COALESCE(details->'meta','{}'::jsonb) || jsonb_build_object('game',details->'game','unit',details->'unit'))";

/// One shared-wallet game's own plays for its in-app ledger, newest first.
/// Every row has `id, kind, record_id, room_code, delta, balance_after,
/// details` ([`RECEIPT_SQL`]) and `created_at`; `extra` adds the further
/// `wallet_ledger` columns that game's client reads.
pub async fn game_plays(
    pool: &PgPool,
    user: Uuid,
    game: &str,
    extra: &[&str],
    page: crate::pagination::Query,
) -> sqlx::Result<Vec<Value>> {
    let extra: String = extra.iter().map(|column| format!(",{column}")).collect();
    sqlx::query_scalar(&format!(
        "SELECT to_jsonb(l) FROM (SELECT id,kind,record_id,room_code,delta,balance_after,\
         {RECEIPT_SQL} AS details,created_at{extra} FROM wallet_ledger \
         WHERE user_id=$1 AND game=$2 AND kind='play' AND details->>'game'=$3 \
         ORDER BY id DESC LIMIT $4 OFFSET $5) l"
    ))
    .bind(user)
    .bind(GAME)
    .bind(game)
    .bind(page.limit())
    .bind(page.offset())
    .fetch_all(pool)
    .await
}

/// Posts a one-player unit (a solo play, or one step of one) whose replay the
/// caller has already ruled out under the wallet lock, and returns the
/// balance after it.
pub async fn settle_one(conn: &mut PgConnection, posting: &Posting<'_>) -> sqlx::Result<i64> {
    match settle(conn, posting).await? {
        Posted::Applied(receipts) if receipts.len() == 1 && posting.entries.len() == 1 => {
            Ok(receipts[0].balance_after)
        }
        Posted::Applied(_) => Err(sqlx::Error::RowNotFound),
        Posted::Replayed => Err(refuse(
            posting,
            RECEIPT_CONFLICT,
            "receipt exists for a new solo play",
        )),
    }
}

/// Alerts and returns the terminal error of a posting that must not commit.
pub(crate) fn refuse(posting: &Posting<'_>, reason: &'static str, detail: &str) -> sqlx::Error {
    alert_refused(posting, reason, detail);
    protocol(reason)
}

fn alert_refused(posting: &Posting<'_>, reason: &'static str, detail: &str) {
    tracing::error!(
        alert_error = "Gold-bean settlement refused: it would break the ledger",
        alert_cause = "a posting would reuse a receipt id or is malformed; nothing was written and the room writer stops instead of paying twice or retrying forever / 结算会占用已有流水 id 或数据不合法，已整笔拒绝并停止该房间写入，不会重复入账也不会无限重试",
        alert_action = "AI team inspects the wallet, its reservations and the room checkpoint, then settles or voids the hand by hand; Loong action: none / AI 核查钱包、预留与房间检查点后手工结算或作废本手，Loong 无需操作",
        "[bean_posting_refused] reason={reason} game={} record={} room={} {detail}",
        posting.game,
        posting.record_id,
        posting.room_code.unwrap_or("-"),
    );
}

/// Posts one settled unit to every participant's wallet on the caller's
/// transaction. All-or-nothing: on any error nothing of this posting may
/// commit, so the caller must roll its transaction back. Wallet rows are
/// locked in user-id order.
pub async fn settle(conn: &mut PgConnection, posting: &Posting<'_>) -> sqlx::Result<Posted> {
    if posting.entries.is_empty() {
        return Ok(Posted::Applied(Vec::new()));
    }
    let mut entries: Vec<&Entry> = posting.entries.iter().collect();
    entries.sort_unstable_by_key(|entry| entry.user);
    if entries.len() > MAX_ENTRIES || entries.windows(2).any(|pair| pair[0].user == pair[1].user) {
        alert_refused(posting, INVALID, "duplicate or oversized participant list");
        return Err(protocol(INVALID));
    }
    let mut users: Vec<Uuid> = entries.iter().map(|entry| entry.user).collect();
    let mut balances = lock_balances(conn, &users).await?;
    if balances.len() != users.len() {
        // A first wallet may still be queued on the grant writer. The grant
        // is idempotent on the wallet key and ordered by the row lock.
        for user in users.iter().filter(|user| !balances.contains_key(user)) {
            grant_on(conn, *user).await?;
        }
        balances = lock_balances(conn, &users).await?;
        // [`grant_on`] grants nothing to an erased or erasure-claimed
        // account: its share is dropped instead of re-creating a wallet.
        for user in users.iter().filter(|user| !balances.contains_key(user)) {
            tracing::warn!(
                game = posting.game, %user, record = %posting.record_id,
                "bean_settlement_account_gone"
            );
        }
        entries.retain(|entry| balances.contains_key(&entry.user));
        if entries.is_empty() {
            return Ok(Posted::Applied(Vec::new()));
        }
        users = entries.iter().map(|entry| entry.user).collect();
    }
    let existing: Vec<(Uuid, i64, Option<Value>)> = sqlx::query_as(
        "SELECT user_id,delta,details FROM wallet_ledger \
         WHERE game=$1 AND kind='play' AND user_id=ANY($2) AND record_id=$3",
    )
    .bind(GAME)
    .bind(&users)
    .bind(posting.record_id)
    .fetch_all(&mut *conn)
    .await?;
    // Per user: the delta of this posting's own receipt, when it exists.
    let mut posted: HashMap<Uuid, i64> = HashMap::new();
    for (user, delta, row) in existing {
        if row
            .as_ref()
            .and_then(game_slug)
            .is_some_and(|game| game == posting.game)
        {
            posted.insert(user, delta);
        } else {
            alert_refused(
                posting,
                RECEIPT_CONFLICT,
                "receipt id holds another game's receipt",
            );
            return Err(protocol(RECEIPT_CONFLICT));
        }
    }
    if !posted.is_empty() {
        if posted.len() == entries.len()
            && entries
                .iter()
                .all(|entry| posted.get(&entry.user) == Some(&entry.delta))
        {
            return Ok(Posted::Replayed);
        }
        alert_refused(posting, RECEIPT_CONFLICT, "partial or different replay");
        return Err(protocol(RECEIPT_CONFLICT));
    }
    for entry in &entries {
        let balance = balances[&entry.user];
        let Some(after) = balance.checked_add(entry.delta) else {
            alert_refused(posting, INVALID, "balance overflow");
            return Err(protocol(INVALID));
        };
        if after < 0 && entry.delta < 0 {
            // Posted in full; never clamped, never refused.
            tracing::warn!(
                game = posting.game, user = %entry.user, record = %posting.record_id,
                balance, delta = entry.delta, after, "bean_settlement_unbacked"
            );
            crate::beans::alert_negative_settlement(posting.record_id, Some(entry.user), 1);
        }
    }
    let deltas: Vec<i64> = entries.iter().map(|entry| entry.delta).collect();
    let played: Vec<i32> = entries
        .iter()
        .map(|entry| i32::from(entry.completes_hand))
        .collect();
    let won: Vec<i32> = entries
        .iter()
        .map(|entry| i32::from(entry.completes_hand && entry.won))
        .collect();
    let rows: Vec<String> = entries
        .iter()
        .map(|entry| details(posting, entry).to_string())
        .collect();
    // The rows are locked above, so the ledger reads each exact new balance.
    let written: Vec<(Uuid, i64, i64)> = sqlx::query_as(
        "/* bk:bean_ledger_post */ \
         WITH e AS (SELECT * FROM unnest($1::uuid[],$2::bigint[],$3::int[],$4::int[],$5::text[]) \
                    AS e(user_id,delta,played,won,details)), \
         updated AS (UPDATE wallets w SET balance=w.balance+e.delta,net_won=w.net_won+e.delta, \
                hands_played=w.hands_played+e.played,hands_won=w.hands_won+e.won,updated_at=now() \
              FROM e WHERE w.user_id=e.user_id AND w.game=$6 \
              RETURNING w.user_id,w.balance,e.delta,e.played,e.details) \
         INSERT INTO wallet_ledger(user_id,game,kind,record_id,room_code,delta,balance_after,details,hand_completed,has_bots) \
         SELECT user_id,$6,'play',$7,$8,delta,balance,details::jsonb,played=1,$9 FROM updated \
         RETURNING user_id,delta,balance_after",
    )
    .bind(&users)
    .bind(&deltas)
    .bind(&played)
    .bind(&won)
    .bind(&rows)
    .bind(GAME)
    .bind(posting.record_id)
    .bind(posting.room_code)
    .bind(posting.has_bots)
    .fetch_all(conn.writer())
    .await?;
    if written.len() != entries.len() {
        // Unreachable with the rows locked; never commit a partial posting.
        alert_refused(posting, INVALID, "partial wallet update");
        return Err(protocol(INVALID));
    }
    let mut receipts: Vec<Receipt> = written
        .into_iter()
        .map(|(user, delta, balance_after)| Receipt {
            user,
            delta,
            balance_after,
        })
        .collect();
    receipts.sort_unstable_by_key(|receipt| receipt.user);
    Ok(Posted::Applied(receipts))
}

/// Locks the participants' bean wallets in user-id order.
async fn lock_balances(
    conn: &mut PgConnection,
    users: &[Uuid],
) -> sqlx::Result<HashMap<Uuid, i64>> {
    let rows: Vec<(Uuid, i64)> = sqlx::query_as(
        "SELECT user_id,balance FROM wallets WHERE game=$1 AND user_id=ANY($2) \
         ORDER BY user_id FOR UPDATE",
    )
    .bind(GAME)
    .bind(users)
    .fetch_all(conn.writer())
    .await?;
    Ok(rows.into_iter().collect())
}

// ---------------------------------------------------------------------------
// Non-play postings: grants, rewards, purchases, refunds, spending, gifts.
// ---------------------------------------------------------------------------

/// `wallet_ledger.kind` of a non-play bean row. `refill` is not listed: no
/// bean writer or row uses it (it belongs to the retired point wallets).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RowKind {
    Grant,
    Bonus,
    Purchase,
    Refund,
}

impl RowKind {
    pub const fn as_str(self) -> &'static str {
        match self {
            RowKind::Grant => "grant",
            RowKind::Bonus => "bonus",
            RowKind::Purchase => "purchase",
            RowKind::Refund => "refund",
        }
    }
}

macro_rules! reasons {
    ($($(#[$doc:meta])* $variant:ident => ($code:literal, $kind:ident)),+ $(,)?) => {
        /// Why a non-play row exists: `details.reason`, and the first half of
        /// its idempotency key. Each reason has exactly one ledger kind.
        #[derive(Debug, Clone, Copy, PartialEq, Eq)]
        pub enum Reason { $($(#[$doc])* $variant),+ }

        impl Reason {
            /// Every reason code of a non-play row.
            pub const ALL: &'static [Reason] = &[$(Reason::$variant),+];
            pub const fn as_str(self) -> &'static str {
                match self { $(Reason::$variant => $code),+ }
            }
            pub const fn kind(self) -> RowKind {
                match self { $(Reason::$variant => RowKind::$kind),+ }
            }
        }
    };
}

reasons! {
    /// The first-use allowance of a new wallet ([`grant_on`] only).
    FirstUseGrant => ("first_use_grant", Grant),
    DailyRefill => ("daily_refill", Bonus),
    RegistrationWelcome => ("registration_welcome", Bonus),
    RegistrationReferral => ("registration_referral", Bonus),
    AdminGrant => ("admin_grant", Bonus),
    FeedbackReward => ("feedback_reward", Bonus),
    /// The automatic thank-you for submitting feedback, once per Singapore day.
    FeedbackSubmitted => ("feedback_submitted", Bonus),
    RedeemCode => ("redeem_code", Bonus),
    BeansExchange => ("beans_exchange", Bonus),
    BeansGiftSent => ("beans_gift_sent", Bonus),
    BeansGiftReceived => ("beans_gift_received", Bonus),
    Purchase => ("purchase", Purchase),
    Refund => ("refund", Refund),
    TournamentEntryFee => ("tournament_entry_fee", Bonus),
    TournamentEntryRefund => ("tournament_entry_refund", Bonus),
    TournamentChampion => ("tournament_champion", Bonus),
    TournamentRunnerUp => ("tournament_runner_up", Bonus),
    TournamentThird => ("tournament_third", Bonus),
    TournamentFinalTable => ("tournament_final_table", Bonus),
    TournamentParticipation => ("tournament_participation", Bonus),
    /// Bonus credits a game pays beside its play row.
    SicboJackpot => ("sicbo_jackpot", Bonus),
    LotteryJackpot => ("lottery_jackpot", Bonus),
    ScratchReward => ("scratch_reward", Bonus),
    CasualReward => ("casual_reward", Bonus),
    /// The single row a wallet starts from after the ledger was rebuilt
    /// (written by a schema file, never by the server). `meta.initial_grant`
    /// is the first-use allowance already contained in it.
    OpeningBalance => ("opening_balance", Grant),
}

impl Reason {
    /// The reason of a non-play ledger row.
    pub fn of(details: &Value) -> Option<Reason> {
        let code = details.get("reason").and_then(Value::as_str)?;
        Reason::ALL.iter().copied().find(|r| r.as_str() == code)
    }
}

/// How one leg moves beans.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Amount {
    /// Always applies, also to a negative wallet.
    Credit(i64),
    /// Refused as a whole transfer ([`TransferError::Insufficient`]) unless
    /// the account's AVAILABLE beans ([`available_on`]) cover it.
    Spend(i64),
    /// An operator claw-back (payment refund): debits what the balance still
    /// holds, never below zero; `meta.requested` / `meta.applied` record both.
    Reclaim(i64),
}

/// One account's side of a [`Transfer`].
#[derive(Debug, Clone)]
pub struct Leg<'a> {
    pub user: Uuid,
    pub reason: Reason,
    /// The movement's natural id within its reason (gift id, request id,
    /// Singapore day, ...). `"<reason>:<key>"` is unique per account in the
    /// database (`wallet_ledger.transfer_key`).
    pub key: String,
    pub amount: Amount,
    /// Catalog slug when the row belongs to a game (jackpots, rewards).
    pub game: Option<&'a str>,
    /// Reason-specific facts, stored as `details.meta`.
    pub meta: Value,
}

impl Leg<'static> {
    pub fn new(
        user: Uuid,
        reason: Reason,
        key: impl ToString,
        amount: Amount,
        meta: Value,
    ) -> Self {
        Leg {
            user,
            reason,
            key: key.to_string(),
            amount,
            game: None,
            meta,
        }
    }
}

impl Leg<'_> {
    /// The row's `wallet_ledger.transfer_key`.
    pub fn transfer_key(&self) -> String {
        transfer_key(self.reason, &self.key)
    }

    fn requested(&self) -> i64 {
        match self.amount {
            Amount::Credit(n) | Amount::Spend(n) | Amount::Reclaim(n) => n,
        }
    }
}

/// `wallet_ledger.transfer_key` of a movement.
pub fn transfer_key(reason: Reason, key: &str) -> String {
    format!("{}:{key}", reason.as_str())
}

/// One non-play movement. All legs commit together or not at all; a gift is
/// one transfer with the sender's and the recipient's leg.
#[derive(Debug, Clone)]
pub struct Transfer<'a> {
    pub legs: Vec<Leg<'a>>,
}

impl<'a> Transfer<'a> {
    pub fn one(leg: Leg<'a>) -> Self {
        Transfer { legs: vec![leg] }
    }
}

/// One written (or, on replay, previously written) non-play row.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TransferReceipt {
    pub user: Uuid,
    pub ledger_id: i64,
    /// Signed beans actually moved.
    pub delta: i64,
    pub balance_after: i64,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Transferred {
    /// Wallets and ledger rows were written in the caller's transaction.
    Applied(Vec<TransferReceipt>),
    /// Every leg was already posted under its key with the same amount;
    /// nothing changed. The receipts are the stored rows.
    Replayed(Vec<TransferReceipt>),
}

impl Transferred {
    pub fn receipts(&self) -> &[TransferReceipt] {
        match self {
            Transferred::Applied(rows) | Transferred::Replayed(rows) => rows,
        }
    }

    /// The receipt of `user`'s leg.
    pub fn of(&self, user: Uuid) -> sqlx::Result<TransferReceipt> {
        self.receipts()
            .iter()
            .copied()
            .find(|receipt| receipt.user == user)
            .ok_or(sqlx::Error::RowNotFound)
    }

    pub fn applied(&self) -> bool {
        matches!(self, Transferred::Applied(_))
    }
}

#[derive(Debug)]
pub enum TransferError {
    /// A [`Amount::Spend`] leg exceeds the account's available beans.
    /// Nothing was written; the caller's transaction is still usable.
    Insufficient {
        user: Uuid,
        available: i64,
        needed: i64,
    },
    /// A key of this transfer already holds a different movement, or only
    /// part of the transfer. Alerted; nothing was written.
    KeyConflict,
    /// Malformed (no legs, a duplicate account, a negative amount, an
    /// unusable key, overflow). Alerted; nothing was written.
    Invalid,
    Db(sqlx::Error),
}

/// `details.kind` of every non-play row.
pub const TRANSFER: &str = "transfer";
pub(crate) const TRANSFER_KEY_CONFLICT: &str = "bean_transfer_key_conflict";
pub(crate) const TRANSFER_INVALID: &str = "bean_transfer_invalid";
pub(crate) const INSUFFICIENT: &str = "insufficient_beans";

impl From<sqlx::Error> for TransferError {
    fn from(error: sqlx::Error) -> Self {
        TransferError::Db(error)
    }
}

impl From<TransferError> for sqlx::Error {
    /// For callers whose movement can only fail exceptionally (credits).
    fn from(error: TransferError) -> Self {
        match error {
            TransferError::Db(error) => error,
            TransferError::Insufficient { .. } => protocol(INSUFFICIENT),
            TransferError::KeyConflict => protocol(TRANSFER_KEY_CONFLICT),
            TransferError::Invalid => protocol(TRANSFER_INVALID),
        }
    }
}

/// The `details` object of one non-play row (same envelope as [`details`]).
pub fn transfer_details(leg: &Leg<'_>, applied: i64) -> Value {
    let mut meta = match &leg.meta {
        Value::Object(map) => map.clone(),
        _ => serde_json::Map::new(),
    };
    if let Amount::Reclaim(requested) = leg.amount {
        meta.insert("requested".into(), json!(requested));
        meta.insert("applied".into(), json!(applied));
    }
    let mut details = json!({
        "v": DETAILS_VERSION,
        "kind": TRANSFER,
        "reason": leg.reason.as_str(),
        "unit": "beans",
        "meta": meta,
    });
    if let Some(game) = leg.game {
        details["game"] = json!(game);
    }
    details
}

fn alert_transfer_refused(transfer: &Transfer<'_>, reason: &'static str, detail: &str) {
    let first = transfer.legs.first();
    tracing::error!(
        alert_error = "Gold-bean transfer refused: it would break the ledger",
        alert_cause = "a non-play bean movement would reuse an idempotency key for a different amount or is malformed; nothing was written / 非对局金豆变动会以不同金额复用幂等键或数据不合法，已整笔拒绝，未写入",
        alert_action = "AI team compares the request with the ledger row holding that key and fixes the caller; Loong action: none / AI 对照占用该键的流水排查调用方，Loong 无需操作",
        "[bean_transfer_refused] reason={reason} legs={} first_reason={} first_user={} {detail}",
        transfer.legs.len(),
        first.map_or("-", |leg| leg.reason.as_str()),
        first.map(|leg| leg.user.to_string()).unwrap_or_else(|| "-".into()),
    );
}

/// Creates a missing bean wallet with its first-use allowance and the ledger
/// row of it, in the caller's transaction. The ONLY implementation of the
/// first-use grant. Idempotent on the wallet key and on
/// `first_use_grant:<user>`. An account that no longer exists or that account
/// erasure has claimed (`deleted_at = 'epoch'`) is never granted: the
/// account row is share-locked for the grant, so erasure, which takes it
/// `FOR UPDATE`, either sees the new wallet or runs first. An existing
/// wallet costs one index probe and takes no lock. Returns whether a wallet
/// was created.
pub async fn grant_on(conn: &mut PgConnection, user: Uuid) -> sqlx::Result<bool> {
    let details = json!({
        "v": DETAILS_VERSION,
        "kind": TRANSFER,
        "reason": Reason::FirstUseGrant.as_str(),
        "unit": "beans",
        "meta": {},
    });
    let created = sqlx::query(
        "/* bk:bean_ledger_grant */ \
         WITH alive AS (SELECT u.id FROM users u \
                WHERE u.id=$1 AND u.deleted_at IS DISTINCT FROM 'epoch'::timestamptz \
                  AND NOT EXISTS(SELECT 1 FROM wallets w WHERE w.user_id=$1 AND w.game=$2) \
                FOR KEY SHARE OF u), \
         created AS (INSERT INTO wallets(user_id,game,balance) SELECT id,$2,$3 FROM alive \
                ON CONFLICT(user_id,game) DO NOTHING RETURNING user_id) \
         INSERT INTO wallet_ledger(user_id,game,kind,delta,balance_after,details,transfer_key) \
         SELECT user_id,$2,'grant',$3,$3,$4,$5 FROM created",
    )
    .bind(user)
    .bind(GAME)
    .bind(crate::beans::GRANT)
    .bind(details)
    .bind(transfer_key(Reason::FirstUseGrant, &user.to_string()))
    .execute(conn.writer())
    .await?
    .rows_affected();
    Ok(created == 1)
}

/// Posts one non-play movement on the caller's transaction: for every leg the
/// balance update and its ledger row, or nothing. The second entry point of
/// the bean ledger beside [`settle`]; every non-game writer uses it.
///
/// * Wallet rows are locked in user-id order; a missing wallet gets its
///   first-use grant first ([`grant_on`]).
/// * Idempotent on each leg's `transfer_key`: the same legs again are
///   [`Transferred::Replayed`] with the stored receipts; anything else under
///   a key is [`TransferError::KeyConflict`]. The unique index
///   `wallet_ledger_transfer_uniq` enforces the same in the database.
/// * [`Amount::Spend`] is checked against [`available_on`], so the caller
///   must hold `beans_read_memory::mutation` for the spender since before
///   BEGIN, like every SQL bean debit. Credits always apply.
/// * `net_won` and the hand counters never change; a purchase counts one
///   refill.
///
/// On `Insufficient` nothing was written and the transaction is intact. On
/// any other error the caller must roll back. Projections (session balance,
/// read memory) stay with the caller, after COMMIT.
pub async fn transfer(
    pool: &PgPool,
    conn: &mut PgConnection,
    transfer: &Transfer<'_>,
) -> Result<Transferred, TransferError> {
    post(Some(pool), conn, transfer).await
}

/// [`transfer`] for movements without a [`Amount::Spend`] leg (credits and
/// claw-backs), which need no availability check and therefore no pool.
pub async fn credit(
    conn: &mut PgConnection,
    transfer: &Transfer<'_>,
) -> Result<Transferred, TransferError> {
    post(None, conn, transfer).await
}

async fn post(
    pool: Option<&PgPool>,
    conn: &mut PgConnection,
    transfer: &Transfer<'_>,
) -> Result<Transferred, TransferError> {
    let mut legs: Vec<&Leg<'_>> = transfer.legs.iter().collect();
    legs.sort_unstable_by_key(|leg| leg.user);
    let malformed = legs.is_empty()
        || legs.len() > MAX_ENTRIES
        || legs.windows(2).any(|pair| pair[0].user == pair[1].user)
        || legs.iter().any(|leg| {
            leg.requested() < 0
                || leg.key.is_empty()
                || leg.key.len() > 120
                || matches!(leg.reason, Reason::FirstUseGrant | Reason::OpeningBalance)
                || (pool.is_none() && matches!(leg.amount, Amount::Spend(_)))
        });
    if malformed {
        alert_transfer_refused(transfer, TRANSFER_INVALID, "malformed legs");
        return Err(TransferError::Invalid);
    }
    let users: Vec<Uuid> = legs.iter().map(|leg| leg.user).collect();
    let mut balances = lock_balances(conn, &users).await?;
    if balances.len() != users.len() {
        for user in users.iter().filter(|user| !balances.contains_key(user)) {
            grant_on(conn, *user).await?;
        }
        balances = lock_balances(conn, &users).await?;
        if balances.len() != users.len() {
            // Unknown, erased or erasure-claimed account.
            return Err(TransferError::Db(sqlx::Error::RowNotFound));
        }
    }
    let keys: Vec<String> = legs.iter().map(|leg| leg.transfer_key()).collect();
    let existing: Vec<(Uuid, String, i64, i64, i64, Option<Value>)> = sqlx::query_as(
        "SELECT user_id,transfer_key,id,delta,balance_after,details FROM wallet_ledger \
         WHERE game=$1 AND user_id=ANY($2) AND transfer_key=ANY($3)",
    )
    .bind(GAME)
    .bind(&users)
    .bind(&keys)
    .fetch_all(&mut *conn)
    .await?;
    let mut replayed = Vec::new();
    for (leg, key) in legs.iter().zip(&keys) {
        let Some((_, _, id, delta, balance_after, row)) = existing
            .iter()
            .find(|(user, stored, ..)| *user == leg.user && stored == key)
        else {
            continue;
        };
        let same = match leg.amount {
            Amount::Credit(n) => *delta == n,
            Amount::Spend(n) => *delta == -n,
            Amount::Reclaim(n) => row
                .as_ref()
                .map(meta)
                .is_some_and(|m| m["requested"] == json!(n)),
        };
        if !same {
            alert_transfer_refused(transfer, TRANSFER_KEY_CONFLICT, "key holds another amount");
            return Err(TransferError::KeyConflict);
        }
        replayed.push(TransferReceipt {
            user: leg.user,
            ledger_id: *id,
            delta: *delta,
            balance_after: *balance_after,
        });
    }
    if !replayed.is_empty() {
        if replayed.len() == legs.len() {
            return Ok(Transferred::Replayed(replayed));
        }
        alert_transfer_refused(transfer, TRANSFER_KEY_CONFLICT, "partial replay");
        return Err(TransferError::KeyConflict);
    }
    let mut deltas = Vec::with_capacity(legs.len());
    for leg in &legs {
        let balance = balances[&leg.user];
        let delta = match leg.amount {
            Amount::Credit(n) => n,
            Amount::Spend(n) => {
                let Some(pool) = pool else {
                    return Err(TransferError::Invalid);
                };
                let available = available_on(pool, conn, leg.user, balance, None).await?;
                if available < n {
                    return Err(TransferError::Insufficient {
                        user: leg.user,
                        available,
                        needed: n,
                    });
                }
                -n
            }
            Amount::Reclaim(n) => -n.min(balance).max(0),
        };
        if balance.checked_add(delta).is_none() {
            alert_transfer_refused(transfer, TRANSFER_INVALID, "balance overflow");
            return Err(TransferError::Invalid);
        }
        deltas.push(delta);
    }
    let kinds: Vec<&str> = legs.iter().map(|leg| leg.reason.kind().as_str()).collect();
    let refills: Vec<i32> = legs
        .iter()
        .map(|leg| i32::from(leg.reason.kind() == RowKind::Purchase))
        .collect();
    let rows: Vec<String> = legs
        .iter()
        .zip(&deltas)
        .map(|(leg, delta)| transfer_details(leg, -delta).to_string())
        .collect();
    // The rows are locked above, so the ledger reads each exact new balance.
    let written: Vec<(i64, Uuid, i64, i64)> = sqlx::query_as(
        "/* bk:bean_ledger_transfer */ \
         WITH e AS (SELECT * FROM unnest($1::uuid[],$2::bigint[],$3::text[],$4::text[],$5::text[],$6::int[]) \
                    AS e(user_id,delta,kind,transfer_key,details,refills)), \
         updated AS (UPDATE wallets w SET balance=w.balance+e.delta,refills=w.refills+e.refills,updated_at=now() \
              FROM e WHERE w.user_id=e.user_id AND w.game=$7 \
              RETURNING w.user_id,w.balance,e.delta,e.kind,e.transfer_key,e.details) \
         INSERT INTO wallet_ledger(user_id,game,kind,delta,balance_after,details,transfer_key) \
         SELECT user_id,$7,kind,delta,balance,details::jsonb,transfer_key FROM updated \
         RETURNING id,user_id,delta,balance_after",
    )
    .bind(&users)
    .bind(&deltas)
    .bind(&kinds)
    .bind(&keys)
    .bind(&rows)
    .bind(&refills)
    .bind(GAME)
    .fetch_all(conn.writer())
    .await?;
    if written.len() != legs.len() {
        // Unreachable with the rows locked; never commit a partial transfer.
        alert_transfer_refused(transfer, TRANSFER_INVALID, "partial wallet update");
        return Err(TransferError::Invalid);
    }
    let mut receipts: Vec<TransferReceipt> = written
        .into_iter()
        .map(|(ledger_id, user, delta, balance_after)| TransferReceipt {
            user,
            ledger_id,
            delta,
            balance_after,
        })
        .collect();
    receipts.sort_unstable_by_key(|receipt| receipt.user);
    Ok(Transferred::Applied(receipts))
}

/// The seat a reservation belongs to.
#[derive(Debug, Clone, Copy)]
pub enum Seat<'a> {
    /// A hold'em table, addressed by its session.
    Poker { session: Uuid },
    /// A code-addressed room; `revision` is the checkpoint revision that
    /// will carry the new stack.
    Room {
        game: &'a str,
        code: &'a str,
        revision: u64,
    },
}

#[derive(Debug)]
pub enum ReserveError {
    /// The wallet cannot cover the whole target stack beside its other seats.
    Insufficient,
    /// The bean authority cannot answer now; nothing was reserved.
    Unavailable(sqlx::Error),
}

/// Reserves `target` beans as the user's WHOLE stack at `seat` (not an
/// increment), after checking it against the authoritative spendable
/// balance. Reservations only ever grow here; they shrink when the seat's
/// durable state commits and end with [`release`].
pub fn reserve(pool: &PgPool, seat: Seat<'_>, user: Uuid, target: u64) -> Result<(), ReserveError> {
    match seat {
        Seat::Poker { session } => {
            match crate::beans_read_memory::reserve_poker_stack(pool, session, user, target) {
                Ok(true) => Ok(()),
                Ok(false) => Err(ReserveError::Insufficient),
                Err(error) => Err(ReserveError::Unavailable(error)),
            }
        }
        Seat::Room {
            game,
            code,
            revision,
        } => crate::beans_read_memory::reserve_card_stack(pool, game, code, user, target, revision)
            .map_err(|error| match &error {
                sqlx::Error::Protocol(m) if m == "insufficient_beans" => ReserveError::Insufficient,
                _ => ReserveError::Unavailable(error),
            }),
    }
}

/// Ends every reservation of a seat's room once the room itself is over
/// (its checkpoint removed, its session closed).
pub fn release(pool: &PgPool, seat: Seat<'_>) {
    match seat {
        Seat::Poker { session } => crate::beans_read_memory::release_poker_session(pool, session),
        Seat::Room { game, code, .. } => {
            crate::beans_read_memory::release_card_room(pool, game, code)
        }
    }
}

/// Spendable beans for a SQL debit holding the wallet row lock; `balance`
/// is that locked durable balance.
pub async fn available_on(
    pool: &PgPool,
    conn: &mut PgConnection,
    user: Uuid,
    balance: i64,
    excluding_sicbo: Option<&str>,
) -> sqlx::Result<i64> {
    crate::beans::available_on(pool, conn, user, balance, excluding_sicbo).await
}

/// One wallet whose balance disagrees with its ledger.
#[derive(Debug, Clone, Serialize)]
pub struct Mismatch {
    pub user_id: Uuid,
    pub balance: Option<i64>,
    pub ledger_sum: Option<i64>,
    pub last_balance_after: Option<i64>,
}

/// Result of one [`reconcile`] pass.
#[derive(Debug, Clone, Serialize, Default)]
pub struct Reconciliation {
    pub checked_at: Option<chrono::DateTime<chrono::Utc>>,
    pub elapsed_ms: u64,
    /// Bean wallets compared.
    pub wallets: i64,
    /// `balance != SUM(ledger.delta)`.
    pub sum_mismatch: i64,
    /// `balance !=` the latest ledger row's `balance_after`.
    pub last_mismatch: i64,
    pub wallet_without_ledger: i64,
    /// Accounts with ledger rows but no wallet row; reported, not a mismatch.
    pub ledger_without_wallet: i64,
    /// Wallets whose account row is gone. The account purge removes a
    /// wallet and its ledger before the account, so this should stay zero.
    pub wallets_without_account: i64,
    /// Wallets a settlement left below zero (allowed; zero spendable).
    pub negative_wallets: i64,
    /// Wallets whose live reservations exceed their balance.
    pub over_reserved: usize,
    pub examples: Vec<Mismatch>,
    pub over_reserved_examples: Vec<Uuid>,
    /// The pass could not finish (timeout, storage); counts are then zero.
    pub error: Option<String>,
}

impl Reconciliation {
    pub fn clean(&self) -> bool {
        self.error.is_none()
            && self.sum_mismatch == 0
            && self.last_mismatch == 0
            && self.wallet_without_ledger == 0
            && self.over_reserved == 0
    }
}

const RECONCILE_EXAMPLES: i64 = 20;
const RECONCILE_STATEMENT_TIMEOUT_MS: i64 = 10_000;
const RECONCILE_START_DELAY: Duration = Duration::from_secs(45);
const RECONCILE_INTERVAL: Duration = Duration::from_secs(6 * 60 * 60);

fn last() -> &'static Mutex<Option<Reconciliation>> {
    static LAST: OnceLock<Mutex<Option<Reconciliation>>> = OnceLock::new();
    LAST.get_or_init(Default::default)
}

/// The most recent finished pass, for the admin beans page. Memory only.
pub fn last_reconciliation() -> Option<Reconciliation> {
    last().lock().unwrap_or_else(|e| e.into_inner()).clone()
}

/// An account the purge has claimed (`deleted_at = 'epoch'`) is being erased
/// statement by statement, ledger first, so it is skipped while that runs.
/// Compares every bean wallet with its ledger in ONE read-only statement
/// (one snapshot, so a concurrent settlement is seen whole or not at all;
/// no row locks), bounded by a statement timeout, and compares live
/// reservations with balances in memory. Never called on a player path.
pub async fn reconcile(pool: &PgPool) -> sqlx::Result<Reconciliation> {
    let started = std::time::Instant::now();
    let mut tx = pool.begin_read().await?;
    sqlx::query(&format!(
        "SET LOCAL statement_timeout = {RECONCILE_STATEMENT_TIMEOUT_MS}"
    ))
    .execute(&mut *tx)
    .await?;
    let (
        wallets,
        sum_mismatch,
        last_mismatch,
        wallet_without_ledger,
        ledger_without_wallet,
        wallets_without_account,
        negative_wallets,
        examples,
    ): (i64, i64, i64, i64, i64, i64, i64, Value) = sqlx::query_as(
        "/* bk:bean_ledger_reconcile */ \
         WITH l AS (SELECT user_id,sum(delta)::bigint AS ledger_sum, \
                (max(ARRAY[id,balance_after]))[2] AS last_balance_after \
              FROM wallet_ledger WHERE game=$1 GROUP BY user_id), \
         j AS (SELECT COALESCE(w.user_id,l.user_id) AS user_id,w.balance,l.ledger_sum,l.last_balance_after \
              FROM (SELECT user_id,balance FROM wallets WHERE game=$1) w FULL JOIN l USING(user_id)), \
         bad AS (SELECT * FROM j WHERE balance IS NOT NULL \
              AND (balance IS DISTINCT FROM ledger_sum OR balance IS DISTINCT FROM last_balance_after) \
              AND NOT EXISTS(SELECT 1 FROM users u WHERE u.id=j.user_id AND u.deleted_at='epoch')) \
         SELECT (SELECT count(*) FROM j WHERE balance IS NOT NULL), \
           (SELECT count(*) FROM bad WHERE ledger_sum IS NOT NULL AND balance<>ledger_sum), \
           (SELECT count(*) FROM bad WHERE last_balance_after IS NOT NULL AND balance<>last_balance_after), \
           (SELECT count(*) FROM bad WHERE ledger_sum IS NULL), \
           (SELECT count(*) FROM j WHERE balance IS NULL), \
           (SELECT count(*) FROM j WHERE balance IS NOT NULL \
              AND NOT EXISTS(SELECT 1 FROM users u WHERE u.id=j.user_id)), \
           (SELECT count(*) FROM j WHERE balance < 0), \
           COALESCE((SELECT jsonb_agg(to_jsonb(x)) FROM (SELECT * FROM bad ORDER BY user_id LIMIT $2) x),'[]'::jsonb)",
    )
    .bind(GAME)
    .bind(RECONCILE_EXAMPLES)
    .fetch_one(&mut *tx)
    .await?;
    tx.commit().await?;
    let examples = examples
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|row| {
            Some(Mismatch {
                user_id: row["user_id"].as_str()?.parse().ok()?,
                balance: row["balance"].as_i64(),
                ledger_sum: row["ledger_sum"].as_i64(),
                last_balance_after: row["last_balance_after"].as_i64(),
            })
        })
        .collect();
    let over = crate::beans_read_memory::over_reserved(pool);
    Ok(Reconciliation {
        checked_at: Some(chrono::Utc::now()),
        elapsed_ms: started.elapsed().as_millis() as u64,
        wallets,
        sum_mismatch,
        last_mismatch,
        wallet_without_ledger,
        ledger_without_wallet,
        wallets_without_account,
        negative_wallets,
        over_reserved: over.len(),
        examples,
        over_reserved_examples: over.into_iter().take(RECONCILE_EXAMPLES as usize).collect(),
        error: None,
    })
}

/// Background reconciliation: once shortly after startup (readiness never
/// waits for it), then every six hours. A mismatch is alerted; nothing is
/// ever repaired automatically.
pub async fn run_reconciliation(pool: PgPool) {
    tokio::time::sleep(RECONCILE_START_DELAY).await;
    loop {
        let outcome = match tokio::time::timeout(Duration::from_secs(30), reconcile(&pool)).await {
            Ok(Ok(report)) => report,
            Ok(Err(error)) => Reconciliation {
                checked_at: Some(chrono::Utc::now()),
                error: Some(error.to_string()),
                ..Reconciliation::default()
            },
            Err(_) => Reconciliation {
                checked_at: Some(chrono::Utc::now()),
                error: Some("timed out".into()),
                ..Reconciliation::default()
            },
        };
        if let Some(error) = &outcome.error {
            tracing::warn!(%error, "bean_ledger_reconcile_unfinished");
        } else if outcome.clean() {
            tracing::info!(
                wallets = outcome.wallets,
                negative_wallets = outcome.negative_wallets,
                ledger_without_wallet = outcome.ledger_without_wallet,
                elapsed_ms = outcome.elapsed_ms,
                "bean_ledger_reconciled"
            );
        } else {
            tracing::error!(
                alert_error = "Gold-bean ledger check found wallets that do not match their ledger",
                alert_cause = "a balance differs from the sum of its ledger rows or from the latest balance_after, or live reservations exceed a balance / 有钱包余额与流水合计或最新 balance_after 不一致，或在桌预留超过余额",
                alert_action = "AI team opens the admin beans page for the listed accounts and traces the first diverging ledger row; nothing is auto-corrected; Loong action: none until AI reports / AI 在后台金豆页按名单追查首条不一致流水，系统不会自动修正，AI 汇报前 Loong 无需操作",
                "[bean_ledger_mismatch] wallets={} sum_mismatch={} last_mismatch={} wallet_without_ledger={} over_reserved={} first={:?} first_over_reserved={:?}",
                outcome.wallets,
                outcome.sum_mismatch,
                outcome.last_mismatch,
                outcome.wallet_without_ledger,
                outcome.over_reserved,
                outcome.examples.first().map(|row| row.user_id),
                outcome.over_reserved_examples.first(),
            );
        }
        *last().lock().unwrap_or_else(|e| e.into_inner()) = Some(outcome);
        tokio::time::sleep(RECONCILE_INTERVAL).await;
    }
}

// ---------------------------------------------------------------------------
// Wallet rows and the live balance projection of the seats that play on beans.
//
// One `wallets` row per user (game 10); every balance change is one
// `wallet_ledger` row written in the same transaction, so a balance always
// equals the sum of that wallet's ledger deltas. Poker chips (`users.chips`)
// and room-scoped points never pass through here.
// ---------------------------------------------------------------------------

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
pub async fn ensure(pool: &PgPool, user_id: Uuid) -> sqlx::Result<Wallet> {
    if held(GAME, user_id) {
        if let Some(wallet) = projected(GAME, user_id) {
            return Ok(wallet);
        }
    }
    ensure_durable(pool, user_id).await
}

/// Creates the wallet with its grant on first use; idempotent afterwards.
async fn ensure_durable(pool: &PgPool, user_id: Uuid) -> sqlx::Result<Wallet> {
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
    .bind(GAME)
    .fetch_optional(&mut *conn)
    .await?;
    tracing::debug!(target: "game_latency", game=GAME, stage="wallet_read",
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
    grant_on(&mut tx, user_id).await?;
    let row = sqlx::query(
        "SELECT balance, hands_played, hands_won, net_won, refills \
         FROM wallets WHERE user_id = $1 AND game = $2",
    )
    .bind(user_id)
    .bind(GAME)
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
