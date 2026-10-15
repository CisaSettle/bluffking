//! Phase-4 server-blind core — integration tests + KAT vector emission.
//!
//! **Cross-vendor AI-audited (ADR-076/077/078); open-source + independently
//! verifiable (ADR-063 §4).** These tests
//! exercise the real threshold-ElGamal / DKG / Chaum–Pedersen path
//! (`mental_poker::crypto_real`) end to end, with **all parties simulated
//! locally using the real OS CSPRNG**. In production the real path runs only for
//! the engine-blind table class (ADR-070); the generic `mental_poker_production`
//! provider stays rejected.
//!
//! No database required: `cargo test -p mental-poker`.

use mental_poker::crypto_real::decrypt::{
    combine, dleq_prove, partial_decrypt, verify_and_open, DecryptionShare, OpenError,
    ThresholdDecryptionProof, SCHEME,
};
use mental_poker::crypto_real::dkg::{verify_dkg, DkgRun};
use mental_poker::crypto_real::ec::{card_point, point_to_hex, scalar_to_hex, Ct, DECK_SIZE};
use rand::rngs::OsRng;

use curve25519_dalek::constants::RISTRETTO_BASEPOINT_POINT as G;
use curve25519_dalek::ristretto::RistrettoPoint;
use curve25519_dalek::scalar::Scalar;
fn pubkeys(run: &DkgRun) -> Vec<(String, RistrettoPoint)> {
    run.parties
        .iter()
        .map(|p| (p.party_id.clone(), p.q_i))
        .collect()
}

fn open_proof(run: &DkgRun, deck_index: u32, ct: &Ct, rng: &mut OsRng) -> ThresholdDecryptionProof {
    ThresholdDecryptionProof {
        scheme: SCHEME.to_string(),
        shares: run
            .parties
            .iter()
            .map(|p| partial_decrypt(p, deck_index, ct, rng))
            .collect(),
    }
}

/// Re-chain (recompute previous_event_hash) and re-sign every event of a
/// transcript with the coordinator key — so a post-hoc payload edit yields a
/// structurally valid transcript whose ONLY defect is the edited content (here:
/// a tampered decryption share), isolating the cryptographic-proof rejection.
fn rechain_and_sign(
    t: &mut mental_poker::Transcript,
    coord_sig: &mental_poker::signing::MockSignatureProvider,
) {
    use mental_poker::signing::SignatureProvider;
    let mut prev = mental_poker::hash::ZERO_HASH;
    for ev in &mut t.events {
        ev.previous_event_hash = mental_poker::hash::hex_hash(&prev);
        ev.signature = String::new();
        let eh = ev.event_hash();
        ev.signature = coord_sig.sign(mental_poker::events::COORDINATOR, &eh);
        prev = eh;
    }
}

#[path = "phase4_server_blind/mixed_mode.rs"]
mod mixed_mode;
#[path = "phase4_server_blind/reenc_binding.rs"]
mod reenc_binding;
#[path = "phase4_server_blind/threshold_roundtrip.rs"]
mod threshold_roundtrip;
#[path = "phase4_server_blind/transcript_proofs.rs"]
mod transcript_proofs;
#[path = "phase4_server_blind/vectors.rs"]
mod vectors;
