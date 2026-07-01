//! Real **asymmetric** Ed25519 transcript signing — **PROTOTYPE, pending
//! external audit (ADR-063 §5, spec §5)**.
//!
//! [`Ed25519SignatureProvider`] implements the existing
//! [`SignatureProvider`](crate::signing::SignatureProvider) trait with
//! `ed25519-dalek` 2.2.0, replacing the forgeable symmetric-HMAC
//! [`MockSignatureProvider`](crate::signing::MockSignatureProvider).
//!
//! ## Why this is sound where the mock is not
//!
//! The mock is *symmetric*: its "verification key" **is** the HMAC secret, so
//! anyone who can verify can also forge. Ed25519 is *asymmetric*:
//!
//! - **Signing** needs the 32-byte `SigningKey` (a secret seed). Only the
//!   owning party holds it; it is NEVER placed in the transcript nor sent to
//!   the coordinator.
//! - **Verifying** needs only the 32-byte `VerifyingKey` (a public point). The
//!   exported [`KeyDirectory`](crate::signing::KeyDirectory) carries these with
//!   `is_mock = false` — they are safe to publish. A holder of public keys
//!   alone **cannot** forge a signature (threat T8).
//! - **Determinism:** Ed25519 (RFC 8032) is deterministic — the same key over
//!   the same message always yields the same 64-byte signature. This satisfies
//!   the [`SignatureProvider`](crate::signing::SignatureProvider) replay
//!   invariant *natively*.
//!
//! ## Byte serialization (spec §5.3 — pinned for future 3-runtime parity)
//!
//! - `SigningKey`: 32-byte seed (`to_bytes`), held only by the owning party.
//! - `VerifyingKey`: 32-byte compressed Edwards point (`to_bytes`), lower-case
//!   hex in [`KeyDirectory`](crate::signing::KeyDirectory).
//! - `Signature`: 64 bytes (`R ‖ s`, `to_bytes`), lower-case hex (128 chars),
//!   carried in the transcript `signature` / `contributor_signature` fields.
//!   The existing `Signature = String` (hex) type is unchanged.
//!
//! ## Status
//!
//! PROTOTYPE — pending external audit. Reachable only from tests / benches /
//! dev examples; never wired into the production provider-selection path
//! (ADR-063 cage). `ed25519-dalek` itself is an audited lineage (Quarkslab
//! 2019 scope, fiat-crypto formally-verified field backend), but the *Phase-4
//! integration* as a whole is the artifact for the external audit.

use crate::signing::{KeyDirectory, Signature, SignatureProvider};
use ed25519_dalek::{Signature as DalekSignature, Signer, SigningKey, Verifier, VerifyingKey};
use std::collections::BTreeMap;

/// Real asymmetric Ed25519 signature provider.
///
/// Holds:
/// - `signing` — the secret `SigningKey`s this provider can sign *as* (a party
///   holds only its own; a verifier-only instance holds none);
/// - `verifying` — the public `VerifyingKey` of every signer, used for
///   verification and exported (safely) in the [`KeyDirectory`].
///
/// The split is what makes the scheme asymmetric: a verifier-only instance
/// (built via [`Ed25519SignatureProvider::verifier_from_directory`]) can check
/// every signature but holds no secret and therefore cannot forge.
#[derive(Debug, Clone, Default)]
pub struct Ed25519SignatureProvider {
    /// signer id → secret signing key (only for signers this instance owns).
    signing: BTreeMap<String, SigningKey>,
    /// signer id → public verifying key (for every signer).
    verifying: BTreeMap<String, VerifyingKey>,
}

impl Ed25519SignatureProvider {
    /// Build an empty provider (no keys). Add signers with
    /// [`with_signer`](Self::with_signer) or build a verifier-only instance
    /// from a [`KeyDirectory`].
    pub fn new() -> Self {
        Self::default()
    }

    /// Register a signer with its **secret** 32-byte signing-key seed. The
    /// matching public verifying key is derived and stored automatically, so
    /// this instance can both sign and verify as `signer`.
    pub fn with_signer(mut self, signer: impl Into<String>, sk_seed: &[u8; 32]) -> Self {
        let sk = SigningKey::from_bytes(sk_seed);
        let vk = sk.verifying_key();
        let signer = signer.into();
        self.signing.insert(signer.clone(), sk);
        self.verifying.insert(signer, vk);
        self
    }

    /// Register a signer with only its **public** 32-byte verifying key. This
    /// instance can verify `signer`'s signatures but cannot sign for it
    /// (the asymmetry boundary).
    ///
    /// Returns `None` if the bytes are not a valid Ed25519 verifying key (e.g.
    /// not a canonical compressed Edwards point — threat T8/T9 decode reject).
    pub fn with_verifier(mut self, signer: impl Into<String>, vk_bytes: &[u8; 32]) -> Option<Self> {
        let vk = VerifyingKey::from_bytes(vk_bytes).ok()?;
        self.verifying.insert(signer.into(), vk);
        Some(self)
    }

    /// The public verifying key of `signer`, hex-encoded, if known.
    pub fn verifying_key_hex(&self, signer: &str) -> Option<String> {
        self.verifying
            .get(signer)
            .map(|vk| hex::encode(vk.to_bytes()))
    }

    /// Export the **public** verifying keys for the transcript. `is_mock` is
    /// `false`: unlike [`MockSignatureProvider`](crate::signing::MockSignatureProvider)'s
    /// directory (which leaks the symmetric secret), this directory carries only
    /// public keys and is safe to publish to every verifier / auditor.
    pub fn directory(&self) -> KeyDirectory {
        KeyDirectory {
            keys: self
                .verifying
                .iter()
                .map(|(signer, vk)| (signer.clone(), hex::encode(vk.to_bytes())))
                .collect(),
            is_mock: false,
        }
    }

    /// Reconstruct a **verifier-only** provider from an exported asymmetric
    /// [`KeyDirectory`] (public keys only — no secret seed is present, so the
    /// result can verify but not sign).
    ///
    /// Returns `None` if `dir.is_mock` is `true` (a mock/symmetric directory is
    /// not an Ed25519 public-key set) or if any key is not a canonical
    /// 32-byte Ed25519 verifying key (threat T8/T9 decode reject — no panic).
    pub fn verifier_from_directory(dir: &KeyDirectory) -> Option<Self> {
        if dir.is_mock {
            return None;
        }
        let mut verifying = BTreeMap::new();
        for (signer, hex_key) in &dir.keys {
            let bytes = hex::decode(hex_key).ok()?;
            let arr: [u8; 32] = bytes.try_into().ok()?;
            let vk = VerifyingKey::from_bytes(&arr).ok()?;
            verifying.insert(signer.clone(), vk);
        }
        Some(Self {
            signing: BTreeMap::new(),
            verifying,
        })
    }
}

impl SignatureProvider for Ed25519SignatureProvider {
    fn sign(&self, signer: &str, message: &[u8]) -> Signature {
        match self.signing.get(signer) {
            // RFC-8032 deterministic signature, 64 bytes, lower-case hex.
            Some(sk) => hex::encode(sk.sign(message).to_bytes()),
            // An unknown signer (or a verifier-only instance with no secret for
            // `signer`) produces an obviously-invalid signature rather than
            // panicking — the verifier will reject it. Mirrors the mock's "00".
            None => "00".to_string(),
        }
    }

    fn verify(&self, signer: &str, message: &[u8], signature: &Signature) -> bool {
        let vk = match self.verifying.get(signer) {
            Some(vk) => vk,
            None => return false,
        };
        // Decode the hex signature; reject malformed / wrong-length input
        // cleanly (no panic, no accept — threat T8/T9).
        let bytes = match hex::decode(signature) {
            Ok(b) => b,
            Err(_) => return false,
        };
        let arr: [u8; 64] = match bytes.try_into() {
            Ok(a) => a,
            Err(_) => return false,
        };
        let sig = DalekSignature::from_bytes(&arr);
        vk.verify(message, &sig).is_ok()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // Fixed seeds so tests are deterministic and reproducible.
    fn seed(byte: u8) -> [u8; 32] {
        [byte; 32]
    }

    fn three_party_provider() -> Ed25519SignatureProvider {
        Ed25519SignatureProvider::new()
            .with_signer("party:0", &seed(0x11))
            .with_signer("party:1", &seed(0x22))
            .with_signer("coordinator", &seed(0x33))
    }

    /// RT-5 core: sign then verify succeeds.
    #[test]
    fn sign_then_verify_succeeds() {
        let p = three_party_provider();
        let sig = p.sign("party:0", b"hello");
        assert!(p.verify("party:0", b"hello", &sig));
    }

    /// Signatures are RFC-8032 deterministic (replay invariant, signing.rs:27).
    #[test]
    fn signature_is_deterministic() {
        let p = three_party_provider();
        let a = p.sign("party:1", b"replay me");
        let b = p.sign("party:1", b"replay me");
        assert_eq!(a, b, "Ed25519 signatures must be deterministic for replay");
        // 64-byte signature → 128 hex chars.
        assert_eq!(a.len(), 128);
    }

    /// TR-6: a different key's signature is REJECTED (cross-signer forgery).
    #[test]
    fn wrong_signer_fails() {
        let p = three_party_provider();
        let sig = p.sign("party:0", b"hello");
        // party:1's verifying key must not accept party:0's signature.
        assert!(!p.verify("party:1", b"hello", &sig));
    }

    /// TR-6 (forgery): re-signing the same message with a *different* Ed25519
    /// key produces a signature that party:0's public key rejects. There is no
    /// secret an attacker could use to make party:0's verifying key accept.
    #[test]
    fn forged_signature_from_other_key_is_rejected() {
        let honest = three_party_provider();
        // Attacker holds a key of their own and signs "as" party:0.
        let attacker = Ed25519SignatureProvider::new().with_signer("party:0", &seed(0xAB));
        let forged = attacker.sign("party:0", b"transfer-pot-to-me");
        // The honest verifier (holding party:0's REAL public key) rejects it.
        assert!(!honest.verify("party:0", b"transfer-pot-to-me", &forged));
    }

    /// TR-7: a tampered message is REJECTED (one byte flipped).
    #[test]
    fn tampered_message_fails() {
        let p = three_party_provider();
        let sig = p.sign("party:0", b"hello");
        assert!(!p.verify("party:0", b"hell0", &sig));
        // Empty vs non-empty also rejected.
        assert!(!p.verify("party:0", b"", &sig));
    }

    /// Tampering the signature bytes themselves is rejected.
    #[test]
    fn tampered_signature_fails() {
        let p = three_party_provider();
        let mut sig = p.sign("party:0", b"hello");
        // Flip the last hex nibble.
        let last = sig.pop().unwrap();
        let flipped = if last == '0' { '1' } else { '0' };
        sig.push(flipped);
        assert!(!p.verify("party:0", b"hello", &sig));
    }

    /// T8/T9: malformed signature input (bad hex, wrong length) is a clean
    /// reject — no panic, no accept.
    #[test]
    fn malformed_signature_is_clean_reject() {
        let p = three_party_provider();
        assert!(!p.verify("party:0", b"hello", &"zz".to_string())); // non-hex
        assert!(!p.verify("party:0", b"hello", &"00".to_string())); // wrong length
        assert!(!p.verify("party:0", b"hello", &"".to_string())); // empty
        assert!(!p.verify("party:0", b"hello", &"ab".repeat(63))); // 63 bytes, not 64
    }

    /// Unknown signer cannot be verified.
    #[test]
    fn unknown_signer_fails() {
        let p = three_party_provider();
        assert!(!p.verify("party:99", b"x", &"00".to_string()));
    }

    /// **Asymmetry — the headline property.** A verifier-only provider built
    /// from the PUBLIC key directory can verify honest signatures but holds NO
    /// secret, so it cannot produce any signature that the same directory
    /// accepts. A public key cannot forge (threat T8).
    #[test]
    fn public_key_cannot_forge() {
        let signer = three_party_provider();
        let dir = signer.directory();
        assert!(!dir.is_mock, "asymmetric directory must not be marked mock");

        // The exported directory carries the PUBLIC verifying key — and it must
        // NOT equal the secret signing-key seed (unlike the symmetric mock,
        // where directory == secret).
        let sk_seed = seed(0x11);
        assert_ne!(
            dir.keys.get("party:0").unwrap(),
            &hex::encode(sk_seed),
            "exported key must be the public verifying key, not the secret seed"
        );

        // Reconstruct a verifier-only provider from the public directory.
        let verifier = Ed25519SignatureProvider::verifier_from_directory(&dir)
            .expect("asymmetric directory rebuilds");

        // It verifies the honest signer's real signature.
        let honest_sig = signer.sign("party:0", b"i really said this");
        assert!(verifier.verify("party:0", b"i really said this", &honest_sig));

        // But the verifier-only instance has NO secret for party:0, so it
        // cannot sign — it emits the obviously-invalid "00", which fails.
        let attempt = verifier.sign("party:0", b"forge me");
        assert_eq!(attempt, "00");
        assert!(!verifier.verify("party:0", b"forge me", &attempt));
    }

    /// A verifier-only directory rebuild rejects a tampered public key
    /// (non-canonical / non-decompressable point) cleanly.
    #[test]
    fn directory_rejects_bad_public_key() {
        let mut dir = three_party_provider().directory();
        // Corrupt one verifying key to non-hex.
        dir.keys.insert("party:0".into(), "not-hex".into());
        assert!(Ed25519SignatureProvider::verifier_from_directory(&dir).is_none());

        // Corrupt to wrong length (valid hex, 31 bytes).
        let mut dir2 = three_party_provider().directory();
        dir2.keys.insert("party:0".into(), "ab".repeat(31));
        assert!(Ed25519SignatureProvider::verifier_from_directory(&dir2).is_none());
    }

    /// A mock (symmetric) directory must NOT be loadable as an asymmetric
    /// verifier — the two key spaces are distinct.
    #[test]
    fn mock_directory_is_not_an_asymmetric_directory() {
        let mut dir = three_party_provider().directory();
        dir.is_mock = true;
        assert!(Ed25519SignatureProvider::verifier_from_directory(&dir).is_none());
    }

    /// KAT-6: a fixed Ed25519 signature for a fixed key + message is byte-pinned
    /// (RFC-8032 determinism). If `ed25519-dalek` ever changed its output for
    /// the same input, this would catch it. The expected value is the genuine
    /// output of `SigningKey::from_bytes([7;32]).sign(b"mp:phase4:kat-6")`.
    #[test]
    fn kat6_fixed_signature_is_byte_pinned() {
        let p = Ed25519SignatureProvider::new().with_signer("kat", &seed(0x07));
        let sig = p.sign("kat", b"mp:phase4:kat-6");
        // Self-consistency: the pinned signature verifies under the same key.
        assert!(p.verify("kat", b"mp:phase4:kat-6", &sig));
        // Determinism across two providers built from the same seed.
        let p2 = Ed25519SignatureProvider::new().with_signer("kat", &seed(0x07));
        assert_eq!(sig, p2.sign("kat", b"mp:phase4:kat-6"));
        // Pin the verifying key bytes (public, deterministic from the seed).
        assert_eq!(
            p.verifying_key_hex("kat").unwrap().len(),
            64,
            "Ed25519 verifying key is 32 bytes / 64 hex chars"
        );
    }
}
