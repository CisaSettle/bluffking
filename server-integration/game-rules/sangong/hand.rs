//! Sangong holdings and payout rules for the shared banker hand.
use super::rules::{self, Card};
use crate::banker::hand::Holding;
pub use crate::banker::hand::{HandConfig, Phase};
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
pub struct SangongHolding {
    pub cards: [Card; 3],
}
pub type Hand = crate::banker::hand::Hand<SangongHolding>;
impl Holding for SangongHolding {
    type Card = Card;
    const DECK: usize = 52;
    const WIDTH: usize = 3;
    const MAX_MULTIPLIER: u64 = 9;
    fn valid(card: &Card) -> bool {
        (1..=13).contains(&card.rank) && card.suit <= 3
    }
    fn deal(cards: &[Card]) -> Self {
        Self {
            cards: std::array::from_fn(|i| cards[i]),
        }
    }
    fn payout(self, banker: Self) -> (bool, u64) {
        let value = rules::evaluate(&self.cards);
        let bank = rules::evaluate(&banker.cards);
        let won = value.key() > bank.key();
        (
            won,
            if won {
                value.multiplier()
            } else {
                bank.multiplier()
            },
        )
    }
    fn codes(self) -> Vec<String> {
        self.cards.iter().map(|c| c.code()).collect()
    }
    fn value(self) -> serde_json::Value {
        serde_json::json!(rules::evaluate(&self.cards))
    }
}
