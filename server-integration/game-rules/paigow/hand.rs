//! Paigow holdings and payout rules for the shared banker hand.
use super::rules::{self, Tile};
use crate::banker::hand::Holding;
pub use crate::banker::hand::{HandConfig, Phase};
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
pub struct PaigowHolding {
    pub tiles: [Tile; 2],
}
pub type Hand = crate::banker::hand::Hand<PaigowHolding>;
impl Holding for PaigowHolding {
    type Card = Tile;
    const DECK: usize = rules::DECK;
    const WIDTH: usize = 2;
    const MAX_MULTIPLIER: u64 = rules::MAX_MULTIPLIER;
    fn valid(card: &Tile) -> bool {
        card.valid()
    }
    fn deal(cards: &[Tile]) -> Self {
        Self {
            tiles: std::array::from_fn(|i| cards[i]),
        }
    }
    fn payout(self, banker: Self) -> (bool, u64) {
        let value = rules::evaluate(&self.tiles);
        let bank = rules::evaluate(&banker.tiles);
        let won = value.beats_banker(bank);
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
        self.tiles.iter().map(|c| c.code()).collect()
    }
    fn value(self) -> serde_json::Value {
        serde_json::json!(rules::evaluate(&self.tiles))
    }
}
