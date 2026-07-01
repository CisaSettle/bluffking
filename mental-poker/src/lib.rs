//! # Mental Poker dealing protocol
//!
//! This crate refactors card dealing so that **neither the backend nor the
//! clients are trusted**. It provides:
//!
//! - a [`DealingProvider`] abstraction so game logic no longer depends on a
//!   server-side RNG/shuffle directly;
//! - [`ExistingServerDealingProvider`] — the legacy trusted-server shuffle,
//!   retained for rollback;
//! - `mental_poker_prefer` — the production policy that first tries the current
//!   interactive transcript mode on eligible all-human hands and falls back to
//!   [`ExistingServerDealingProvider`] when the table is ineligible or the
//!   choreography aborts;
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
//! guard [`guard_provider_allowed`] keeps the explicit mock provider out of
//! production and rejects the generic `mental_poker_production` provider
//! (engine-blind selects real crypto via `resolve_mp_crypto_mode`).
//! `mental_poker_prefer` may run in production as a best-effort transcript mode,
//! but it must not be described as server-blind cryptographic Mental Poker. See
//! `docs/mental-poker-dealing-refactor.md`.
//!
//! Pure crate: no async, no IO, no DB — `cargo test -p mental-poker` needs no
//! database.

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
/// [`guard_provider_allowed`] still keeps the generic `mental_poker_production`
/// provider rejected at startup. Play-money only.
pub mod crypto_real;
pub mod events;
pub mod existing;
pub mod hash;
pub mod mental;
pub mod pf;
pub mod provider;
pub mod signing;
pub mod state;
pub mod transcript;
pub mod verifier;

pub use existing::ExistingServerDealingProvider;
pub use mental::{MentalPokerDealingProvider, Scenario};
pub use provider::{DealRequest, DealingProvider, DealingProviderKind, DealtHand};
pub use transcript::{Transcript, PROTOCOL_VERSION};
pub use verifier::{
    classify_soundness, verify, verify_fairness, SchemeSoundness, VerifyError, VerifyFairnessError,
    VerifyReport,
};

/// Reject a dealing-provider selection that is unsafe for the current
/// environment. The server calls this at startup and **panics** on `Err`.
///
/// - `existing_server` — always allowed.
/// - `mental_poker_prefer` — allowed in all environments. It is a production
///   policy that attempts the current interactive transcript mode first and
///   falls back per hand to `existing_server`; it is not server-blind crypto.
/// - `mental_poker_mock` — allowed only in an explicitly-listed non-production
///   env (dev/CI/staging/test/local; its crypto is dev-only mock). FAIL-CLOSED:
///   an unset/empty/unrecognised `APP_ENV` is treated as production and rejected
///   (backend review F-CFG-1). **Unchanged by ADR-070.**
/// - `mental_poker_production` — the **generic, UNAUDITED** real-crypto path:
///   rejected everywhere. **ADR-070 does NOT un-cage this** — only the specific
///   cross-vendor-AI-audited engine-blind composition is prod-permitted (below).
/// - `mental_poker_engine_blind` — the **cross-vendor-AI-audited engine-blind n-of-n
///   composition** (ADR-066/067/068; `crypto_real/`). ADR-070 P5 permits it in
///   production. **IMPORTANT — this is a record, not the live gate:** the live
///   engine-blind path is NOT selected via `DealingProviderKind`/this guard. It
///   is routed by the per-session `engine_blind` flag (`session.rs`
///   `engine_blind_routes_blind_coordinator`) and gated by
///   `server::mp_dealing::resolve_mp_crypto_mode` (which only returns `Real` in
///   prod for an `engine_blind` session) plus the Mock-void safety net
///   (`mp_engine_blind_live.rs` — a `Mock` engine-blind hand still VOIDS). This
///   guard validates the **startup `DEALING_PROVIDER`** value, and this variant
///   is intentionally NOT parseable from that env var (`DealingProviderKind::parse`),
///   so it can never be selected at startup. Permitting it here keeps the
///   cross-vendor-AI-audited-vs-generic distinction reviewable and future-proofs an explicit
///   programmatic selection — without un-caging the generic
///   `mental_poker_production` path.
pub fn guard_provider_allowed(kind: DealingProviderKind, app_env: &str) -> Result<(), String> {
    // backend review F-CFG-1 — FAIL-CLOSED ALLOWLIST. Previously this was a
    // `matches!(... "production" | "prod")` DENYLIST, so a dropped/typo'd/empty
    // APP_ENV silently counted as non-production and would let the dev mock
    // provider be accepted in prod. Now only explicitly-listed
    // dev/CI/staging/test/local values are treated as non-production; anything
    // unrecognised (unset/empty/typo) is treated as production and rejects the
    // mock provider. Mirrors `server::config::Config::is_production_strength`.
    let is_production = !matches!(
        app_env.trim().to_lowercase().as_str(),
        "development" | "dev" | "ci" | "staging" | "test" | "local"
    );
    match kind {
        DealingProviderKind::ExistingServer => Ok(()),
        DealingProviderKind::PreferMentalPoker => Ok(()),
        DealingProviderKind::MentalPokerMock => {
            if is_production {
                Err(
                    "DEALING_PROVIDER=mental_poker_mock relies on dev-only mock \
                     crypto and must not run when APP_ENV=production"
                        .to_string(),
                )
            } else {
                Ok(())
            }
        }
        // The GENERIC unaudited real-crypto path stays rejected EVERYWHERE.
        // ADR-070 §6 cage point 2 keeps this caged (only the engine-blind
        // composition below is un-caged).
        DealingProviderKind::MentalPokerProduction => Err(
            "DEALING_PROVIDER=mental_poker_production is the generic UNAUDITED \
             real-crypto path and stays rejected everywhere; ADR-070 un-cages \
             ONLY the cross-vendor-AI-audited engine-blind composition (see \
             docs/architecture/adr/ADR-070-engine-blind-production-ga.md §6)"
                .to_string(),
        ),
        // ADR-070 P5 — the cross-vendor-AI-audited engine-blind n-of-n composition is
        // prod-permitted. (Not reachable from the startup DEALING_PROVIDER env;
        // see the doc-comment — the live gate is resolve_mp_crypto_mode + the
        // per-session engine_blind routing flag + the Mock-void safety net.)
        DealingProviderKind::MentalPokerEngineBlind => Ok(()),
    }
}

/// Construct the configured dealing provider after [`guard_provider_allowed`]
/// has approved it. `entropy` is OS randomness the server supplies for the
/// Mental Poker master seed. Returns `None` for `mental_poker_production`
/// (no implementation yet). `mental_poker_prefer` uses the same current
/// transcript implementation as `mental_poker_mock`; callers decide eligibility
/// and fallback before invoking it.
pub fn select_provider(
    kind: DealingProviderKind,
    entropy: Vec<u8>,
) -> Option<Box<dyn DealingProvider>> {
    match kind {
        DealingProviderKind::ExistingServer => Some(Box::new(ExistingServerDealingProvider::new())),
        DealingProviderKind::PreferMentalPoker | DealingProviderKind::MentalPokerMock => {
            Some(Box::new(MentalPokerDealingProvider::new(entropy)))
        }
        // `MentalPokerProduction` has no `DealingProvider` impl. The cross-vendor-AI-audited
        // `MentalPokerEngineBlind` composition is NOT a `DealingProvider` either —
        // it runs as the engine-blind coordinator (`run_engine_blind_hand`), not
        // through this trait — so it also returns `None` here.
        DealingProviderKind::MentalPokerProduction
        | DealingProviderKind::MentalPokerEngineBlind => None,
    }
}

#[cfg(test)]
mod guard_tests {
    use super::*;

    #[test]
    fn existing_server_always_allowed() {
        assert!(guard_provider_allowed(DealingProviderKind::ExistingServer, "production").is_ok());
        assert!(guard_provider_allowed(DealingProviderKind::ExistingServer, "dev").is_ok());
    }

    #[test]
    fn mock_blocked_in_production() {
        assert!(
            guard_provider_allowed(DealingProviderKind::MentalPokerMock, "production").is_err()
        );
        assert!(guard_provider_allowed(DealingProviderKind::MentalPokerMock, "prod").is_err());
        assert!(
            guard_provider_allowed(DealingProviderKind::MentalPokerMock, "PRODUCTION").is_err()
        );
    }

    #[test]
    fn prefer_allowed_in_production() {
        assert!(
            guard_provider_allowed(DealingProviderKind::PreferMentalPoker, "production").is_ok()
        );
        assert!(guard_provider_allowed(DealingProviderKind::PreferMentalPoker, "prod").is_ok());
        assert!(guard_provider_allowed(DealingProviderKind::PreferMentalPoker, "dev").is_ok());
    }

    #[test]
    fn mock_allowed_outside_production() {
        // Only explicitly-listed dev/CI/staging/test/local envs accept the mock.
        assert!(guard_provider_allowed(DealingProviderKind::MentalPokerMock, "dev").is_ok());
        assert!(guard_provider_allowed(DealingProviderKind::MentalPokerMock, "staging").is_ok());
        assert!(guard_provider_allowed(DealingProviderKind::MentalPokerMock, "local").is_ok());
    }

    #[test]
    fn mock_rejected_for_unknown_or_empty_env() {
        // backend review F-CFG-1 — FAIL-CLOSED: an unset/empty/typo'd APP_ENV is
        // treated as production and must reject the dev-only mock provider, NOT
        // accept it (the old denylist accepted "" as non-production).
        for env in &["", "  ", "unknown", "produciton", "live", "render"] {
            assert!(
                guard_provider_allowed(DealingProviderKind::MentalPokerMock, env).is_err(),
                "mock must be rejected for unrecognised env {env:?} (fail-closed)"
            );
        }
    }

    #[test]
    fn production_crypto_rejected_everywhere() {
        // ADR-070 §6 invariant (b): the GENERIC unaudited real-crypto path stays
        // rejected EVERYWHERE — including production — after the un-cage.
        assert!(guard_provider_allowed(DealingProviderKind::MentalPokerProduction, "dev").is_err());
        assert!(
            guard_provider_allowed(DealingProviderKind::MentalPokerProduction, "production")
                .is_err()
        );
        assert!(
            guard_provider_allowed(DealingProviderKind::MentalPokerProduction, "prod").is_err()
        );
        // And it must NOT be reachable from the startup DEALING_PROVIDER env at all
        // for the engine-blind variant (that is routed per-session, not via env).
        assert_eq!(
            DealingProviderKind::parse("mental_poker_engine_blind"),
            None
        );
    }

    #[test]
    fn engine_blind_composition_permitted_in_production() {
        // ADR-070 P5 un-cage: the cross-vendor-AI-audited engine-blind n-of-n composition is
        // prod-permitted (and allowed in every env). It is a record/distinction
        // here — the live gate is resolve_mp_crypto_mode + the per-session
        // engine_blind routing flag — but the guard must not reject it.
        assert!(
            guard_provider_allowed(DealingProviderKind::MentalPokerEngineBlind, "production")
                .is_ok(),
            "the cross-vendor-AI-audited engine-blind composition must be prod-permitted (ADR-070 P5)"
        );
        assert!(
            guard_provider_allowed(DealingProviderKind::MentalPokerEngineBlind, "prod").is_ok()
        );
        assert!(guard_provider_allowed(DealingProviderKind::MentalPokerEngineBlind, "dev").is_ok());
        // Distinct from the generic unaudited path, which stays rejected (b).
        assert!(
            guard_provider_allowed(DealingProviderKind::MentalPokerProduction, "production")
                .is_err()
        );
    }

    #[test]
    fn production_kind_has_no_mock_fallback_provider() {
        assert!(
            select_provider(DealingProviderKind::MentalPokerProduction, vec![0u8; 32]).is_none(),
            "mental_poker_production must not silently fall back to mock crypto"
        );
        // The engine-blind composition is NOT a DealingProvider (it runs as the
        // coordinator), so select_provider returns None — it must NEVER silently
        // fall back to the mock transcript provider.
        assert!(
            select_provider(DealingProviderKind::MentalPokerEngineBlind, vec![0u8; 32]).is_none(),
            "engine-blind must not fall back to a mock DealingProvider"
        );
    }

    #[test]
    fn prefer_selects_current_transcript_provider() {
        let provider = select_provider(DealingProviderKind::PreferMentalPoker, vec![0u8; 32])
            .expect("prefer mode uses current transcript provider");
        assert_eq!(provider.name(), "mental_poker_mock");
        assert!(provider.is_verifiable());
    }
}
