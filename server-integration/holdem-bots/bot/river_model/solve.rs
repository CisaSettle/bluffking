//! Two-player, one-size river game. At either opening opportunity the actor
//! checks or bets. Facing a bet the other player folds or calls. Every
//! compatible pair of exact pockets has its product-of-reaches chance weight.

use std::time::Instant;

#[derive(Clone, Copy)]
pub(crate) struct Combo {
    pub cards: [u8; 2],
    /// Equal adjacent ranks tie. Rows are sorted weak to strong.
    pub rank: u32,
    pub weight: [f64; 2],
}

pub(super) const OOP_OPEN: usize = 0;
pub(super) const IP_OPEN: usize = 1;
pub(crate) const OOP_CALL: usize = 2;
pub(crate) const IP_CALL: usize = 3;
type Strategy = [f64; 4];

#[derive(Clone, Copy, Default)]
struct Payoff {
    mass: f64,
    check: f64,
    called: f64,
}

struct Mass {
    total: [f64; 3],
    by_card: [[f64; 3]; 52],
}
impl Mass {
    fn zero() -> Self {
        Self {
            total: [0.; 3],
            by_card: [[0.; 3]; 52],
        }
    }
    fn add(&mut self, cards: [u8; 2], weights: [f64; 3]) {
        let [a, b] = cards.map(usize::from);
        for (k, w) in weights.into_iter().enumerate() {
            self.total[k] += w;
            self.by_card[a][k] += w;
            self.by_card[b][k] += w;
        }
    }
    /// For complete sets containing the queried combo, add its own weight
    /// back once after subtracting both card totals.
    fn compatible(&self, cards: [u8; 2], self_weight: [f64; 3]) -> [f64; 3] {
        let [a, b] = cards.map(usize::from);
        std::array::from_fn(|k| {
            self.total[k] - self.by_card[a][k] - self.by_card[b][k] + self_weight[k]
        })
    }
}

fn opponent_weights(player: usize, row: &Combo, s: Strategy) -> [f64; 3] {
    let w = row.weight[1 - player];
    if player == 0 {
        [w, w * s[IP_CALL], w * s[IP_OPEN]]
    } else {
        [w, w * s[OOP_OPEN], w * (1. - s[OOP_OPEN]) * s[OOP_CALL]]
    }
}

/// Passes the counterfactual action values and the whole-policy / optimal
/// own response value for each exact pocket against the frozen opponent.
fn values(
    rows: &[Combo],
    strategy: &[Strategy],
    player: usize,
    pot: u32,
    bet: u32,
    mut visit: impl FnMut(usize, [f64; 2], [f64; 2], f64, f64, f64),
) {
    let mut all = Mass::zero();
    for (i, row) in rows.iter().enumerate() {
        all.add(row.cards, opponent_weights(player, row, strategy[i]));
    }
    let b = f64::from(bet) / f64::from(pot);
    // At a two-player table OOP is left of the button and wins the odd chip.
    let odd = (if player == 0 { 0.5 } else { -0.5 }) * f64::from(pot % 2) / f64::from(pot);
    let mut weaker = Mass::zero();
    let mut start = 0;
    while start < rows.len() {
        let mut end = start;
        let mut equal = Mass::zero();
        while end < rows.len() && rows[start].rank == rows[end].rank {
            equal.add(
                rows[end].cards,
                opponent_weights(player, &rows[end], strategy[end]),
            );
            end += 1;
        }
        for i in start..end {
            let row = &rows[i];
            let s = strategy[i];
            let own = opponent_weights(player, row, s);
            let mass = all.compatible(row.cards, own);
            let win = weaker.compatible(row.cards, [0.; 3]);
            let tie = equal.compatible(row.cards, own);
            let p: [Payoff; 3] = std::array::from_fn(|k| {
                let share = win[k] + tie[k] * 0.5;
                Payoff {
                    mass: mass[k],
                    check: share + odd * tie[k],
                    called: (1. + 2. * b) * share - b * mass[k] + odd * tie[k],
                }
            });
            let (open, call, reached, best) = if player == 0 {
                let bet_ev = p[0].mass - p[1].mass + p[1].called;
                let check_back = p[0].check - p[2].check;
                let check_ev = check_back + s[OOP_CALL] * p[2].called;
                let ev = s[OOP_OPEN] * bet_ev + (1. - s[OOP_OPEN]) * check_ev;
                let best = bet_ev.max(check_back + p[2].called.max(0.));
                ([check_ev, bet_ev], [0., p[2].called], ev, best)
            } else {
                let bet_ev = p[0].mass - p[1].mass - p[2].mass + p[2].called;
                let check_ev = p[0].check - p[1].check;
                let ev =
                    s[IP_CALL] * p[1].called + s[IP_OPEN] * bet_ev + (1. - s[IP_OPEN]) * check_ev;
                let best = p[1].called.max(0.) + bet_ev.max(check_ev);
                ([check_ev, bet_ev], [0., p[1].called], ev, best)
            };
            visit(i, open, call, reached, best, mass[0]);
        }
        for i in start..end {
            weaker.add(
                rows[i].cards,
                opponent_weights(player, &rows[i], strategy[i]),
            );
        }
        start = end;
    }
}

pub(crate) struct Policy {
    pub actions: Vec<Strategy>,
    /// Unilateral root EV gains in units of the starting pot.
    pub gaps: [f64; 2],
    pub iterations: usize,
}
fn evaluate(rows: &[Combo], strategy: &[Strategy], pot: u32, bet: u32) -> Option<[f64; 2]> {
    let mut gaps = [0.; 2];
    for (player, gap) in gaps.iter_mut().enumerate() {
        let mut joint = 0.;
        let mut improvement = 0.;
        values(
            rows,
            strategy,
            player,
            pot,
            bet,
            |i, _, _, policy, best, mass| {
                joint += rows[i].weight[player] * mass;
                improvement += rows[i].weight[player] * (best - policy);
            },
        );
        if !(joint > 0. && joint.is_finite() && improvement.is_finite()) {
            return None;
        }
        *gap = (improvement / joint).max(0.);
    }
    Some(gaps)
}

/// Input rows are unique board-compatible pockets, with nonnegative finite
/// weights. Only a strategy whose frozen root gap passes the quality gate
/// is returned. The shared CPU worker also imposes an absolute deadline.
pub(crate) fn solve(
    rows: &[Combo],
    pot: u32,
    bet: u32,
    limit: usize,
    gap: f64,
    deadline: Instant,
) -> Option<Policy> {
    if rows.is_empty() || pot == 0 || bet == 0 {
        return None;
    }
    let mut current = vec![[0.5; 4]; rows.len()];
    let mut regrets = vec![[[0.; 2]; 4]; rows.len()];
    let mut sums = vec![[0.; 4]; rows.len()];
    let mut own_checks = vec![0.; rows.len()];
    let mut time_weight = 0.;
    for iteration in 1..=limit {
        if Instant::now() >= deadline {
            return None;
        }
        let t = iteration as f64;
        time_weight += t;
        for i in 0..rows.len() {
            let own_reach = 1. - current[i][OOP_OPEN];
            own_checks[i] += t * own_reach;
            for node in 0..4 {
                let reach = if node == OOP_CALL { own_reach } else { 1. };
                sums[i][node] += t * reach * current[i][node];
            }
        }
        for player in 0..2 {
            values(
                rows,
                &current,
                player,
                pot,
                bet,
                |i, open, call, _, _, _| {
                    for (node, action_ev) in [(player, open), (player + 2, call)] {
                        let p = current[i][node];
                        let ev = (1. - p) * action_ev[0] + p * action_ev[1];
                        for (r, u) in regrets[i][node].iter_mut().zip(action_ev) {
                            *r = (*r + u - ev).max(0.);
                        }
                    }
                },
            );
            for (i, s) in current.iter_mut().enumerate() {
                for node in [player, player + 2] {
                    let [a, b] = regrets[i][node];
                    s[node] = if a + b > 0. { b / (a + b) } else { 0.5 };
                }
            }
        }
        if iteration.is_power_of_two() || iteration == limit {
            let avg: Vec<Strategy> = sums
                .iter()
                .enumerate()
                .map(|(i, s)| {
                    std::array::from_fn(|k| {
                        if k == OOP_CALL {
                            if own_checks[i] > 0. {
                                s[k] / own_checks[i]
                            } else {
                                0.5
                            }
                        } else {
                            s[k] / time_weight
                        }
                    })
                })
                .collect();
            let gaps = evaluate(rows, &avg, pot, bet)?;
            // Extra numerical slack covers cancellation in the sweeps. Input
            // mass is normalized and the live bet-to-pot ratio is bounded.
            if gaps.iter().all(|x| *x + 1e-9 <= gap) && Instant::now() < deadline {
                return Some(Policy {
                    actions: avg,
                    gaps,
                    iterations: iteration,
                });
            }
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    /// Direct joint-deal tree evaluator. Maximise over each player's four
    /// pure contingent plans *after* summing over legal opposing pockets.
    fn exhaustive(rows: &[Combo], profile: &[Strategy], pot: u32, bet: u32) -> [f64; 2] {
        std::array::from_fn(|p| {
            let mut mass = 0.;
            let mut gain = 0.;
            for (i, mine) in rows.iter().enumerate() {
                let mut plans = [0.; 5]; // frozen, then four pure responses
                for (j, theirs) in rows.iter().enumerate() {
                    if mine.cards.iter().any(|c| theirs.cards.contains(c)) {
                        continue;
                    }
                    let w = mine.weight[p] * theirs.weight[1 - p];
                    mass += w;
                    for (k, value) in plans.iter_mut().enumerate() {
                        let mut mine_policy = profile[i];
                        if k > 0 {
                            mine_policy[p] = f64::from(((k - 1) & 1) != 0);
                            mine_policy[p + 2] = f64::from(((k - 1) & 2) != 0);
                        }
                        let profiles = if p == 0 {
                            [mine_policy, profile[j]]
                        } else {
                            [profile[j], mine_policy]
                        };
                        let oop_bet = profiles[0][OOP_OPEN];
                        let oop_call = profiles[0][OOP_CALL];
                        let ip_call = profiles[1][IP_CALL];
                        let ip_bet = profiles[1][IP_OPEN];
                        let award = |wager: u32| {
                            let paid = pot + 2 * wager;
                            let chips = match mine.rank.cmp(&theirs.rank) {
                                std::cmp::Ordering::Less => 0,
                                std::cmp::Ordering::Greater => paid,
                                std::cmp::Ordering::Equal => {
                                    paid / 2 + u32::from(p == 0) * (paid % 2)
                                }
                            };
                            f64::from(chips) - f64::from(wager)
                        };
                        let bet_at_start = if p == 0 { f64::from(pot) } else { 0. };
                        let bet_after_check = f64::from(pot) - bet_at_start;
                        let open_branch = (1. - ip_call) * bet_at_start + ip_call * award(bet);
                        let delayed_branch =
                            oop_call * award(bet) + (1. - oop_call) * bet_after_check;
                        *value += w
                            * (oop_bet * open_branch
                                + (1. - oop_bet)
                                    * (ip_bet * delayed_branch + (1. - ip_bet) * award(0)))
                            / f64::from(pot);
                    }
                }
                gain += plans[1..].iter().copied().fold(f64::NEG_INFINITY, f64::max) - plans[0];
            }
            gain / mass
        })
    }

    fn ranges() -> Vec<Combo> {
        [
            ([1, 2], 1, [1., 3.]),
            ([4, 5], 1, [3., 2.]),
            ([1, 3], 2, [2., 0.]),
            ([6, 7], 3, [0., 4.]),
            ([2, 6], 4, [3., 1.]),
            ([3, 5], 4, [4., 3.]),
            ([8, 9], 5, [1., 5.]),
        ]
        .into_iter()
        .map(|(cards, rank, weight)| Combo {
            cards,
            rank,
            weight,
        })
        .collect()
    }

    #[test]
    fn rank_sweeps_match_hidden_card_consistent_exhaustive_best_responses() {
        let rows = ranges();
        let policy: Vec<Strategy> = (0..rows.len())
            .map(|i| std::array::from_fn(|n| ((i * 13 + n * 7 + 1) % 17) as f64 / 16.))
            .collect();
        for (pot, bet) in [(11, 6), (100, 35), (43, 119)] {
            let actual = evaluate(&rows, &policy, pot, bet).unwrap();
            let expected = exhaustive(&rows, &policy, pot, bet);
            for (a, b) in actual.into_iter().zip(expected) {
                assert!((a - b).abs() < 1e-11);
            }
            let solved = solve(
                &rows,
                pot,
                bet,
                512,
                0.0025,
                Instant::now() + Duration::from_secs(10),
            )
            .unwrap();
            assert!(solved.iterations > 1);
            for (measured, independent) in
                solved
                    .gaps
                    .into_iter()
                    .zip(exhaustive(&rows, &solved.actions, pot, bet))
            {
                assert!(independent <= 0.0025 && (independent - measured).abs() < 1e-11);
            }
        }
    }

    #[test]
    fn refused_solutions_have_no_action_policy() {
        let rows = ranges();
        assert!(solve(
            &rows,
            20,
            10,
            1,
            1e-9,
            Instant::now() + Duration::from_secs(1)
        )
        .is_none());
        assert!(solve(&rows, 20, 10, 256, 0.0025, Instant::now()).is_none());
        let no_compatible = [Combo {
            cards: [1, 2],
            rank: 1,
            weight: [1., 1.],
        }];
        assert!(solve(
            &no_compatible,
            20,
            10,
            4,
            0.0025,
            Instant::now() + Duration::from_secs(1)
        )
        .is_none());
    }
}
