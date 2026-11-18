//! Deterministic three-dice draws and the actual small/big payout rules.
//!
//! A `sicbo` draw mixes the committed server seed with the bettor's client
//! seed. A `sicbo-room` draw commits the server seed before betting opens, then
//! folds in a 32-byte seed from the browser of every player whose wager the
//! round settles ([`room_client_seed`]), so everyone at a table sees the same
//! dice and the server cannot know them before the last such seed arrives. A
//! player confirms their own seed is listed; the server can still refuse a
//! wager (and its seed) before accepting it. The verifier cannot tell a
//! server-added seed from a bettor's, so players compare the listed count with
//! the human bettors at the table.
use crate::proof::{seed_bytes, sha256_fields, Reveal};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};

/// A wager is any whole stake from 1 up to the bettor's balance (梭哈 stakes
/// all of it); the server refuses a stake above what the bettor can cover.
pub const MIN_STAKE: i64 = 1;
pub const MAX_STAKE: i64 = 1_000_000_000_000;

#[derive(Debug, Clone, Copy, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum Side {
    Small,
    Big,
}

/// Room seats (humans and AI) bound how many player seeds a round can list.
pub const MAX_ROOM_SEEDS: usize = 8;

/// The room reveal's client seed: SHA-256 over the round's player seeds in
/// ascending order, or empty when no player wagered. Seeds are 64 lowercase
/// hex characters and distinct.
pub fn room_client_seed(player_seeds: &[String]) -> Result<String, String> {
    if player_seeds.is_empty() {
        return Ok(String::new());
    }
    if player_seeds.len() > MAX_ROOM_SEEDS {
        return Err("too many sicbo-room player seeds".into());
    }
    if !player_seeds.windows(2).all(|pair| pair[0] < pair[1]) {
        return Err("sicbo-room player seeds must be sorted and distinct".into());
    }
    let seeds = player_seeds
        .iter()
        .map(|seed| seed_bytes(seed))
        .collect::<Result<Vec<_>, _>>()?;
    let mut fields: Vec<&[u8]> = vec![b"bluffking-draw-v1/sicbo-room-players"];
    fields.extend(seeds.iter().map(|seed| seed.as_slice()));
    Ok(hex::encode(sha256_fields(&fields)))
}

/// Replays three uniform 1..=6 draws from the verified seed transcript. A room
/// reveal's client seed must be [`room_client_seed`] of `player_seeds`; a solo
/// reveal lists none.
pub fn draw(reveal: &Reveal, player_seeds: &[String]) -> Result<[u8; 3], String> {
    match reveal.commitment.game.as_str() {
        "sicbo" if player_seeds.is_empty() => {}
        "sicbo" => return Err("solo sicbo lists no room player seeds".into()),
        "sicbo-room" => {
            if reveal.client_seed != room_client_seed(player_seeds)? {
                return Err("sicbo-room client seed does not match the player seeds".into());
            }
        }
        _ => return Err("not a sicbo reveal".into()),
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

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Proof {
    kind: String,
    reveal: Reveal,
    dice: [u8; 3],
    total: u8,
    triple: bool,
    /// Room rounds settled before player seeds existed list none.
    #[serde(default)]
    player_seeds: Vec<String>,
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
    let dice = draw(&proof.reveal, &proof.player_seeds)?;
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
            Ok(payout_proof(
                &proof.reveal,
                dice,
                &proof.player_seeds,
                side,
                stake,
            ))
        }
        (None, None, None, None) if proof.kind == "sicbo-room" => {
            Ok(draw_proof(&proof.reveal, dice, &proof.player_seeds))
        }
        _ => Err("sicbo payout proof must include side, stake, won and delta".into()),
    }
}
