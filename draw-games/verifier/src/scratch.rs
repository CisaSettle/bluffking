//! Reproducible scratch cards and their prize calculation.
//!
//! A purchase consumes one verified entropy stream. Cards are generated in
//! index order; each consumes six numbers and then its lucky number. A card's
//! seed is disclosed only after all its spots are revealed (batches reveal
//! every card immediately).

use crate::proof::Reveal;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};

pub const PRIZES: [i64; 6] = [1, 1, 2, 2, 5, 10];

/// Version-one service prizes are additive to the unchanged number-match payout.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum Reward {
    Beans { amount: i64 },
    Membership { tier: String, days: i64 },
}

/// Reproducible reward suffix after the current purchase's number draws.
pub fn rewards(reveal: &Reveal, cost: i64, count: usize) -> Result<Vec<Option<Reward>>, String> {
    if reveal.commitment.game != "scratch" || cost != 100 || ![1, 5, 10].contains(&count) {
        return Err("invalid service reward purchase".into());
    }
    let mut entropy = reveal.entropy()?;
    for _ in 0..count * 7 {
        entropy.below(30);
    }
    Ok((0..count)
        .map(|_| match entropy.below(10_000) {
            0..=19 => Some(Reward::Beans { amount: 10_000 }),
            20 => Some(Reward::Beans { amount: 100_000 }),
            21..=30 => Some(Reward::Beans { amount: 1_000 }),
            31 => Some(Reward::Membership {
                tier: "max".into(),
                days: 30,
            }),
            _ => None,
        })
        .collect())
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Card {
    pub numbers: [i16; 6],
    pub lucky: i16,
    pub prize: i64,
    pub hits: i16,
}

/// The same award calculation is used by the server and independent verifier.
pub fn payout(cost: i64, lucky: i16, numbers: &[i16; 6]) -> Result<(i64, i16), String> {
    if cost != 100
        || !(1..=30).contains(&lucky)
        || numbers.iter().any(|number| !(1..=30).contains(number))
    {
        return Err("invalid scratch card".into());
    }
    let mut prize = 0;
    let mut hits = 0;
    for (number, multiplier) in numbers.iter().zip(PRIZES) {
        if *number == lucky {
            prize += cost * multiplier;
            hits += 1;
        }
    }
    Ok((prize, hits))
}

/// Replay every card in a purchase, without an operating-system RNG.
pub fn generate(reveal: &Reveal, cost: i64, count: usize) -> Result<Vec<Card>, String> {
    if reveal.commitment.game != "scratch" {
        return Err("scratch proof has a different game".into());
    }
    if cost != 100 || ![1, 5, 10].contains(&count) {
        return Err("invalid scratch purchase".into());
    }
    let mut entropy = reveal.entropy()?;
    let mut cards = Vec::with_capacity(count);
    for _ in 0..count {
        let numbers = std::array::from_fn(|_| entropy.below(30) as i16 + 1);
        let lucky = entropy.below(30) as i16 + 1;
        let (prize, hits) = payout(cost, lucky, &numbers)?;
        cards.push(Card {
            numbers,
            lucky,
            prize,
            hits,
        });
    }
    Ok(cards)
}

pub fn replay(reveal: &Reveal, cost: i64, count: usize, index: usize) -> Result<Card, String> {
    if index >= count {
        return Err("scratch card index is outside the purchase".into());
    }
    generate(reveal, cost, count)?
        .into_iter()
        .nth(index)
        .ok_or_else(|| "scratch card index is outside the purchase".into())
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Receipt {
    kind: String,
    reveal: Reveal,
    cost: i64,
    count: usize,
    index: usize,
    numbers: [i16; 6],
    lucky: i16,
    prize: i64,
    hits: i16,
    reward_version: u8,
    #[serde(default)]
    reward: Option<Reward>,
}

/// Verify the commitment, reproduce the card and check the reported award.
/// This verifies the supplied receipt; comparing its commitment with the one
/// retained before purchase is necessary to establish the pre-purchase promise.
pub fn verify(value: &Value) -> Result<Value, String> {
    let receipt: Receipt = serde_json::from_value(value.clone())
        .map_err(|error| format!("invalid scratch receipt: {error}"))?;
    if receipt.kind != "scratch" {
        return Err("receipt is not a scratch card".into());
    }
    let card = replay(&receipt.reveal, receipt.cost, receipt.count, receipt.index)?;
    if receipt.numbers != card.numbers
        || receipt.lucky != card.lucky
        || receipt.prize != card.prize
        || receipt.hits != card.hits
    {
        return Err("scratch card or prize differs from the committed draw".into());
    }
    if receipt.reward_version != 1 {
        return Err("unsupported scratch reward version".into());
    }
    let expected = rewards(&receipt.reveal, receipt.cost, receipt.count)?;
    if expected[receipt.index] != receipt.reward {
        return Err("service reward differs from the committed draw".into());
    }
    Ok(json!({
        "reward_version": receipt.reward_version,
        "reward": receipt.reward,
        "verified": true,
        "kind": "scratch",
        "draw_id": receipt.reveal.commitment.draw_id,
        "cost": receipt.cost,
        "count": receipt.count,
        "index": receipt.index,
        "numbers": card.numbers,
        "lucky": card.lucky,
        "prize": card.prize,
        "hits": card.hits,
    }))
}
