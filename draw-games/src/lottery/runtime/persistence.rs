//! Only startup and this ordered writer touch SQL. No rules, random draws,
//! balance admission or closing-clock checks run again after memory acceptance.
use super::*;
use crate::game_catalog::BEANS;

pub(super) fn weight(data: &Data) -> sqlx::Result<usize> {
    let mut n = 0usize;
    for t in data.tickets.values() {
        n = n.saturating_add(serde_json::to_vec(t).map_err(|_| db_unavailable())?.len() * 4 + 256)
    }
    for r in data.periods.values() {
        n = n.saturating_add(serde_json::to_vec(r).map_err(|_| db_unavailable())?.len() * 4 + 256)
    }
    for r in data.receipts.values() {
        n = n.saturating_add(serde_json::to_vec(r).map_err(|_| db_unavailable())?.len() * 4 + 256)
    }
    for o in data.offers.values() {
        n = n.saturating_add(serde_json::to_vec(o).map_err(|_| db_unavailable())?.len() * 4 + 256);
    }
    for winners in data.winners.values() {
        for w in winners {
            n = n.saturating_add(
                serde_json::to_vec(w).map_err(|_| db_unavailable())?.len() * 4 + 256,
            );
        }
    }
    Ok(n)
}
fn bounded(data: &Data) -> sqlx::Result<()> {
    if data.tickets.len()
        + data.periods.len()
        + data.receipts.len()
        + data.offers.len()
        + data.winners.values().map(Vec::len).sum::<usize>()
        > MAX_ROWS
        || data.bytes > MAX_BYTES
    {
        Err(db_unavailable())
    } else {
        Ok(())
    }
}

/// A deterministic mismatch between the accepted event and SQL. Retrying the
/// same event cannot succeed, so the writer fences and memory rehydrates.
const PERMANENT: &str = "lottery_permanent: ";
fn permanent(what: &str) -> sqlx::Error {
    sqlx::Error::Protocol(format!("{PERMANENT}{what}"))
}
/// Permanent = decided by the event itself, so retrying it cannot succeed:
/// our own mismatch markers, a bean posting or bonus credit the ledger
/// refuses, every integrity violation (SQLSTATE class 23: unique, foreign
/// key, CHECK, which includes the ledger balance and immutability guards),
/// every data exception (class 22) and a trigger's plain refusal (P0001: the
/// published-plan and completed-draw guards). The writer stops at such an
/// event and `discard_fenced` drops it alone and resumes the rest. Timeouts,
/// lock waits, serialization failures and connection loss stay retryable.
///
/// None of these is reachable through a player's request today: the bean
/// wallet may go negative (no balance CHECK), a pending credit is never
/// spendable, record ids and transfer keys are derived and checked before
/// admission, tickets carry the plan of their period, and an account cannot
/// be erased while it has accepted work.
pub(super) fn is_permanent(error: &sqlx::Error) -> bool {
    if crate::bean_ledger::terminal_reason(error).is_some() {
        return true;
    }
    match error {
        sqlx::Error::Protocol(message) => {
            message.starts_with(PERMANENT)
                || message == crate::bean_ledger::TRANSFER_KEY_CONFLICT
                || message == crate::bean_ledger::TRANSFER_INVALID
        }
        sqlx::Error::Database(e) => e.code().is_some_and(|code| {
            code.starts_with("23") || code.starts_with("22") || code == "P0001"
        }),
        _ => false,
    }
}

pub(super) async fn hydrate(pool: &PgPool) -> sqlx::Result<Data> {
    hydrate_missing(pool, &[], &Game::DRAWN).await
}

/// Fills the free slots of `games` with their oldest unsettled periods that
/// are not already in memory (`existing`).
pub(super) async fn hydrate_missing(
    pool: &PgPool,
    existing: &[(String, i64)],
    games: &[Game],
) -> sqlx::Result<Data> {
    let mut tx = pool.begin_read().await?;
    sqlx::query("SET TRANSACTION ISOLATION LEVEL REPEATABLE READ,READ ONLY")
        .execute(&mut *tx)
        .await?;
    sqlx::query(
        "SELECT set_config('lock_timeout','500ms',true),set_config('statement_timeout','2s',true)",
    )
    .execute(&mut *tx)
    .await?;
    let mut data = Data::default();
    // Three slots per game, oldest outstanding work first. Never hydrate history.
    for &game in games {
        let loaded: Vec<i64> = existing
            .iter()
            .filter(|(g, _)| g == game.as_str())
            .map(|(_, p)| *p)
            .collect();
        let remaining = PERIOD_SLOTS.saturating_sub(loaded.len()) as i64;
        if remaining == 0 {
            continue;
        }
        let rows = sqlx::query("SELECT game,period,status,numbers,bonus,bonus_numbers,drawn_at,settled_at,tickets,stakes,players,sales,paid,winners,proof_plan,draw_proof FROM lottery_periods WHERE game=$1 AND status<>'settled' AND NOT(period=ANY($2)) ORDER BY period LIMIT $3")
            .bind(game.as_str()).bind(&loaded).bind(remaining).fetch_all(&mut *tx).await?;
        for row in rows {
            let period = PeriodRow::from_row(&row);
            data.bytes = data.bytes.saturating_add(resident(&period));
            data.periods
                .insert((game.as_str().into(), period.period), period);
        }
    }
    let keys: Vec<_> = data.periods.keys().cloned().collect();
    for (game, period) in keys {
        load_tickets(&mut tx, &mut data, &game, period).await?;
    }
    tx.commit().await?;
    sort_recent(&mut data);
    Ok(data)
}

/// Every ticket of one period, settled or not, in id pages.
async fn load_tickets(
    conn: &mut crate::db_writer::PgConnection,
    data: &mut Data,
    game: &str,
    period: i64,
) -> sqlx::Result<()> {
    let mut after = None::<Uuid>;
    loop {
        let rows = sqlx::query(&format!("SELECT user_id,game,{} FROM lottery_tickets WHERE game=$1 AND period=$2 AND id>COALESCE($3,'00000000-0000-0000-0000-000000000000'::uuid) ORDER BY id LIMIT $4", store::TICKET_COLUMNS))
            .bind(game).bind(period).bind(after).bind(PAGE).fetch_all(&mut *conn).await?;
        if rows.is_empty() {
            return Ok(());
        }
        for row in rows {
            let card = TicketRow::from_row(&row);
            after = Some(card.id);
            data.insert(OwnedTicket {
                user: row.get("user_id"),
                game: game.to_owned(),
                card,
            });
        }
        bounded(data)?;
    }
}

fn sort_recent(data: &mut Data) {
    for ids in data.recent.values_mut() {
        let mut sorted: Vec<_> = ids.drain(..).collect();
        sorted.sort_by_key(|id| std::cmp::Reverse((data.tickets[id].card.bought_at, *id)));
        sorted.truncate(100);
        *ids = sorted.into();
    }
}

/// Periods already marked settled that still hold unsettled number tickets,
/// oldest first. A single owner never produces one (a period is marked
/// settled only after every ticket of it settled, and nothing is sold after
/// the draw); they come from a second backend on the same database marking a
/// period settled without having seen another backend's tickets. Read once
/// per start through the unsettled-ticket index. A period without a stored
/// plan or draw cannot be settled by the runtime and is only counted, like
/// the unsettled tickets whose period row no longer exists (retention removed
/// it): the second number returned is periods plus such tickets.
pub(super) async fn stranded(pool: &PgPool) -> sqlx::Result<(Vec<(String, i64)>, i64)> {
    let mut tx = pool.begin_read().await?;
    sqlx::query(
        "SELECT set_config('lock_timeout','500ms',true),set_config('statement_timeout','2s',true)",
    )
    .execute(&mut *tx)
    .await?;
    let rows: Vec<(String, i64, bool)> = sqlx::query_as(
        "SELECT p.game,p.period,(p.numbers IS NOT NULL AND p.proof_plan IS NOT NULL AND p.draw_proof IS NOT NULL) \
           FROM lottery_periods p \
          WHERE p.status='settled' AND (p.game,p.period) IN \
                (SELECT t.game,t.period FROM lottery_tickets t WHERE t.settled_at IS NULL AND t.game<>'scratch') \
          ORDER BY p.game,p.period LIMIT 1000",
    )
    .fetch_all(&mut *tx)
    .await?;
    let orphaned: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM lottery_tickets t WHERE t.settled_at IS NULL AND t.game<>'scratch' \
            AND NOT EXISTS(SELECT 1 FROM lottery_periods p WHERE p.game=t.game AND p.period=t.period)",
    )
    .fetch_one(&mut *tx)
    .await?;
    tx.commit().await?;
    let unusable = rows.iter().filter(|row| !row.2).count() as i64 + orphaned;
    Ok((
        rows.into_iter()
            .filter(|row| row.2)
            .map(|(game, period, _)| (game, period))
            .collect(),
        unusable,
    ))
}

/// One [`stranded`] period with all of its tickets, reopened for settlement:
/// in memory it is `drawn` again, with the draw it already has. `None` when
/// it is no longer stranded.
pub(super) async fn load_stranded(
    pool: &PgPool,
    game: &str,
    period: i64,
) -> sqlx::Result<Option<Data>> {
    let mut tx = pool.begin_read().await?;
    sqlx::query("SET TRANSACTION ISOLATION LEVEL REPEATABLE READ,READ ONLY")
        .execute(&mut *tx)
        .await?;
    sqlx::query(
        "SELECT set_config('lock_timeout','500ms',true),set_config('statement_timeout','2s',true)",
    )
    .execute(&mut *tx)
    .await?;
    let row = sqlx::query("SELECT game,period,status,numbers,bonus,bonus_numbers,drawn_at,settled_at,tickets,stakes,players,sales,paid,winners,proof_plan,draw_proof FROM lottery_periods p WHERE game=$1 AND period=$2 AND status='settled' AND numbers IS NOT NULL AND proof_plan IS NOT NULL AND draw_proof IS NOT NULL AND EXISTS(SELECT 1 FROM lottery_tickets t WHERE t.game=p.game AND t.period=p.period AND t.settled_at IS NULL)")
        .bind(game).bind(period).fetch_optional(&mut *tx).await?;
    let Some(row) = row else {
        return Ok(None);
    };
    let mut reopened = PeriodRow::from_row(&row);
    reopened.status = "drawn".into();
    reopened.settled_at = None;
    let mut data = Data {
        bytes: resident(&reopened),
        ..Data::default()
    };
    data.periods.insert((game.to_owned(), period), reopened);
    load_tickets(&mut tx, &mut data, game, period).await?;
    tx.commit().await?;
    sort_recent(&mut data);
    Ok(Some(data))
}

/// The daily retention job: its own pool transactions, no writer connection.
/// True when a statement deleted a full batch, so more rows are due.
pub(super) async fn retention(pool: &PgPool) -> sqlx::Result<bool> {
    super::super::draw_loop::retention(pool)
        .await
        .map_err(|e| match e {
            Error::Db(e) => e,
            _ => db_unavailable(),
        })
}

pub(super) async fn persist(
    pool: &PgPool,
    conn: &mut crate::db_writer::PgConnection,
    event: &Event,
) -> sqlx::Result<()> {
    if event.kind == "retention" {
        retention(pool).await?;
        return Ok(());
    }
    let game = Game::parse(&event.game).ok_or_else(|| permanent("game"))?;
    let fingerprint =
        store::body_sha256(&serde_json::to_value(event).map_err(|_| permanent("event json"))?);
    // One round trip opens the transaction with its local timeouts (same
    // values as the former separate `set_config(..., true)` statement).
    let mut tx = conn
        .begin_with("BEGIN READ WRITE; SET LOCAL lock_timeout = '500ms'; SET LOCAL statement_timeout = '2s'")
        .await?;
    if let Some(user) = event.owner {
        crate::db::lock_game_participants(&mut tx, &[user]).await?;
        crate::bean_ledger::lock(&mut tx, BEANS, user)
            .await?
            .ok_or_else(|| permanent("wallet row missing"))?;
    }
    // The event's wallet effect is one bean-ledger play posting under the
    // event id, plus one bonus credit keyed by the same id when the event
    // pays a jackpot or card bonus; both are written before anything else of
    // the event. The posting is idempotent on (event id, user): a replay
    // means our own earlier commit (a lost acknowledgement), whose bonus,
    // tickets and period committed with it.
    if let (Some(user), Some(effect)) = (event.owner, &event.effect) {
        let posted =
            crate::bean_ledger::settle(&mut tx, &posting(event, user, effect, &fingerprint))
                .await?;
        if posted == crate::bean_ledger::Posted::Replayed {
            // Same id and delta but another accepted event is not ours.
            if stored_fingerprint(&mut tx, user, event.id)
                .await?
                .as_deref()
                != Some(fingerprint.as_str())
            {
                return Err(permanent("ledger fingerprint mismatch"));
            }
            tx.commit().await?;
            publish(pool, event);
            // The attempt that committed never reached its statistics.
            statistics(event, game);
            return Ok(());
        }
        if effect.bonus > 0 {
            let reason = if game == Game::Scratch {
                crate::bean_ledger::Reason::ScratchReward
            } else {
                crate::bean_ledger::Reason::LotteryJackpot
            };
            let credited = crate::bean_ledger::credit(
                &mut tx,
                &crate::bean_ledger::Transfer::one(crate::bean_ledger::Leg {
                    game: Some(&event.game),
                    ..crate::bean_ledger::Leg::new(
                        user,
                        reason,
                        event.id,
                        crate::bean_ledger::Amount::Credit(effect.bonus),
                        json!({"lottery_event_id": event.id}),
                    )
                }),
            )
            .await
            .map_err(sqlx::Error::from)?;
            // The play row was new, so its bonus cannot exist yet.
            if !credited.applied() {
                return Err(permanent("bonus credit already posted"));
            }
        }
    }
    if let (Some(user), Some(id)) = (event.owner, event.claim) {
        let removed = sqlx::query(
            "DELETE FROM draw_commitments WHERE id=$1 AND user_id=$2 AND game='scratch'",
        )
        .bind(id)
        .bind(user)
        .execute(&mut *tx)
        .await?
        .rows_affected();
        if removed != 1 {
            return Err(permanent("scratch commitment missing"));
        }
    }
    if let Some(offer) = &event.offer {
        let changed = sqlx::query("INSERT INTO draw_commitments(id,user_id,game,server_seed) VALUES($1,$2,'scratch',$3) ON CONFLICT(user_id,game) DO UPDATE SET id=EXCLUDED.id,server_seed=EXCLUDED.server_seed WHERE draw_commitments.id=EXCLUDED.id OR draw_commitments.id=$4").bind(offer.id).bind(offer.user).bind(&offer.seed).bind(offer.previous).execute(&mut *tx).await?.rows_affected();
        if changed != 1 {
            return Err(permanent("scratch commitment replaced"));
        }
    }
    let cards = &event.cards;
    if let Some(row) = &event.period {
        let plan: draw_games::lottery::Plan = serde_json::from_value(
            row.proof_plan
                .clone()
                .ok_or_else(|| permanent("proof plan"))?,
        )
        .map_err(|_| permanent("proof plan"))?;
        let opens = DateTime::from_timestamp_millis(
            plan.sales_opens_at_ms()
                .map_err(|_| permanent("proof plan"))?,
        )
        .ok_or_else(|| permanent("proof plan"))?;
        let draw = DateTime::from_timestamp_millis(plan.draw_at_ms)
            .ok_or_else(|| permanent("proof plan"))?;
        let changed = sqlx::query("INSERT INTO lottery_periods(game,period,status,opens_at,draw_at,proof_plan,numbers,bonus,bonus_numbers,drawn_at,tickets,stakes,players,sales,paid,winners,draw_proof,settled_at) VALUES($1,$2,$3,$4,$5,$6,$7,$8,$9,$10,$11,$12,$13,$14,$15,$16,$17,CASE WHEN $3='settled' THEN $18::timestamptz ELSE NULL END) ON CONFLICT(game,period) DO UPDATE SET status=EXCLUDED.status,numbers=EXCLUDED.numbers,bonus=EXCLUDED.bonus,bonus_numbers=EXCLUDED.bonus_numbers,drawn_at=EXCLUDED.drawn_at,tickets=EXCLUDED.tickets,stakes=EXCLUDED.stakes,players=EXCLUDED.players,sales=EXCLUDED.sales,paid=EXCLUDED.paid,winners=EXCLUDED.winners,draw_proof=EXCLUDED.draw_proof,settled_at=EXCLUDED.settled_at WHERE lottery_periods.proof_plan=EXCLUDED.proof_plan")
 .bind(&event.game).bind(row.period).bind(&row.status).bind(opens).bind(draw).bind(&row.proof_plan).bind(&row.numbers).bind(row.bonus).bind(&row.bonus_numbers).bind(row.drawn_at).bind(row.tickets).bind(row.stakes).bind(row.players).bind(row.sales).bind(row.paid).bind(row.winners).bind(&row.draw_proof).bind(row.settled_at).execute(&mut *tx).await?.rows_affected();
        if changed != 1 {
            return Err(permanent("period proof plan mismatch"));
        }
    }
    if !cards.is_empty() {
        let values = cards
            .iter()
            .map(|t| {
                let mut value = serde_json::to_value(&t.card).expect("ticket serializes");
                value["user_id"] = json!(t.user);
                value["game"] = json!(t.game);
                value
            })
            .collect::<Vec<_>>();
        let changed = sqlx::query("INSERT INTO lottery_tickets(id,purchase_id,game,period,user_id,bought_at,entry,stakes,multiplier,cost,revealed,prize,tier,settled_at,proof_plan) SELECT id,purchase_id,game,period,user_id,bought_at,entry,stakes,multiplier,cost,revealed,prize,tier,settled_at,proof_plan FROM jsonb_to_recordset($1) AS x(id uuid,purchase_id uuid,game text,period bigint,user_id uuid,bought_at timestamptz,entry jsonb,stakes int,multiplier smallint,cost bigint,revealed boolean[],prize bigint,tier smallint,settled_at timestamptz,proof_plan jsonb) ON CONFLICT(id) DO UPDATE SET revealed=EXCLUDED.revealed,prize=EXCLUDED.prize,tier=EXCLUDED.tier,settled_at=EXCLUDED.settled_at,entry=EXCLUDED.entry WHERE lottery_tickets.user_id=EXCLUDED.user_id AND lottery_tickets.game=EXCLUDED.game")
 .bind(json!(values)).execute(&mut *tx).await?.rows_affected();
        if changed != cards.len() as u64 {
            return Err(permanent("ticket owner mismatch"));
        }
    }
    if let Some(user) = event.owner {
        let settled: Vec<_> = cards
            .iter()
            .filter(|t| t.card.settled_at.is_some())
            .map(|t| t.card.clone())
            .collect();
        if !settled.is_empty() {
            if game == Game::Scratch {
                super::super::scratch::notify_winners(&mut tx, user, &settled)
                    .await
                    .map_err(|error| match error {
                        Error::Db(e) => e,
                        // Decided by the stored card: no retry can change it.
                        _ => permanent("scratch winner notice"),
                    })?;
            } else {
                let paid = settled.iter().map(|t| t.prize.unwrap_or(0)).sum::<i64>();
                if paid > 0 {
                    let period = settled[0].period;
                    let tiers = settled
                        .iter()
                        .filter(|t| t.prize.unwrap_or(0) > 0)
                        .filter_map(|t| t.tier)
                        .collect::<Vec<_>>();
                    let tier = if game == Game::Happy8 {
                        tiers.into_iter().max()
                    } else {
                        tiers.into_iter().min()
                    }
                    .unwrap_or(0);
                    sqlx::query("INSERT INTO lottery_winners(id,game,period,user_id,name,tier,prize) VALUES($1,$2,$3,$4,$5,$6,$7) ON CONFLICT(game,period,id) DO NOTHING").bind(event.id).bind(game.as_str()).bind(period).bind(user).bind(name(pool,user)).bind(tier).bind(paid).execute(&mut *tx).await?;
                    super::super::draw_loop::notify_winner(
                        &mut tx, user, game, period, paid, &settled,
                    )
                    .await?;
                }
            }
        }
    }
    tx.commit().await?;
    publish(pool, event);
    statistics(event, game);
    Ok(())
}
/// Day statistics of a committed event, sent once: by the attempt that saw
/// the commit succeed, or by the replay of one whose acknowledgement was lost.
/// Fire-and-forget; never part of the write.
fn statistics(event: &Event, game: Game) {
    let Some(user) = event.owner else { return };
    if let Some(receipt) = &event.receipt {
        crate::stats_writer::send(crate::stats_writer::StatsEvent::LotteryPurchase {
            day: super::super::sgt_day(event.at),
            game: game.as_str(),
            user,
            tickets: event.cards.len() as i64,
            stakes: event
                .cards
                .iter()
                .map(|t| i64::from(t.card.stakes) * i64::from(t.card.multiplier))
                .sum(),
            sales: receipt.charged,
        });
    }
    // Prizes of the cards this event settled (a settlement that paid nothing
    // still counts, like a played round).
    let settled = || event.cards.iter().filter(|t| t.card.settled_at.is_some());
    if settled().next().is_some() {
        crate::stats_writer::send(crate::stats_writer::StatsEvent::LotteryPaid {
            day: super::super::sgt_day(event.at),
            game: game.as_str(),
            user,
            paid: settled().map(|t| t.card.prize.unwrap_or(0)).sum(),
        });
    }
}

/// The play posting of an event (`details.v = 2`, kind `draw`). `meta.op` is
/// the event kind (`purchase`, `scratch_batch`, `reveal`, `settle`); `prize`
/// is what its settled tickets paid; a purchase carries its replay facts.
fn posting<'a>(
    event: &'a Event,
    user: Uuid,
    effect: &PendingEffect,
    fingerprint: &str,
) -> crate::bean_ledger::Posting<'a> {
    let settled = event.cards.iter().filter(|t| t.card.settled_at.is_some());
    let mut meta = json!({
        "op": event.kind,
        "lottery_event": fingerprint,
        "ticket_ids": event.cards.iter().map(|t| t.card.id).collect::<Vec<_>>(),
        "prize": settled.map(|t| t.card.prize.unwrap_or(0)).sum::<i64>(),
    });
    if let Some(period) = event
        .cards
        .first()
        .map(|t| t.card.period)
        .filter(|p| *p != 0)
    {
        meta["period"] = json!(period);
    }
    if let Some(receipt) = &event.receipt {
        meta["body_sha256"] = json!(receipt.hash);
        meta["charged"] = json!(receipt.charged);
        meta["balance_before"] = json!(receipt.before);
        meta["accepted_balance_after"] = json!(receipt.after);
    }
    crate::bean_ledger::Posting {
        game: &event.game,
        kind: crate::bean_ledger::Kind::Draw,
        record_id: event.id,
        room_code: None,
        session: None,
        has_bots: false,
        meta,
        entries: vec![crate::bean_ledger::Entry {
            user,
            delta: effect.delta,
            completes_hand: effect.played != 0,
            won: effect.won != 0,
            meta: None,
        }],
    }
}

/// The accepted-event fingerprint of the receipt posted for `record`.
async fn stored_fingerprint(
    conn: &mut crate::db_writer::PgConnection,
    user: Uuid,
    record: Uuid,
) -> sqlx::Result<Option<String>> {
    sqlx::query_scalar(
        "SELECT details->'meta'->>'lottery_event' FROM wallet_ledger \
          WHERE user_id=$1 AND game=$2 AND kind='play' AND record_id=$3 \
            AND details->'meta'->>'lottery_event' IS NOT NULL LIMIT 1",
    )
    .bind(user)
    .bind(BEANS)
    .bind(record)
    .fetch_optional(conn)
    .await
}

fn publish(pool: &PgPool, event: &Event) {
    if let Some(user) = event.owner {
        crate::read_cache::invalidate_unread_count(pool, user);
    }
}
