//! Offline protocol dealing interface used by the transcript CLI.

use crate::transcript::Transcript;
use engine::{Card, DeckSeed};

/// Everything a provider needs to deal one hand.
#[derive(Debug, Clone)]
pub struct DealRequest {
    /// Hand id (UUID string).
    pub hand_id: String,
    /// Table / room id.
    pub table_id: String,
    /// Number of seated players.
    pub num_players: u8,
    /// Dealer button seat.
    pub button_seat: u8,
    /// Big blind amount.
    pub big_blind: u64,
    /// Small blind amount.
    pub small_blind: u64,
}

/// The output of a protocol deal: a 52-card deck order and signed transcript.
#[derive(Debug, Clone)]
pub struct DealtHand {
    /// The 52 cards in final deal order. The engine consumes this via
    /// `engine::Deck::from_cards`; [`crate::state::ProtocolState`] tracks each
    /// deck index's reveal role.
    pub deck: Vec<Card>,
    /// A 256-bit reproducibility seed recorded on the `hands` row
    /// (`deck_seed_b` BYTEA — ADR-062 §2).
    pub deck_seed: DeckSeed,
    /// The signed, verifiable transcript.
    pub transcript: Transcript,
}

/// Abstraction over how a hand's deck is produced.
pub trait DealingProvider {
    /// Stable provider identifier (matches the feature-flag value).
    fn name(&self) -> &'static str;

    /// `true` if this provider produces a verifiable transcript.
    fn is_verifiable(&self) -> bool;

    /// Deal one hand.
    fn deal(&self, request: &DealRequest) -> DealtHand;
}
