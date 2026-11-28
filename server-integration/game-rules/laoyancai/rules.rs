//! One 52-card deck. Points decide wins; the winning hand supplies the multiplier.
use serde::{Deserialize, Serialize};

use crate::banker::card::Card;

/// Tens and faces count zero.
pub fn points(card: Card) -> u8 {
    if card.rank >= 10 {
        0
    } else {
        card.rank
    }
}
#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
pub struct Value {
    pub points: u8,
    pub multiplier: u64,
    pub natural: bool,
}
pub fn evaluate(cards: &[Card]) -> Value {
    let points = cards.iter().map(|c| points(*c)).sum::<u8>() % 10;
    let natural = cards.len() == 2 && points >= 8;
    let same_suit = cards.iter().all(|c| c.suit == cards[0].suit);
    let multiplier = if cards.len() == 3 && cards.iter().all(|c| c.rank == cards[0].rank) {
        5
    } else if cards.len() == 3 && same_suit {
        3
    } else if natural && same_suit {
        2
    } else {
        1
    };
    Value {
        points,
        multiplier,
        natural,
    }
}
