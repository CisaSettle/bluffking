//! 炸金花 card rules: deck, hand categories and pairwise comparison.
//!
//! Ranking (high → low): 豹子 > 顺金 > 金花 > 顺子 > 对子 > 单张.
//! Q-K-A is the highest straight, A-2-3 second, 2-3-4 lowest. Suits never break ties.
//! Off-suit 2-3-5 beats 豹子 and loses to everything else.
//!
//! 赖子 (optional, chosen at room creation): every card of one rank 2..A, the
//! two jokers added to the deck, or both are wild ([`Wild`]). A 赖子 stands
//! for whichever card not already in the hand makes the best hand. A hand
//! holding a 赖子 loses to an otherwise equal natural hand.

use std::cmp::Ordering;

use serde::Serialize;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, serde::Serialize, serde::Deserialize)]
pub struct Card {
    /// 2..=14 (14 = A); [`SMALL_JOKER_RANK`] / [`BIG_JOKER_RANK`] for the
    /// jokers of a 大小王 room.
    pub rank: u8,
    /// 0..=3 → ♠ ♥ ♣ ♦.
    pub suit: u8,
}

pub const SMALL_JOKER_RANK: u8 = 15;
pub const BIG_JOKER_RANK: u8 = 16;

impl Card {
    pub const SMALL_JOKER: Card = Card::new(SMALL_JOKER_RANK, 0);
    pub const BIG_JOKER: Card = Card::new(BIG_JOKER_RANK, 0);

    pub const fn new(rank: u8, suit: u8) -> Self {
        Self { rank, suit }
    }

    pub fn is_joker(self) -> bool {
        self.rank >= SMALL_JOKER_RANK
    }

    /// Wire form: rank char + suit char, e.g. `"As"`, `"Th"`, `"2d"`; the
    /// jokers are `"SJ"` (small) and `"BJ"` (big), as in 斗地主.
    pub fn code(self) -> String {
        match self.rank {
            SMALL_JOKER_RANK => return "SJ".into(),
            BIG_JOKER_RANK => return "BJ".into(),
            _ => {}
        }
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

/// Suit-major (♠ ♥ ♣ ♦), ranks 2..A. Only the bot's equity tables and 赖子
/// substitution use this order; dealt cards come from the mental-poker deck.
pub fn full_deck() -> Vec<Card> {
    let mut deck = Vec::with_capacity(54);
    for suit in 0..4 {
        for rank in 2..=14 {
            deck.push(Card::new(rank, suit));
        }
    }
    deck
}

/// The room's deck: [`full_deck`] plus the small and big joker in a 大小王 room.
pub fn room_deck(wild: Wild) -> Vec<Card> {
    let mut deck = full_deck();
    if wild.jokers {
        deck.extend([Card::SMALL_JOKER, Card::BIG_JOKER]);
    }
    deck
}

/// A room's 赖子 setting, fixed at creation.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Wild {
    /// Every card of this rank (2..=14) is a 赖子.
    pub rank: Option<u8>,
    /// The deck holds the two jokers, both 赖子.
    pub jokers: bool,
}

impl Wild {
    pub fn is_wild(self, card: Card) -> bool {
        card.is_joker() || self.rank == Some(card.rank)
    }
    pub fn any(self) -> bool {
        self.jokers || self.rank.is_some()
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Category {
    HighCard,
    Pair,
    Straight,
    Flush,
    StraightFlush,
    Leopard,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct HandValue {
    pub category: Category,
    /// Tie-break keys, most significant first.
    pub keys: [u8; 3],
    /// Off-suit 2-3-5 (only meaningful against a 豹子).
    pub is_235: bool,
    /// Made with at least one 赖子. Hands checkpointed before 赖子 existed
    /// decode as natural, which they are.
    #[serde(default)]
    pub wild: bool,
}

impl HandValue {
    // Keep stored keys stable for existing checkpoints and bot lookup tables.
    // Both straight categories use QKA > A23 > JQK > ... > 234.
    fn comparison_keys(&self) -> [u8; 3] {
        if matches!(self.category, Category::Straight | Category::StraightFlush) {
            match self.keys[0] {
                14 => [15, 0, 0],
                3 => [14, 0, 0],
                _ => self.keys,
            }
        } else {
            self.keys
        }
    }

    pub(super) fn base_cmp(&self, other: &Self) -> Ordering {
        self.category
            .cmp(&other.category)
            .then_with(|| self.comparison_keys().cmp(&other.comparison_keys()))
            // Natural beats 赖子 when category and ranks tie.
            .then_with(|| other.wild.cmp(&self.wild))
    }
}

/// Best value of `cards` under the room's 赖子 setting. A joker is always a
/// 赖子; it only exists in a 大小王 room's deck.
pub fn evaluate_with(cards: &[Card; 3], wild: Wild) -> HandValue {
    if !wild.any() {
        return evaluate(cards);
    }
    let natural: Vec<Card> = cards.iter().copied().filter(|c| !wild.is_wild(*c)).collect();
    let leopard = |rank| HandValue {
        category: Category::Leopard,
        keys: [rank, 0, 0],
        is_235: false,
        wild: true,
    };
    match natural.as_slice() {
        [_, _, _] => evaluate(cards),
        [a, b] => full_deck()
            .into_iter()
            .filter(|c| c != a && c != b)
            .map(|c| evaluate(&[*a, *b, c]))
            .max_by(HandValue::base_cmp)
            .map(|v| HandValue { wild: true, ..v })
            .expect("deck has spare cards"),
        // Two 赖子 always complete the natural card's 豹子; three make 豹子 A.
        [a] => leopard(a.rank),
        _ => leopard(14),
    }
}

pub fn evaluate(cards: &[Card; 3]) -> HandValue {
    let mut ranks = [cards[0].rank, cards[1].rank, cards[2].rank];
    ranks.sort_unstable_by(|a, b| b.cmp(a));
    let flush = cards[0].suit == cards[1].suit && cards[1].suit == cards[2].suit;

    if ranks[0] == ranks[2] {
        return HandValue {
            category: Category::Leopard,
            keys: [ranks[0], 0, 0],
            is_235: false,
            wild: false,
        };
    }
    let straight_high = if ranks[0] == ranks[1] + 1 && ranks[1] == ranks[2] + 1 {
        Some(ranks[0])
    } else if ranks == [14, 3, 2] {
        Some(3)
    } else {
        None
    };
    if let Some(high) = straight_high {
        return HandValue {
            category: if flush {
                Category::StraightFlush
            } else {
                Category::Straight
            },
            keys: [high, 0, 0],
            is_235: false,
            wild: false,
        };
    }
    if flush {
        return HandValue {
            category: Category::Flush,
            keys: ranks,
            is_235: false,
            wild: false,
        };
    }
    if ranks[0] == ranks[1] || ranks[1] == ranks[2] {
        let (pair, kicker) = if ranks[0] == ranks[1] {
            (ranks[0], ranks[2])
        } else {
            (ranks[1], ranks[0])
        };
        return HandValue {
            category: Category::Pair,
            keys: [pair, kicker, 0],
            is_235: false,
            wild: false,
        };
    }
    HandValue {
        category: Category::HighCard,
        keys: ranks,
        is_235: ranks == [5, 3, 2],
        wild: false,
    }
}

/// Head-to-head comparison including the 235-beats-豹子 rule.
pub fn compare(a: &HandValue, b: &HandValue) -> Ordering {
    if a.is_235 && b.category == Category::Leopard {
        return Ordering::Greater;
    }
    if b.is_235 && a.category == Category::Leopard {
        return Ordering::Less;
    }
    a.base_cmp(b)
}

/// Indices (into `hands`) that win a multi-way showdown; more than one = split.
///
/// A player wins when nobody beats them head-to-head. The 235 rule can make
/// the relation cyclic (235 > 豹子 > pair > 235); when that leaves nobody
/// unbeaten the showdown falls back to plain ranking without the 235 rule.
pub fn showdown_winners(hands: &[HandValue]) -> Vec<usize> {
    let unbeaten = |cmp: &dyn Fn(&HandValue, &HandValue) -> Ordering| -> Vec<usize> {
        (0..hands.len())
            .filter(|&i| {
                (0..hands.len()).all(|j| i == j || cmp(&hands[j], &hands[i]) != Ordering::Greater)
            })
            .collect()
    };
    let winners = unbeaten(&compare);
    if winners.is_empty() {
        unbeaten(&HandValue::base_cmp)
    } else {
        winners
    }
}
