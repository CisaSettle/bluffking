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
//! - [`guard_provider_allowed`](crate::guard_provider_allowed) keeps the generic
//!   `mental_poker_production` provider **rejected at startup**;
//! - [`select_provider`](crate::select_provider) returns `None` for it, and there
//!   is **no** new `DealingProviderKind` variant — engine-blind selects the real
//!   crypto via `resolve_mp_crypto_mode` + the engine-blind session path, NOT via
//!   the generic provider.
//!
//! Un-caging the GENERIC provider is separate future work (ADR-062
//! Milestone E); ADR-070 already un-caged the specific engine-blind composition.

pub mod decrypt;
pub mod dkg;
pub mod ec;
pub mod ed25519_signer;
pub mod shuffle;

pub use ed25519_signer::Ed25519SignatureProvider;
pub use shuffle::RealShuffleProofProvider;
