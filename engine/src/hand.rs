//! Hand and board card containers.

use crate::card::Card;
use serde::{Deserialize, Serialize};

/// The street (betting round) of a hand.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Street {
    /// Pre-flop: two hole cards dealt, no community cards.
    Preflop,
    /// Flop: three community cards.
    Flop,
    /// Turn: fourth community card.
    Turn,
    /// River: fifth community card.
    River,
}

impl std::fmt::Display for Street {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let s = match self {
            Street::Preflop => "preflop",
            Street::Flop => "flop",
            Street::Turn => "turn",
            Street::River => "river",
        };
        write!(f, "{s}")
    }
}

/// A player's two hole cards.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct HoleCards {
    /// First hole card.
    pub card1: Card,
    /// Second hole card.
    pub card2: Card,
}

impl HoleCards {
    /// Create hole cards from two cards.
    pub const fn new(card1: Card, card2: Card) -> Self {
        Self { card1, card2 }
    }

    /// Returns the two cards as an array.
    pub fn as_array(&self) -> [Card; 2] {
        [self.card1, self.card2]
    }
}

/// The community cards on the board, accumulated by street.
///
/// Cards are revealed progressively: flop (3 cards) → turn (1) → river (1).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct BoardCards {
    /// The three flop cards, or `None` before the flop.
    pub flop: Option<[Card; 3]>,
    /// The turn card, or `None` before the turn.
    pub turn: Option<Card>,
    /// The river card, or `None` before the river.
    pub river: Option<Card>,
}

impl BoardCards {
    /// Empty board (pre-flop state).
    pub const fn empty() -> Self {
        Self {
            flop: None,
            turn: None,
            river: None,
        }
    }

    /// How many community cards are currently on the board.
    pub fn count(&self) -> usize {
        let flop = if self.flop.is_some() { 3 } else { 0 };
        let turn = if self.turn.is_some() { 1 } else { 0 };
        let river = if self.river.is_some() { 1 } else { 0 };
        flop + turn + river
    }

    /// All revealed community cards as a `Vec`.
    pub fn all_cards(&self) -> Vec<Card> {
        let mut v = Vec::with_capacity(5);
        if let Some(flop) = self.flop {
            v.extend_from_slice(&flop);
        }
        if let Some(turn) = self.turn {
            v.push(turn);
        }
        if let Some(river) = self.river {
            v.push(river);
        }
        v
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::card::{Card, Rank, Suit};

    fn c(r: Rank, s: Suit) -> Card {
        Card::new(r, s)
    }

    #[test]
    fn empty_board_has_zero_cards() {
        let board = BoardCards::empty();
        assert_eq!(board.count(), 0);
        assert!(board.all_cards().is_empty());
    }

    #[test]
    fn flop_board_has_three_cards() {
        let board = BoardCards {
            flop: Some([
                c(Rank::Ace, Suit::Spades),
                c(Rank::King, Suit::Diamonds),
                c(Rank::Seven, Suit::Clubs),
            ]),
            turn: None,
            river: None,
        };
        assert_eq!(board.count(), 3);
        assert_eq!(board.all_cards().len(), 3);
    }

    #[test]
    fn full_board_has_five_cards() {
        let board = BoardCards {
            flop: Some([
                c(Rank::Ace, Suit::Spades),
                c(Rank::King, Suit::Diamonds),
                c(Rank::Seven, Suit::Clubs),
            ]),
            turn: Some(c(Rank::Two, Suit::Hearts)),
            river: Some(c(Rank::Nine, Suit::Diamonds)),
        };
        assert_eq!(board.count(), 5);
        assert_eq!(board.all_cards().len(), 5);
    }
}
