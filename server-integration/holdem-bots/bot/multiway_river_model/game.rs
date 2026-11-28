//! The last seat may check or bet. Each earlier seat can fold or call that
//! wager in turn. Each seat acts once, so its exact unilateral best response
//! in the bucket game is the better action in each of its information sets.
use super::chance::{Joint, K, RANKS};
use std::time::Instant;

pub(super) const ACTOR: [usize; 4] = [2, 0, 1, 1];
type Strategy = [[f64; K]; 4];
type Values = [[[f64; 2]; K]; 4];
pub(super) struct Policy {
    pub p: Strategy,
    pub iterations: usize,
    #[cfg(test)]
    pub gaps: [f64; 3],
}
struct Cell {
    bucket: [usize; 3],
    // Expected payout to each seat, weighted by disjoint deal probability.
    // Leaves: check, both fold, fold/call, call/fold, call/call.
    util: [[f64; 3]; 5],
}
fn payout(ranks: [u8; 3], participants: [bool; 3], pot: u32, bet: u32) -> [f64; 3] {
    let best = (0..3)
        .filter(|&i| participants[i])
        .map(|i| ranks[i])
        .max()
        .unwrap();
    let winners = (0..3)
        .filter(|&i| participants[i] && ranks[i] == best)
        .count() as u64;
    let bets = participants.into_iter().filter(|&x| x).count() as u64;
    let total = u64::from(pot) + bets * u64::from(bet);
    let mut odds = total % winners;
    std::array::from_fn(|i| {
        if !participants[i] {
            return 0.;
        }
        let mut net = -f64::from(bet);
        if ranks[i] == best {
            net += (total / winners) as f64;
            if odds > 0 {
                odds -= 1;
                net += 1.;
            }
        }
        net / f64::from(pot)
    })
}
fn cells(joint: &Joint, pot: u32, bet: u32) -> Option<Vec<Cell>> {
    let mass: f64 = joint.iter().flatten().sum();
    if !mass.is_finite() || mass <= 0. || pot == 0 {
        return None;
    }
    let leaves = RANKS.map(|r| {
        [
            payout(r, [true, true, true], pot, 0),
            [0., 0., 1.],
            payout(r, [false, true, true], pot, bet),
            payout(r, [true, false, true], pot, bet),
            payout(r, [true, true, true], pot, bet),
        ]
    });
    let mut cells = Vec::new();
    for (i, counts) in joint.iter().enumerate() {
        if !counts.iter().any(|&w| w > 0.) {
            continue;
        }
        let mut util = [[0.; 3]; 5];
        for (q, world) in counts.iter().zip(&leaves) {
            for (out, input) in util.iter_mut().zip(world) {
                for p in 0..3 {
                    out[p] += q * input[p] / mass;
                }
            }
        }
        cells.push(Cell {
            bucket: [i / (K * K), i / K % K, i % K],
            util,
        });
    }
    Some(cells)
}
fn mix(lo: f64, hi: f64, p: f64) -> f64 {
    lo + p * (hi - lo)
}
fn evaluate(cells: &[Cell], strategy: &Strategy) -> Values {
    let mut values = [[[0.; 2]; K]; 4];
    for cell in cells {
        let [a, b, c] = cell.bucket;
        let [check, ff, fc, cf, cc] = cell.util;
        let bet = strategy[0][c];
        let call = strategy[1][a];
        let last_fold = strategy[2][b];
        let last_call = strategy[3][b];
        values[0][c][0] += check[2];
        values[0][c][1] += mix(
            mix(ff[2], fc[2], last_fold),
            mix(cf[2], cc[2], last_call),
            call,
        );
        // The responder who folds always receives zero. The final responder
        // sees whether the previous player called when deciding to continue.
        values[1][a][1] += bet * mix(cf[0], cc[0], last_call);
        values[2][b][1] += bet * (1. - call) * fc[1];
        values[3][b][1] += bet * call * cc[1];
    }
    values
}
fn gaps(values: &Values, strategy: &Strategy) -> [f64; 3] {
    let mut out = [0.; 3];
    for (node, bins) in values.iter().enumerate() {
        for (k, &[left, right]) in bins.iter().enumerate() {
            out[ACTOR[node]] += left.max(right) - mix(left, right, strategy[node][k]);
        }
    }
    out
}
pub(super) fn solve(joint: &Joint, pot: u32, bet: u32, deadline: Instant) -> Option<Policy> {
    let cells = cells(joint, pot, bet)?;
    let mut p = [[0.5; K]; 4];
    let mut avg = [[0.; K]; 4];
    let mut regrets = [[[0.; 2]; K]; 4];
    let mut total = 0.;
    for iteration in 1..=512 {
        if Instant::now() >= deadline {
            return None;
        }
        let weight = iteration as f64;
        total += weight;
        let values = evaluate(&cells, &p);
        for node in 0..4 {
            for k in 0..K {
                let [lo, hi] = values[node][k];
                let old = p[node][k];
                avg[node][k] += weight * old;
                let ev = mix(lo, hi, old);
                let r = &mut regrets[node][k];
                r[0] = (r[0] + lo - ev).max(0.);
                r[1] = (r[1] + hi - ev).max(0.);
                if r[0] + r[1] > 0. {
                    p[node][k] = r[1] / (r[0] + r[1]);
                }
            }
        }
        if iteration % 8 == 0 {
            let frozen = avg.map(|v| v.map(|x| x / total));
            let adv = gaps(&evaluate(&cells, &frozen), &frozen);
            if adv.iter().all(|&x| x.is_finite() && x <= 0.0008) && Instant::now() < deadline {
                return Some(Policy {
                    p: frozen,
                    iterations: iteration,
                    #[cfg(test)]
                    gaps: adv,
                });
            }
        }
    }
    None
}
