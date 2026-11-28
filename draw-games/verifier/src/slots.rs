//! Fixed-stake slots draw and payout, shared by production and the verifier.
use crate::proof::Entropy;
use serde_json::{json, Value};

pub const BET: i64 = 1;
const MULTIPLIERS: [i64; 6] = [3, 5, 8, 14, 25, 45];

pub fn settle(entropy: &mut Entropy) -> Value {
    let reels: [usize; 3] = std::array::from_fn(|_| entropy.below(6) as usize);
    let [a, b, c] = reels;
    let multiplier = if a == b && b == c {
        MULTIPLIERS[a]
    } else if a == b || b == c || a == c {
        1
    } else {
        0
    };
    let payout = BET * multiplier;
    let jackpot_bonus = if payout > 0 && entropy.below(10_000) == 0 {
        BET * 5 * MULTIPLIERS[5]
    } else {
        0
    };
    json!({"bet": BET, "payout": payout, "reels": reels, "jackpot_bonus": jackpot_bonus})
}
