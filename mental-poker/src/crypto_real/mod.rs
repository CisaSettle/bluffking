//! # Real cryptography — **cross-vendor AI-audited (ADR-076/077/078); open-source + verifiable**
//!
//! The real cryptography specified in `docs/architecture/mp-phase4-crypto-spec.md`:
//!
//! - [`SignatureProvider`](crate::signing::SignatureProvider) →
//!   [`ed25519_signer::Ed25519SignatureProvider`] (component 1);
//! - threshold ElGamal + Chaum–Pedersen decryption in [`decrypt`] (component 2);
//! - [`ShuffleProofProvider`](crate::crypto::ShuffleProofProvider) →
//!   [`shuffle::RealShuffleProofProvider`], a sound sigma-based verifiable
//!   re-encryption shuffle (component 3);
//! - the n-of-n DKG in [`dkg`] and the ristretto255 ElGamal layer in [`ec`].
//!
//! ## Status: GA'd for engine-blind (ADR-070); cross-vendor AI-audited
//!
//! ADR-070 (2026-06-23) released these **for the engine-blind table class**:
//! they run in production for engine-blind (n-of-n server-blind, opt-in
//! all-human) sessions. Still true:
//!
//! - the GA gate was a clean cross-vendor AI audit (ADR-076/077/078); the code
//!   is **open-source and every proof is independently checkable**,
//!   play-money only;
//! - real crypto is selected by `server::mp_dealing::resolve_mp_crypto_mode`
//!   and the engine-blind session coordinator.

pub mod decrypt;
pub mod dkg;
pub mod ec;
pub mod ed25519_signer;
pub mod recovery;
pub mod shuffle;

pub use ed25519_signer::Ed25519SignatureProvider;
pub use shuffle::RealShuffleProofProvider;
