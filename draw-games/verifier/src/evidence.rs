//! Server-signed evidence verified against a caller's pinned Ed25519 key.
use crate::proof::{seed_bytes, sha256_fields};
use ed25519_dalek::{Signature, Signer, SigningKey, VerifyingKey};
use serde::{de::DeserializeOwned, Deserialize, Serialize};

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SignedEvidence {
    pub public_key: String,
    /// Hex of the exact signed UTF-8 JSON bytes. JSON key ordering is irrelevant
    /// to verification because the recipient checks the original bytes.
    pub payload: String,
    pub signature: String,
}

fn digest(payload: &[u8]) -> [u8; 32] {
    sha256_fields(&[b"bluffking-signed-evidence-v1", payload])
}

impl SignedEvidence {
    pub fn verify<T: DeserializeOwned>(&self, expected_public_key: &str) -> Result<T, String> {
        if self.public_key != expected_public_key {
            return Err("evidence key does not match the pinned key".into());
        }
        let key = VerifyingKey::from_bytes(&seed_bytes(expected_public_key)?)
            .map_err(|_| "invalid evidence public key")?;
        if self.signature.len() != Signature::BYTE_SIZE * 2 {
            return Err("invalid evidence signature length".into());
        }
        let signature =
            hex::decode(&self.signature).map_err(|_| "invalid evidence signature hex")?;
        let signature =
            Signature::from_slice(&signature).map_err(|_| "invalid evidence signature")?;
        let payload = hex::decode(&self.payload).map_err(|_| "invalid evidence payload hex")?;
        key.verify_strict(&digest(&payload), &signature)
            .map_err(|_| "evidence signature verification failed")?;
        serde_json::from_slice(&payload).map_err(|error| error.to_string())
    }
}

pub struct EvidenceSigner(SigningKey);

impl EvidenceSigner {
    pub fn new(seed: &[u8; 32]) -> Self {
        Self(SigningKey::from_bytes(seed))
    }

    pub fn public_key(&self) -> String {
        hex::encode(self.0.verifying_key().to_bytes())
    }

    pub fn sign<T: Serialize>(&self, claim: &T) -> Result<SignedEvidence, String> {
        let payload = serde_json::to_vec(claim).map_err(|error| error.to_string())?;
        Ok(SignedEvidence {
            public_key: self.public_key(),
            payload: hex::encode(&payload),
            signature: hex::encode(self.0.sign(&digest(&payload)).to_bytes()),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::{json, Value};

    #[test]
    fn verifies_exact_payload_against_the_pinned_key() {
        let signer = EvidenceSigner::new(&[91; 32]);
        let claim = json!({"kind": "test", "revision": 1});
        let mut evidence = signer.sign(&claim).unwrap();
        assert_eq!(
            evidence.verify::<Value>(&signer.public_key()).unwrap(),
            claim
        );
        assert!(evidence.verify::<Value>(&hex::encode([92; 32])).is_err());

        evidence.payload =
            hex::encode(serde_json::to_vec(&json!({"kind":"test","revision":2})).unwrap());
        assert!(evidence.verify::<Value>(&signer.public_key()).is_err());
    }
}
