//! Offline transcript verifier.
//!
//! [`verify`] replays an exported [`Transcript`] and checks **every** rule in
//! `docs/mental-poker-dealing-refactor.md` §5. It is pure: no DB, no network,
//! no clock. Anyone — a player, an auditor, a regulator — can run it against an
//! exported transcript and a key directory to confirm the hand was dealt
//! honestly.

use crate::crypto::{
    DecryptionProvider, MockDecryptionProvider, MockShuffleProofProvider, Salt,
    ShuffleProofProvider,
};
use crate::events::*;
use crate::hash::{canonical_json, ds_hash, hex_hash, parse_hash, Hash, ZERO_HASH};
use crate::signing::{MockSignatureProvider, SignatureProvider};
use crate::state::{Phase, ProtocolState, StateError};
use crate::transcript::{Transcript, TranscriptEvent};
use serde_json::Value;
use thiserror::Error;

/// Whether the proof systems a verified transcript *declares* are the audited,
/// cryptographically-sound schemes or dev-only **mocks**.
///
/// This is the crux of BUG-108: the production `mental_poker_prefer` policy deals
/// eligible all-human hands with the **mock** crypto suite (`mock-shuffle-v1` +
/// `mock-decrypt-v1`, mock signing envelope — see `crypto.rs` module docs). Those
/// proofs are *not* cryptographically sound: a malicious shuffler that replaces a
/// card still produces a "valid" mock proof. A `verify()` `Ok` on such a
/// transcript means only that it **replays consistently** — it is NOT a
/// provable-fairness guarantee. Every consumer that presents a transcript to a
/// real player (the `mp-verify` CLI, any UI) MUST consult this and refuse to
/// claim cryptographic fairness for a [`SchemeSoundness::DevMock`] result.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SchemeSoundness {
    /// Every proof system is the audited real scheme: the real re-encryption
    /// shuffle, real threshold-ElGamal/Chaum–Pedersen decryption, and asymmetric
    /// Ed25519 signing. A `verify()` `Ok` here *is* a cryptographic fairness
    /// guarantee.
    Sound,
    /// At least one proof system is a DEV-ONLY MOCK (e.g. `mock-shuffle-v1`,
    /// `mock-decrypt-v1`, or the mock HMAC signing envelope). The transcript
    /// replays consistently but the proofs do NOT establish a true shuffle or
    /// honest decryption — this is NOT a provable-fairness guarantee.
    DevMock,
}

/// Classify a transcript's declared proof systems as cryptographically [`Sound`]
/// or [`DevMock`]. Pure: depends only on the declared scheme strings + whether
/// the signing key directory is the mock (symmetric HMAC) kind.
///
/// FAIL-CLOSED: anything other than the full real composition is [`DevMock`], so
/// the mock production path (`mental_poker_prefer`) can never be reported as a
/// sound fairness guarantee.
///
/// [`Sound`]: SchemeSoundness::Sound
/// [`DevMock`]: SchemeSoundness::DevMock
pub fn classify_soundness(
    shuffle_scheme: &str,
    decryption_scheme: &str,
    signing_is_mock: bool,
) -> SchemeSoundness {
    let shuffle_sound = shuffle_scheme == crate::crypto_real::shuffle::SCHEME;
    let decrypt_sound = decryption_scheme == crate::crypto_real::decrypt::SCHEME;
    if shuffle_sound && decrypt_sound && !signing_is_mock {
        SchemeSoundness::Sound
    } else {
        SchemeSoundness::DevMock
    }
}

/// A successful verification report.
#[derive(Debug, Clone)]
pub struct VerifyReport {
    /// Number of events replayed.
    pub events_checked: usize,
    /// Phase the transcript ended in (`Complete` or `Aborted`).
    pub final_phase: Phase,
    /// Number of players in the hand.
    pub num_players: u8,
    /// Card ids revealed across the hand, in deck-index order.
    pub revealed_card_ids: Vec<u8>,
    /// Whether the verified transcript's proof systems are cryptographically
    /// sound or dev-only mocks (BUG-108). An `Ok` report with
    /// [`SchemeSoundness::DevMock`] is a consistent **replay**, NOT a
    /// provable-fairness guarantee — consumers MUST NOT present it as one.
    pub soundness: SchemeSoundness,
}

impl VerifyReport {
    /// BUG-108 — whether this verified transcript may be presented to a real
    /// player as a cryptographic provable-fairness guarantee.
    ///
    /// `true` only for [`SchemeSoundness::Sound`]. A [`SchemeSoundness::DevMock`]
    /// report replays consistently but its proofs are cryptographically vacuous
    /// (a card-swapping shuffler still produces a "valid" mock proof), so this
    /// returns `false` and every fairness-facing consumer MUST refuse it. Prefer
    /// [`verify_fairness`], which fails closed on your behalf.
    pub fn is_provably_fair(&self) -> bool {
        matches!(self.soundness, SchemeSoundness::Sound)
    }
}

/// A verification failure, tagged with the offending event's sequence number.
#[derive(Debug, Error)]
#[error("transcript verification failed at event #{sequence_number}: {kind}")]
pub struct VerifyError {
    /// Sequence number of the event that failed (or the last seen).
    pub sequence_number: u64,
    /// The specific failure.
    pub kind: VerifyErrorKind,
}

/// ADR-041 §4.1 — event types that require a contributor signature.
const CONTRIBUTOR_REQUIRED_EVENTS: &[&str] = &[
    event_type::KEY_REGISTERED,
    event_type::SHUFFLE_CONTRIBUTION,
    event_type::FINAL_DECK_ACK,
    event_type::HOLE_CARD_OPENED,
];

/// The category of a [`VerifyError`].
#[derive(Debug, Error)]
pub enum VerifyErrorKind {
    /// The transcript had no events.
    #[error("transcript is empty")]
    Empty,
    /// An event's protocol version disagreed with the transcript.
    #[error("protocol version mismatch: event has {got}, transcript {want}")]
    ProtocolVersion {
        /// Version on the event.
        got: u16,
        /// Version on the transcript.
        want: u16,
    },
    /// An event's hand/table id disagreed with the transcript.
    #[error("hand/table id mismatch")]
    IdentityMismatch,
    /// `sequence_number` was not dense and monotonic from 0.
    #[error("sequence out of order: expected {expected}, got {got}")]
    SequenceOrder {
        /// Expected sequence number.
        expected: u64,
        /// Observed sequence number.
        got: u64,
    },
    /// `previous_event_hash` did not match the prior event's hash.
    #[error("hash chain broken")]
    BrokenChain,
    /// `payload_hash` did not match the payload.
    #[error("payload hash mismatch")]
    PayloadHashMismatch,
    /// A signer was not present in the key directory.
    #[error("unknown signer '{0}'")]
    UnknownSigner(String),
    /// A signature failed to verify.
    #[error("invalid signature from '{0}'")]
    BadSignature(String),
    /// An event was signed by the wrong party.
    #[error("wrong signer: event signed by '{got}', expected '{want}'")]
    WrongSigner {
        /// Actual signer.
        got: String,
        /// Required signer.
        want: String,
    },
    /// `state_hash_before` disagreed with the verifier's recomputed state.
    #[error("state_hash_before mismatch")]
    StateHashBefore,
    /// `state_hash_after` disagreed with the verifier's recomputed state.
    #[error("state_hash_after mismatch")]
    StateHashAfter,
    /// The shared state machine rejected the event.
    #[error("{0}")]
    State(#[from] StateError),
    /// A payload could not be parsed for a proof / signer check.
    #[error("malformed payload: {0}")]
    MalformedPayload(String),
    /// A shuffle proof did not verify.
    #[error("shuffle proof rejected for round {0}")]
    BadShuffleProof(u32),
    /// F2 (DKG rogue-key defense): a `key_registered` event on the real-shuffle
    /// path carried no party-bound Schnorr proof-of-knowledge of its
    /// `shuffle_pubkey`'s discrete log, or the PoK did not verify. Summing such a
    /// key into the joint key would let a registrant pick `Q_rogue = a·G − Σ
    /// Q_honest` (a joint key it controls the secret of) and decrypt every card.
    /// Only PoK-verified keys are summed.
    #[error("missing or invalid DKG key proof-of-knowledge for party '{0}'")]
    BadKeyProofOfKnowledge(String),
    /// mp-phase4 re-audit r1 (HIGH): the committed final ciphertext deck the
    /// `cp-threshold` threshold-open anchor (`with_expected_deck`) is rooted in is
    /// absent, malformed, or — when the real re-encryption shuffle proof is also
    /// present — does NOT equal that proof's output deck. Both the
    /// `final_deck_committed.deck_ct` source and the last shuffle proof's output
    /// must agree exactly; otherwise a transcript builder could keep the genuine,
    /// acked `final_deck_hash` (and verified shuffle proof) while substituting a
    /// `deck_ct` of its choosing for opens to be anchored against. Distinct from
    /// the engine-blind residual: this is a soundness check on the offline replay
    /// verifier's own trust anchor.
    #[error("committed ciphertext deck not bound to the verified shuffle output: {0}")]
    CiphertextDeckUnbound(String),
    /// A decryption proof did not verify.
    #[error("decryption proof rejected for deck index {0}")]
    BadDecryptionProof(u32),
    /// The transcript ended without reaching a terminal phase.
    #[error("transcript did not terminate (ended in {0:?})")]
    NotTerminal(Phase),
    /// The transcript declares a proof scheme the verifier cannot check.
    #[error("unsupported scheme '{0}'")]
    UnsupportedScheme(String),
    /// The key directory could not be used to build a signature verifier.
    #[error("unusable key directory: {0}")]
    BadKeyDirectory(String),
    /// ADR-041 §4: a client-action event is missing the required contributor signature.
    #[error("missing contributor signature on event type '{0}'")]
    MissingContributorSignature(String),
    /// ADR-041 §4: a contributor signature failed verification.
    #[error("invalid contributor signature from '{0}'")]
    BadContributorSignature(String),
    /// ADR-041 §4: the contributor signer is not the party the state machine
    /// attributes this action to. Without this bind, a valid signature from any
    /// keyholder over a payload-declared `contributor` proves nothing about who
    /// actually authorized the key-registration / shuffle / deck-ack / hole-open
    /// the event applies — letting party A forge party B's action.
    #[error(
        "contributor '{contributor}' is not the expected authorizer '{expected}' \
         for event type '{event_type}'"
    )]
    ContributorPartyMismatch {
        /// The party named in the payload's `contributor` field (the signer).
        contributor: String,
        /// The party the protocol requires to have authored this event type.
        expected: String,
        /// The event type whose binding failed.
        event_type: String,
    },
}

fn err(seq: u64, kind: VerifyErrorKind) -> VerifyError {
    VerifyError {
        sequence_number: seq,
        kind,
    }
}

/// Verify an exported transcript end to end.
pub fn verify(transcript: &Transcript) -> Result<VerifyReport, VerifyError> {
    if transcript.events.is_empty() {
        return Err(err(0, VerifyErrorKind::Empty));
    }

    // --- Select crypto providers from the transcript's declared schemes. ---
    let shuffle: Box<dyn ShuffleProofProvider> = match transcript.shuffle_scheme.as_str() {
        "mock-shuffle-v1" => Box::new(MockShuffleProofProvider),
        // PROTOTYPE (ADR-063 §3 / spec §3.4): the real sound verifiable
        // re-encryption shuffle. Verification is self-contained — the prover's
        // ciphertext decks + the sigma argument travel in `ShuffleProof.attestation`
        // and are bound to the event's `input_hash`/`output_hash` (deck-hash check).
        // This recognizes a real-crypto transcript for replay; it does NOT un-gate
        // production dealing (`guard_provider_allowed` still rejects
        // `mental_poker_production`; no production call site constructs it — the
        // ADR-063 cage).
        crate::crypto_real::shuffle::SCHEME => {
            Box::new(crate::crypto_real::shuffle::RealShuffleProofProvider::verifier())
        }
        other => {
            return Err(err(
                0,
                VerifyErrorKind::UnsupportedScheme(other.to_string()),
            ))
        }
    };
    let decryption: Box<dyn DecryptionProvider> = match transcript.decryption_scheme.as_str() {
        "mock-decrypt-v1" => Box::new(MockDecryptionProvider),
        // PROTOTYPE (ADR-063 §4 / audit F3): the real threshold-ElGamal +
        // Chaum–Pedersen decryption scheme. To verify the n-of-n opens the
        // provider needs the DKG party public keys `Q_i`; we reconstruct them
        // from the transcript's `key_registered` events (`shuffle_pubkey` field),
        // the same trusted, contributor-signed source F2 sums into the joint key.
        // This makes an exported real-crypto transcript end-to-end checkable
        // offline; it does NOT un-gate production (the ADR-063 cage stands —
        // guard_provider_allowed still rejects mental_poker_production).
        crate::crypto_real::decrypt::SCHEME => {
            let party_pubkeys = collect_party_pubkeys(transcript)?;
            // F3 (mp-phase4 audit — DEFENSIVE, decouple from the shuffle scheme):
            // EVERY threshold open is anchored to the committed final ciphertext
            // deck the parties acknowledged (`final_deck_ack`), so an opening cannot
            // claim a ciphertext other than the one the `final_deck_committed` step
            // froze. The committed deck is transcript-bound on the SAME event the
            // parties sign — `final_deck_committed.deck_ct` — so the anchor is
            // available even when the shuffle is NOT the real one (a mixed mode the
            // audit requires we not regress on). For the real shuffle, the deck also
            // travels in the last shuffle proof's attestation; either source yields
            // the same 52-entry ciphertext deck. A cp-threshold transcript with NO
            // recoverable committed ciphertext deck is malformed → clean reject
            // (the anchor is MANDATORY for this scheme; we never fall back to the
            // unanchored provider, which would let a prover open a self-supplied
            // ciphertext).
            let deck = final_ciphertext_deck(transcript)?;
            Box::new(
                crate::crypto_real::decrypt::RealThresholdDecryptionProvider::with_expected_deck(
                    party_pubkeys,
                    deck,
                ),
            )
        }
        other => {
            return Err(err(
                0,
                VerifyErrorKind::UnsupportedScheme(other.to_string()),
            ))
        }
    };
    // Signature scheme dispatch by key-directory kind:
    // - `is_mock == true`  → the dev-only symmetric HMAC mock (forgeable; never
    //   server-blind). Retained for the non-blind transcript mode.
    // - `is_mock == false` → the real **asymmetric** Ed25519 verifier, keyed by
    //   PUBLIC verifying keys only (ADR-063 §5 / spec §5.2). This is the arm
    //   that previously returned "no asymmetric verifier available"; Phase-4
    //   increment 1 fills it in.
    //
    // PROTOTYPE: the Ed25519 arm is reachable from `verify()` so transcripts
    // signed with real keys can be replayed/checked, but it does NOT un-gate
    // production dealing — `guard_provider_allowed` still rejects
    // `mental_poker_production`, and no production call site constructs the real
    // providers (ADR-063 cage).
    let sig: Box<dyn SignatureProvider> = if transcript.key_directory.is_mock {
        match MockSignatureProvider::from_directory(&transcript.key_directory) {
            Some(p) => Box::new(p),
            None => {
                return Err(err(
                    0,
                    VerifyErrorKind::BadKeyDirectory("malformed mock keys".into()),
                ))
            }
        }
    } else {
        match crate::crypto_real::Ed25519SignatureProvider::verifier_from_directory(
            &transcript.key_directory,
        ) {
            Some(p) => Box::new(p),
            None => {
                return Err(err(
                    0,
                    VerifyErrorKind::BadKeyDirectory(
                        "malformed asymmetric (Ed25519) key directory".into(),
                    ),
                ))
            }
        }
    };

    let mut state = ProtocolState::new();
    let mut prev_hash: Hash = ZERO_HASH;

    // F2 (shuffle key binding): for the real re-encryption-shuffle scheme the
    // verifier MUST pin every shuffle proof to the DKG-derived joint key rather
    // than trusting the joint key carried inside the prover's attestation
    // (otherwise a shuffler could re-encrypt under a key it controls and decrypt
    // every card). The trusted joint key is `Σ shuffle_pubkey` over the parties'
    // `key_registered` events — each `shuffle_pubkey` is a party's DKG public
    // share `Q_i`, signed by that party (contributor layer) and bound to its
    // identity. We accumulate it across `key_registered` events (which always
    // precede shuffles) and hand the running sum to the shuffle check. `None`
    // until at least one key is registered, and left `None` entirely for the
    // mock scheme (whose `shuffle_pubkey` is a hash, not a curve point).
    let real_shuffle = transcript.shuffle_scheme == crate::crypto_real::shuffle::SCHEME;
    let real_decrypt = transcript.decryption_scheme == crate::crypto_real::decrypt::SCHEME;

    // F2 (mp-phase4 audit — DEFENSIVE, decouple from the shuffle scheme). The
    // rogue-key proof-of-knowledge gate must run whenever the JOINT KEY underwrites
    // ANYTHING in the transcript — and it does so for BOTH the real shuffle (which
    // binds proofs to `Σ Q_i`) AND the real threshold decryption (whose `Q_i` shares
    // sum to the joint key the deck is encrypted under, `Ct.c2 = M + r·Q`). A
    // transcript may legitimately pair real `cp-threshold` decryption with a
    // non-real shuffle (a mixed mode). If the PoK gate were keyed only on the
    // shuffle scheme, that mixed mode would sum unproven `Q_i` into the directory
    // the decryption verifier trusts — re-opening the rogue-key hole
    // (`Q_rogue = a·G − Σ Q_honest` ⇒ joint key `a·G` ⇒ decrypt every card). So we
    // REQUIRE + verify each party's party-bound Schnorr PoK whenever the shuffle is
    // real OR the decryption scheme is cp-threshold.
    let require_key_pok = real_shuffle || real_decrypt;
    // Whether to BIND each shuffle proof to the accumulated joint key. Only the
    // real shuffle consumes an expected joint key (the mock shuffle ignores it and
    // its `shuffle_pubkey` is a hash, not a curve point). The PoK gate above is a
    // SUPERSET of this — under a mock shuffle + real decryption we still verify the
    // PoKs but do not feed a joint key into the (mock) shuffle check.
    let bind_shuffle_key = real_shuffle;
    let mut joint_key_acc: Option<curve25519_dalek::ristretto::RistrettoPoint> = None;

    for (i, event) in transcript.events.iter().enumerate() {
        let seq = event.sequence_number;

        // (4a) version / identity.
        if event.protocol_version != transcript.protocol_version {
            return Err(err(
                seq,
                VerifyErrorKind::ProtocolVersion {
                    got: event.protocol_version,
                    want: transcript.protocol_version,
                },
            ));
        }
        if event.hand_id != transcript.hand_id || event.table_id != transcript.table_id {
            return Err(err(seq, VerifyErrorKind::IdentityMismatch));
        }

        // (4) sequence numbers dense + monotonic from 0.
        if event.sequence_number != i as u64 {
            return Err(err(
                seq,
                VerifyErrorKind::SequenceOrder {
                    expected: i as u64,
                    got: event.sequence_number,
                },
            ));
        }

        // (2) hash chain.
        if parse_hash(&event.previous_event_hash) != Some(prev_hash) {
            return Err(err(seq, VerifyErrorKind::BrokenChain));
        }

        // (1) payload hash.
        if hex_hash(&event.computed_payload_hash()) != event.payload_hash {
            return Err(err(seq, VerifyErrorKind::PayloadHashMismatch));
        }

        // (3) signature over the event hash.
        let event_hash = event.event_hash();
        if !transcript.key_directory.keys.contains_key(&event.signer) {
            return Err(err(
                seq,
                VerifyErrorKind::UnknownSigner(event.signer.clone()),
            ));
        }
        if !sig.verify(&event.signer, &event_hash, &event.signature) {
            return Err(err(
                seq,
                VerifyErrorKind::BadSignature(event.signer.clone()),
            ));
        }

        // (3b) ADR-041 §4: the envelope signer must always be `coordinator`.
        // Client-action events that previously required the party to sign the
        // envelope now require it to sign the *contributor claim* in the payload.
        if event.signer != COORDINATOR {
            return Err(err(
                seq,
                VerifyErrorKind::WrongSigner {
                    got: event.signer.clone(),
                    want: COORDINATOR.to_string(),
                },
            ));
        }

        // (3c) ADR-041 §4: for client-action event types, verify the contributor
        // signature in the payload. The contributor signs a canonical claim
        // independently of the transcript placement.
        if CONTRIBUTOR_REQUIRED_EVENTS.contains(&event.event_type.as_str()) {
            verify_contributor_signature(seq, event, sig.as_ref())?;
        }

        // (6) state hash before.
        if hex_hash(&state.state_hash()) != event.state_hash_before {
            return Err(err(seq, VerifyErrorKind::StateHashBefore));
        }

        // (5) state machine: apply the event.
        state
            .apply(&event.event_type, &event.payload)
            .map_err(|e| err(seq, VerifyErrorKind::State(e)))?;

        // (6) state hash after.
        if hex_hash(&state.state_hash()) != event.state_hash_after {
            return Err(err(seq, VerifyErrorKind::StateHashAfter));
        }

        // F2: accumulate the trusted joint key from registered shuffle pubkeys.
        // Gated on `require_key_pok` (real shuffle OR cp-threshold decryption —
        // mp-phase4 audit), NOT on the shuffle scheme alone. Done AFTER the state
        // machine has accepted the event (so a malformed key_registered already
        // aborted above) and BEFORE the next shuffle is checked.
        //
        // ROGUE-KEY DEFENSE (audit F2, re-audit round 1): the joint key is `Σ Q_i`
        // and every shuffle proof is bound to it, so a registrant that can set its
        // OWN `Q_i` arbitrarily can steer the joint key to `a·G` for an `a` it
        // knows (`Q_rogue = a·G − Σ Q_honest`) — then decrypt every card via
        // `C2 − a·C1 = M`. We therefore sum **only** keys that carry a valid
        // PARTY-BOUND Schnorr proof-of-knowledge of `log_G(Q_i)`: a rogue cannot
        // produce one for `Q_rogue` because it does not know that discrete log
        // (it knows `a = log_G(Σ Q_i)`, not `log_G` of its own share). The PoK is
        // bound to `party_id` (F4), so the untrusted coordinator also cannot
        // relabel one party's PoK onto another's key.
        if require_key_pok && event.event_type == event_type::KEY_REGISTERED {
            let qi_hex = event
                .payload
                .get("shuffle_pubkey")
                .and_then(Value::as_str)
                .ok_or_else(|| err(seq, malformed("key_registered missing shuffle_pubkey")))?;
            let qi = crate::crypto_real::ec::point_from_hex(qi_hex)
                .ok_or_else(|| err(seq, malformed("shuffle_pubkey not a ristretto point")))?;
            // F-CRYPTO-15: reject an identity (x_i = 0) share BEFORE summing it
            // into the joint key (see `ec::is_identity_pubkey` for the rationale).
            if crate::crypto_real::ec::is_identity_pubkey(&qi) {
                let party = event
                    .payload
                    .get("party_id")
                    .and_then(Value::as_str)
                    .unwrap_or("");
                return Err(err(
                    seq,
                    VerifyErrorKind::BadKeyProofOfKnowledge(party.to_string()),
                ));
            }
            let party = event
                .payload
                .get("party_id")
                .and_then(Value::as_str)
                .ok_or_else(|| err(seq, malformed("key_registered missing party_id")))?;
            // Require + verify the party-bound PoK BEFORE summing this key.
            let pok: crate::crypto_real::dkg::SchnorrPok = event
                .payload
                .get("key_pok")
                .and_then(|v| serde_json::from_value(v.clone()).ok())
                .ok_or_else(|| {
                    err(
                        seq,
                        VerifyErrorKind::BadKeyProofOfKnowledge(party.to_string()),
                    )
                })?;
            if !crate::crypto_real::dkg::schnorr_verify(party, &qi, &pok) {
                return Err(err(
                    seq,
                    VerifyErrorKind::BadKeyProofOfKnowledge(party.to_string()),
                ));
            }
            joint_key_acc = Some(joint_key_acc.unwrap_or_default() + qi);
        }

        // (8 / 12) cryptographic proof checks via the provider interfaces.
        let expected_joint_key = if bind_shuffle_key {
            joint_key_acc.map(|q| crate::crypto_real::ec::point_to_hex(&q))
        } else {
            None
        };
        verify_proofs(
            seq,
            event,
            shuffle.as_ref(),
            decryption.as_ref(),
            expected_joint_key.as_deref(),
        )?;

        prev_hash = event_hash;
    }

    // (16) terminal state.
    if !state.is_terminal() {
        let last = transcript.events.last().unwrap().sequence_number;
        return Err(err(last, VerifyErrorKind::NotTerminal(state.phase)));
    }

    Ok(VerifyReport {
        events_checked: transcript.events.len(),
        final_phase: state.phase,
        num_players: state.num_players,
        revealed_card_ids: state.opened_card_ids.clone(),
        // BUG-108: record whether the proof systems are the audited real schemes
        // or dev-only mocks, so no consumer mistakes a consistent mock-crypto
        // replay for a cryptographic fairness guarantee. The schemes were already
        // validated/dispatched above; classify from the transcript's declarations.
        soundness: classify_soundness(
            &transcript.shuffle_scheme,
            &transcript.decryption_scheme,
            transcript.key_directory.is_mock,
        ),
    })
}

/// Why [`verify_fairness`] refused to certify a transcript as provably fair.
#[derive(Debug, Error)]
pub enum VerifyFairnessError {
    /// The transcript did not even replay consistently — the underlying
    /// [`verify`] check failed. Carries the original [`VerifyError`].
    #[error("{0}")]
    Replay(#[from] VerifyError),
    /// BUG-108: the transcript REPLAYS consistently but its declared proof
    /// systems are dev-only mocks ([`SchemeSoundness::DevMock`]) — it is NOT a
    /// cryptographic provable-fairness guarantee and MUST NOT be presented to a
    /// real player as one. Distinct from [`Replay`](Self::Replay): the bytes are
    /// internally consistent; what is missing is cryptographic *soundness*.
    #[error(
        "transcript replays consistently but is NOT provably fair (dev-only mock \
         crypto): shuffle='{shuffle_scheme}' decryption='{decryption_scheme}' \
         signing_is_mock={signing_is_mock}"
    )]
    NotProvablyFair {
        /// The transcript's declared shuffle scheme (e.g. `mock-shuffle-v1`).
        shuffle_scheme: String,
        /// The transcript's declared decryption scheme (e.g. `mock-decrypt-v1`).
        decryption_scheme: String,
        /// Whether the signing key directory is the dev-only symmetric mock.
        signing_is_mock: bool,
    },
}

/// BUG-108 (fail-closed fairness gate) — the strict counterpart to [`verify`].
///
/// [`verify`] returns `Ok` for ANY transcript that **replays consistently**,
/// including the dev-only mock-crypto suite ([`SchemeSoundness::DevMock`]) the
/// production `mental_poker_prefer` policy deals. That `Ok` is the right answer
/// for an internal replay *self-check* (e.g. the server validating a transcript
/// it just built — `mp_dealing.rs`), but it is the WRONG answer for any consumer
/// asking the *fairness* question "may I present this to a real player as
/// provably fair?": a mock replay is cryptographically vacuous, so a bare `Ok`
/// (or a CLI exit 0) lets automation mistake it for a passed fairness check.
///
/// `verify_fairness` is that consumer's API and it **fails closed**:
///   * a replay failure propagates as [`VerifyFairnessError::Replay`];
///   * a consistent replay whose schemes are dev-mocks returns
///     [`VerifyFairnessError::NotProvablyFair`] — NOT `Ok`;
///   * only a [`SchemeSoundness::Sound`] transcript returns `Ok(report)`.
///
/// The `mp-verify` CLI routes this distinction to a dedicated non-zero exit code
/// (3) so scripts/exports/UI cannot treat a mock transcript as verified-fair.
pub fn verify_fairness(transcript: &Transcript) -> Result<VerifyReport, VerifyFairnessError> {
    let report = verify(transcript)?;
    match report.soundness {
        SchemeSoundness::Sound => Ok(report),
        SchemeSoundness::DevMock => Err(VerifyFairnessError::NotProvablyFair {
            shuffle_scheme: transcript.shuffle_scheme.clone(),
            decryption_scheme: transcript.decryption_scheme.clone(),
            signing_is_mock: transcript.key_directory.is_mock,
        }),
    }
}

/// Per-event shuffle / decryption proof verification.
fn verify_proofs(
    seq: u64,
    event: &crate::transcript::TranscriptEvent,
    shuffle: &dyn ShuffleProofProvider,
    decryption: &dyn DecryptionProvider,
    expected_joint_key: Option<&str>,
) -> Result<(), VerifyError> {
    match event.event_type.as_str() {
        event_type::SHUFFLE_CONTRIBUTION => {
            let p: ShuffleContributionPayload = parse(seq, &event.payload)?;
            let ih = parse_hash(&p.input_deck_hash)
                .ok_or_else(|| err(seq, malformed("input deck hash")))?;
            let oh = parse_hash(&p.output_deck_hash)
                .ok_or_else(|| err(seq, malformed("output deck hash")))?;
            // F2: bind to the DKG-derived joint key (real scheme only; `None`
            // for the mock). A shuffle proven under any other key is rejected.
            if !shuffle.verify_shuffle(&p.party_id, p.round, &ih, &oh, expected_joint_key, &p.proof)
            {
                return Err(err(seq, VerifyErrorKind::BadShuffleProof(p.round)));
            }
        }
        event_type::HOLE_CARD_OPENED => {
            let p: HoleCardOpenedPayload = parse(seq, &event.payload)?;
            check_decryption(seq, decryption, &p.owner_party_id, &p.card)?;
        }
        event_type::COMMUNITY_REVEALED => {
            let p: CommunityRevealedPayload = parse(seq, &event.payload)?;
            for card in &p.cards {
                // Community cards are jointly decrypted; the coordinator
                // publishes the aggregate proof.
                check_decryption(seq, decryption, COORDINATOR, card)?;
            }
        }
        _ => {}
    }
    Ok(())
}

fn check_decryption(
    seq: u64,
    decryption: &dyn DecryptionProvider,
    party: &str,
    card: &OpenedCard,
) -> Result<(), VerifyError> {
    let salt = parse_salt(&card.salt).ok_or_else(|| err(seq, malformed("salt")))?;
    if !decryption.verify_decryption(party, card.deck_index, card.card_id, &salt, &card.proof) {
        return Err(err(
            seq,
            VerifyErrorKind::BadDecryptionProof(card.deck_index),
        ));
    }
    Ok(())
}

/// ADR-041 §4: verify contributor signature on a client-action event.
///
/// Extracts the `contributor` and `contributor_signature` fields from the
/// payload, computes the canonical claim for that event type, and verifies
/// the signature using the key directory.
fn verify_contributor_signature(
    seq: u64,
    event: &TranscriptEvent,
    sig: &dyn SignatureProvider,
) -> Result<(), VerifyError> {
    // Extract contributor + signature from the payload.
    let missing = || {
        err(
            seq,
            VerifyErrorKind::MissingContributorSignature(event.event_type.clone()),
        )
    };
    let contributor = event
        .payload
        .get("contributor")
        .and_then(Value::as_str)
        .ok_or_else(missing)?;
    let contributor_sig = event
        .payload
        .get("contributor_signature")
        .and_then(Value::as_str)
        .ok_or_else(missing)?;

    // ADR-041 §4 — CONTRIBUTOR BINDING. The signature check below proves only
    // that *some* keyholder signed a claim naming `contributor`. For
    // party-authored events we must ALSO require that `contributor` is the party
    // the shared state machine credits the action to (`party_id`). Without this
    // bind the untrusted coordinator can record one party's key-registration /
    // shuffle / deck-ack under a different identity it controls (audit
    // 2026-06-03: verifier contributor-binding gap).
    if let Some(expected) = expected_contributor(&event.event_type, &event.payload) {
        if contributor != expected {
            return Err(err(
                seq,
                VerifyErrorKind::ContributorPartyMismatch {
                    contributor: contributor.to_string(),
                    expected: expected.to_string(),
                    event_type: event.event_type.clone(),
                },
            ));
        }
    }

    // Build the canonical claim object per ADR-041 §4.1.
    let claim_obj = contributor_claim_object(
        &event.event_type,
        &event.hand_id,
        contributor,
        &event.payload,
    )
    .ok_or_else(|| {
        err(
            seq,
            VerifyErrorKind::MalformedPayload(format!(
                "cannot build claim for event type '{}'",
                event.event_type
            )),
        )
    })?;

    let claim_hash = ds_hash("mp:claim:v1", &[&canonical_json(&claim_obj)]);

    if !sig.verify(contributor, &claim_hash, &contributor_sig.to_string()) {
        return Err(err(
            seq,
            VerifyErrorKind::BadContributorSignature(contributor.to_string()),
        ));
    }
    Ok(())
}

/// The party id that MUST have produced the contributor signature for a
/// **party-authored** event (ADR-041 §4). When `Some`, the verifier binds the
/// payload's self-declared `contributor` to this value so the untrusted
/// coordinator cannot record one party's action under another party's identity:
/// - `key_registered` / `shuffle_contribution` / `final_deck_ack` →
///   the contributor must equal the payload's `party_id` (the seat the shared
///   state machine credits the action to). Without this bind the coordinator
///   can register a key IT controls as another player's signing key (then forge
///   all their future contributor signatures), forge their shuffle, or forge
///   their deck acknowledgement.
///
/// `hole_card_opened` returns `None` (no party binding). Its card *value* is
/// already pinned by the committed-deck commitment + the per-card decryption
/// proof (checked separately against `owner_party_id` in `verify_proofs`), so
/// the contributor identity carries no additional authority — and the two
/// honest builders disagree on whether it is the owner or the coordinator, so
/// binding it would reject valid transcripts without closing any attack.
fn expected_contributor<'a>(event_type_str: &str, payload: &'a Value) -> Option<&'a str> {
    match event_type_str {
        event_type::KEY_REGISTERED
        | event_type::SHUFFLE_CONTRIBUTION
        | event_type::FINAL_DECK_ACK => payload.get("party_id").and_then(Value::as_str),
        _ => None,
    }
}

/// Build the canonical claim JSON object for a given event type (ADR-041 §4.1).
///
/// This is the **single source of truth** for the contributor-claim shape. The
/// claim hash is `ds_hash` over `canonical_json` of this object, so the builder
/// (`mental.rs`, which signs) and the verifier (here, which checks) MUST produce
/// byte-identical objects — any drift would silently reject every honest
/// transcript with `BadContributorSignature`. `mental.rs` therefore calls *this*
/// function rather than keeping a second copy (audit 2026-06-03: two-site drift).
pub(crate) fn contributor_claim_object(
    event_type_str: &str,
    hand_id: &str,
    contributor: &str,
    payload: &Value,
) -> Option<Value> {
    use serde_json::json;
    match event_type_str {
        event_type::KEY_REGISTERED => {
            let signing_pubkey = payload.get("signing_pubkey")?.as_str()?;
            let shuffle_pubkey = payload.get("shuffle_pubkey")?.as_str()?;
            let party_id = payload.get("party_id")?.as_str()?;
            Some(json!({
                "hand_id": hand_id,
                "party_id": party_id,
                "signing_pubkey": signing_pubkey,
                "shuffle_pubkey": shuffle_pubkey,
            }))
        }
        event_type::SHUFFLE_CONTRIBUTION => {
            let round = payload.get("round")?.as_u64()?;
            let input_deck_hash = payload.get("input_deck_hash")?.as_str()?;
            let output_deck_hash = payload.get("output_deck_hash")?.as_str()?;
            let proof_attestation = payload.get("proof")?.get("attestation")?.as_str()?;
            Some(json!({
                "hand_id": hand_id,
                "round": round,
                "input_deck_hash": input_deck_hash,
                "output_deck_hash": output_deck_hash,
                "proof_attestation": proof_attestation,
            }))
        }
        event_type::FINAL_DECK_ACK => {
            let final_deck_hash = payload.get("final_deck_hash")?.as_str()?;
            Some(json!({
                "hand_id": hand_id,
                "party_id": contributor,
                "final_deck_hash": final_deck_hash,
            }))
        }
        event_type::HOLE_CARD_OPENED => {
            let deck_index = payload.get("card")?.get("deck_index")?.as_u64()?;
            let card_id = payload.get("card")?.get("card_id")?.as_u64()?;
            Some(json!({
                "hand_id": hand_id,
                "deck_index": deck_index,
                "card_id": card_id,
            }))
        }
        _ => None,
    }
}

/// F3: reconstruct the DKG party public keys `[(party_id, Q_i)]` from the
/// transcript's `key_registered` events (the `shuffle_pubkey` field is each
/// party's DKG public share). Used to build the real threshold-decryption
/// verifier. Each `shuffle_pubkey` must be a valid ristretto point (clean reject
/// otherwise). The set must be non-empty and duplicate-free (one key per party).
fn collect_party_pubkeys(
    transcript: &Transcript,
) -> Result<Vec<(String, curve25519_dalek::ristretto::RistrettoPoint)>, VerifyError> {
    let mut out: Vec<(String, curve25519_dalek::ristretto::RistrettoPoint)> = Vec::new();
    for event in &transcript.events {
        if event.event_type != event_type::KEY_REGISTERED {
            continue;
        }
        let seq = event.sequence_number;
        let party = event
            .payload
            .get("party_id")
            .and_then(Value::as_str)
            .ok_or_else(|| err(seq, malformed("key_registered missing party_id")))?;
        let qi_hex = event
            .payload
            .get("shuffle_pubkey")
            .and_then(Value::as_str)
            .ok_or_else(|| err(seq, malformed("key_registered missing shuffle_pubkey")))?;
        let qi = crate::crypto_real::ec::point_from_hex(qi_hex)
            .ok_or_else(|| err(seq, malformed("shuffle_pubkey not a ristretto point")))?;
        // F-CRYPTO-15 (code-review #7): reject an identity (x_i=0) share at the
        // SITE keys are reconstructed for the threshold-decrypt verifier, not only
        // in the main event loop's KEY_REGISTERED guard — these keys flow into
        // RealThresholdDecryptionProvider. Defense-in-depth so the guard lives
        // next to the reconstruction. See `ec::is_identity_pubkey` for the why.
        if crate::crypto_real::ec::is_identity_pubkey(&qi) {
            return Err(err(
                seq,
                VerifyErrorKind::BadKeyProofOfKnowledge(party.to_string()),
            ));
        }
        if out.iter().any(|(p, _)| p == party) {
            return Err(err(seq, malformed("duplicate key_registered for party")));
        }
        out.push((party.to_string(), qi));
    }
    if out.is_empty() {
        return Err(err(
            0,
            malformed("real decryption scheme requires registered party keys"),
        ));
    }
    Ok(out)
}

/// F3 (verifier anchor): recover the committed final ciphertext deck the parties
/// acknowledged, so every threshold open can be pinned to `deck[deck_index]`.
///
/// mp-phase4 re-audit r1 (HIGH) — soundness of the offline replay verifier's
/// trust anchor. The deck is sourced from `final_deck_committed.deck_ct` (the
/// deck transcript-bound on the event the parties sign via `final_deck_ack`) and,
/// when the REAL re-encryption shuffle is present, ALSO from the last
/// `shuffle_contribution` proof's output deck. Both sources must agree exactly:
///   * `deck_ct` is the F3 anchor the threshold opens are pinned to
///     (`with_expected_deck`). On the reenc path the state machine already binds
///     it to the verified-shuffle `final_deck_hash` (`apply_final_deck`); this
///     adds an independent ciphertext-level cross-check against the proof output,
///     so a substituted `deck_ct` is rejected even if the state-machine bind were
///     somehow bypassed.
///   * The last shuffle proof's output deck is the cryptographically verified
///     permutation (`verify_shuffle` checked it during the event loop).
///
/// Returns `Err` (clean reject) when:
///   * no recoverable ciphertext deck exists (neither source present/decodable),
///   * `deck_ct` has the wrong length or a malformed point, or
///   * `deck_ct` and the real shuffle proof's output deck disagree.
///
/// The previous `None`-via-fallback was a soundness hole: it let `deck_ct` be
/// CONSULTED-OR-IGNORED instead of CROSS-CHECKED, so an attacker-chosen `deck_ct`
/// could be anchored against while the genuine, acked `final_deck_hash` + shuffle
/// proof remained untouched.
fn final_ciphertext_deck(
    transcript: &Transcript,
) -> Result<Vec<crate::crypto_real::ec::Ct>, VerifyError> {
    use crate::crypto_real::ec::{deck_hash as ct_deck_hash, Ct, DECK_SIZE};

    // The seq of `final_deck_committed`, for error attribution (0 if absent).
    let committed_seq = transcript
        .events
        .iter()
        .find(|e| e.event_type == event_type::FINAL_DECK_COMMITTED)
        .map(|e| e.sequence_number)
        .unwrap_or(0);

    // (1) The ciphertext deck bound on `final_deck_committed.deck_ct`. Decode
    // strictly: present, exactly 52 entries, every point valid.
    let from_committed: Option<Vec<Ct>> = transcript
        .events
        .iter()
        .find(|e| e.event_type == event_type::FINAL_DECK_COMMITTED)
        .and_then(|ev| serde_json::from_value::<FinalDeckCommittedPayload>(ev.payload.clone()).ok())
        .and_then(|payload| payload.deck_ct)
        .map(|wire| {
            if wire.len() != DECK_SIZE {
                return Err(err(
                    committed_seq,
                    VerifyErrorKind::CiphertextDeckUnbound(format!(
                        "final_deck_committed.deck_ct has {} entries, expected {DECK_SIZE}",
                        wire.len()
                    )),
                ));
            }
            let decoded: Option<Vec<Ct>> = wire.iter().map(Ct::from_wire).collect();
            decoded.ok_or_else(|| {
                err(
                    committed_seq,
                    VerifyErrorKind::CiphertextDeckUnbound(
                        "final_deck_committed.deck_ct has a malformed ElGamal point".into(),
                    ),
                )
            })
        })
        .transpose()?;

    // (2) The real re-encryption shuffle's last-round output deck (only the real
    // shuffle carries it). This is the cryptographically verified permutation.
    let from_proof: Option<Vec<Ct>> =
        if transcript.shuffle_scheme == crate::crypto_real::shuffle::SCHEME {
            let last_shuffle = transcript
                .events
                .iter()
                .rev()
                .find(|e| e.event_type == event_type::SHUFFLE_CONTRIBUTION);
            match last_shuffle {
                Some(ev) => {
                    let payload: ShuffleContributionPayload =
                        serde_json::from_value(ev.payload.clone()).map_err(|e| {
                            err(
                                ev.sequence_number,
                                VerifyErrorKind::MalformedPayload(e.to_string()),
                            )
                        })?;
                    crate::crypto_real::shuffle::output_deck_from_proof(&payload.proof)
                }
                None => None,
            }
        } else {
            None
        };

    match (from_committed, from_proof) {
        // Both present → MUST match byte-for-byte (ciphertext-deck hash equality).
        // This is the cross-check the audit requires: a coordinator that keeps the
        // genuine `final_deck_hash` + verified shuffle proof but substitutes
        // `deck_ct` is caught here even before the state-machine bind.
        (Some(committed), Some(proof)) => {
            if hex_hash(&ct_deck_hash(&committed)) != hex_hash(&ct_deck_hash(&proof)) {
                return Err(err(
                    committed_seq,
                    VerifyErrorKind::CiphertextDeckUnbound(
                        "final_deck_committed.deck_ct does not equal the verified \
                         re-encryption shuffle's output deck"
                            .into(),
                    ),
                ));
            }
            Ok(committed)
        }
        // Only the committed deck (mixed mode: cp-threshold decryption + non-real
        // shuffle). The state machine cannot cross-check it against a real proof,
        // so on that mode the anchor rests on the acked `final_deck_hash` alone —
        // which for a non-reenc shuffle is the per-card-commit hash; opens are then
        // anchored to this deck and cross-checked against `final_deck_commits`.
        (Some(committed), None) => Ok(committed),
        // Only the proof (real shuffle, `deck_ct` omitted — older transcript).
        (None, Some(proof)) => Ok(proof),
        // Neither → malformed cp-threshold transcript: clean reject.
        (None, None) => Err(err(
            committed_seq,
            VerifyErrorKind::CiphertextDeckUnbound(
                "cp-threshold decryption requires a committed ciphertext deck \
                 (final_deck_committed.deck_ct or the real shuffle proof)"
                    .into(),
            ),
        )),
    }
}

fn parse<T: serde::de::DeserializeOwned>(seq: u64, payload: &Value) -> Result<T, VerifyError> {
    serde_json::from_value(payload.clone())
        .map_err(|e| err(seq, VerifyErrorKind::MalformedPayload(e.to_string())))
}

fn malformed(what: &str) -> VerifyErrorKind {
    VerifyErrorKind::MalformedPayload(what.to_string())
}

fn parse_salt(hex_str: &str) -> Option<Salt> {
    let bytes = hex::decode(hex_str).ok()?;
    if bytes.len() != 32 {
        return None;
    }
    let mut out = [0u8; 32];
    out.copy_from_slice(&bytes);
    Some(out)
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod soundness_tests {
    //! BUG-108 regression: a `verify()` Ok on the production mock-crypto path
    //! must report [`SchemeSoundness::DevMock`], never be mistaken for a sound
    //! provable-fairness guarantee.
    use super::*;
    use crate::crypto_real::{decrypt, shuffle};
    use crate::mental::MentalPokerDealingProvider;
    use crate::provider::{DealRequest, DealingProvider};

    const MOCK_SHUFFLE: &str = "mock-shuffle-v1";
    const MOCK_DECRYPT: &str = "mock-decrypt-v1";

    #[test]
    fn classify_full_mock_is_devmock() {
        assert_eq!(
            classify_soundness(MOCK_SHUFFLE, MOCK_DECRYPT, true),
            SchemeSoundness::DevMock
        );
    }

    #[test]
    fn classify_full_real_is_sound() {
        assert_eq!(
            classify_soundness(shuffle::SCHEME, decrypt::SCHEME, false),
            SchemeSoundness::Sound
        );
    }

    #[test]
    fn classify_is_devmock_if_any_component_is_mock() {
        // Real shuffle + real decrypt but mock signing envelope → NOT sound.
        assert_eq!(
            classify_soundness(shuffle::SCHEME, decrypt::SCHEME, true),
            SchemeSoundness::DevMock
        );
        // Mock shuffle, real decrypt, real signing → NOT sound.
        assert_eq!(
            classify_soundness(MOCK_SHUFFLE, decrypt::SCHEME, false),
            SchemeSoundness::DevMock
        );
        // Real shuffle, mock decrypt, real signing → NOT sound.
        assert_eq!(
            classify_soundness(shuffle::SCHEME, MOCK_DECRYPT, false),
            SchemeSoundness::DevMock
        );
        // An unknown/garbage scheme is never sound.
        assert_eq!(
            classify_soundness("totally-bogus", decrypt::SCHEME, false),
            SchemeSoundness::DevMock
        );
    }

    #[test]
    fn mock_dealt_transcript_verifies_but_reports_devmock() {
        // This is exactly the production `mental_poker_prefer` deal: the mock
        // Mental Poker provider. It MUST verify (consistent replay) yet be flagged
        // DevMock so the `mp-verify` CLI / any UI never claims it is provably fair.
        let request = DealRequest {
            hand_id: "bug108-hand-0001".to_string(),
            table_id: "bug108-table".to_string(),
            num_players: 3,
            button_seat: 0,
            big_blind: 20,
            small_blind: 10,
        };
        let dealt = MentalPokerDealingProvider::deterministic().deal(&request);
        let transcript = dealt
            .transcript
            .expect("mental poker provider always produces a transcript");
        // Sanity: the production mock path declares the mock schemes.
        assert_eq!(transcript.shuffle_scheme, MOCK_SHUFFLE);
        assert_eq!(transcript.decryption_scheme, MOCK_DECRYPT);
        assert!(transcript.key_directory.is_mock);

        let report = verify(&transcript).expect("mock transcript replays consistently");
        assert_eq!(
            report.soundness,
            SchemeSoundness::DevMock,
            "the production mock-crypto deal must NOT be reported as a sound \
             provable-fairness guarantee (BUG-108)"
        );
        assert!(
            !report.is_provably_fair(),
            "is_provably_fair() must be false for a dev-mock replay (BUG-108)"
        );
    }

    #[test]
    fn verify_fairness_fails_closed_on_mock_deal() {
        // FAIL-CLOSED (BUG-108): the production mock deal replays consistently —
        // `verify()` returns Ok — but the strict `verify_fairness()` gate MUST
        // refuse it with `NotProvablyFair`, never Ok, so no consumer (CLI exit
        // code, server export, UI) can treat a mock replay as verified-fair.
        let request = DealRequest {
            hand_id: "bug108-strict-0001".to_string(),
            table_id: "bug108-strict".to_string(),
            num_players: 3,
            button_seat: 0,
            big_blind: 20,
            small_blind: 10,
        };
        let dealt = MentalPokerDealingProvider::deterministic().deal(&request);
        let transcript = dealt
            .transcript
            .expect("mental poker provider always produces a transcript");

        // `verify()` (replay self-check) still accepts it — internal callers rely
        // on this and must NOT regress.
        verify(&transcript).expect("mock transcript still replays consistently");

        // `verify_fairness()` (the fairness gate) fails closed.
        match verify_fairness(&transcript) {
            Err(VerifyFairnessError::NotProvablyFair {
                signing_is_mock, ..
            }) => {
                assert!(
                    signing_is_mock,
                    "the mock deal uses the mock signing directory"
                );
            }
            other => panic!(
                "verify_fairness must fail closed with NotProvablyFair on the mock \
                 deal, got {other:?}"
            ),
        }
    }

    #[test]
    fn verify_fairness_propagates_replay_failure() {
        // A transcript that does not even replay fails with `Replay`, NOT
        // `NotProvablyFair` — the two errors are distinct (corrupt vs. mock).
        let request = DealRequest {
            hand_id: "bug108-corrupt-0001".to_string(),
            table_id: "bug108-corrupt".to_string(),
            num_players: 3,
            button_seat: 0,
            big_blind: 20,
            small_blind: 10,
        };
        let dealt = MentalPokerDealingProvider::deterministic().deal(&request);
        let mut transcript = dealt
            .transcript
            .expect("mental poker provider always produces a transcript");
        // Corrupt the chain so the replay itself fails.
        transcript
            .events
            .truncate(transcript.events.len().saturating_sub(1));

        match verify_fairness(&transcript) {
            Err(VerifyFairnessError::Replay(_)) => {}
            other => panic!("expected a Replay error for a corrupt transcript, got {other:?}"),
        }
    }
}
