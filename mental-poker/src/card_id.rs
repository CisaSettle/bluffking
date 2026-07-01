//! Canonical card identity for the Mental Poker protocol.
//!
//! A [`CardId`] is an integer `0..=51`. The mapping is stable and
//! domain-independent so it can appear in commitments and the transcript:
//!
//! ```text
//! card_id = rank_index * 4 + suit_index
//! rank_index : 0..=12  in Rank::ALL order (Two .. Ace)
//! suit_index : 0..=3   in Suit::ALL order (Spades, Hearts, Diamonds, Clubs)
//! ```

use engine::{Card, Rank, Suit};

/// A canonical card identifier in the range `0..=51`.
pub type CardId = u8;

/// Number of cards in a standard deck.
pub const DECK_SIZE: usize = 52;

/// Convert an engine [`Card`] to its canonical [`CardId`].
pub fn card_to_id(card: Card) -> CardId {
    let rank_index = Rank::ALL.iter().position(|&r| r == card.rank).unwrap_or(0);
    let suit_index = Suit::ALL.iter().position(|&s| s == card.suit).unwrap_or(0);
    (rank_index * 4 + suit_index) as CardId
}

/// Convert a canonical [`CardId`] back to an engine [`Card`].
///
/// Returns `None` if `id >= 52`.
pub fn id_to_card(id: CardId) -> Option<Card> {
    if (id as usize) >= DECK_SIZE {
        return None;
    }
    let rank = Rank::ALL[(id / 4) as usize];
    let suit = Suit::ALL[(id % 4) as usize];
    Some(Card::new(rank, suit))
}

/// `true` if `id` is a valid card identifier (`0..=51`).
pub fn is_valid_card_id(id: u32) -> bool {
    id < DECK_SIZE as u32
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn round_trip_all_52() {
        for id in 0..52u8 {
            let card = id_to_card(id).expect("valid id");
            assert_eq!(card_to_id(card), id, "round trip failed for id {id}");
        }
    }

    #[test]
    fn ids_are_unique_and_dense() {
        let mut seen = [false; 52];
        for id in 0..52u8 {
            let card = id_to_card(id).unwrap();
            let back = card_to_id(card) as usize;
            assert!(!seen[back], "duplicate id {back}");
            seen[back] = true;
        }
        assert!(seen.iter().all(|&b| b));
    }

    #[test]
    fn out_of_range_rejected() {
        assert!(id_to_card(52).is_none());
        assert!(id_to_card(255).is_none());
        assert!(!is_valid_card_id(52));
        assert!(is_valid_card_id(51));
    }
}
