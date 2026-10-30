//! # Mental Poker dealing protocol
//!
//! This crate refactors card dealing so that **neither the backend nor the
//! clients are trusted**. It provides:
//!
//! - a [`DealingProvider`] abstraction so game logic no longer depends on a
//!   server-side RNG/shuffle directly;
//! - [`MentalPokerDealingProvider`] — the untrusted-dealer protocol
//!   (encrypted deck, re-encryption shuffle, final deck commitment, owner-only
//!   reveal, staged community reveal) producing a signed [`Transcript`];
//! - an offline [`verify`]r that replays a transcript and checks every rule.
//!
//! ## Safety
//!
//! Two crypto suites live here. The default `Mock*` implementations are
//! **dev-grade — not cryptographically sound** (they only replay consistently,
//! `SchemeSoundness::DevMock`). The real suite (`crypto_real`) is sound up to its
//! stated bounds (the interim re-encryption-shuffle argument's soundness error is
//! ~2⁻²⁶ at N=52, not negligible — see `crypto_real::shuffle`) and, since
//! **ADR-070 (2026-06-23)**, runs in production for the engine-blind table class —
//! cross-vendor AI-audited (ADR-076/077/078), open-source + verifiable. The runtime
//! engine-blind coordinator selects real crypto via `resolve_mp_crypto_mode`;
//! mock hands cannot reach production settlement.
//!
//! Pure crate: no async, no IO, no DB.

// Rust review standard (L2): pure crate — forbid `unsafe` outright so any future
// `unsafe` is a hard compile error, not a review judgement call.
#![forbid(unsafe_code)]

pub mod card_id;
pub mod crypto;
/// Real cryptography (ADR-063 primitives) — **GA'd for the engine-blind table
/// class by ADR-070 (2026-06-23); cross-vendor AI-audited (ADR-076/077/078),
/// open-source + independently verifiable** (the GA gate was a clean
/// cross-vendor AI audit).
///
/// Houses the real `ed25519-dalek`-backed [`SignatureProvider`] plus the
/// verifiable-shuffle and threshold-decryption providers that replace the
/// dev-only `Mock*` crypto behind the existing trait seams. In production these
/// are selected ONLY for the engine-blind table class (n-of-n server-blind
/// dealing, opt-in all-human rooms), via `resolve_mp_crypto_mode`;
/// real dealing remains confined to that session composition.
pub mod crypto_real;
pub mod events;
pub mod hash;
pub mod mental;
pub mod pf;
pub mod pf2;
pub mod provider;
pub mod signing;
pub mod state;
pub mod transcript;
pub mod verifier;

pub use mental::{MentalPokerDealingProvider, Scenario};
pub use provider::{DealRequest, DealingProvider, DealtHand};
pub use transcript::{Transcript, PROTOCOL_VERSION};
pub use verifier::{
    classify_soundness, verify, verify_fairness, SchemeSoundness, VerifyError, VerifyFairnessError,
    VerifyReport,
};
