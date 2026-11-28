//! Single-zero roulette (轮盘): the rules only; [`super`] plays it.
use rand::Rng;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};

use super::{Outcome, Refusal, SoloGame, Stake};

/// A round stakes 1..=MAX_BET whole beans in total.
const MAX_BET: i64 = 100_000_000;
const MAX_WAGERS: usize = 49;
const RED: [u8; 18] = [
    1, 3, 5, 7, 9, 12, 14, 16, 18, 19, 21, 23, 25, 27, 30, 32, 34, 36,
];

#[derive(Clone, Deserialize, Serialize, PartialEq)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
enum Selection {
    Number { value: u8 },
    Red,
    Black,
    Odd,
    Even,
    Low,
    High,
    Dozen { value: u8 },
    Column { value: u8 },
}

impl Selection {
    fn valid(&self) -> bool {
        match self {
            Self::Number { value } => *value <= 36,
            Self::Dozen { value } | Self::Column { value } => (1..=3).contains(value),
            _ => true,
        }
    }
    fn multiplier(&self, n: u8) -> i64 {
        if let Self::Number { value } = self {
            return if n == *value { 36 } else { 0 };
        }
        if n == 0 {
            return 0;
        }
        let won = match self {
            Self::Red => RED.contains(&n),
            Self::Black => !RED.contains(&n),
            Self::Odd => n % 2 == 1,
            Self::Even => n.is_multiple_of(2),
            Self::Low => n <= 18,
            Self::High => n >= 19,
            Self::Dozen { value } => (n - 1) / 12 + 1 == *value,
            Self::Column { value } => (n - 1) % 3 + 1 == *value,
            Self::Number { .. } => false,
        };
        if !won {
            0
        } else if matches!(self, Self::Dozen { .. } | Self::Column { .. }) {
            3
        } else {
            2
        }
    }
}

#[derive(Clone, Deserialize, Serialize, PartialEq)]
#[serde(deny_unknown_fields)]
struct Wager {
    selection: Selection,
    amount: i64,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Spin {
    wagers: Vec<Wager>,
    #[serde(default)]
    expected_balance: Option<i64>,
}

pub struct Roulette;

impl SoloGame for Roulette {
    const ID: &'static str = "roulette";
    type Play = Spin;

    fn stake(play: &Spin) -> Result<Stake, Refusal> {
        let total = play
            .wagers
            .iter()
            .try_fold(0i64, |sum, w| sum.checked_add(w.amount));
        match total {
            Some(amount)
                if !play.wagers.is_empty()
                    && play.wagers.len() <= MAX_WAGERS
                    && play
                        .wagers
                        .iter()
                        .all(|w| w.selection.valid() && w.amount > 0)
                    && (1..=MAX_BET).contains(&amount)
                    && play
                        .expected_balance
                        .is_none_or(|seen| amount == seen.min(MAX_BET)) =>
            {
                Ok(Stake {
                    amount,
                    all_in: play.expected_balance,
                })
            }
            _ => Err(Refusal::invalid("invalid_bet")),
        }
    }

    fn same_play(play: &Spin, meta: &Value) -> bool {
        serde_json::from_value::<Vec<Wager>>(meta["wagers"].clone())
            .is_ok_and(|wagers| wagers == play.wagers)
    }

    fn draw(
        play: &Spin,
        stake: i64,
        _: Option<&Value>,
        rng: &mut dyn rand::RngCore,
    ) -> Result<Outcome, Refusal> {
        let number = rng.gen_range(0..=36u8);
        // The stake is capped before multiplication: at most MAX_BET * 36.
        let payout = play
            .wagers
            .iter()
            .map(|w| w.amount * w.selection.multiplier(number))
            .sum::<i64>();
        Ok(Outcome::paid(
            payout,
            stake,
            json!({"wagers": play.wagers, "number": number, "bet": stake, "payout": payout}),
        ))
    }

    fn reply(meta: &Value, balance: i64) -> Value {
        json!({"request_id": meta["request_id"], "wagers": meta["wagers"], "number": meta["number"],
            "bet": meta["bet"], "payout": meta["payout"], "balance": balance})
    }

    fn profile() -> Value {
        json!({"max_bet": MAX_BET})
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn all_in_obeys_round_limit() {
        let stake = |amount, expected_balance| {
            Roulette::stake(&Spin {
                wagers: vec![Wager {
                    selection: Selection::Red,
                    amount,
                }],
                expected_balance,
            })
        };

        assert!(stake(MAX_BET + 1, None).is_err());
        assert!(stake(MAX_BET + 1, Some(MAX_BET + 1)).is_err());
        assert!(stake(MAX_BET - 1, Some(MAX_BET + 1)).is_err());
        for (amount, expected) in [
            (MAX_BET - 1, MAX_BET - 1),
            (MAX_BET, MAX_BET),
            (MAX_BET, MAX_BET + 1),
        ] {
            assert!(matches!(
                stake(amount, Some(expected)),
                Ok(Stake { amount: a, all_in: Some(b) }) if a == amount && b == expected
            ));
        }
    }
}
