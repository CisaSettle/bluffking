//! SQL of the lottery center: purchases, the hall view, refills, listings and
//! presence. Every statement is an index lookup or a bounded index range
//! (period counters live on the period row; the feed reads the newest
//! tickets of a game). Lock order everywhere: period row → account lock
//! (`db::lock_active_users`) → wallet row.
use super::handlers::Error;
use super::rules::{self, Entry};
use super::{Game, Period, POLICY};
use crate::wallet;
use chrono::{DateTime, Utc};
use serde_json::{json, Value};
use sqlx::{postgres::PgRow, PgConnection, PgPool, Row};
use std::time::{Duration, Instant};
use uuid::Uuid;

pub const TICKET_COLUMNS: &str =
    "id, purchase_id, period, bought_at, entry, stakes, multiplier, cost, revealed, prize, tier, settled_at";
const PERIOD_COLUMNS: &str =
    "period, status, numbers, bonus, drawn_at, tickets, stakes, players, sales, paid, winners";
/// Tickets scanned for the hall feed (a purchase holds at most 10).
const FEED_SCAN: i64 = 100;
const FEED_LEN: usize = 10;
const WINNERS_BOARD: i64 = 20;
const RECENT_DRAWS: i64 = 10;
/// Presence rows this old no longer count as in the hall.
const PRESENT_WINDOW: Duration = Duration::from_secs(30);
const PRESENCE_UPSERT_EVERY: Duration = Duration::from_secs(10);
const CROWD_MEMBERS: i64 = 12;

#[derive(Debug, Clone)]
pub struct TicketRow {
    pub id: Uuid,
    pub purchase_id: Uuid,
    pub period: i64,
    pub bought_at: DateTime<Utc>,
    pub entry: Value,
    pub stakes: i32,
    pub multiplier: i16,
    pub cost: i64,
    pub revealed: Option<Vec<bool>>,
    pub prize: Option<i64>,
    pub tier: Option<i16>,
    pub settled_at: Option<DateTime<Utc>>,
}

impl TicketRow {
    pub fn from_row(row: &PgRow) -> Self {
        TicketRow {
            id: row.get("id"),
            purchase_id: row.get("purchase_id"),
            period: row.get("period"),
            bought_at: row.get("bought_at"),
            entry: row.get("entry"),
            stakes: row.get("stakes"),
            multiplier: row.get("multiplier"),
            cost: row.get("cost"),
            revealed: row.get("revealed"),
            prize: row.get("prize"),
            tier: row.get("tier"),
            settled_at: row.get("settled_at"),
        }
    }
}

/// The contract's `Ticket` (scratch numbers projected by `revealed`).
pub fn ticket_json(game: Game, t: &TicketRow) -> Value {
    let label = game.schedule().map(|s| s.period(t.period).label);
    let mut out = json!({
        "id": t.id, "purchase_id": t.purchase_id, "period": t.period, "label": label,
        "bought_at_ms": t.bought_at.timestamp_millis(),
        "entry": t.entry, "stakes": t.stakes, "multiplier": t.multiplier, "cost": t.cost,
        "prize": t.prize, "tier": t.tier, "settled": t.settled_at.is_some(),
    });
    if game == Game::Scratch {
        super::scratch::project(&mut out, t);
    }
    out
}

#[derive(Debug, Clone)]
pub struct PeriodRow {
    pub period: i64,
    pub status: String,
    pub numbers: Option<Vec<i16>>,
    pub bonus: Option<i16>,
    pub drawn_at: Option<DateTime<Utc>>,
    pub tickets: i32,
    pub stakes: i32,
    pub players: i32,
    pub sales: i64,
    pub paid: i64,
    pub winners: i32,
}

impl PeriodRow {
    fn from_row(row: &PgRow) -> Self {
        PeriodRow {
            period: row.get("period"),
            status: row.get("status"),
            numbers: row.get("numbers"),
            bonus: row.get("bonus"),
            drawn_at: row.get("drawn_at"),
            tickets: row.get("tickets"),
            stakes: row.get("stakes"),
            players: row.get("players"),
            sales: row.get("sales"),
            paid: row.get("paid"),
            winners: row.get("winners"),
        }
    }
    fn summary(&self, game: Game) -> Value {
        let label = game.schedule().map(|s| s.period(self.period).label);
        json!({
            "period": self.period, "label": label,
            "drawn_at_ms": self.drawn_at.map(|d| d.timestamp_millis()),
            "numbers": self.numbers.clone().unwrap_or_default(), "bonus": self.bonus,
            "tickets": self.tickets, "players": self.players, "paid": self.paid,
        })
    }
}

/// A committed purchase as the buy endpoint reports it.
#[derive(Debug, Clone)]
pub struct Purchase {
    pub purchased: Vec<Value>,
    pub charged: i64,
    pub balance_before: i64,
    pub balance_after: i64,
}

#[derive(Debug, Clone, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub struct NumberBuy {
    pub request_id: Uuid,
    pub period: i64,
    pub multiplier: i64,
    pub tickets: Vec<Value>,
}

pub fn body_sha256(body: &Value) -> String {
    use sha2::{Digest, Sha256};
    hex::encode(Sha256::digest(body.to_string().as_bytes()))
}

/// The ledger row a derived purchase record id already wrote, if any.
pub(super) async fn stored_play(
    conn: &mut PgConnection,
    user: Uuid,
    record_id: Uuid,
) -> sqlx::Result<Option<(Value, i64)>> {
    let row = sqlx::query(
        "SELECT details, balance_after FROM wallet_ledger WHERE game IN ($1, 23) AND record_id=$2 AND user_id=$3",
    )
    .bind(POLICY.game)
    .bind(record_id)
    .bind(user)
    .fetch_optional(conn)
    .await?;
    Ok(row.map(|r| (r.get("details"), r.get("balance_after"))))
}

/// Replay of a purchase whose derived id is already in the ledger: the same
/// normalised body returns the stored purchase, another body conflicts.
pub(super) async fn replay(
    conn: &mut PgConnection,
    game: Game,
    user: Uuid,
    record_id: Uuid,
    body_hash: &str,
) -> Result<Option<Purchase>, Error> {
    let Some((details, balance_after)) = stored_play(conn, user, record_id).await? else {
        return Ok(None);
    };
    if details["body_sha256"].as_str() != Some(body_hash) {
        return Err(Error::RequestConflict);
    }
    let ids: Vec<Uuid> = serde_json::from_value(details["ticket_ids"].clone()).unwrap_or_default();
    let rows = sqlx::query(&format!(
        "SELECT {TICKET_COLUMNS} FROM lottery_tickets WHERE purchase_id=$1"
    ))
    .bind(record_id)
    .fetch_all(conn)
    .await?;
    let mut tickets: Vec<TicketRow> = rows.iter().map(TicketRow::from_row).collect();
    tickets.sort_by_key(|t| ids.iter().position(|id| *id == t.id).unwrap_or(usize::MAX));
    Ok(Some(Purchase {
        purchased: tickets.iter().map(|t| ticket_json(game, t)).collect(),
        charged: details["charged"].as_i64().unwrap_or_default(),
        balance_before: details["balance_before"].as_i64().unwrap_or_default(),
        balance_after,
    }))
}

/// A request id is shared across purchase kinds; switching to/from a scratch
/// batch must conflict rather than debit again under another record namespace.
pub(super) async fn replay_request(
    conn: &mut PgConnection,
    game: Game,
    user: Uuid,
    request_id: Uuid,
    body_hash: &str,
) -> Result<Option<Purchase>, Error> {
    for record_id in [
        super::purchase_id(user, request_id),
        super::scratch_batch_id(user, request_id),
    ] {
        if let Some(purchase) = replay(conn, game, user, record_id, body_hash).await? {
            return Ok(Some(purchase));
        }
    }
    Ok(None)
}

/// Reads an existing lottery wallet without a write. First creation holds the
/// active account lock through the wallet grant and ledger commit, so a purge
/// claim cannot race a late request into recreating account-owned money rows.
pub(super) async fn ensure_wallet(pool: &PgPool, user: Uuid) -> sqlx::Result<i64> {
    if let Some(balance) =
        sqlx::query_scalar::<_, i64>("SELECT balance FROM wallets WHERE user_id=$1 AND game=$2")
            .bind(user)
            .bind(POLICY.game)
            .fetch_optional(pool)
            .await?
    {
        return Ok(balance);
    }
    let mut tx = pool.begin().await?;
    crate::db::lock_active_users(&mut tx, &[user]).await?;
    let inserted = sqlx::query(
        "INSERT INTO wallets (user_id,game,balance) VALUES ($1,$2,$3) ON CONFLICT (user_id,game) DO NOTHING",
    ).bind(user).bind(POLICY.game).bind(POLICY.grant).execute(&mut *tx).await?.rows_affected();
    if inserted == 1 {
        sqlx::query(
            "INSERT INTO wallet_ledger (user_id,game,kind,delta,balance_after) VALUES ($1,$2,'grant',$3,$3)",
        ).bind(user).bind(POLICY.game).bind(POLICY.grant).execute(&mut *tx).await?;
    }
    let balance = sqlx::query_scalar("SELECT balance FROM wallets WHERE user_id=$1 AND game=$2")
        .bind(user)
        .bind(POLICY.game)
        .fetch_one(&mut *tx)
        .await?;
    tx.commit().await?;
    Ok(balance)
}

/// One ticket purchase of a number game (contract: purchase transaction).
pub async fn buy_numbers(
    pool: &PgPool,
    user: Uuid,
    game: Game,
    body: &NumberBuy,
) -> Result<Purchase, Error> {
    let schedule = game.schedule().ok_or(Error::InvalidEntry)?;
    if body.request_id.is_nil()
        || !(1..=rules::MAX_MULTIPLIER).contains(&body.multiplier)
        || body.tickets.is_empty()
        || body.tickets.len() > rules::MAX_TICKETS_PER_PURCHASE
    {
        return Err(Error::InvalidEntry);
    }
    let entries = body
        .tickets
        .iter()
        .map(|t| Entry::parse(game, t).map_err(|_| Error::InvalidEntry))
        .collect::<Result<Vec<_>, _>>()?;
    let normalised = json!({
        "game": game.as_str(), "period": body.period, "multiplier": body.multiplier,
        "tickets": entries.iter().map(Entry::to_value).collect::<Vec<_>>(),
    });
    let body_hash = body_sha256(&normalised);
    let record_id = super::purchase_id(user, body.request_id);
    let stakes: Vec<i64> = entries.iter().map(Entry::stakes).collect();
    let new_stakes: i64 = stakes.iter().sum();
    let costs: Vec<i64> = stakes
        .iter()
        .map(|s| s * game.unit() * body.multiplier)
        .collect();
    let charged: i64 = costs.iter().sum();

    ensure_wallet(pool, user).await?;
    // (A) Replay without locks.
    {
        let mut conn = pool.acquire().await?;
        if let Some(stored) =
            replay_request(&mut conn, game, user, body.request_id, &body_hash).await?
        {
            return Ok(stored);
        }
    }
    let mut tx = pool.begin().await?;
    // (B) The period row, only for the period on sale right now.
    let mut selling = schedule.period_at(super::now_ms());
    let mut period_status: Option<String> = None;
    if body.period == selling.id {
        sqlx::query(
            "INSERT INTO lottery_periods (game, period, opens_at, draw_at) VALUES ($1, $2, $3, $4) \
             ON CONFLICT (game, period) DO NOTHING",
        )
        .bind(game.as_str())
        .bind(selling.id)
        .bind(DateTime::from_timestamp_millis(selling.opens_ms).unwrap_or_default())
        .bind(DateTime::from_timestamp_millis(selling.draw_ms).unwrap_or_default())
        .execute(&mut *tx)
        .await?;
        period_status = sqlx::query_scalar(
            "SELECT status FROM lottery_periods WHERE game=$1 AND period=$2 FOR UPDATE",
        )
        .bind(game.as_str())
        .bind(selling.id)
        .fetch_optional(&mut *tx)
        .await?;
    }
    // (C) Account, wallet, then the replay check again under the locks.
    crate::db::lock_active_users(&mut tx, &[user]).await?;
    let balance = wallet::lock(&mut tx, POLICY.game, user)
        .await?
        .ok_or(sqlx::Error::RowNotFound)?;
    if let Some(stored) = replay_request(&mut tx, game, user, body.request_id, &body_hash).await? {
        tx.commit().await?;
        return Ok(stored);
    }
    require_spendable(&mut tx, user).await?;
    // (D) Sales window, caps, balance.
    let now = Utc::now();
    selling = schedule.period_at(now.timestamp_millis());
    if body.period != selling.id
        || period_status.as_deref() != Some("open")
        || now.timestamp_millis() >= selling.closes_ms
    {
        return Err(Error::SalesClosed);
    }
    let (count, used): (i64, i64) = sqlx::query_as(
        "SELECT COUNT(*)::bigint, COALESCE(SUM(stakes), 0)::bigint FROM lottery_tickets \
         WHERE game=$1 AND period=$2 AND user_id=$3",
    )
    .bind(game.as_str())
    .bind(selling.id)
    .bind(user)
    .fetch_one(&mut *tx)
    .await?;
    if count + entries.len() as i64 > rules::MAX_TICKETS_PER_PERIOD
        || used + new_stakes > rules::MAX_STAKES_PER_PERIOD
    {
        return Err(Error::TooManyTickets);
    }
    if charged > balance {
        return Err(Error::InsufficientBalance);
    }
    let ids: Vec<Uuid> = (0..entries.len()).map(|_| Uuid::new_v4()).collect();
    let play = wallet::Play {
        record_id,
        delta: -charged,
        completes_hand: false,
        won: false,
        room_code: None,
        revision: None,
        details: Some(json!({
            "purpose": "purchase", "game": game.as_str(), "period": selling.id,
            "body_sha256": body_hash, "ticket_ids": ids, "charged": charged, "balance_before": balance,
        })),
        hand_completed: None,
        has_bots: None,
    };
    let balance_after = wallet::settle(&mut tx, POLICY.game, user, &play).await?;
    let entry_values: Vec<Value> = entries.iter().map(Entry::to_value).collect();
    let stakes_i32: Vec<i32> = stakes.iter().map(|s| *s as i32).collect();
    sqlx::query(
        "INSERT INTO lottery_tickets (id, purchase_id, game, period, user_id, bought_at, entry, stakes, multiplier, cost) \
         SELECT u.id, $5, $6, $7, $8, $9, u.entry, u.stakes, $10, u.cost \
         FROM UNNEST($1::uuid[], $2::jsonb[], $3::int4[], $4::int8[]) AS u(id, entry, stakes, cost)",
    )
    .bind(&ids)
    .bind(&entry_values)
    .bind(&stakes_i32)
    .bind(&costs)
    .bind(record_id)
    .bind(game.as_str())
    .bind(selling.id)
    .bind(user)
    .bind(now)
    .bind(body.multiplier as i16)
    .execute(&mut *tx)
    .await?;
    sqlx::query(
        "UPDATE lottery_periods SET tickets = tickets + $3, stakes = stakes + $4, sales = sales + $5, \
         players = players + $6 WHERE game=$1 AND period=$2",
    )
    .bind(game.as_str())
    .bind(selling.id)
    .bind(entries.len() as i32)
    .bind(new_stakes as i32)
    .bind(charged)
    .bind(i32::from(count == 0))
    .execute(&mut *tx)
    .await?;
    let rows = sqlx::query(&format!(
        "SELECT {TICKET_COLUMNS} FROM lottery_tickets WHERE purchase_id=$1"
    ))
    .bind(record_id)
    .fetch_all(&mut *tx)
    .await?;
    tx.commit().await?;
    crate::stats_writer::send(crate::stats_writer::StatsEvent::LotteryPurchase {
        day: super::sgt_day(now),
        game: game.as_str(),
        user,
        tickets: entries.len() as i64,
        stakes: new_stakes,
        sales: charged,
    });
    let mut tickets: Vec<TicketRow> = rows.iter().map(TicketRow::from_row).collect();
    tickets.sort_by_key(|t| ids.iter().position(|id| *id == t.id).unwrap_or(usize::MAX));
    Ok(Purchase {
        purchased: tickets.iter().map(|t| ticket_json(game, t)).collect(),
        charged,
        balance_before: balance,
        balance_after,
    })
}

/// Legacy lottery route shares the single daily beans entitlement.
pub async fn refill(pool: &PgPool, user: Uuid, request_id: Uuid) -> Result<i64, Error> {
    if request_id.is_nil() {
        return Err(Error::InvalidEntry);
    }
    match crate::beans::claim_daily(pool, user).await? {
        crate::beans::DailyOutcome::Granted { balance, .. } => Ok(balance),
        crate::beans::DailyOutcome::AlreadyClaimed { .. } => Err(Error::DailyAlreadyClaimed),
    }
}

/// Called after locking the shared wallet: poker admission uses this same lock.
/// A seated stack or unfinished hand is not available for a second game's stake.
pub(super) async fn require_spendable(conn: &mut PgConnection, user: Uuid) -> Result<(), Error> {
    let seated: bool = sqlx::query_scalar(
        "SELECT EXISTS(SELECT 1 FROM session_users su JOIN game_sessions gs ON gs.id=su.session_id
         JOIN public_tables pt ON pt.session_id=gs.id
         WHERE su.user_id=$1 AND su.left_at IS NULL AND gs.ended_at IS NULL AND pt.beans)",
    )
    .bind(user)
    .fetch_one(&mut *conn)
    .await?;
    if seated || crate::beans::holds::outstanding(user) {
        return Err(Error::BeansInUse);
    }
    // Exact hand-scoped locks cover departed players on another local backend;
    // a completed hand releases them even while its table stays open.
    if crate::local_runtime::beans_held(user).map_err(sqlx::Error::Io)? {
        return Err(Error::BeansInUse);
    }
    Ok(())
}

async fn tickets_of(
    conn: &mut PgConnection,
    game: Game,
    period: i64,
    user: Uuid,
) -> sqlx::Result<Vec<TicketRow>> {
    let rows = sqlx::query(&format!(
        "SELECT {TICKET_COLUMNS} FROM lottery_tickets WHERE game=$1 AND period=$2 AND user_id=$3 \
         ORDER BY bought_at DESC, id"
    ))
    .bind(game.as_str())
    .bind(period)
    .bind(user)
    .fetch_all(conn)
    .await?;
    Ok(rows.iter().map(TicketRow::from_row).collect())
}

pub(super) async fn recent_periods(
    conn: &mut PgConnection,
    game: Game,
    limit: i64,
) -> sqlx::Result<Vec<PeriodRow>> {
    let rows = sqlx::query(&format!(
        "SELECT {PERIOD_COLUMNS} FROM lottery_periods WHERE game=$1 AND status <> 'open' \
         ORDER BY drawn_at DESC LIMIT $2"
    ))
    .bind(game.as_str())
    .bind(limit)
    .fetch_all(conn)
    .await?;
    Ok(rows.iter().map(PeriodRow::from_row).collect())
}

/// Public name of a user: display name or the opaque fallback.
pub fn public_name(user: Uuid, display_name: Option<String>) -> String {
    let name = display_name
        .map(|n| n.trim().to_owned())
        .filter(|n| !n.is_empty())
        .unwrap_or_else(|| crate::display_name::fallback_display_name(user));
    crate::content_moderation::public_display_name(&name, user)
}

async fn names_of(
    conn: &mut PgConnection,
    ids: &[Uuid],
) -> sqlx::Result<std::collections::HashMap<Uuid, String>> {
    if ids.is_empty() {
        return Ok(Default::default());
    }
    let rows = sqlx::query("SELECT id, display_name FROM users WHERE id = ANY($1)")
        .bind(ids)
        .fetch_all(conn)
        .await?;
    let mut names: std::collections::HashMap<Uuid, String> = rows
        .into_iter()
        .map(|r| {
            let id: Uuid = r.get("id");
            (id, public_name(id, r.get("display_name")))
        })
        .collect();
    for id in ids {
        names
            .entry(*id)
            .or_insert_with(|| crate::display_name::fallback_display_name(*id));
    }
    Ok(names)
}

struct FeedScan {
    purchases: Vec<Value>,
    /// Scratch: cards that paid, as `won` events.
    wins: Vec<Value>,
}

/// The newest purchases of a game (grouped by purchase) from the feed index.
async fn feed_scan(conn: &mut PgConnection, game: Game) -> sqlx::Result<FeedScan> {
    let rows = sqlx::query(
        "SELECT purchase_id, user_id, bought_at, stakes, prize, tier, settled_at FROM lottery_tickets \
         WHERE game=$1 ORDER BY bought_at DESC LIMIT $2",
    )
    .bind(game.as_str())
    .bind(FEED_SCAN)
    .fetch_all(&mut *conn)
    .await?;
    let mut purchases: Vec<(Uuid, Uuid, DateTime<Utc>, i64, i64)> = Vec::new();
    let mut wins: Vec<(Uuid, DateTime<Utc>, i16, i64)> = Vec::new();
    for row in &rows {
        let purchase: Uuid = row.get("purchase_id");
        let user: Uuid = row.get("user_id");
        let at: DateTime<Utc> = row.get("bought_at");
        let stakes: i32 = row.get("stakes");
        if let Some(p) = purchases.iter_mut().find(|p| p.0 == purchase) {
            p.3 += 1;
            p.4 += i64::from(stakes);
        } else if purchases.len() < FEED_LEN {
            purchases.push((purchase, user, at, 1, i64::from(stakes)));
        }
        if game == Game::Scratch && wins.len() < FEED_LEN {
            if let (Some(prize), Some(settled)) = (
                row.get::<Option<i64>, _>("prize"),
                row.get::<Option<DateTime<Utc>>, _>("settled_at"),
            ) {
                if prize > 0 {
                    wins.push((
                        user,
                        settled,
                        row.get::<Option<i16>, _>("tier").unwrap_or(0),
                        prize,
                    ));
                }
            }
        }
    }
    let mut ids: Vec<Uuid> = purchases
        .iter()
        .map(|p| p.1)
        .chain(wins.iter().map(|w| w.0))
        .collect();
    ids.sort_unstable();
    ids.dedup();
    let names = names_of(conn, &ids).await?;
    let name = |id: Uuid| names.get(&id).cloned().unwrap_or_default();
    Ok(FeedScan {
        purchases: purchases
            .iter()
            .map(|(_, user, at, count, stakes)| {
                json!({ "at_ms": at.timestamp_millis(), "user_id": user, "name": name(*user),
                        "kind": "bought", "stakes": stakes, "count": count })
            })
            .collect(),
        wins: wins
            .iter()
            .map(|(user, at, tier, prize)| {
                json!({ "at_ms": at.timestamp_millis(), "user_id": user, "name": name(*user),
                        "kind": "won", "tier": tier, "prize": prize })
            })
            .collect(),
    })
}

fn merge_feed(mut events: Vec<Value>) -> Vec<Value> {
    events.sort_by_key(|e| std::cmp::Reverse(e["at_ms"].as_i64().unwrap_or_default()));
    events.truncate(FEED_LEN);
    events
}

fn presence_touched() -> &'static dashmap::DashMap<(Game, Uuid), Instant> {
    static TOUCHED: std::sync::OnceLock<dashmap::DashMap<(Game, Uuid), Instant>> =
        std::sync::OnceLock::new();
    TOUCHED.get_or_init(Default::default)
}

/// Forgets throttle entries older than the presence window (retention sweep).
pub fn prune_presence_throttle() {
    presence_touched().retain(|_, at| at.elapsed() < Duration::from_secs(3600));
}

/// Presence is optional: never await it in the view/purchase response. At most
/// two tasks run, no pending queue, and each attempt expires after 500 ms.
fn schedule_presence(pool: &PgPool, game: Game, user: Uuid) {
    static SLOTS: std::sync::OnceLock<std::sync::Arc<tokio::sync::Semaphore>> =
        std::sync::OnceLock::new();
    let slots = SLOTS.get_or_init(|| std::sync::Arc::new(tokio::sync::Semaphore::new(2)));
    let Ok(permit) = slots.clone().try_acquire_owned() else {
        return;
    };
    let key = (game, user);
    // Bound the process-local throttle as well as the database work.
    if presence_touched().len() >= 4096 && !presence_touched().contains_key(&key) {
        return;
    }
    match presence_touched().entry(key) {
        dashmap::mapref::entry::Entry::Occupied(mut entry) => {
            if entry.get().elapsed() < PRESENCE_UPSERT_EVERY {
                return;
            }
            entry.insert(Instant::now());
        }
        dashmap::mapref::entry::Entry::Vacant(entry) => {
            entry.insert(Instant::now());
        }
    }
    let pool = pool.clone();
    tokio::spawn(async move {
        let _permit = permit;
        // Failure/backpressure drops this optional observation; a later poll
        // may try again after the throttle interval. Never report recursively.
        let _ = tokio::time::timeout(
            Duration::from_millis(500),
            touch_presence(&pool, game, user),
        )
        .await;
    });
}

async fn touch_presence(pool: &PgPool, game: Game, user: Uuid) -> sqlx::Result<()> {
    let mut tx = pool.begin().await?;
    sqlx::query("SELECT set_config('lock_timeout','100ms',true),set_config('statement_timeout','250ms',true)")
        .execute(&mut *tx)
        .await?;
    crate::db::lock_active_users(&mut tx, &[user]).await?;
    let display: Option<String> = sqlx::query_scalar("SELECT display_name FROM users WHERE id=$1")
        .bind(user)
        .fetch_one(&mut *tx)
        .await?;
    sqlx::query(
        "INSERT INTO lottery_presence (game, user_id, name, seen_at) VALUES ($1, $2, $3, now()) \
         ON CONFLICT (game, user_id) DO UPDATE SET seen_at = EXCLUDED.seen_at, name = EXCLUDED.name",
    )
    .bind(game.as_str()).bind(user).bind(public_name(user, display))
    .execute(&mut *tx).await?;
    tx.commit().await
}

async fn crowd(conn: &mut PgConnection, game: Game, now: DateTime<Utc>) -> sqlx::Result<Value> {
    let since = now - chrono::Duration::from_std(PRESENT_WINDOW).unwrap_or_default();
    let rows = sqlx::query(
        "SELECT user_id, name, seen_at, count(*) OVER () AS present FROM lottery_presence \
         WHERE game=$1 AND seen_at >= $2 ORDER BY seen_at DESC LIMIT $3",
    )
    .bind(game.as_str())
    .bind(since)
    .bind(CROWD_MEMBERS)
    .fetch_all(conn)
    .await?;
    let present: i64 = rows.first().map_or(0, |r| r.get("present"));
    let members: Vec<Value> = rows
        .iter()
        .map(|r| {
            json!({ "user_id": r.get::<Uuid, _>("user_id"), "name": r.get::<String, _>("name"),
                    "seen_at_ms": r.get::<DateTime<Utc>, _>("seen_at").timestamp_millis() })
        })
        .collect();
    Ok(json!({ "present": present, "members": members }))
}

async fn draw_json(
    conn: &mut PgConnection,
    game: Game,
    user: Uuid,
    row: &PeriodRow,
) -> sqlx::Result<Value> {
    let winners = sqlx::query(
        "SELECT user_id, name, tier, prize FROM lottery_winners WHERE game=$1 AND period=$2 \
         ORDER BY prize DESC LIMIT $3",
    )
    .bind(game.as_str())
    .bind(row.period)
    .bind(WINNERS_BOARD)
    .fetch_all(&mut *conn)
    .await?;
    let winners: Vec<Value> = winners
        .iter()
        .map(|w| {
            json!({ "user_id": w.get::<Option<Uuid>, _>("user_id"), "name": w.get::<String, _>("name"),
                    "tier": w.get::<i16, _>("tier"), "prize": w.get::<i64, _>("prize") })
        })
        .collect();
    let mine = tickets_of(conn, game, row.period, user).await?;
    let label = game.schedule().map(|s| s.period(row.period).label);
    Ok(json!({
        "period": row.period, "label": label,
        "drawn_at_ms": row.drawn_at.map(|d| d.timestamp_millis()).unwrap_or_default(),
        "status": row.status, "numbers": row.numbers.clone().unwrap_or_default(), "bonus": row.bonus,
        "tickets": row.tickets, "players": row.players, "sales": row.sales, "paid": row.paid,
        "winners_count": row.winners, "winners": winners,
        "mine": mine.iter().map(|t| ticket_json(game, t)).collect::<Vec<_>>(),
    }))
}

/// `GET /api/lottery/{game}`.
pub async fn view(pool: &PgPool, user: Uuid, game: Game) -> Result<Value, Error> {
    let balance = ensure_wallet(pool, user).await?;
    schedule_presence(pool, game, user);
    let now = Utc::now();
    let now_ms = now.timestamp_millis();
    let mut conn = pool.acquire().await?;
    let crowd = crowd(&mut conn, game, now).await?;
    let scan = feed_scan(&mut conn, game).await?;
    let Some(schedule) = game.schedule() else {
        let my_tickets = super::scratch::my_cards(&mut conn, user).await?;
        return Ok(json!({
            "game": game.as_str(), "now_ms": now_ms, "balance": balance, "unit": game.unit(),
            "period": Value::Null,
            "my_tickets": my_tickets.iter().map(|t| ticket_json(game, t)).collect::<Vec<_>>(),
            "last_draw": Value::Null, "recent": [], "crowd": crowd,
            "feed": merge_feed(scan.purchases.into_iter().chain(scan.wins).collect()),
        }));
    };
    let selling = schedule.period_at(now_ms);
    let row = sqlx::query(&format!(
        "SELECT {PERIOD_COLUMNS} FROM lottery_periods WHERE game=$1 AND period=$2"
    ))
    .bind(game.as_str())
    .bind(selling.id)
    .fetch_optional(&mut *conn)
    .await?
    .map(|r| PeriodRow::from_row(&r));
    let open = row.as_ref().is_none_or(|r| r.status == "open") && now_ms < selling.closes_ms;
    let period = period_json(&selling, row.as_ref(), open);
    let my_tickets = tickets_of(&mut conn, game, selling.id, user).await?;
    let recent = recent_periods(&mut conn, game, RECENT_DRAWS).await?;
    let mut feed = scan.purchases;
    let last_draw = match recent.first() {
        Some(last) => {
            let draw = draw_json(&mut conn, game, user, last).await?;
            let label = draw["label"].clone();
            let at = draw["drawn_at_ms"].clone();
            if let Some(winners) = draw["winners"].as_array() {
                feed.extend(winners.iter().map(|w| {
                    json!({ "at_ms": at, "user_id": w["user_id"], "name": w["name"], "kind": "won",
                            "tier": w["tier"], "prize": w["prize"], "period_label": label })
                }));
            }
            draw
        }
        None => Value::Null,
    };
    Ok(json!({
        "game": game.as_str(), "now_ms": now_ms, "balance": balance, "unit": game.unit(),
        "period": period,
        "my_tickets": my_tickets.iter().map(|t| ticket_json(game, t)).collect::<Vec<_>>(),
        "last_draw": last_draw,
        "recent": recent.iter().map(|r| r.summary(game)).collect::<Vec<_>>(),
        "crowd": crowd, "feed": merge_feed(feed),
    }))
}

fn period_json(selling: &Period, row: Option<&PeriodRow>, open: bool) -> Value {
    json!({
        "id": selling.id, "label": selling.label,
        "opens_ms": selling.opens_ms, "closes_ms": selling.closes_ms, "draw_ms": selling.draw_ms,
        "status": if open { "open" } else { "closed" },
        "tickets": row.map_or(0, |r| r.tickets), "stakes": row.map_or(0, |r| r.stakes),
        "players": row.map_or(0, |r| r.players), "sales": row.map_or(0, |r| r.sales),
    })
}

/// `GET /api/lottery/{game}/draws`.
pub async fn draws(pool: &PgPool, game: Game, limit: i64) -> Result<Vec<Value>, Error> {
    let mut conn = pool.acquire().await?;
    let rows = recent_periods(&mut conn, game, limit.clamp(1, 100)).await?;
    Ok(rows.iter().map(|r| r.summary(game)).collect())
}

/// `GET /api/lottery/{game}/tickets`: the viewer's newest tickets with the
/// period result once drawn.
pub async fn tickets(
    pool: &PgPool,
    user: Uuid,
    game: Game,
    limit: i64,
) -> Result<Vec<Value>, Error> {
    let rows = sqlx::query(&format!(
        "SELECT t.{} , p.numbers AS result_numbers, p.bonus AS result_bonus \
         FROM lottery_tickets t LEFT JOIN lottery_periods p \
           ON p.game = t.game AND p.period = t.period AND p.status <> 'open' \
         WHERE t.user_id=$1 AND t.game=$2 ORDER BY t.bought_at DESC, t.id LIMIT $3",
        TICKET_COLUMNS.replace(", ", ", t.")
    ))
    .bind(user)
    .bind(game.as_str())
    .bind(limit.clamp(1, 100))
    .fetch_all(pool)
    .await?;
    Ok(rows
        .iter()
        .map(|r| {
            let mut ticket = ticket_json(game, &TicketRow::from_row(r));
            let numbers: Option<Vec<i16>> = r.get("result_numbers");
            ticket["result"] = match numbers {
                Some(numbers) => {
                    json!({ "numbers": numbers, "bonus": r.get::<Option<i16>, _>("result_bonus") })
                }
                None => Value::Null,
            };
            ticket
        })
        .collect())
}
