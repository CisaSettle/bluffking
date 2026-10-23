//! Persisted scratch cards. Hidden numbers are projected out of every response.
use super::{
    handlers::Error,
    store::{self, Purchase, TicketRow, TICKET_COLUMNS},
    Game, POLICY,
};
use crate::wallet;
use chrono::Utc;
use rand::{rngs::OsRng, Rng};
use serde_json::{json, Value};
use sqlx::{PgConnection, PgPool, Row};
use uuid::Uuid;

const PRIZES: [i64; 6] = [1, 1, 2, 2, 5, 10];

#[derive(serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ScratchBuy {
    pub request_id: Uuid,
    pub cost: i64,
    pub count: usize,
}

pub fn project(out: &mut Value, ticket: &TicketRow) {
    let revealed = ticket.revealed.clone().unwrap_or_else(|| vec![false; 6]);
    let numbers: Vec<Value> = (0..6)
        .map(|i| {
            if revealed.get(i) == Some(&true) {
                ticket.entry["numbers"][i].clone()
            } else {
                Value::Null
            }
        })
        .collect();
    // Replace both copies: never leave the persisted secret inside `entry`.
    out["entry"] = json!({"cost": ticket.cost, "lucky": ticket.entry["lucky"], "numbers": numbers});
    out["lucky"] = ticket.entry["lucky"].clone();
    out["numbers"] = json!(numbers);
    out["revealed"] = json!(revealed);
}

fn prize(entry: &Value, cost: i64) -> (i64, i16) {
    let mut total = 0;
    let mut hits = 0;
    for (i, factor) in PRIZES.iter().enumerate() {
        if entry["numbers"][i] == entry["lucky"] {
            total += cost * factor;
            hits += 1;
        }
    }
    (total, hits)
}

pub async fn buy(pool: &PgPool, user: Uuid, body: &ScratchBuy) -> Result<Purchase, Error> {
    if body.request_id.is_nil()
        || ![10, 50, 100].contains(&body.cost)
        || ![1, 5, 10].contains(&body.count)
    {
        return Err(Error::InvalidEntry);
    }
    let batch = body.count > 1;
    let record_id = if batch {
        super::scratch_batch_id(user, body.request_id)
    } else {
        super::purchase_id(user, body.request_id)
    };
    let hash = store::body_sha256(&json!({"game":"scratch", "cost":body.cost, "count":body.count}));
    store::ensure_wallet(pool, user).await?;
    {
        let mut conn = pool.acquire().await?;
        if let Some(p) = replay(&mut conn, user, body.request_id, &hash).await? {
            return Ok(p);
        }
    }
    let mut tx = pool.begin().await?;
    crate::db::lock_active_users(&mut tx, &[user]).await?;
    let before = wallet::lock(&mut tx, POLICY.game, user)
        .await?
        .ok_or(sqlx::Error::RowNotFound)?;
    if let Some(p) = replay(&mut tx, user, body.request_id, &hash).await? {
        tx.commit().await?;
        return Ok(p);
    }
    store::require_spendable(&mut tx, user).await?;
    if !batch {
        let count: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM lottery_tickets WHERE game='scratch' AND period=0 AND user_id=$1 AND settled_at IS NULL")
            .bind(user).fetch_one(&mut *tx).await?;
        if count >= 50 {
            return Err(Error::TooManyTickets);
        }
    }
    let charged = body.cost * body.count as i64;
    if charged > before {
        return Err(Error::InsufficientBalance);
    }
    let now = Utc::now();
    let mut cards = Vec::with_capacity(body.count);
    for _ in 0..body.count {
        let numbers: Vec<i16> = (0..6).map(|_| OsRng.gen_range(1..=30)).collect();
        let entry =
            json!({"cost":body.cost, "lucky":OsRng.gen_range(1..=30i16), "numbers":numbers});
        let (paid, tier) = prize(&entry, body.cost);
        cards.push(TicketRow {
            id: Uuid::new_v4(),
            purchase_id: record_id,
            period: 0,
            bought_at: now,
            entry,
            stakes: 1,
            multiplier: 1,
            cost: body.cost,
            revealed: Some(vec![batch; 6]),
            prize: batch.then_some(paid),
            tier: batch.then_some(tier),
            settled_at: batch.then_some(now),
        });
    }
    let paid: i64 = cards.iter().filter_map(|t| t.prize).sum();
    let ids: Vec<Uuid> = cards.iter().map(|t| t.id).collect();
    let after = wallet::settle(&mut tx, POLICY.game, user, &wallet::Play {
        record_id, delta: paid - charged, completes_hand: batch, won: paid > 0,
        room_code: None, revision: None, hand_completed: None, has_bots: None,
        details: Some(json!({"purpose":if batch {"scratch_batch"} else {"purchase"}, "game":"scratch", "body_sha256":hash, "ticket_ids":ids, "charged":charged, "balance_before":before})),
    }).await?;
    for t in &cards {
        sqlx::query("INSERT INTO lottery_tickets (id,purchase_id,game,period,user_id,bought_at,entry,stakes,multiplier,cost,revealed,prize,tier,settled_at) VALUES ($1,$2,'scratch',0,$3,$4,$5,1,1,$6,$7,$8,$9,$10)")
            .bind(t.id).bind(record_id).bind(user).bind(now).bind(&t.entry).bind(t.cost).bind(&t.revealed).bind(t.prize).bind(t.tier).bind(t.settled_at)
            .execute(&mut *tx).await?;
    }
    tx.commit().await?;
    crate::stats_writer::send(crate::stats_writer::StatsEvent::LotteryPurchase {
        day: super::sgt_day(now),
        game: "scratch",
        user,
        tickets: body.count as i64,
        stakes: body.count as i64,
        sales: charged,
    });
    if batch {
        emit_paid(user, paid);
    }
    Ok(Purchase {
        purchased: cards
            .iter()
            .map(|t| store::ticket_json(Game::Scratch, t))
            .collect(),
        charged,
        balance_before: before,
        balance_after: after,
    })
}

// The same request id cannot be reused with another quantity (and therefore namespace).
async fn replay(
    conn: &mut PgConnection,
    user: Uuid,
    request: Uuid,
    hash: &str,
) -> Result<Option<Purchase>, Error> {
    for id in [
        super::purchase_id(user, request),
        super::scratch_batch_id(user, request),
    ] {
        if let Some(p) = store::replay(conn, Game::Scratch, user, id, hash).await? {
            return Ok(Some(p));
        }
    }
    Ok(None)
}

pub async fn my_cards(conn: &mut PgConnection, user: Uuid) -> sqlx::Result<Vec<TicketRow>> {
    let rows = sqlx::query(&format!("SELECT {TICKET_COLUMNS} FROM lottery_tickets WHERE game='scratch' AND period=0 AND user_id=$1 AND settled_at IS NULL UNION SELECT {TICKET_COLUMNS} FROM lottery_tickets WHERE purchase_id=(SELECT purchase_id FROM lottery_tickets WHERE game='scratch' AND user_id=$1 ORDER BY bought_at DESC,id DESC LIMIT 1) ORDER BY bought_at DESC,id"))
        .bind(user).fetch_all(conn).await?;
    Ok(rows.iter().map(TicketRow::from_row).collect())
}

pub async fn reveal(
    pool: &PgPool,
    user: Uuid,
    ticket: Uuid,
    index: Option<usize>,
) -> Result<(Value, i64), Error> {
    reveal_inner(pool, user, ticket, index, false).await
}

async fn reveal_inner(
    pool: &PgPool,
    user: Uuid,
    ticket: Uuid,
    index: Option<usize>,
    expiry: bool,
) -> Result<(Value, i64), Error> {
    if index.is_some_and(|i| i >= 6) {
        return Err(Error::InvalidEntry);
    }
    let mut tx = pool.begin().await?;
    if expiry {
        sqlx::query("SELECT set_config('lock_timeout','500ms',true),set_config('statement_timeout','2s',true)")
            .execute(&mut *tx)
            .await?;
    }
    // Lock the account before its records, matching account purge. No period for scratch.
    let live = crate::db::lock_live_accounts(&mut tx, &[user]).await?;
    if live.is_empty() && !expiry {
        return Err(Error::TicketNotFound);
    }
    let balance = if live.is_empty() {
        None
    } else {
        wallet::lock(&mut tx, POLICY.game, user).await?
    };
    let row = sqlx::query(&format!("SELECT {TICKET_COLUMNS} FROM lottery_tickets WHERE id=$1 AND game='scratch' AND user_id=$2 FOR UPDATE"))
        .bind(ticket).bind(user).fetch_optional(&mut *tx).await?.ok_or(Error::TicketNotFound)?;
    let mut card = TicketRow::from_row(&row);
    if card.settled_at.is_some() {
        return Err(Error::AlreadySettled);
    }
    let mut shown = card.revealed.take().unwrap_or_else(|| vec![false; 6]);
    shown.resize(6, false);
    if let Some(i) = index {
        shown[i] = true;
    } else {
        shown.fill(true);
    }
    let all = shown.iter().all(|v| *v);
    let mut after = balance.unwrap_or(0);
    let mut paid = 0;
    if all {
        let (amount, tier) = if balance.is_some() {
            prize(&card.entry, card.cost)
        } else {
            (0, 0)
        };
        paid = amount;
        if balance.is_some() {
            after = wallet::settle(
                &mut tx,
                POLICY.game,
                user,
                &wallet::Play {
                    record_id: super::scratch_credit_id(card.id),
                    delta: paid,
                    completes_hand: true,
                    won: paid > 0,
                    room_code: None,
                    revision: None,
                    hand_completed: None,
                    has_bots: None,
                    details: Some(
                        json!({"purpose":"scratch", "ticket_id":card.id, "prize":paid,"tier":tier}),
                    ),
                },
            )
            .await?;
        }
        card.prize = Some(paid);
        card.tier = Some(tier);
        card.settled_at = Some(Utc::now());
    }
    card.revealed = Some(shown);
    sqlx::query(
        "UPDATE lottery_tickets SET revealed=$2,prize=$3,tier=$4,settled_at=$5 WHERE id=$1",
    )
    .bind(card.id)
    .bind(&card.revealed)
    .bind(card.prize)
    .bind(card.tier)
    .bind(card.settled_at)
    .execute(&mut *tx)
    .await?;
    tx.commit().await?;
    if all && balance.is_some() {
        emit_paid(user, paid);
    }
    Ok((store::ticket_json(Game::Scratch, &card), after))
}

fn emit_paid(user: Uuid, paid: i64) {
    crate::stats_writer::send(crate::stats_writer::StatsEvent::LotteryPaid {
        day: super::sgt_day(Utc::now()),
        game: "scratch",
        user,
        paid,
    });
}

/// Bounded expiry work: never delete an unfinished card before its payout.
pub async fn expire(pool: &PgPool) -> Result<(), Error> {
    let rows = sqlx::query("SELECT id,user_id FROM lottery_tickets WHERE game='scratch' AND settled_at IS NULL AND bought_at < now()-interval '30 days' ORDER BY bought_at,id LIMIT 20")
        .fetch_all(pool).await?;
    for row in rows {
        match reveal_inner(pool, row.get("user_id"), row.get("id"), None, true).await {
            Ok(_) | Err(Error::TicketNotFound | Error::AlreadySettled) => {}
            Err(error) => return Err(error),
        }
    }
    Ok(())
}
