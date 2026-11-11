//! Tickets a station sells and what they win: entry normalisation, 注数 and
//! the closed-form prize of a 复式 ticket. Unit prices and payout tables are
//! the client's current constants (`client-game/src/views/games/*/`). These
//! are fixed practice gold-bean prizes, not real-money lottery pool prizes.
use crate::proof::{sha256_fields, Entropy};
use chrono::{FixedOffset, TimeZone};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};

pub const MAX_STAKES: i64 = 100;
/// Highest ticket multiplier (倍投), as in real lotteries. Only ALL-IN uses
/// more than 1: it compresses repeated basic stakes into multiplied rows.
pub const MAX_MULTIPLIER: i64 = 99;
/// ALL-IN keeps its existing receipt grouping independently of manual purchases.
pub const MAX_ALL_IN_GROUPS: usize = 10;
/// Most basic stakes one ALL-IN buys; a larger balance buys this many.
pub const MAX_ALL_IN_STAKES: i64 = MAX_ALL_IN_GROUPS as i64 * MAX_MULTIPLIER;
pub const MAX_TICKETS_PER_PURCHASE: usize = 100;
pub const MAX_TICKETS_PER_PERIOD: i64 = 100;
pub const MAX_STAKES_PER_PERIOD: i64 = 1_000;

pub const SSQ_RED_MAX: u8 = 33;
pub const SSQ_BLUE_MAX: u8 = 16;
pub const DLT_FRONT_MAX: u8 = 35;
pub const DLT_BACK_MAX: u8 = 12;
pub const TOTO_MAX: u8 = 49;
pub const HAPPY8_MAX: u8 = 80;
pub const HAPPY8_DRAW: usize = 20;

/// Beans per winning 注 (stake included) by prize group, at [`Game::unit`]
/// beans per 注 (SSQ/DLT 2, TOTO 1).
pub const SSQ_PRIZE: [i64; 7] = [0, 2_000_000, 200_000, 3_000, 200, 10, 5];
/// Seven prize tiers under the 2026 DLT matching rules, in practice beans.
pub const DLT_PRIZE: [i64; 8] = [0, 2_000_000, 200_000, 5_000, 300, 150, 15, 5];
pub const TOTO_PRIZE: [i64; 8] = [0, 1_000_000, 100_000, 1_000, 300, 50, 25, 10];
/// 福彩3D at 2 beans per 注: 520×, 173× and 86.5× the stake (return ~52%).
pub const FC3D_STRAIGHT: i64 = 1_040;
pub const FC3D_GROUP3: i64 = 346;
pub const FC3D_GROUP6: i64 = 173;
/// A jackpot ticket also earns this many times one top-tier stake prize.
pub const JACKPOT_BONUS_FACTOR: i64 = 5;

/// Happy8 beans per 2-bean 注 (stake included) by picks × hits.
pub fn happy8_prize(picks: usize, hits: usize) -> i64 {
    match (picks, hits) {
        (1, 1) => 5,
        (2, 2) => 19,
        (3, 2) => 3,
        (3, 3) => 52,
        (4, 2) => 3,
        (4, 3) => 5,
        (4, 4) => 93,
        (5, 3) => 3,
        (5, 4) => 20,
        (5, 5) => 1_000,
        (6, 3) => 3,
        (6, 4) => 10,
        (6, 5) => 30,
        (6, 6) => 2_880,
        (7, 0) => 2,
        (7, 4) => 4,
        (7, 5) => 30,
        (7, 6) => 300,
        (7, 7) => 8_500,
        (8, 0) => 2,
        (8, 4) => 3,
        (8, 5) => 10,
        (8, 6) => 80,
        (8, 7) => 800,
        (8, 8) => 50_000,
        (9, 0) => 2,
        (9, 4) => 3,
        (9, 5) => 5,
        (9, 6) => 22,
        (9, 7) => 225,
        (9, 8) => 2_000,
        (9, 9) => 250_000,
        (10, 0) => 2,
        (10, 5) => 3,
        (10, 6) => 5,
        (10, 7) => 80,
        (10, 8) => 720,
        (10, 9) => 8_000,
        (10, 10) => 5_000_000,
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

    /// Whether a settled result hit this game's jackpot: one award per
    /// winning ticket, regardless of its stake count. Only the pooled-style
    /// draws (SSQ/DLT/TOTO first prize) have one; Happy8 and 福彩3D pay
    /// their fixed table only.
    pub fn awards_jackpot(&self, prize: i64, tier: i16) -> bool {
        prize > 0
            && match self {
                Entry::Ssq { .. } | Entry::Dlt { .. } | Entry::Toto { .. } => tier == 1,
                Entry::Happy8 { .. } | Entry::Straight(_) | Entry::Group3(_) | Entry::Group6(_) => {
                    false
                }
            }
    }

    /// Extra beans for a jackpot ticket: [`JACKPOT_BONUS_FACTOR`] × ONE
    /// top-tier stake prize at this ticket's multiplier (never a multiple of a
    /// compound ticket's whole payout). `Some(0)` when the result is not a
    /// jackpot; `None` on overflow.
    pub fn jackpot_bonus(&self, multiplier: i64, prize: i64, tier: i16) -> Option<i64> {
        if !self.awards_jackpot(prize, tier) {
            return Some(0);
        }
        let top = match self {
            Entry::Ssq { .. } => SSQ_PRIZE[1],
            Entry::Dlt { .. } => DLT_PRIZE[1],
            Entry::Toto { .. } => TOTO_PRIZE[1],
            Entry::Happy8 { .. } | Entry::Straight(_) | Entry::Group3(_) | Entry::Group6(_) => {
                return Some(0)
            }
        };
        top.checked_mul(multiplier)?
            .checked_mul(JACKPOT_BONUS_FACTOR)
    }

    /// Prize in beans and tier for `multiplier` × this ticket. `None` when
    /// the stored draw does not fit the game (never for rows the draw loop
    /// wrote) or the multiplier is out of range.
    pub fn prize(
        &self,
        multiplier: i64,
        numbers: &[i16],
        bonus: Option<i16>,
        bonus_numbers: &[i16],
    ) -> Option<(i64, i16)> {
        if !(1..=MAX_MULTIPLIER).contains(&multiplier) {
            return None;
        }
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

/// 封盘: sales close this long before the draw.
pub const CLOSE_MS: i64 = 5_000;
/// Period labels and report days use Singapore time.
pub const SGT_OFFSET_SECS: i32 = 8 * 60 * 60;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Game {
    Ssq,
    Dlt,
    Toto,
    Happy8,
    Fucai3d,
    Scratch,
}

impl Game {
    pub const ALL: [Game; 6] = [
        Game::Ssq,
        Game::Dlt,
        Game::Toto,
        Game::Happy8,
        Game::Fucai3d,
        Game::Scratch,
    ];
    /// The scheduled number games, in draw-loop order.
    pub const DRAWN: [Game; 5] = [
        Game::Happy8,
        Game::Fucai3d,
        Game::Ssq,
        Game::Dlt,
        Game::Toto,
    ];

    pub fn parse(name: &str) -> Option<Game> {
        Some(match name {
            "ssq" => Game::Ssq,
            "dlt" => Game::Dlt,
            "toto" => Game::Toto,
            "happy8" => Game::Happy8,
            "fucai3d" => Game::Fucai3d,
            "scratch" => Game::Scratch,
            _ => return None,
        })
    }
    pub fn as_str(self) -> &'static str {
        match self {
            Game::Ssq => "ssq",
            Game::Dlt => "dlt",
            Game::Toto => "toto",
            Game::Happy8 => "happy8",
            Game::Fucai3d => "fucai3d",
            Game::Scratch => "scratch",
        }
    }
    /// Beans per 注 (scratch: per card): the smallest price at which every
    /// prize tier is a whole number of beans.
    pub fn unit(self) -> i64 {
        match self {
            Game::Ssq | Game::Dlt | Game::Happy8 | Game::Fucai3d => 2,
            Game::Toto => 1,
            Game::Scratch => crate::scratch::COST,
        }
    }
    /// All current number lotteries share the minute boundary.
    pub fn schedule(self) -> Option<Schedule> {
        (self != Game::Scratch).then_some(Schedule {
            interval_ms: 60_000,
            offset_ms: 0,
        })
    }
}

/// Wall-clock aligned periods: `period = floor((now − offset) / interval)`.
#[derive(Debug, Clone, Copy)]
pub struct Schedule {
    pub interval_ms: i64,
    pub offset_ms: i64,
}

#[derive(Debug, Clone)]
pub struct Period {
    pub id: i64,
    pub opens_ms: i64,
    pub closes_ms: i64,
    pub draw_ms: i64,
    pub label: String,
}

impl Schedule {
    /// The period selling at `now_ms` (its draw is the next boundary).
    pub fn period_at(&self, now_ms: i64) -> Period {
        self.period((now_ms - self.offset_ms).div_euclid(self.interval_ms))
    }
    pub fn period(&self, id: i64) -> Period {
        let opens_ms = id * self.interval_ms + self.offset_ms;
        let draw_ms = opens_ms + self.interval_ms;
        Period {
            id,
            opens_ms,
            closes_ms: draw_ms - CLOSE_MS,
            draw_ms,
            label: self.label(draw_ms),
        }
    }
    /// `YYYYMMDD-NNN`: the SGT day of the draw and the draw's 1-based index
    /// within that day.
    pub fn label(&self, draw_ms: i64) -> String {
        let sgt = sgt();
        let at = sgt
            .timestamp_millis_opt(draw_ms)
            .single()
            .unwrap_or_else(|| {
                sgt.timestamp_millis_opt(0)
                    .single()
                    .expect("epoch is representable")
            });
        let day_start_ms = sgt
            .from_local_datetime(&at.date_naive().and_hms_opt(0, 0, 0).expect("midnight"))
            .single()
            .map_or(draw_ms, |d| d.timestamp_millis());
        // Draws happen at j·interval + offset (j ≥ 1); count the day's draws
        // up to this one.
        let j = (draw_ms - self.offset_ms).div_euclid(self.interval_ms);
        let first_j =
            (day_start_ms - self.offset_ms + self.interval_ms - 1).div_euclid(self.interval_ms);
        let index = (j - first_j + 1).max(1);
        format!("{}-{:03}", at.format("%Y%m%d"), index)
    }
}

pub fn sgt() -> FixedOffset {
    FixedOffset::east_opt(SGT_OFFSET_SECS).expect("Singapore UTC offset is valid")
}

/// v4 = the minute schedule with the 1-bean-scale prize tables above.
pub const PROTOCOL: &str = "bluffking-lottery-drand-v4-minute";

/// All choice points are derived from the game and wall-clock period. The
/// commitment binds the protocol version, sales deadline and exact future pulse.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct Plan {
    pub protocol: String,
    pub game: String,
    pub period: i64,
    pub draw_at_ms: i64,
    pub closes_at_ms: i64,
    pub chain_hash: String,
    pub beacon_round: u64,
    pub commitment: String,
}

impl Plan {
    pub fn new(game: Game, period: i64) -> Result<Self, String> {
        let schedule = game.schedule().ok_or("not a scheduled lottery")?;
        let draw_at_ms = period
            .checked_add(1)
            .and_then(|n| n.checked_mul(schedule.interval_ms))
            .and_then(|n| n.checked_add(schedule.offset_ms))
            .ok_or("period overflow")?;
        let closes_at_ms = draw_at_ms.checked_sub(CLOSE_MS).ok_or("period overflow")?;
        let beacon_round = crate::beacon::round_at_or_after(draw_at_ms)?;
        let commitment = hex::encode(sha256_fields(&[
            PROTOCOL.as_bytes(),
            game.as_str().as_bytes(),
            &period.to_be_bytes(),
            &draw_at_ms.to_be_bytes(),
            &closes_at_ms.to_be_bytes(),
            crate::beacon::CHAIN_HASH.as_bytes(),
            &beacon_round.to_be_bytes(),
        ]));
        Ok(Self {
            protocol: PROTOCOL.into(),
            game: game.as_str().into(),
            period,
            draw_at_ms,
            closes_at_ms,
            chain_hash: crate::beacon::CHAIN_HASH.into(),
            beacon_round,
            commitment,
        })
    }

    pub fn validate(&self) -> Result<Game, String> {
        let game = Game::parse(&self.game).ok_or("unknown lottery game")?;
        if self.protocol != PROTOCOL {
            return Err("unsupported lottery protocol".into());
        }
        if *self != Self::new(game, self.period)? {
            return Err("plan does not match the fixed public schedule and protocol".into());
        }
        Ok(game)
    }

    /// Use the accepted plan's own schedule for saved ticket windows and labels.
    pub fn sales_opens_at_ms(&self) -> Result<i64, String> {
        let game = self.validate()?;
        self.draw_at_ms
            .checked_sub(
                game.schedule()
                    .ok_or("not a scheduled lottery")?
                    .interval_ms,
            )
            .ok_or_else(|| "sales window overflow".into())
    }

    pub fn label(&self) -> Result<String, String> {
        let game = self.validate()?;
        Ok(game
            .schedule()
            .ok_or("not a scheduled lottery")?
            .label(self.draw_at_ms))
    }
}

/// Partial Fisher-Yates with rejection sampling through Entropy::below. TOTO
/// preserves the seventh sampled ball as its additional number before sorting.
fn sample(entropy: &mut Entropy, max: u8, count: usize, sorted: bool) -> Vec<i16> {
    let mut balls: Vec<i16> = (1..=i16::from(max)).collect();
    for i in 0..count {
        let j = i + entropy.below((balls.len() - i) as u64) as usize;
        balls.swap(i, j);
    }
    balls.truncate(count);
    if sorted {
        balls.sort_unstable();
    }
    balls
}

/// The exact deterministic sampler used by production and the offline verifier.
pub fn draw(game: Game, entropy: &mut Entropy) -> (Vec<i16>, Option<i16>, Vec<i16>) {
    match game {
        Game::Ssq => (
            sample(entropy, SSQ_RED_MAX, 6, true),
            Some(entropy.below(u64::from(SSQ_BLUE_MAX)) as i16 + 1),
            Vec::new(),
        ),
        Game::Dlt => (
            sample(entropy, DLT_FRONT_MAX, 5, true),
            None,
            sample(entropy, DLT_BACK_MAX, 2, true),
        ),
        Game::Toto => {
            let seven = sample(entropy, TOTO_MAX, 7, false);
            let mut winning = seven[..6].to_vec();
            winning.sort_unstable();
            (winning, Some(seven[6]), Vec::new())
        }
        Game::Happy8 => (
            sample(entropy, HAPPY8_MAX, HAPPY8_DRAW, true),
            None,
            Vec::new(),
        ),
        Game::Fucai3d => (
            (0..3).map(|_| entropy.below(10) as i16).collect(),
            None,
            Vec::new(),
        ),
        Game::Scratch => (Vec::new(), None, Vec::new()),
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DrawProof {
    pub kind: String,
    pub plan: Plan,
    pub beacon: crate::beacon::Beacon,
    pub numbers: Vec<i16>,
    pub bonus: Option<i16>,
    pub bonus_numbers: Vec<i16>,
}

impl DrawProof {
    pub fn new(plan: Plan, beacon: crate::beacon::Beacon) -> Result<Self, String> {
        let game = plan.validate()?;
        if beacon.round != plan.beacon_round {
            return Err("beacon is not the preselected round".into());
        }
        let randomness = crate::beacon::verify(&beacon)?;
        let seed = sha256_fields(&[
            b"bluffking-lottery-entropy-v1",
            plan.commitment.as_bytes(),
            &randomness,
        ]);
        let (numbers, bonus, bonus_numbers) = draw(game, &mut Entropy::new(seed));
        Ok(Self {
            kind: "lottery".into(),
            plan,
            beacon,
            numbers,
            bonus,
            bonus_numbers,
        })
    }
}

/// Replay a draw, then optionally recompute one saved ticket's stakes, cost and
/// credited prize. A server-supplied receipt alone cannot prove when it was seen.
pub fn verify(value: &Value) -> Result<Value, String> {
    let mut draw_value = value.clone();
    let ticket = draw_value
        .as_object_mut()
        .ok_or("proof must be an object")?
        .remove("ticket");
    let proof: DrawProof =
        serde_json::from_value(draw_value).map_err(|_| "invalid lottery proof")?;
    if proof.kind != "lottery" {
        return Err("not a lottery proof".into());
    }
    let expected = DrawProof::new(proof.plan.clone(), proof.beacon.clone())?;
    if proof.numbers != expected.numbers
        || proof.bonus != expected.bonus
        || proof.bonus_numbers != expected.bonus_numbers
    {
        return Err("winning numbers do not match the selected beacon".into());
    }
    let game = proof.plan.validate()?;
    let mut receipt_result = Value::Null;
    if let Some(ticket) = ticket {
        let plan: Plan = serde_json::from_value(ticket["proof_plan"].clone())
            .map_err(|_| "ticket has no accepted plan")?;
        if plan != proof.plan || ticket["period"].as_i64() != Some(plan.period) {
            return Err("ticket was accepted for a different plan or period".into());
        }
        let bought_at_ms = ticket["bought_at_ms"]
            .as_i64()
            .ok_or("missing ticket purchase time")?;
        let opens_at_ms = plan.sales_opens_at_ms()?;
        if !(opens_at_ms..plan.closes_at_ms).contains(&bought_at_ms) {
            return Err("ticket purchase time is outside the published sales window".into());
        }
        let entry = Entry::parse(game, &ticket["entry"]).map_err(|_| "invalid ticket entry")?;
        if entry.to_value() != ticket["entry"] {
            return Err("ticket entry is not normalized".into());
        }
        let multiplier = ticket["multiplier"]
            .as_i64()
            .filter(|m| (1..=MAX_MULTIPLIER).contains(m))
            .ok_or("invalid ticket multiplier")?;
        let cost = entry
            .stakes()
            .checked_mul(game.unit())
            .and_then(|n| n.checked_mul(multiplier))
            .ok_or("ticket cost overflow")?;
        if ticket["stakes"].as_i64() != Some(entry.stakes())
            || ticket["cost"].as_i64() != Some(cost)
        {
            return Err("ticket stakes or cost mismatch".into());
        }
        let (prize, tier) = entry
            .prize(
                multiplier,
                &proof.numbers,
                proof.bonus,
                &proof.bonus_numbers,
            )
            .ok_or("cannot calculate prize")?;
        let settled = ticket["settled"]
            .as_bool()
            .ok_or("missing ticket settlement state")?;
        if settled
            && (ticket["prize"].as_i64() != Some(prize)
                || ticket["tier"].as_i64() != Some(i64::from(tier)))
        {
            return Err("credited ticket prize or tier mismatch".into());
        }
        if !settled && (!ticket["prize"].is_null() || !ticket["tier"].is_null()) {
            return Err("unsettled ticket claims a credited prize".into());
        }
        let jackpot = settled && entry.awards_jackpot(prize, tier);
        let expected_bonus = entry
            .jackpot_bonus(multiplier, prize, tier)
            .ok_or("jackpot bonus overflow")?;
        let bonus = &ticket["jackpot_bonus"];
        if jackpot {
            if bonus.as_i64() != Some(expected_bonus) {
                return Err("jackpot bonus mismatch".into());
            }
        } else if !bonus.is_null() {
            return Err("ticket claims an unearned jackpot bonus".into());
        }
        let credited_bonus = if jackpot { expected_bonus } else { 0 };
        receipt_result = json!({ "stakes": entry.stakes(), "cost": cost, "prize": prize, "tier": tier,
            "jackpot_bonus": credited_bonus, "settled": settled, "credited_prize_checked": settled });
    }
    Ok(
        json!({ "kind": "lottery", "valid": true, "game": game.as_str(), "period": proof.plan.period,
        "commitment": proof.plan.commitment, "numbers": proof.numbers, "bonus": proof.bonus,
        "bonus_numbers": proof.bonus_numbers, "ticket": receipt_result,
        "publication": "compare with the plan saved before purchase; this file alone does not prove publication time" }),
    )
}
