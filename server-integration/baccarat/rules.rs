pub use draw_games::baccarat::rules::*;
use rand::{rngs::OsRng, seq::SliceRandom};

pub fn shoe() -> Vec<u8> {
    let mut cards: Vec<u8> = (0..8).flat_map(|_| 0..52).collect();
    cards.shuffle(&mut OsRng);
    // Burn the exposed first card and its rank's count (faces count as ten).
    if let Some(card) = cards.pop() {
        let burn = (card % 13 + 1).min(10) as usize;
        cards.truncate(cards.len() - burn);
    }
    cards
}
