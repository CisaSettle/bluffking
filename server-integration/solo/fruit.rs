//! 水果机 (fruit machine): a 24-cell light ring with eight symbol bets and a
//! 大/小 guess. One authoritative draw per request.
use rand::{seq::index::sample, Rng};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};

use super::{Outcome, Refusal, SoloGame, Stake};

/// Symbols, in bet-button order: BAR, 77, star, watermelon, bell, mango, orange, apple.
const SYMBOLS: usize = 8;
/// A symbol bet is 0..=MAX_SYMBOL_BET beans (the cabinet's two-digit meters).
const MAX_SYMBOL_BET: i64 = 99;
/// One 大/小 guess stakes 1..=MAX_GUESS beans.
const MAX_GUESS: i64 = 50_000;
const LUCKY: u8 = 8;

/// One ring cell: (symbol or `LUCKY`, multiplier including stake, draw weight /10 000).
/// The order is the classic cabinet ring, clockwise from the top-left corner.
/// Weights give every symbol the same theoretical return (≈95 %, lucky lights
/// included), so no single bet is better than another.
const CELLS: [(u8, i64, u32); 24] = [
    (6, 10, 254),    // 0  orange (top-left corner)
    (4, 20, 106),    // 1  bell
    (0, 50, 63),     // 2  BAR ×50
    (0, 100, 63),    // 3  BAR ×100
    (7, 5, 254),     // 4  apple
    (7, 3, 962),     // 5  small apple
    (5, 15, 155),    // 6  mango (top-right corner)
    (3, 20, 254),    // 7  watermelon
    (3, 3, 964),     // 8  small watermelon
    (LUCKY, 0, 200), // 9  LUCKY
    (7, 5, 254),     // 10 apple
    (6, 3, 964),     // 11 small orange
    (6, 10, 254),    // 12 orange (bottom-right corner)
    (4, 20, 106),    // 13 bell
    (1, 3, 1126),    // 14 small 77
    (1, 40, 148),    // 15 77
    (7, 5, 254),     // 16 apple
    (5, 3, 884),     // 17 small mango
    (5, 15, 155),    // 18 mango (bottom-left corner)
    (2, 30, 197),    // 19 star
    (2, 3, 1126),    // 20 small star
    (LUCKY, 0, 200), // 21 LUCKY
    (7, 5, 254),     // 22 apple
    (4, 3, 803),     // 23 small bell
];
const TOTAL_WEIGHT: u32 = 10_000;
/// A LUCKY stop lights 2..=4 distinct extra cells drawn uniformly from the
/// cells paying ×20 or less.
const LUCKY_POOL: [u8; 18] = [
    0, 1, 4, 5, 6, 7, 8, 10, 11, 12, 13, 14, 16, 17, 18, 20, 22, 23,
];

#[derive(Clone, Copy, Deserialize, Serialize, PartialEq)]
#[serde(rename_all = "snake_case")]
enum Side {
    Big,
    Small,
}

#[derive(Clone, Deserialize, Serialize, PartialEq)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
enum Bet {
    /// Beans on each symbol, in `SYMBOLS` order.
    Ring { bets: [i64; SYMBOLS] },
    /// 1–6 small, 8–13 big; 7 loses both. A win returns ×2.
    Guess { side: Side, amount: i64 },
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Play {
    play: Bet,
}

pub struct Fruit;

impl SoloGame for Fruit {
    const ID: &'static str = "fruit";
    type Play = Play;

    fn stake(play: &Play) -> Result<Stake, Refusal> {
        let stake = match &play.play {
            Bet::Ring { bets } => {
                if bets.iter().any(|b| !(0..=MAX_SYMBOL_BET).contains(b)) {
                    None
                } else {
                    Some(bets.iter().sum::<i64>()).filter(|s| *s > 0)
                }
            }
            Bet::Guess { amount, .. } => Some(*amount).filter(|a| (1..=MAX_GUESS).contains(a)),
        };
        stake.map(Stake::of).ok_or(Refusal::invalid("invalid_bet"))
    }
    fn same_play(play: &Play, meta: &Value) -> bool {
        serde_json::from_value::<Bet>(meta["play"].clone()).is_ok_and(|bet| bet == play.play)
    }
    fn draw(
        play: &Play,
        bet: i64,
        _: Option<&Value>,
        rng: &mut dyn rand::RngCore,
    ) -> Result<Outcome, Refusal> {
        let mut receipt = json!({"play": play.play, "bet": bet});
        let payout = match &play.play {
            Bet::Ring { bets } => {
                let cell = weighted_cell(rng.gen_range(0..TOTAL_WEIGHT));
                let lit: Vec<u8> = if CELLS[cell as usize].0 == LUCKY {
                    let n = rng.gen_range(2..=4);
                    sample(rng, LUCKY_POOL.len(), n)
                        .into_iter()
                        .map(|i| LUCKY_POOL[i])
                        .collect()
                } else {
                    Vec::new()
                };
                let pays = |c: u8| {
                    let (symbol, multiplier, _) = CELLS[c as usize];
                    if symbol == LUCKY {
                        0
                    } else {
                        bets[symbol as usize] * multiplier
                    }
                };
                receipt["cell"] = json!(cell);
                receipt["lucky"] = json!(lit);
                // Bounded: 99 × 100 on a single stop, or 4 lights × 99 × 20.
                pays(cell) + lit.iter().map(|&c| pays(c)).sum::<i64>()
            }
            Bet::Guess { side, amount } => {
                let number: u8 = rng.gen_range(1..=13);
                receipt["number"] = json!(number);
                let won = match side {
                    Side::Big => number >= 8,
                    Side::Small => number <= 6,
                };
                if won {
                    amount * 2
                } else {
                    0
                }
            }
        };
        receipt["payout"] = json!(payout);
        Ok(Outcome::paid(payout, bet, receipt))
    }
    fn reply(meta: &Value, balance: i64) -> Value {
        let mut reply = json!({
            "request_id": meta["request_id"],
            "play": meta["play"],
            "bet": meta["bet"],
            "payout": meta["payout"],
            "balance": balance,
        });
        for field in ["cell", "number"] {
            if !meta[field].is_null() {
                reply[field] = meta[field].clone();
            }
        }
        if meta["lucky"].as_array().is_some_and(|lit| !lit.is_empty()) {
            reply["lucky"] = meta["lucky"].clone();
        }
        reply
    }
    fn profile() -> Value {
        json!({"max_symbol_bet": MAX_SYMBOL_BET, "max_guess": MAX_GUESS})
    }
}

fn weighted_cell(mut roll: u32) -> u8 {
    for (i, &(_, _, weight)) in CELLS.iter().enumerate() {
        if roll < weight {
            return i as u8;
        }
        roll -= weight;
    }
    unreachable!("weights sum to TOTAL_WEIGHT")
}

const _: () = {
    let mut sum = 0;
    let mut i = 0;
    while i < CELLS.len() {
        sum += CELLS[i].2;
        i += 1;
    }
    assert!(sum == TOTAL_WEIGHT);
    let mut pooled = 0;
    let mut k = 0;
    while k < LUCKY_POOL.len() {
        let (symbol, multiplier, _) = CELLS[LUCKY_POOL[k] as usize];
        assert!(symbol != LUCKY && multiplier <= 20);
        // Strictly ascending, so every pooled cell is distinct.
        assert!(k == 0 || LUCKY_POOL[k] > LUCKY_POOL[k - 1]);
        pooled += 1;
        k += 1;
    }
    let mut eligible = 0;
    let mut c = 0;
    while c < CELLS.len() {
        if CELLS[c].0 != LUCKY && CELLS[c].1 <= 20 {
            eligible += 1;
        }
        c += 1;
    }
    assert!(pooled == eligible);
};
