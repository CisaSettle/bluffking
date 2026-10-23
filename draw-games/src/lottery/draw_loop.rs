//! Database-arbitrated draws and re-entrant, one-user settlements.
use super::{
    handlers::Error,
    rules::Entry,
    store::{self, TicketRow, TICKET_COLUMNS},
    Game, POLICY,
};
use crate::wallet;
use chrono::{DateTime, Utc};
use serde_json::json;
use sqlx::{PgPool, Row};
use std::time::{Duration, Instant};
use uuid::Uuid;

/// Every backend may run this loop; row locks arbitrate all changes.
pub async fn run(pool: PgPool) {
    let mut tick = tokio::time::interval(Duration::from_secs(1));
    tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    let mut retention_due = Instant::now();
    loop {
        tick.tick().await;
        if !crate::write_behind::accepting() {
            break;
        }
        // Finish old accounting before producing any new draws.
        for _ in 0..20 {
            match settle_one(&pool).await {
                Ok(true) => {}
                Ok(false) => break,
                Err(error) => {
                    tracing::warn!(?error, "lottery settlement failed");
                    break;
                }
            }
        }
        for game in Game::DRAWN {
            if let Err(error) = draw_one(&pool, game).await {
                tracing::warn!(?error, game = game.as_str(), "lottery draw failed");
            }
        }
        if Instant::now() >= retention_due {
            retention_due = Instant::now() + Duration::from_secs(3600);
            if let Err(error) = retention(&pool).await {
                tracing::warn!(?error, "lottery retention failed");
            }
        }
    }
}

async fn draw_one(pool: &PgPool, game: Game) -> Result<(), Error> {
    let schedule = game.schedule().ok_or(Error::InvalidEntry)?;
    let latest = schedule.period(schedule.period_at(super::now_ms()).id - 1);
    let mut tx = pool.begin().await?;
    // Contention aborts this attempt; committed tickets remain queued for the next tick.
    sqlx::query(
        "SELECT set_config('lock_timeout','500ms',true),set_config('statement_timeout','2s',true)",
    )
    .execute(&mut *tx)
    .await?;
    // Only current-1 can be created here: retention can never regenerate old draws.
    sqlx::query("INSERT INTO lottery_periods (game,period,opens_at,draw_at) VALUES ($1,$2,$3,$4) ON CONFLICT (game,period) DO NOTHING")
        .bind(game.as_str()).bind(latest.id)
        .bind(DateTime::from_timestamp_millis(latest.opens_ms).ok_or(Error::InvalidEntry)?)
        .bind(DateTime::from_timestamp_millis(latest.draw_ms).ok_or(Error::InvalidEntry)?)
        .execute(&mut *tx).await?;
    let period: Option<i64> = sqlx::query_scalar("SELECT period FROM lottery_periods WHERE game=$1 AND status <> 'settled' AND status='open' AND draw_at<=now() AND (tickets>0 OR period=$2) ORDER BY period LIMIT 1 FOR UPDATE SKIP LOCKED")
        .bind(game.as_str()).bind(latest.id).fetch_optional(&mut *tx).await?;
    if let Some(period) = period {
        let (numbers, bonus, bonus_numbers) = super::rules::draw(game);
        sqlx::query("UPDATE lottery_periods SET status='drawn',numbers=$3,bonus=$4,bonus_numbers=$5,drawn_at=now() WHERE game=$1 AND period=$2 AND status='open'")
            .bind(game.as_str()).bind(period).bind(numbers).bind(bonus).bind(bonus_numbers).execute(&mut *tx).await?;
    }
    tx.commit().await?;
    Ok(())
}

async fn settle_one(pool: &PgPool) -> Result<bool, Error> {
    // One small index probe per scheduled game, then choose the oldest period.
    let games: Vec<&str> = Game::DRAWN.iter().map(|game| game.as_str()).collect();
    let work = sqlx::query("SELECT p.game,p.period FROM UNNEST($1::text[]) AS g(game) CROSS JOIN LATERAL (SELECT game,period,draw_at FROM lottery_periods WHERE game=g.game AND status<>'settled' AND status='drawn' ORDER BY period LIMIT 1) p ORDER BY p.draw_at,p.game LIMIT 1")
        .bind(games).fetch_optional(pool).await?;
    let Some(work) = work else {
        return Ok(false);
    };
    let name: String = work.get("game");
    let game = Game::parse(&name).ok_or(Error::InvalidEntry)?;
    let period: i64 = work.get("period");
    let mut tx = pool.begin().await?;
    // Contention aborts this attempt; committed tickets remain queued for the next tick.
    sqlx::query(
        "SELECT set_config('lock_timeout','500ms',true),set_config('statement_timeout','2s',true)",
    )
    .execute(&mut *tx)
    .await?;
    let row = sqlx::query("SELECT numbers,bonus,bonus_numbers FROM lottery_periods WHERE game=$1 AND period=$2 AND status='drawn' FOR UPDATE SKIP LOCKED")
        .bind(game.as_str()).bind(period).fetch_optional(&mut *tx).await?;
    let Some(row) = row else {
        return Ok(false);
    };
    let numbers: Vec<i16> = row.get("numbers");
    let bonus: Option<i16> = row.get("bonus");
    let bonus_numbers: Vec<i16> = row.get("bonus_numbers");
    let user:Option<Uuid> = sqlx::query_scalar("SELECT user_id FROM lottery_tickets WHERE game=$1 AND period=$2 AND settled_at IS NULL ORDER BY user_id,id LIMIT 1")
        .bind(game.as_str()).bind(period).fetch_optional(&mut *tx).await?;
    let Some(user) = user else {
        sqlx::query("UPDATE lottery_periods SET status='settled',settled_at=now() WHERE game=$1 AND period=$2")
            .bind(game.as_str()).bind(period).execute(&mut *tx).await?;
        tx.commit().await?;
        return Ok(true);
    };
    let live = crate::db::lock_live_accounts(&mut tx, &[user]).await?;
    let balance = if live.is_empty() {
        None
    } else {
        wallet::lock(&mut tx, POLICY.game, user).await?
    };
    // Re-read after account lock: purge may have removed tickets while we waited.
    let rows = sqlx::query(&format!("SELECT {TICKET_COLUMNS} FROM lottery_tickets WHERE game=$1 AND period=$2 AND user_id=$3 AND settled_at IS NULL ORDER BY id FOR UPDATE"))
        .bind(game.as_str()).bind(period).bind(user).fetch_all(&mut *tx).await?;
    if rows.is_empty() {
        tx.commit().await?;
        return Ok(true);
    }
    let mut tickets: Vec<TicketRow> = rows.iter().map(TicketRow::from_row).collect();
    let mut paid = 0i64;
    let mut best = 0i16;
    let now = Utc::now();
    for ticket in &mut tickets {
        let (prize, tier) = if balance.is_some() {
            Entry::from_stored(game, &ticket.entry)
                .and_then(|e| {
                    e.prize(
                        i64::from(ticket.multiplier),
                        &numbers,
                        bonus,
                        &bonus_numbers,
                    )
                })
                .ok_or(Error::InvalidEntry)?
        } else {
            (0, 0)
        };
        paid = paid.checked_add(prize).ok_or(Error::InvalidEntry)?;
        if prize > 0 {
            best = if game == Game::Happy8 {
                best.max(tier)
            } else if best == 0 {
                tier
            } else {
                best.min(tier)
            };
        }
        ticket.prize = Some(prize);
        ticket.tier = Some(tier);
        ticket.settled_at = Some(now);
    }
    let record_id = super::draw_credit_id(game, period, user);
    if balance.is_some() {
        wallet::settle(&mut tx,POLICY.game,user,&wallet::Play {
            record_id,delta:paid,completes_hand:true,won:paid>0,room_code:None,revision:None,hand_completed:None,has_bots:None,
            details:Some(json!({"purpose":"draw","game":game.as_str(),"period":period,"tickets":tickets.iter().map(|t|json!({"id":t.id,"prize":t.prize,"tier":t.tier})).collect::<Vec<_>>()})),
        }).await?;
    }
    let ids: Vec<Uuid> = tickets.iter().map(|t| t.id).collect();
    let prizes: Vec<i64> = tickets.iter().map(|t| t.prize.unwrap_or(0)).collect();
    let tiers: Vec<i16> = tickets.iter().map(|t| t.tier.unwrap_or(0)).collect();
    sqlx::query("UPDATE lottery_tickets t SET prize=u.prize,tier=u.tier,settled_at=$4 FROM UNNEST($1::uuid[],$2::int8[],$3::int2[]) AS u(id,prize,tier) WHERE t.id=u.id")
        .bind(ids).bind(prizes).bind(tiers).bind(now).execute(&mut *tx).await?;
    if paid > 0 {
        let display: Option<String> =
            sqlx::query_scalar("SELECT display_name FROM users WHERE id=$1")
                .bind(user)
                .fetch_one(&mut *tx)
                .await?;
        sqlx::query("INSERT INTO lottery_winners (id,game,period,user_id,name,tier,prize) VALUES ($1,$2,$3,$4,$5,$6,$7)")
            .bind(record_id).bind(game.as_str()).bind(period).bind(user).bind(store::public_name(user,display)).bind(best).bind(paid).execute(&mut *tx).await?;
    }
    sqlx::query(
        "UPDATE lottery_periods SET paid=paid+$3,winners=winners+$4 WHERE game=$1 AND period=$2",
    )
    .bind(game.as_str())
    .bind(period)
    .bind(paid)
    .bind(i32::from(paid > 0))
    .execute(&mut *tx)
    .await?;
    tx.commit().await?;
    if balance.is_some() {
        crate::stats_writer::send(crate::stats_writer::StatsEvent::LotteryPaid {
            day: super::sgt_day(now),
            game: game.as_str(),
            user,
            paid,
        });
    }
    Ok(true)
}

async fn retention(pool: &PgPool) -> Result<(), Error> {
    super::scratch::expire(pool).await?;
    // Each statement removes at most 1,000 rows; winners go before their period.
    for statement in [
        "DELETE FROM lottery_tickets WHERE id IN (SELECT id FROM lottery_tickets WHERE settled_at < now()-interval '30 days' ORDER BY settled_at LIMIT 1000 FOR UPDATE SKIP LOCKED)",
        "DELETE FROM lottery_winners WHERE ctid IN (SELECT w.ctid FROM lottery_periods p JOIN lottery_winners w USING (game,period) WHERE p.status='settled' AND p.settled_at < now()-interval '30 days' ORDER BY p.settled_at LIMIT 1000 FOR UPDATE OF w SKIP LOCKED)",
        "DELETE FROM lottery_periods WHERE (game,period) IN (SELECT p.game,p.period FROM lottery_periods p WHERE status='settled' AND settled_at < now()-interval '30 days' AND NOT EXISTS (SELECT 1 FROM lottery_winners w WHERE w.game=p.game AND w.period=p.period) ORDER BY settled_at LIMIT 1000 FOR UPDATE SKIP LOCKED)",
        "DELETE FROM lottery_presence WHERE (game,user_id) IN (SELECT game,user_id FROM lottery_presence WHERE seen_at < now()-interval '1 hour' ORDER BY seen_at LIMIT 1000 FOR UPDATE SKIP LOCKED)",
        "DELETE FROM game_room_checkpoints WHERE ctid IN (SELECT ctid FROM game_room_checkpoints WHERE game='scratch' AND (updated_at < now()-interval '7 days' OR (updated_at < now()-interval '1 day' AND state->>'closed'='true')) ORDER BY updated_at LIMIT 1000 FOR UPDATE SKIP LOCKED)",
     ] {
        let mut tx = pool.begin().await?;
        sqlx::query("SELECT set_config('lock_timeout','500ms',true),set_config('statement_timeout','2s',true)")
            .execute(&mut *tx).await?;
        sqlx::query(statement).execute(&mut *tx).await?;
        tx.commit().await?;
    }
    store::prune_presence_throttle();
    Ok(())
}
