//! Legacy dealing provider — the current trusted-server shuffle.
//!
//! `ExistingServerDealingProvider` wraps the engine's existing
//! `PokerRng` + Fisher–Yates shuffle behind the [`DealingProvider`] trait so
//! that game logic depends on the *abstraction* rather than on the server RNG
//! directly. It is retained as the default and as the rollback path.
//!
//! **It is not a Mental Poker provider.** The server is still the trusted
//! dealer: it picks the seed, knows every card, and is the sole authority.
//! `deal()` therefore returns `transcript: None` — there is nothing to verify.

use crate::provider::{DealRequest, DealingProvider, DealtHand};
use engine::{Deck, PokerRng};

/// Legacy trusted-server dealing provider. See module docs.
#[derive(Debug, Clone, Default)]
pub struct ExistingServerDealingProvider;

impl ExistingServerDealingProvider {
    /// Construct the provider.
    pub fn new() -> Self {
        Self
    }
}

impl DealingProvider for ExistingServerDealingProvider {
    fn name(&self) -> &'static str {
        "existing_server"
    }

    fn is_verifiable(&self) -> bool {
        false
    }

    fn deal(&self, _request: &DealRequest) -> DealtHand {
        // Identical shuffle to the legacy path: an OS-seeded ChaCha20 RNG
        // driving the engine's Fisher–Yates shuffle.
        let mut rng = PokerRng::from_os();
        let deck_seed = rng.seed();
        let deck = Deck::new(&mut rng);
        DealtHand {
            deck: deck.cards().to_vec(),
            deck_seed,
            transcript: None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashSet;

    fn req() -> DealRequest {
        DealRequest {
            hand_id: "h".into(),
            table_id: "t".into(),
            num_players: 2,
            button_seat: 0,
            big_blind: 20,
            small_blind: 10,
        }
    }

    #[test]
    fn deals_52_unique_cards_and_no_transcript() {
        let dealt = ExistingServerDealingProvider::new().deal(&req());
        assert_eq!(dealt.deck.len(), 52);
        let unique: HashSet<_> = dealt.deck.iter().copied().collect();
        assert_eq!(unique.len(), 52, "all 52 cards must be distinct");
        assert!(
            dealt.transcript.is_none(),
            "legacy provider has no transcript"
        );
    }
}
