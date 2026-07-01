//! The [`DealingProvider`] abstraction.
//!
//! Game logic no longer reaches for a shuffle directly: it asks a
//! `DealingProvider` for a [`DealtHand`]. Two providers exist —
//! [`crate::existing::ExistingServerDealingProvider`] (the legacy
//! trusted-server shuffle, retained for rollback) and
//! [`crate::mental::MentalPokerDealingProvider`] (the untrusted-dealer
//! protocol). Selection is by feature flag — see [`DealingProviderKind`].

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

/// The output of a deal: a 52-card deck order plus an optional transcript.
#[derive(Debug, Clone)]
pub struct DealtHand {
    /// The 52 cards in final deal order. The engine consumes this via
    /// `engine::Deck::from_cards`; deck index → role layout is documented in
    /// `docs/mental-poker-dealing-refactor.md` §2.3.
    pub deck: Vec<Card>,
    /// A 256-bit reproducibility seed recorded on the `hands` row
    /// (`deck_seed_b` BYTEA — ADR-062 §2).
    pub deck_seed: DeckSeed,
    /// The signed, verifiable transcript — `None` for the legacy provider,
    /// which offers no untrusted-dealer guarantee.
    pub transcript: Option<Transcript>,
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

/// The `DEALING_PROVIDER` feature-flag values (see `docs/...refactor.md` §9).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DealingProviderKind {
    /// Legacy trusted-server shuffle. Not a Mental Poker provider.
    ExistingServer,
    /// Prefer the current interactive Mental Poker transcript mode on eligible
    /// all-human tables, falling back per hand to [`ExistingServer`].
    ///
    /// This is an operational policy, not a claim that audited server-blind
    /// crypto has landed. Successful hands still produce the current
    /// `mental_poker_mock` transcript/provider string.
    PreferMentalPoker,
    /// Mental Poker protocol with **mock** crypto. Dev only.
    MentalPokerMock,
    /// Mental Poker protocol with **generic, UNAUDITED** real crypto. Rejected
    /// everywhere by [`crate::guard_provider_allowed`] until an independent audit
    /// lands. ADR-070 does NOT un-cage this generic path — only the specific
    /// engine-blind composition ([`Self::MentalPokerEngineBlind`]).
    MentalPokerProduction,
    /// The **audited engine-blind n-of-n composition** (ADR-066/067/068; crypto
    /// in `mental-poker/src/crypto_real/`). ADR-070 P5 permits this variant in
    /// production behind the clean-Codex audit gate. **NOTE:** the live
    /// engine-blind path is NOT selected via this `DealingProviderKind` — it is
    /// routed by the per-session `engine_blind` flag (`session.rs`
    /// `engine_blind_routes_blind_coordinator`) and gated by
    /// `server::mp_dealing::resolve_mp_crypto_mode` (+ the Mock-void safety net).
    /// This variant exists so [`crate::guard_provider_allowed`] can record, as a
    /// reviewable distinction, that the audited engine-blind composition is
    /// prod-permitted while the generic [`Self::MentalPokerProduction`] stays
    /// caged. It is intentionally NOT parseable from `DEALING_PROVIDER`.
    MentalPokerEngineBlind,
}

impl DealingProviderKind {
    /// Parse the `DEALING_PROVIDER` env value. Unknown values map to `None`.
    ///
    /// `MentalPokerEngineBlind` is intentionally **NOT** parseable here: the live
    /// engine-blind path is routed by the per-session `engine_blind` flag, never
    /// by a startup `DEALING_PROVIDER=...` selection, so it must never be
    /// reachable through the env-var startup path.
    pub fn parse(value: &str) -> Option<Self> {
        match value.trim() {
            "existing_server" => Some(Self::ExistingServer),
            "mental_poker_prefer" => Some(Self::PreferMentalPoker),
            "mental_poker_mock" => Some(Self::MentalPokerMock),
            "mental_poker_production" => Some(Self::MentalPokerProduction),
            _ => None,
        }
    }

    /// The canonical flag string.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::ExistingServer => "existing_server",
            Self::PreferMentalPoker => "mental_poker_prefer",
            Self::MentalPokerMock => "mental_poker_mock",
            Self::MentalPokerProduction => "mental_poker_production",
            Self::MentalPokerEngineBlind => "mental_poker_engine_blind",
        }
    }

    /// The persisted provider for a completed transcript, if this kind uses
    /// the current interactive Mental Poker implementation.
    pub fn transcript_provider_kind(self) -> Self {
        match self {
            Self::PreferMentalPoker => Self::MentalPokerMock,
            other => other,
        }
    }

    /// `true` if this kind relies on dev-only mock crypto.
    pub fn uses_mock_crypto(self) -> bool {
        matches!(self, Self::MentalPokerMock | Self::PreferMentalPoker)
    }
}
