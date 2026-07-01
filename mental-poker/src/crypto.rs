//! Encrypted deck model and the verifiable-proof provider interfaces.
//!
//! # Safety
//!
//! The provider *interfaces* ([`ShuffleProofProvider`], [`DecryptionProvider`])
//! are production-shaped. The *implementations* in this file
//! ([`MockShuffleProofProvider`], [`MockDecryptionProvider`]) are **dev-only**
//! and are **not** cryptographically sound:
//!
//! - A mock shuffle proof binds a proof object to a specific
//!   `(party, round, input_deck_hash, output_deck_hash)` tuple — so the proof
//!   cannot be lifted onto a different shuffle and transcript tampering of the
//!   proof object is caught. It does **not** prove the output deck is a true
//!   permutation + re-encryption of the input deck. A malicious shuffler that
//!   replaces a card would still produce a "valid" mock proof.
//! - A mock decryption proof likewise binds to `(party, deck_index, card_id,
//!   salt)` but does not prove a correct threshold partial decryption.
//!
//! The real path (`crypto_real/`) provides a verifiable re-encryption shuffle +
//! threshold ElGamal / Chaum–Pedersen decryption; this Mock does not.

use crate::card_id::CardId;
use crate::hash::{ds_hash, hex_hash, Hash};
use serde::{Deserialize, Serialize};

/// One encrypted card: a hiding, binding commitment to its plaintext.
pub type EncCard = Hash;

/// An encrypted 52-card deck — a list of per-card commitments.
pub type EncDeck = Vec<EncCard>;

/// 32 bytes of commitment randomness ("salt"), kept secret until the card opens.
pub type Salt = [u8; 32];

/// Hiding/binding commitment to a single card.
///
/// `commit = H("mp:card-commit:v1", [card_id], salt)`. The salt makes it
/// hiding (the coordinator cannot recover `card_id`); SHA-256 makes it binding
/// (the committer cannot later open it to a different card).
pub fn card_commit(card_id: CardId, salt: &Salt) -> EncCard {
    ds_hash("mp:card-commit:v1", &[&[card_id], salt])
}

/// Hash of a full encrypted deck — the value published as the deck commitment.
pub fn deck_hash(deck: &EncDeck) -> Hash {
    let refs: Vec<&[u8]> = deck.iter().map(|c| c.as_slice()).collect();
    ds_hash("mp:deck-hash:v1", &refs)
}

// ---------------------------------------------------------------------------
// Shuffle proof
// ---------------------------------------------------------------------------

/// A verifiable shuffle proof, embedded in the transcript as JSON.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ShuffleProof {
    /// Proof scheme identifier (e.g. `mock-shuffle-v1`).
    pub scheme: String,
    /// Hex hash of the deck this shuffle consumed.
    pub input_deck_hash: String,
    /// Hex hash of the deck this shuffle produced.
    pub output_deck_hash: String,
    /// Scheme-specific attestation bytes (hex).
    pub attestation: String,
}

/// Produces and verifies shuffle proofs.
///
/// `verify_shuffle` MUST be callable offline by the verifier with no secret
/// state beyond the proof object and the public deck hashes.
pub trait ShuffleProofProvider {
    /// Identifier of the proof scheme this provider implements.
    fn scheme(&self) -> &'static str;

    /// Prove that `output_hash` is a valid re-encryption shuffle of `input_hash`
    /// performed by `party` in `round`.
    fn prove_shuffle(
        &self,
        party: &str,
        round: u32,
        input_hash: &Hash,
        output_hash: &Hash,
    ) -> ShuffleProof;

    /// Verify a shuffle proof against the public deck hashes.
    ///
    /// `expected_joint_key` is the DKG-derived joint public key the shuffle MUST
    /// have been performed under, as canonical 64-hex (e.g. the `Σ Q_i` an
    /// independent verifier reconstructs from the DKG). For schemes whose
    /// soundness depends on the encryption key (the real re-encryption shuffle),
    /// the verifier MUST bind the proof to this key rather than trusting a joint
    /// key carried inside the attestation — otherwise a shuffler could prove a
    /// shuffle under a key it controls (audit F2). `None` means "no external key
    /// to bind" (the dev-only mock, which carries no key, ignores it).
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

/// **UNSAFE / DEV-ONLY** shuffle proof provider. See module docs.
#[derive(Debug, Clone, Default)]
pub struct MockShuffleProofProvider;

impl MockShuffleProofProvider {
    fn attestation(party: &str, round: u32, input_hash: &Hash, output_hash: &Hash) -> String {
        hex_hash(&ds_hash(
            "mp:shuffle-proof:v1",
            &[
                party.as_bytes(),
                &round.to_le_bytes(),
                input_hash,
                output_hash,
            ],
        ))
    }
}

impl ShuffleProofProvider for MockShuffleProofProvider {
    fn scheme(&self) -> &'static str {
        "mock-shuffle-v1"
    }

    fn prove_shuffle(
        &self,
        party: &str,
        round: u32,
        input_hash: &Hash,
        output_hash: &Hash,
    ) -> ShuffleProof {
        ShuffleProof {
            scheme: self.scheme().to_string(),
            input_deck_hash: hex_hash(input_hash),
            output_deck_hash: hex_hash(output_hash),
            attestation: Self::attestation(party, round, input_hash, output_hash),
        }
    }

    fn verify_shuffle(
        &self,
        party: &str,
        round: u32,
        input_hash: &Hash,
        output_hash: &Hash,
        _expected_joint_key: Option<&str>,
        proof: &ShuffleProof,
    ) -> bool {
        // The dev-only mock carries no encryption key, so there is nothing to
        // bind `expected_joint_key` against (the mock is not server-blind and is
        // never used in the real-crypto path — see module docs).
        proof.scheme == self.scheme()
            && proof.input_deck_hash == hex_hash(input_hash)
            && proof.output_deck_hash == hex_hash(output_hash)
            && constant_time_str_eq(
                &proof.attestation,
                &Self::attestation(party, round, input_hash, output_hash),
            )
    }
}

// ---------------------------------------------------------------------------
// Decryption proof
// ---------------------------------------------------------------------------

/// A verifiable decryption proof, embedded in the transcript as JSON.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DecryptionProof {
    /// Proof scheme identifier (e.g. `mock-decrypt-v1`).
    pub scheme: String,
    /// Scheme-specific attestation bytes (hex).
    pub attestation: String,
}

/// Produces and verifies card-decryption proofs.
pub trait DecryptionProvider {
    /// Identifier of the proof scheme this provider implements.
    fn scheme(&self) -> &'static str;

    /// Prove that opening `deck_index` to `(card_id, salt)` is a correct
    /// decryption performed by `party`.
    fn prove_decryption(
        &self,
        party: &str,
        deck_index: u32,
        card_id: CardId,
        salt: &Salt,
    ) -> DecryptionProof;

    /// Verify a decryption proof.
    fn verify_decryption(
        &self,
        party: &str,
        deck_index: u32,
        card_id: CardId,
        salt: &Salt,
        proof: &DecryptionProof,
    ) -> bool;
}

/// **UNSAFE / DEV-ONLY** decryption proof provider. See module docs.
#[derive(Debug, Clone, Default)]
pub struct MockDecryptionProvider;

impl MockDecryptionProvider {
    fn attestation(party: &str, deck_index: u32, card_id: CardId, salt: &Salt) -> String {
        hex_hash(&ds_hash(
            "mp:decrypt-proof:v1",
            &[
                party.as_bytes(),
                &deck_index.to_le_bytes(),
                &[card_id],
                salt,
            ],
        ))
    }
}

impl DecryptionProvider for MockDecryptionProvider {
    fn scheme(&self) -> &'static str {
        "mock-decrypt-v1"
    }

    fn prove_decryption(
        &self,
        party: &str,
        deck_index: u32,
        card_id: CardId,
        salt: &Salt,
    ) -> DecryptionProof {
        DecryptionProof {
            scheme: self.scheme().to_string(),
            attestation: Self::attestation(party, deck_index, card_id, salt),
        }
    }

    fn verify_decryption(
        &self,
        party: &str,
        deck_index: u32,
        card_id: CardId,
        salt: &Salt,
        proof: &DecryptionProof,
    ) -> bool {
        proof.scheme == self.scheme()
            && constant_time_str_eq(
                &proof.attestation,
                &Self::attestation(party, deck_index, card_id, salt),
            )
    }
}

/// Constant-time comparison of two attestation strings.
///
/// The mock attestations are public, deterministically-derivable values, so no
/// secret leaks through an early-exit `==` today. We still match the
/// constant-time discipline of [`crate::signing`]'s signature check so the
/// production swap-in — where attestations become secret-derived — inherits the
/// safe comparison instead of copying a timing-leaky `==`.
fn constant_time_str_eq(a: &str, b: &str) -> bool {
    let (a, b) = (a.as_bytes(), b.as_bytes());
    if a.len() != b.len() {
        return false;
    }
    let mut diff = 0u8;
    for (x, y) in a.iter().zip(b.iter()) {
        diff |= x ^ y;
    }
    diff == 0
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn card_commit_is_hiding_and_binding() {
        let s1 = [1u8; 32];
        let s2 = [2u8; 32];
        // Different salt → different commitment (hiding).
        assert_ne!(card_commit(7, &s1), card_commit(7, &s2));
        // Different card, same salt → different commitment (binding).
        assert_ne!(card_commit(7, &s1), card_commit(8, &s1));
        // Deterministic.
        assert_eq!(card_commit(7, &s1), card_commit(7, &s1));
    }

    #[test]
    fn shuffle_proof_round_trips() {
        let p = MockShuffleProofProvider;
        let ih = ds_hash("d", &[b"in"]);
        let oh = ds_hash("d", &[b"out"]);
        let proof = p.prove_shuffle("party:0", 0, &ih, &oh);
        assert!(p.verify_shuffle("party:0", 0, &ih, &oh, None, &proof));
    }

    #[test]
    fn shuffle_proof_rejects_tampered_attestation() {
        let p = MockShuffleProofProvider;
        let ih = ds_hash("d", &[b"in"]);
        let oh = ds_hash("d", &[b"out"]);
        let mut proof = p.prove_shuffle("party:0", 0, &ih, &oh);
        proof.attestation = hex_hash(&ds_hash("d", &[b"forged"]));
        assert!(!p.verify_shuffle("party:0", 0, &ih, &oh, None, &proof));
    }

    #[test]
    fn shuffle_proof_rejects_wrong_decks() {
        let p = MockShuffleProofProvider;
        let ih = ds_hash("d", &[b"in"]);
        let oh = ds_hash("d", &[b"out"]);
        let other = ds_hash("d", &[b"other"]);
        let proof = p.prove_shuffle("party:0", 0, &ih, &oh);
        assert!(!p.verify_shuffle("party:0", 0, &ih, &other, None, &proof));
        assert!(!p.verify_shuffle("party:0", 1, &ih, &oh, None, &proof));
    }

    #[test]
    fn constant_time_str_eq_matches_string_equality() {
        assert!(constant_time_str_eq("abcd", "abcd"));
        assert!(!constant_time_str_eq("abcd", "abce"));
        // Length mismatch is rejected without panicking (and short-circuits).
        assert!(!constant_time_str_eq("abc", "abcd"));
        assert!(constant_time_str_eq("", ""));
    }

    #[test]
    fn decryption_proof_round_trips() {
        let p = MockDecryptionProvider;
        let salt = [9u8; 32];
        let proof = p.prove_decryption("party:1", 3, 42, &salt);
        assert!(p.verify_decryption("party:1", 3, 42, &salt, &proof));
        // Wrong card id is rejected.
        assert!(!p.verify_decryption("party:1", 3, 41, &salt, &proof));
    }
}
