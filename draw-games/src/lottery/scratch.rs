//! Persisted scratch cards. Hidden numbers are projected out of every response.
use super::{
    handlers::Error,
    store::{TicketRow, TICKET_COLUMNS},
};
use crate::db_writer::PgConnection;
use crate::draw_fairness;
use draw_games::scratch;
use serde_json::{json, Value};
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
    /// Reward table the client saved in its pre-purchase receipt; must be
    /// [`scratch::REWARD_VERSION`].
    pub reward_version: u8,
}

impl ScratchBuy {
    /// Idempotency identity of the purchase.
    pub fn identity(&self) -> Value {
        let mut identity = json!({"game":"scratch", "cost":self.cost, "count":self.count,
            "reward_version":self.reward_version,
            "fairness": {"offer_id":self.fairness.offer_id,"commitment":self.fairness.commitment,"client_seed":self.fairness.client_seed}});
        if self.all_in {
            identity["all_in"] = json!(true);
            identity["expected_balance"] = json!(self.expected_balance);
        }
        identity
    }
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
            "reward_version": ticket.entry["reward_version"], "reward": ticket.entry["reward"],
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
    out["reward_version"] = ticket.entry["reward_version"].clone();
    out["reward"] = if all && ticket.settled_at.is_some() {
        ticket.entry["reward"].clone()
    } else {
        Value::Null
    };
    // Beans actually credited for the reward.
    out["reward_beans"] = if all && ticket.settled_at.is_some() {
        ticket.entry["reward_beans"].clone()
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

pub async fn my_cards(conn: &mut PgConnection, user: Uuid) -> sqlx::Result<Vec<TicketRow>> {
    let rows = sqlx::query(&format!("SELECT {TICKET_COLUMNS} FROM lottery_tickets WHERE game='scratch' AND period=0 AND user_id=$1 AND settled_at IS NULL UNION SELECT {TICKET_COLUMNS} FROM lottery_tickets WHERE purchase_id=(SELECT purchase_id FROM lottery_tickets WHERE game='scratch' AND user_id=$1 ORDER BY bought_at DESC,id DESC LIMIT 1) ORDER BY bought_at DESC,id LIMIT 1051"))
        .bind(user).fetch_all(conn).await?;
    Ok(rows.iter().map(TicketRow::from_row).collect())
}

/// Beans credited for a card's stored service reward.
pub(super) fn reward_beans(card: &TicketRow) -> Result<i64, Error> {
    let reward: Option<scratch::Reward> =
        serde_json::from_value(card.entry["reward"].clone()).map_err(|_| Error::InvalidEntry)?;
    reward
        .map_or(Ok(0), |reward| reward.settled_beans())
        .map_err(|_| Error::InvalidEntry)
}

/// Aggregate the settled cards of one event into one durable notice, including
/// service rewards. Replay exits before this function, and the key (the
/// ticket for a single card, the purchase for a batch) deduplicates it.
pub(super) async fn notify_winners(
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
    for card in cards {
        let amount = card.prize.unwrap_or(0);
        prize = prize.checked_add(amount).ok_or(Error::InvalidEntry)?;
        let reward = reward_beans(card)?;
        if amount > 0 || reward > 0 {
            winning_tickets += 1;
        }
        bonus = bonus.checked_add(reward).ok_or(Error::InvalidEntry)?;
    }
    if winning_tickets == 0 {
        return Ok(false);
    }
    let credited = prize.checked_add(bonus).ok_or(Error::InvalidEntry)?;
    let zh_title = "刮刮乐中奖奖励已到账";
    let en_title = "Scratch-card winnings credited";
    let zh_body = format!(
        "本次 {} 张刮刮乐中有 {winning_tickets} 张中奖。中奖派彩 {prize} 金豆、额外奖励 {bonus} 金豆，共 {credited} 金豆已自动到账，无需领取。",
        cards.len(),
    );
    let en_body = format!(
        "{winning_tickets} of {} scratch cards won. Your {prize} gold-bean prize and {bonus} bonus beans ({credited} total) were credited automatically. No claim needed.",
        cards.len(),
    );
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
            "ticket_count": cards.len(), "winning_tickets": winning_tickets,
        },
        "i18n": {"zh": {"title": zh_title, "body": zh_body}, "en": {"title": en_title, "body": en_body}},
    }))
    .bind(key)
    .execute(conn.writer())
    .await?;
    Ok(true)
}
