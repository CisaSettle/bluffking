//! Deterministic three-dice draws and the actual 鱼虾蟹 (Fish Prawn Crab)
//! payout rules.
//!
//! Each die shows one of six symbols; a face value 1..=6 maps to
//! [`Symbol::ALL`] in order. A bettor stakes on one or more symbols. For each
//! staked symbol that shows on `k >= 1` dice the bettor wins `k × stake`;
//! a symbol that shows on no die loses its stake.
//!
//! Each new round publishes a fixed close time and the first drand quicknet
//! pulse at least one second later. Historical seed proofs can still be replayed.
use crate::beacon::{self, Beacon};
use crate::proof::{self, sha256_fields, Entropy, Reveal};
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
fn draw(reveal: &Reveal, player_seeds: &[String]) -> Result<[u8; 3], String> {
    if reveal.commitment.game != "yuxiaxie-room" {
        return Err("not a yuxiaxie reveal".into());
    }
    proof::dice(reveal, player_seeds)
}

pub const BEACON_GAME: &str = "yuxiaxie-beacon";
pub const BEACON_PROTOCOL: &str = "bluffking-yuxiaxie-drand-v1";
pub const REVEAL_WAIT_MS: i64 = 1_000;

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct Plan {
    pub protocol: String,
    pub game: String,
    pub draw_id: String,
    pub close_ms: i64,
    pub beacon_round: u64,
    pub commitment: String,
}

impl Plan {
    pub fn new(draw_id: String, close_ms: i64) -> Result<Self, String> {
        if draw_id.is_empty() || draw_id.len() > 128 || !draw_id.is_ascii() {
            return Err("invalid Fish Prawn Crab round identity".into());
        }
        let draw_ms = close_ms
            .checked_add(REVEAL_WAIT_MS)
            .ok_or("invalid Fish Prawn Crab close time")?;
        let beacon_round = beacon::round_at_or_after(draw_ms)?;
        let commitment = sha256_fields(&[
            b"bluffking-yuxiaxie-drand-v1/plan",
            beacon::CHAIN_HASH.as_bytes(),
            draw_id.as_bytes(),
            &close_ms.to_be_bytes(),
            &beacon_round.to_be_bytes(),
        ]);
        Ok(Self {
            protocol: BEACON_PROTOCOL.into(),
            game: BEACON_GAME.into(),
            draw_id,
            close_ms,
            beacon_round,
            commitment: hex::encode(commitment),
        })
    }
}

pub fn beacon_draw(plan: &Plan, pulse: &Beacon) -> Result<[u8; 3], String> {
    if *plan != Plan::new(plan.draw_id.clone(), plan.close_ms)? || plan.beacon_round != pulse.round
    {
        return Err("Fish Prawn Crab beacon differs from the committed plan".into());
    }
    let randomness = beacon::verify(pulse)?;
    let mut entropy = Entropy::new(sha256_fields(&[
        b"bluffking-yuxiaxie-drand-v1/outcome",
        plan.commitment.as_bytes(),
        &randomness,
    ]));
    Ok(std::array::from_fn(|_| (entropy.below(6) + 1) as u8))
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

fn faces(dice: [u8; 3]) -> Vec<Symbol> {
    dice.iter()
        .filter_map(|die| Symbol::ALL.get(usize::from(*die).wrapping_sub(1)).copied())
        .collect()
}

/// The room-wide proof contains no individual wager or account information.
pub fn beacon_proof(plan: &Plan, pulse: &Beacon, dice: [u8; 3]) -> Value {
    json!({
        "kind": BEACON_GAME,
        "plan": plan,
        "beacon": pulse,
        "dice": dice,
        "faces": faces(dice),
    })
}

/// Attaches the payout rule to a replayed draw for one bettor's wager.
pub fn with_payout(mut proof: Value, dice: [u8; 3], bets: &Bets) -> Value {
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
    reveal: Option<Reveal>,
    player_seeds: Option<Vec<String>>,
    plan: Option<Plan>,
    beacon: Option<Beacon>,
    dice: [u8; 3],
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
    let (dice, expected) = match (&*proof.kind, proof.reveal, proof.plan, proof.beacon) {
        ("yuxiaxie-room", Some(reveal), None, None) => {
            let seeds = proof
                .player_seeds
                .as_deref()
                .ok_or("Fish Prawn Crab proof requires the player seed list")?;
            let dice = draw(&reveal, seeds)?;
            let expected = json!({
                "kind": proof.kind,
                "reveal": reveal,
                "dice": dice,
                "faces": faces(dice),
                "player_seeds": seeds,
            });
            (dice, expected)
        }
        (BEACON_GAME, None, Some(plan), Some(pulse)) if proof.player_seeds.is_none() => {
            let dice = beacon_draw(&plan, &pulse)?;
            (dice, beacon_proof(&plan, &pulse, dice))
        }
        _ => return Err("invalid Fish Prawn Crab proof type".into()),
    };
    if proof.dice != dice || json!(proof.faces) != expected["faces"] {
        return Err("Fish Prawn Crab result differs from the proof".into());
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
            if proof.player_seeds.as_ref().is_some_and(Vec::is_empty) {
                return Err("A settled wager requires a player contribution".into());
            }
            Ok(with_payout(expected, dice, &bets))
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
        let mut proof = with_payout(
            json!({"kind":game,"reveal":reveal,"dice":dice,"faces":faces(dice),"player_seeds":player_seeds}),
            dice,
            &bets,
        );
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
        receipt.as_object_mut().unwrap().remove("client_seed");
        assert!(verify_document(&proof, Some(&receipt)).is_err());
        proof.as_object_mut().unwrap().remove("player_seeds");
        assert!(verify_document(&proof, None).is_err());
        proof["player_seeds"] = json!([player_seeds[1]]);
        assert!(verify_document(&proof, None).is_err());
    }
}
