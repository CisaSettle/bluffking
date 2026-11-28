//! Tiles: 0..8 dots, 9..17 bamboo, 18..20 red/green/white dragons (four each).
//! Xiangyang profile: ordinary 8x cap, double dragon pairs 16x. Liability for
//! exposed listening hands is applied by the game, after the hand-type cap.
//! Rule references: https://www.tcy365.com/news/d1864.html (Xiangyang cap),
//! https://static.laiyouxi.com/games/11.html (melds, hand types and liability).
use serde::{Deserialize, Serialize};

pub type Tile = u8;
pub const TYPES: usize = 21;

/// Immutable room rules.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct RuleOptions {
    pub horse: bool,
    pub piao: u8,
    pub wild: bool,
    pub kong_chain: bool,
}
impl Default for RuleOptions {
    fn default() -> Self {
        Self {
            horse: false,
            piao: 0,
            wild: false,
            kong_chain: true,
        }
    }
}
impl RuleOptions {
    pub fn valid(self) -> bool {
        matches!(self.piao, 0 | 1 | 2 | 5)
    }
}

/// The shared tile-game melds ([`crate::mahjong`]).
pub use crate::mahjong::{Meld, MeldKind};

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct Score {
    pub patterns: Vec<String>,
    pub raw: u32,
    pub multiplier: u32,
}

#[derive(Clone, Copy, Default)]
pub struct WinContext {
    pub tile: Tile,
    pub kong_draw: bool,
    pub kong_discard: bool,
    pub rob_kong: bool,
    pub last_tile: bool,
}

struct TileCounts {
    concealed: [u8; TYPES],
    physical: [u8; TYPES],
}

fn counts(tiles: impl Iterator<Item = Tile>, melds: &[Meld]) -> Option<TileCounts> {
    let mut concealed = [0; TYPES];
    for tile in tiles {
        let n = concealed.get_mut(tile as usize)?;
        *n += 1;
        if *n > 4 {
            return None;
        }
    }
    let mut physical = concealed;
    for m in melds {
        let n = physical.get_mut(m.tile as usize)?;
        *n += if m.kind == MeldKind::Pung { 3 } else { 4 };
        if *n > 4 {
            return None;
        }
    }
    Some(TileCounts {
        concealed,
        physical,
    })
}

// Enumerating actual decompositions prevents counting an incompatible middle
// wait together with seven pairs, or with an all-triplet interpretation.
fn sets(
    c: &mut [u8; TYPES],
    left: usize,
    triplets: &mut Vec<Tile>,
    mids: &mut Vec<Tile>,
    out: &mut Vec<(Vec<Tile>, Vec<Tile>)>,
) {
    let Some(i) = c.iter().position(|&n| n > 0) else {
        if left == 0 {
            out.push((triplets.clone(), mids.clone()));
        }
        return;
    };
    if left == 0 {
        return;
    }
    if c[i] >= 3 {
        c[i] -= 3;
        triplets.push(i as Tile);
        sets(c, left - 1, triplets, mids, out);
        triplets.pop();
        c[i] += 3;
    }
    if i < 18 && i % 9 <= 6 && c[i + 1] > 0 && c[i + 2] > 0 {
        c[i] -= 1;
        c[i + 1] -= 1;
        c[i + 2] -= 1;
        mids.push((i + 1) as Tile);
        sets(c, left - 1, triplets, mids, out);
        mids.pop();
        c[i] += 1;
        c[i + 1] += 1;
        c[i + 2] += 1;
    }
}

fn add(s: &mut Score, name: &str, n: u32) {
    s.patterns.push(name.to_owned());
    s.raw = s.raw.saturating_mul(n);
}

pub fn score(tiles: &[Tile], melds: &[Meld], ctx: WinContext) -> Option<Score> {
    if melds.len() > 4 || tiles.len() != 14 - melds.len() * 3 || !tiles.contains(&ctx.tile) {
        return None;
    }
    let c = counts(tiles.iter().copied(), melds)?.concealed;
    score_counts(tiles, melds, ctx, c, c)
}

fn score_counts(
    tiles: &[Tile],
    melds: &[Meld],
    ctx: WinContext,
    c: [u8; TYPES],
    physical: [u8; TYPES],
) -> Option<Score> {
    let mut candidates = Vec::new();
    if melds.is_empty() && c.iter().all(|n| n % 2 == 0) {
        let quads = physical.iter().filter(|&&n| n == 4).count();
        let mut s = Score {
            raw: 1,
            ..Score::default()
        };
        add(
            &mut s,
            match quads {
                0 => "seven_pairs",
                1 => "dragon_pairs",
                _ => "double_dragon_pairs",
            },
            match quads {
                0 => 4,
                1 => 8,
                _ => 16,
            },
        );
        candidates.push(s);
    }
    for pair in 0..TYPES {
        if c[pair] < 2 {
            continue;
        }
        let mut rest = c;
        rest[pair] -= 2;
        let mut decompositions = Vec::new();
        sets(
            &mut rest,
            4 - melds.len(),
            &mut Vec::new(),
            &mut Vec::new(),
            &mut decompositions,
        );
        for (mut trips, mids) in decompositions {
            trips.extend(melds.iter().map(|m| m.tile));
            let mut s = Score {
                raw: 1,
                ..Score::default()
            };
            if mids.is_empty() {
                add(&mut s, "all_triplets", 2);
            }
            let dragons = trips.iter().filter(|&&t| t >= 18).count();
            if dragons == 3 {
                add(&mut s, "big_dragons", 8);
            } else if dragons == 2 && pair >= 18 {
                add(&mut s, "small_dragons", 4);
            }
            if melds.len() == 4 {
                add(&mut s, "single_wait", 4);
            }
            if (ctx.tile == 4 || ctx.tile == 13) && mids.contains(&ctx.tile) {
                add(&mut s, "five_wait", 2);
            }
            for (tile, &n) in physical.iter().enumerate() {
                if n == 4 {
                    add(&mut s, "concealed_four", 4);
                }
                if n > 0
                    && melds
                        .iter()
                        .any(|m| m.kind == MeldKind::Pung && m.tile as usize == tile)
                {
                    add(&mut s, "exposed_four", 2);
                }
            }
            candidates.push(s);
        }
    }
    for s in &mut candidates {
        let mut all = tiles.to_vec();
        all.extend(melds.iter().map(|m| m.tile));
        if all.iter().all(|&t| t < 9) || all.iter().all(|&t| (9..18).contains(&t)) {
            add(s, "pure_suit", 4);
        }
        if ctx.kong_draw {
            add(s, "kong_draw", 2);
        }
        if ctx.kong_discard {
            add(s, "kong_discard", 2);
        }
        if ctx.rob_kong {
            add(s, "rob_kong", 2);
        }
        if ctx.last_tile {
            add(s, "last_tile", 2);
        }
        if s.patterns.is_empty() {
            s.patterns.push("plain".to_owned());
        }
        s.multiplier = s
            .raw
            .min(if s.patterns.iter().any(|p| p == "double_dragon_pairs") {
                16
            } else {
                8
            });
    }
    candidates.into_iter().max_by_key(|s| (s.multiplier, s.raw))
}

/// Red dragons substitute only inside the concealed winning hand. Physical
/// four-copy validation precedes substitution; invented quads never earn bonuses.
pub fn score_with_options(
    tiles: &[Tile],
    melds: &[Meld],
    ctx: WinContext,
    wild: bool,
) -> Option<Score> {
    if !wild || !tiles.contains(&18) {
        return score(tiles, melds, ctx);
    }
    if melds.len() > 4 || tiles.len() != 14 - melds.len() * 3 || !tiles.contains(&ctx.tile) {
        return None;
    }
    let physical = counts(tiles.iter().copied(), melds)?.concealed;
    if !wild_win(physical, melds.len()) {
        return None;
    }
    let mut c = physical;
    let jokers = c[18];
    c[18] = 0;
    let mut best = score(tiles, melds, ctx);
    let mut physical = physical;
    physical[18] = 0;
    fn assign(
        c: &mut [u8; TYPES],
        left: u8,
        start: usize,
        melds: &[Meld],
        ctx: WinContext,
        physical: [u8; TYPES],
        best: &mut Option<Score>,
    ) {
        if left == 0 {
            let tiles: Vec<Tile> = c
                .iter()
                .enumerate()
                .flat_map(|(i, &n)| std::iter::repeat_n(i as Tile, n as usize))
                .collect();
            if let Some(mut s) = score_counts(&tiles, melds, ctx, *c, physical) {
                s.patterns.push("wild".into());
                if best
                    .as_ref()
                    .is_none_or(|b| (s.multiplier, s.raw) > (b.multiplier, b.raw))
                {
                    *best = Some(s);
                }
            }
            return;
        }
        for i in start..TYPES {
            c[i] += 1;
            assign(c, left - 1, i, melds, ctx, physical, best);
            c[i] -= 1;
        }
    }
    assign(&mut c, jokers, 0, melds, ctx, physical, &mut best);
    best
}

fn wild_sets(c: &mut [u8; TYPES], wild: u8, left: usize) -> bool {
    let Some(i) = c.iter().position(|&n| n > 0) else {
        return wild as usize == left * 3;
    };
    if left == 0 {
        return false;
    }
    let n = c[i].min(3);
    if 3 - n <= wild {
        c[i] -= n;
        let found = wild_sets(c, wild - (3 - n), left - 1);
        c[i] += n;
        if found {
            return true;
        }
    }
    if i < 18 {
        for start in i.saturating_sub(2).max(i / 9 * 9)..=i.min(i / 9 * 9 + 6) {
            let used: [u8; 3] = std::array::from_fn(|j| u8::from(c[start + j] > 0));
            let missing = 3 - used.iter().sum::<u8>();
            if missing > wild {
                continue;
            }
            for j in 0..3 {
                c[start + j] -= used[j];
            }
            let found = wild_sets(c, wild - missing, left - 1);
            for j in 0..3 {
                c[start + j] += used[j];
            }
            if found {
                return true;
            }
        }
    }
    false
}
fn wild_win(mut c: [u8; TYPES], melds: usize) -> bool {
    let wild = c[18];
    c[18] = 0;
    if melds == 0 && c.iter().filter(|&&n| n % 2 != 0).count() <= wild as usize {
        return true;
    }
    for pair in 0..TYPES {
        let n = c[pair].min(2);
        if 2 - n > wild {
            continue;
        }
        c[pair] -= n;
        let found = wild_sets(&mut c, wild - (2 - n), 4 - melds);
        c[pair] += n;
        if found {
            return true;
        }
    }
    false
}
fn winning_draws(counts: &TileCounts, melds: usize, wild: bool) -> impl Iterator<Item = Tile> + '_ {
    (0..TYPES as Tile).filter(move |&tile| {
        let i = tile as usize;
        if counts.physical[i] >= 4 {
            return false;
        }
        let mut c = counts.concealed;
        c[i] += 1;
        if wild {
            return wild_win(c, melds);
        }
        if melds == 0 && c.iter().all(|n| n % 2 == 0) {
            return true;
        }
        (0..TYPES).any(|pair| {
            if c[pair] < 2 {
                return false;
            }
            let mut rest = c;
            rest[pair] -= 2;
            has_sets(&mut rest, 4 - melds)
        })
    })
}

fn waiting_counts(tiles: &[Tile], melds: &[Meld]) -> Option<TileCounts> {
    if melds.len() > 4 || tiles.len() != 13 - melds.len() * 3 {
        return None;
    }
    counts(tiles.iter().copied(), melds)
}

pub fn waits_with_options(tiles: &[Tile], melds: &[Meld], wild: bool) -> Vec<Tile> {
    waiting_counts(tiles, melds)
        .map(|c| winning_draws(&c, melds.len(), wild).collect())
        .unwrap_or_default()
}

pub fn waits(tiles: &[Tile], melds: &[Meld]) -> Vec<Tile> {
    waits_with_options(tiles, melds, false)
}

/// A ready declaration needs one winning draw after this discard.
pub(super) fn can_declare_ready(tiles: &[Tile], melds: &[Meld], wild: bool, discard: Tile) -> bool {
    if melds.len() > 4 || tiles.len() != 14 - melds.len() * 3 {
        return false;
    }
    let Some(i) = tiles.iter().position(|&t| t == discard) else {
        return false;
    };
    let rest = tiles[..i].iter().chain(&tiles[i + 1..]).copied();
    counts(rest, melds).is_some_and(|c| winning_draws(&c, melds.len(), wild).next().is_some())
}

/// Smallest listening fragment after hiding complete groups. Every candidate
/// must retain the entire original wait set, including physical-copy limits.
pub fn listening_tiles(tiles: &[Tile], melds: &[Meld], wild: bool) -> Vec<Tile> {
    let Some(counts) = waiting_counts(tiles, melds) else {
        return vec![];
    };
    let target: Vec<_> = winning_draws(&counts, melds.len(), wild).collect();
    if target.is_empty() {
        return vec![];
    }
    let mut best = tiles.to_vec();
    best.sort_unstable();
    let available: Vec<_> = (0..TYPES as Tile)
        .filter(|&t| counts.physical[t as usize] < 4)
        .collect();
    // Recursive search state is passed positionally to avoid a per-call struct.
    #[allow(clippy::too_many_arguments)]
    fn visit(
        c: [u8; TYPES],
        groups: usize,
        eye: bool,
        wild: bool,
        available: &[Tile],
        target: &[Tile],
        best: &mut Vec<Tile>,
        seen: &mut std::collections::HashSet<([u8; TYPES], bool)>,
    ) {
        if !seen.insert((c, eye)) {
            return;
        }
        let waits: Vec<_> = available
            .iter()
            .copied()
            .filter(|&t| {
                let mut full = c;
                full[t as usize] += 1;
                if eye {
                    if wild {
                        let jokers = full[18];
                        full[18] = 0;
                        return wild_sets(&mut full, jokers, 4 - groups);
                    }
                    return has_sets(&mut full, 4 - groups);
                }
                if wild {
                    return wild_win(full, groups);
                }
                (0..TYPES).any(|pair| {
                    if full[pair] < 2 {
                        return false;
                    }
                    let mut rest = full;
                    rest[pair] -= 2;
                    has_sets(&mut rest, 4 - groups)
                })
            })
            .collect();
        if waits != target {
            return;
        }
        let rest: Vec<_> = c
            .iter()
            .enumerate()
            .flat_map(|(t, &n)| std::iter::repeat_n(t as Tile, n as usize))
            .collect();
        if rest.len() < best.len() {
            *best = rest;
        }
        if groups == 4 {
            return;
        }
        for t in 0..TYPES {
            if wild && t == 18 {
                continue;
            }
            if !eye && c[t] >= 2 {
                let mut next = c;
                next[t] -= 2;
                visit(next, groups, true, wild, available, target, best, seen);
            }
            if c[t] >= 3 {
                let mut next = c;
                next[t] -= 3;
                visit(next, groups + 1, eye, wild, available, target, best, seen);
            }
            if t < 18 && t % 9 < 7 && c[t] > 0 && c[t + 1] > 0 && c[t + 2] > 0 {
                let mut next = c;
                for n in &mut next[t..=t + 2] {
                    *n -= 1;
                }
                visit(next, groups + 1, eye, wild, available, target, best, seen);
            }
        }
    }
    visit(
        counts.concealed,
        melds.len(),
        false,
        wild,
        &available,
        &target,
        &mut best,
        &mut Default::default(),
    );
    // Seven pairs has no complete triples: hide its completed pairs only
    // when the remaining singleton explains every wait.
    if !wild && melds.is_empty() {
        let odd: Vec<_> = counts
            .concealed
            .iter()
            .enumerate()
            .filter(|(_, n)| **n % 2 == 1)
            .map(|(t, _)| t as Tile)
            .collect();
        if odd.len() == 1 && target == odd && odd.len() < best.len() {
            best = odd;
        }
    }
    best
}

fn has_sets(c: &mut [u8; TYPES], left: usize) -> bool {
    let Some(i) = c.iter().position(|&n| n > 0) else {
        return left == 0;
    };
    if left == 0 {
        return false;
    }
    if c[i] >= 3 {
        c[i] -= 3;
        let found = has_sets(c, left - 1);
        c[i] += 3;
        if found {
            return true;
        }
    }
    if i < 18 && i % 9 <= 6 && c[i + 1] > 0 && c[i + 2] > 0 {
        c[i] -= 1;
        c[i + 1] -= 1;
        c[i + 2] -= 1;
        let found = has_sets(c, left - 1);
        c[i] += 1;
        c[i + 1] += 1;
        c[i + 2] += 1;
        return found;
    }
    false
}

pub fn choose_discard_with_options(tiles: &[Tile], melds: &[Meld], wild: bool) -> Tile {
    let mut best = (i32::MIN, tiles.first().copied().unwrap_or(0));
    for (i, &tile) in tiles.iter().enumerate() {
        let mut rest = tiles.to_vec();
        rest.remove(i);
        let w = waits_with_options(&rest, melds, wild).len() as i32;
        let cohesion: i32 = rest
            .iter()
            .map(|&t| {
                rest.iter()
                    .filter(|&&u| {
                        u == t || (u < 18 && t < 18 && u / 9 == t / 9 && u.abs_diff(t) <= 2)
                    })
                    .count() as i32
            })
            .sum();
        let value = w * 1000
            + cohesion
            + if wild {
                rest.iter().filter(|&&t| t == 18).count() as i32 * 100
            } else {
                0
            };
        if value > best.0 {
            best = (value, tile);
        }
    }
    best.1
}
