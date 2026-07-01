//! # Real cryptography — **PROTOTYPE, pending external audit (ADR-063)**
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
//! ## Status: NOT audited, NOT shipped, NOT wired into production
//!
//! This is a **Milestone-B decision-gate prototype** and a future
//! **external-audit artifact** — it is explicitly **not** "audited / shipped /
//! server-blind in production". The hard cage of ADR-063 §"CAGE" stands:
//!
//! - [`guard_provider_allowed`](crate::guard_provider_allowed) keeps
//!   `mental_poker_production` **rejected at startup everywhere**;
//! - [`select_provider`](crate::select_provider) returns `None` for it;
//! - there is **no** new `DealingProviderKind` variant;
//! - the real providers in this module are reachable **only** from
//!   `#[cfg(test)]`, benches, and dev examples — **never** from `session.rs` or
//!   any other live production-path call site.
//!
//! Un-gating production is the **external audit's** job (ADR-062 Milestone E),
//! not this workflow's.

pub mod decrypt;
pub mod dkg;
pub mod ec;
pub mod ed25519_signer;
pub mod shuffle;

pub use ed25519_signer::Ed25519SignatureProvider;
pub use shuffle::RealShuffleProofProvider;
