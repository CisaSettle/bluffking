//! Guiyang table profile: 108 tiles, no chi, no wilds, additive points.
//! References: tcy365.com/news/d24903.html and m.tcy365.com/news/d1558.html.
//! Regional chicken values differ: this table uses charging=3, liability=2,
//! concealed/added/exposed beans=2/3/1, and documents that profile in the UI.
pub use crate::mahjong::{Meld, MeldKind};
use serde::{Deserialize, Serialize};
pub type Tile = u8; // dots 0..8, bamboo 9..17, characters 18..26
pub const TYPES: usize = 27;
pub const CHICKEN: Tile = 9;
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Score {
    pub pattern: String,
    pub points: i64,
}
fn counts(tiles: &[Tile], melds: &[Meld]) -> Option<[u8; TYPES]> {
    if melds.len() > 4 {
        return None;
    }
    let mut c = [0u8; TYPES];
    for &t in tiles {
        let n = c.get_mut(t as usize)?;
        *n = n.checked_add(1)?;
    }
    let mut physical = c;
    for m in melds {
        if m.from >= 4 {
            return None;
        }
        let n = physical.get_mut(m.tile as usize)?;
        *n = n.checked_add(if m.kind == MeldKind::Pung { 3 } else { 4 })?;
    }
    physical.iter().all(|&n| n <= 4).then_some(c)
}
fn sets(c: &mut [u8; TYPES], left: usize, triplets_only: bool) -> bool {
    let Some(i) = c.iter().position(|&n| n > 0) else {
        return left == 0;
    };
    if left == 0 {
        return false;
    }
    if c[i] >= 3 {
        c[i] -= 3;
        let yes = sets(c, left - 1, triplets_only);
        c[i] += 3;
        if yes {
            return true;
        }
    }
    if !triplets_only && i % 9 <= 6 && c[i + 1] > 0 && c[i + 2] > 0 {
        c[i] -= 1;
        c[i + 1] -= 1;
        c[i + 2] -= 1;
        let yes = sets(c, left - 1, false);
        c[i] += 1;
        c[i + 1] += 1;
        c[i + 2] += 1;
        if yes {
            return true;
        }
    }
    false
}
pub fn score(tiles: &[Tile], melds: &[Meld]) -> Option<Score> {
    let c = counts(tiles, melds)?;
    if tiles.len() != 14 - 3 * melds.len() {
        return None;
    }
    let pure = tiles
        .iter()
        .chain(melds.iter().map(|m| &m.tile))
        .all(|t| t / 9 == tiles[0] / 9);
    let seven = melds.is_empty() && c.iter().all(|n| n % 2 == 0);
    let dragon = seven && c.contains(&4);
    let mut ordinary = false;
    let mut triplets = false;
    for pair in 0..TYPES {
        if c[pair] >= 2 {
            let mut rest = c;
            rest[pair] -= 2;
            ordinary |= sets(&mut rest, 4 - melds.len(), false);
            triplets |= sets(&mut rest, 4 - melds.len(), true);
        }
    }
    let (pattern, points) = if dragon {
        if pure {
            ("pure_dragon_pairs", 20)
        } else {
            ("dragon_pairs", 10)
        }
    } else if seven {
        if pure {
            ("pure_seven_pairs", 17)
        } else {
            ("seven_pairs", 7)
        }
    } else if triplets {
        if pure {
            ("pure_triplets", 15)
        } else {
            ("all_triplets", 5)
        }
    } else if ordinary {
        if pure {
            ("pure_suit", 10)
        } else {
            ("plain", 1)
        }
    } else {
        return None;
    };
    Some(Score {
        pattern: pattern.into(),
        points,
    })
}
pub fn waits(tiles: &[Tile], melds: &[Meld]) -> Vec<(Tile, Score)> {
    if melds.len() > 4 || tiles.len() != 13 - 3 * melds.len() {
        return vec![];
    }
    (0..TYPES as u8)
        .filter_map(|tile| {
            let mut h = tiles.to_vec();
            h.push(tile);
            score(&h, melds).map(|s| (tile, s))
        })
        .collect()
}
pub fn next_chicken(tile: Tile) -> Tile {
    tile / 9 * 9 + (tile % 9 + 1) % 9
}
/// Deterministic bot: prefer a real tenpai discard, otherwise preserve pairs and
/// close suited tiles. No opponent concealed tiles or future wall are consulted.
pub fn choose_discard(hand: &[Tile], melds: &[Meld]) -> Tile {
    let mut unique = hand.to_vec();
    unique.sort_unstable();
    unique.dedup();
    unique
        .into_iter()
        .max_by_key(|&t| {
            let mut rest = hand.to_vec();
            rest.remove(rest.iter().position(|&x| x == t).unwrap());
            let wait = waits(&rest, melds).len() as i32;
            let links = rest
                .iter()
                .map(|&x| {
                    rest.iter()
                        .map(|&y| {
                            if x == y {
                                4
                            } else if x / 9 == y / 9 && x.abs_diff(y) <= 2 {
                                1
                            } else {
                                0
                            }
                        })
                        .sum::<i32>()
                })
                .sum::<i32>();
            wait * 1000 + links
        })
        .unwrap_or(0)
}
