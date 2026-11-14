//! Distributed Key Generation (n-of-n) — **cross-vendor AI-audited
//! (ADR-076/077/078); open-source + verifiable (spec §2)**.
//!
//! Produces a joint ElGamal public key `Q = Σ Q_i` over `n` parties such that
//! decryption requires **all n** secret shares `x_i` (n-of-n; no relaxed
//! quorum, threat T4). The coordinator holds **no** share, so
//! it can decrypt nothing (threat T1).
//!
//! ## Live engine-blind registration (single shot, no commit round)
//!
//! Each real client runs its own [`DkgParty`] and registers its public share
//! `Q_i = x_i·G` with a party-bound Schnorr proof of knowledge of `x_i` in one
//! `mp_register_key` message. The coordinator verifies every proof and relays
//! the full registry only after all `n` seats registered; every client re-checks
//! each peer's proof and computes `Q = Σ Q_i` itself.
//!
//! The PoK is the **rogue-key defence** (threat T10): a party cannot register
//! `Q_i = x·G − Σ_{j≠i} Q_j` to make `Q` a key it alone controls, and cannot
//! replay another party's proof. Decryption is n-of-n, so a card stays hidden
//! while any one party keeps its `x_i` secret.
//!
//! The joint key is **not unbiasable** on this path. Without a commit round, a
//! dishonest coordinator that sees `Q_i` values as they arrive (or a player it
//! leaks them to) can grind a share, or abort and restart the deal, to steer the
//! public `Q`. That bias reveals no honest share and does not allow decryption;
//! it is a disclosed residual of the trusted-operator scope (spec §2).
//!
//! A Pedersen commit-before-reveal round was considered and not adopted: no
//! message carries a commitment and this crate has no code for one. The joint
//! secret `x = Σ x_i` is never assembled in one place — each party keeps only
//! `x_i`.
//!
//! ## Status
//!
//! Cross-vendor AI-audited (ADR-076/077/078); open-source + verifiable. GA'd for
//! the engine-blind table class by ADR-070; in
//! production these run ONLY for engine-blind sessions (`resolve_mp_crypto_mode`).

use crate::crypto_real::ec::{point_from_hex, point_to_hex, scalar_from_hex, scalar_to_hex};
use curve25519_dalek::constants::RISTRETTO_BASEPOINT_POINT as G;
// PERF: fixed-base precomputed table for G (constant-time; safe on prover paths).
use curve25519_dalek::constants::RISTRETTO_BASEPOINT_TABLE as GT;
use curve25519_dalek::ristretto::RistrettoPoint;
use curve25519_dalek::scalar::Scalar;
use merlin::Transcript;
use rand_core::{CryptoRng, RngCore};
use serde::{Deserialize, Serialize};

/// Squeeze a Fiat–Shamir challenge scalar from a merlin transcript (64 wide
/// bytes → `from_bytes_mod_order_wide`). Shared helper for all sigma proofs.
pub(crate) fn challenge_scalar(t: &mut Transcript, label: &'static [u8]) -> Scalar {
    let mut wide = [0u8; 64];
    t.challenge_bytes(label, &mut wide);
    Scalar::from_bytes_mod_order_wide(&wide)
}

// ---------------------------------------------------------------------------
// Schnorr proof of knowledge of a discrete log (rogue-key defense)
// ---------------------------------------------------------------------------

/// A Schnorr PoK that the prover knows `x` such that `Q = x·G`.
/// Wire form `{"r": "<64hex point>", "s": "<64hex scalar>"}` (spec §5.4).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SchnorrPok {
    /// Commitment `R = k·G`.
    pub r: String,
    /// Response `s = k + c·x`.
    pub s: String,
}

/// Bind the Schnorr statement into a merlin transcript and squeeze the
/// challenge. The transcript label `mp:schnorr:v1` separates DKG PoKs from any
/// other Schnorr use (domain separation).
///
/// F4 (PoK attribution): the challenge absorbs the prover's `party_id` so the
/// proof is **bound to that party**. Without it, the Fiat–Shamir challenge is a
/// function of `(G, Q, R)` only, so a PoK produced for party A's share `Q_A`
/// could be relabelled and presented as party B's (the untrusted coordinator
/// re-attributing one party's contribution to an identity it controls). Binding
/// `party_id` makes a relabelled proof squeeze a different challenge → reject.
fn schnorr_challenge(party_id: &str, q: &RistrettoPoint, r_commit: &RistrettoPoint) -> Scalar {
    let mut t = Transcript::new(b"mp:schnorr:v1");
    t.append_message(b"party", party_id.as_bytes());
    t.append_message(b"G", G.compress().as_bytes());
    t.append_message(b"Q", q.compress().as_bytes());
    t.append_message(b"R", r_commit.compress().as_bytes());
    challenge_scalar(&mut t, b"c")
}

/// Prove knowledge of `x` for `Q = x·G`, **bound to `party_id`** (F4).
pub fn schnorr_prove<R: RngCore + CryptoRng>(
    party_id: &str,
    x: &Scalar,
    q: &RistrettoPoint,
    rng: &mut R,
) -> SchnorrPok {
    let k = Scalar::random(rng);
    let r_commit = &k * GT;
    let c = schnorr_challenge(party_id, q, &r_commit);
    let s = k + c * x;
    SchnorrPok {
        r: point_to_hex(&r_commit),
        s: scalar_to_hex(&s),
    }
}

/// Verify a Schnorr PoK for `Q` **as `party_id`**: checks `s·G == R + c·Q` with
/// the challenge bound to `party_id` (F4). Returns `false` on any malformed
/// field (T9 — clean reject, no panic) and on a party-id mismatch (the PoK was
/// produced under a different party label, so the challenge differs).
pub fn schnorr_verify(party_id: &str, q: &RistrettoPoint, pok: &SchnorrPok) -> bool {
    let r_commit = match point_from_hex(&pok.r) {
        Some(p) => p,
        None => return false,
    };
    let s = match scalar_from_hex(&pok.s) {
        Some(s) => s,
        None => return false,
    };
    let c = schnorr_challenge(party_id, q, &r_commit);
    &s * GT == r_commit + c * q
}

// ---------------------------------------------------------------------------
// ADR-078 §5.1 — message-binding Schnorr signature under x_i (the
// `vk_dlog_binding` primitive, closes TH-K). ADDITIVE: the existing key_pok
// `schnorr_prove`/`schnorr_verify`/`schnorr_challenge` above are UNCHANGED and
// byte-identical (the DKG `key_pok` path must not move).
// ---------------------------------------------------------------------------

/// Fiat–Shamir challenge for a message-binding Schnorr signature under `x_i` for
/// `Q = x_i·G`, domain-separated from the DKG `key_pok` (label
/// `mp:schnorr-bind:v1`, distinct from `key_pok`'s `mp:schnorr:v1`) and
/// additionally absorbing the bound `message` (the canonical `bindClaim_v2`
/// bytes, INCLUDING `ed25519_vk`).
///
/// A signature under this challenge proves the signer knows `x_i` AND commits to
/// `message` — exactly what `vk_dlog_binding` needs (ADR-078 §3.2). The distinct
/// transcript label means a `key_pok` can never be replayed as a
/// `vk_dlog_binding` and vice-versa (the two squeeze different challenges).
fn schnorr_bind_challenge(
    party_id: &str,
    q: &RistrettoPoint,
    r_commit: &RistrettoPoint,
    message: &[u8],
) -> Scalar {
    let mut t = Transcript::new(b"mp:schnorr-bind:v1");
    t.append_message(b"party", party_id.as_bytes());
    t.append_message(b"G", G.compress().as_bytes());
    t.append_message(b"Q", q.compress().as_bytes());
    t.append_message(b"R", r_commit.compress().as_bytes());
    t.append_message(b"msg", message); // <-- binds ed25519_vk via bindClaim_v2
    challenge_scalar(&mut t, b"c")
}

/// ADR-078 §5.1 — sign `message` under `x` (where `Q = x·G`), bound to
/// `party_id`. The wire form reuses the existing [`SchnorrPok`] shape
/// `{"r":"<64hex point>","s":"<64hex scalar>"}`.
///
/// CRITICAL: a FRESH per-signature nonce `k = Scalar::random(rng)` (mirrors the
/// sound `schnorr_prove`). A reused/static nonce across two distinct messages
/// would leak `x` (two-equation linear solve), so the caller MUST pass a real
/// CSPRNG (`OsRng`).
pub fn schnorr_sign_bound<R: RngCore + CryptoRng>(
    party_id: &str,
    x: &Scalar,
    q: &RistrettoPoint,
    message: &[u8],
    rng: &mut R,
) -> SchnorrPok {
    let k = Scalar::random(rng);
    let r_commit = &k * GT;
    let c = schnorr_bind_challenge(party_id, q, &r_commit, message);
    let s = k + c * x;
    SchnorrPok {
        r: point_to_hex(&r_commit),
        s: scalar_to_hex(&s),
    }
}

/// ADR-078 §5.1 — verify a message-binding Schnorr signature for the PUBLIC `q`
/// as `party_id` over `message`: `s·G == R + c·Q` with the challenge bound to
/// `party_id` + `message`. Holds only if the signer knew `x = log_G(q)` AND
/// signed over exactly `message`.
///
/// Returns `false` on any malformed field (T9 — clean reject, no panic), under a
/// different `q` (wrong `x_i`), or over a different `message` (the challenge
/// differs). A `key_pok` (label `mp:schnorr:v1`) never verifies here, and a
/// `vk_dlog_binding` never verifies as a `key_pok` — the labels are disjoint.
pub fn schnorr_verify_bound(
    party_id: &str,
    q: &RistrettoPoint,
    message: &[u8],
    sig: &SchnorrPok,
) -> bool {
    let r_commit = match point_from_hex(&sig.r) {
        Some(p) => p,
        None => return false,
    };
    let s = match scalar_from_hex(&sig.s) {
        Some(s) => s,
        None => return false,
    };
    let c = schnorr_bind_challenge(party_id, q, &r_commit, message);
    &s * GT == r_commit + c * q
}

// ---------------------------------------------------------------------------
// Per-party DKG state
// ---------------------------------------------------------------------------

/// A party's secret DKG state: its secret share `x_i` and its public share
/// `Q_i`. The secret `x_i` NEVER leaves the party (and is never sent to the
/// coordinator).
///
/// U31 (dual-AI OSS review): the `Debug` impl is manual and **redacts** `x_i`
/// (a stray `{:?}` in a log line / panic message must never leak key
/// material), and `Drop` best-effort scrubs the scalar (see the impls below
/// for the honest caveat on the scrub).
#[derive(Clone)]
pub struct DkgParty {
    /// Stable party identifier (e.g. `"party:0"`).
    pub party_id: String,
    /// Secret key share `x_i` (NEVER published).
    pub x_i: Scalar,
    /// Public key share `Q_i = x_i·G`.
    pub q_i: RistrettoPoint,
}

// U31 (dual-AI OSS review): manual Debug — the secret share `x_i` is REDACTED
// so `{:?}` (logs, panics, `dbg!`, the derived Debug of any containing struct)
// can never print it; only the public fields are shown.
impl std::fmt::Debug for DkgParty {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("DkgParty")
            .field("party_id", &self.party_id)
            .field("x_i", &"<redacted>")
            .field("q_i", &point_to_hex(&self.q_i))
            .finish()
    }
}

// U31 (dual-AI OSS review): best-effort scrub of the secret share when the
// party state is dropped, so the plaintext scalar does not linger in freed
// memory. HONEST CAVEAT: `zeroize` is deliberately NOT a dependency of this
// crate, so this is a plain overwrite — it removes the obvious freed-memory
// copy but is not hardened against compiler dead-store elimination or copies
// made by moves; a guaranteed wipe (the `zeroize` crate's fenced write) is a
// follow-up hardening item, not an increment-1 claim.
impl Drop for DkgParty {
    fn drop(&mut self) {
        self.x_i = Scalar::ZERO;
    }
}

impl DkgParty {
    /// Pick the secret share `x_i` from `rng` (production: `OsRng`) and compute
    /// the public share `Q_i = x_i·G`.
    pub fn generate<R: RngCore + CryptoRng>(party_id: impl Into<String>, rng: &mut R) -> Self {
        let x_i = Scalar::random(rng);
        let q_i = &x_i * GT;
        DkgParty {
            party_id: party_id.into(),
            x_i,
            q_i,
        }
    }
}
