//! 十三水 hand evaluation: five-card and three-card rows, special hands and
//! the best non-foul arrangement of thirteen cards.
//!
//! A is high and A2345 is the lowest straight. Suits never break ties. Rows
//! compare by kind, then by ranks in significance order; a three-card row equal
//! to a five-card row's leading ranks is a tie, so it never makes a foul.
use serde::{Deserialize, Serialize};

pub const HAND: usize = 13;
pub const ROWS: [usize; 3] = [3, 5, 5];

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
pub struct Card {
    /// 2..=14, A = 14.
    pub rank: u8,
    /// 0..=3: s h c d.
    pub suit: u8,
}
const RANKS: &[u8; 13] = b"23456789TJQKA";
const SUITS: &[u8; 4] = b"shcd";
impl Card {
    /// Native id in the shared shuffle proof's 52-card manifest.
    pub fn from_deck_id(id: u16) -> Option<Self> {
        let id = u8::try_from(id).ok().filter(|id| *id < 52)?;
        Some(Self {
            rank: id % 13 + 2,
            suit: id / 13,
        })
    }

    pub fn valid(self) -> bool {
        (2..=14).contains(&self.rank) && self.suit < 4
    }
    pub fn code(self) -> String {
        format!(
            "{}{}",
            RANKS[usize::from(self.rank - 2)] as char,
            SUITS[usize::from(self.suit)] as char
        )
    }
    pub fn parse(code: &str) -> Option<Self> {
        let b = code.as_bytes();
        if b.len() != 2 {
            return None;
        }
        let rank = RANKS.iter().position(|r| *r == b[0])? as u8 + 2;
        let suit = SUITS.iter().position(|s| *s == b[1])? as u8;
        Some(Self { rank, suit })
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Kind {
    High,
    Pair,
    TwoPair,
    Trips,
    Straight,
    Flush,
    FullHouse,
    Quads,
    StraightFlush,
}

/// One evaluated row: its kind and ranks in significance order.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Eval {
    pub kind: Kind,
    pub ranks: Vec<u8>,
}
impl Eval {
    /// Total order across three- and five-card rows (missing ranks count 0).
    pub fn key(&self) -> u32 {
        let mut key = (self.kind as u32) << 20;
        for (i, r) in self.ranks.iter().take(5).enumerate() {
            key |= u32::from(*r) << (16 - 4 * i);
        }
        key
    }
}

pub fn evaluate(cards: &[Card]) -> Eval {
    let mut counts = [0u8; 15];
    for c in cards {
        counts[usize::from(c.rank)] += 1;
    }
    // (count, rank), most significant first.
    let mut groups: Vec<(u8, u8)> = (2..=14u8)
        .rev()
        .filter(|r| counts[usize::from(*r)] > 0)
        .map(|r| (counts[usize::from(r)], r))
        .collect();
    groups.sort_by(|a, b| b.cmp(a));
    let ranks: Vec<u8> = groups.iter().map(|g| g.1).collect();
    let five = cards.len() == 5;
    let flush = five && cards.iter().all(|c| c.suit == cards[0].suit);
    let straight_top = (five && groups.len() == 5)
        .then(|| {
            if ranks[0] - ranks[4] == 4 {
                Some(ranks[0])
            } else if ranks == [14, 5, 4, 3, 2] {
                Some(5)
            } else {
                None
            }
        })
        .flatten();
    let shape: Vec<u8> = groups.iter().map(|g| g.0).collect();
    let kind = match (straight_top, flush, shape.as_slice()) {
        (Some(_), true, _) => Kind::StraightFlush,
        (_, _, [4, ..]) => Kind::Quads,
        (_, _, [3, 2]) => Kind::FullHouse,
        (_, true, _) => Kind::Flush,
        (Some(_), _, _) => Kind::Straight,
        (_, _, [3, ..]) => Kind::Trips,
        (_, _, [2, 2, ..]) => Kind::TwoPair,
        (_, _, [2, ..]) => Kind::Pair,
        _ => Kind::High,
    };
    let ranks = match straight_top {
        Some(top) if matches!(kind, Kind::Straight | Kind::StraightFlush) => vec![top],
        _ => ranks,
    };
    Eval { kind, ranks }
}

/// 道 the winner of row `row` (0 front, 1 middle, 2 back) receives.
pub fn row_points(row: usize, kind: Kind) -> i64 {
    match (row, kind) {
        (0, Kind::Trips) => 3,
        (1, Kind::FullHouse) => 2,
        (1, Kind::Quads) => 8,
        (1, Kind::StraightFlush) => 10,
        (2, Kind::Quads) => 4,
        (2, Kind::StraightFlush) => 5,
        _ => 1,
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Special {
    Dragon,
    SixPairs,
    ThreeStraights,
    ThreeFlushes,
}
impl Special {
    /// 道 won from every opponent without a stronger special.
    pub fn points(self) -> i64 {
        match self {
            Self::Dragon => 13,
            Self::SixPairs | Self::ThreeStraights | Self::ThreeFlushes => 3,
        }
    }
}

pub fn special(cards: &[Card]) -> Option<Special> {
    if cards.len() != HAND {
        return None;
    }
    let mut counts = [0u8; 15];
    let mut suits = [0u8; 4];
    for c in cards {
        counts[usize::from(c.rank)] += 1;
        suits[usize::from(c.suit)] += 1;
    }
    if counts[2..=14].iter().all(|n| *n == 1) {
        return Some(Special::Dragon);
    }
    if counts.iter().map(|n| n / 2).sum::<u8>() == 6 {
        return Some(Special::SixPairs);
    }
    if three_straights(&mut counts) {
        return Some(Special::ThreeStraights);
    }
    // Each row is one suit: some assignment of rows to suits uses every card.
    for a in 0..4 {
        for b in 0..4 {
            for c in 0..4 {
                let mut used = [0u8; 4];
                used[a] += 3;
                used[b] += 5;
                used[c] += 5;
                if used == suits {
                    return Some(Special::ThreeFlushes);
                }
            }
        }
    }
    None
}

/// Whether the rank counts split into a three-card and two five-card straights.
fn three_straights(counts: &mut [u8; 15]) -> bool {
    // A plays high (QKA) or low (A23 / A2345).
    fn take(counts: &mut [u8; 15], ranks: &[u8]) -> bool {
        if ranks.iter().any(|r| counts[usize::from(*r)] == 0) {
            return false;
        }
        ranks.iter().for_each(|r| counts[usize::from(*r)] -= 1);
        true
    }
    fn give(counts: &mut [u8; 15], ranks: &[u8]) {
        ranks.iter().for_each(|r| counts[usize::from(*r)] += 1);
    }
    fn run(start: u8, len: u8) -> Vec<u8> {
        (start..start + len).map(|r| if r == 1 { 14 } else { r }).collect()
    }
    for f in 1..=12u8 {
        let front = run(f, 3);
        if !take(counts, &front) {
            continue;
        }
        for m in 1..=10u8 {
            let middle = run(m, 5);
            if !take(counts, &middle) {
                continue;
            }
            let rest = (1..=10u8).any(|b| {
                let back = run(b, 5);
                back.iter().all(|r| counts[usize::from(*r)] == 1)
                    && counts.iter().map(|n| u32::from(*n)).sum::<u32>() == 5
            });
            give(counts, &middle);
            if rest {
                give(counts, &front);
                return true;
            }
        }
        give(counts, &front);
    }
    false
}

/// Rows as card lists, front first.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Rows {
    pub front: Vec<Card>,
    pub middle: Vec<Card>,
    pub back: Vec<Card>,
}
impl Rows {
    pub fn rows(&self) -> [&[Card]; 3] {
        [&self.front, &self.middle, &self.back]
    }
    pub fn evals(&self) -> [Eval; 3] {
        self.rows().map(evaluate)
    }
    pub fn foul(&self) -> bool {
        let [f, m, b] = self.evals();
        f.key() > m.key() || m.key() > b.key()
    }
    pub fn all(&self) -> Vec<Card> {
        self.rows().concat()
    }
}

/// Rough chance a row wins against a random opponent's row in that position,
/// from its kind and leading rank.
fn win_chance(row: usize, e: &Eval) -> f64 {
    let lead = f64::from(e.ranks.first().copied().unwrap_or(2).saturating_sub(2)) / 12.0;
    let (lo, hi) = match (row, e.kind) {
        (0, Kind::High) => (0.02, 0.35),
        (0, Kind::Pair) => (0.45, 0.95),
        (0, _) => (0.97, 1.0),
        (1, Kind::High) => (0.01, 0.08),
        (1, Kind::Pair) => (0.10, 0.40),
        (1, Kind::TwoPair) => (0.45, 0.65),
        (1, Kind::Trips) => (0.68, 0.78),
        (1, Kind::Straight) => (0.78, 0.85),
        (1, Kind::Flush) => (0.85, 0.92),
        (1, Kind::FullHouse) => (0.93, 0.98),
        (1, _) => (0.99, 1.0),
        (_, Kind::High) => (0.0, 0.02),
        (_, Kind::Pair) => (0.03, 0.12),
        (_, Kind::TwoPair) => (0.15, 0.35),
        (_, Kind::Trips) => (0.38, 0.50),
        (_, Kind::Straight) => (0.50, 0.62),
        (_, Kind::Flush) => (0.62, 0.78),
        (_, Kind::FullHouse) => (0.80, 0.92),
        (_, Kind::Quads) => (0.94, 0.99),
        (_, Kind::StraightFlush) => (0.99, 1.0),
    };
    lo + (hi - lo) * lead
}

/// The non-foul arrangement with the best expected 道 against one opponent.
/// Every 3/5/5 split is tried (72 072 for thirteen cards).
pub fn best(cards: &[Card]) -> Rows {
    assert_eq!(cards.len(), HAND, "thirteen cards");
    let full: u16 = (1 << HAND) - 1;
    let pick = |mask: u16| -> Vec<Card> {
        (0..HAND)
            .filter(|i| mask & (1 << i) != 0)
            .map(|i| cards[i])
            .collect()
    };
    let mut five: Vec<Option<(u32, f64, f64)>> = vec![None; 1 << HAND];
    let mut three: Vec<Option<(u32, f64)>> = vec![None; 1 << HAND];
    for mask in 0..=full {
        match mask.count_ones() {
            5 => {
                let e = evaluate(&pick(mask));
                let value = |row| (2.0 * win_chance(row, &e) - 1.0) * row_points(row, e.kind) as f64;
                five[usize::from(mask)] = Some((e.key(), value(1), value(2)));
            }
            3 => {
                let e = evaluate(&pick(mask));
                three[usize::from(mask)] =
                    Some((e.key(), (2.0 * win_chance(0, &e) - 1.0) * row_points(0, e.kind) as f64));
            }
            _ => {}
        }
    }
    let mut chosen: Option<(f64, u16, u16)> = None;
    for back in 0..=full {
        let Some((bk, _, bv)) = five[usize::from(back)] else {
            continue;
        };
        let rest = full ^ back;
        let mut middle = rest;
        while middle != 0 {
            if let (Some((mk, mv, _)), Some((fk, fv))) =
                (five[usize::from(middle)], three[usize::from(rest ^ middle)])
            {
                let value = fv + mv + bv;
                if fk <= mk && mk <= bk && chosen.is_none_or(|(v, _, _)| value > v) {
                    chosen = Some((value, middle, back));
                }
            }
            middle = (middle - 1) & rest;
        }
    }
    // The strongest five cards at the back always leave a legal split.
    let (_, middle, back) = chosen.expect("a non-foul split exists");
    let sorted = |mask| {
        let mut row = pick(mask);
        row.sort_by(|a, b| b.cmp(a));
        row
    };
    Rows {
        front: sorted(full ^ middle ^ back),
        middle: sorted(middle),
        back: sorted(back),
    }
}
