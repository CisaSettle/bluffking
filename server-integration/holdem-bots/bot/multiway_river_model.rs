//! A three-handed river subgame after two checks to the button. All three
//! live players were dealt in and can cover the half-pot opening wager.
//! Candidate strategies use twenty public-range-weighted strength buckets.
//! The independent audit integrates every legal holding, card removal, ties
//! and odd chips. It admits a candidate only when each seat gains at most
//! 0.25% of the pot by using its best *per-pocket* policy in this action tree.
use super::preflop_blueprint::{flop_reach, FlopReach};
use super::river_model::{card, index, known_mask};
use super::strategy::LineEvidence;
use super::{BotPersona, DecisionContext, PublicAction};
use engine::{BoardCards, Chips, HoleCards, PlayerAction, Street};
use rand::{rngs::StdRng, Rng};
use std::{sync::Arc, time::Instant};

mod audit;
mod chance;
mod exploit;
mod game;
use chance::{Hand, K};
pub(crate) use exploit::overfolds;

#[derive(PartialEq, Eq)]
struct Root {
    board: BoardCards,
    seats: [u8; 3], // clockwise starting immediately left of button
    stacks: [u32; 3],
    history: Vec<PublicAction>,
    pot: u32,
    bet: u32,
    big_blind: u32,
}
#[derive(Clone)]
pub(crate) struct Request {
    root: Arc<Root>,
    flop: Arc<FlopReach>,
    lines: Arc<[LineEvidence; 3]>,
}
pub(crate) struct Query {
    root: Arc<Root>,
    node: usize,
    hole: HoleCards,
}
pub(crate) struct Solved {
    root: Arc<Root>,
    rows: Vec<Hand>,
    policy: game::Policy,
    base: [Distribution; 3],
    button_bets: Distribution,
    certificate: audit::Certificate,
    overfolder_shift: Option<Vec<f64>>,
}

impl Request {
    pub(crate) fn from_context(ctx: &DecisionContext<'_>) -> Option<(Self, Query)> {
        if ctx.persona != BotPersona::Shark
            || ctx.street != Street::River
            || !ctx.plain_cash
            || ctx.has_straddle
            || ctx.table.seats.len() != 3
            || !ctx.table.is_known()
        {
            return None;
        }
        let players = ctx.table.seats;
        let d = players
            .iter()
            .position(|s| s.seat == ctx.table.dealer_seat)?;
        let ordered = [&players[(d + 1) % 3], &players[(d + 2) % 3], &players[d]];
        let seats = ordered.map(|s| s.seat);
        let stacks = ordered.map(|s| s.stack.checked_add(s.committed_this_street));
        let stacks = [stacks[0]?, stacks[1]?, stacks[2]?];
        let pot = ordered
            .iter()
            .try_fold(ctx.pot.0, |p, s| p.checked_sub(s.committed_this_street))?;
        let bet = pot.div_ceil(2);
        if bet < ctx.big_blind || stacks.iter().any(|&s| s <= bet) {
            return None;
        }
        let history = ctx.public_history?;
        let (before, river) =
            history.split_at(history.partition_point(|a| a.street != Street::River));
        if before.iter().any(|a| a.action == PlayerAction::Fold) {
            return None;
        }
        let [c0, c1, rest @ ..] = river else {
            return None;
        };
        if c0.seat != seats[0]
            || c1.seat != seats[1]
            || c0.action != PlayerAction::Check
            || c1.action != PlayerAction::Check
        {
            return None;
        }
        let opening = |a: &PublicAction| {
            a.seat == seats[2] && a.increased_bet && a.amount == bet && a.to_call_before == 0
        };
        let node = match rest {
            [] => 0,
            [b] if opening(b) => 1,
            [b, r] if opening(b) && r.seat == seats[0] && r.action == PlayerAction::Fold => 2,
            [b, r] if opening(b) && r.seat == seats[0] && r.is_call_like() && r.amount == bet => 3,
            _ => return None,
        };
        if ctx.seat != seats[game::ACTOR[node]]
            || (node == 0 && (ctx.to_call.0 != 0 || !ctx.can_raise || ctx.min_raise_to?.0 > bet))
            || (node > 0 && ctx.to_call.0 != bet)
        {
            return None;
        }
        let flop = Arc::new(flop_reach(ctx)?);
        let root = Arc::new(Root {
            board: ctx.board.clone(),
            seats,
            stacks,
            history: before.to_vec(),
            pot,
            bet,
            big_blind: ctx.big_blind,
        });
        let lines = Arc::new([
            LineEvidence::after_river_check(ctx, seats[0]),
            LineEvidence::after_river_check(ctx, seats[1]),
            LineEvidence::before_river(ctx, seats[2]),
        ]);
        Some((
            Self {
                root: root.clone(),
                flop,
                lines,
            },
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
        let board = engine::eval::board_eval(&root.board);
        let known = known_mask(&root.board);
        let mut entries = Vec::with_capacity(1081);
        let mut total = [0.; 3];
        for a in 0..52 {
            if a % 8 == 0 && Instant::now() >= deadline {
                return None;
            }
            if known & (1 << a) != 0 {
                continue;
            }
            for b in a + 1..52 {
                if known & (1 << b) != 0 {
                    continue;
                }
                let hole = HoleCards::new(card(a), card(b));
                let mut w = [0.; 3];
                for p in 0..3 {
                    w[p] = self.flop.weight(root.seats[p], hole)? * self.lines[p].weight(hole);
                    if !w[p].is_finite() || w[p] < 0. {
                        return None;
                    }
                    total[p] += w[p];
                }
                entries.push((engine::eval::rank_with_board(&board, &hole), [a, b], w));
            }
        }
        if total.iter().any(|&w| w <= 0.) {
            return None;
        }
        entries.sort_unstable_by_key(|&(r, c, _)| (r, c));
        let mut rows = Vec::with_capacity(entries.len());
        let mut start = 0;
        let mut rank = 0;
        let mut cumulative = 0.;
        while start < entries.len() {
            let end = start + entries[start..].partition_point(|v| v.0 == entries[start].0);
            // Put tied ranks in the same bin. The mean of the three public
            // marginal ranges reserves precision for hands likely on this line.
            let group_mass: f64 = entries[start..end]
                .iter()
                .map(|(_, _, w)| (w[0] / total[0] + w[1] / total[1] + w[2] / total[2]) / 3.)
                .sum();
            let bucket = (((cumulative + 0.5 * group_mass) * K as f64) as usize).min(K - 1);
            cumulative += group_mass;
            for &(_, cards, weights) in &entries[start..end] {
                rows.push(Hand {
                    cards,
                    rank,
                    bucket,
                    weight: std::array::from_fn(|p| weights[p] / total[p]),
                });
            }
            start = end;
            rank += 1;
        }
        let joint = chance::joint(&rows, 8, deadline)?;
        let policy = game::solve(&joint, root.pot, root.bet, deadline)?;
        let certificate = audit::evaluate(&rows, &policy.p, root.pot, root.bet, deadline)?;
        // The exact per-pocket best response includes every bucket response.
        if certificate.gaps.iter().any(|&g| g > 0.0025) {
            return None;
        }
        let base = std::array::from_fn(|p| Distribution::new(&rows, |h| h.weight[p]));
        let button_bets = Distribution::new(&rows, |h| h.weight[2] * policy.p[0][h.bucket]);
        (Instant::now() < deadline).then_some(Solved {
            root,
            rows,
            policy,
            base,
            button_bets,
            certificate,
            overfolder_shift: None,
        })
    }
}

// For a live query, integrate over the other two seats using the actual
// hero blockers. Zero or very unlikely public responses do not use a root
// strategy with a good average gap but insufficient support on this branch.
struct Distribution {
    total: f64,
    one: [f64; 52],
    two: Vec<[f64; 52]>,
}
impl Distribution {
    fn new(rows: &[Hand], weight: impl Fn(&Hand) -> f64) -> Self {
        let mut d = Self {
            total: 0.,
            one: [0.; 52],
            two: vec![[0.; 52]; 52],
        };
        for h in rows {
            let w = weight(h);
            let [a, b] = h.cards.map(usize::from);
            d.total += w;
            d.one[a] += w;
            d.one[b] += w;
            d.two[a][b] = w;
            d.two[b][a] = w;
        }
        d
    }
    fn disjoint(&self, c: [usize; 4]) -> f64 {
        let [a, b, d, e] = c;
        (self.total - self.one[a] - self.one[b] - self.one[d] - self.one[e]
            + self.two[a][b]
            + self.two[a][d]
            + self.two[a][e]
            + self.two[b][d]
            + self.two[b][e]
            + self.two[d][e])
            .max(0.)
    }
}
impl Solved {
    pub(crate) fn quality(&self) -> (usize, [f64; 3]) {
        (self.policy.iterations, self.certificate.gaps)
    }
    pub(crate) fn decision(
        &self,
        q: Query,
        rng: &mut StdRng,
        model: Option<&super::OpponentModel<'_>>,
    ) -> Option<(PlayerAction, &'static str)> {
        if q.root != self.root {
            return None;
        }
        let mut cards = q.hole.as_array().map(index);
        cards.sort_unstable();
        let player = game::ACTOR[q.node];
        let index = self
            .rows
            .iter()
            .position(|h| h.cards == cards && h.weight[player] > 0.)?;
        let hand = &self.rows[index];
        let [other, last] = match player {
            2 => [0, 1],
            0 => [1, 2],
            _ => [0, 2],
        };
        let after = if q.node == 0 {
            &self.base[last]
        } else {
            &self.button_bets
        };
        let [a, b] = cards.map(usize::from);
        let mut all = 0.;
        let mut reached = 0.;
        for h in &self.rows {
            if h.weight[other] == 0. || h.cards.iter().any(|c| cards.contains(c)) {
                continue;
            }
            let [c, d] = h.cards.map(usize::from);
            let dead = [a, b, c, d];
            all += h.weight[other] * self.base[last].disjoint(dead);
            let first = match q.node {
                2 => 1. - self.policy.p[1][h.bucket],
                3 => self.policy.p[1][h.bucket],
                _ => 1.,
            };
            reached += h.weight[other] * first * after.disjoint(dead);
        }
        if all <= 0. || reached < 0.001 * all {
            return None;
        }
        let base = self.policy.p[q.node][hand.bucket];
        let shift = self
            .overfolder_shift
            .as_ref()
            .filter(|_| q.node == 0 && model.is_some_and(|m| overfolds(m, self.root.seats[0])))
            .map_or(0., |s| s[index]);
        let draw: f64 = rng.gen();
        let choose = draw < base + shift;
        let action = match (q.node, choose) {
            (0, true) => PlayerAction::Raise {
                amount: Chips(self.root.bet),
            },
            (0, false) => PlayerAction::Check,
            (_, true) => PlayerAction::Call,
            (_, false) => PlayerAction::Fold,
        };
        let source = if choose != (draw < base) {
            "three_way_river_overfolder"
        } else {
            "three_way_river_bucket_model"
        };
        Some((action, source))
    }
}

#[cfg(test)]
mod tests;
