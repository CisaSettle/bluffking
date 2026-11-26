//! 短牌德州 card rules: the 36-card deck and best-five-of-seven evaluation.
//!
//! Ranking (high → low), as GGPoker plays it: straight flush > four of a kind >
//! flush > full house > straight > three of a kind > two pair > one pair > high
//! card. A-6-7-8-9 is
//! the lowest straight (and straight flush); the Ace also plays high. Suits
//! never break ties.

use serde::{Deserialize, Serialize};

/// Lowest rank in the deck.
pub const LOW_RANK: u8 = 6;
pub const DECK_SIZE: usize = 36;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct Card {
    /// 6..=14 (14 = A).
    pub rank: u8,
    /// 0..=3 → ♠ ♥ ♣ ♦.
    pub suit: u8,
}

impl Card {
    pub const fn new(rank: u8, suit: u8) -> Self {
        Self { rank, suit }
    }

    /// Wire form: rank char + suit char, e.g. `"As"`, `"Th"`, `"6d"`.
    pub fn code(self) -> String {
        let r = match self.rank {
            14 => 'A',
            13 => 'K',
            12 => 'Q',
            11 => 'J',
            10 => 'T',
            n => char::from(b'0' + n),
        };
        let s = ['s', 'h', 'c', 'd'][usize::from(self.suit & 3)];
        format!("{r}{s}")
    }
}

pub fn full_deck() -> Vec<Card> {
    let mut deck = Vec::with_capacity(DECK_SIZE);
    for suit in 0..4 {
        for rank in LOW_RANK..=14 {
            deck.push(Card::new(rank, suit));
        }
    }
    deck
}

/// Hand categories in short-deck order, so the derived `Ord` is the ranking.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Category {
    HighCard,
    OnePair,
    TwoPair,
    ThreeOfAKind,
    Straight,
    FullHouse,
    Flush,
    FourOfAKind,
    StraightFlush,
}

/// A comparable hand value: category first, then tie-break ranks (most
/// significant first, zero-padded).
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub struct HandValue {
    pub category: Category,
    pub keys: [u8; 5],
}

/// The best hand among `cards` (2..=7 cards) and the five cards that make it.
/// Fewer than five cards (preflop) rate the pairs/high cards present.
pub fn best_hand(cards: &[Card]) -> (HandValue, Vec<Card>) {
    debug_assert!((1..=7).contains(&cards.len()));
    if cards.len() < 5 {
        return (partial(cards), cards.to_vec());
    }
    let (value, best) = best_five(cards);
    (value, best.to_vec())
}

pub fn best_five(cards: &[Card]) -> (HandValue, [Card; 5]) {
    debug_assert!((5..=7).contains(&cards.len()));
    let n = cards.len();
    let mut best: Option<(HandValue, [usize; 5])> = None;
    for a in 0..n {
        for b in a + 1..n {
            for c in b + 1..n {
                for d in c + 1..n {
                    for e in d + 1..n {
                        let idx = [a, b, c, d, e];
                        let v = five(&idx.map(|i| cards[i]));
                        if best.as_ref().is_none_or(|(bv, _)| v > *bv) {
                            best = Some((v, idx));
                        }
                    }
                }
            }
        }
    }
    let (v, idx) = best.expect("at least five cards");
    (v, idx.map(|i| cards[i]))
}

pub fn evaluate(cards: &[Card]) -> HandValue {
    best_hand(cards).0
}

/// Up to five (count, rank) groups ordered by count and rank; unused keys
/// are zero so both the showdown and partial-hand tie breakers are unchanged.
fn groups(cards: &[Card]) -> [(u8, u8); 5] {
    let mut counts = [0u8; 15];
    for c in cards {
        counts[usize::from(c.rank)] += 1;
    }
    let mut g = [(0, 0); 5];
    let mut n = 0;
    for rank in (LOW_RANK..=14).rev() {
        let count = counts[usize::from(rank)];
        if count != 0 {
            g[n] = (count, rank);
            n += 1;
        }
    }
    g[..n].sort_unstable_by(|a, b| b.cmp(a));
    g
}

fn keys_from(ranks: impl IntoIterator<Item = u8>) -> [u8; 5] {
    let mut keys = [0u8; 5];
    for (k, r) in keys.iter_mut().zip(ranks) {
        *k = r;
    }
    keys
}

fn five(cards: &[Card; 5]) -> HandValue {
    let g = groups(cards);
    let flush = cards.iter().all(|c| c.suit == cards[0].suit);
    let straight_high = if g[4].0 != 0 {
        let ranks = g.map(|(_, rank)| rank);
        if ranks[0] - ranks[4] == 4 {
            Some(ranks[0])
        } else if ranks == [14, 9, 8, 7, 6] {
            // A-6-7-8-9: the Ace plays low, so the Nine is the top card.
            Some(9)
        } else {
            None
        }
    } else {
        None
    };
    let by_group = || keys_from(g.iter().map(|&(_, r)| r));
    let (category, keys) = match (straight_high, flush, g[0].0, g[1].0) {
        (Some(h), true, _, _) => (Category::StraightFlush, keys_from([h])),
        (_, _, 4, _) => (Category::FourOfAKind, by_group()),
        (_, true, _, _) => (Category::Flush, by_group()),
        (_, _, 3, 2) => (Category::FullHouse, by_group()),
        (_, _, 3, _) => (Category::ThreeOfAKind, by_group()),
        (Some(h), false, _, _) => (Category::Straight, keys_from([h])),
        (_, _, 2, 2) => (Category::TwoPair, by_group()),
        (_, _, 2, _) => (Category::OnePair, by_group()),
        _ => (Category::HighCard, by_group()),
    };
    HandValue { category, keys }
}

/// Preflop / short inputs: only pairs, trips and high cards can be named.
fn partial(cards: &[Card]) -> HandValue {
    let g = groups(cards);
    let category = match (g[0].0, g[1].0) {
        (4, _) => Category::FourOfAKind,
        (3, _) => Category::ThreeOfAKind,
        (2, 2) => Category::TwoPair,
        (2, _) => Category::OnePair,
        _ => Category::HighCard,
    };
    HandValue {
        category,
        keys: keys_from(g.iter().map(|&(_, r)| r)),
    }
}

/// Evaluate shared-table cards with the Short Deck rules.
pub(crate) fn rank(hole: &crate::HoleCards, board: &crate::BoardCards) -> HandValue {
    let cards: Vec<_> = [hole.card1, hole.card2]
        .into_iter()
        .chain(board.all_cards())
        .map(|card| Card::new(card.rank as u8 + 2, card.suit as u8))
        .collect();
    evaluate(&cards)
}

/// Convert only for the shared wire format, after Short Deck comparison.
impl From<HandValue> for crate::eval::HandRank {
    fn from(value: HandValue) -> Self {
        use crate::eval::HandRank;
        let strength = value
            .keys
            .into_iter()
            .fold(0, |key, rank| (key << 4) | u32::from(rank));
        match value.category {
            Category::HighCard => HandRank::HighCard(strength),
            Category::OnePair => HandRank::OnePair(strength),
            Category::TwoPair => HandRank::TwoPair(strength),
            Category::ThreeOfAKind => HandRank::ThreeOfAKind(strength),
            Category::Straight => HandRank::Straight(strength),
            Category::FullHouse => HandRank::FullHouse(strength),
            Category::Flush => HandRank::Flush(strength),
            Category::FourOfAKind => HandRank::FourOfAKind(strength),
            Category::StraightFlush => HandRank::StraightFlush(u32::from(value.keys[0] - 5)),
        }
    }
}
