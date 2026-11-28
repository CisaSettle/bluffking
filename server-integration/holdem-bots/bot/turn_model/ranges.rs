use super::Request;
use engine::{Card, HoleCards, Rank, Suit};
use std::fmt::Write;

struct Row {
    cards: [usize; 2],
    hole: HoleCards,
    weight: [f64; 2],
}

/// Sum both players' product likelihood over disjoint holdings without
/// expanding over a million joint assignments. The identical combo shares both
/// cards and was subtracted twice, so add its weight back once.
fn joint_mass(rows: &[Row], keep: &[[bool; 2]]) -> f64 {
    let mut card = [0.; 52];
    let mut total = 0.;
    for (r, k) in rows.iter().zip(keep) {
        if k[1] {
            total += r.weight[1];
            for c in r.cards {
                card[c] += r.weight[1];
            }
        }
    }
    rows.iter()
        .zip(keep)
        .filter(|(_, k)| k[0])
        .map(|(r, k)| {
            let [a, b] = r.cards;
            r.weight[0] * (total - card[a] - card[b] + if k[1] { r.weight[1] } else { 0. }).max(0.)
        })
        .sum()
}

pub(super) fn for_solve(req: &Request) -> Option<([String; 2], f64)> {
    let mut rows = Vec::with_capacity(1128);
    let cards: Vec<_> = Rank::ALL
        .into_iter()
        .flat_map(|r| Suit::ALL.map(|s| Card::new(r, s)))
        .collect();
    for (a, &first) in cards.iter().enumerate() {
        if req.board.contains(&first) {
            continue;
        }
        for (b, &second) in cards.iter().enumerate().skip(a + 1) {
            if req.board.contains(&second) {
                continue;
            }
            // The upstream exact-combo grammar requires high rank first.
            let hole = HoleCards::new(second, first);
            let mut weight = [0.; 2];
            for (actor, w) in weight.iter_mut().enumerate() {
                *w = f64::from(
                    (req.preflop.weight(req.seats[actor], hole)? * req.lines[actor].weight(hole))
                        as f32,
                );
                if !w.is_finite() || !(0.0..=1.0).contains(w) {
                    return None;
                }
            }
            rows.push(Row {
                cards: [a, b],
                hole,
                weight,
            });
        }
    }
    let full = joint_mass(&rows, &vec![[true; 2]; rows.len()]);
    if full <= 0.0 {
        return None;
    }
    let mut keep = vec![[false; 2]; rows.len()];
    // Bound the working set before constructing the upstream chance tree. A
    // broad line whose tail carries material joint mass is declined.
    let ordered = [0, 1].map(|actor| {
        let mut sorted: Vec<_> = (0..rows.len()).collect();
        sorted.sort_by(|&a, &b| rows[b].weight[actor].total_cmp(&rows[a].weight[actor]));
        sorted
    });
    // Winnings relative to turn root lie in [-stack, pot+stack]. Any
    // continuation and its best response differ by at most this span on the
    // excluded event. The retained joint distribution is exactly conditional.
    let span = 1. + 2. * f64::from(req.stack) / f64::from(req.pot);
    let mut previous = 0;
    let mut tail_loss = f64::INFINITY;
    for limit in [128, 224, 320, 448] {
        for (actor, order) in ordered.iter().enumerate() {
            for &i in &order[previous..limit] {
                keep[i][actor] = rows[i].weight[actor] > 0.;
            }
        }
        previous = limit;
        let retained = joint_mass(&rows, &keep);
        tail_loss = (1. - retained / full).clamp(0., 1.) * span;
        if tail_loss <= 0.005 {
            break;
        }
    }
    if tail_loss > 0.005 {
        return None;
    }
    let mut ranges = [String::new(), String::new()];
    for (r, k) in rows.iter().zip(keep) {
        for (p, text) in ranges.iter_mut().enumerate() {
            if !k[p] {
                continue;
            }
            if !text.is_empty() {
                text.push(',');
            }
            write!(
                text,
                "{}{}:{}",
                r.hole.card1, r.hole.card2, r.weight[p] as f32
            )
            .ok()?;
        }
    }
    Some((ranges, tail_loss))
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn turn_model_tail_bound_matches_blocked_joint_sum() {
        let mut rows = Vec::new();
        for a in 0..6 {
            for b in a + 1..6 {
                rows.push(Row {
                    cards: [a, b],
                    hole: HoleCards::new(
                        Card::new(Rank::ALL[a], Suit::Hearts),
                        Card::new(Rank::ALL[b], Suit::Clubs),
                    ),
                    weight: [(a + b) as f64 / 10., (1 + b - a) as f64 / 8.],
                });
            }
        }
        let keep: Vec<_> = (0..rows.len()).map(|i| [i % 3 != 0, i % 5 != 0]).collect();
        let mut brute = 0.;
        for (i, l) in rows.iter().enumerate() {
            for (j, r) in rows.iter().enumerate() {
                if keep[i][0] && keep[j][1] && !l.cards.iter().any(|c| r.cards.contains(c)) {
                    brute += l.weight[0] * r.weight[1];
                }
            }
        }
        assert!((brute - joint_mass(&rows, &keep)).abs() < 1e-12);
    }
}
