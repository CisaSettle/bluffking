//! Tickets a station sells and what they win: entry normalisation, 注数 and
//! the closed-form prize of a 复式 ticket. Unit prices and payout tables are
//! the client's current constants (`client-game/src/views/games/*/`). These
//! are fixed practice gold-bean prizes, not real-money lottery pool prizes.
use super::Game;
use rand::rngs::OsRng;
use serde_json::{json, Value};

pub const MAX_STAKES: i64 = 100;
pub const MAX_MULTIPLIER: i64 = 10;
pub const MAX_TICKETS_PER_PURCHASE: usize = 10;
pub const MAX_TICKETS_PER_PERIOD: i64 = 50;
pub const MAX_STAKES_PER_PERIOD: i64 = 1_000;

pub const SSQ_RED_MAX: u8 = 33;
pub const SSQ_BLUE_MAX: u8 = 16;
pub const DLT_FRONT_MAX: u8 = 35;
pub const DLT_BACK_MAX: u8 = 12;
pub const TOTO_MAX: u8 = 49;
pub const HAPPY8_MAX: u8 = 80;
pub const HAPPY8_DRAW: usize = 20;

/// Chips per winning 注 by prize group (`PAYOUTS × BET 10`).
pub const SSQ_PRIZE: [i64; 7] = [0, 10_000_000, 1_000_000, 15_000, 1_000, 50, 25];
/// Seven prize tiers under the 2026 DLT matching rules, in practice beans.
pub const DLT_PRIZE: [i64; 8] = [0, 10_000_000, 1_000_000, 25_000, 1_500, 750, 75, 25];
pub const TOTO_PRIZE: [i64; 8] = [0, 10_000_000, 1_000_000, 10_000, 3_000, 500, 250, 100];
pub const FC3D_STRAIGHT: i64 = 1_040;
pub const FC3D_GROUP3: i64 = 346;
pub const FC3D_GROUP6: i64 = 173;

/// Happy8 chips per 注 (stake included) by picks × hits.
pub fn happy8_prize(picks: usize, hits: usize) -> i64 {
    match (picks, hits) {
        (1, 1) => 9,
        (2, 2) => 38,
        (3, 2) => 6,
        (3, 3) => 104,
        (4, 2) => 6,
        (4, 3) => 10,
        (4, 4) => 186,
        (5, 3) => 6,
        (5, 4) => 40,
        (5, 5) => 2_000,
        (6, 3) => 6,
        (6, 4) => 20,
        (6, 5) => 60,
        (6, 6) => 5_760,
        (7, 0) => 4,
        (7, 4) => 8,
        (7, 5) => 60,
        (7, 6) => 600,
        (7, 7) => 17_000,
        (8, 0) => 4,
        (8, 4) => 6,
        (8, 5) => 20,
        (8, 6) => 160,
        (8, 7) => 1_600,
        (8, 8) => 100_000,
        (9, 0) => 4,
        (9, 4) => 6,
        (9, 5) => 10,
        (9, 6) => 44,
        (9, 7) => 450,
        (9, 8) => 4_000,
        (9, 9) => 500_000,
        (10, 0) => 4,
        (10, 5) => 6,
        (10, 6) => 10,
        (10, 7) => 160,
        (10, 8) => 1_440,
        (10, 9) => 16_000,
        (10, 10) => 10_000_000,
        _ => 0,
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Entry {
    /// 6–12 red of 1–33 × 1–16 blue of 1–16.
    Ssq { red: Vec<u8>, blue: Vec<u8> },
    /// 5–8 front of 1–35 × 2–12 back of 1–12, at most 100 single stakes.
    Dlt { front: Vec<u8>, back: Vec<u8> },
    /// 6–9 of 1–49 (system 7/8/9).
    Toto { numbers: Vec<u8> },
    /// 1–10 of 1–80.
    Happy8 { numbers: Vec<u8> },
    /// 直选: a digit set per position.
    Straight([Vec<u8>; 3]),
    /// 组三: every ordered pair (x, y) of the picks is one 注 x x y.
    Group3(Vec<u8>),
    /// 组六: every triple of the picks.
    Group6(Vec<u8>),
}

#[derive(Debug)]
pub struct Invalid;

fn numbers(
    value: &Value,
    min_len: usize,
    max_len: usize,
    lo: u8,
    hi: u8,
) -> Result<Vec<u8>, Invalid> {
    let items = value.as_array().ok_or(Invalid)?;
    if items.len() > 64 {
        return Err(Invalid);
    }
    let mut out = Vec::with_capacity(items.len());
    for item in items {
        let n = item.as_u64().ok_or(Invalid)?;
        if n < u64::from(lo) || n > u64::from(hi) {
            return Err(Invalid);
        }
        out.push(n as u8);
    }
    out.sort_unstable();
    out.dedup();
    if out.len() < min_len || out.len() > max_len {
        return Err(Invalid);
    }
    Ok(out)
}

/// DLT selections must be distinct; legacy games keep `numbers` deduplication.
fn distinct_numbers(
    value: &Value,
    min_len: usize,
    max_len: usize,
    lo: u8,
    hi: u8,
) -> Result<Vec<u8>, Invalid> {
    let submitted_len = value.as_array().ok_or(Invalid)?.len();
    let out = numbers(value, min_len, max_len, lo, hi)?;
    if out.len() != submitted_len {
        return Err(Invalid);
    }
    Ok(out)
}

fn object_keys(value: &Value, allowed: &[&str]) -> Result<(), Invalid> {
    let object = value.as_object().ok_or(Invalid)?;
    if object.keys().any(|k| !allowed.contains(&k.as_str())) {
        return Err(Invalid);
    }
    Ok(())
}

impl Entry {
    /// Sorts one submitted entry; DLT rejects duplicates, legacy games deduplicate.
    pub fn parse(game: Game, value: &Value) -> Result<Entry, Invalid> {
        let entry = match game {
            Game::Ssq => {
                object_keys(value, &["red", "blue"])?;
                Entry::Ssq {
                    red: numbers(&value["red"], 6, 12, 1, SSQ_RED_MAX)?,
                    blue: numbers(&value["blue"], 1, 16, 1, SSQ_BLUE_MAX)?,
                }
            }
            Game::Dlt => {
                object_keys(value, &["front", "back"])?;
                Entry::Dlt {
                    front: distinct_numbers(&value["front"], 5, 8, 1, DLT_FRONT_MAX)?,
                    back: distinct_numbers(&value["back"], 2, 12, 1, DLT_BACK_MAX)?,
                }
            }
            Game::Toto => {
                object_keys(value, &["numbers"])?;
                Entry::Toto {
                    numbers: numbers(&value["numbers"], 6, 9, 1, TOTO_MAX)?,
                }
            }
            Game::Happy8 => {
                object_keys(value, &["numbers"])?;
                Entry::Happy8 {
                    numbers: numbers(&value["numbers"], 1, 10, 1, HAPPY8_MAX)?,
                }
            }
            Game::Fucai3d => {
                object_keys(value, &["mode", "digits"])?;
                match value["mode"].as_str().ok_or(Invalid)? {
                    "straight" => {
                        let sets = value["digits"].as_array().ok_or(Invalid)?;
                        if sets.len() != 3 {
                            return Err(Invalid);
                        }
                        Entry::Straight([
                            numbers(&sets[0], 1, 10, 0, 9)?,
                            numbers(&sets[1], 1, 10, 0, 9)?,
                            numbers(&sets[2], 1, 10, 0, 9)?,
                        ])
                    }
                    "group3" => Entry::Group3(numbers(&value["digits"], 2, 10, 0, 9)?),
                    "group6" => Entry::Group6(numbers(&value["digits"], 3, 9, 0, 9)?),
                    _ => return Err(Invalid),
                }
            }
            Game::Scratch => return Err(Invalid),
        };
        if entry.stakes() > MAX_STAKES {
            return Err(Invalid);
        }
        Ok(entry)
    }

    /// Re-reads a stored entry (written by [`Entry::to_value`]).
    pub fn from_stored(game: Game, value: &Value) -> Option<Entry> {
        Entry::parse(game, value).ok()
    }

    pub fn to_value(&self) -> Value {
        match self {
            Entry::Ssq { red, blue } => json!({ "red": red, "blue": blue }),
            Entry::Dlt { front, back } => json!({ "front": front, "back": back }),
            Entry::Toto { numbers } | Entry::Happy8 { numbers } => json!({ "numbers": numbers }),
            Entry::Straight(sets) => json!({ "mode": "straight", "digits": sets }),
            Entry::Group3(digits) => json!({ "mode": "group3", "digits": digits }),
            Entry::Group6(digits) => json!({ "mode": "group6", "digits": digits }),
        }
    }

    /// 注数 of the ticket.
    pub fn stakes(&self) -> i64 {
        match self {
            Entry::Ssq { red, blue } => choose(red.len(), 6) * blue.len() as i64,
            Entry::Dlt { front, back } => choose(front.len(), 5) * choose(back.len(), 2),
            Entry::Toto { numbers } => choose(numbers.len(), 6),
            Entry::Happy8 { .. } => 1,
            Entry::Straight(sets) => sets.iter().map(|s| s.len() as i64).product(),
            Entry::Group3(digits) => {
                let n = digits.len() as i64;
                n * (n - 1)
            }
            Entry::Group6(digits) => choose(digits.len(), 3),
        }
    }

    /// Prize in chips and tier for `multiplier` × this ticket against a draw.
    /// `None` when the stored draw does not fit the game (never for rows the
    /// draw loop wrote).
    pub fn prize(
        &self,
        multiplier: i64,
        numbers: &[i16],
        bonus: Option<i16>,
        bonus_numbers: &[i16],
    ) -> Option<(i64, i16)> {
        let (per_multiplier, tier) = match self {
            Entry::Ssq { red, blue } => {
                if numbers.len() != 6 {
                    return None;
                }
                let winning_blue = bonus?;
                let m = red
                    .iter()
                    .filter(|n| numbers.contains(&i16::from(**n)))
                    .count();
                let u = red.len() - m;
                let b = blue.len() as i64;
                let bh = i64::from(blue.contains(&(winning_blue as u8)));
                let mut total = 0i64;
                let mut tier = 0i16;
                for k in 0..=6usize {
                    let ways = choose(m, k).checked_mul(choose(u, 6 - k))?;
                    if ways == 0 {
                        continue;
                    }
                    let hit = ssq_group(k, true);
                    let miss = ssq_group(k, false);
                    if hit > 0 && bh > 0 {
                        tier = best_tier(tier, hit);
                        total = total.checked_add(
                            ways.checked_mul(bh.checked_mul(SSQ_PRIZE[hit as usize])?)?,
                        )?;
                    }
                    if miss > 0 && b - bh > 0 {
                        tier = best_tier(tier, miss);
                        total = total.checked_add(
                            ways.checked_mul((b - bh).checked_mul(SSQ_PRIZE[miss as usize])?)?,
                        )?;
                    }
                }
                (total, tier)
            }
            Entry::Dlt { front, back } => {
                if bonus.is_some()
                    || !valid_distinct_draw(numbers, 5, DLT_FRONT_MAX)
                    || !valid_distinct_draw(bonus_numbers, 2, DLT_BACK_MAX)
                    || !(1..=MAX_MULTIPLIER).contains(&multiplier)
                {
                    return None;
                }
                let front_hits = front
                    .iter()
                    .filter(|n| numbers.contains(&i16::from(**n)))
                    .count();
                let back_hits = back
                    .iter()
                    .filter(|n| bonus_numbers.contains(&i16::from(**n)))
                    .count();
                let mut total = 0i64;
                let mut tier = 0i16;
                // Count every 5+2 subset, including lower-tier wins on the
                // same compound ticket. Selecting more numbers never turns
                // the ticket into a single top-tier payout.
                for f in 0..=5usize {
                    let front_ways = choose(front_hits, f)
                        .checked_mul(choose(front.len() - front_hits, 5 - f))?;
                    if front_ways == 0 {
                        continue;
                    }
                    for b in 0..=2usize {
                        let ways = front_ways
                            .checked_mul(choose(back_hits, b))?
                            .checked_mul(choose(back.len() - back_hits, 2 - b))?;
                        let group = dlt_group(f, b);
                        if ways > 0 && group > 0 {
                            tier = best_tier(tier, group);
                            total =
                                total.checked_add(ways.checked_mul(DLT_PRIZE[group as usize])?)?;
                        }
                    }
                }
                (total, tier)
            }
            Entry::Toto { numbers: picks } => {
                if numbers.len() != 6 {
                    return None;
                }
                let additional = bonus?;
                let m = picks
                    .iter()
                    .filter(|n| numbers.contains(&i16::from(**n)))
                    .count();
                let a = usize::from(picks.contains(&(additional as u8)));
                let u = picks.len() - m - a;
                let mut total = 0i64;
                let mut tier = 0i16;
                for k in 0..=6usize {
                    for j in 0..=a {
                        if k + j > 6 {
                            continue;
                        }
                        let ways = choose(m, k)
                            .checked_mul(choose(a, j))?
                            .checked_mul(choose(u, 6 - k - j))?;
                        if ways == 0 {
                            continue;
                        }
                        let group = toto_group(k, j == 1);
                        if group > 0 {
                            tier = best_tier(tier, group);
                            total =
                                total.checked_add(ways.checked_mul(TOTO_PRIZE[group as usize])?)?;
                        }
                    }
                }
                (total, tier)
            }
            Entry::Happy8 { numbers: picks } => {
                if numbers.len() != HAPPY8_DRAW {
                    return None;
                }
                let hits = picks
                    .iter()
                    .filter(|n| numbers.contains(&i16::from(**n)))
                    .count();
                (happy8_prize(picks.len(), hits), hits as i16)
            }
            Entry::Straight(sets) => {
                if numbers.len() != 3 {
                    return None;
                }
                let won = sets
                    .iter()
                    .zip(numbers)
                    .all(|(set, digit)| set.contains(&(*digit as u8)));
                let prize = if won { FC3D_STRAIGHT } else { 0 };
                (prize, i16::from(won))
            }
            Entry::Group3(digits) => {
                if numbers.len() != 3 {
                    return None;
                }
                let (a, b, c) = (numbers[0], numbers[1], numbers[2]);
                let distinct = [a, b, c]
                    .iter()
                    .collect::<std::collections::BTreeSet<_>>()
                    .len();
                let won = distinct == 2 && numbers.iter().all(|d| digits.contains(&(*d as u8)));
                let prize = if won { FC3D_GROUP3 } else { 0 };
                (prize, i16::from(won))
            }
            Entry::Group6(digits) => {
                if numbers.len() != 3 {
                    return None;
                }
                let distinct = numbers
                    .iter()
                    .collect::<std::collections::BTreeSet<_>>()
                    .len();
                let won = distinct == 3 && numbers.iter().all(|d| digits.contains(&(*d as u8)));
                let prize = if won { FC3D_GROUP6 } else { 0 };
                (prize, i16::from(won))
            }
        };
        Some((per_multiplier.checked_mul(multiplier)?, tier))
    }
}

/// Lower non-zero group wins; 0 means no prize yet.
fn best_tier(current: i16, group: i16) -> i16 {
    if current == 0 || group < current {
        group
    } else {
        current
    }
}

fn valid_distinct_draw(numbers: &[i16], count: usize, max: u8) -> bool {
    numbers.len() == count
        && numbers
            .iter()
            .enumerate()
            .all(|(i, n)| (1..=i16::from(max)).contains(n) && !numbers[..i].contains(n))
}

/// 2026 seven-tier basic DLT rules for one 5+2 stake (no additional bet).
fn dlt_group(front: usize, back: usize) -> i16 {
    match (front, back) {
        (5, 2) => 1,
        (5, 1) => 2,
        (5, 0) | (4, 2) => 3,
        (4, 1) => 4,
        (4, 0) | (3, 2) => 5,
        (3, 1) | (2, 2) => 6,
        (3, 0) | (2, 1) | (1, 2) | (0, 2) => 7,
        _ => 0,
    }
}

/// Today's `prizeGroup` for a single 6+1 注 with `k` red matches.
fn ssq_group(k: usize, blue_hit: bool) -> i16 {
    match (k, blue_hit) {
        (6, true) => 1,
        (6, false) => 2,
        (5, true) => 3,
        (5, false) | (4, true) => 4,
        (4, false) | (3, true) => 5,
        (_, true) => 6,
        _ => 0,
    }
}

/// Today's `prizeGroup` for a single 6-number 注 with `k` matches.
fn toto_group(k: usize, additional: bool) -> i16 {
    match (k, additional) {
        (6, _) => 1,
        (5, true) => 2,
        (5, false) => 3,
        (4, true) => 4,
        (4, false) => 5,
        (3, true) => 6,
        (3, false) => 7,
        _ => 0,
    }
}

/// C(n, k) for the small sizes a ticket can have.
pub fn choose(n: usize, k: usize) -> i64 {
    if k > n {
        return 0;
    }
    let k = k.min(n - k);
    let mut result = 1i64;
    for i in 0..k {
        result = result * (n - i) as i64 / (i as i64 + 1);
    }
    result
}

/// Unbiased draw of `count` distinct numbers from 1..=max, sorted.
fn sample(max: u8, count: usize) -> Vec<i16> {
    let mut picked: Vec<i16> = rand::seq::index::sample(&mut OsRng, usize::from(max), count)
        .into_iter()
        .map(|i| i as i16 + 1)
        .collect();
    picked.sort_unstable();
    picked
}

/// Winning numbers of one period: `(numbers, scalar bonus, bonus numbers)`.
/// DLT uses two back numbers; existing scalar-bonus games keep their wire shape.
pub fn draw(game: Game) -> (Vec<i16>, Option<i16>, Vec<i16>) {
    use rand::Rng;
    match game {
        Game::Ssq => (
            sample(SSQ_RED_MAX, 6),
            Some(OsRng.gen_range(1..=i16::from(SSQ_BLUE_MAX))),
            Vec::new(),
        ),
        Game::Dlt => (sample(DLT_FRONT_MAX, 5), None, sample(DLT_BACK_MAX, 2)),
        Game::Toto => {
            let seven: Vec<i16> = rand::seq::index::sample(&mut OsRng, usize::from(TOTO_MAX), 7)
                .into_iter()
                .map(|i| i as i16 + 1)
                .collect();
            let mut winning = seven[..6].to_vec();
            winning.sort_unstable();
            (winning, Some(seven[6]), Vec::new())
        }
        Game::Happy8 => (sample(HAPPY8_MAX, HAPPY8_DRAW), None, Vec::new()),
        Game::Fucai3d => (
            (0..3).map(|_| OsRng.gen_range(0..=9i16)).collect(),
            None,
            Vec::new(),
        ),
        Game::Scratch => (Vec::new(), None, Vec::new()),
    }
}
