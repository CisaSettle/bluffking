//! Heads-up turn / river check-or-jam game for short remaining stacks.
//! Both players' flop actions condition the blueprint ranges through the same
//! public likelihood model. The chance node deals every compatible river.
//! Solver allocation, CFR, best-response measurement and traversal run off actor.

use super::{
    preflop_blueprint::{flop_reach, FlopReach},
    strategy::LineEvidence,
    BotPersona, DecisionContext, PublicAction,
};
use engine::{Card, HoleCards, PlayerAction, Street};
use gto_solver::{
    policy::{PolicyAction, PolicyStep, SolvedPostflop},
    Player, SolveControl, SolveLimits, SolveRequest, SolveStreet,
};
use rand::{rngs::StdRng, Rng};
use std::{sync::LazyLock, time::Instant};

// The global live-solve permit is shared with other bot policies. Limit this
// solver to one CPU even if the process-wide Rayon pool has more threads.
static POOL: LazyLock<Option<rayon::ThreadPool>> = LazyLock::new(|| {
    rayon::ThreadPoolBuilder::new()
        .num_threads(1)
        .thread_name(|_| "shark-turn-cfr".into())
        .build()
        .ok()
});

mod ranges;

pub(crate) struct Request {
    board: [Card; 4],
    seats: [u8; 2], // OOP, IP
    pot: u32,
    stack: u32,
    preflop: FlopReach,
    lines: [LineEvidence; 2],
}

pub(crate) struct Query {
    path: Vec<PolicyStep>,
    hero: HoleCards,
    actor: Player,
    street: Street,
    pot: u32,
    behind: [u32; 2],
    committed: [u32; 2],
    board: Vec<Card>,
    to_call: u32,
    can_raise: bool,
}

pub(crate) struct Decision {
    actions: Vec<(PlayerAction, f32)>,
    iterations: u32,
    // Conservative unilateral improvement bound, including removed joint mass.
    max_gap: f64,
}

impl Decision {
    pub(crate) fn sample(&self, rng: &mut StdRng) -> PlayerAction {
        let mut draw =
            rng.gen::<f64>() * self.actions.iter().map(|(_, p)| f64::from(*p)).sum::<f64>();
        for (action, p) in &self.actions {
            draw -= f64::from(*p);
            if draw < 0.0 {
                return action.clone();
            }
        }
        self.actions
            .last()
            .expect("admitted mixed policy")
            .0
            .clone()
    }
    pub(crate) fn quality(&self) -> (u32, f64) {
        (self.iterations, self.max_gap)
    }
}

impl Request {
    pub(crate) fn from_context(ctx: &DecisionContext<'_>) -> Option<(Self, Query)> {
        (ctx.street == Street::Turn)
            .then(|| Self::new(ctx))
            .flatten()
    }
    /// Only the owner of the hand's turn-policy provenance should call this
    /// after a previously played turn root. No independent river re-root.
    pub(crate) fn after_turn(ctx: &DecisionContext<'_>) -> Option<(Self, Query)> {
        (ctx.street == Street::River)
            .then(|| Self::new(ctx))
            .flatten()
    }

    fn new(ctx: &DecisionContext<'_>) -> Option<(Self, Query)> {
        let table = ctx.table;
        if ctx.persona != BotPersona::Shark
            || !ctx.plain_cash
            || ctx.has_straddle
            || table.seats.len() != 2
        {
            return None;
        }
        let dealer = table.dealer_seat;
        let oop = table.seats.iter().find(|p| p.seat != dealer)?;
        let ip = table.seats.iter().find(|p| p.seat == dealer)?;
        if oop.folded || ip.folded {
            return None;
        }
        let seats = [oop.seat, ip.seat];
        let history = ctx.public_history?;
        let after = &history
            [history.partition_point(|f| matches!(f.street, Street::Preflop | Street::Flop))..];
        // Cheap spot filter prior to any range or replay allocation. Only a
        // check-through can reach another street in the chosen tree.
        let mut original = [oop.stack, ip.stack];
        let mut turn_pot = ctx.pot.0;
        let mut path = Vec::new();
        let mut previous = Street::Turn;
        for fact in after {
            if fact.street != previous {
                if previous != Street::Turn || fact.street != Street::River {
                    return None;
                }
                path.push(PolicyStep::Deal(ctx.board.river?));
                previous = fact.street;
            }
            let idx = seats.iter().position(|&s| s == fact.seat)?;
            original[idx] = original[idx].checked_add(fact.amount)?;
            turn_pot = turn_pot.checked_sub(fact.amount)?;
            let player = [Player::Oop, Player::Ip][idx];
            let action = match fact {
                PublicAction {
                    action: PlayerAction::Check,
                    ..
                } => PolicyAction::Check,
                a if a.increased_bet && a.to_call_before == 0 => {
                    PolicyAction::AllIn(i32::try_from(a.committed_to).ok()?)
                }
                _ => return None,
            };
            path.push(PolicyStep::Act { player, action });
        }
        if ctx.street == Street::River && previous == Street::Turn {
            path.push(PolicyStep::Deal(ctx.board.river?));
        }
        let stack = original[0];
        if stack == 0
            || stack != original[1]
            || stack > turn_pot / 2
            || turn_pot < ctx.big_blind
            || i32::try_from(stack).is_err()
            || i32::try_from(turn_pot).is_err()
        {
            return None;
        }
        if path.iter().any(
            |p| matches!(p,PolicyStep::Act{action:PolicyAction::AllIn(n),..} if *n!=stack as i32),
        ) {
            return None;
        }
        let preflop = flop_reach(ctx)?;
        if preflop.dealer != ip.seat {
            return None;
        }
        let [a, b, c] = ctx.board.flop?;
        let request = Self {
            board: [a, b, c, ctx.board.turn?],
            seats,
            pot: turn_pot,
            stack,
            preflop,
            lines: seats.map(|s| LineEvidence::before_turn(ctx, s)),
        };
        let query = Query {
            path,
            hero: ctx.hole,
            actor: if ctx.seat == oop.seat {
                Player::Oop
            } else {
                Player::Ip
            },
            street: ctx.street,
            pot: ctx.pot.0,
            behind: [oop.stack, ip.stack],
            committed: [oop.committed_this_street, ip.committed_this_street],
            board: ctx.board.all_cards(),
            to_call: ctx.to_call.0,
            can_raise: ctx.can_raise,
        };
        Some((request, query))
    }

    pub(crate) fn solve(self, query: Query, deadline: Instant) -> Option<Decision> {
        if Instant::now() >= deadline {
            return None;
        }
        let (ranges, tail_loss) = ranges::for_solve(&self)?;
        if Instant::now() >= deadline {
            return None;
        }
        let [a, b, c, t] = self.board;
        let [oop_range, ip_range] = ranges;
        let req = SolveRequest {
            street: SolveStreet::Turn,
            flop: [a, b, c],
            turn: Some(t),
            river: None,
            oop_range,
            ip_range,
            starting_pot: self.pot as i32,
            effective_stack: self.stack as i32,
            bet_sizes: "a".into(),
            raise_sizes: String::new(),
            hero: None,
            solving_player: Player::Oop,
            limits: SolveLimits {
                max_memory_bytes: 12_000_000,
                max_iterations: 80,
                target_exploitability_pct_of_pot: 0.0075,
            },
        };
        let ctl = SolveControl {
            deadline: Some(deadline),
            cancel: None,
        };
        let mut policy = POOL
            .as_ref()?
            .install(|| SolvedPostflop::solve(req, &ctl))
            .ok()?;
        let quality = policy.root_quality();
        // The solver measures every 10 iterations and once more at a time
        // cutoff. Only accept the first regularly scheduled converged check.
        // With the fixed one-thread pool, later queries reproduce that same
        // strategy even when they start with a different remaining budget.
        if !quality.iterations.is_multiple_of(10) || Instant::now() >= deadline {
            return None;
        }
        let decision = POOL
            .as_ref()?
            .install(|| policy.decision(&query.path, query.hero))
            .ok()?;
        if decision.street != query.street
            || decision.player != query.actor
            || decision.pot != i32::try_from(query.pot).ok()?
            || decision.remaining_stacks != query.behind.map(|v| v as i32)
            || decision.committed_this_street != query.committed.map(|v| v as i32)
            || decision.board.get(3..) != query.board.get(3..)
            || decision.board.len() != query.board.len()
            || !decision.board[..3]
                .iter()
                .all(|c| query.board[..3].contains(c))
        {
            return None;
        }
        let mut actions = Vec::new();
        for f in decision.actions {
            let action = match f.action {
                PolicyAction::Check if query.to_call == 0 => PlayerAction::Check,
                PolicyAction::Fold if query.to_call > 0 => PlayerAction::Fold,
                PolicyAction::Call if query.to_call > 0 => PlayerAction::AllIn,
                PolicyAction::AllIn(n) if query.can_raise && n == self.stack as i32 => {
                    PlayerAction::AllIn
                }
                _ => return None,
            };
            if f.probability > 0.0 {
                actions.push((action, f.probability));
            }
        }
        if actions.is_empty() || Instant::now() >= deadline {
            return None;
        }
        Some(Decision {
            actions,
            iterations: quality.iterations,
            max_gap: 2.0 * f64::from(quality.exploitability_fraction_of_pot) + tail_loss,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::bot::preflop_blueprint::snapshot_with;
    use engine::{Chips, GameHand, PlayerId, PokerRng};
    use rand::SeedableRng;
    use std::time::Duration;

    fn act(h: &mut GameHand, p: PlayerAction) {
        h.apply_action(h.snapshot().current_actor.unwrap(), p)
            .unwrap();
    }
    fn raised_hand(seed: u8) -> GameHand {
        let mut h = GameHand::new_with_rng(
            vec![
                (PlayerId(34), Chips(1000), 2),
                (PlayerId(71), Chips(1000), 7),
            ],
            0,
            Chips(10),
            Chips(5),
            PokerRng::from_seed_bytes([seed; 32]),
        );
        h.start().unwrap();
        for amount in [25, 75, 150, 300] {
            act(
                &mut h,
                PlayerAction::Raise {
                    amount: Chips(amount),
                },
            );
        }
        act(&mut h, PlayerAction::Call);
        act(&mut h, PlayerAction::Raise { amount: Chips(595) });
        act(&mut h, PlayerAction::Call);
        h
    }

    #[test]
    fn second_turn_decision_requires_own_model_check() {
        let mut hand = raised_hand(18);
        act(&mut hand, PlayerAction::Check);
        act(&mut hand, PlayerAction::AllIn);
        snapshot_with(
            &hand,
            |_| {},
            |ctx| {
                let mut work = crate::bot::postflop_work::Hand::default();
                work.record(ctx.seat, Street::Turn, "range_policy");
                assert!(Request::from_context(ctx).is_some());
                assert!(work.prepare(ctx, None).is_none());
                work.record(ctx.seat, Street::Turn, "hu_turn_jam_model");
                assert!(work.admits_turn(ctx));
            },
        );
    }

    #[test]
    fn turn_model_chance_policy_matches_real_turn_and_river_after_checks() {
        verify_chance_path(Duration::from_secs(8));
    }

    #[test]
    #[ignore = "explicit live worker deadline measurement"]
    fn turn_model_worker_budget() {
        verify_chance_path(Duration::from_millis(400));
    }

    fn verify_chance_path(budget: Duration) {
        let mut h = raised_hand(18);
        let choose = |h: &GameHand, river: bool| {
            snapshot_with(
                h,
                |_| {},
                |ctx| {
                    let (r, q) = if river {
                        Request::after_turn(ctx)
                    } else {
                        Request::from_context(ctx)
                    }
                    .unwrap();
                    let root = r.pot;
                    let begin = Instant::now();
                    let range = ranges::for_solve(&r).expect("concentrated public ranges");
                    eprintln!(
                        "turn sparse tail {:0.6} preparation {:?} combos {:?}",
                        range.1,
                        begin.elapsed(),
                        range.0.each_ref().map(|s| s.split(',').count())
                    );
                    let start = Instant::now();
                    let out = r.solve(q, start + budget).unwrap();
                    eprintln!(
                        "turn solved {:?} {:?} ({} pot), river={river}",
                        out.quality(),
                        start.elapsed(),
                        root
                    );
                    assert!(out.quality().1 <= 0.02);
                    let mut rng = StdRng::seed_from_u64(17);
                    let action = out.sample(&mut rng);
                    assert!(matches!(action, PlayerAction::Check | PlayerAction::AllIn));
                },
            )
        };
        choose(&h, false);
        act(&mut h, PlayerAction::Check);
        act(&mut h, PlayerAction::Check);
        choose(&h, true);
    }
}
