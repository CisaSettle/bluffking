//! Two timescales: daily population recipe selection, live seat-specific reads
//! still run through decide_with_opponents on EVERY action. No cards leave a hand.
//! This is bounded empirical tuning, not an equilibrium solver or a win guarantee.
use super::{BotPersona, PersonaProfile, PERSONA_ROSTER};
use chrono::{DateTime, NaiveDate, Utc};
use rand::{Rng, SeedableRng};
use serde::{Deserialize, Serialize};
use std::sync::{LazyLock, RwLock};
use uuid::Uuid;

mod storage;
pub use storage::{record, run};

// Bump for ANY recipe/decision/reward change; never compare incompatible recipes.
pub const RECIPE_VERSION: i32 = 17;
const MODES: usize = 3;
const ARMS: usize = 3;
static POLICY: LazyLock<RwLock<Policy>> = LazyLock::new(|| RwLock::new(Policy::default()));
static PROFILES: LazyLock<[[PersonaProfile; ARMS]; 7]> = LazyLock::new(|| {
    PERSONA_ROSTER.map(|base| {
        std::array::from_fn(|arm| {
            let mut p = base;
            let shift = match arm {
                1 => -4,
                2 => 4,
                _ => 0,
            };
            let adjust = |value: u8, amount: i16| (i16::from(value) + amount).clamp(1, 95) as u8;
            p.rfi_pct = p.rfi_pct.map(|v| adjust(v, shift / 2));
            p.cbet_dry_pct = adjust(p.cbet_dry_pct, shift);
            p.cbet_wet_pct = adjust(p.cbet_wet_pct, shift);
            p.barrel_dry_pct = adjust(p.barrel_dry_pct, shift);
            p.barrel_wet_pct = adjust(p.barrel_wet_pct, shift);
            p.probe_turn_pct = adjust(p.probe_turn_pct, shift);
            p.delayed_cbet_pct = adjust(p.delayed_cbet_pct, shift);
            p.third_barrel_pct = adjust(p.third_barrel_pct, shift);
            p
        })
    })
});

/// Stable recipes preserve persona identity and all legality/value/range guards.
pub fn profile(persona: BotPersona, arm: u8) -> &'static PersonaProfile {
    if persona == BotPersona::Shark {
        return super::lookup_profile(persona);
    }
    &PROFILES[persona as usize][usize::from(arm).min(ARMS - 1)]
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
struct Policy {
    run_date: Option<NaiveDate>,
    arms: [[u8; 7]; MODES],
}
impl Policy {
    fn valid(&self) -> bool {
        self.arms.iter().flatten().all(|a| usize::from(*a) < ARMS)
    }
}

#[derive(Clone, Debug)]
pub struct HandPolicy {
    hand_id: Uuid,
    mode: i16,
    bb: u32,
    seats: Vec<SeatAssignment>,
    collect: bool,
}
#[derive(Clone, Debug, Serialize)]
struct SeatAssignment {
    seat: u8,
    persona: u8,
    arm: u8,
    exploration: bool,
    start: u64,
}
#[derive(Clone, Debug, Serialize)]
struct SeatOutcome {
    #[serde(flatten)]
    assignment: SeatAssignment,
    delta: i64,
    squid_delta: i64,
    reward: f64,
}
#[derive(Clone, Debug)]
pub struct CompletedSample {
    hand_id: Uuid,
    mode: i16,
    bb: u32,
    completed_at: DateTime<Utc>,
    seats: Vec<SeatOutcome>,
}
impl HandPolicy {
    /// Inputs are public hand-start stacks and persona labels, never a deck seed.
    /// Sort seats before randomization: HashMap iteration cannot change assignment.
    pub fn new(
        hand_id: Uuid,
        mode: i16,
        bb: u32,
        seats: impl IntoIterator<Item = (u8, BotPersona, u64)>,
    ) -> Self {
        let policy = POLICY.read().unwrap_or_else(|e| e.into_inner()).clone();
        Self::assigned(hand_id, mode, bb, seats, &policy, storage::enabled())
    }
    fn assigned(
        hand_id: Uuid,
        mode: i16,
        bb: u32,
        seats: impl IntoIterator<Item = (u8, BotPersona, u64)>,
        policy: &Policy,
        enabled: bool,
    ) -> Self {
        let mut seats: Vec<_> = seats.into_iter().collect();
        seats.sort_by_key(|s| s.0);
        let mode = mode.clamp(0, (MODES - 1) as i16);
        // Independent entropy; neither deal RNG nor action RNG is consumed.
        let mut rng = rand::rngs::StdRng::from_entropy();
        let focal = if enabled && !seats.is_empty() && rng.gen_bool(0.30) {
            Some(rng.gen_range(0..seats.len()))
        } else {
            None
        };
        let assignments = seats
            .into_iter()
            .enumerate()
            .map(|(i, (seat, persona, start))| SeatAssignment {
                seat,
                persona: persona as u8,
                start,
                exploration: persona != BotPersona::Shark && focal == Some(i),
                arm: if persona == BotPersona::Shark {
                    0
                } else if focal == Some(i) {
                    rng.gen_range(0..ARMS) as u8
                } else {
                    policy.arms[mode as usize][persona as usize]
                },
            })
            .collect();
        Self {
            hand_id,
            mode,
            bb,
            seats: assignments,
            collect: enabled,
        }
    }
    pub fn profile(&self, seat: u8, persona: BotPersona) -> &'static PersonaProfile {
        self.seats
            .iter()
            .find(|s| s.seat == seat && s.persona == persona as u8)
            .map_or_else(
                || super::lookup_profile(persona),
                |s| profile(persona, s.arm),
            )
    }
    /// Replay has no original recipe assignment: keep playing, never relabel it.
    pub fn disable_collection(&mut self) {
        self.collect = false;
    }
    pub fn complete(&self, final_stack: impl Fn(u8) -> Option<i64>) -> Option<CompletedSample> {
        if !self.collect || self.bb == 0 || self.seats.is_empty() {
            return None;
        }
        let seats: Option<Vec<_>> = self
            .seats
            .iter()
            .map(|s| {
                let end = final_stack(s.seat)?;
                let delta = end.checked_sub(i64::try_from(s.start).ok()?)?;
                Some(SeatOutcome {
                    assignment: s.clone(),
                    delta,
                    squid_delta: 0,
                    reward: (delta as f64 / f64::from(self.bb)).clamp(-100.0, 100.0),
                })
            })
            .collect();
        Some(CompletedSample {
            hand_id: self.hand_id,
            mode: self.mode,
            bb: self.bb,
            completed_at: Utc::now(),
            seats: seats?,
        })
    }
}

impl CompletedSample {
    /// Include the actual side-game cash flow settled by this hand, including
    /// virtual buy-in-funded penalties. Never treat that funding as profit.
    /// This is immediate net return, not a learned multi-hand token value.
    pub fn add_squid_transfers(&mut self, transfers: &[crate::protocol::SquidTransfer]) {
        for seat in &mut self.seats {
            let delta: i128 = transfers
                .iter()
                .map(|t| {
                    i128::from(t.amount)
                        * (i128::from(t.to_seat == seat.assignment.seat)
                            - i128::from(t.from_seat == seat.assignment.seat))
                })
                .sum();
            let total = i128::from(seat.squid_delta) + delta;
            let Ok(squid_delta) = i64::try_from(total) else {
                self.seats.clear();
                return;
            };
            seat.squid_delta = squid_delta;
            seat.reward = ((seat.delta as f64 + seat.squid_delta as f64) / f64::from(self.bb))
                .clamp(-100.0, 100.0);
        }
    }
}

#[derive(Clone, Copy, Debug, Default, Serialize)]
struct Stats {
    n: i64,
    mean: f64,
    variance: f64,
}
fn improves(candidate: Stats, incumbent: Stats, minimum: i64) -> bool {
    candidate.n >= minimum
        && incumbent.n >= minimum
        && candidate.mean.is_finite()
        && incumbent.mean.is_finite()
        && candidate.variance.is_finite()
        && incumbent.variance.is_finite()
        && candidate.mean - incumbent.mean
            > 0.05
                + 4.0
                    * (candidate.variance.max(0.0) / candidate.n as f64
                        + incumbent.variance.max(0.0) / incumbent.n as f64)
                        .sqrt()
}
fn select(incumbent: u8, train: [Stats; ARMS], holdout: [Stats; ARMS]) -> u8 {
    // Select on training only; the fresh day is a gate, never an arm selector.
    let candidate = (0..ARMS)
        .filter(|&a| a != incumbent as usize && improves(train[a], train[incumbent as usize], 200))
        .max_by(|&a, &b| train[a].mean.total_cmp(&train[b].mean));
    candidate
        .filter(|&a| improves(holdout[a], holdout[incumbent as usize], 50))
        .map_or(incumbent, |a| a as u8)
}
