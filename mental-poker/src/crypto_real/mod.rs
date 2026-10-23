//! # Real cryptography — **cross-vendor AI-audited (ADR-076/077/078); open-source + verifiable (ADR-063)**
//!
//! This module replaces the dev-only `Mock*` crypto with **real** cryptography
//! behind the three existing trait seams of ADR-041 / ADR-063 §3:
//!
//! - [`SignatureProvider`](crate::signing::SignatureProvider) →
//!   [`ed25519_signer::Ed25519SignatureProvider`] (component 1);
//! - [`DecryptionProvider`](crate::crypto::DecryptionProvider) → threshold
//!   ElGamal + Chaum–Pedersen in [`decrypt`] (component 2);
//! - [`ShuffleProofProvider`](crate::crypto::ShuffleProofProvider) →
//!   [`shuffle::RealShuffleProofProvider`], a sound sigma-based verifiable
//!   re-encryption shuffle (component 3).
//!
//! ## Status: GA'd for engine-blind (ADR-070); cross-vendor AI-audited
//!
//! ADR-070 (2026-06-23) lifted the ADR-063 cage **for the engine-blind table
//! class**: these real providers now run in production for engine-blind (n-of-n
//! server-blind, opt-in all-human) sessions. Still true:
//!
//! - the GA gate was a clean cross-vendor AI audit (ADR-076/077/078); the code
//!   is **open-source and the transcript is independently verifiable offline**,
//!   play-money only;
//! - real crypto is selected by `server::mp_dealing::resolve_mp_crypto_mode`
//!   and the engine-blind session coordinator.

pub mod decrypt;
pub mod dkg;
pub mod ec;
pub mod ed25519_signer;
pub mod shuffle;

pub use ed25519_signer::Ed25519SignatureProvider;
pub use shuffle::RealShuffleProofProvider;
