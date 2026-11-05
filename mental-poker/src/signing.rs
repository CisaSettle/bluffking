//! Signature seam.
//!
//! [`SignatureProvider`] is the abstraction the dealing protocol uses to sign
//! and verify messages. The implementation is the asymmetric Ed25519
//! [`Ed25519SignatureProvider`](crate::crypto_real::Ed25519SignatureProvider),
//! so a verifier only ever holds public keys.

use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

/// A signature over a message, hex-encoded.
pub type Signature = String;

/// Signs and verifies protocol messages.
///
/// Implementations must be deterministic: signing the same message with the
/// same signer always yields the same signature.
pub trait SignatureProvider {
    /// Sign `message` as `signer`. Returns a hex-encoded signature.
    fn sign(&self, signer: &str, message: &[u8]) -> Signature;

    /// Verify that `signature` is a valid signature over `message` by `signer`.
    fn verify(&self, signer: &str, message: &[u8], signature: &Signature) -> bool;
}

/// The set of public verification keys needed to check signatures
/// (signer id → hex-encoded Ed25519 verifying key). Safe to publish.
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct KeyDirectory {
    /// signer id → hex-encoded verification key.
    pub keys: BTreeMap<String, String>,
}
