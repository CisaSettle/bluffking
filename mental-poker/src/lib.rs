//! # Verifiable card dealing
//!
//! Pure, verifiable dealing primitives shared by the server, the browser
//! (`mp-wasm`) and the offline verifiers:
//!
//! - [`pf`] — ADR-064 commit-reveal dealing for standard tables, checked
//!   offline by the `pf_verify` binary;
//! - [`pf2`] / [`pf2_zjh`] / [`card_proof`] — verifiable-dealing bundles (locked
//!   encrypted deck, shuffle proof, decryption proofs for opened cards);
//! - [`crypto_real`] — the real cryptography (DKG, verifiable re-encryption
//!   shuffle, threshold decryption, Ed25519) behind the [`crypto`] and
//!   [`signing`] seams, used by engine-blind (server-blind) dealing.
//!
//! ## Safety
//!
//! The real suite (`crypto_real`) is sound up to its stated bounds (the interim
//! re-encryption-shuffle argument's soundness error is ~2⁻²⁶ at N=52, not
//! negligible — see `crypto_real::shuffle`) and, since **ADR-070 (2026-06-23)**,
//! runs in production for the engine-blind table class — cross-vendor
//! AI-audited (ADR-076/077/078), open-source + verifiable. The runtime
//! engine-blind coordinator selects real crypto via `resolve_mp_crypto_mode`
//! and deals nothing when it is unavailable.
//!
//! Pure crate: no async, no IO, no DB.

// Rust review standard (L2): pure crate — forbid `unsafe` outright so any future
// `unsafe` is a hard compile error, not a review judgement call.
#![forbid(unsafe_code)]

pub mod card_id;
pub mod card_proof;
pub mod crypto;
/// Real cryptography (`docs/architecture/mp-phase4-crypto-spec.md`) — **GA'd
/// for the engine-blind table class by ADR-070 (2026-06-23); cross-vendor AI-audited (ADR-076/077/078),
/// open-source + independently verifiable** (the GA gate was a clean
/// cross-vendor AI audit).
///
/// Houses the real `ed25519-dalek`-backed [`signing::SignatureProvider`] plus
/// the verifiable-shuffle and threshold-decryption primitives. In production
/// these are selected ONLY for the engine-blind table class (n-of-n server-blind
/// dealing, opt-in all-human rooms), via `resolve_mp_crypto_mode`;
/// real dealing remains confined to that session composition.
pub mod crypto_real;
pub mod events;
pub mod hash;
pub mod pf;
pub mod pf2;
pub mod pf2_zjh;
pub mod signing;
