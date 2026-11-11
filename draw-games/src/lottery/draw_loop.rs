//! Durable side work of a settled draw: the winner's inbox receipt and the
//! daily retention sweep. Draws and settlements themselves are decided in
//! memory by [`super::runtime`] and written by its ordered writer.
use super::{handlers::Error, store::TicketRow, Game};
use crate::db_writer::PgPool;
use serde_json::json;
use uuid::Uuid;

/// A winner's inbox receipt commits atomically with the credited prizes. The
/// deterministic identity and unique per-user key also make replay harmless.
pub(super) async fn notify_winner(
    conn: &mut crate::db_writer::PgConnection,
    user: Uuid,
    game: Game,
    period: i64,
    paid: i64,
    tickets: &[TicketRow],
) -> sqlx::Result<()> {
    if game == Game::Scratch {
        return Ok(());
    }
    // The registry's display name, as the hub and lottery center show it.
    let (zh_game, en_game) = crate::game_catalog::game(game.as_str())
        .map_or((game.as_str(), game.as_str()), |entry| (entry.zh, entry.en));
    let mut bonus = 0i64;
    let mut jackpots = 0usize;
    for ticket in tickets {
        if let Some(amount) = ticket.entry["jackpot_bonus"].as_i64() {
            bonus = bonus
                .checked_add(amount)
                .ok_or_else(|| sqlx::Error::Protocol("lottery jackpot bonus overflow".into()))?;
            jackpots += 1;
        }
    }
    let credited = paid
        .checked_add(bonus)
        .ok_or_else(|| sqlx::Error::Protocol("lottery credit overflow".into()))?;
    let zh_title = format!("{zh_game}中奖奖励已到账");
    let en_title = format!("{en_game} winnings credited");
    let (zh_body, en_body) = if bonus > 0 {
        (
            format!("本期中奖 {paid} 金豆，另有 {jackpots} 张票据中头奖获额外奖励 {bonus} 金豆，共 {credited} 金豆已自动到账，无需领取。"),
            format!("Your {paid} gold beans from this draw plus a {bonus}-bean jackpot bonus on {jackpots} ticket(s) ({credited} total) were credited automatically. No claim needed."),
        )
    } else {
        (
            format!("本期中奖 {paid} 金豆已自动到账，无需领取。"),
            format!("Your {paid} gold beans from this draw were credited automatically. No claim needed."),
        )
    };
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
            "game": game.as_str(), "period": period, "prize": paid, "bonus_beans": bonus,
            "credited_beans": credited,
            "ticket_count": tickets.len(),
            "winning_tickets": tickets.iter().filter(|t| t.prize.unwrap_or(0) > 0).count(),
        },
        "i18n": {"zh": {"title": zh_title, "body": zh_body}, "en": {"title": en_title, "body": en_body}},
    }))
    .bind(key)
    .execute(conn.writer())
    .await?;
    Ok(())
}

/// True when a statement deleted a full batch of 1,000 rows.
pub(super) async fn retention(pool: &PgPool) -> Result<bool, Error> {
    let mut full = false;
    // Each statement removes at most 1,000 rows; winners go before their period.
    for statement in [
        "DELETE FROM lottery_tickets WHERE id IN (SELECT id FROM lottery_tickets WHERE settled_at < now()-interval '30 days' ORDER BY settled_at LIMIT 1000 FOR UPDATE SKIP LOCKED)",
        "DELETE FROM lottery_winners WHERE ctid IN (SELECT w.ctid FROM lottery_periods p JOIN lottery_winners w USING (game,period) WHERE p.status='settled' AND p.settled_at < now()-interval '30 days' ORDER BY p.settled_at LIMIT 1000 FOR UPDATE OF w SKIP LOCKED)",
        "DELETE FROM lottery_periods WHERE (game,period) IN (SELECT p.game,p.period FROM lottery_periods p WHERE status='settled' AND settled_at < now()-interval '30 days' AND NOT EXISTS (SELECT 1 FROM lottery_winners w WHERE w.game=p.game AND w.period=p.period) ORDER BY settled_at LIMIT 1000 FOR UPDATE SKIP LOCKED)",
     ] {
        let mut tx = pool.begin().await?;
        sqlx::query("SELECT set_config('lock_timeout','500ms',true),set_config('statement_timeout','2s',true)")
            .execute(&mut *tx).await?;
        full |= sqlx::query(statement).execute(&mut *tx).await?.rows_affected() >= 1000;
        tx.commit().await?;
    }
    Ok(full)
}
