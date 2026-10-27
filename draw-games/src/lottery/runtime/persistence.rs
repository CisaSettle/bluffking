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

pub(super) async fn hydrate(pool: &PgPool) -> sqlx::Result<Data> {
    crate::live_directory::wait_for_open_transactions(pool).await?;
    let mut tx = pool.begin().await?;
    sqlx::query("SET TRANSACTION ISOLATION LEVEL REPEATABLE READ,READ ONLY")
        .execute(&mut *tx)
        .await?;
    let games: Vec<_> = Game::ALL.iter().map(|g| g.as_str()).collect();
    let mut data = Data::default();
    let (mut after_game, mut after_period) = (String::new(), i64::MIN);
    loop {
        let rows=sqlx::query("SELECT game,period,status,numbers,bonus,bonus_numbers,drawn_at,settled_at,tickets,stakes,players,sales,paid,winners,proof_plan,draw_proof FROM lottery_periods WHERE game=ANY($1) AND (game,period)>($2,$3) ORDER BY game,period LIMIT $4").bind(&games).bind(&after_game).bind(after_period).bind(PAGE).fetch_all(&mut *tx).await?;
        if rows.is_empty() {
            break;
        }
        for row in rows {
            after_game = row.get("game");
            after_period = row.get("period");
            let period = PeriodRow::from_row(&row);
            data.bytes = data.bytes.saturating_add(resident(&period));
            data.periods
                .insert((after_game.clone(), after_period), period);
        }
        bounded(&data)?;
    }
    let mut after = None::<Uuid>;
    loop {
        let rows=sqlx::query(&format!("SELECT user_id,game,{} FROM lottery_tickets WHERE game=ANY($1) AND (game<>'scratch' OR (cost=100 AND entry->'reward_version'='1'::jsonb)) AND ($2::uuid IS NULL OR id>$2) ORDER BY id LIMIT $3",store::TICKET_COLUMNS)).bind(&games).bind(after).bind(PAGE).fetch_all(&mut *tx).await?;
        if rows.is_empty() {
            break;
        }
        for row in rows {
            let card = TicketRow::from_row(&row);
            after = Some(card.id);
            data.insert(OwnedTicket {
                user: row.get("user_id"),
                game: row.get("game"),
                card,
            });
        }
        bounded(&data)?;
    }
    let mut after_id = 0i64;
    loop {
        let rows=sqlx::query("SELECT id,user_id,record_id,balance_after,details FROM wallet_ledger WHERE game=$1 AND kind='play' AND details->>'game'=ANY($2) AND details->>'body_sha256' IS NOT NULL AND id>$3 ORDER BY id LIMIT $4").bind(BEANS).bind(&games).bind(after_id).bind(PAGE).fetch_all(&mut *tx).await?;
        if rows.is_empty() {
            break;
        }
        for row in rows {
            after_id = row.get("id");
            let user: Uuid = row.get("user_id");
            let Some(record) = row.get::<Option<Uuid>, _>("record_id") else {
                continue;
            };
            let d: Value = row.get("details");
            let ids =
                serde_json::from_value(d["ticket_ids"].clone()).map_err(|_| db_unavailable())?;
            data.receipts.insert(
                (user, record),
                Receipt {
                    game: d["game"].as_str().ok_or_else(db_unavailable)?.into(),
                    record,
                    hash: d["body_sha256"].as_str().ok_or_else(db_unavailable)?.into(),
                    tickets: ids,
                    charged: d["charged"].as_i64().unwrap_or(0),
                    before: d["balance_before"].as_i64().unwrap_or(0),
                    after: d["accepted_balance_after"]
                        .as_i64()
                        .unwrap_or_else(|| row.get("balance_after")),
                },
            );
            data.bytes = data
                .bytes
                .saturating_add(resident(&data.receipts[&(user, record)]));
        }
        bounded(&data)?;
    }
    let mut after = None::<Uuid>;
    loop {
        let rows=sqlx::query("SELECT id,user_id,server_seed FROM draw_commitments WHERE game='scratch' AND ($1::uuid IS NULL OR id>$1) ORDER BY id LIMIT $2").bind(after).bind(PAGE).fetch_all(&mut *tx).await?;
        if rows.is_empty() {
            break;
        }
        for row in rows {
            let id = row.get("id");
            after = Some(id);
            let user = row.get("user_id");
            data.offers.insert(
                user,
                Offer {
                    user,
                    id,
                    seed: row.get("server_seed"),
                    fresh: false,
                    previous: None,
                },
            );
            data.bytes = data.bytes.saturating_add(resident(&data.offers[&user]));
        }
        bounded(&data)?;
    }
    let mut after = None::<Uuid>;
    loop {
        let rows=sqlx::query("SELECT id,game,period,user_id,name,tier,prize FROM lottery_winners WHERE game=ANY($1) AND ($2::uuid IS NULL OR id>$2) ORDER BY id LIMIT $3").bind(&games).bind(after).bind(PAGE).fetch_all(&mut *tx).await?;
        if rows.is_empty() {
            break;
        }
        for row in rows {
            after = Some(row.get("id"));
            let game: String = row.get("game");
            let period: i64 = row.get("period");
            let key = (game, period);
            data.winners.entry(key.clone()).or_default().push(Winner {
                user: row.get("user_id"),
                name: row.get("name"),
                tier: row.get("tier"),
                prize: row.get("prize"),
            });
            let winner = data.winners.get(&key).and_then(|v| v.last());
            if let Some(winner) = winner {
                data.bytes = data.bytes.saturating_add(resident(winner));
            }
        }
        bounded(&data)?;
    }
    tx.commit().await?;
    for ids in data.recent.values_mut() {
        let mut sorted: Vec<_> = ids.drain(..).collect();
        sorted.sort_by_key(|id| std::cmp::Reverse((data.tickets[id].card.bought_at, *id)));
        sorted.truncate(100);
        *ids = sorted.into();
    }
    Ok(data)
}

pub(super) async fn persist(
    pool: &PgPool,
    event: &Event,
) -> sqlx::Result<Vec<(Uuid, crate::game_rewards::Grant)>> {
    if event.kind == "retention" {
        super::super::draw_loop::retention(pool)
            .await
            .map_err(|e| match e {
                Error::Db(e) => e,
                _ => db_unavailable(),
            })?;
        return Ok(vec![]);
    }
    let game = Game::parse(&event.game).ok_or_else(db_unavailable)?;
    let fingerprint =
        store::body_sha256(&serde_json::to_value(event).map_err(|_| db_unavailable())?);
    let mut tx = pool.begin().await?;
    if let Some(user) = event.owner {
        crate::db::lock_game_participants(&mut tx, &[user]).await?;
        crate::wallet::lock(&mut tx, BEANS, user)
            .await?
            .ok_or(sqlx::Error::RowNotFound)?;
    }
    if let (Some(user), Some(effect)) = (event.owner, &event.effect) {
        if let Some((delta, details)) = sqlx::query_as::<_, (i64, Option<Value>)>(
            "SELECT delta,details FROM wallet_ledger WHERE user_id=$1 AND game=$2 AND record_id=$3",
        )
        .bind(user)
        .bind(BEANS)
        .bind(event.id)
        .fetch_optional(&mut *tx)
        .await?
        {
            if delta != effect.ledger_delta
                || details.as_ref().and_then(|d| d["lottery_event"].as_str())
                    != Some(fingerprint.as_str())
            {
                return Err(db_unavailable());
            }
            let grants = read_grants(&mut tx, &event.max_cards).await?;
            tx.commit().await?;
            publish(pool, event);
            return Ok(grants);
        }
    }
    if let Some(offer) = &event.offer {
        sqlx::query("INSERT INTO draw_commitments(id,user_id,game,server_seed) VALUES($1,$2,'scratch',$3) ON CONFLICT(user_id,game) DO UPDATE SET id=EXCLUDED.id,server_seed=EXCLUDED.server_seed WHERE draw_commitments.id=EXCLUDED.id OR draw_commitments.id=$4").bind(offer.id).bind(offer.user).bind(&offer.seed).bind(offer.previous).execute(&mut *tx).await?;
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
            return Err(db_unavailable());
        }
    }
    let mut cards = event.cards.clone();
    let mut grants = vec![];
    for id in &event.max_cards {
        let user = event.owner.ok_or_else(db_unavailable)?;
        let grant = crate::game_rewards::grant_max_on(&mut tx, user, &event.game, *id).await?;
        if let Some(card) = cards.iter_mut().find(|t| t.card.id == *id) {
            card.card.entry["reward_grant"] = json!(grant)
        }
        grants.push((*id, grant));
    }
    if let Some(row) = &event.period {
        let plan: draw_games::lottery::Plan =
            serde_json::from_value(row.proof_plan.clone().ok_or_else(db_unavailable)?)
                .map_err(|_| db_unavailable())?;
        let opens = DateTime::from_timestamp_millis(
            plan.sales_opens_at_ms().map_err(|_| db_unavailable())?,
        )
        .ok_or_else(db_unavailable)?;
        let draw = DateTime::from_timestamp_millis(plan.draw_at_ms).ok_or_else(db_unavailable)?;
        let changed = sqlx::query("INSERT INTO lottery_periods(game,period,status,opens_at,draw_at,proof_plan,numbers,bonus,bonus_numbers,drawn_at,tickets,stakes,players,sales,paid,winners,draw_proof,settled_at) VALUES($1,$2,$3,$4,$5,$6,$7,$8,$9,$10,$11,$12,$13,$14,$15,$16,$17,CASE WHEN $3='settled' THEN $18::timestamptz ELSE NULL END) ON CONFLICT(game,period) DO UPDATE SET status=EXCLUDED.status,numbers=EXCLUDED.numbers,bonus=EXCLUDED.bonus,bonus_numbers=EXCLUDED.bonus_numbers,drawn_at=EXCLUDED.drawn_at,tickets=EXCLUDED.tickets,stakes=EXCLUDED.stakes,players=EXCLUDED.players,sales=EXCLUDED.sales,paid=EXCLUDED.paid,winners=EXCLUDED.winners,draw_proof=EXCLUDED.draw_proof,settled_at=EXCLUDED.settled_at WHERE lottery_periods.proof_plan=EXCLUDED.proof_plan")
 .bind(&event.game).bind(row.period).bind(&row.status).bind(opens).bind(draw).bind(&row.proof_plan).bind(&row.numbers).bind(row.bonus).bind(&row.bonus_numbers).bind(row.drawn_at).bind(row.tickets).bind(row.stakes).bind(row.players).bind(row.sales).bind(row.paid).bind(row.winners).bind(&row.draw_proof).bind(row.settled_at).execute(&mut *tx).await?.rows_affected();
        if changed != 1 {
            return Err(db_unavailable());
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
            return Err(db_unavailable());
        }
    }
    if let (Some(user), Some(effect)) = (event.owner, &event.effect) {
        if let Some(day) = effect.day {
            let n=sqlx::query("INSERT INTO daily_usage(scope,subject_key,feature,usage_date,count) VALUES('feature_user',$1,'beans_daily_refill',$2,1) ON CONFLICT DO NOTHING").bind(user.to_string()).bind(day).execute(&mut *tx).await?.rows_affected();
            if n != 1 {
                return Err(db_unavailable());
            }
        }
        if event.bonus > 0 {
            let reason = if event.kind == "daily" {
                json!({"reason":"daily_refill"})
            } else {
                json!({"purpose":"scratch_reward","reward_version":1,"lottery_event_id":event.id})
            };
            sqlx::query("WITH updated AS (UPDATE wallets SET balance=balance+$3,updated_at=now() WHERE user_id=$1 AND game=$2 RETURNING balance) INSERT INTO wallet_ledger(user_id,game,kind,delta,balance_after,details) SELECT $1,$2,'bonus',$3,balance,$4 FROM updated").bind(user).bind(BEANS).bind(event.bonus).bind(reason).execute(&mut *tx).await?;
        }
        let mut details = json!({"purpose":event.kind,"game":event.game,"lottery_event":fingerprint,"ticket_ids":event.cards.iter().map(|t|t.card.id).collect::<Vec<_>>()});
        if let Some(receipt) = &event.receipt {
            details["body_sha256"] = json!(receipt.hash);
            details["charged"] = json!(receipt.charged);
            details["balance_before"] = json!(receipt.before);
            details["accepted_balance_after"] = json!(receipt.after)
        }
        crate::wallet::settle(
            &mut tx,
            BEANS,
            user,
            &crate::wallet::Play {
                record_id: event.id,
                delta: effect.net,
                completes_hand: effect.played != 0,
                won: effect.won != 0,
                room_code: None,
                revision: None,
                details: Some(details),
                hand_completed: None,
                has_bots: None,
            },
        )
        .await?;
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
                        _ => db_unavailable(),
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
    if let Some(user) = event.owner {
        if !event.max_cards.is_empty() {
            crate::entitlement::invalidate_tier(pool, user)
        }
        crate::read_cache::invalidate_unread_count(pool, user);
        if let Some(receipt) = &event.receipt {
            crate::stats_writer::send(crate::stats_writer::StatsEvent::LotteryPurchase {
                day: super::super::sgt_day(event.at),
                game: game.as_str(),
                user,
                tickets: cards.len() as i64,
                stakes: cards
                    .iter()
                    .map(|t| i64::from(t.card.stakes) * i64::from(t.card.multiplier))
                    .sum(),
                sales: receipt.charged,
            });
        }
    }
    Ok(grants)
}
async fn read_grants(
    conn: &mut sqlx::PgConnection,
    ids: &[Uuid],
) -> sqlx::Result<Vec<(Uuid, crate::game_rewards::Grant)>> {
    if ids.is_empty() {
        return Ok(vec![]);
    }
    let rows = sqlx::query("SELECT id,entry FROM lottery_tickets WHERE id=ANY($1)")
        .bind(ids)
        .fetch_all(conn)
        .await?;
    let mut result = vec![];
    for row in rows {
        let entry: Value = row.get("entry");
        if !entry["reward_grant"].is_null() {
            result.push((
                row.get("id"),
                serde_json::from_value(entry["reward_grant"].clone())
                    .map_err(|_| db_unavailable())?,
            ))
        }
    }
    Ok(result)
}

fn publish(pool: &PgPool, event: &Event) {
    if let Some(user) = event.owner {
        if !event.max_cards.is_empty() {
            crate::entitlement::invalidate_tier(pool, user);
        }
        crate::read_cache::invalidate_unread_count(pool, user);
    }
}
