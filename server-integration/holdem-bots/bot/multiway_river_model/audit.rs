//! Exact unilateral deviations by private pocket against a *frozen* profile.
//! The action tree gives each player one turn. For the two opponents' ranges
//! A and B already excluding hero's cards, their compatible weight is
//! sum(A) sum(B) minus the dot of card marginals plus sum_h A(h) B(h).
//! The final term corrects identical pockets being subtracted twice.
use super::{
    chance::{Hand, K},
    game::ACTOR,
};
use std::time::Instant;

const ALL: usize = 0;
const LOW: usize = 1;
const TIE: usize = 2;
struct Mass {
    total: f64,
    card: [f64; 52],
    both: Box<[[f64; 52]; 52]>,
}
impl Mass {
    fn new() -> Self {
        Self {
            total: 0.,
            card: [0.; 52],
            both: Box::new([[0.; 52]; 52]),
        }
    }
    fn add(&mut self, c: [usize; 2], w: f64) {
        let [a, b] = c;
        self.total += w;
        self.card[a] += w;
        self.card[b] += w;
        self.both[a][b] += w;
        self.both[b][a] += w;
    }
    fn total_excluding(&self, a: usize, b: usize) -> f64 {
        self.total - self.card[a] - self.card[b] + self.both[a][b]
    }
    fn exclude(&self, a: usize, b: usize) -> Blocked {
        let mut cards = [0.; 52];
        for (z, item) in cards.iter_mut().enumerate() {
            *item = self.card[z] - self.both[a][z] - self.both[b][z];
        }
        cards[a] = 0.;
        cards[b] = 0.;
        Blocked {
            total: self.total_excluding(a, b),
            cards,
        }
    }
}
struct Blocked {
    total: f64,
    cards: [f64; 52],
}
impl Blocked {
    fn joint(&self, b: &Self, same: f64) -> f64 {
        let mut dot = 0.;
        for i in 0..52 {
            dot += self.cards[i] * b.cards[i];
        }
        (self.total * b.total - dot + same).max(0.)
    }
}
struct Factors([Mass; 3]); // A, B, A*B on the same two cards
impl Factors {
    fn new() -> Self {
        Self(std::array::from_fn(|_| Mass::new()))
    }
    fn add(&mut self, c: [usize; 2], w: [f64; 2]) {
        self.0[0].add(c, w[0]);
        self.0[1].add(c, w[1]);
        self.0[2].add(c, w[0] * w[1]);
    }
}
struct Leaf {
    node: usize,
    action: usize,
    // Two other seats in ascending seat order. At zero the action-path
    // factor is one. Positive node+1 means took that action; negative
    // means declined it.
    filters: [i8; 2],
    active: [bool; 3],
    wager: bool,
}
const LEAVES: [Leaf; 9] = [
    Leaf {
        node: 0,
        action: 0,
        filters: [0, 0],
        active: [true, true, true],
        wager: false,
    },
    Leaf {
        node: 0,
        action: 1,
        filters: [-2, -3],
        active: [false, false, true],
        wager: true,
    },
    Leaf {
        node: 0,
        action: 1,
        filters: [-2, 3],
        active: [false, true, true],
        wager: true,
    },
    Leaf {
        node: 0,
        action: 1,
        filters: [2, -4],
        active: [true, false, true],
        wager: true,
    },
    Leaf {
        node: 0,
        action: 1,
        filters: [2, 4],
        active: [true, true, true],
        wager: true,
    },
    Leaf {
        node: 1,
        action: 1,
        filters: [-4, 1],
        active: [true, false, true],
        wager: true,
    },
    Leaf {
        node: 1,
        action: 1,
        filters: [4, 1],
        active: [true, true, true],
        wager: true,
    },
    Leaf {
        node: 2,
        action: 1,
        filters: [-2, 1],
        active: [false, true, true],
        wager: true,
    },
    Leaf {
        node: 3,
        action: 1,
        filters: [2, 1],
        active: [true, true, true],
        wager: true,
    },
];
fn branch(filter: i8, bucket: usize, p: &[[f64; K]; 4]) -> f64 {
    if filter == 0 {
        return 1.;
    }
    let v = p[filter.unsigned_abs() as usize - 1][bucket];
    if filter > 0 {
        v
    } else {
        1. - v
    }
}
fn share(total: u64, hero: usize, ties: &[usize]) -> f64 {
    let n = 1 + ties.len() as u64;
    (total / n + u64::from((ties.iter().filter(|&&s| s < hero).count() as u64) < total % n)) as f64
}
pub(super) struct Certificate {
    pub gaps: [f64; 3],
    #[cfg(test)]
    pub bucket_gaps: [f64; 3],
    /// Conditional on this exact button pocket and opponent card removal.
    pub opening: Vec<[f64; 2]>,
    pub first_call_rate: f64,
}
pub(super) fn evaluate(
    rows: &[Hand],
    p: &[[f64; K]; 4],
    pot: u32,
    bet: u32,
    deadline: Instant,
) -> Option<Certificate> {
    integrate(rows, p, pot, bet, &LEAVES, deadline)
}

pub(super) fn opening_values(
    rows: &[Hand],
    p: &[[f64; K]; 4],
    pot: u32,
    bet: u32,
    deadline: Instant,
) -> Option<Vec<[f64; 2]>> {
    Some(integrate(rows, p, pot, bet, &LEAVES[..5], deadline)?.opening)
}

fn integrate(
    rows: &[Hand],
    p: &[[f64; K]; 4],
    pot: u32,
    bet: u32,
    leaves: &[Leaf],
    deadline: Instant,
) -> Option<Certificate> {
    let mut opening_mass = vec![0.; rows.len()];
    let mut bet_mass = 0.;
    let mut call_mass = 0.;
    let mut values = vec![[[0.; 2]; 4]; rows.len()];
    let mut mass = 0.;
    for l in leaves {
        let hero = ACTOR[l.node];
        let others = match hero {
            0 => [1, 2],
            1 => [0, 2],
            _ => [0, 1],
        };
        let [a_live, b_live] = others.map(|s| l.active[s]);
        let amount = if l.wager { u64::from(bet) } else { 0 };
        let total = u64::from(pot) + amount * l.active.into_iter().filter(|&v| v).count() as u64;
        let pa = share(total, hero, &[others[0]]);
        let pb = share(total, hero, &[others[1]]);
        let pab = share(total, hero, &others);
        let mut full = Factors::new();
        let mut low = Factors::new();
        let weights: Vec<_> = rows
            .iter()
            .map(|h| {
                std::array::from_fn(|i| h.weight[others[i]] * branch(l.filters[i], h.bucket, p))
            })
            .collect();
        for (h, &w) in rows.iter().zip(&weights) {
            full.add(h.cards.map(usize::from), w);
        }
        let mut start = 0;
        while start < rows.len() {
            if Instant::now() >= deadline {
                return None;
            }
            let end = start + rows[start..].partition_point(|h| h.rank == rows[start].rank);
            let mut tie = Factors::new();
            for i in start..end {
                tie.add(rows[i].cards.map(usize::from), weights[i]);
            }
            let by_rank = [&full, &low, &tie];
            for i in start..end {
                if rows[i].weight[hero] == 0. {
                    continue;
                }
                let [x, y] = rows[i].cards.map(usize::from);
                let first = by_rank.map(|m| m.0[0].exclude(x, y));
                let second = by_rank.map(|m| m.0[1].exclude(x, y));
                let identical = by_rank.map(|m| m.0[2].total_excluding(x, y));
                let joint = |a: usize, b: usize| {
                    let same = if a == ALL {
                        identical[b]
                    } else if b == ALL || a == b {
                        identical[a]
                    } else {
                        0.
                    };
                    first[a].joint(&second[b], same)
                };
                let all = joint(ALL, ALL);
                let mut ev = -(amount as f64) * all;
                let a = if a_live { LOW } else { ALL };
                let b = if b_live { LOW } else { ALL };
                ev += total as f64 * joint(a, b);
                if a_live {
                    ev += pa * joint(TIE, b);
                }
                if b_live {
                    ev += pb * joint(a, TIE);
                }
                if a_live && b_live {
                    ev += pab * joint(TIE, TIE);
                }
                values[i][l.node][l.action] += ev / f64::from(pot) * rows[i].weight[hero];
                if l.node == 0 && l.action == 0 {
                    opening_mass[i] = all * rows[i].weight[hero];
                    mass += opening_mass[i];
                } else if l.node == 0 {
                    let reach = all * rows[i].weight[hero] * p[0][rows[i].bucket];
                    bet_mass += reach;
                    if l.active[0] {
                        call_mass += reach;
                    }
                }
            }
            for i in start..end {
                low.add(rows[i].cards.map(usize::from), weights[i]);
            }
            start = end;
        }
    }
    if !mass.is_finite() || mass <= 0. {
        return None;
    }
    let opening: Vec<_> = values
        .iter()
        .zip(opening_mass)
        .map(|(v, m)| if m > 0. { v[0].map(|x| x / m) } else { [0.; 2] })
        .collect();
    let mut gaps = [0.; 3];
    let mut by_bucket = [[[0.; 2]; K]; 4];
    for (h, v) in rows.iter().zip(values) {
        for (n, [x, y]) in v.into_iter().enumerate() {
            gaps[ACTOR[n]] += x.max(y) - (x + p[n][h.bucket] * (y - x));
            by_bucket[n][h.bucket][0] += x;
            by_bucket[n][h.bucket][1] += y;
        }
    }
    let mut bucket_gaps = [0.; 3];
    for (n, values) in by_bucket.into_iter().enumerate() {
        for (k, [x, y]) in values.into_iter().enumerate() {
            bucket_gaps[ACTOR[n]] += x.max(y) - (x + p[n][k] * (y - x));
        }
    }
    let gaps = gaps.map(|x| x / mass);
    let bucket_gaps = bucket_gaps.map(|x| x / mass);
    (gaps.iter().chain(&bucket_gaps).all(|g| g.is_finite()) && Instant::now() < deadline).then_some(
        Certificate {
            gaps,
            #[cfg(test)]
            bucket_gaps,
            opening,
            first_call_rate: if bet_mass > 0. {
                call_mass / bet_mass
            } else {
                0.
            },
        },
    )
}
