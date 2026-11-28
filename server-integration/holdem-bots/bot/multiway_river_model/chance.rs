//! Candidate training tensor of bucket triples and showdown order. For
//! each selected responder pair, integrate every compatible button pocket
//! exactly. The subsequent independent per-pocket audit uses the whole grid.
use std::time::Instant;

pub(super) const K: usize = 20;
const ORDERS: usize = 13;
pub(super) const RANKS: [[u8; 3]; ORDERS] = [
    [1, 3, 0],
    [1, 3, 1],
    [1, 3, 2],
    [1, 3, 3],
    [1, 3, 4],
    [3, 1, 0],
    [3, 1, 1],
    [3, 1, 2],
    [3, 1, 3],
    [3, 1, 4],
    [2, 2, 1],
    [2, 2, 2],
    [2, 2, 3],
];
pub(super) type Joint = Vec<[f64; ORDERS]>;

pub(super) struct Hand {
    pub cards: [u8; 2],
    pub rank: u16,
    pub bucket: usize,
    pub weight: [f64; 3],
}
impl Hand {
    fn mask(&self) -> u64 {
        (1 << self.cards[0]) | (1 << self.cards[1])
    }
}

// All these masses are for button hands disjoint from one fixed responder.
// The per-card marginals let us additionally remove the other responder.
#[derive(Clone)]
struct Margins {
    buckets: [f64; K],
    by_card: [[f64; K]; 52],
    low: f64,
    tied: f64,
    low_card: [f64; 52],
    tied_card: [f64; 52],
}
impl Margins {
    fn new() -> Self {
        Self {
            buckets: [0.; K],
            by_card: [[0.; K]; 52],
            low: 0.,
            tied: 0.,
            low_card: [0.; 52],
            tied_card: [0.; 52],
        }
    }
    fn cut(&self, cut: &Hand, blocked: &Hand) -> (f64, f64) {
        let [a, b] = blocked.cards.map(usize::from);
        let own = if cut.bucket == blocked.bucket {
            blocked.weight[2]
        } else {
            0.
        };
        let low = self.low - self.low_card[a] - self.low_card[b]
            + if blocked.rank < cut.rank { own } else { 0. };
        let tied = self.tied - self.tied_card[a] - self.tied_card[b]
            + if blocked.rank == cut.rank { own } else { 0. };
        (low.max(0.), tied.max(0.))
    }
}
pub(super) fn joint(hands: &[Hand], stride: usize, deadline: Instant) -> Option<Joint> {
    let masks: Vec<_> = hands.iter().map(Hand::mask).collect();
    let button: Vec<_> = hands
        .iter()
        .zip(&masks)
        .filter(|(h, _)| h.weight[2] > 0.)
        .collect();
    let mut margins = Vec::with_capacity(hands.len());
    for (i, h) in hands.iter().enumerate() {
        if i % 16 == 0 && Instant::now() >= deadline {
            return None;
        }
        let mut m = Margins::new();
        if h.weight[0] > 0. || h.weight[1] > 0. {
            for &(other, &mask) in &button {
                if masks[i] & mask != 0 {
                    continue;
                }
                let [a, b] = other.cards.map(usize::from);
                let k = other.bucket;
                let w = other.weight[2];
                m.buckets[k] += w;
                m.by_card[a][k] += w;
                m.by_card[b][k] += w;
                if k == h.bucket {
                    if other.rank < h.rank {
                        m.low += w;
                        m.low_card[a] += w;
                        m.low_card[b] += w;
                    } else if other.rank == h.rank {
                        m.tied += w;
                        m.tied_card[a] += w;
                        m.tied_card[b] += w;
                    }
                }
            }
        }
        margins.push(m);
    }
    let middle: Vec<_> = hands
        .iter()
        .enumerate()
        .filter(|(_, h)| h.weight[1] > 0.)
        .collect();
    let mut joint = vec![[0.; ORDERS]; K * K * K];
    for (i, h0) in hands.iter().enumerate() {
        if i % 8 == 0 && Instant::now() >= deadline {
            return None;
        }
        if h0.weight[0] == 0. {
            continue;
        }
        let m0 = &margins[i];
        // Stratify the responder pair grid for candidate training only.
        // Successive ranks cycle through the possible suit/blocker offsets.
        // The final audit independently integrates *all* joint deals.
        let offset = (i + i / stride) % stride;
        for &(j, h1) in middle.iter().skip(offset).step_by(stride) {
            if masks[i] & masks[j] != 0 {
                continue;
            }
            let [a, b] = h1.cards.map(usize::from);
            let w = h0.weight[0] * h1.weight[1];
            let out = &mut joint[(h0.bucket * K + h1.bucket) * K..][..K];
            let (lo, hi, mlo, mhi, base) = if h0.rank <= h1.rank {
                (h0, h1, m0, &margins[j], 0)
            } else {
                (h1, h0, &margins[j], m0, 5)
            };
            let same = lo.rank == hi.rank;
            let (ltlo, eqlo) = mlo.cut(lo, hi);
            let (lthi, eqhi) = if same { (ltlo, eqlo) } else { mhi.cut(hi, lo) };
            for (k, row) in out.iter_mut().enumerate() {
                let t = m0.buckets[k] - m0.by_card[a][k] - m0.by_card[b][k]
                    + if k == h1.bucket { h1.weight[2] } else { 0. };
                if t <= 0. {
                    continue;
                }
                if same {
                    if k < lo.bucket {
                        row[10] += w * t;
                    } else if k > lo.bucket {
                        row[12] += w * t;
                    } else {
                        row[10] += w * ltlo;
                        row[11] += w * eqlo;
                        row[12] += w * (t - ltlo - eqlo).max(0.);
                    }
                } else if k < lo.bucket {
                    row[base] += w * t;
                } else if k > hi.bucket {
                    row[base + 4] += w * t;
                } else if k != lo.bucket && k != hi.bucket {
                    row[base + 2] += w * t;
                } else {
                    let (l, e) = if k == lo.bucket {
                        (ltlo, eqlo)
                    } else {
                        (0., 0.)
                    };
                    let (u, v) = if k == hi.bucket {
                        (lthi, eqhi)
                    } else {
                        (t, 0.)
                    };
                    row[base] += w * l;
                    row[base + 1] += w * e;
                    row[base + 2] += w * (u - l - e).max(0.);
                    row[base + 3] += w * v;
                    row[base + 4] += w * (t - u - v).max(0.);
                }
            }
        }
    }
    Some(joint)
}
