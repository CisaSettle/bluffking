//! Heads-up river equilibrium conditional on the public preflop blueprint
//! reach, the reference flop/turn action likelihoods, and a single opening
//! size. The worker solves both seats' exact pockets without a query hand.

use super::preflop_blueprint::{flop_reach, FlopReach};
use super::strategy::LineEvidence;
use super::{BotPersona, DecisionContext, OpponentModel, PublicAction, ShoveRead};
use engine::{BoardCards, Card, Chips, HoleCards, PlayerAction, Rank, Street, Suit};
use rand::{rngs::StdRng, Rng};
use std::{sync::Arc, time::Instant};

mod folded;
pub(crate) mod solve;
use solve::{Combo, IP_CALL, IP_OPEN, OOP_CALL, OOP_OPEN};

#[cfg_attr(test, derive(Clone))]
#[derive(PartialEq, Eq)]
struct Root {
    board: BoardCards,
    seats: [u8; 2],          // First and last to act clockwise from the actual button.
    folders: Vec<(u8, u32)>, // seats and chips behind at the start of the flop
    stacks: [u32; 2],
    history: Vec<PublicAction>, // before the river
    pot: u32,
    big_blind: u32,
    bet: u32,
}

#[derive(Clone)]
pub(crate) struct Request {
    root: Arc<Root>,
    flop: Arc<FlopReach>,
    lines: Arc<[LineEvidence; 2]>,
}

pub(crate) struct Query {
    root: Arc<Root>,
    node: usize,
    hole: HoleCards,
}

pub(crate) struct Solved {
    root: Arc<Root>,
    rows: Vec<Combo>,
    policy: solve::Policy,
    folder: Option<Box<folded::FoldRange>>,
}

/// Dense card index of the river solvers: suit-major, `suit * 13 + rank`.
pub(super) fn index(card: Card) -> u8 {
    card.suit as u8 * 13 + card.rank as u8
}
/// The card at dense index `i` (inverse of [`index`]).
pub(super) fn card(i: u8) -> Card {
    Card {
        suit: Suit::ALL[usize::from(i / 13)],
        rank: Rank::ALL[usize::from(i % 13)],
    }
}
/// Bit mask of the board's cards by dense [`index`].
pub(super) fn known_mask(board: &BoardCards) -> u64 {
    board
        .all_cards()
        .iter()
        .fold(0u64, |m, &c| m | (1u64 << index(c)))
}

impl Request {
    pub(crate) fn from_context(ctx: &DecisionContext<'_>) -> Option<(Self, Query)> {
        if ctx.persona != BotPersona::Shark
            || ctx.street != Street::River
            || !ctx.plain_cash
            || ctx.has_straddle
            || !(2..=4).contains(&ctx.table.seats.len())
            || !ctx.table.is_known()
            || ctx.board.count() != 5
        {
            return None;
        }
        let all = ctx.table.seats;
        let dealer = all.iter().position(|s| s.seat == ctx.table.dealer_seat)?;
        let mut order = (1..=all.len())
            .map(|off| &all[(dealer + off) % all.len()])
            .filter(|s| !s.folded);
        let (oop, ip) = (order.next()?, order.next()?);
        if order.next().is_some() || ctx.table.seat_entry(ctx.seat)?.stack != ctx.stack_chips {
            return None;
        }
        let seats = [oop.seat, ip.seat];
        let stacks = [
            oop.stack.checked_add(oop.committed_this_street)?,
            ip.stack.checked_add(ip.committed_this_street)?,
        ];
        let pot = ctx
            .pot
            .0
            .checked_sub(oop.committed_this_street)?
            .checked_sub(ip.committed_this_street)?;
        let bet = pot
            .div_ceil(2)
            .max(ctx.big_blind)
            .min(stacks[0])
            .min(stacks[1]);
        if pot == 0 || bet == 0 || (bet < ctx.big_blind && stacks[0] != stacks[1]) {
            return None;
        }
        let history = ctx.public_history?;
        let (before, river) =
            history.split_at(history.partition_point(|a| a.street != Street::River));
        let check = |a: &PublicAction, seat: u8| a.seat == seat && a.action == PlayerAction::Check;
        let open = |a: &PublicAction, seat: u8| {
            a.seat == seat && a.increased_bet && a.amount == bet && a.to_call_before == 0
        };
        let node = match river {
            [] if ctx.seat == seats[0] => OOP_OPEN,
            [a] if ctx.seat == seats[1] && check(a, seats[0]) => IP_OPEN,
            [a] if ctx.seat == seats[1] && open(a, seats[0]) => IP_CALL,
            [a, b] if ctx.seat == seats[0] && check(a, seats[0]) && open(b, seats[1]) => OOP_CALL,
            _ => return None,
        };
        if node < 2
            && (!ctx.can_raise
                || ctx.to_call.0 != 0
                || !(bet == ctx.stack_chips || ctx.min_raise_to.is_some_and(|min| bet >= min.0)))
        {
            return None;
        }
        if node >= 2 && ctx.to_call.0 != bet {
            return None;
        }
        // The solver enumerates a standard deck. Reject a malformed board
        // before evaluating any candidate holding. The private query remains
        // separate so its cards can never vary the opponent model or policy.
        let board = ctx.board.all_cards();
        if board
            .iter()
            .enumerate()
            .any(|(i, c)| board[..i].contains(c))
        {
            return None;
        }
        let flop = Arc::new(flop_reach(ctx)?);
        // A later fold needs a postflop-conditioned prior for that seat.
        let folders: Vec<_> = flop
            .players
            .iter()
            .filter(|p| p.folded)
            .map(|p| (p.seat, p.stack))
            .collect();
        if folders.len() != all.len() - 2 {
            return None;
        }
        let root = Arc::new(Root {
            board: ctx.board.clone(),
            seats,
            folders,
            stacks,
            history: before.to_vec(),
            pot,
            big_blind: ctx.big_blind,
            bet,
        });
        let req = Self {
            root: root.clone(),
            flop,
            lines: Arc::new(seats.map(|s| LineEvidence::before_river(ctx, s))),
        };
        Some((
            req,
            Query {
                root,
                node,
                hole: ctx.hole,
            },
        ))
    }

    pub(crate) fn same_root(&self, other: &Self) -> bool {
        self.root == other.root
    }

    pub(crate) fn solve(self, deadline: Instant) -> Option<Solved> {
        if Instant::now() >= deadline {
            return None;
        }
        let root = self.root;
        let be = engine::eval::board_eval(&root.board);
        let known = known_mask(&root.board);
        let mut sorted = Vec::with_capacity(1081);
        let mut total = [0.; 2];
        for a in 0..52 {
            if known & (1u64 << a) != 0 {
                continue;
            }
            for b in a + 1..52 {
                if known & (1u64 << b) != 0 {
                    continue;
                }
                let hole = HoleCards::new(card(a), card(b));
                let mut weight = [0.; 2];
                for (player, w) in weight.iter_mut().enumerate() {
                    *w = self.flop.weight(root.seats[player], hole)?
                        * self.lines[player].weight(hole);
                    if !w.is_finite() || *w < 0. {
                        return None;
                    }
                    total[player] += *w;
                }
                sorted.push((engine::eval::rank_with_board(&be, &hole), [a, b], weight));
            }
        }
        if total.iter().any(|&w| w <= 0.) {
            return None;
        }
        sorted.sort_unstable_by_key(|&(rank, cards, _)| (rank, cards));
        let mut rank = 0u32;
        let mut previous = None;
        let rows: Vec<_> = sorted
            .into_iter()
            .map(|(value, cards, weights)| {
                if previous != Some(value) {
                    rank += 1;
                    previous = Some(value);
                }
                Combo {
                    cards,
                    rank,
                    weight: [weights[0] / total[0], weights[1] / total[1]],
                }
            })
            .collect();
        let mut policy = solve::solve(&rows, root.pot, root.bet, 256, 0.0025, deadline)?;
        let folder = if !root.folders.is_empty() {
            // Factorized CFR supplies a candidate. Admit it only after each
            // player's exact best response under the joint live and folded prior.
            let dead = Box::new(folded::FoldRange::new(&rows, &self.flop, &root.folders)?);
            let gaps = folded::gaps(&rows, &policy, &dead, root.pot, root.bet, deadline)?;
            if gaps.iter().any(|&g| g + 1e-9 > 0.0025) {
                return None;
            }
            policy.gaps = gaps;
            Some(dead)
        } else {
            None
        };
        Some(Solved {
            root,
            rows,
            policy,
            folder,
        })
    }
}

impl Solved {
    pub(crate) fn quality(&self) -> (usize, [f64; 2]) {
        (self.policy.iterations, self.policy.gaps)
    }

    /// Entire exact-combo policy at this node. The positive branch means
    /// bet at an opening node and call at a response node.
    fn likelihoods(&self, node: usize) -> impl Iterator<Item = (&Combo, f64)> {
        self.rows
            .iter()
            .zip(&self.policy.actions)
            .map(move |(row, p)| (row, p[node]))
    }

    pub(crate) fn decision(
        &self,
        query: Query,
        rng: &mut StdRng,
        model: Option<&OpponentModel<'_>>,
    ) -> Option<(PlayerAction, &'static str)> {
        if self.root != query.root {
            return None;
        }
        let mut cards = query.hole.as_array().map(index);
        cards.sort_unstable();
        let node = query.node;
        let player = node % 2;
        let i = self
            .rows
            .iter()
            .position(|row| row.cards == cards && row.weight[player] > 0.)?;
        let own = self.policy.actions[i];
        let earlier = if node == OOP_CALL {
            1. - own[OOP_OPEN]
        } else {
            1.
        };
        let opponent_node = if node == OOP_CALL { IP_OPEN } else { OOP_OPEN };
        let read = model
            .filter(|_| {
                node < 2
                    && (0.10..=0.90).contains(&own[node])
                    && self.root.bet == self.root.pot.div_ceil(2)
                    && self.root.stacks.iter().all(|&stack| stack > self.root.bet)
            })
            .and_then(|m| {
                let stats = m.river_response_stats(self.root.seats[1 - player])?;
                let folds = stats.river_half_pot_folds?[0];
                let n = u32::from(folds.opportunities);
                if !(20..=32).contains(&n) {
                    return None;
                }
                let sample = m.min_cbet_sample.max(20);
                let continues = ShoveRead {
                    recent: !folds.recent & (u32::MAX >> (32 - n)),
                    ..folds
                };
                if folds.lower_frequency(sample)? > 0.62 {
                    Some(true)
                } else if continues.lower_frequency(sample)? > 0.70 {
                    Some(false)
                } else {
                    None
                }
            });
        let mut all = 0.;
        let mut reached = 0.;
        let mut checked = [0.; 2];
        let mut called = [0.; 2];
        let dead = self.folder.as_ref().map(|f| f.excluding(cards));
        for (j, (row, p)) in self.likelihoods(opponent_node).enumerate() {
            if row.cards.iter().any(|c| cards.contains(c)) {
                continue;
            }
            let w = row.weight[1 - player] * dead.as_ref().map_or(1., |f| f.mass(row.cards));
            let branch = match node {
                OOP_OPEN => 1.,
                IP_OPEN => 1. - p,
                _ => p,
            };
            all += w;
            reached += w * branch;
            if read.is_some() {
                let opponent = self.policy.actions[j];
                let check = if node == OOP_OPEN {
                    1. - opponent[IP_OPEN]
                } else {
                    branch
                };
                let call = branch * opponent[if node == OOP_OPEN { IP_CALL } else { OOP_CALL }];
                let equity = match self.rows[i].rank.cmp(&row.rank) {
                    std::cmp::Ordering::Less => 0.,
                    std::cmp::Ordering::Equal => 0.5,
                    std::cmp::Ordering::Greater => 1.,
                };
                checked[0] += w * check * equity;
                checked[1] += w * check;
                called[0] += w * call * equity;
                called[1] += w * call;
            }
        }
        // A frozen policy's root gap alone cannot validate a vanishingly
        // rare conditional history. Decline off-range cards and branches.
        if all <= 0. || earlier * reached < 0.001 * all {
            return None;
        }
        let p = own[node].clamp(0., 1.);
        let baseline = if self.folder.is_some() {
            "hu_river_folded_range_model"
        } else {
            "hu_river_line_model"
        };
        let (offset, label) = if checked[1] > 0. && checked[0] <= 0.20 * checked[1] {
            match read {
                Some(true) => (0.1, "hu_river_overfolder_bluff"),
                Some(false) => (-0.1, "hu_river_station_restrain"),
                None => (0., baseline),
            }
        } else if read == Some(false)
            && checked[1] > 0.
            && checked[0] >= 0.75 * checked[1]
            && called[1] > 0.
            && called[0] >= 0.65 * called[1]
        {
            (0.1, "hu_river_station_value")
        } else {
            (0., baseline)
        };
        // One draw identifies actual changes to the baseline, including a
        // station turning a sampled weak bluff into a check.
        let draw: f64 = rng.gen();
        let choose = draw < (p + offset).clamp(0., 1.);
        let source = if choose != (draw < p) {
            label
        } else {
            baseline
        };
        let action = match (node < 2, choose) {
            (true, true) if self.root.bet == self.root.stacks[player] => PlayerAction::AllIn,
            (true, true) => PlayerAction::Raise {
                amount: Chips(self.root.bet),
            },
            (true, false) => PlayerAction::Check,
            (false, true) => PlayerAction::Call,
            (false, false) => PlayerAction::Fold,
        };
        Some((action, source))
    }
}

#[cfg(test)]
mod tests;
