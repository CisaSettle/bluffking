//! Shuffled deck of 52 cards.

use crate::card::{Card, Rank, Suit};
use crate::rng::PokerRng;
use thiserror::Error;

/// Errors that can occur when dealing from a [`Deck`].
#[derive(Debug, Clone, PartialEq, Eq, Error)]
pub enum DeckError {
    /// The deck is exhausted — all 52 cards have been dealt.
    #[error("deck is exhausted")]
    Exhausted,
}

/// A standard 52-card deck, shuffled at construction time.
///
/// Cards are dealt in order from the top. The deck cannot be reused once
/// all 52 cards are dealt; callers should construct a new [`Deck`] for each hand.
#[derive(Debug, Clone)]
pub struct Deck {
    cards: Vec<Card>,
    next: usize,
}

impl Deck {
    /// Construct a freshly shuffled deck using the provided RNG.
    pub fn new(rng: &mut PokerRng) -> Self {
        let mut cards: Vec<Card> = Rank::ALL
            .iter()
            .flat_map(|&rank| Suit::ALL.iter().map(move |&suit| Card::new(rank, suit)))
            .collect();

        // Fisher–Yates shuffle using the engine's RNG.
        let n = cards.len();
        for i in (1..n).rev() {
            let j = rng_range(rng, i + 1);
            cards.swap(i, j);
        }

        Self { cards, next: 0 }
    }

    /// Construct a deck from an explicit, externally-determined card order.
    ///
    /// Used by the dealing abstraction (`mental-poker` crate): a
    /// `DealingProvider` produces the 52-card order — legacy server shuffle or
    /// a Mental Poker protocol run — and the engine consumes it here instead of
    /// shuffling itself. The first card dealt is `cards[0]`.
    ///
    /// The caller is responsible for passing a valid 52-card deck; the engine
    /// does not re-validate uniqueness (the provider, and — for Mental Poker —
    /// the transcript verifier, are responsible for that).
    pub fn from_cards(cards: Vec<Card>) -> Self {
        Self { cards, next: 0 }
    }

    /// The full deck in deal order (dealt and undealt cards alike).
    ///
    /// Lets a `DealingProvider` read back the order it produced; also used to
    /// snapshot a legacy shuffle into a `Vec<Card>`.
    pub fn cards(&self) -> &[Card] {
        &self.cards
    }

    /// Deal the top card from the deck.
    ///
    /// Returns `Err(DeckError::Exhausted)` after all 52 cards have been dealt.
    pub fn deal(&mut self) -> Result<Card, DeckError> {
        if self.next >= self.cards.len() {
            return Err(DeckError::Exhausted);
        }
        let card = self.cards[self.next];
        self.next += 1;
        Ok(card)
    }

    /// Number of cards remaining in the deck.
    pub fn remaining(&self) -> usize {
        self.cards.len() - self.next
    }
}

/// Generates a uniform random integer in `[0, n)` using the engine RNG.
fn rng_range(rng: &mut PokerRng, n: usize) -> usize {
    // Use rejection sampling to avoid modulo bias.
    // For n up to 52 the loop terminates quickly.
    if n <= 1 {
        return 0;
    }
    // Reject values in the "overhang" to avoid bias (loop-invariant in `n`).
    let threshold = u64::MAX - (u64::MAX % n as u64);
    let mut buf = [0u8; 8];
    loop {
        rng.fill_bytes(&mut buf);
        let v = u64::from_le_bytes(buf);
        if v < threshold {
            return (v % n as u64) as usize;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashSet;

    #[test]
    fn new_produces_52_unique_cards() {
        let mut rng = PokerRng::from_seed(42);
        let mut deck = Deck::new(&mut rng);
        let mut seen = HashSet::new();
        for _ in 0..52 {
            let card = deck.deal().expect("should have 52 cards");
            assert!(seen.insert(card), "duplicate card: {card}");
        }
        assert_eq!(seen.len(), 52);
    }

    #[test]
    fn deal_returns_err_after_52() {
        let mut rng = PokerRng::from_seed(0);
        let mut deck = Deck::new(&mut rng);
        for _ in 0..52 {
            deck.deal().unwrap();
        }
        assert_eq!(deck.deal(), Err(DeckError::Exhausted));
    }

    #[test]
    fn remaining_decrements() {
        let mut rng = PokerRng::from_seed(1);
        let mut deck = Deck::new(&mut rng);
        assert_eq!(deck.remaining(), 52);
        deck.deal().unwrap();
        assert_eq!(deck.remaining(), 51);
    }

    #[test]
    fn deterministic_from_seed() {
        let mut rng_a = PokerRng::from_seed(999);
        let mut rng_b = PokerRng::from_seed(999);
        let card_a = Deck::new(&mut rng_a).deal().unwrap();
        let card_b = Deck::new(&mut rng_b).deal().unwrap();
        assert_eq!(card_a, card_b);
    }
}
