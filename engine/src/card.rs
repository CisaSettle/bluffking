//! Card model: [`Suit`], [`Rank`], [`Card`].

use serde::{Deserialize, Serialize};
use std::fmt;

/// The four suits of a standard deck.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Suit {
    /// ♠ Spades
    Spades,
    /// ♥ Hearts
    Hearts,
    /// ♦ Diamonds
    Diamonds,
    /// ♣ Clubs
    Clubs,
}

impl Suit {
    /// All four suits in a canonical order.
    pub const ALL: [Suit; 4] = [Suit::Spades, Suit::Hearts, Suit::Diamonds, Suit::Clubs];

    /// Single-character abbreviation used in Display and string parsing.
    pub fn char(self) -> char {
        match self {
            Suit::Spades => 's',
            Suit::Hearts => 'h',
            Suit::Diamonds => 'd',
            Suit::Clubs => 'c',
        }
    }
}

impl fmt::Display for Suit {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.char())
    }
}

/// Card rank, 2 through Ace.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub enum Rank {
    Two,
    Three,
    Four,
    Five,
    Six,
    Seven,
    Eight,
    Nine,
    Ten,
    Jack,
    Queen,
    King,
    Ace,
}

impl Rank {
    /// All ranks in ascending order.
    pub const ALL: [Rank; 13] = [
        Rank::Two,
        Rank::Three,
        Rank::Four,
        Rank::Five,
        Rank::Six,
        Rank::Seven,
        Rank::Eight,
        Rank::Nine,
        Rank::Ten,
        Rank::Jack,
        Rank::Queen,
        Rank::King,
        Rank::Ace,
    ];

    /// Parse the canonical uppercase rank abbreviation.
    pub fn from_char(ch: char) -> Option<Self> {
        Self::ALL.into_iter().find(|rank| rank.char() == ch)
    }

    /// Single-character abbreviation.
    pub fn char(self) -> char {
        match self {
            Rank::Two => '2',
            Rank::Three => '3',
            Rank::Four => '4',
            Rank::Five => '5',
            Rank::Six => '6',
            Rank::Seven => '7',
            Rank::Eight => '8',
            Rank::Nine => '9',
            Rank::Ten => 'T',
            Rank::Jack => 'J',
            Rank::Queen => 'Q',
            Rank::King => 'K',
            Rank::Ace => 'A',
        }
    }
}

impl fmt::Display for Rank {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.char())
    }
}

/// A single playing card.
///
/// # Display
/// Cards display as rank + suit, e.g. `"As"` for Ace of Spades,
/// `"Th"` for Ten of Hearts.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct Card {
    /// The card's rank.
    pub rank: Rank,
    /// The card's suit.
    pub suit: Suit,
}

impl Card {
    /// Create a new card.
    pub const fn new(rank: Rank, suit: Suit) -> Self {
        Self { rank, suit }
    }
}

impl fmt::Display for Card {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}{}", self.rank.char(), self.suit.char())
    }
}

impl Serialize for Card {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.collect_str(self)
    }
}

impl<'de> Deserialize<'de> for Card {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let s = String::deserialize(deserializer)?;
        parse_card(&s).ok_or_else(|| serde::de::Error::custom(format!("invalid card: {s}")))
    }
}

/// Parse a card from its 2-character string representation (e.g. "As", "Th").
pub fn parse_card(s: &str) -> Option<Card> {
    let mut chars = s.chars();
    let rank_ch = chars.next()?;
    let suit_ch = chars.next()?;
    if chars.next().is_some() {
        return None; // too many chars
    }
    let rank = Rank::from_char(rank_ch.to_ascii_uppercase())?;
    let suit = match suit_ch {
        's' | 'S' => Suit::Spades,
        'h' | 'H' => Suit::Hearts,
        'd' | 'D' => Suit::Diamonds,
        'c' | 'C' => Suit::Clubs,
        _ => return None,
    };
    Some(Card::new(rank, suit))
}

/// Parse a canonical 169-grid hand class: AA, AKs, KQo (high rank first).
/// Pairs carry suited=false. Invalid ranks, ordering or suffixes are rejected.
pub fn parse_hand_class(token: &str) -> Option<(Rank, Rank, bool)> {
    let b = token.as_bytes();
    let high = Rank::from_char(*b.first()? as char)?;
    let low = Rank::from_char(*b.get(1)? as char)?;
    match (b.len(), b.get(2)) {
        (2, None) if high == low => Some((high, low, false)),
        (3, Some(b's')) if high > low => Some((high, low, true)),
        (3, Some(b'o')) if high > low => Some((high, low, false)),
        _ => None,
    }
}

/// Check uniqueness in a standard 52-card deck without allocating.
pub fn cards_unique(cards: impl IntoIterator<Item = Card>) -> bool {
    let mut seen = 0u64;
    for card in cards {
        let bit = 1u64 << (card.rank as u8 * 4 + card.suit as u8);
        if seen & bit != 0 {
            return false;
        }
        seen |= bit;
    }
    true
}
