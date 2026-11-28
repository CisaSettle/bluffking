//! The same explicitly documented rules as the browser practice variant.
use serde::{Deserialize, Serialize};
pub type Tile = u8;
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Kind {
    Chow,
    Pung,
    Kong,
    Concealed,
}
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Meld {
    pub kind: Kind,
    pub tiles: Vec<Tile>,
    pub from: usize,
}
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct Seat {
    pub hand: Vec<Tile>,
    pub melds: Vec<Meld>,
    pub flowers: Vec<Tile>,
    pub river: Vec<Tile>,
    pub ready: bool,
    pub waits: Vec<Tile>,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Score {
    pub seat: usize,
    pub flowers: usize,
    pub fan: u32,
    pub patterns: Vec<String>,
    pub points: i64,
}
pub fn count(tiles: &[Tile], tile: Tile) -> usize {
    tiles.iter().filter(|&&t| t == tile).count()
}
pub fn remove(tiles: &mut Vec<Tile>, tile: Tile) {
    let i = tiles
        .iter()
        .position(|&t| t == tile)
        .expect("validated tile");
    tiles.remove(i);
}
fn concealed_counts(hand: impl Iterator<Item = Tile>) -> Option<[u8; 31]> {
    let mut counts = [0; 31];
    for tile in hand {
        let n = counts.get_mut(tile as usize)?;
        *n += 1;
        if *n > 4 {
            return None;
        }
    }
    Some(counts)
}

fn suit_totals(c: &[u8; 31]) -> [u8; 4] {
    let mut totals = [0; 4];
    for (suit, tiles) in c.chunks(9).enumerate() {
        totals[suit] = tiles.iter().sum();
    }
    totals
}

// A set is wholly inside one suit or the honours. Only the pair's group
// can contain a number of tiles congruent to two modulo three.
fn pair_group(totals: [u8; 4]) -> Option<usize> {
    let mut pair = None;
    for (group, size) in totals.into_iter().enumerate() {
        match size % 3 {
            0 => {}
            2 if pair.is_none() => pair = Some(group),
            _ => return None,
        }
    }
    pair
}

// Scoring visits all decompositions in their original order. Legality stops at
// the first one. A four-set hand has at most four concealed triplets.
fn visit_decompositions(
    mut c: [u8; 31],
    pair: usize,
    mut visit: impl FnMut(&[Tile]) -> bool,
) -> bool {
    fn groups(
        c: &mut [u8; 31],
        triplets: &mut [Tile; 4],
        used: usize,
        visit: &mut impl FnMut(&[Tile]) -> bool,
    ) -> bool {
        let Some(t) = c.iter().position(|&n| n > 0) else {
            return visit(&triplets[..used]);
        };
        if c[t] >= 3 {
            c[t] -= 3;
            triplets[used] = t as Tile;
            let done = groups(c, triplets, used + 1, visit);
            c[t] += 3;
            if done {
                return true;
            }
        }
        if t < 27 && t % 9 <= 6 && c[t + 1] > 0 && c[t + 2] > 0 {
            c[t] -= 1;
            c[t + 1] -= 1;
            c[t + 2] -= 1;
            let done = groups(c, triplets, used, visit);
            c[t] += 1;
            c[t + 1] += 1;
            c[t + 2] += 1;
            return done;
        }
        false
    }
    let mut triplets = [0; 4];
    for p in pair * 9..((pair + 1) * 9).min(c.len()) {
        if c[p] >= 2 {
            c[p] -= 2;
            let done = groups(&mut c, &mut triplets, 0, &mut visit);
            c[p] += 2;
            if done {
                return true;
            }
        }
    }
    false
}

fn complete_counts(hand: &[Tile], melds: usize) -> Option<([u8; 31], usize)> {
    if melds > 4 || hand.len() != 14 - melds * 3 {
        return None;
    }
    let c = concealed_counts(hand.iter().copied())?;
    Some((c, pair_group(suit_totals(&c))?))
}

pub fn decompositions(hand: &[Tile], melds: usize) -> Vec<Vec<Tile>> {
    let mut out = vec![];
    if let Some((c, pair)) = complete_counts(hand, melds) {
        visit_decompositions(c, pair, |triplets| {
            out.push(triplets.to_vec());
            false
        });
    }
    out
}
pub fn winning(hand: &[Tile], melds: usize) -> bool {
    complete_counts(hand, melds).is_some_and(|(c, pair)| visit_decompositions(c, pair, |_| true))
}

fn winning_draws(
    hand: impl Iterator<Item = Tile>,
    len: usize,
    melds: &[Meld],
) -> impl Iterator<Item = Tile> {
    let counts = if melds.len() <= 4 && len == 13 - melds.len() * 3 {
        concealed_counts(hand)
    } else {
        None
    };
    let mut owned = counts.unwrap_or_default();
    let totals = suit_totals(&owned);
    for &tile in melds.iter().flat_map(|m| &m.tiles) {
        if let Some(n) = owned.get_mut(tile as usize) {
            *n = n.saturating_add(1);
        }
    }
    let draw_pairs: [Option<usize>; 4] = std::array::from_fn(|group| {
        let mut after = totals;
        after[group] += 1;
        pair_group(after)
    });
    (0..31).filter(move |&tile| {
        let Some(mut c) = counts else {
            return false;
        };
        let Some(pair) = draw_pairs[tile as usize / 9] else {
            return false;
        };
        if owned[tile as usize] >= 4 {
            return false;
        }
        c[tile as usize] += 1;
        visit_decompositions(c, pair, |_| true)
    })
}

pub fn waits(hand: &[Tile], melds: &[Meld]) -> Vec<Tile> {
    winning_draws(hand.iter().copied(), hand.len(), melds).collect()
}

pub(super) fn can_knock(hand: &[Tile], melds: &[Meld], discard: Tile) -> bool {
    let Some(i) = hand.iter().position(|&t| t == discard) else {
        return false;
    };
    let rest = hand[..i].iter().chain(&hand[i + 1..]).copied();
    winning_draws(rest, hand.len() - 1, melds).next().is_some()
}
pub fn preserves(p: &Seat, hand: &[Tile], melds: &[Meld]) -> bool {
    !p.ready || waits(hand, melds) == p.waits
}
pub fn chow_forbidden(tile: Tile, needed: &[Tile]) -> Vec<Tile> {
    let mut out: Vec<_> = (0..27)
        .filter(|&t| {
            let mut seq = needed.to_vec();
            seq.push(t);
            seq.sort_unstable();
            seq[0] / 9 == seq[2] / 9 && seq[1] == seq[0] + 1 && seq[2] == seq[1] + 1
        })
        .collect();
    if !out.contains(&tile) {
        out.push(tile);
    }
    out
}
pub fn score(p: &Seat, seat: usize, hand: &[Tile], replacement: bool, rob: bool) -> Score {
    let all: Vec<_> = hand
        .iter()
        .copied()
        .chain(p.melds.iter().flat_map(|m| m.tiles.iter().copied()))
        .collect();
    let suits: std::collections::BTreeSet<_> =
        all.iter().filter(|&&t| t < 27).map(|t| t / 9).collect();
    decompositions(hand, p.melds.len())
        .into_iter()
        .map(|triplets| {
            let flowers = p.flowers.len()
                + triplets.iter().filter(|&&t| t >= 27).count()
                + p.melds
                    .iter()
                    .map(|m| {
                        if m.kind == Kind::Chow {
                            0
                        } else {
                            usize::from(m.tiles[0] >= 27)
                                + if m.kind == Kind::Concealed {
                                    2
                                } else {
                                    usize::from(m.kind == Kind::Kong)
                                }
                        }
                    })
                    .sum::<usize>();
            let mut fan = 0;
            let mut patterns = vec![];
            let mut add = |name: &str, n| {
                patterns.push(name.to_owned());
                fan += n;
            };
            if p.melds.iter().all(|m| m.kind == Kind::Concealed) {
                add("concealed", 1);
            }
            if triplets.len() + p.melds.len() == 4 && p.melds.iter().all(|m| m.kind != Kind::Chow) {
                add("triplets", 1);
            }
            if suits.len() == 1 {
                if all.iter().any(|&t| t >= 27) {
                    add("mixed", 1);
                } else {
                    add("pure", 2);
                }
            }
            if p.melds.len() == 4 {
                add("single", 1);
            }
            if replacement {
                add("replacement", 1);
            }
            if flowers == 0 {
                patterns.push("flowerless".into());
            }
            if rob {
                patterns.push("rob".into());
            }
            let points = ((if flowers == 0 { 10 } else { 1 + flowers as i64 })
                * (1i64 << fan)
                * if rob { 3 } else { 1 })
            .min(128);
            Score {
                seat,
                flowers,
                fan,
                patterns,
                points,
            }
        })
        .max_by_key(|s| s.points)
        .expect("validated winning hand")
}
