//! A pinned drand quicknet chain, verified without trusting its HTTP relay.
use drand_verify::{G2PubkeyRfc, Pubkey};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

pub const CHAIN_HASH: &str = "52db9ba70e0cc0f6eaf7803dd07447a1f5477735fd3f661792ba94600c84e971";
pub const PUBLIC_KEY: &str = "83cf0f2896adee7eb8b5f01fcad3912212c437e0073e911fb90022d3e760183c8c4b450b6a0a6c3ac6a5776a2d1064510d1fec758c921cc22b0e17e63aaf4bcb5ed66304de9cf809bd274ca73bab4af5a6e9c76a4bc09e76eae8991ef5ece45a";
pub const GENESIS_SECONDS: i64 = 1_692_803_367;
pub const PERIOD_MS: i64 = 3_000;

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct Beacon {
    pub round: u64,
    pub signature: String,
}

/// The first beacon scheduled at or after the draw. Sales close five seconds
/// earlier; this selection never depends on a relay response or on its value.
pub fn round_at_or_after(draw_ms: i64) -> Result<u64, String> {
    let delta = draw_ms
        .checked_sub(GENESIS_SECONDS * 1_000)
        .filter(|n| *n >= 0)
        .ok_or("draw predates the pinned beacon chain")?;
    let round = delta
        .checked_add(PERIOD_MS - 1)
        .map(|n| n / PERIOD_MS + 1)
        .ok_or("draw time overflow")?;
    Ok(round as u64)
}

/// Validate the BLS signature for this exact round and return SHA-256(signature).
/// A relay's supplied `randomness` or chain metadata is never trusted.
pub fn verify(beacon: &Beacon) -> Result<[u8; 32], String> {
    if beacon.round == 0 || beacon.signature.len() != 96 {
        return Err("invalid beacon round or signature length".into());
    }
    let mut public_key = [0; 96];
    hex::decode_to_slice(PUBLIC_KEY, &mut public_key).map_err(|_| "invalid pinned key")?;
    let signature = hex::decode(&beacon.signature).map_err(|_| "invalid beacon signature hex")?;
    let public_key =
        G2PubkeyRfc::from_fixed(public_key).map_err(|_| "invalid pinned beacon public key")?;
    if !public_key
        .verify(beacon.round, b"", &signature)
        .map_err(|_| "invalid beacon signature")?
    {
        return Err("beacon signature verification failed".into());
    }
    Ok(Sha256::digest(signature).into())
}
