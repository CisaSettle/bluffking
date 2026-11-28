//! Claw eligibility, catch and payout, shared by production and the verifier.
use crate::proof::Entropy;
use serde::{Deserialize, Serialize};

pub const COST: i64 = 1;
pub const PRIZE: i64 = 2;
/// The 0.01% jackpot catch pays 5 × the normal prize.
const JACKPOT_PRIZE: i64 = 5 * PRIZE;
pub const RADIUS: f64 = 0.28;
// One server-owned chance for every toy and every eligible aim.
pub const CATCH_PERCENT: u32 = 30;
pub const POSITIONS: [(f64, f64); 6] = [
    (-1.7, -1.1),
    (0.0, -1.1),
    (1.7, -1.1),
    (-0.65, 0.4),
    (0.6, 0.4),
    (1.85, 0.4),
];

#[derive(Clone, Copy, Deserialize, Serialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct Aim {
    pub x: f64,
    pub z: f64,
}

impl Aim {
    pub fn valid(self) -> bool {
        self.x.is_finite() && self.z.is_finite() && self.x.abs() <= 2.1 && self.z.abs() <= 1.65
    }
}

#[derive(Serialize)]
pub struct Catch {
    pub aim: Aim,
    pub caught: Option<usize>,
    pub jackpot: bool,
    pub payout: i64,
}

pub fn settle(aim: Aim, entropy: &mut Entropy) -> Result<Catch, String> {
    if !aim.valid() {
        return Err("Invalid aim".into());
    }
    // The coordinate range fixes all operands to small finite numbers.
    let caught = POSITIONS
        .iter()
        .position(|(x, z)| {
            let dx = aim.x - x;
            let dz = aim.z - z;
            dx * dx + dz * dz <= RADIUS * RADIUS
        })
        .filter(|_| entropy.below(100) < u64::from(CATCH_PERCENT));
    let jackpot = caught.is_some() && entropy.below(10_000) == 0;
    let payout = match (caught, jackpot) {
        (None, _) => 0,
        (Some(_), true) => JACKPOT_PRIZE,
        (Some(_), false) => PRIZE,
    };
    Ok(Catch {
        aim,
        caught,
        jackpot,
        payout,
    })
}
