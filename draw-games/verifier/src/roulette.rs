//! Single-zero roulette draw and payout, shared by production and the verifier.
use crate::proof::Entropy;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};

/// A round stakes 1..=MAX_BET whole beans in total.
pub const MAX_BET: i64 = 100_000_000;
const MAX_WAGERS: usize = 49;
const RED: [u8; 18] = [
    1, 3, 5, 7, 9, 12, 14, 16, 18, 19, 21, 23, 25, 27, 30, 32, 34, 36,
];

#[derive(Clone, Deserialize, Serialize, PartialEq)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum Selection {
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
pub struct Wager {
    pub selection: Selection,
    pub amount: i64,
}

pub fn stake(wagers: &[Wager]) -> Option<i64> {
    if wagers.is_empty()
        || wagers.len() > MAX_WAGERS
        || !wagers.iter().all(|w| w.selection.valid() && w.amount > 0)
    {
        return None;
    }
    let total = wagers
        .iter()
        .try_fold(0i64, |sum, w| sum.checked_add(w.amount))?;
    (1..=MAX_BET).contains(&total).then_some(total)
}

pub fn settle(wagers: &[Wager], entropy: &mut Entropy) -> Result<Value, String> {
    let bet = stake(wagers).ok_or("Invalid roulette wagers")?;
    let number = entropy.below(37) as u8;
    let payout = wagers
        .iter()
        .map(|w| w.amount * w.selection.multiplier(number))
        .sum::<i64>();
    Ok(json!({"wagers": wagers, "number": number, "bet": bet, "payout": payout}))
}
