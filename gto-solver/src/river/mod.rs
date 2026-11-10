//! Bounded multiplayer river solving, with independent information-set best
//! responses. All compatible private-card tuples are integrated, never sampled.
//! CFR+ has no general multiplayer convergence guarantee: only the measured
//! deviation gaps admit a baseline. This is an input-range / action-tree model,
//! not a solution to preflop or a guarantee about omitted betting sizes.

mod cfr;
mod exploit;
mod tree;

use crate::SolveControl;
use engine::{BettingRound, Card, HoleCards, PlayerAction};
pub use exploit::{ExploitLimits, ExploitPolicy, ExploitReport, OpponentLock};
use serde::{Deserialize, Serialize};
use std::time::Instant;
use thiserror::Error;

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct WeightedHand {
    pub hand: HoleCards,
    pub weight: f64,
}

/// Players are in clockwise seat order. Every listed player is live at the
/// river root (zero-stack/all-in players are allowed). Previously folded seats
/// and their card-removal effects are NOT implicitly reconstructed.
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RiverPlayer {
    pub stack: u32,
    pub prior_contribution: u32,
    pub range: Vec<WeightedHand>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RiverRequest {
    pub board: [Card; 5],
    pub players: Vec<RiverPlayer>,
    pub dealer: usize,
    pub big_blind: u32,
    /// Allowed non-all-in raise-TO amounts in chips, used on this river only.
    /// Fold/check/call and legal all-ins are supplied by the engine. No rounding
    /// or nearest-size mapping is performed.
    pub raise_to: Vec<u32>,
    #[serde(default)]
    pub limits: RiverLimits,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RiverLimits {
    pub max_nodes: usize,
    pub max_assignments: usize,
    pub max_depth: usize,
    pub max_storage_bytes: u64,
    pub max_work: u64,
    pub max_iterations: u32,
    /// Every player's improvement from a unilateral best response must be at
    /// most this fraction of the root pot. NOT an average-only acceptance test.
    pub target_max_gap_fraction: f64,
}

impl Default for RiverLimits {
    fn default() -> Self {
        Self {
            max_nodes: 20_000,
            max_assignments: 100_000,
            max_depth: 128,
            max_storage_bytes: 128_000_000,
            max_work: 100_000_000,
            max_iterations: 2_000,
            target_max_gap_fraction: 0.001,
        }
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RiverStep {
    pub player: usize,
    pub action: PlayerAction,
}

#[derive(Clone, Debug, Serialize)]
pub struct DeviationMetrics {
    /// Net chips: total award minus prior and river contributions.
    pub policy_ev: Vec<f64>,
    pub best_response_ev: Vec<f64>,
    pub unilateral_gain: Vec<f64>,
    pub nash_conv_chips: f64,
    pub max_gain_fraction_of_pot: f64,
    /// Evaluation arithmetic slack, not a certified interval-arithmetic bound.
    pub numeric_slack_chips: f64,
}

#[derive(Clone, Debug, Serialize)]
pub struct SolveReport {
    pub admitted: bool,
    pub iterations: u32,
    pub root_pot: u32,
    pub public_nodes: usize,
    pub compatible_assignments: usize,
    pub accounted_storage_bytes: u64,
    pub work: u64,
    pub metrics: DeviationMetrics,
}

#[derive(Clone, Debug, Serialize)]
pub struct RiverDecision {
    pub player: usize,
    pub hand: HoleCards,
    pub actions: Vec<PlayerAction>,
    pub probabilities: Vec<f64>,
    pub pot: u32,
    pub stacks: Vec<u32>,
    pub river_contributions: Vec<u32>,
}

impl RiverDecision {
    pub fn sample(&self, quantile: f64) -> Result<PlayerAction, RiverError> {
        if !quantile.is_finite() || !(0.0..1.0).contains(&quantile) {
            return Err(RiverError::Invalid("quantile must be in [0,1)".into()));
        }
        validate_probabilities(&self.probabilities, self.actions.len())?;
        let threshold = quantile * self.probabilities.iter().sum::<f64>();
        let mut cumulative = 0.0;
        for (action, probability) in self.actions.iter().zip(&self.probabilities) {
            cumulative += probability;
            if *probability > 0.0 && threshold < cumulative {
                return Ok(action.clone());
            }
        }
        Err(RiverError::Numerical)
    }
}

#[derive(Clone, Debug, Serialize)]
pub struct TeacherValue {
    pub hand: HoleCards,
    /// Root posterior probability after all players' card-removal constraints.
    pub joint_reach_probability: f64,
    pub expected_net_chips: f64,
}

#[derive(Debug, Error)]
pub enum RiverError {
    #[error("invalid river input: {0}")]
    Invalid(String),
    #[error("river budget exhausted: {0}; no policy admitted")]
    Budget(&'static str),
    #[error("river solve cancelled")]
    Cancelled,
    #[error("river solve deadline reached; no policy admitted")]
    Deadline,
    #[error("nonfinite or inconsistent river evaluation")]
    Numerical,
    #[error("river baseline has not met its measured deviation target")]
    NotAdmitted,
    #[error("history or hand is not reachable in this policy")]
    Unreachable,
}

struct Joint {
    hands: Vec<usize>,
    probability: f64,
}

struct Node {
    round: BettingRound,
    actor: Option<usize>,
    actions: Vec<PlayerAction>,
    children: Vec<usize>,
    regrets: Vec<f64>,
    average: Vec<f64>,
    // Terminal utility, indexed by compatible tuple then player. Values come
    // from the canonical engine pot settlement, including refunds/odd chips.
    payoffs: Vec<f64>,
}

type Profile = Vec<Vec<f64>>;

struct Model {
    request: RiverRequest,
    nodes: Vec<Node>,
    joints: Vec<Joint>,
    weights: Vec<Vec<f64>>,
    chance_mass: f64,
    root_pot: u32,
}

struct Budget<'a> {
    limits: &'a RiverLimits,
    control: &'a SolveControl,
    work: u64,
    storage: u64,
}

impl<'a> Budget<'a> {
    fn new(limits: &'a RiverLimits, control: &'a SolveControl) -> Self {
        Self {
            limits,
            control,
            work: 0,
            storage: 0,
        }
    }
    fn tick(&mut self, work: u64) -> Result<(), RiverError> {
        self.work = self
            .work
            .checked_add(work)
            .ok_or(RiverError::Budget("work"))?;
        if self.work > self.limits.max_work {
            return Err(RiverError::Budget("work"));
        }
        self.control
            .check_cancel()
            .map_err(|_| RiverError::Cancelled)?;
        if self.control.deadline.is_some_and(|t| Instant::now() >= t) {
            return Err(RiverError::Deadline);
        }
        Ok(())
    }
    fn reserve(&mut self, bytes: u64) -> Result<(), RiverError> {
        self.storage = self
            .storage
            .checked_add(bytes)
            .ok_or(RiverError::Budget("storage"))?;
        if self.storage > self.limits.max_storage_bytes {
            return Err(RiverError::Budget("storage"));
        }
        Ok(())
    }
}

/// A computed profile plus its independently evaluated deviation gaps.
/// An unadmitted result exposes diagnostics only, not an executable strategy.
/// No opponent model can mutate the baseline stored here.
pub struct RiverSolution {
    model: Model,
    profile: Profile,
    report: SolveReport,
}

impl RiverSolution {
    pub fn solve(request: RiverRequest, control: &SolveControl) -> Result<Self, RiverError> {
        let limits = request.limits.clone();
        let mut budget = Budget::new(&limits, control);
        let mut model = Model::build(request, &mut budget)?;
        let (profile, metrics, iterations) = model.train(&mut budget)?;
        let report = SolveReport {
            admitted: metrics.max_gain_fraction_of_pot <= limits.target_max_gap_fraction,
            iterations,
            root_pot: model.root_pot,
            public_nodes: model.nodes.len(),
            compatible_assignments: model.joints.len(),
            accounted_storage_bytes: budget.storage,
            work: budget.work,
            metrics,
        };
        Ok(Self {
            model,
            profile,
            report,
        })
    }
    pub fn report(&self) -> &SolveReport {
        &self.report
    }
    pub fn request(&self) -> &RiverRequest {
        &self.model.request
    }
    pub fn decision(
        &self,
        path: &[RiverStep],
        hand: HoleCards,
    ) -> Result<RiverDecision, RiverError> {
        if !self.report.admitted {
            return Err(RiverError::NotAdmitted);
        }
        self.model.decision(&self.profile, path, hand)
    }
    /// Offline teacher records, conditional on each exact combo. Rejected
    /// baselines cannot contaminate a future value-network training set.
    pub fn teacher_values(
        &self,
        control: &SolveControl,
    ) -> Result<Vec<Vec<TeacherValue>>, RiverError> {
        if !self.report.admitted {
            return Err(RiverError::NotAdmitted);
        }
        let mut budget = Budget::new(&self.model.request.limits, control);
        budget.reserve(self.report.accounted_storage_bytes)?;
        self.model.teacher_values(&self.profile, &mut budget)
    }
}

fn validate_probabilities(values: &[f64], expected: usize) -> Result<(), RiverError> {
    if expected == 0
        || values.len() != expected
        || values
            .iter()
            .any(|p| !p.is_finite() || !(0.0..=1.0).contains(p))
        || (values.iter().sum::<f64>() - 1.0).abs() > 1e-9
    {
        return Err(RiverError::Invalid("invalid action distribution".into()));
    }
    Ok(())
}

fn card_mask(card: Card) -> u64 {
    1 << (card.rank as u64 * 4 + card.suit as u64)
}
fn hand_mask(hand: HoleCards) -> u64 {
    card_mask(hand.card1) | card_mask(hand.card2)
}
