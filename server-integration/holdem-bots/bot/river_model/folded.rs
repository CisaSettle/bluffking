//! Exact folded-pocket marginal and unilateral river response certificates.
//! The folder's action may have been highly selective. Its two unrevealed
//! cards are integrated over the authenticated preflop flop-reach factors.

use super::{
    card,
    solve::{Combo, Policy, IP_CALL, IP_OPEN, OOP_CALL, OOP_OPEN},
    FlopReach,
};
use engine::HoleCards;
use std::time::Instant;

type Pairs = [[f64; 52]; 52];

struct Marginals {
    total: f64,
    by_card: [f64; 52],
    pair: Pairs,
}

impl Marginals {
    fn empty() -> Self {
        Self {
            total: 0.,
            by_card: [0.; 52],
            pair: [[0.; 52]; 52],
        }
    }
    fn add_pocket(&mut self, a: usize, b: usize, w: f64) {
        self.total += w;
        self.by_card[a] += w;
        self.by_card[b] += w;
        self.pair[a][b] = w;
        self.pair[b][a] = w;
    }
}

pub(super) struct FoldRange {
    union: Marginals,
    two: Option<Box<TwoFolders>>,
}

struct TwoFolders {
    first: Pairs,
    second: Pairs,
    triple: Vec<Pairs>,
}

pub(super) struct Available<'a> {
    all: f64,
    by_card: [f64; 52],
    pair: &'a Pairs,
    two: Option<(&'a TwoFolders, usize, usize)>,
}

impl FoldRange {
    pub(super) fn new(rows: &[Combo], reach: &FlopReach, seats: &[(u8, u32)]) -> Option<Self> {
        let weights = |seat| {
            let mut out = Marginals::empty();
            for row in rows {
                let [a, b] = row.cards;
                let w = reach.weight(seat, HoleCards::new(card(a), card(b)))?;
                out.add_pocket(a as usize, b as usize, w);
            }
            (out.total > 0.).then_some(out)
        };
        match seats {
            [(seat, _)] => Some(Self {
                union: weights(*seat)?,
                two: None,
            }),
            [(first, _), (second, _)] => Self::two(weights(*first)?, weights(*second)?),
            _ => None,
        }
    }

    fn two(first: Marginals, second: Marginals) -> Option<Self> {
        let mut triple = vec![[[0.; 52]; 52]; 52];
        let mut union = Marginals::empty();
        // For each triple in the four dead cards, one folder owns an entire
        // pair from the triple, the other owns the remaining card plus a free
        // fourth. Sum the six possible assignments without enumerating it.
        let assigned = |a: usize, b: usize, c: usize| {
            first.pair[a][b] * (second.by_card[c] - second.pair[c][a] - second.pair[c][b])
                + second.pair[a][b] * (first.by_card[c] - first.pair[c][a] - first.pair[c][b])
        };
        let cards: [usize; 52] = std::array::from_fn(|i| i);
        for &a in &cards {
            for &b in &cards[a + 1..] {
                for &c in &cards[b + 1..] {
                    let weight = assigned(a, b, c) + assigned(a, c, b) + assigned(b, c, a);
                    triple[a][b][c] = weight;
                    triple[a][c][b] = weight;
                    triple[b][a][c] = weight;
                    triple[b][c][a] = weight;
                    triple[c][a][b] = weight;
                    triple[c][b][a] = weight;
                    // Each four-card deal containing a pair has two other cards.
                    for (x, y) in [(a, b), (a, c), (b, c)] {
                        union.pair[x][y] += weight * 0.5;
                    }
                }
            }
        }
        for &a in &cards {
            for &b in &cards[a + 1..] {
                let weight = union.pair[a][b];
                union.pair[b][a] = weight;
                // Three other cards accompany either member of the pair.
                union.by_card[a] += weight / 3.;
                union.by_card[b] += weight / 3.;
            }
        }
        union.total = union.by_card.iter().sum::<f64>() / 4.;
        (union.total > 0.).then(|| Self {
            union,
            two: Some(Box::new(TwoFolders {
                first: first.pair,
                second: second.pair,
                triple,
            })),
        })
    }

    pub(super) fn excluding(&self, hero: [u8; 2]) -> Available<'_> {
        let [a, b] = hero.map(usize::from);
        let f = &self.union;
        Available {
            all: f.total - f.by_card[a] - f.by_card[b] + f.pair[a][b],
            by_card: std::array::from_fn(|c| {
                f.by_card[c] - f.pair[a][c] - f.pair[b][c]
                    + self.two.as_ref().map_or(0., |two| two.triple[a][b][c])
            }),
            pair: &f.pair,
            two: self.two.as_deref().map(|two| (two, a, b)),
        }
    }
}

impl Available<'_> {
    /// Inclusion-exclusion over four live cards. Villain cannot overlap hero.
    #[inline]
    pub(super) fn mass(&self, other: [u8; 2]) -> f64 {
        let (c, d) = (usize::from(other[0]), usize::from(other[1]));
        let mut mass = self.all - self.by_card[c] - self.by_card[d] + self.pair[c][d];
        if let Some((t, a, b)) = self.two {
            // For a specified quartet the only assignments consist of the six
            // ways to assign a pair to the first folder and its complement
            // to the second. Both actual players' distinct factors are used.
            mass += t.first[a][b] * t.second[c][d]
                + t.second[a][b] * t.first[c][d]
                + t.first[a][c] * t.second[b][d]
                + t.second[a][c] * t.first[b][d]
                + t.first[a][d] * t.second[b][c]
                + t.second[a][d] * t.first[b][c]
                - t.triple[a][c][d]
                - t.triple[b][c][d];
        }
        mass.max(0.)
    }
}

#[derive(Clone, Copy)]
struct BranchValues {
    // Against each opponent pocket, three counterfactual values for each
    // player. OOP: bet at root, check back, check-call; IP: call an opener,
    // check after OOP check, bet after OOP check.
    b0: f64,
    c0: f64,
    k0: f64,
    k1: f64,
    c1: f64,
    b1: f64,
}

struct Rival {
    cards: [u8; 2],
    mask: u64,
    // loss, tie, win from the held hero pocket's point of view
    values: [BranchValues; 3],
    mass: [f64; 2],
}

pub(super) fn gaps(
    rows: &[Combo],
    policy: &Policy,
    folder: &FoldRange,
    pot: u32,
    bet: u32,
    deadline: Instant,
) -> Option<[f64; 2]> {
    let b = f64::from(bet) / f64::from(pot);
    let odd = f64::from(pot % 2) * 0.5 / f64::from(pot);
    let rivals: Vec<_> = rows
        .iter()
        .zip(&policy.actions)
        .map(|(r, s)| {
            let [w1, w0] = r.weight;
            let [a, c] = r.cards;
            Rival {
                cards: r.cards,
                mask: (1u64 << a) | (1u64 << c),
                mass: [w0, w1],
                values: std::array::from_fn(|outcome| {
                    let half = outcome as f64 * 0.5;
                    let odd = if outcome == 1 { odd } else { 0. };
                    let base = (1. + 2. * b) * half - b;
                    let win0 = base + odd;
                    let win1 = base - odd;
                    let check = 1. - s[OOP_OPEN];
                    BranchValues {
                        b0: w0 * (1. - s[IP_CALL] + s[IP_CALL] * win0),
                        c0: w0 * (1. - s[IP_OPEN]) * (half + odd),
                        k0: w0 * s[IP_OPEN] * win0,
                        k1: w1 * s[OOP_OPEN] * win1,
                        c1: w1 * check * (half - odd),
                        b1: w1 * check * (1. - s[OOP_CALL] + s[OOP_CALL] * win1),
                    }
                }),
            }
        })
        .collect();
    let mut total = [0.; 2];
    let mut gain = [0.; 2];
    for (i, h) in rows.iter().enumerate() {
        if i.is_multiple_of(16) && Instant::now() >= deadline {
            return None;
        }
        let [a, b] = h.cards;
        let mask = (1u64 << a) | (1u64 << b);
        let available = folder.excluding(h.cards);
        let mut b0 = 0.;
        let mut c0 = 0.;
        let mut k0 = 0.;
        let mut k1 = 0.;
        let mut c1 = 0.;
        let mut b1 = 0.;
        let mut m0 = 0.;
        let mut m1 = 0.;
        let lo = rows.partition_point(|r| r.rank < h.rank);
        let hi = rows.partition_point(|r| r.rank <= h.rank);
        for (outcome, slice) in [(2, &rivals[..lo]), (1, &rivals[lo..hi]), (0, &rivals[hi..])] {
            for r in slice {
                if mask & r.mask != 0 {
                    continue;
                }
                let f = available.mass(r.cards);
                let v = r.values[outcome];
                m0 += f * r.mass[0];
                m1 += f * r.mass[1];
                b0 += f * v.b0;
                c0 += f * v.c0;
                k0 += f * v.k0;
                k1 += f * v.k1;
                c1 += f * v.c1;
                b1 += f * v.b1;
            }
        }
        let s = policy.actions[i];
        let ev0 = s[OOP_OPEN] * b0 + (1. - s[OOP_OPEN]) * (c0 + s[OOP_CALL] * k0);
        let ev1 = s[IP_CALL] * k1 + s[IP_OPEN] * b1 + (1. - s[IP_OPEN]) * c1;
        gain[0] += h.weight[0] * (b0.max(c0 + k0.max(0.)) - ev0);
        gain[1] += h.weight[1] * (k1.max(0.) + b1.max(c1) - ev1);
        total[0] += h.weight[0] * m0;
        total[1] += h.weight[1] * m1;
    }
    let result = [gain[0] / total[0], gain[1] / total[1]];
    (result.iter().all(|x| x.is_finite() && *x >= -1e-10) && Instant::now() < deadline)
        .then_some(result)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    #[test]
    fn folded_pocket_gaps_match_direct_three_hand_deals() {
        check_against_all_deals(None);
    }

    #[test]
    fn two_folded_pockets_match_direct_four_hand_deals() {
        check_against_all_deals(Some([0.49, 0.22, 0.87, 0.08, 0.40, 0.62, 0.31, 0.05]));
    }

    fn check_against_all_deals(second_weights: Option<[f64; 8]>) {
        let pairs = [
            [0, 1],
            [0, 2],
            [2, 3],
            [5, 6],
            [7, 8],
            [4, 7],
            [1, 6],
            [3, 5],
        ];
        let mut first = Marginals::empty();
        let weights = [0.13, 0.52, 0.0, 0.17, 0.23, 0.91, 0.07, 0.64];
        for (&[a, b], w) in pairs.iter().zip(weights) {
            first.add_pocket(a as usize, b as usize, w);
        }
        let dead = if let Some(second_weights) = second_weights {
            let mut second = Marginals::empty();
            for (&[a, b], w) in pairs.iter().zip(second_weights) {
                second.add_pocket(a as usize, b as usize, w);
            }
            FoldRange::two(first, second).unwrap()
        } else {
            FoldRange {
                union: first,
                two: None,
            }
        };
        let rows: Vec<_> = pairs
            .iter()
            .enumerate()
            .map(|(i, &cards)| Combo {
                cards,
                rank: (i / 2) as u32,
                weight: [(1 + i) as f64 / 9., (8 - i) as f64 / 8.],
            })
            .collect();
        let policy = Policy {
            actions: (0..pairs.len())
                .map(|i| std::array::from_fn(|n| ((i * 7 + n * 3 + 1) % 11) as f64 / 10.))
                .collect(),
            iterations: 1,
            gaps: [0.; 2],
        };
        let (pot, bet) = (11, 6);
        let exact = gaps(
            &rows,
            &policy,
            &dead,
            pot,
            bet,
            Instant::now() + Duration::from_secs(1),
        )
        .unwrap();
        for (p, value) in exact.into_iter().enumerate() {
            let mut mass = 0.;
            let mut gain = 0.;
            for (i, h) in rows.iter().enumerate() {
                let mut plans = [0.; 5];
                for (j, v) in rows.iter().enumerate() {
                    if h.cards.iter().any(|c| v.cards.contains(c)) {
                        continue;
                    }
                    let direct: f64 = pairs
                        .iter()
                        .zip(weights)
                        .filter(|(f, _)| {
                            !f.iter().any(|c| h.cards.contains(c) || v.cards.contains(c))
                        })
                        .map(|(f, w)| {
                            w * second_weights.map_or(1., |second| {
                                pairs
                                    .iter()
                                    .zip(second)
                                    .filter(|(g, _)| {
                                        !g.iter().any(|c| {
                                            f.contains(c)
                                                || h.cards.contains(c)
                                                || v.cards.contains(c)
                                        })
                                    })
                                    .map(|(_, w)| w)
                                    .sum()
                            })
                        })
                        .sum();
                    let k = dead.excluding(h.cards).mass(v.cards);
                    assert!((k - direct).abs() < 1e-12);
                    let w = h.weight[p] * v.weight[1 - p] * direct;
                    mass += w;
                    let win = |stake: u32| {
                        let gross = pot + 2 * stake;
                        let award = match h.rank.cmp(&v.rank) {
                            std::cmp::Ordering::Equal => {
                                gross / 2 + u32::from(p == 0) * (gross % 2)
                            }
                            std::cmp::Ordering::Less => 0,
                            std::cmp::Ordering::Greater => gross,
                        };
                        (f64::from(award) - f64::from(stake)) / f64::from(pot)
                    };
                    for (n, out) in plans.iter_mut().enumerate() {
                        let mut own = policy.actions[i];
                        if n > 0 {
                            own[p] = f64::from((n - 1) & 1 != 0);
                            own[p + 2] = f64::from((n - 1) & 2 != 0);
                        }
                        let [o, t] = if p == 0 {
                            [own, policy.actions[j]]
                        } else {
                            [policy.actions[j], own]
                        };
                        let a = o[OOP_OPEN];
                        let b = t[IP_OPEN];
                        let c = o[OOP_CALL];
                        let d = t[IP_CALL];
                        let awarded = (a * (1. - d) * f64::from(p == 0))
                            + (1. - a) * b * (1. - c) * f64::from(p == 1)
                            + (1. - a) * (1. - b) * win(0)
                            + (a * d + (1. - a) * b * c) * win(bet);
                        *out += w * awarded;
                    }
                }
                gain += plans[1..].iter().copied().reduce(f64::max).unwrap() - plans[0];
            }
            assert!((value - gain / mass).abs() < 1e-11);
        }
    }
}
