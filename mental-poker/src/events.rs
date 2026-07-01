//! Typed transcript event payloads.
//!
//! Each [`crate::transcript::TranscriptEvent`] carries an `event_type` string
//! and a JSON `payload`. The structs here are the typed view of those payloads;
//! the builder serializes from them and the verifier deserializes back into
//! them.

use crate::crypto::{DecryptionProof, ShuffleProof};
use serde::{Deserialize, Serialize};
use serde_json::Value;

/// Event type identifiers. One per dealing-protocol step.
pub mod event_type {
    /// Hand announced by the coordinator.
    pub const HAND_INIT: &str = "hand_init";
    /// A party published its keys.
    pub const KEY_REGISTERED: &str = "key_registered";
    /// A party submitted a re-encryption shuffle.
    pub const SHUFFLE_CONTRIBUTION: &str = "shuffle_contribution";
    /// The coordinator froze the final encrypted deck.
    pub const FINAL_DECK_COMMITTED: &str = "final_deck_committed";
    /// A party signed an acknowledgement of the final deck hash.
    pub const FINAL_DECK_ACK: &str = "final_deck_ack";
    /// A hole card was opened to its owner.
    pub const HOLE_CARD_OPENED: &str = "hole_card_opened";
    /// Community cards were jointly revealed for a street.
    pub const COMMUNITY_REVEALED: &str = "community_revealed";
    /// The hand was aborted with evidence.
    pub const HAND_ABORTED: &str = "hand_aborted";
    /// Terminal event — closes the transcript.
    pub const HAND_COMPLETE: &str = "hand_complete";
}

/// `coordinator` signer id — the untrusted relay.
pub const COORDINATOR: &str = "coordinator";

/// Party id for the player in `seat`.
pub fn party_id(seat: u8) -> String {
    format!("party:{seat}")
}

/// One seat ↔ party mapping declared at hand init.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PlayerEntry {
    /// Seat index.
    pub seat: u8,
    /// Party id participating from this seat.
    pub party_id: String,
}

/// `hand_init` payload.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct HandInitPayload {
    /// All seats in the hand, in seat order.
    pub players: Vec<PlayerEntry>,
    /// Dealer button seat.
    pub button_seat: u8,
    /// Big blind amount.
    pub big_blind: u64,
    /// Small blind amount.
    pub small_blind: u64,
    /// ADR-041 §5.1 — canonical starting-deck representation.
    ///
    /// `Some("wire")` selects `wire_deck_hash([0..51])` as the round-0
    /// `input_deck_hash` seed; this is the interactive (real-client) path.
    /// `None` (absent from JSON) selects `canonical_initial_deck_hash()` —
    /// the Phase-1 coordinator-simulated path in `mental.rs`. Absent from
    /// transcripts produced before this field was added → backward compatible.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub deck_repr: Option<String>,
}

/// `key_registered` payload.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct KeyRegisteredPayload {
    /// Registering party.
    pub party_id: String,
    /// Seat of the registering party.
    pub seat: u8,
    /// Hex-encoded signing verification key.
    pub signing_pubkey: String,
    /// Hex-encoded shuffle/encryption public key.
    pub shuffle_pubkey: String,
    /// ADR-041 §4 contributor layer. Party id of the contributor (= party_id).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub contributor: Option<String>,
    /// ADR-041 §4 contributor signature over the canonical claim for key_registered.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub contributor_signature: Option<String>,
    /// F2 (DKG rogue-key defense) — a **party-bound Schnorr proof of knowledge**
    /// of the discrete log of `shuffle_pubkey` (`Q_i = x_i·G`). The real
    /// re-encryption-shuffle verifier reconstructs the joint key as `Σ Q_i` and
    /// binds every shuffle proof to it. Without a PoK gate, a malicious
    /// registrant could set `Q_rogue = a·G − Σ Q_honest` so the joint key sums to
    /// `a·G` (a key it controls the secret of) and then decrypt every card —
    /// total server-blindness collapse (audit F2, re-audit round 1). The
    /// verifier therefore sums **only** keys carrying a valid party-bound PoK; a
    /// rogue cannot produce one because it does not know `log_G(Q_rogue)`.
    ///
    /// Wire form is the DKG [`SchnorrPok`](crate::crypto_real::dkg::SchnorrPok)
    /// `{"r","s"}`. Absent (`None`) for the dev-only mock-shuffle path, whose
    /// `shuffle_pubkey` is a hash, not a curve point (there is no discrete log to
    /// prove). The real-shuffle verifier REQUIRES it; the mock verifier ignores
    /// it. Backward-compatible: omitted from JSON when `None`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub key_pok: Option<crate::crypto_real::dkg::SchnorrPok>,
}

/// `shuffle_contribution` payload.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ShuffleContributionPayload {
    /// Party performing this shuffle.
    pub party_id: String,
    /// Zero-based shuffle round.
    pub round: u32,
    /// Hex hash of the input deck.
    pub input_deck_hash: String,
    /// Hex hash of the output deck.
    pub output_deck_hash: String,
    /// Verifiable shuffle proof.
    pub proof: ShuffleProof,
    /// ADR-041 §4 contributor layer. Party id of the contributor (= party_id).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub contributor: Option<String>,
    /// ADR-041 §4 contributor signature over the canonical claim for shuffle_contribution.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub contributor_signature: Option<String>,
}

/// `final_deck_committed` payload.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FinalDeckCommittedPayload {
    /// Hex hash of the final encrypted deck.
    pub final_deck_hash: String,
    /// The 52 final per-card commitments (hex). Hiding — no plaintext.
    pub deck: Vec<String>,
    /// F3 (mp-phase4 audit) — the committed 52-entry CIPHERTEXT deck, the exact
    /// ElGamal `(C1, C2)` per index the parties froze + acknowledged.
    ///
    /// Why this is here and not only in the shuffle proof: the offline verifier
    /// anchors every threshold open to `deck_ct[deck_index]` whenever the
    /// decryption scheme is `cp-threshold-ristretto-v1`, so a prover cannot open a
    /// ciphertext other than the one this `final_deck_committed` step pinned. The
    /// real re-encryption shuffle ALSO carries the deck in its last proof's
    /// attestation, but a transcript may legitimately pair the real threshold
    /// decryption with a non-real shuffle (a mixed mode the audit requires we not
    /// regress on); binding the ciphertext deck HERE — on the same event the
    /// parties sign via `final_deck_ack` — makes the anchor available to the
    /// verifier regardless of the shuffle scheme.
    ///
    /// `None` (omitted from JSON) for the dev-only mock-decryption path, which has
    /// no ciphertext deck. Additive + backward compatible: absent from transcripts
    /// produced before this field existed.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub deck_ct: Option<Vec<crate::crypto_real::ec::CtWire>>,
}

/// `final_deck_ack` payload.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FinalDeckAckPayload {
    /// Acknowledging party.
    pub party_id: String,
    /// The final deck hash this party agrees to.
    pub final_deck_hash: String,
    /// ADR-041 §4 contributor layer. Party id of the contributor (= party_id).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub contributor: Option<String>,
    /// ADR-041 §4 contributor signature over the canonical claim for final_deck_ack.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub contributor_signature: Option<String>,
}

/// One opened card with its decryption witness.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct OpenedCard {
    /// Index into the final deck (`0..=51`).
    pub deck_index: u32,
    /// Revealed card id (`0..=51`).
    pub card_id: u8,
    /// Hex-encoded 32-byte commitment salt.
    pub salt: String,
    /// Verifiable decryption proof.
    pub proof: DecryptionProof,
}

/// `hole_card_opened` payload.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct HoleCardOpenedPayload {
    /// Seat this hole card belongs to.
    pub seat: u8,
    /// Party that owns (and opened) the card.
    pub owner_party_id: String,
    /// The opened card.
    pub card: OpenedCard,
    /// ADR-041 §4 contributor layer. Party id of the contributor (= owner_party_id).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub contributor: Option<String>,
    /// ADR-041 §4 contributor signature over the canonical claim for hole_card_opened.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub contributor_signature: Option<String>,
}

/// `community_revealed` payload.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CommunityRevealedPayload {
    /// `flop`, `turn`, or `river`.
    pub stage: String,
    /// The opened community cards for this stage.
    pub cards: Vec<OpenedCard>,
}

/// `hand_aborted` payload.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct HandAbortedPayload {
    /// Party (or coordinator) that raised the abort.
    pub aborted_by: String,
    /// Human-readable reason.
    pub reason: String,
    /// Machine-checkable evidence (scheme-specific).
    pub evidence: Value,
}

/// `hand_complete` payload.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct HandCompletePayload {
    /// Total number of cards revealed across the hand.
    pub revealed_card_count: u32,
}
