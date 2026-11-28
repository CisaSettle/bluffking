//! Deterministic three-dice draws and the actual small/big payout rules.
//!
//! A `sicbo` draw mixes the committed server seed with the bettor's client
//! seed. A `sicbo-room` draw commits the server seed before betting opens, then
//! folds in the sorted, distinct 32-byte seeds of accepted wagers that settle
//! ([`crate::proof::dealer_client_seed`]). Everyone at the table sees the same
//! dice. A receipt can confirm the bettor's own seed was included. The server
//! can refuse a wager or abort the round, and the proof has no signed list
//! authenticating every seed. The count can be compared with the human bettors.
use crate::proof::{self, sha256_fields, Reveal};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};

/// A wager is any whole stake from 1 up to the bettor's balance (梭哈 stakes
/// all of it); the server refuses a stake above what the bettor can cover.
pub const MIN_STAKE: i64 = 1;
pub const MAX_STAKE: i64 = 1_000_000_000_000;

/// A separate purpose-bound stream determines the fixed bean jackpot. The
/// bettor's round seed identifies the wager without exposing an account ID.
pub const JACKPOT_BONUS: i64 = 10;

pub fn jackpot(reveal: &Reveal, player_seed: &str, won: bool) -> i64 {
    if !won {
        return 0;
    }
    let seed = sha256_fields(&[
        b"bluffking-sicbo-jackpot-v2",
        reveal.commitment.game.as_bytes(),
        reveal.commitment.draw_id.as_bytes(),
        reveal.server_seed.as_bytes(),
        reveal.client_seed.as_bytes(),
        player_seed.as_bytes(),
    ]);
    if proof::Entropy::new(seed).below(10_000) == 0 {
        JACKPOT_BONUS
    } else {
        0
    }
}

#[derive(Debug, Clone, Copy, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum Side {
    Small,
    Big,
}

/// Replay the three dice from the verified seed transcript.
pub fn draw(reveal: &Reveal, player_seeds: &[String]) -> Result<[u8; 3], String> {
    if !matches!(reveal.commitment.game.as_str(), "sicbo" | "sicbo-room") {
        return Err("Not a Sic Bo reveal".into());
    }
    proof::dice(reveal, player_seeds)
}

pub fn wins(dice: [u8; 3], side: Side) -> bool {
    if dice.iter().any(|die| !(1..=6).contains(die)) || (dice[0] == dice[1] && dice[1] == dice[2]) {
        return false;
    }
    let total: u8 = dice.iter().sum();
    match side {
        Side::Small => (4..=10).contains(&total),
        Side::Big => (11..=17).contains(&total),
    }
}

/// Callers validate the positive stake against MIN_STAKE..=MAX_STAKE first.
pub fn delta(dice: [u8; 3], side: Side, stake: i64) -> i64 {
    if wins(dice, side) {
        stake
    } else {
        -stake
    }
}

/// The room-wide proof contains no individual wager or account information:
/// player seeds are random and listed in sorted order, not by seat.
pub fn draw_proof(reveal: &Reveal, dice: [u8; 3], player_seeds: &[String]) -> Value {
    let mut proof = json!({
        "kind": reveal.commitment.game,
        "reveal": reveal,
        "dice": dice,
        "total": dice.iter().sum::<u8>(),
        "triple": dice[0] == dice[1] && dice[1] == dice[2],
    });
    if reveal.commitment.game == "sicbo-room" {
        proof["player_seeds"] = json!(player_seeds);
    }
    proof
}

/// Attaches the payout rule to a replayed draw, for either a solo or room bet.
pub fn payout_proof(
    reveal: &Reveal,
    dice: [u8; 3],
    player_seeds: &[String],
    side: Side,
    stake: i64,
) -> Value {
    let mut proof = draw_proof(reveal, dice, player_seeds);
    proof["side"] = json!(side);
    proof["stake"] = json!(stake);
    proof["won"] = json!(wins(dice, side));
    proof["delta"] = json!(delta(dice, side, stake));
    proof
}

/// Attach the separately credited bean jackpot to the bettor's own proof.
pub fn add_jackpot(proof: &mut Value, player_seed: &str, bonus: i64) {
    proof["jackpot_seed"] = json!(player_seed);
    proof["jackpot_bonus"] = json!(bonus);
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Proof {
    kind: String,
    reveal: Reveal,
    dice: [u8; 3],
    total: u8,
    triple: bool,
    player_seeds: Option<Vec<String>>,
    side: Option<Side>,
    stake: Option<i64>,
    won: Option<bool>,
    delta: Option<i64>,
    jackpot_seed: Option<String>,
    jackpot_bonus: Option<i64>,
}

/// Checks the commitment, exact dice, summary and (when present) payout.
/// Returns the reconstructed proof; any changed asserted result is rejected.
pub fn verify(value: &Value) -> Result<Value, String> {
    let proof: Proof = serde_json::from_value(value.clone()).map_err(|e| e.to_string())?;
    if proof.kind != proof.reveal.commitment.game {
        return Err("sicbo proof kind does not match the committed game".into());
    }
    let seeds = match (proof.kind.as_str(), proof.player_seeds.as_deref()) {
        ("sicbo-room", Some(seeds)) => seeds,
        ("sicbo", None) => &[],
        _ => return Err("Unexpected seed list for the Sic Bo game".into()),
    };
    let dice = draw(&proof.reveal, seeds)?;
    if proof.dice != dice
        || proof.total != dice.iter().sum::<u8>()
        || proof.triple != (dice[0] == dice[1] && dice[1] == dice[2])
    {
        return Err("sicbo draw does not match the reveal".into());
    }
    match (proof.side, proof.stake, proof.won, proof.delta) {
        (Some(side), Some(stake), Some(won), Some(claimed_delta)) => {
            if proof.kind == "sicbo-room" && seeds.is_empty() {
                return Err("A settled wager requires a player contribution".into());
            }
            if !(MIN_STAKE..=MAX_STAKE).contains(&stake) {
                return Err("sicbo stake is outside the allowed range".into());
            }
            if won != wins(dice, side) || claimed_delta != delta(dice, side, stake) {
                return Err("sicbo payout does not match the rules".into());
            }
            let mut replayed = payout_proof(&proof.reveal, dice, seeds, side, stake);
            match (proof.jackpot_seed, proof.jackpot_bonus) {
                (None, None) => {}
                (Some(seed), Some(bonus))
                    if proof.kind == "sicbo-room"
                        && seeds.contains(&seed)
                        && jackpot(&proof.reveal, &seed, won) == bonus =>
                {
                    add_jackpot(&mut replayed, &seed, bonus);
                }
                _ => {
                    return Err(
                        "The Sic Bo jackpot does not match the bettor seed and rules".into(),
                    )
                }
            }
            Ok(replayed)
        }
        (None, None, None, None)
            if proof.kind == "sicbo-room"
                && proof.jackpot_seed.is_none()
                && proof.jackpot_bonus.is_none() =>
        {
            Ok(draw_proof(&proof.reveal, dice, seeds))
        }
        _ => Err("sicbo payout proof must include side, stake, won and delta".into()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{document::verify_document, proof::commit, proof::dealer_client_seed};

    #[test]
    fn jackpot_uses_the_bean_bettors_saved_seed() {
        let game = "sicbo-room";
        let draw_id = "room-round-1";
        let server_seed = format!("{:064x}", 0x267);
        let seeds = ["01".repeat(32)];
        let commitment = commit(game, draw_id, &server_seed).unwrap();
        let reveal = Reveal {
            commitment: commitment.clone(),
            server_seed,
            client_seed: dealer_client_seed(game, &seeds).unwrap(),
        };
        let dice = draw(&reveal, &seeds).unwrap();
        assert_eq!(dice, [6, 6, 4]);
        assert_eq!(jackpot(&reveal, &seeds[0], true), JACKPOT_BONUS);
        let mut proof = payout_proof(&reveal, dice, &seeds, Side::Big, 5);
        let receipt = json!({
            "protocol":"bluffking-receipt-v1", "commitment":commitment, "client_seed":seeds[0],
            "terms":{"kind":game,"round_id":draw_id,"unit":"beans","side":"big","stake":5}
        });
        assert!(verify_document(&proof, Some(&receipt)).is_err());
        add_jackpot(&mut proof, &seeds[0], JACKPOT_BONUS);
        assert_eq!(
            verify_document(&proof, Some(&receipt)).unwrap()["receipt_match"],
            "matched"
        );
        proof["jackpot_bonus"] = json!(0);
        assert!(verify_document(&proof, Some(&receipt)).is_err());

        let seeds = ["01".repeat(32), "02".repeat(32)];
        let reveal = Reveal {
            client_seed: dealer_client_seed(game, &seeds).unwrap(),
            ..reveal
        };
        let dice = draw(&reveal, &seeds).unwrap();
        let mut proof = payout_proof(&reveal, dice, &seeds, Side::Big, 5);
        let mut receipt = receipt;
        for unit in [Value::Null, json!("unknown"), json!("practice")] {
            receipt["terms"]["unit"] = unit;
            assert!(verify_document(&proof, Some(&receipt)).is_ok());
        }
        add_jackpot(
            &mut proof,
            &seeds[0],
            jackpot(&reveal, &seeds[0], wins(dice, Side::Big)),
        );
        for unit in [Value::Null, json!("unknown"), json!("practice")] {
            receipt["terms"]["unit"] = unit;
            assert!(verify_document(&proof, Some(&receipt)).is_err());
        }
        receipt["terms"]["unit"] = json!("beans");
        receipt["client_seed"] = json!(seeds[1]);
        assert!(verify_document(&proof, Some(&receipt)).is_err());
    }

    #[test]
    fn room_proof_requires_seed_list_and_receipt_requires_bettor_seed() {
        let server_seed = "00".repeat(32);
        let commitment = commit("sicbo-room", "round-1", &server_seed).unwrap();
        let mut reveal = Reveal {
            commitment,
            server_seed,
            client_seed: String::new(),
        };
        let dice = draw(&reveal, &[]).unwrap();
        let mut proof = draw_proof(&reveal, dice, &[]);
        assert!(verify(&proof).is_ok());
        proof.as_object_mut().unwrap().remove("player_seeds");
        assert!(verify(&proof).is_err());
        assert!(verify(&payout_proof(&reveal, dice, &[], Side::Big, 5)).is_err());

        let player_seed = "01".repeat(32);
        let seeds = [player_seed.clone()];
        reveal.client_seed = dealer_client_seed("sicbo-room", &seeds).unwrap();
        let dice = draw(&reveal, &seeds).unwrap();
        let proof = payout_proof(&reveal, dice, &seeds, Side::Big, 5);
        let mut receipt = json!({
            "protocol": "bluffking-receipt-v1",
            "commitment": reveal.commitment,
            "terms": {"kind": "sicbo-room", "round_id": "round-1", "side": "big", "stake": 5}
        });
        assert!(verify_document(&proof, None).is_ok());
        assert!(verify_document(&proof, Some(&receipt)).is_err());
        receipt["client_seed"] = json!(player_seed);
        assert!(verify_document(&proof, Some(&receipt)).is_ok());
    }
}
