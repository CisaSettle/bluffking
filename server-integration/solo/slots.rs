//! 老虎机: three reels, one fixed 1-bean bet.
use rand::Rng;
use serde::Deserialize;
use serde_json::{json, Value};

use super::{Outcome, Refusal, SoloGame, Stake};

const BET: i64 = 1;
const MULTIPLIERS: [i64; 6] = [3, 5, 8, 14, 25, 45];
/// A winning spin's rare jackpot adds 5 × the top payout (45 × bet).
const JACKPOT_MULTIPLIER: i64 = 5 * MULTIPLIERS[5];

pub struct Slots;

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Play {
    /// Slots have one fixed bet; an all-in request is refused.
    #[serde(default)]
    expected_balance: Option<i64>,
}

impl SoloGame for Slots {
    const ID: &'static str = "slots";
    type Play = Play;

    fn stake(play: &Play) -> Result<Stake, Refusal> {
        if play.expected_balance.is_some() {
            return Err(Refusal::invalid("invalid_bet"));
        }
        Ok(Stake::of(BET))
    }
    fn same_play(_: &Play, _: &Value) -> bool {
        true
    }
    fn draw(
        _: &Play,
        bet: i64,
        _: Option<&Value>,
        rng: &mut dyn rand::RngCore,
    ) -> Result<Outcome, Refusal> {
        let reels: [u8; 3] = [
            rng.gen_range(0..6),
            rng.gen_range(0..6),
            rng.gen_range(0..6),
        ];
        let [a, b, c] = reels;
        let multiplier = if a == b && b == c {
            MULTIPLIERS[a as usize]
        } else if a == b || b == c || a == c {
            1
        } else {
            0
        };
        let payout = bet * multiplier;
        // The jackpot is a rare additional prize; the published reel payout is unchanged.
        let jackpot_bonus = if payout > 0 && rng.gen_ratio(1, 10_000) {
            bet * JACKPOT_MULTIPLIER
        } else {
            0
        };
        Ok(Outcome::paid(
            payout + jackpot_bonus,
            bet,
            json!({"bet": bet, "payout": payout, "reels": reels, "jackpot_bonus": jackpot_bonus}),
        ))
    }
    fn reply(meta: &Value, balance: i64) -> Value {
        json!({
            "request_id": meta["request_id"],
            "reels": meta["reels"],
            "bet": meta["bet"],
            "payout": meta["payout"],
            "jackpot_bonus": meta.get("jackpot_bonus").cloned().unwrap_or(json!(0)),
            "balance": balance,
            "played": true,
        })
    }
}
