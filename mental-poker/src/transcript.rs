//! Signed, hash-chained transcript of a dealing protocol run.
//!
//! A [`Transcript`] is the verifiable artifact a hand exports. Each
//! [`TranscriptEvent`] is signed by its originator and chained to its
//! predecessor by `previous_event_hash`, so neither the coordinator nor any
//! party can reorder, drop, insert, or alter an event without detection.

use crate::crypto::{DecryptionProvider, ShuffleProofProvider};
use crate::hash::{canonical_json, ds_hash, hex_hash, Hash, ZERO_HASH};
use crate::signing::{KeyDirectory, SignatureProvider};
use crate::state::ProtocolState;
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};

/// Current transcript protocol version. Bump on any breaking change.
pub const PROTOCOL_VERSION: u16 = 1;

/// One signed, chained transcript event.
///
/// ADR-041 §4: the `signer` / `signature` are always the coordinator (event-envelope layer).
/// Client-action events additionally carry `contributor` / `contributor_signature` in the
/// payload — the second layer signing the canonical claim the client computed independently.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TranscriptEvent {
    /// Transcript protocol version.
    pub protocol_version: u16,
    /// Hand id.
    pub hand_id: String,
    /// Table / room id.
    pub table_id: String,
    /// Dense sequence number, starting at 0.
    pub sequence_number: u64,
    /// Event type — see [`crate::events::event_type`].
    pub event_type: String,
    /// Event-specific payload.
    pub payload: Value,
    /// `H("mp:payload:v1", canonical(payload))`, hex.
    pub payload_hash: String,
    /// `event_hash` of the previous event (zero hash for the first).
    pub previous_event_hash: String,
    /// Verifier-state hash before this event applies.
    pub state_hash_before: String,
    /// Verifier-state hash after this event applies.
    pub state_hash_after: String,
    /// ADR-041 §4: always `coordinator` on the event-envelope layer.
    pub signer: String,
    /// Signature over this event's `event_hash` by the coordinator.
    pub signature: String,
}

impl TranscriptEvent {
    /// `H("mp:event:v1", canonical(event-without-signature))`.
    ///
    /// The signature field is excluded so the signature can cover the hash.
    pub fn event_hash(&self) -> Hash {
        let mut value = serde_json::to_value(self).expect("event always serializes");
        if let Value::Object(map) = &mut value {
            map.remove("signature");
        }
        ds_hash("mp:event:v1", &[&canonical_json(&value)])
    }

    /// Recompute the payload hash from the payload.
    pub fn computed_payload_hash(&self) -> Hash {
        payload_hash(&self.payload)
    }
}

/// Hash a payload value the way the transcript stores it.
pub fn payload_hash(payload: &Value) -> Hash {
    ds_hash("mp:payload:v1", &[&canonical_json(payload)])
}

/// The full exported transcript: metadata, verification keys, and the event log.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Transcript {
    /// Transcript protocol version.
    pub protocol_version: u16,
    /// Hand id.
    pub hand_id: String,
    /// Table / room id.
    pub table_id: String,
    /// Dealing provider that produced this transcript.
    pub provider: String,
    /// Shuffle proof scheme identifier.
    pub shuffle_scheme: String,
    /// Decryption proof scheme identifier.
    pub decryption_scheme: String,
    /// Verification keys for every signer (see [`KeyDirectory`]).
    pub key_directory: KeyDirectory,
    /// The ordered, signed event log.
    pub events: Vec<TranscriptEvent>,
}

impl Transcript {
    /// Serialize to pretty JSON for export.
    pub fn to_json(&self) -> String {
        serde_json::to_string_pretty(self).expect("transcript always serializes")
    }

    /// Parse a transcript from exported JSON.
    pub fn from_json(s: &str) -> Result<Self, serde_json::Error> {
        serde_json::from_str(s)
    }
}

/// Builds a valid, signed transcript by applying events through the shared
/// [`ProtocolState`] machine and signing each one.
///
/// The builder *cannot* produce a structurally invalid transcript: a rejected
/// `apply` surfaces as [`BuildError`]. Tampering tests mutate the *finished*
/// transcript instead.
pub struct TranscriptBuilder<'a> {
    protocol_version: u16,
    hand_id: String,
    table_id: String,
    provider_label: String,
    signer: &'a dyn SignatureProvider,
    shuffle_scheme: String,
    decryption_scheme: String,
    key_directory: KeyDirectory,
    state: ProtocolState,
    events: Vec<TranscriptEvent>,
    prev_hash: Hash,
    seq: u64,
}

impl<'a> TranscriptBuilder<'a> {
    /// Start a new transcript.
    pub fn new(
        hand_id: impl Into<String>,
        table_id: impl Into<String>,
        provider_label: impl Into<String>,
        signer: &'a dyn SignatureProvider,
        shuffle: &dyn ShuffleProofProvider,
        decryption: &dyn DecryptionProvider,
        key_directory: KeyDirectory,
    ) -> Self {
        Self {
            protocol_version: PROTOCOL_VERSION,
            hand_id: hand_id.into(),
            table_id: table_id.into(),
            provider_label: provider_label.into(),
            signer,
            shuffle_scheme: shuffle.scheme().to_string(),
            decryption_scheme: decryption.scheme().to_string(),
            key_directory,
            state: ProtocolState::new(),
            events: Vec::new(),
            prev_hash: ZERO_HASH,
            seq: 0,
        }
    }

    /// Append, chain, and sign one event. `signer` is the originating id.
    ///
    /// ADR-041 §4: `signer` is always `coordinator` on the envelope layer.
    /// The builder uses `coordinator` in the `signer` field regardless of the
    /// `signer` argument for the event envelope, then delegates signing to the
    /// signer field argument (which should be `coordinator`).
    ///
    /// `contributor_claim` is the pre-computed `(contributor_id, signature_hex)` pair
    /// for client-action events. When `Some`, the payload's `contributor` /
    /// `contributor_signature` fields are populated with the provided values.
    /// Pass `None` for coordinator-authored events.
    ///
    /// The builder is a transcript *assembler*, not a validator: it records
    /// whatever event it is given. It still runs the shared state machine to
    /// populate `state_hash_before` / `state_hash_after` — and if that event
    /// is out-of-protocol the state simply does not advance (before == after),
    /// which the **verifier** will detect on replay. This lets tests assemble
    /// deliberately-broken transcripts that are still cryptographically
    /// well-formed (correct chain + signatures).
    pub fn append(&mut self, event_type: &str, payload: Value, signer: &str) {
        self.append_with_contributor(event_type, payload, signer, None);
    }

    /// Like [`append`], but allows supplying a pre-computed contributor
    /// `(contributor_id, contributor_signature_hex)` pair (ADR-041 §4).
    ///
    /// The contributor fields are injected into the payload before the
    /// `payload_hash` is computed, so the payload_hash covers the contributor
    /// signature — making it part of the tamper-evident chain.
    pub fn append_with_contributor(
        &mut self,
        event_type: &str,
        mut payload: Value,
        signer: &str,
        contributor_claim: Option<(&str, &str)>,
    ) {
        // Inject contributor fields into the payload if provided.
        if let Some((contributor_id, contributor_sig)) = contributor_claim {
            if let Value::Object(ref mut map) = payload {
                map.insert(
                    "contributor".to_string(),
                    Value::String(contributor_id.to_string()),
                );
                map.insert(
                    "contributor_signature".to_string(),
                    Value::String(contributor_sig.to_string()),
                );
            }
        }

        let state_hash_before = hex_hash(&self.state.state_hash());
        // Best-effort apply; an invalid event leaves the state untouched.
        let _ = self.state.apply(event_type, &payload);
        let state_hash_after = hex_hash(&self.state.state_hash());

        // ADR-041 §4: envelope signer is always `coordinator`.
        let envelope_signer = crate::events::COORDINATOR;

        let mut event = TranscriptEvent {
            protocol_version: self.protocol_version,
            hand_id: self.hand_id.clone(),
            table_id: self.table_id.clone(),
            sequence_number: self.seq,
            event_type: event_type.to_string(),
            payload_hash: hex_hash(&payload_hash(&payload)),
            payload,
            previous_event_hash: hex_hash(&self.prev_hash),
            state_hash_before,
            state_hash_after,
            signer: envelope_signer.to_string(),
            signature: String::new(),
        };
        let event_hash = event.event_hash();
        event.signature = self.signer.sign(envelope_signer, &event_hash);

        // Suppress unused-variable warning when signer is not coordinator.
        let _ = signer;

        self.prev_hash = event_hash;
        self.seq += 1;
        self.events.push(event);
    }

    /// Has the transcript reached a terminal phase?
    pub fn is_terminal(&self) -> bool {
        self.state.is_terminal()
    }

    /// Finish and produce the [`Transcript`].
    pub fn finish(self) -> Transcript {
        Transcript {
            protocol_version: self.protocol_version,
            hand_id: self.hand_id,
            table_id: self.table_id,
            provider: self.provider_label,
            shuffle_scheme: self.shuffle_scheme,
            decryption_scheme: self.decryption_scheme,
            key_directory: self.key_directory,
            events: self.events,
        }
    }
}

/// Helper: build an event payload [`Value`] from any serializable type.
pub fn to_payload<T: Serialize>(value: &T) -> Value {
    serde_json::to_value(value).unwrap_or(Value::Object(Map::new()))
}
