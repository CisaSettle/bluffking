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
