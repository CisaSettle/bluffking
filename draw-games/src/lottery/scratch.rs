//! Persisted scratch cards. Hidden numbers are projected out of every response.
use super::{
    handlers::Error,
    store::{self, Purchase, TicketRow, TICKET_COLUMNS},
    Game, POLICY,
};
use crate::{draw_fairness, wallet};
use chrono::Utc;
use draw_games::scratch;
use serde_json::{json, Value};
use sqlx::{PgConnection, PgPool, Row};
use uuid::Uuid;

#[derive(serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ScratchBuy {
    pub request_id: Uuid,
    pub cost: i64,
    pub count: usize,
    pub fairness: draw_fairness::Challenge,
    #[serde(default)]
    pub all_in: bool,
    #[serde(default)]
    pub expected_balance: Option<i64>,
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
    let all = (0..6).all(|i| revealed.get(i) == Some(&true));
    let proof = &ticket.entry["fairness"];
    let fairness = if all {
        // Verify the card and reward actually returned by storage.
        json!({
            "kind": "scratch", "reveal": proof["reveal"],
            "cost": ticket.cost, "count": proof["count"], "index": proof["index"],
            "numbers": ticket.entry["numbers"], "lucky": ticket.entry["lucky"],
            "prize": ticket.prize, "hits": ticket.tier,
            "reward_version": 1, "reward": ticket.entry["reward"],
        })
    } else {
        json!({
            "kind": "scratch",
            "commitment": {
                "protocol": proof["reveal"]["protocol"],
                "game": proof["reveal"]["game"],
                "draw_id": proof["reveal"]["draw_id"],
                "commitment": proof["reveal"]["commitment"],
            },
            "client_seed": proof["reveal"]["client_seed"],
            "cost": ticket.cost, "count": proof["count"], "index": proof["index"],
            "verified": false,
        })
    };
    out["reward_version"] = json!(1);
    out["reward"] = if all && ticket.settled_at.is_some() {
        ticket.entry["reward"].clone()
    } else {
        Value::Null
    };
    out["reward_grant"] = if all && ticket.settled_at.is_some() {
        ticket.entry["reward_grant"].clone()
    } else {
        Value::Null
    };
    // Whitelist both copies: the persisted proof contains the server seed and
    // complete card, neither of which may escape before all six spots open.
    out["entry"] = json!({"cost": ticket.cost, "lucky": ticket.entry["lucky"], "numbers": numbers, "fairness": fairness});
    out["lucky"] = ticket.entry["lucky"].clone();
    out["numbers"] = json!(numbers);
    out["revealed"] = json!(revealed);
    out["fairness"] = fairness;
}

fn prize(entry: &Value, cost: i64) -> Result<(i64, i16), Error> {
    let numbers: [i16; 6] =
        serde_json::from_value(entry["numbers"].clone()).map_err(|_| Error::InvalidEntry)?;
    let lucky: i16 =
        serde_json::from_value(entry["lucky"].clone()).map_err(|_| Error::InvalidEntry)?;
    scratch::payout(cost, lucky, &numbers).map_err(|_| Error::InvalidEntry)
}

pub async fn buy(pool: &PgPool, user: Uuid, body: &ScratchBuy) -> Result<Purchase, Error> {
    if body.request_id.is_nil()
        || body.cost != 100
        || (!body.all_in && ![1, 5, 10].contains(&body.count))
        || (body.all_in && (body.count == 0 || body.expected_balance.is_none()))
        || (!body.all_in && body.expected_balance.is_some())
    {
        return Err(Error::InvalidEntry);
    }
    if body.count > scratch::MAX_BATCH_CARDS {
        return Err(Error::TooManyTickets);
    }
    // ALL-IN is an immediate reveal even when the balance buys just one card.
    let batch = body.count > 1 || body.all_in;
    let record_id = if batch {
        super::scratch_batch_id(user, body.request_id)
    } else {
        super::purchase_id(user, body.request_id)
    };
    let mut identity = json!({"game":"scratch", "cost":body.cost, "count":body.count,
        "fairness": {"offer_id":body.fairness.offer_id,"commitment":body.fairness.commitment,"client_seed":body.fairness.client_seed}});
    if body.all_in {
        identity["all_in"] = json!(true);
        identity["expected_balance"] = json!(body.expected_balance);
    }
    let hash = store::body_sha256(&identity);
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
    if body.all_in {
        if body.expected_balance != Some(before) {
            return Err(Error::RequestConflict);
        }
        if before / body.cost != body.count as i64 {
            return Err(Error::InvalidEntry);
        }
    }
    if !batch {
        let count: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM lottery_tickets WHERE game='scratch' AND period=0 AND user_id=$1 AND settled_at IS NULL AND cost=100 AND entry->'reward_version'='1'::jsonb")
            .bind(user).fetch_one(&mut *tx).await?;
        if count >= 50 {
            return Err(Error::TooManyTickets);
        }
    }
    let charged = body
        .cost
        .checked_mul(body.count as i64)
        .ok_or(Error::InvalidEntry)?;
    if charged > before {
        return Err(Error::InsufficientBalance);
    }
    // Consume the previously published commitment in this same transaction.
    let challenge = &body.fairness;
    let proof = draw_fairness::claim(&mut tx, user, "scratch", body.request_id, challenge)
        .await
        .map_err(|error| match error {
            draw_fairness::ClaimError::Invalid => Error::InvalidEntry,
            draw_fairness::ClaimError::Conflict => Error::RequestConflict,
            draw_fairness::ClaimError::Db(error) => Error::Db(error),
        })?;
    let generated =
        scratch::generate(&proof, body.cost, body.count).map_err(|_| Error::InvalidEntry)?;
    let rewards =
        scratch::rewards(&proof, body.cost, body.count).map_err(|_| Error::InvalidEntry)?;
    let now = Utc::now();
    let mut cards = Vec::with_capacity(body.count);
    for (index, card) in generated.into_iter().enumerate() {
        let fairness = json!({
            "kind": "scratch", "reveal": proof,
            "cost": body.cost, "count": body.count, "index": index,
            "numbers": card.numbers, "lucky": card.lucky,
            "prize": card.prize, "hits": card.hits,
        });
        let entry = json!({
            "cost": body.cost, "lucky": card.lucky, "numbers": card.numbers,
            "fairness": fairness, "reward_version":1, "reward":rewards[index],
        });
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
            prize: batch.then_some(card.prize),
            tier: batch.then_some(card.hits),
            settled_at: batch.then_some(now),
            proof_plan: None,
        });
    }
    // Bonus rows precede the purchase row so its stored balance also replays
    // the final post-bonus balance after a lost response.
    let mut membership_granted = false;
    if batch {
        for card in &mut cards {
            membership_granted |= grant_reward(&mut tx, user, card).await?;
        }
    }
    let paid = cards
        .iter()
        .filter_map(|t| t.prize)
        .try_fold(0i64, |sum, prize| {
            sum.checked_add(prize).ok_or(Error::InvalidEntry)
        })?;
    let ids: Vec<Uuid> = cards.iter().map(|t| t.id).collect();
    let after = wallet::settle(&mut tx, POLICY.game, user, &wallet::Play {
        record_id, delta: paid - charged, completes_hand: batch, won: paid > 0,
        room_code: None, revision: None, hand_completed: None, has_bots: None,
        details: Some(json!({"purpose":if batch {"scratch_batch"} else {"purchase"}, "game":"scratch", "body_sha256":hash, "ticket_ids":ids, "charged":charged, "balance_before":before})),
    }).await?;
    // One bounded insert keeps a 1,000-card purchase from issuing a query per
    // card while the account wallet is locked. All cards share reveal state.
    let entries: Vec<Value> = cards.iter().map(|card| card.entry.clone()).collect();
    let prizes: Vec<Option<i64>> = cards.iter().map(|card| card.prize).collect();
    let tiers: Vec<Option<i16>> = cards.iter().map(|card| card.tier).collect();
    sqlx::query(
        "INSERT INTO lottery_tickets (id,purchase_id,game,period,user_id,bought_at,entry,stakes,multiplier,cost,revealed,prize,tier,settled_at) \
         SELECT u.id,$5,'scratch',0,$6,$7,u.entry,1,1,$8,$9,u.prize,u.tier,$10 \
         FROM UNNEST($1::uuid[],$2::jsonb[],$3::int8[],$4::int2[]) AS u(id,entry,prize,tier)",
    )
    .bind(&ids)
    .bind(&entries)
    .bind(&prizes)
    .bind(&tiers)
    .bind(record_id)
    .bind(user)
    .bind(now)
    .bind(body.cost)
    .bind(vec![batch; 6])
    .bind(batch.then_some(now))
    .execute(&mut *tx)
    .await?;
    let notified = if batch {
        notify_winners(&mut tx, user, &cards).await?
    } else {
        false
    };
    tx.commit().await?;
    if notified {
        crate::read_cache::invalidate_unread_count(pool, user);
    }
    wallet::project_session_balance(POLICY.game, user, after);
    if membership_granted {
        crate::entitlement::invalidate_tier(pool, user);
    }
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
    let rows = sqlx::query(&format!("SELECT {TICKET_COLUMNS} FROM lottery_tickets WHERE game='scratch' AND period=0 AND user_id=$1 AND cost=100 AND entry->'reward_version'='1'::jsonb AND settled_at IS NULL UNION SELECT {TICKET_COLUMNS} FROM lottery_tickets WHERE purchase_id=(SELECT purchase_id FROM lottery_tickets WHERE game='scratch' AND user_id=$1 AND cost=100 AND entry->'reward_version'='1'::jsonb ORDER BY bought_at DESC,id DESC LIMIT 1) ORDER BY bought_at DESC,id"))
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
    let row = sqlx::query(&format!("SELECT {TICKET_COLUMNS} FROM lottery_tickets WHERE id=$1 AND game='scratch' AND user_id=$2 AND cost=100 AND entry->'reward_version'='1'::jsonb FOR UPDATE"))
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
    let mut membership_granted = false;
    let mut notified = false;
    if all {
        let (amount, tier) = if balance.is_some() {
            prize(&card.entry, card.cost)?
        } else {
            (0, 0)
        };
        paid = amount;
        if balance.is_some() {
            membership_granted = grant_reward(&mut tx, user, &mut card).await?;
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
        if balance.is_some() {
            notified = notify_winner(&mut tx, user, &card).await?;
        }
    }
    card.revealed = Some(shown);
    sqlx::query(
        "UPDATE lottery_tickets SET revealed=$2,prize=$3,tier=$4,settled_at=$5,entry=$6 WHERE id=$1",
    )
    .bind(card.id)
    .bind(&card.revealed)
    .bind(card.prize)
    .bind(card.tier)
    .bind(card.settled_at)
    .bind(&card.entry)
    .execute(&mut *tx)
    .await?;
    tx.commit().await?;
    if notified {
        crate::read_cache::invalidate_unread_count(pool, user);
    }
    if balance.is_some() {
        wallet::project_session_balance(POLICY.game, user, after);
    }
    if membership_granted {
        crate::entitlement::invalidate_tier(pool, user);
    }
    if all && balance.is_some() {
        emit_paid(user, paid);
    }
    Ok((store::ticket_json(Game::Scratch, &card), after))
}

/// Caller holds the user, wallet and (for reveals) ticket locks. Award, ledger,
/// entitlement and settlement commit or roll back together; replay never enters
/// this function for an already settled card/purchase.
async fn grant_reward(
    conn: &mut PgConnection,
    user: Uuid,
    card: &mut TicketRow,
) -> Result<bool, Error> {
    let reward: Option<scratch::Reward> =
        serde_json::from_value(card.entry["reward"].clone()).map_err(|_| Error::InvalidEntry)?;
    match reward {
        Some(scratch::Reward::Beans { amount }) => {
            sqlx::query(
                "WITH updated AS (UPDATE wallets SET balance=balance+$3,updated_at=now() WHERE user_id=$1 AND game=$2 RETURNING balance) \
                 INSERT INTO wallet_ledger(user_id,game,kind,delta,balance_after,details) \
                 SELECT $1,$2,'bonus',$3,balance,$4 FROM updated"
            ).bind(user).bind(POLICY.game).bind(amount)
                .bind(json!({"purpose":"scratch_reward","ticket_id":card.id,"reward_version":1}))
                .execute(&mut *conn).await?;
            Ok(false)
        }
        Some(scratch::Reward::Membership { tier, days }) => {
            if tier != "max" || days != 30 {
                return Err(Error::InvalidEntry);
            }
            let grant = crate::game_rewards::grant_max_on(conn, user, "scratch", card.id).await?;
            card.entry["reward_grant"] = json!(grant);
            Ok(true)
        }
        None => Ok(false),
    }
}

/// Include both number-match winnings and the service reward actually granted.
/// This runs only after the caller has credited the card in the same transaction.
async fn notify_winner(
    conn: &mut PgConnection,
    user: Uuid,
    card: &TicketRow,
) -> Result<bool, Error> {
    notify_winners(conn, user, std::slice::from_ref(card)).await
}

/// Aggregate a settled batch into one durable notice, including exact grants.
/// Replay exits before this function, and the purchase key deduplicates it.
async fn notify_winners(
    conn: &mut PgConnection,
    user: Uuid,
    cards: &[TicketRow],
) -> Result<bool, Error> {
    let Some(first) = cards.first() else {
        return Ok(false);
    };
    let mut prize = 0i64;
    let mut bonus = 0i64;
    let mut winning_tickets = 0usize;
    let mut grants = Vec::new();
    for card in cards {
        let amount = card.prize.unwrap_or(0);
        prize = prize.checked_add(amount).ok_or(Error::InvalidEntry)?;
        let reward: Option<scratch::Reward> = serde_json::from_value(card.entry["reward"].clone())
            .map_err(|_| Error::InvalidEntry)?;
        if amount > 0 || reward.is_some() {
            winning_tickets += 1;
        }
        match reward {
            Some(scratch::Reward::Beans { amount }) => {
                bonus = bonus.checked_add(amount).ok_or(Error::InvalidEntry)?;
            }
            Some(scratch::Reward::Membership { .. }) => {
                let grant: crate::game_rewards::Grant =
                    serde_json::from_value(card.entry["reward_grant"].clone())
                        .map_err(|_| Error::InvalidEntry)?;
                grants.push(grant);
            }
            None => {}
        }
    }
    if winning_tickets == 0 {
        return Ok(false);
    }
    let membership = if grants.is_empty() {
        Value::Null
    } else {
        json!({
            "tier": "max", "days": 30, "grants": grants.len(),
            "entitlement_id": grants.last().map(|grant| grant.entitlement_id),
            "starts_at_ms": grants.first().map(|grant| grant.starts_at_ms),
            "ends_at_ms": grants.last().map(|grant| grant.ends_at_ms),
        })
    };
    let credited = prize.checked_add(bonus).ok_or(Error::InvalidEntry)?;
    let zh_title = "刮刮乐中奖奖励已到账";
    let en_title = "Scratch-card winnings credited";
    let mut zh_body = format!(
        "本次 {} 张刮刮乐中有 {winning_tickets} 张中奖。中奖派彩 {prize} 金豆、额外奖励 {bonus} 金豆，共 {credited} 金豆已自动到账，无需领取。",
        cards.len(),
    );
    let mut en_body = format!(
        "{winning_tickets} of {} scratch cards won. Your {prize} gold-bean prize and {bonus} bonus beans ({credited} total) were credited automatically. No claim needed.",
        cards.len(),
    );
    if !grants.is_empty() {
        let days = grants.len() * 30;
        zh_body.push_str(&format!(
            "Max 会员共 {days} 天已自动增加到会员期限，无需领取。"
        ));
        en_body.push_str(&format!(" {days} days of Max membership were added to your membership term automatically. No claim needed."));
    }
    let key = if cards.len() == 1 {
        // Single reveals retain the existing ticket-based identity.
        format!("lottery-win:{}", super::scratch_credit_id(first.id))
    } else {
        format!("lottery-win:scratch-batch:{}", first.purchase_id)
    };
    sqlx::query(
        "INSERT INTO notifications (id,user_id,type,title,body,payload,dedupe_key) \
         VALUES ($1,$2,'rank',$3,$4,$5,$6) \
         ON CONFLICT (user_id,dedupe_key) WHERE dedupe_key IS NOT NULL DO NOTHING",
    )
    .bind(super::record_id(&key))
    .bind(user)
    .bind(zh_title)
    .bind(&zh_body)
    .bind(json!({
        "lottery": {
            "game": "scratch", "period": 0, "ticket_id": first.id,
            "purchase_id": first.purchase_id,
            "prize": prize, "bonus_beans": bonus, "credited_beans": credited,
            "ticket_count": cards.len(), "winning_tickets": winning_tickets, "membership": membership,
        },
        "i18n": {"zh": {"title": zh_title, "body": zh_body}, "en": {"title": en_title, "body": en_body}},
    }))
    .bind(key)
    .execute(conn)
    .await?;
    Ok(true)
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
    let rows = sqlx::query("SELECT id,user_id FROM lottery_tickets WHERE game='scratch' AND cost=100 AND entry->'reward_version'='1'::jsonb AND settled_at IS NULL AND bought_at < now()-interval '30 days' ORDER BY bought_at,id LIMIT 20")
        .fetch_all(pool).await?;
    for row in rows {
        match reveal_inner(pool, row.get("user_id"), row.get("id"), None, true).await {
            Ok(_) | Err(Error::TicketNotFound | Error::AlreadySettled) => {}
            Err(error) => return Err(error),
        }
    }
    Ok(())
}
