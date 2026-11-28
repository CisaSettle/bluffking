//! Single-zero roulette draw and payout, shared by production and the verifier.
use crate::proof::{dealer_client_seed, Entropy, Reveal};

pub const ROOM_GAME: &str = "roulette-room";
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
    let payout = payout(wagers, number);
    Ok(json!({"wagers": wagers, "number": number, "bet": bet, "payout": payout}))
}

/// Gross return for an admitted wager at one single-zero pocket.
pub fn payout(wagers: &[Wager], number: u8) -> i64 {
    wagers
        .iter()
        .map(|w| w.amount * w.selection.multiplier(number))
        .sum()
}

pub fn room_number(reveal: &Reveal, seeds: &[String]) -> Result<u8, String> {
    if reveal.commitment.game != ROOM_GAME
        || reveal.client_seed != dealer_client_seed(ROOM_GAME, seeds)?
    {
        return Err("Roulette room contribution does not match the player seeds".into());
    }
    Ok(reveal.entropy()?.below(37) as u8)
}

pub fn verify_room(proof: &Value) -> Result<Value, String> {
    let reveal: Reveal =
        serde_json::from_value(proof["reveal"].clone()).map_err(|e| e.to_string())?;
    let seeds: Vec<String> =
        serde_json::from_value(proof["player_seeds"].clone()).map_err(|e| e.to_string())?;
    let number = room_number(&reveal, &seeds)?;
    if proof["number"] != number {
        return Err("Roulette pocket differs from the replayed draw".into());
    }
    if proof.get("wagers").is_none() {
        if ["stake", "payout", "delta"]
            .iter()
            .any(|key| proof.get(key).is_some())
        {
            return Err("Roulette payout proof is missing wagers".into());
        }
        return Ok(json!({"number":number}));
    }
    let wagers: Vec<Wager> =
        serde_json::from_value(proof["wagers"].clone()).map_err(|e| e.to_string())?;
    let stake = stake(&wagers).ok_or("Invalid roulette wagers")?;
    let payout = payout(&wagers, number);
    let delta = payout - stake;
    if proof["stake"] != stake || proof["payout"] != payout || proof["delta"] != delta {
        return Err("Roulette payout differs from the replayed wager".into());
    }
    Ok(json!({"number":number,"wagers":wagers,"stake":stake,"payout":payout,"delta":delta}))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::proof::commit;

    #[test]
    fn room_replays_one_pocket_and_each_players_payout() {
        let server_seed = "ab".repeat(32);
        let seeds = vec!["12".repeat(32), "34".repeat(32)];
        let reveal = Reveal {
            commitment: commit(ROOM_GAME, "roulette-test-round", &server_seed).unwrap(),
            client_seed: dealer_client_seed(ROOM_GAME, &seeds).unwrap(),
            server_seed,
        };
        let number = room_number(&reveal, &seeds).unwrap();
        let public = json!({"kind":ROOM_GAME,"reveal":reveal,"player_seeds":seeds,"number":number});
        assert_eq!(verify_room(&public).unwrap(), json!({"number":number}));

        for (pocket, payout) in [(number, 180), ((number + 1) % 37, 0)] {
            let mut proof = public.clone();
            proof["wagers"] = json!([Wager {
                selection: Selection::Number { value: pocket },
                amount: 5,
            }]);
            proof["stake"] = json!(5);
            proof["payout"] = json!(payout);
            proof["delta"] = json!(payout - 5);
            let checked = verify_room(&proof).unwrap();
            assert_eq!(checked["number"], number);
            assert_eq!(checked["payout"], payout);
            proof["payout"] = json!(payout + 1);
            assert!(verify_room(&proof).is_err());
        }

        let mut tampered = public;
        tampered["player_seeds"][0] = json!("56".repeat(32));
        assert!(verify_room(&tampered).is_err());
    }
}
