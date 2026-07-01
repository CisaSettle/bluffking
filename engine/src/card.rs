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
        serializer.serialize_str(&self.to_string())
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
    let rank = match rank_ch {
        '2' => Rank::Two,
        '3' => Rank::Three,
        '4' => Rank::Four,
        '5' => Rank::Five,
        '6' => Rank::Six,
        '7' => Rank::Seven,
        '8' => Rank::Eight,
        '9' => Rank::Nine,
        'T' | 't' => Rank::Ten,
        'J' | 'j' => Rank::Jack,
        'Q' | 'q' => Rank::Queen,
        'K' | 'k' => Rank::King,
        'A' | 'a' => Rank::Ace,
        _ => return None,
    };
    let suit = match suit_ch {
        's' | 'S' => Suit::Spades,
        'h' | 'H' => Suit::Hearts,
        'd' | 'D' => Suit::Diamonds,
        'c' | 'C' => Suit::Clubs,
        _ => return None,
    };
    Some(Card::new(rank, suit))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn display_ace_spades() {
        assert_eq!(Card::new(Rank::Ace, Suit::Spades).to_string(), "As");
    }

    #[test]
    fn display_ten_hearts() {
        assert_eq!(Card::new(Rank::Ten, Suit::Hearts).to_string(), "Th");
    }

    #[test]
    fn parse_round_trip() {
        let card = Card::new(Rank::King, Suit::Diamonds);
        let s = card.to_string();
        let parsed = parse_card(&s).expect("parse_card failed");
        assert_eq!(card, parsed);
    }

    #[test]
    fn parse_invalid_returns_none() {
        assert!(parse_card("XY").is_none());
        assert!(parse_card("").is_none());
        assert!(parse_card("Ash").is_none());
    }

    #[test]
    fn serde_round_trip() {
        let card = Card::new(Rank::Queen, Suit::Clubs);
        let json = serde_json::to_string(&card).unwrap();
        let back: Card = serde_json::from_str(&json).unwrap();
        assert_eq!(card, back);
    }
}
