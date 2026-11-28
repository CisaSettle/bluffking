//! Fixed Chengdu blood-battle profile: three suits, missing suit, 16x cap.
pub use crate::mahjong::{Meld, MeldKind};
use serde::{Deserialize, Serialize};
pub type Tile = u8;
pub const TYPES: usize = 27;
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Score {
    pub multiplier: u32,
    pub patterns: Vec<String>,
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
        let ok = sets(c, left - 1, triplets_only);
        c[i] += 3;
        if ok {
            return true;
        }
    }
    if !triplets_only && i % 9 <= 6 && c[i + 1] > 0 && c[i + 2] > 0 {
        c[i] -= 1;
        c[i + 1] -= 1;
        c[i + 2] -= 1;
        let ok = sets(c, left - 1, false);
        c[i] += 1;
        c[i + 1] += 1;
        c[i + 2] += 1;
        if ok {
            return true;
        }
    }
    false
}
pub fn score(tiles: &[Tile], melds: &[Meld], missing: u8) -> Option<Score> {
    if missing > 2 || melds.len() > 4 || tiles.len() != 14 - 3 * melds.len() {
        return None;
    }
    let mut c = [0u8; TYPES];
    for &t in tiles {
        if t >= 27 || t / 9 == missing {
            return None;
        }
        c[t as usize] += 1;
    }
    let mut physical = c;
    for m in melds {
        if m.tile >= 27 || m.tile / 9 == missing || m.from >= 4 {
            return None;
        }
        physical[m.tile as usize] += if m.kind == MeldKind::Pung { 3 } else { 4 };
    }
    if physical.iter().any(|&n| n > 4) {
        return None;
    }
    let pairs = melds.is_empty() && c.iter().all(|n| n % 2 == 0);
    let mut ordinary = false;
    let mut triplets = false;
    for i in 0..TYPES {
        if c[i] >= 2 {
            c[i] -= 2;
            ordinary |= sets(&mut c, 4 - melds.len(), false);
            triplets |= sets(&mut c, 4 - melds.len(), true);
            c[i] += 2;
        }
    }
    if !pairs && !ordinary {
        return None;
    }
    let mut fan = 0;
    let mut patterns = vec![];
    if pairs {
        fan += 2;
        patterns.push("seven_pairs".into());
    } else if triplets {
        fan += 1;
        patterns.push("all_triplets".into());
    } else {
        patterns.push("plain".into());
    }
    let suits = (0..3)
        .filter(|&s| physical[s * 9..s * 9 + 9].iter().any(|&n| n > 0))
        .count();
    if suits == 1 {
        fan += 2;
        patterns.push("pure_suit".into());
    }
    let roots = physical.iter().filter(|&&n| n == 4).count();
    if roots > 0 {
        fan += roots;
        patterns.push("roots".into());
    }
    Some(Score {
        multiplier: 1 << fan.min(4),
        patterns,
    })
}
pub fn waits(tiles: &[Tile], melds: &[Meld], missing: u8) -> Vec<(Tile, Score)> {
    (0..27)
        .filter_map(|t| {
            let mut h = tiles.to_vec();
            h.push(t);
            score(&h, melds, missing).map(|s| (t, s))
        })
        .collect()
}
pub fn discard(tiles: &[Tile], candidates: &[Tile]) -> Tile {
    *candidates
        .iter()
        .min_by_key(|&&t| {
            let same = tiles.iter().filter(|&&v| v == t).count() as i32;
            let nearby = tiles
                .iter()
                .filter(|&&v| v / 9 == t / 9 && v != t && v.abs_diff(t) <= 2)
                .count() as i32;
            same * 5 + nearby * 2
        })
        .expect("legal discard candidates")
}
