//! Frozen, queryable two-player postflop continuation policies.
//!
//! This is the solver component of a future baseline-first bot, not a complete
//! poker bot. Root ranges are supplied assumptions; preflop, multiplayer,
//! off-tree actions and safe subgame re-solving are not provided here. A low
//! ROOT exploitability does not bound conditional error at every descendant.
//! No opponent model or node lock can mutate this baseline.

use super::*;

/// Exact tree action, in the solver's chip units. Bet/Raise/AllIn amounts are
/// the acting player's TOTAL commitment on the CURRENT street (not an added
/// payment, and not its total contribution since the root). A runtime adapter
/// must independently match these amounts to the poker engine's legal actions.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(tag = "action", content = "amount", rename_all = "snake_case")]
pub enum PolicyAction {
    Fold,
    Check,
    Call,
    Bet(i32),
    Raise(i32),
    AllIn(i32),
}

impl PolicyAction {
    fn from_solver(action: PfAction) -> Result<Self, PolicyError> {
        Ok(match action {
            PfAction::Fold => Self::Fold,
            PfAction::Check => Self::Check,
            PfAction::Call => Self::Call,
            PfAction::Bet(n) => Self::Bet(n),
            PfAction::Raise(n) => Self::Raise(n),
            PfAction::AllIn(n) => Self::AllIn(n),
            _ => return Err(PolicyError::InvalidStrategy),
        })
    }
}

/// Public history from the solved root. An explicit actor detects swapped
/// positions; chance steps specify the actual dealt card, not a branch index.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
pub enum PolicyStep {
    Act {
        player: Player,
        action: PolicyAction,
    },
    Deal(Card),
}

/// Measured quality of the unlocked ROOT policy, for exactly the root request
/// and the wrapper's disclosed tree settings. This is neither a full-game
/// certificate nor a bound on a queried node's conditional exploitability.
#[derive(Debug, Clone, Copy, PartialEq, Serialize)]
pub struct RootQuality {
    pub exploitability_chips: f32,
    /// A fraction, not percentage points: 0.005 means 0.5% of the root pot.
    pub exploitability_fraction_of_pot: f32,
    pub target_fraction_of_pot: f32,
    pub iterations: u32,
    pub memory_bytes: u64,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct PolicyFrequency {
    pub action: PolicyAction,
    pub probability: f32,
}

/// One exact combo's continuation at a reachable decision node. Kept distinct
/// from `SolveOutput`: the quality figure belongs to the root, not this node.
#[derive(Debug, Clone, Serialize)]
pub struct PolicyDecision {
    pub player: Player,
    pub street: engine::hand::Street,
    pub board: Vec<Card>,
    /// All contributions since the root, in OOP/IP order, across all streets.
    pub contributed_since_root: [i32; 2],
    pub committed_this_street: [i32; 2],
    pub pot: i32,
    pub remaining_stacks: [i32; 2],
    pub hand: HoleCards,
    pub actions: Vec<PolicyFrequency>,
    pub equity: f32,
    pub ev: f32,
    pub root_quality: RootQuality,
}

impl PolicyDecision {
    /// Sample this combo's mixed policy using the caller's RNG quantile.
    /// No randomness, range averaging or hand-authored override is hidden here.
    pub fn sample(&self, quantile: f64) -> Result<PolicyAction, PolicyError> {
        if !quantile.is_finite() || !(0.0..1.0).contains(&quantile) {
            return Err(PolicyError::InvalidQuantile);
        }
        let total = validate_distribution(&self.actions)?;
        // Rescale only accepted floating-point summation noise. This cannot
        // turn a zero-probability action into a sampled action at either edge.
        let target = quantile * total;
        let mut cumulative = 0.0;
        for row in &self.actions {
            cumulative += f64::from(row.probability);
            if row.probability > 0.0 && target < cumulative {
                return Ok(row.action);
            }
        }
        Err(PolicyError::InvalidStrategy)
    }
}

#[derive(Debug, Error, PartialEq)]
pub enum PolicyError {
    #[error(transparent)]
    Solve(#[from] SolveError),
    #[error("policy requires a finite nonnegative tolerance and a positive iteration cap")]
    InvalidQualityTarget,
    #[error("root policy did not meet the requested exploitability bound: achieved={achieved_fraction}, target={target_fraction}, iterations={iterations}, stop={stop:?}")]
    NotConverged {
        achieved_fraction: f32,
        target_fraction: f32,
        iterations: u32,
        stop: SolveStop,
    },
    #[error("history step {step} does not exactly match the solved tree")]
    InvalidHistory { step: usize },
    #[error("history ends at a chance or terminal node, not a player decision")]
    NotDecision,
    #[error("decision has no positive blocker-compatible joint reach")]
    Unreachable,
    #[error("hero combo has no positive blocker-compatible reach at this node")]
    HeroNotReachable,
    #[error("solver returned an invalid strategy or value")]
    InvalidStrategy,
    #[error("sampling quantile must be finite and in [0, 1)")]
    InvalidQuantile,
}

/// Exclusive ownership of one frozen, unlocked policy. Queries reset the
/// cursor before AND after traversal, including failures. No game internals or
/// mutation hooks escape this type; callers cannot alter the baseline.
pub struct SolvedPostflop {
    game: PostFlopGame,
    request: SolveRequest,
    quality: RootQuality,
}

impl SolvedPostflop {
    /// Solve and admit a baseline only after its measured root error meets the
    /// requested bound. An iteration/deadline stop is not silently admitted.
    /// Use on an isolated compute worker, never on the async game actor.
    pub fn solve(mut request: SolveRequest, ctl: &SolveControl) -> Result<Self, PolicyError> {
        let target = request.limits.target_exploitability_pct_of_pot;
        if !target.is_finite() || target < 0.0 || request.limits.max_iterations == 0 {
            return Err(PolicyError::InvalidQualityTarget);
        }
        // Hero is a later lookup only, never an input to the equilibrium.
        request.hero = None;
        let (game, run) = solve_game(&request, ctl)?;
        let fraction = run.exploitability / request.starting_pot as f32;
        if run.stop != SolveStop::Converged
            || !fraction.is_finite()
            || fraction < 0.0
            || fraction > target
        {
            return Err(PolicyError::NotConverged {
                achieved_fraction: fraction,
                target_fraction: target,
                iterations: run.iterations,
                stop: run.stop,
            });
        }
        let quality = RootQuality {
            exploitability_chips: run.exploitability,
            exploitability_fraction_of_pot: fraction,
            target_fraction_of_pot: target,
            iterations: run.iterations,
            memory_bytes: game.memory_usage().0,
        };
        Ok(Self {
            game,
            request,
            quality,
        })
    }

    pub fn root_quality(&self) -> RootQuality {
        self.quality
    }

    /// Look up the actual actor's exact combo after the complete public path.
    /// Fails closed on unsupported actions; never maps to a nearby bet size or
    /// restarts a new solve with guessed ranges. OOP/IP are fixed from the root.
    pub fn decision(
        &mut self,
        history: &[PolicyStep],
        hero: HoleCards,
    ) -> Result<PolicyDecision, PolicyError> {
        self.game.back_to_root();
        let result = self.decision_from_root(history, hero);
        self.game.back_to_root();
        result
    }

    fn decision_from_root(
        &mut self,
        history: &[PolicyStep],
        hero: HoleCards,
    ) -> Result<PolicyDecision, PolicyError> {
        let mut street_start_contributions = [0, 0];
        for (step, action) in history.iter().enumerate() {
            let invalid = || PolicyError::InvalidHistory { step };
            if self.game.is_terminal_node() {
                return Err(invalid());
            }
            let index = match *action {
                PolicyStep::Deal(card) if self.game.is_chance_node() => {
                    let id = card_from_str(&card.to_string()).map_err(|_| invalid())?;
                    if self.game.possible_cards() & (1u64 << id) == 0 {
                        return Err(invalid());
                    }
                    street_start_contributions = self.game.total_bet_amount();
                    usize::from(id)
                }
                PolicyStep::Act { player, action }
                    if !self.game.is_chance_node()
                        && self.game.current_player() == player.index() =>
                {
                    self.game
                        .available_actions()
                        .iter()
                        .position(|&a| PolicyAction::from_solver(a).ok() == Some(action))
                        .ok_or_else(invalid)?
                }
                _ => return Err(invalid()),
            };
            self.game.play(index);
        }
        if self.game.is_terminal_node() || self.game.is_chance_node() {
            return Err(PolicyError::NotDecision);
        }
        let p = self.game.current_player();
        let player = match p {
            0 => Player::Oop,
            1 => Player::Ip,
            _ => return Err(PolicyError::InvalidStrategy),
        };
        self.game.cache_normalized_weights();
        let weights = self.game.normalized_weights(p);
        if weights.iter().any(|w| !w.is_finite() || *w < 0.0) {
            return Err(PolicyError::InvalidStrategy);
        }
        if !weights.iter().any(|w| *w > 0.0) {
            return Err(PolicyError::Unreachable);
        }
        let private = self.game.private_cards(p);
        let a =
            card_from_str(&hero.card1.to_string()).map_err(|_| PolicyError::HeroNotReachable)?;
        let b =
            card_from_str(&hero.card2.to_string()).map_err(|_| PolicyError::HeroNotReachable)?;
        let combo = (a.min(b), a.max(b));
        let h = private
            .iter()
            .position(|&c| c == combo)
            .filter(|&i| weights.get(i).is_some_and(|w| *w > 0.0))
            .ok_or(PolicyError::HeroNotReachable)?;
        let available = self.game.available_actions();
        let strategy = self.game.strategy();
        if strategy.len() != available.len() * private.len() {
            return Err(PolicyError::InvalidStrategy);
        }
        let actions: Vec<_> = available
            .into_iter()
            .enumerate()
            .map(|(i, a)| {
                Ok(PolicyFrequency {
                    action: PolicyAction::from_solver(a)?,
                    probability: strategy[i * private.len() + h],
                })
            })
            .collect::<Result<_, PolicyError>>()?;
        validate_distribution(&actions)?;
        let equity = *self
            .game
            .equity(p)
            .get(h)
            .ok_or(PolicyError::InvalidStrategy)?;
        let ev = *self
            .game
            .expected_values(p)
            .get(h)
            .ok_or(PolicyError::InvalidStrategy)?;
        if !equity.is_finite() || !(0.0..=1.0).contains(&equity) || !ev.is_finite() {
            return Err(PolicyError::InvalidStrategy);
        }
        let contributed = self.game.total_bet_amount();
        let board: Vec<Card> = self
            .game
            .current_board()
            .into_iter()
            .map(|id| {
                let card_text = postflop_solver::card_to_string(id)
                    .map_err(|_| PolicyError::InvalidStrategy)?;
                engine::card::parse_card(&card_text).ok_or(PolicyError::InvalidStrategy)
            })
            .collect::<Result<_, _>>()?;
        let street = match board.len() {
            3 => engine::hand::Street::Flop,
            4 => engine::hand::Street::Turn,
            5 => engine::hand::Street::River,
            _ => return Err(PolicyError::InvalidStrategy),
        };
        Ok(PolicyDecision {
            player,
            street,
            board,
            contributed_since_root: contributed,
            committed_this_street: [
                contributed[0] - street_start_contributions[0],
                contributed[1] - street_start_contributions[1],
            ],
            pot: self.request.starting_pot + contributed[0] + contributed[1],
            remaining_stacks: contributed.map(|n| self.request.effective_stack - n),
            hand: hero,
            actions,
            equity,
            ev,
            root_quality: self.quality,
        })
    }
}

fn validate_distribution(actions: &[PolicyFrequency]) -> Result<f64, PolicyError> {
    if actions.is_empty()
        || actions
            .iter()
            .any(|a| !a.probability.is_finite() || !(0.0..=1.0).contains(&a.probability))
    {
        return Err(PolicyError::InvalidStrategy);
    }
    let sum: f64 = actions.iter().map(|a| f64::from(a.probability)).sum();
    if (sum - 1.0).abs() > 1e-5 {
        return Err(PolicyError::InvalidStrategy);
    }
    Ok(sum)
}
