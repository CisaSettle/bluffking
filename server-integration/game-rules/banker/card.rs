//! The standard 52-card deck the banker card games deal: rank 1 (A) to 13
//! (K), suits spades, hearts, clubs, diamonds. Each game scores it itself.
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct Card {
    pub rank: u8,
    pub suit: u8,
}
impl Card {
    pub fn code(self) -> String {
        let r = match self.rank {
            1 => 'A',
            10 => 'T',
            11 => 'J',
            12 => 'Q',
            13 => 'K',
            n => char::from(b'0' + n),
        };
        format!("{r}{}", ['s', 'h', 'c', 'd'][self.suit as usize])
    }
    /// One of the 52 real cards.
    pub fn valid(self) -> bool {
        (1..=13).contains(&self.rank) && self.suit <= 3
    }
    /// The full deck in id order: id `n` is rank `n % 13 + 1` of suit `n / 13`.
    pub fn deck() -> Vec<Self> {
        (0..52u8)
            .map(|id| Self {
                rank: id % 13 + 1,
                suit: id / 13,
            })
            .collect()
    }
}

pub fn codes(cards: &[Card]) -> Vec<String> {
    cards.iter().map(|card| card.code()).collect()
}
