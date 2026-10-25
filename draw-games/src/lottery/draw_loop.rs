//! Database-arbitrated draws and re-entrant, one-user settlements.
use super::{
    handlers::Error,
    rules::Entry,
    store::{self, TicketRow, TICKET_COLUMNS},
    Game, POLICY,
};
use crate::wallet;
use chrono::Utc;
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
    let current = schedule.period_at(super::now_ms());
    // Persist the plan while this period is selling, even when its hall has no
    // visitors. Never create an already elapsed period with a retrospective plan.
    {
        let mut conn = pool.acquire().await?;
        store::ensure_period(&mut conn, game, &current).await?;
    }
    let candidate = sqlx::query("SELECT period,proof_plan FROM lottery_periods WHERE game=$1 AND status <> 'settled' AND status='open' AND draw_at<=now() AND proof_plan IS NOT NULL ORDER BY period LIMIT 1")
        .bind(game.as_str()).fetch_optional(pool).await?;
    let Some(candidate) = candidate else {
        return Ok(());
    };
    let period: i64 = candidate.get("period");
    let plan_value: Option<serde_json::Value> = candidate.get("proof_plan");
    // HTTP and BLS verification run before taking a transaction, account lock,
    // wallet lock or period lock. No replacement pulse or OS-random fallback is
    // permitted for a planned period; an outage leaves exactly this draw queued.
    let proof = {
        let value = plan_value
            .as_ref()
            .ok_or_else(|| Error::Proof("missing draw plan".into()))?;
        let plan: draw_games::lottery::Plan = serde_json::from_value(value.clone())
            .map_err(|_| Error::Proof("invalid stored draw plan".into()))?;
        plan.validate().map_err(Error::Proof)?;
        if plan.game != game.as_str() || plan.period != period {
            return Err(Error::Proof("draw plan belongs to another period".into()));
        }
        let Some(beacon) = super::beacon::fetch(plan.beacon_round)
            .await
            .map_err(Error::Proof)?
        else {
            return Ok(());
        };
        draw_games::lottery::DrawProof::new(plan, beacon).map_err(Error::Proof)?
    };
    let mut tx = pool.begin().await?;
    sqlx::query(
        "SELECT set_config('lock_timeout','500ms',true),set_config('statement_timeout','2s',true)",
    )
    .execute(&mut *tx)
    .await?;
    let row = sqlx::query("SELECT proof_plan FROM lottery_periods WHERE game=$1 AND period=$2 AND status='open' AND draw_at<=now() FOR UPDATE SKIP LOCKED")
        .bind(game.as_str()).bind(period).fetch_optional(&mut *tx).await?;
    let Some(row) = row else {
        return Ok(());
    };
    if row.get::<Option<serde_json::Value>, _>("proof_plan") != plan_value {
        return Err(Error::Proof(
            "draw plan changed while awaiting its beacon".into(),
        ));
    }
    sqlx::query("UPDATE lottery_periods SET status='drawn',numbers=$3,bonus=$4,bonus_numbers=$5,drawn_at=now(),draw_proof=$6 WHERE game=$1 AND period=$2 AND status='open'")
        .bind(game.as_str()).bind(period).bind(&proof.numbers).bind(proof.bonus).bind(&proof.bonus_numbers)
        .bind(json!(proof)).execute(&mut *tx).await?;
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
    let mut membership_granted = false;
    let now = Utc::now();
    for ticket in &mut tickets {
        let (prize, tier) = if balance.is_some() {
            let entry = Entry::from_stored(game, &ticket.entry).ok_or(Error::InvalidEntry)?;
            let (prize, tier) = entry
                .prize(
                    i64::from(ticket.multiplier),
                    &numbers,
                    bonus,
                    &bonus_numbers,
                )
                .ok_or(Error::InvalidEntry)?;
            if entry.awards_max(prize, tier) {
                let grant =
                    crate::game_rewards::grant_max_on(&mut tx, user, game.as_str(), ticket.id)
                        .await?;
                ticket.entry["reward_grant"] = json!(grant);
                membership_granted = true;
            }
            (prize, tier)
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
    let entries: Vec<serde_json::Value> = tickets.iter().map(|t| t.entry.clone()).collect();
    sqlx::query("UPDATE lottery_tickets t SET prize=u.prize,tier=u.tier,entry=u.entry,settled_at=$5 FROM UNNEST($1::uuid[],$2::int8[],$3::int2[],$4::jsonb[]) AS u(id,prize,tier,entry) WHERE t.id=u.id")
        .bind(ids).bind(prizes).bind(tiers).bind(entries).bind(now).execute(&mut *tx).await?;
    if paid > 0 {
        let display: Option<String> =
            sqlx::query_scalar("SELECT display_name FROM users WHERE id=$1")
                .bind(user)
                .fetch_one(&mut *tx)
                .await?;
        sqlx::query("INSERT INTO lottery_winners (id,game,period,user_id,name,tier,prize) VALUES ($1,$2,$3,$4,$5,$6,$7)")
            .bind(record_id).bind(game.as_str()).bind(period).bind(user).bind(store::public_name(user,display)).bind(best).bind(paid).execute(&mut *tx).await?;
        notify_winner(&mut tx, user, game, period, paid, &tickets).await?;
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
    if paid > 0 {
        crate::read_cache::invalidate_unread_count(pool, user);
    }
    if membership_granted {
        crate::entitlement::invalidate_tier(pool, user);
    }
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

/// A winner's inbox receipt commits atomically with the credited prizes. The
/// deterministic identity and unique per-user key also make replay harmless.
async fn notify_winner(
    conn: &mut sqlx::PgConnection,
    user: Uuid,
    game: Game,
    period: i64,
    paid: i64,
    tickets: &[TicketRow],
) -> sqlx::Result<()> {
    let (zh_game, en_game) = match game {
        Game::Ssq => ("双色球", "Double Color Ball"),
        Game::Dlt => ("超级大乐透", "Super Lotto"),
        Game::Toto => ("TOTO", "TOTO"),
        Game::Happy8 => ("快乐8", "Happy 8"),
        Game::Fucai3d => ("福彩3D", "Welfare 3D"),
        Game::Scratch => return Ok(()),
    };
    let mut grant_count = 0i64;
    let mut starts_at: Option<i64> = None;
    let mut ends_at: Option<i64> = None;
    for ticket in tickets {
        let grant = &ticket.entry["reward_grant"];
        if let (Some(start), Some(end)) =
            (grant["starts_at_ms"].as_i64(), grant["ends_at_ms"].as_i64())
        {
            grant_count += 1;
            starts_at = Some(starts_at.map_or(start, |previous| previous.min(start)));
            ends_at = Some(ends_at.map_or(end, |previous| previous.max(end)));
        }
    }
    let days = grant_count * 30;
    let membership = if grant_count == 0 {
        serde_json::Value::Null
    } else {
        json!({
            "tier": "max", "days": days, "grants": grant_count,
            "starts_at_ms": starts_at, "ends_at_ms": ends_at,
        })
    };
    let zh_title = format!("{zh_game}中奖奖励已到账");
    let en_title = format!("{en_game} winnings credited");
    let mut zh_body = format!("本期中奖 {paid} 金豆已自动到账，无需领取。");
    let mut en_body = format!(
        "Your {paid} gold beans from this draw were credited automatically. No claim needed."
    );
    if days > 0 {
        zh_body.push_str(&format!("另获 Max 会员 {days} 天，已自动增加到会员期限。"));
        en_body.push_str(&format!(
            " An additional {days} days of Max membership were added to your membership term."
        ));
    }
    let key = format!("lottery-win:{}", super::draw_credit_id(game, period, user));
    sqlx::query(
        "INSERT INTO notifications (id,user_id,type,title,body,payload,dedupe_key) \
         VALUES ($1,$2,'rank',$3,$4,$5,$6) \
         ON CONFLICT (user_id,dedupe_key) WHERE dedupe_key IS NOT NULL DO NOTHING",
    )
    .bind(super::record_id(&key))
    .bind(user)
    .bind(&zh_title)
    .bind(&zh_body)
    .bind(json!({
        "lottery": {
            "game": game.as_str(), "period": period, "prize": paid, "bonus_beans": 0,
            "credited_beans": paid,
            "ticket_count": tickets.len(),
            "winning_tickets": tickets.iter().filter(|t| t.prize.unwrap_or(0) > 0).count(),
            "membership": membership,
        },
        "i18n": {"zh": {"title": zh_title, "body": zh_body}, "en": {"title": en_title, "body": en_body}},
    }))
    .bind(key)
    .execute(conn)
    .await?;
    Ok(())
}

async fn retention(pool: &PgPool) -> Result<(), Error> {
    super::scratch::expire(pool).await?;
    // Each statement removes at most 1,000 rows; winners go before their period.
    for statement in [
        "DELETE FROM lottery_tickets WHERE id IN (SELECT id FROM lottery_tickets WHERE settled_at < now()-interval '30 days' ORDER BY settled_at LIMIT 1000 FOR UPDATE SKIP LOCKED)",
        "DELETE FROM lottery_winners WHERE ctid IN (SELECT w.ctid FROM lottery_periods p JOIN lottery_winners w USING (game,period) WHERE p.status='settled' AND p.settled_at < now()-interval '30 days' ORDER BY p.settled_at LIMIT 1000 FOR UPDATE OF w SKIP LOCKED)",
        "DELETE FROM lottery_periods WHERE (game,period) IN (SELECT p.game,p.period FROM lottery_periods p WHERE status='settled' AND settled_at < now()-interval '30 days' AND NOT EXISTS (SELECT 1 FROM lottery_winners w WHERE w.game=p.game AND w.period=p.period) ORDER BY settled_at LIMIT 1000 FOR UPDATE SKIP LOCKED)",
        "DELETE FROM lottery_presence WHERE (game,user_id) IN (SELECT game,user_id FROM lottery_presence WHERE seen_at < now()-interval '1 hour' ORDER BY seen_at LIMIT 1000 FOR UPDATE SKIP LOCKED)",
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
