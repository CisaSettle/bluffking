//! Verifiable-shuffle proof interface.
//!
//! [`ShuffleProof`] is the wire/proof object and [`ShuffleProofProvider`] the
//! verify seam (proofs are produced by `Shuffle::prove*` directly). The implementation is the real re-encryption shuffle in
//! [`crate::crypto_real::shuffle`], used by engine-blind dealing and by the
//! `pf2` / card-proof verifiable-dealing bundles.

use crate::hash::Hash;
use serde::{Deserialize, Serialize};

// ---------------------------------------------------------------------------
// Shuffle proof
// ---------------------------------------------------------------------------

/// A verifiable shuffle proof, carried as JSON.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ShuffleProof {
    /// Proof scheme identifier (e.g. `reenc-shuffle-ristretto-v2`).
    pub scheme: String,
    /// Hex hash of the deck this shuffle consumed.
    pub input_deck_hash: String,
    /// Hex hash of the deck this shuffle produced.
    pub output_deck_hash: String,
    /// Scheme-specific attestation bytes (hex).
    pub attestation: String,
}

/// Verifies shuffle proofs.
///
/// `verify_shuffle` MUST be callable offline by the verifier with no secret
/// state beyond the proof object and the public deck hashes.
pub trait ShuffleProofProvider {
    /// Verify a shuffle proof against the public deck hashes.
    ///
    /// `expected_joint_key` is the DKG-derived joint public key the shuffle MUST
    /// have been performed under, as canonical 64-hex (e.g. the `Σ Q_i` an
    /// independent verifier reconstructs from the DKG). For schemes whose
    /// soundness depends on the encryption key (the real re-encryption shuffle),
    /// the verifier MUST bind the proof to this key rather than trusting a joint
    /// key carried inside the attestation — otherwise a shuffler could prove a
    /// shuffle under a key it controls (audit F2). `None` means "no external key
    /// to bind".
    fn verify_shuffle(
        &self,
        party: &str,
        round: u32,
        input_hash: &Hash,
        output_hash: &Hash,
        expected_joint_key: Option<&str>,
        proof: &ShuffleProof,
    ) -> bool;
}
