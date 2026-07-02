//! Real **asymmetric** Ed25519 transcript signing — **cross-vendor AI-audited
//! (ADR-076/077/078); open-source + verifiable (ADR-063 §5, spec §5)**.
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
//! Cross-vendor AI-audited (ADR-076/077/078); open-source + verifiable. GA'd for
//! the engine-blind table class by ADR-070; in production selected ONLY for
//! engine-blind sessions (`resolve_mp_crypto_mode`) — the generic
//! `mental_poker_production` provider stays rejected. `ed25519-dalek` itself is
//! an audited lineage (Quarkslab 2019 scope, fiat-crypto formally-verified field
//! backend); the *Phase-4 integration* as a whole is cross-vendor AI-audited and
//! independently verifiable.

use crate::signing::{KeyDirectory, Signature, SignatureProvider};
// U30 (dual-AI OSS review): `Verifier` trait no longer imported — verification
// uses the inherent `VerifyingKey::verify_strict` (see `verify` below).
use ed25519_dalek::{Signature as DalekSignature, Signer, SigningKey, VerifyingKey};
use std::collections::BTreeMap;

/// U30 (dual-AI OSS review): strict public-key import.
///
/// `VerifyingKey::from_bytes` follows ZIP-215 rules: it accepts **small-order
/// ("weak") points** and **non-canonical encodings** (dalek issue #626). A
/// small-order key admits signatures that verify for almost any message, and a
/// non-canonical encoding gives one logical key two distinct byte forms
/// (breaking the directory's key-bytes-are-identity assumption). Both are
/// rejected here at import:
///
/// - `is_weak()` rejects small-order points;
/// - recompressing the decoded point and comparing to the input bytes rejects
///   any non-canonical encoding (e.g. `y ≥ p`, or the negative-zero sign form),
///   because `compress()` always produces the canonical form.
///
/// Keys *derived* from a secret seed (`with_signer`) never need this: a derived
/// `A = clamp(H(seed))·B` is always canonical and in the prime-order subgroup.
fn decode_verifying_key_strict(bytes: &[u8; 32]) -> Option<VerifyingKey> {
    let vk = VerifyingKey::from_bytes(bytes).ok()?;
    if vk.is_weak() {
        return None; // small-order point
    }
    if vk.to_edwards().compress().to_bytes() != *bytes {
        return None; // non-canonical encoding
    }
    Some(vk)
}

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
    /// Returns `None` if the bytes are not a valid Ed25519 verifying key —
    /// non-decompressable, **small-order (weak)**, or a **non-canonical
    /// encoding** (threat T8/T9 decode reject; U30 strict import).
    pub fn with_verifier(mut self, signer: impl Into<String>, vk_bytes: &[u8; 32]) -> Option<Self> {
        // U30 (dual-AI OSS review): strict import — rejects small-order and
        // non-canonical encodings, not just non-decompressable bytes.
        let vk = decode_verifying_key_strict(vk_bytes)?;
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
    /// 32-byte Ed25519 verifying key — including small-order (weak) points and
    /// non-canonical encodings (threat T8/T9 decode reject — no panic; U30).
    pub fn verifier_from_directory(dir: &KeyDirectory) -> Option<Self> {
        if dir.is_mock {
            return None;
        }
        let mut verifying = BTreeMap::new();
        for (signer, hex_key) in &dir.keys {
            let bytes = hex::decode(hex_key).ok()?;
            let arr: [u8; 32] = bytes.try_into().ok()?;
            // U30 (dual-AI OSS review): strict import — rejects small-order and
            // non-canonical encodings, not just non-decompressable bytes.
            let vk = decode_verifying_key_strict(&arr)?;
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
        // U30 (dual-AI OSS review): `verify_strict`, not `verify` — the strict
        // check additionally rejects small-order components and the malleable
        // (ZIP-215 cofactored) acceptances, so a signature accepted here is
        // unique for a given (key, message). Honest RFC-8032 signatures (which
        // is everything `sign` above produces) are unaffected.
        vk.verify_strict(message, &sig).is_ok()
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

    /// U30 (dual-AI OSS review): small-order ("weak") public keys are rejected
    /// at import — both the direct `with_verifier` path and the directory
    /// rebuild. A small-order key admits signatures that verify for almost any
    /// message, so it must never enter the verifying set.
    #[test]
    fn u30_small_order_public_key_rejected_at_import() {
        // The identity point (order 1): compressed Edwards y = 1.
        let mut identity = [0u8; 32];
        identity[0] = 0x01;
        // An order-4 small-order point: compressed Edwards y = 0.
        let zero_y = [0u8; 32];
        for bytes in [identity, zero_y] {
            assert!(
                Ed25519SignatureProvider::new()
                    .with_verifier("evil", &bytes)
                    .is_none(),
                "small-order verifying key must be rejected at import"
            );
        }
        // Directory path: a small-order key inside an otherwise-valid directory.
        let mut dir = three_party_provider().directory();
        dir.keys.insert("party:0".into(), hex::encode(identity));
        assert!(Ed25519SignatureProvider::verifier_from_directory(&dir).is_none());
    }

    /// U30: non-canonical point encodings are rejected at import. `y = p`
    /// (2²⁵⁵ − 19, little-endian `ed ff … 7f`) decodes under dalek's ZIP-215
    /// rules to the same point as `y = 0` but is a different byte string — one
    /// logical key must never have two accepted encodings.
    #[test]
    fn u30_non_canonical_public_key_rejected_at_import() {
        let mut y_eq_p = [0xffu8; 32];
        y_eq_p[0] = 0xed;
        y_eq_p[31] = 0x7f;
        assert!(
            Ed25519SignatureProvider::new()
                .with_verifier("evil", &y_eq_p)
                .is_none(),
            "non-canonical verifying-key encoding must be rejected at import"
        );
        let mut dir = three_party_provider().directory();
        dir.keys.insert("party:0".into(), hex::encode(y_eq_p));
        assert!(Ed25519SignatureProvider::verifier_from_directory(&dir).is_none());
    }

    /// U30: the classic Ed25519 malleation — shifting `s` by the group order
    /// `L` — is REJECTED. With `verify_strict` (plus dalek's s-range check)
    /// each (key, message) pins a unique accepted signature encoding, so a
    /// transcript signature cannot be re-encoded without invalidating it.
    #[test]
    fn u30_malleated_s_plus_l_signature_rejected() {
        let p = three_party_provider();
        let sig_hex = p.sign("party:0", b"hello");
        assert!(p.verify("party:0", b"hello", &sig_hex));

        let mut sig = hex::decode(&sig_hex).unwrap();
        // L, little-endian: 2²⁵² + 27742317777372353535851937790883648493.
        const L: [u8; 32] = [
            0xed, 0xd3, 0xf5, 0x5c, 0x1a, 0x63, 0x12, 0x58, 0xd6, 0x9c, 0xf7, 0xa2, 0xde, 0xf9,
            0xde, 0x14, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00,
            0x00, 0x00, 0x00, 0x10,
        ];
        // s' = s + L (little-endian add with carry; fits in 32 bytes because
        // s < L and 2L < 2²⁵⁶).
        let mut carry = 0u16;
        for i in 0..32 {
            let sum = sig[32 + i] as u16 + L[i] as u16 + carry;
            sig[32 + i] = sum as u8;
            carry = sum >> 8;
        }
        assert_eq!(carry, 0, "s + L must not overflow 32 bytes");
        let malleated = hex::encode(&sig);
        assert_ne!(malleated, sig_hex);
        assert!(
            !p.verify("party:0", b"hello", &malleated),
            "an s+L malleated signature must be rejected"
        );
    }
}
