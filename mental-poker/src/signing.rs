//! Transcript event signing.
//!
//! [`SignatureProvider`] is the abstraction the protocol and verifier use to
//! sign and verify transcript events. Production deployments must back it with
//! an **asymmetric** signature scheme (Ed25519) so the verifier only ever holds
//! public keys.
//!
//! [`MockSignatureProvider`] is a **dev-only** implementation backed by
//! HMAC-SHA256. It is *symmetric*: the "public key" is the HMAC key itself, so
//! anyone holding it can forge signatures. It exists so the transcript / hash
//! chain / verifier can be exercised without pulling in an asymmetric crypto
//! dependency. **Never enable it in production** — the runtime guard in
//! `crate::guard_provider_allowed` enforces this.

use hmac::{Hmac, Mac};
use serde::{Deserialize, Serialize};
use sha2::Sha256;
use std::collections::BTreeMap;

type HmacSha256 = Hmac<Sha256>;

/// A signature over an event hash, hex-encoded for the transcript.
pub type Signature = String;

/// Signs and verifies transcript events.
///
/// Implementations must be deterministic: signing the same message with the
/// same signer always yields the same signature (required for replayable
/// transcripts and stable tests).
pub trait SignatureProvider {
    /// Sign `message` as `signer`. Returns a hex-encoded signature.
    fn sign(&self, signer: &str, message: &[u8]) -> Signature;

    /// Verify that `signature` is a valid signature over `message` by `signer`.
    fn verify(&self, signer: &str, message: &[u8], signature: &Signature) -> bool;
}

/// **UNSAFE / DEV-ONLY.** Symmetric HMAC-SHA256 "signatures".
///
/// Each party has a secret key; verification recomputes the HMAC. Because the
/// verification key equals the signing key, this provides **no** real
/// non-repudiation. Production must replace this with Ed25519.
#[derive(Debug, Clone)]
pub struct MockSignatureProvider {
    /// signer id → secret key bytes.
    keys: BTreeMap<String, Vec<u8>>,
}

/// The set of verification keys needed to check a transcript, embedded in the
/// exported transcript so the verifier is self-contained.
///
/// For [`MockSignatureProvider`] these are the *secret* HMAC keys (symmetric);
/// for a production Ed25519 provider they are *public* keys (safe to publish).
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct KeyDirectory {
    /// signer id → hex-encoded verification key.
    pub keys: BTreeMap<String, String>,
    /// Marks a directory built from symmetric mock keys.
    pub is_mock: bool,
}

impl MockSignatureProvider {
    /// Build a mock provider, deriving a deterministic per-signer key from a
    /// master seed. `signers` is the full list of signer ids (parties +
    /// `coordinator`).
    pub fn from_seed(master_seed: &[u8], signers: &[String]) -> Self {
        let mut keys = BTreeMap::new();
        for signer in signers {
            let key = crate::hash::ds_hash("mp:mock-sig-key:v1", &[master_seed, signer.as_bytes()]);
            keys.insert(signer.clone(), key.to_vec());
        }
        Self { keys }
    }

    /// Reconstruct a provider from an exported [`KeyDirectory`].
    pub fn from_directory(dir: &KeyDirectory) -> Option<Self> {
        let mut keys = BTreeMap::new();
        for (signer, hex_key) in &dir.keys {
            keys.insert(signer.clone(), hex::decode(hex_key).ok()?);
        }
        Some(Self { keys })
    }

    /// Export the verification keys for the transcript.
    pub fn directory(&self) -> KeyDirectory {
        KeyDirectory {
            keys: self
                .keys
                .iter()
                .map(|(k, v)| (k.clone(), hex::encode(v)))
                .collect(),
            is_mock: true,
        }
    }
}

impl MockSignatureProvider {
    fn sign_with_key(key: &[u8], message: &[u8]) -> String {
        let mut mac = HmacSha256::new_from_slice(key).expect("HMAC accepts any key length");
        mac.update(message);
        hex::encode(mac.finalize().into_bytes())
    }
}

impl SignatureProvider for MockSignatureProvider {
    fn sign(&self, signer: &str, message: &[u8]) -> Signature {
        let key = match self.keys.get(signer) {
            Some(k) => k,
            // An unknown signer produces an obviously-invalid signature rather
            // than panicking; the verifier will reject it.
            None => return "00".to_string(),
        };
        Self::sign_with_key(key, message)
    }

    fn verify(&self, signer: &str, message: &[u8], signature: &Signature) -> bool {
        let key = match self.keys.get(signer) {
            Some(k) => k,
            None => return false,
        };
        let expected = hex::decode(Self::sign_with_key(key, message));
        let actual = hex::decode(signature);
        match (expected, actual) {
            (Ok(e), Ok(a)) => constant_time_eq(&e, &a),
            _ => false,
        }
    }
}

/// Constant-time byte comparison to avoid timing leaks on signature checks.
fn constant_time_eq(a: &[u8], b: &[u8]) -> bool {
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

    fn signers() -> Vec<String> {
        vec!["party:0".into(), "party:1".into(), "coordinator".into()]
    }

    #[test]
    fn sign_then_verify_succeeds() {
        let p = MockSignatureProvider::from_seed(b"seed", &signers());
        let sig = p.sign("party:0", b"hello");
        assert!(p.verify("party:0", b"hello", &sig));
    }

    #[test]
    fn wrong_signer_fails() {
        let p = MockSignatureProvider::from_seed(b"seed", &signers());
        let sig = p.sign("party:0", b"hello");
        assert!(!p.verify("party:1", b"hello", &sig));
    }

    #[test]
    fn tampered_message_fails() {
        let p = MockSignatureProvider::from_seed(b"seed", &signers());
        let sig = p.sign("party:0", b"hello");
        assert!(!p.verify("party:0", b"hell0", &sig));
    }

    #[test]
    fn unknown_signer_fails() {
        let p = MockSignatureProvider::from_seed(b"seed", &signers());
        assert!(!p.verify("party:99", b"x", &"00".to_string()));
    }

    #[test]
    fn directory_round_trip() {
        let p = MockSignatureProvider::from_seed(b"seed", &signers());
        let dir = p.directory();
        assert!(dir.is_mock);
        let p2 = MockSignatureProvider::from_directory(&dir).unwrap();
        let sig = p.sign("party:1", b"msg");
        assert!(p2.verify("party:1", b"msg", &sig));
    }
}
