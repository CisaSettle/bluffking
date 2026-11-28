//! Ordinary Bull ranks. A is low for both points and highest-card comparison.
use serde::{Deserialize, Serialize};

pub use crate::banker::card::Card;

/// Tens and faces count ten.
pub fn points(card: Card) -> u8 {
    card.rank.min(10)
}
/// Highest-card order: rank, then spades over hearts over clubs over diamonds.
fn card_key(card: Card) -> (u8, u8) {
    (card.rank, 3 - card.suit)
}
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct Value {
    pub bull: u8,
    pub triple: Option<[usize; 3]>,
    pub highest: Card,
}
impl Value {
    pub fn key(self) -> (u8, (u8, u8)) {
        (self.bull, card_key(self.highest))
    }
}
pub fn evaluate(cards: &[Card; 5]) -> Value {
    let mut value = evaluate_ordinary(cards);
    value.bull = if cards.iter().all(|c| c.rank < 5)
        && cards.iter().map(|c| c.rank as u16).sum::<u16>() <= 10
    {
        13
    } else if cards.iter().all(|c| c.rank >= 11) {
        12
    } else if cards
        .iter()
        .any(|c| cards.iter().filter(|d| d.rank == c.rank).count() == 4)
    {
        11
    } else {
        value.bull
    };
    if value.bull > 10 {
        value.triple = None;
    }
    // For bombs compare the four-of-a-kind rank, not an unrelated kicker.
    if value.bull == 11 {
        value.highest = *cards
            .iter()
            .filter(|c| cards.iter().filter(|d| d.rank == c.rank).count() == 4)
            .max_by_key(|c| card_key(**c))
            .unwrap();
    }
    value
}
pub fn evaluate_ordinary(cards: &[Card; 5]) -> Value {
    let mut triple = None;
    'outer: for a in 0..3 {
        for b in a + 1..4 {
            for c in b + 1..5 {
                if (points(cards[a]) + points(cards[b]) + points(cards[c])).is_multiple_of(10) {
                    triple = Some([a, b, c]);
                    break 'outer;
                }
            }
        }
    }
    let rem = cards.iter().map(|c| points(*c)).sum::<u8>() % 10;
    Value {
        bull: if triple.is_none() {
            0
        } else if rem == 0 {
            10
        } else {
            rem
        },
        triple,
        highest: *cards
            .iter()
            .max_by_key(|c| card_key(**c))
            .expect("five cards"),
    }
}
