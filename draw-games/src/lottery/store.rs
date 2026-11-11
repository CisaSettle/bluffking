//! Read-side SQL of the lottery center: purchase replay, history, listings
//! and proofs, plus the row and wire types the memory runtime shares. Every
//! statement is an index lookup or a bounded index range. Purchases, draws
//! and settlements are accepted and written by [`super::runtime`] only.
use super::handlers::Error;
use super::{Game, Period, POLICY};
use crate::db_writer::{PgConnection, PgPool};
use chrono::{DateTime, Utc};
use draw_games::lottery::Plan;
use serde_json::{json, Value};
use sqlx::{postgres::PgRow, Row};
use uuid::Uuid;

pub const TICKET_COLUMNS: &str =
    "id, purchase_id, period, bought_at, entry, stakes, multiplier, cost, revealed, prize, tier, settled_at, proof_plan";
const PERIOD_COLUMNS: &str =
    "period, status, numbers, bonus, bonus_numbers, drawn_at, tickets, stakes, players, sales, paid, winners, proof_plan, draw_proof";
/// Bounded ticket scan for the hall feed.
const FEED_SCAN: i64 = 100;
const FEED_LEN: usize = 10;
const WINNERS_BOARD: i64 = 20;
const RECENT_DRAWS: i64 = 10;
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
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
    pub proof_plan: Option<Value>,
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
            proof_plan: row.get("proof_plan"),
        }
    }
}

/// Existing period IDs used each game's old interval. Never reinterpret them
/// through today's minute schedule: the immutable plan identifies its version.
fn period_label(game: Game, period: i64, stored: Option<&Value>) -> Option<String> {
    if let Some(stored) = stored {
        let plan: Plan = serde_json::from_value(stored.clone()).ok()?;
        if plan.game != game.as_str() || plan.period != period {
            return None;
        }
        return plan.label().ok();
    }
    // Every current period is created with a plan; plan-less rows predate it.
    let schedule = game.legacy_schedule()?;
    let draw_ms = period
        .checked_add(1)?
        .checked_mul(schedule.interval_ms)?
        .checked_add(schedule.offset_ms)?;
    Some(schedule.label(draw_ms))
}

/// The contract's `Ticket` (scratch numbers projected by `revealed`).
pub fn ticket_json(game: Game, t: &TicketRow) -> Value {
    let label = period_label(game, t.period, t.proof_plan.as_ref());
    let mut out = json!({
        "id": t.id, "purchase_id": t.purchase_id, "period": t.period, "label": label,
        "bought_at_ms": t.bought_at.timestamp_millis(),
        "entry": t.entry, "stakes": t.stakes, "multiplier": t.multiplier, "cost": t.cost,
        "prize": t.prize, "tier": t.tier, "settled": t.settled_at.is_some(),
    });
    if game == Game::Scratch {
        super::scratch::project(&mut out, t);
    } else {
        // Max grants are legacy (pre-bonus) jackpot rewards; new ones are beans.
        out["reward_grant"] = t.entry["reward_grant"].clone();
        out["jackpot_bonus"] = t.entry["jackpot_bonus"].clone();
        // Public entries remain the normalized selected numbers used by proofs.
        if let Some(entry) = out["entry"].as_object_mut() {
            entry.remove("reward_grant");
            entry.remove("jackpot_bonus");
        }
        out["proof_plan"] = json!(t.proof_plan);
        out["proof_status"] = json!(if t.proof_plan.is_some() {
            "planned"
        } else {
            "unavailable"
        });
        if t.proof_plan.is_some() {
            out["proof_url"] = json!(format!(
                "/api/history/draws-proof?game={}&period={}",
                game.as_str(),
                t.period
            ));
        }
    }
    out
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct PeriodRow {
    pub period: i64,
    pub status: String,
    pub numbers: Option<Vec<i16>>,
    pub bonus: Option<i16>,
    pub bonus_numbers: Vec<i16>,
    pub drawn_at: Option<DateTime<Utc>>,
    pub settled_at: Option<DateTime<Utc>>,
    pub tickets: i32,
    pub stakes: i32,
    pub players: i32,
    pub sales: i64,
    pub paid: i64,
    pub winners: i32,
    pub proof_plan: Option<Value>,
    pub draw_proof: Option<Value>,
}

impl PeriodRow {
    pub(super) fn from_row(row: &PgRow) -> Self {
        PeriodRow {
            period: row.get("period"),
            status: row.get("status"),
            numbers: row.get("numbers"),
            bonus: row.get("bonus"),
            bonus_numbers: row.get("bonus_numbers"),
            drawn_at: row.get("drawn_at"),
            settled_at: row.try_get("settled_at").unwrap_or(None),
            tickets: row.get("tickets"),
            stakes: row.get("stakes"),
            players: row.get("players"),
            sales: row.get("sales"),
            paid: row.get("paid"),
            winners: row.get("winners"),
            proof_plan: row.get("proof_plan"),
            draw_proof: row.get("draw_proof"),
        }
    }
    pub(super) fn summary(&self, game: Game) -> Value {
        let label = period_label(game, self.period, self.proof_plan.as_ref());
        json!({
            "period": self.period, "label": label,
            "drawn_at_ms": self.drawn_at.map(|d| d.timestamp_millis()),
            "numbers": self.numbers.clone().unwrap_or_default(), "bonus": self.bonus,
            "bonus_numbers": self.bonus_numbers,
            "tickets": self.tickets, "players": self.players, "paid": self.paid,
            "proof_plan": self.proof_plan, "draw_proof": self.draw_proof,
            "proof_status": proof_status(self.proof_plan.as_ref(), self.draw_proof.as_ref()),
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
    pub proof_commitment: String,
    #[serde(default)]
    pub all_in: bool,
    #[serde(default)]
    pub expected_balance: Option<i64>,
}

fn proof_status(plan: Option<&Value>, proof: Option<&Value>) -> &'static str {
    if proof.is_some() {
        "available"
    } else if plan.is_some() {
        "pending"
    } else {
        "unavailable"
    }
}

pub fn body_sha256(body: &Value) -> String {
    use sha2::{Digest, Sha256};
    hex::encode(Sha256::digest(body.to_string().as_bytes()))
}

async fn replay_stored(
    conn: &mut PgConnection,
    game: Game,
    record_id: Uuid,
    details: Value,
    balance_after: i64,
    body_hash: &str,
) -> Result<Purchase, Error> {
    // The purchase facts of the bean-ledger row (`details.meta`).
    let details = &details["meta"];
    if details["body_sha256"].as_str() != Some(body_hash) {
        return Err(Error::RequestConflict);
    }
    let ids: Vec<Uuid> = serde_json::from_value(details["ticket_ids"].clone()).unwrap_or_default();
    let rows = sqlx::query(&format!(
        "SELECT {TICKET_COLUMNS} FROM lottery_tickets WHERE purchase_id=$1 AND (game<>'scratch' OR (cost=100 AND entry->'reward_version' IN ('1'::jsonb,'2'::jsonb)))"
    ))
    .bind(record_id)
    .fetch_all(conn)
    .await?;
    let mut tickets: Vec<TicketRow> = rows.iter().map(TicketRow::from_row).collect();
    if game == Game::Scratch && tickets.is_empty() {
        return Err(Error::RequestConflict);
    }
    tickets.sort_by_key(|t| ids.iter().position(|id| *id == t.id).unwrap_or(usize::MAX));
    Ok(Purchase {
        purchased: tickets.iter().map(|t| ticket_json(game, t)).collect(),
        charged: details["charged"].as_i64().unwrap_or_default(),
        balance_before: details["balance_before"].as_i64().unwrap_or_default(),
        balance_after: details["accepted_balance_after"]
            .as_i64()
            .unwrap_or(balance_after),
    })
}

/// Purchase preflight in one statement: the ledger row a derived purchase
/// record id already wrote (the same normalised body replays the stored
/// purchase, another body conflicts), plus whether the beans wallet row is
/// already durable (`initial_grant_ready`'s check). A request id is shared
/// across purchase kinds: the first stored record in namespace order decides,
/// so switching to or from a scratch batch conflicts instead of debiting again
/// under the other record namespace.
pub(super) async fn replay_request_and_wallet(
    conn: &mut PgConnection,
    game: Game,
    user: Uuid,
    request_id: Uuid,
    body_hash: &str,
) -> Result<(Option<Purchase>, bool), Error> {
    let ids = [
        super::purchase_id(user, request_id),
        super::scratch_batch_id(user, request_id),
    ];
    let row = sqlx::query(
        "SELECT EXISTS(SELECT 1 FROM wallets WHERE user_id=$3 AND game=$4) AS wallet, \
                EXISTS(SELECT 1 FROM lottery_tickets WHERE purchase_id = ANY($2) AND user_id=$3) AS bought, \
                l.record_id, l.details, l.balance_after \
           FROM (SELECT 1) one \
           LEFT JOIN LATERAL ( \
                SELECT record_id, details, balance_after FROM wallet_ledger \
                 WHERE game = $1 AND record_id = ANY($2) AND user_id=$3 \
                 ORDER BY array_position($2, record_id) LIMIT 1) l ON TRUE",
    )
    .bind(POLICY.game)
    .bind(&ids[..])
    .bind(user)
    .bind(crate::game_catalog::BEANS)
    .fetch_one(&mut *conn)
    .await?;
    let wallet: bool = row.get("wallet");
    let Some(record_id) = row.get::<Option<Uuid>, _>("record_id") else {
        // The tickets are the durable mark of a purchase. One whose ledger
        // row is gone (the ledger was rebuilt) cannot be replayed in detail,
        // but it must never be charged again.
        if row.get::<bool, _>("bought") {
            return Err(Error::RequestConflict);
        }
        return Ok((None, wallet));
    };
    let purchase = replay_stored(
        conn,
        game,
        record_id,
        row.get("details"),
        row.get("balance_after"),
        body_hash,
    )
    .await?;
    Ok((Some(purchase), wallet))
}

pub(super) async fn recent_periods(
    conn: &mut PgConnection,
    game: Game,
    limit: i64,
    offset: i64,
) -> sqlx::Result<Vec<PeriodRow>> {
    let rows = sqlx::query(&format!(
        "SELECT {PERIOD_COLUMNS} FROM lottery_periods WHERE game=$1 AND status <> 'open' \
         ORDER BY drawn_at DESC, period DESC LIMIT $2 OFFSET $3"
    ))
    .bind(game.as_str())
    .bind(limit)
    .bind(offset)
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

/// One feed ticket with its buyer's account row (if it still exists).
#[derive(serde::Deserialize)]
struct FeedRow {
    purchase_id: Uuid,
    user_id: Uuid,
    bought_at: DateTime<Utc>,
    stakes: i32,
    multiplier: i16,
    prize: Option<i64>,
    tier: Option<i16>,
    settled_at: Option<DateTime<Utc>>,
    user_found: bool,
    display_name: Option<String>,
}

impl FeedRow {
    /// An existing account shows its public name; a missing one the opaque
    /// fallback (the former separate `users` lookup's rule).
    fn name(&self) -> String {
        if self.user_found {
            public_name(self.user_id, self.display_name.clone())
        } else {
            crate::display_name::fallback_display_name(self.user_id)
        }
    }
}

struct FeedScan {
    purchases: Vec<Value>,
    /// Scratch: cards that paid, as `won` events.
    wins: Vec<Value>,
}

/// Newest tickets of a game from the feed index, buyer names joined in.
const FEED_SQL: &str = "SELECT t.purchase_id, t.user_id, t.bought_at, t.stakes, t.multiplier, t.prize, t.tier, t.settled_at, \
        u.id IS NOT NULL AS user_found, u.display_name \
   FROM (SELECT purchase_id, user_id, bought_at, stakes, multiplier, prize, tier, settled_at FROM lottery_tickets \
          WHERE game=$1 AND (game<>'scratch' OR (cost=100 AND entry->'reward_version' IN ('1'::jsonb,'2'::jsonb))) \
          ORDER BY bought_at DESC LIMIT $2) t \
   LEFT JOIN users u ON u.id = t.user_id \
  ORDER BY t.bought_at DESC";

fn feed_from_rows(game: Game, rows: &[FeedRow]) -> FeedScan {
    let mut purchases: Vec<(Uuid, &FeedRow, i64, i64)> = Vec::new();
    let mut wins: Vec<(&FeedRow, DateTime<Utc>, i16, i64)> = Vec::new();
    for row in rows {
        let stakes = i64::from(row.stakes) * i64::from(row.multiplier);
        if let Some(p) = purchases.iter_mut().find(|p| p.0 == row.purchase_id) {
            p.2 += 1;
            p.3 += stakes;
        } else if purchases.len() < FEED_LEN {
            purchases.push((row.purchase_id, row, 1, stakes));
        }
        if game == Game::Scratch && wins.len() < FEED_LEN {
            if let (Some(prize), Some(settled)) = (row.prize, row.settled_at) {
                if prize > 0 {
                    wins.push((row, settled, row.tier.unwrap_or(0), prize));
                }
            }
        }
    }
    FeedScan {
        purchases: purchases
            .iter()
            .map(|(_, row, count, stakes)| {
                json!({ "at_ms": row.bought_at.timestamp_millis(), "user_id": row.user_id, "name": row.name(),
                        "kind": "bought", "stakes": stakes, "count": count })
            })
            .collect(),
        wins: wins
            .iter()
            .map(|(row, at, tier, prize)| {
                json!({ "at_ms": at.timestamp_millis(), "user_id": row.user_id, "name": row.name(),
                        "kind": "won", "tier": tier, "prize": prize })
            })
            .collect(),
    }
}

fn merge_feed(mut events: Vec<Value>) -> Vec<Value> {
    events.sort_by_key(|e| std::cmp::Reverse(e["at_ms"].as_i64().unwrap_or_default()));
    events.truncate(FEED_LEN);
    events
}

#[derive(serde::Deserialize)]
struct WinnerRow {
    user_id: Option<Uuid>,
    name: String,
    tier: i16,
    prize: i64,
}

fn draw_value(game: Game, row: &PeriodRow, winners: &[WinnerRow], mine: &[TicketRow]) -> Value {
    let winners: Vec<Value> = winners
        .iter()
        .map(|w| json!({ "user_id": w.user_id, "name": w.name, "tier": w.tier, "prize": w.prize }))
        .collect();
    let label = period_label(game, row.period, row.proof_plan.as_ref());
    json!({
        "period": row.period, "label": label,
        "drawn_at_ms": row.drawn_at.map(|d| d.timestamp_millis()).unwrap_or_default(),
        "status": row.status, "numbers": row.numbers.clone().unwrap_or_default(), "bonus": row.bonus,
        "bonus_numbers": row.bonus_numbers,
        "tickets": row.tickets, "players": row.players, "sales": row.sales, "paid": row.paid,
        "winners_count": row.winners, "winners": winners,
        "proof_plan": row.proof_plan, "draw_proof": row.draw_proof,
        "proof_status": proof_status(row.proof_plan.as_ref(), row.draw_proof.as_ref()),
        "mine": mine.iter().map(|t| ticket_json(game, t)).collect::<Vec<_>>(),
    })
}

pub(super) fn period_json(selling: &Period, row: Option<&PeriodRow>, open: bool) -> Value {
    json!({
        "id": selling.id, "label": selling.label,
        "opens_ms": selling.opens_ms, "closes_ms": selling.closes_ms, "draw_ms": selling.draw_ms,
        "status": if open { "open" } else { "closed" },
        "tickets": row.map_or(0, |r| r.tickets), "stakes": row.map_or(0, |r| r.stakes),
        "players": row.map_or(0, |r| r.players), "sales": row.map_or(0, |r| r.sales),
        "proof_plan": row.and_then(|r| r.proof_plan.as_ref()),
        "proof_status": proof_status(row.and_then(|r| r.proof_plan.as_ref()), row.and_then(|r| r.draw_proof.as_ref())),
    })
}

/// `GET /api/lottery/{game}/draws`.
pub async fn draws(
    pool: &PgPool,
    game: Game,
    limit: i64,
    offset: i64,
) -> Result<Vec<Value>, Error> {
    let mut conn = pool.acquire().await?;
    let rows = recent_periods(&mut conn, game, limit.clamp(1, 100), offset.max(0)).await?;
    Ok(rows.iter().map(|r| r.summary(game)).collect())
}

/// Public, account-free proof retrieval. No player or private ticket data leaves
/// this endpoint. The same URL serves the pre-draw plan and the revealed result.
pub async fn proof(pool: &PgPool, game: Game, period: i64) -> Result<Value, Error> {
    if game.schedule().is_none() {
        return Err(Error::InvalidEntry);
    }
    let row = sqlx::query(
        "SELECT proof_plan,draw_proof FROM lottery_periods WHERE game=$1 AND period=$2",
    )
    .bind(game.as_str())
    .bind(period)
    .fetch_optional(pool)
    .await?
    .ok_or(Error::TicketNotFound)?;
    let plan: Option<Value> = row.get("proof_plan");
    let proof: Option<Value> = row.get("draw_proof");
    Ok(json!({ "game": game.as_str(), "period": period,
        "proof_status": proof_status(plan.as_ref(), proof.as_ref()), "plan": plan, "proof": proof }))
}

/// Filtering happens before pagination, so older wins remain discoverable.
#[derive(Clone, Copy, Default, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TicketFilter {
    #[default]
    All,
    Pending,
    Won,
    Lost,
}

impl TicketFilter {
    fn predicate(self) -> &'static str {
        // Rewards count as wins even when the number-match payout is zero.
        // Unsettled cards must not expose their hidden reward through a filter.
        match self {
            Self::All => "TRUE",
            Self::Pending => "t.settled_at IS NULL",
            Self::Won => "t.settled_at IS NOT NULL AND (COALESCE(t.prize,0)>0 OR (t.game='scratch' AND t.entry->'reward'->>'kind' IN ('beans','membership')) OR (t.entry->'reward_grant' IS NOT NULL AND t.entry->'reward_grant'<>'null'::jsonb))",
            Self::Lost => "t.settled_at IS NOT NULL AND NOT (COALESCE(t.prize,0)>0 OR (t.game='scratch' AND COALESCE(t.entry->'reward'->>'kind','') IN ('beans','membership')) OR (t.entry->'reward_grant' IS NOT NULL AND t.entry->'reward_grant'<>'null'::jsonb))",
        }
    }
}

/// `GET /api/lottery/{game}/tickets`: the viewer's newest tickets with the
/// period result once drawn.
pub async fn tickets(
    pool: &PgPool,
    user: Uuid,
    game: Game,
    limit: i64,
    offset: i64,
    filter: TicketFilter,
) -> Result<Vec<Value>, Error> {
    let rows = sqlx::query(&format!(
        "SELECT t.{} , p.numbers AS result_numbers, p.bonus AS result_bonus, p.bonus_numbers AS result_bonus_numbers, p.draw_proof AS result_proof \
         FROM lottery_tickets t LEFT JOIN lottery_periods p \
           ON p.game = t.game AND p.period = t.period AND p.status <> 'open' \
         WHERE t.user_id=$1 AND t.game=$2 AND (t.game<>'scratch' OR (t.cost=100 AND t.entry->'reward_version' IN ('1'::jsonb,'2'::jsonb))) AND ({}) ORDER BY t.bought_at DESC, t.id LIMIT $3 OFFSET $4",
        TICKET_COLUMNS.replace(", ", ", t."),
        filter.predicate()
    ))
    .bind(user)
    .bind(game.as_str())
    .bind(limit.clamp(1, 101))
    .bind(offset.max(0))
    .fetch_all(pool)
    .await?;
    Ok(rows
        .iter()
        .map(|r| {
            let mut ticket = ticket_json(game, &TicketRow::from_row(r));
            let numbers: Option<Vec<i16>> = r.get("result_numbers");
            if game != Game::Scratch {
                let draw_proof: Option<Value> = r.get("result_proof");
                ticket["draw_proof"] = json!(draw_proof);
                ticket["proof_status"] = json!(proof_status(
                    ticket.get("proof_plan").filter(|p| !p.is_null()), draw_proof.as_ref()));
            }
            ticket["result"] = match numbers {
                Some(numbers) => {
                    json!({ "numbers": numbers, "bonus": r.get::<Option<i16>, _>("result_bonus"),
                        "bonus_numbers": r.get::<Option<Vec<i16>>, _>("result_bonus_numbers").unwrap_or_default() })
                }
                None => Value::Null,
            };
            ticket
        })
        .collect())
}

/// The hall's history in one statement: feed tickets with buyer names, the
/// recent draws, and the newest draw's winners board and the viewer's own
/// tickets in it (each part as one JSON array, decoded by the same row types).
///
/// Perf (2026-10-30): this was five sequential statements on one connection.
/// The hall view is rarely polled, so it usually lands on a pooled backend
/// that has not prepared them (prod 12 ms server time); one prepare, one plan.
fn history_sql() -> &'static str {
    static SQL: std::sync::OnceLock<String> = std::sync::OnceLock::new();
    SQL.get_or_init(|| {
        format!(
            "WITH feed AS ({FEED_SQL}),
             recent AS (
                SELECT {PERIOD_COLUMNS} FROM lottery_periods
                 WHERE game=$1 AND status <> 'open'
                 ORDER BY drawn_at DESC, period DESC LIMIT $3
             ), latest AS (
                SELECT period FROM recent ORDER BY drawn_at DESC, period DESC LIMIT 1
             )
             SELECT
               COALESCE((SELECT jsonb_agg(to_jsonb(f) ORDER BY f.bought_at DESC) FROM feed f), '[]'::jsonb) AS feed,
               COALESCE((SELECT jsonb_agg(to_jsonb(r) ORDER BY r.drawn_at DESC, r.period DESC) FROM recent r), '[]'::jsonb) AS recent,
               COALESCE((SELECT jsonb_agg(to_jsonb(w) ORDER BY w.prize DESC)
                           FROM (SELECT user_id, name, tier, prize FROM lottery_winners
                                  WHERE game=$1 AND period=(SELECT period FROM latest)
                                  ORDER BY prize DESC LIMIT $4) w), '[]'::jsonb) AS winners,
               COALESCE((SELECT jsonb_agg(to_jsonb(t) ORDER BY t.bought_at DESC, t.id)
                           FROM (SELECT {TICKET_COLUMNS} FROM lottery_tickets
                                  WHERE game=$1 AND period=(SELECT period FROM latest) AND user_id=$5) t), '[]'::jsonb) AS mine"
        )
    })
}

fn decode<T: serde::de::DeserializeOwned>(value: Value) -> sqlx::Result<T> {
    serde_json::from_value(value).map_err(|error| sqlx::Error::Decode(Box::new(error)))
}

/// History never joins the runtime's resident working set.
pub(super) async fn decorate_history(
    pool: &PgPool,
    user: Uuid,
    game: Game,
    view: &mut Value,
) -> Result<(), Error> {
    // Scratch has no draws: a zero limit keeps the period parts empty.
    let recent_limit = if game == Game::Scratch {
        0
    } else {
        RECENT_DRAWS
    };
    let row = sqlx::query(history_sql())
        .bind(game.as_str())
        .bind(FEED_SCAN)
        .bind(recent_limit)
        .bind(WINNERS_BOARD)
        .bind(user)
        .fetch_one(pool)
        .await?;
    let rows: Vec<FeedRow> = decode(row.get("feed"))?;
    let scan = feed_from_rows(game, &rows);
    let mut feed = scan.purchases;
    if game == Game::Scratch {
        feed.extend(scan.wins);
    } else {
        let recent: Vec<PeriodRow> = decode(row.get("recent"))?;
        view["recent"] = json!(recent.iter().map(|r| r.summary(game)).collect::<Vec<_>>());
        if let Some(last) = recent.first() {
            let winners: Vec<WinnerRow> = decode(row.get("winners"))?;
            let mine: Vec<TicketRow> = decode(row.get("mine"))?;
            let draw = draw_value(game, last, &winners, &mine);
            if let Some(winners) = draw["winners"].as_array() {
                feed.extend(winners.iter().map(|w| json!({"at_ms":draw["drawn_at_ms"],"user_id":w["user_id"],"name":w["name"],"kind":"won","tier":w["tier"],"prize":w["prize"],"period_label":draw["label"]})));
            }
            view["last_draw"] = draw;
        }
    }
    view["feed"] = json!(merge_feed(feed));
    Ok(())
}
