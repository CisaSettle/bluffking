//! Deterministic three-dice draws and the actual 鱼虾蟹 (Fish Prawn Crab)
//! payout rules.
//!
//! Each die shows one of six symbols; a face value 1..=6 maps to
//! [`Symbol::ALL`] in order. A bettor stakes on one or more symbols. For each
//! staked symbol that shows on `k >= 1` dice the bettor wins `k × stake`;
//! a symbol that shows on no die loses its stake.
//!
//! Room rounds mix all settled bettor seeds with the committed server seed.
use crate::proof::{self, Reveal};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::collections::BTreeMap;

/// Each staked symbol takes any whole stake from 1; the server refuses a
/// wager whose total is above what the bettor can cover.
pub const MIN_STAKE: i64 = 1;
pub const MAX_STAKE: i64 = 1_000_000_000_000;

/// The six die faces, in face-value order (1 = fish … 6 = rooster).
#[derive(Debug, Clone, Copy, Deserialize, Serialize, PartialEq, Eq, PartialOrd, Ord, Hash)]
#[serde(rename_all = "snake_case")]
pub enum Symbol {
    Fish,
    Prawn,
    Crab,
    Gourd,
    Coin,
    Rooster,
}

impl Symbol {
    pub const ALL: [Symbol; 6] = [
        Symbol::Fish,
        Symbol::Prawn,
        Symbol::Crab,
        Symbol::Gourd,
        Symbol::Coin,
        Symbol::Rooster,
    ];
    /// The die face value (1..=6) that shows this symbol.
    pub fn face(self) -> u8 {
        self as u8 + 1
    }
}

/// One bettor's whole wager: a stake per chosen symbol.
pub type Bets = BTreeMap<Symbol, i64>;

/// A wager is valid with 1..=6 symbols, each stake within limits, and a total
/// that still fits [`MAX_STAKE`].
pub fn total(bets: &Bets) -> Option<i64> {
    if bets.is_empty() || bets.values().any(|s| !(MIN_STAKE..=MAX_STAKE).contains(s)) {
        return None;
    }
    bets.values()
        .try_fold(0i64, |sum, stake| sum.checked_add(*stake))
        .filter(|sum| *sum <= MAX_STAKE)
}

/// Replays three uniform 1..=6 draws from the verified seed transcript.
pub fn draw(reveal: &Reveal, player_seeds: &[String]) -> Result<[u8; 3], String> {
    if reveal.commitment.game != "yuxiaxie-room" {
        return Err("not a yuxiaxie reveal".into());
    }
    proof::dice(reveal, player_seeds)
}

/// How many of the three dice show `symbol`.
pub fn count(dice: [u8; 3], symbol: Symbol) -> i64 {
    dice.iter().filter(|die| **die == symbol.face()).count() as i64
}

/// Net result of a valid wager (callers check [`total`] first).
pub fn delta(dice: [u8; 3], bets: &Bets) -> i64 {
    bets.iter()
        .map(|(symbol, stake)| match count(dice, *symbol) {
            0 => -stake,
            hits => stake * hits,
        })
        .sum()
}

/// The room-wide proof contains no individual wager or account information.
pub fn draw_proof(reveal: &Reveal, dice: [u8; 3], player_seeds: Option<&[String]>) -> Value {
    let faces: Vec<Symbol> = dice
        .iter()
        .filter_map(|die| Symbol::ALL.get(usize::from(*die).wrapping_sub(1)).copied())
        .collect();
    let mut proof = json!({
        "kind": reveal.commitment.game,
        "reveal": reveal,
        "dice": dice,
        "faces": faces,
    });
    if let Some(seeds) = player_seeds {
        proof["player_seeds"] = json!(seeds);
    }
    proof
}

/// Attaches the payout rule to a replayed draw for one bettor's wager.
pub fn payout_proof(
    reveal: &Reveal,
    dice: [u8; 3],
    bets: &Bets,
    player_seeds: Option<&[String]>,
) -> Value {
    let mut proof = draw_proof(reveal, dice, player_seeds);
    let delta = delta(dice, bets);
    proof["bets"] = json!(bets);
    proof["stake"] = json!(bets.values().sum::<i64>());
    proof["won"] = json!(delta > 0);
    proof["delta"] = json!(delta);
    proof
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Proof {
    kind: String,
    reveal: Reveal,
    dice: [u8; 3],
    player_seeds: Option<Vec<String>>,
    faces: Vec<Symbol>,
    bets: Option<Bets>,
    stake: Option<i64>,
    won: Option<bool>,
    delta: Option<i64>,
}

/// Checks the commitment, exact dice, faces and (when present) payout.
/// Returns the reconstructed proof; any changed asserted result is rejected.
pub fn verify(value: &Value) -> Result<Value, String> {
    let proof: Proof = serde_json::from_value(value.clone()).map_err(|e| e.to_string())?;
    if proof.kind != proof.reveal.commitment.game {
        return Err("yuxiaxie proof kind does not match the committed game".into());
    }
    let seeds = proof.player_seeds.as_deref();
    let dice = draw(&proof.reveal, seeds.unwrap_or_default())?;
    let expected = draw_proof(&proof.reveal, dice, seeds);
    if proof.dice != dice || json!(proof.faces) != expected["faces"] {
        return Err("yuxiaxie draw does not match the reveal".into());
    }
    match (proof.bets, proof.stake, proof.won, proof.delta) {
        (Some(bets), Some(stake), Some(won), Some(claimed_delta)) => {
            if total(&bets) != Some(stake) {
                return Err("yuxiaxie wager is outside the allowed range".into());
            }
            let actual = delta(dice, &bets);
            if won != (actual > 0) || claimed_delta != actual {
                return Err("yuxiaxie payout does not match the rules".into());
            }
            if seeds.is_some_and(<[String]>::is_empty) {
                return Err("A settled wager requires a player contribution".into());
            }
            Ok(payout_proof(&proof.reveal, dice, &bets, seeds))
        }
        (None, None, None, None) => Ok(expected),
        _ => Err("yuxiaxie payout proof must include bets, stake, won and delta".into()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{document::verify_document, proof::commit, proof::dealer_client_seed};

    #[test]
    fn bettor_receipt_checks_their_seed_and_payout() {
        let game = "yuxiaxie-room";
        let draw_id = "room-round-1";
        let server_seed = "23".repeat(32);
        let player_seeds = ["01".repeat(32), "02".repeat(32)];
        let commitment = commit(game, draw_id, &server_seed).unwrap();
        let reveal = Reveal {
            commitment: commitment.clone(),
            server_seed,
            client_seed: dealer_client_seed(game, &player_seeds).unwrap(),
        };
        let dice = draw(&reveal, &player_seeds).unwrap();
        let bets = Bets::from([(Symbol::Fish, 4), (Symbol::Crab, 1)]);
        let mut proof = payout_proof(&reveal, dice, &bets, Some(&player_seeds));
        let mut receipt = json!({
            "protocol": "bluffking-receipt-v1",
            "commitment": commitment,
            "client_seed": player_seeds[0],
            "terms": {"kind":game,"round_id":draw_id,"bets":bets,"stake":5}
        });
        assert_eq!(
            verify_document(&proof, Some(&receipt)).unwrap()["receipt_match"],
            "matched"
        );
        receipt["client_seed"] = json!("03".repeat(32));
        assert!(verify_document(&proof, Some(&receipt)).is_err());
        proof["player_seeds"] = json!([player_seeds[1]]);
        assert!(verify_document(&proof, None).is_err());
    }
}
