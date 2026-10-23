//! Deterministic three-dice draws and the actual small/big payout rules.
//!
//! A `sicbo` draw mixes the committed server seed with the bettor's client
//! seed. A `sicbo-room` draw uses an empty client seed so everyone at a table
//! sees the same dice. The room's server therefore knows the result before
//! betting; a commitment detects substitution but cannot prevent withholding.
use crate::proof::Reveal;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};

pub const MIN_STAKE: i64 = 10;
pub const MAX_STAKE: i64 = 1_000;

#[derive(Debug, Clone, Copy, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum Side {
    Small,
    Big,
}

/// Replays three uniform 1..=6 draws from the verified seed transcript.
pub fn draw(reveal: &Reveal) -> Result<[u8; 3], String> {
    if !matches!(reveal.commitment.game.as_str(), "sicbo" | "sicbo-room") {
        return Err("not a sicbo reveal".into());
    }
    if reveal.commitment.game == "sicbo-room" && !reveal.client_seed.is_empty() {
        return Err("sicbo-room requires an empty client seed".into());
    }
    let mut entropy = reveal.entropy()?;
    Ok(std::array::from_fn(|_| (entropy.below(6) + 1) as u8))
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

/// The room-wide proof contains no individual wager or account information.
pub fn draw_proof(reveal: &Reveal, dice: [u8; 3]) -> Value {
    json!({
        "kind": reveal.commitment.game,
        "reveal": reveal,
        "dice": dice,
        "total": dice.iter().sum::<u8>(),
        "triple": dice[0] == dice[1] && dice[1] == dice[2],
    })
}

/// Attaches the payout rule to a replayed draw, for either a solo or room bet.
pub fn payout_proof(reveal: &Reveal, dice: [u8; 3], side: Side, stake: i64) -> Value {
    let mut proof = draw_proof(reveal, dice);
    proof["side"] = json!(side);
    proof["stake"] = json!(stake);
    proof["won"] = json!(wins(dice, side));
    proof["delta"] = json!(delta(dice, side, stake));
    proof
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Proof {
    kind: String,
    reveal: Reveal,
    dice: [u8; 3],
    total: u8,
    triple: bool,
    side: Option<Side>,
    stake: Option<i64>,
    won: Option<bool>,
    delta: Option<i64>,
}

/// Checks the commitment, exact dice, summary and (when present) payout.
/// Returns the reconstructed proof; any changed asserted result is rejected.
pub fn verify(value: &Value) -> Result<Value, String> {
    let proof: Proof = serde_json::from_value(value.clone()).map_err(|e| e.to_string())?;
    if proof.kind != proof.reveal.commitment.game {
        return Err("sicbo proof kind does not match the committed game".into());
    }
    let dice = draw(&proof.reveal)?;
    if proof.dice != dice
        || proof.total != dice.iter().sum::<u8>()
        || proof.triple != (dice[0] == dice[1] && dice[1] == dice[2])
    {
        return Err("sicbo draw does not match the reveal".into());
    }
    match (proof.side, proof.stake, proof.won, proof.delta) {
        (Some(side), Some(stake), Some(won), Some(claimed_delta)) => {
            if !(MIN_STAKE..=MAX_STAKE).contains(&stake) {
                return Err("sicbo stake is outside the allowed range".into());
            }
            if won != wins(dice, side) || claimed_delta != delta(dice, side, stake) {
                return Err("sicbo payout does not match the rules".into());
            }
            Ok(payout_proof(&proof.reveal, dice, side, stake))
        }
        (None, None, None, None) if proof.kind == "sicbo-room" => {
            Ok(draw_proof(&proof.reveal, dice))
        }
        _ => Err("sicbo payout proof must include side, stake, won and delta".into()),
    }
}
