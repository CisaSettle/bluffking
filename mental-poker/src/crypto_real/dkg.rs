//! Distributed Key Generation (n-of-n) — **cross-vendor AI-audited
//! (ADR-076/077/078); open-source + verifiable (ADR-063 §2, spec §2)**.
//!
//! Produces a joint ElGamal public key `Q = Σ Q_i` over `n` parties such that
//! decryption requires **all n** secret shares `x_i` (n-of-n; no relaxed
//! quorum, per ADR-041 R2 / threat T4). The coordinator holds **no** share, so
//! it can decrypt nothing (threat T1).
//!
//! ## Mechanism (hand-rolled Feldman/Pedersen-committed DKG)
//!
//! The spec (§2, §4.6) permits a hand-rolled Feldman DKG when the library shape
//! fights the trait. The math here is small enough to review directly:
//!
//! 1. **Commit (DKG-1).** Each party `i` picks `x_i ← OsRng`, computes its
//!    public share `Q_i = x_i·G`, and publishes a **Pedersen commitment**
//!    `Cmt_i = Q_i + blind_i·H` (hides and binds `Q_i`). Commit-before-reveal is
//!    what makes the joint key **unbiasable** (threat T10): a party cannot wait
//!    to see others' `Q_j` and then choose `x_i` to steer `Q`, because every
//!    contribution is locked first.
//! 2. **Reveal (DKG-2).** After all commitments are in, each party reveals
//!    `(Q_i, blind_i)` plus a **Schnorr proof of knowledge** of `x_i`
//!    (rogue-key defense, threat T10 — a party cannot set `Q_i` to cancel
//!    another's contribution without knowing the discrete log).
//! 3. **Verify (DKG-3).** Every party checks each commitment opens to the
//!    revealed `Q_i` (`Cmt_i == Q_i + blind_i·H`) and the Schnorr PoK holds.
//! 4. **Joint key.** `Q = Σ Q_i`. The joint secret `x = Σ x_i` is never
//!    assembled in one place in the real protocol — each party keeps only `x_i`.
//!
//! ## Status
//!
//! Cross-vendor AI-audited (ADR-076/077/078); open-source + verifiable. GA'd for
//! the engine-blind table class by ADR-070 (which lifted the ADR-063 cage); in
//! production these run ONLY for engine-blind sessions (`resolve_mp_crypto_mode`). The
//! increment-1 harness simulates all `n` parties locally with real `OsRng`; the
//! interactive WS choreography is out of scope.

use crate::crypto_real::ec::{
    is_identity_pubkey, point_from_hex, point_to_hex, scalar_from_hex, scalar_to_hex,
};
use curve25519_dalek::constants::RISTRETTO_BASEPOINT_POINT as G;
// PERF: fixed-base precomputed table for G (constant-time; safe on prover paths).
use curve25519_dalek::constants::RISTRETTO_BASEPOINT_TABLE as GT;
use curve25519_dalek::ristretto::RistrettoPoint;
use curve25519_dalek::scalar::Scalar;
use merlin::Transcript;
use rand_core::{CryptoRng, RngCore};
use serde::{Deserialize, Serialize};

use super::ec::pedersen_h;

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
// Pedersen commitment to the public-key share (commit-before-reveal, T10)
// ---------------------------------------------------------------------------

/// `Commit(Q_i, blind) = Q_i + blind·H`. Hiding (blind is secret until reveal)
/// and binding (`log_G(H)` unknown, so a committer cannot open to a different
/// `Q_i`). Returns the commitment point.
fn pedersen_commit(q_i: &RistrettoPoint, blind: &Scalar) -> RistrettoPoint {
    q_i + blind * pedersen_h()
}

// ---------------------------------------------------------------------------
// Per-party DKG state + transcript payloads
// ---------------------------------------------------------------------------

/// A party's secret DKG state: its secret share `x_i`, the blind, and its public
/// share `Q_i`. The secret `x_i` NEVER leaves the party (and is never sent to
/// the coordinator).
///
/// U31 (dual-AI OSS review): the `Debug` impl is manual and **redacts** `x_i`
/// and `blind` (a stray `{:?}` in a log line / panic message must never leak
/// key material), and `Drop` best-effort scrubs both scalars (see the impls
/// below for the honest caveat on the scrub).
#[derive(Clone)]
pub struct DkgParty {
    /// Stable party identifier (e.g. `"party:0"`).
    pub party_id: String,
    /// Secret key share `x_i` (NEVER published).
    pub x_i: Scalar,
    /// Blind used in the commitment (revealed in DKG-2).
    pub blind: Scalar,
    /// Public key share `Q_i = x_i·G`.
    pub q_i: RistrettoPoint,
}

// U31 (dual-AI OSS review): manual Debug — the secret share `x_i` and the
// commitment blind are REDACTED so `{:?}` (logs, panics, `dbg!`, the derived
// Debug of any containing struct such as `DkgRun`) can never print them; only
// the public fields are shown.
impl std::fmt::Debug for DkgParty {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("DkgParty")
            .field("party_id", &self.party_id)
            .field("x_i", &"<redacted>")
            .field("blind", &"<redacted>")
            .field("q_i", &point_to_hex(&self.q_i))
            .finish()
    }
}

// U31 (dual-AI OSS review): best-effort scrub of the secret share + blind when
// the party state is dropped, so the plaintext scalars do not linger in freed
// memory. HONEST CAVEAT: `zeroize` is deliberately NOT a dependency of this
// crate, so this is a plain overwrite — it removes the obvious freed-memory
// copy but is not hardened against compiler dead-store elimination or copies
// made by moves; a guaranteed wipe (the `zeroize` crate's fenced write) is a
// follow-up hardening item, not an increment-1 claim.
impl Drop for DkgParty {
    fn drop(&mut self) {
        self.x_i = Scalar::ZERO;
        self.blind = Scalar::ZERO;
    }
}

impl DkgParty {
    /// DKG-1: pick `x_i ← OsRng`, compute `Q_i`, a blind, and the commitment.
    pub fn generate<R: RngCore + CryptoRng>(party_id: impl Into<String>, rng: &mut R) -> Self {
        let x_i = Scalar::random(rng);
        let blind = Scalar::random(rng);
        let q_i = &x_i * GT;
        DkgParty {
            party_id: party_id.into(),
            x_i,
            blind,
            q_i,
        }
    }

    /// The published commitment (DKG-1 output).
    pub fn commitment(&self) -> DkgCommitment {
        DkgCommitment {
            party_id: self.party_id.clone(),
            commitment: point_to_hex(&pedersen_commit(&self.q_i, &self.blind)),
        }
    }

    /// The reveal (DKG-2 output): `Q_i`, the blind, and a Schnorr PoK of `x_i`.
    pub fn reveal<R: RngCore + CryptoRng>(&self, rng: &mut R) -> DkgShare {
        DkgShare {
            party_id: self.party_id.clone(),
            pubkey_share: point_to_hex(&self.q_i),
            blind: scalar_to_hex(&self.blind),
            pok: schnorr_prove(&self.party_id, &self.x_i, &self.q_i, rng),
        }
    }
}

/// DKG-1 transcript payload: a hiding/binding commitment to `Q_i`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DkgCommitment {
    /// Party identifier.
    pub party_id: String,
    /// `Commit(Q_i, blind)` as 64-hex point.
    pub commitment: String,
}

/// DKG-2 transcript payload: the revealed `Q_i`, blind, and Schnorr PoK.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DkgShare {
    /// Party identifier.
    pub party_id: String,
    /// `Q_i = x_i·G` as 64-hex point.
    pub pubkey_share: String,
    /// The Pedersen blind, revealed so the commitment can be opened.
    pub blind: String,
    /// Schnorr PoK of `x_i` (rogue-key defense).
    pub pok: SchnorrPok,
}

/// Errors from DKG verification (all are clean rejects, never panics).
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum DkgError {
    /// A commitment / share / pok field was not a valid point/scalar.
    #[error("malformed DKG field for {0}")]
    Malformed(String),
    /// A commitment did not open to the revealed `Q_i` (T10).
    #[error("commitment for {0} does not open to revealed Q_i")]
    CommitmentMismatch(String),
    /// A Schnorr PoK failed (rogue key / wrong x_i).
    #[error("Schnorr PoK failed for {0}")]
    BadPok(String),
    /// Commitment and reveal sets did not match up.
    #[error("commitment/share party set mismatch")]
    PartySetMismatch,
}

/// Verify the full DKG: every commitment opens to its revealed `Q_i` and every
/// Schnorr PoK holds; returns the joint key `Q = Σ Q_i`.
///
/// `commitments` and `shares` must cover the same party set (commit-before-
/// reveal: the commitments were fixed first, the reveals open them).
pub fn verify_dkg(
    commitments: &[DkgCommitment],
    shares: &[DkgShare],
) -> Result<RistrettoPoint, DkgError> {
    if commitments.len() != shares.len() {
        return Err(DkgError::PartySetMismatch);
    }
    let mut joint = RistrettoPoint::default(); // identity
                                               // F1 (n-of-n property): the share set must be a BIJECTION onto the commitment
                                               // party set — exactly one share per party, no duplicate, no unknown, no
                                               // missing. Without the duplicate guard, a share set like [P0, P1, P0]
                                               // (len-matched to [P0, P1, P2]) would be accepted: P0's contribution counted
                                               // twice and P2's never required, so a coalition of {P0, P1} could control the
                                               // joint secret and decrypt — defeating n-of-n (audit F1).
    let mut seen: Vec<&str> = Vec::with_capacity(shares.len());
    for share in shares {
        // No party may appear twice (duplicate-party guard).
        if seen.contains(&share.party_id.as_str()) {
            return Err(DkgError::PartySetMismatch);
        }
        seen.push(&share.party_id);

        // Find the matching commitment (commit-before-reveal binding). A missing
        // or unknown party fails here. Combined with the equal-length and
        // distinct-party guards, every commitment party is covered exactly once.
        let cmt = commitments
            .iter()
            .find(|c| c.party_id == share.party_id)
            .ok_or(DkgError::PartySetMismatch)?;

        let q_i = point_from_hex(&share.pubkey_share)
            .ok_or_else(|| DkgError::Malformed(share.party_id.clone()))?;
        // F-CRYPTO-15: reject an identity (x_i = 0) public-key share BEFORE it is
        // summed (see `ec::is_identity_pubkey` for the rationale).
        if is_identity_pubkey(&q_i) {
            return Err(DkgError::BadPok(share.party_id.clone()));
        }
        let blind = scalar_from_hex(&share.blind)
            .ok_or_else(|| DkgError::Malformed(share.party_id.clone()))?;
        let cmt_point = point_from_hex(&cmt.commitment)
            .ok_or_else(|| DkgError::Malformed(share.party_id.clone()))?;

        // The commitment must open to the revealed Q_i (T10: a late-changed
        // share fails its commitment open).
        if cmt_point != pedersen_commit(&q_i, &blind) {
            return Err(DkgError::CommitmentMismatch(share.party_id.clone()));
        }
        // The Schnorr PoK proves knowledge of x_i (rogue-key defense) AND is
        // bound to this party_id (F4): a PoK lifted from another party squeezes a
        // different challenge and fails here.
        if !schnorr_verify(&share.party_id, &q_i, &share.pok) {
            return Err(DkgError::BadPok(share.party_id.clone()));
        }
        joint += q_i;
    }
    Ok(joint)
}

/// A completed DKG run (test/bench harness): all `n` parties + the joint key.
/// In the real protocol each `DkgParty` lives on a different client; here they
/// are simulated locally with real `OsRng`.
#[derive(Debug, Clone)]
pub struct DkgRun {
    /// The `n` parties (each holds its own secret `x_i`).
    pub parties: Vec<DkgParty>,
    /// The joint public key `Q = Σ Q_i`.
    pub joint_key: RistrettoPoint,
    /// The published commitments (DKG-1).
    pub commitments: Vec<DkgCommitment>,
    /// The published shares (DKG-2).
    pub shares: Vec<DkgShare>,
}

impl DkgRun {
    /// Run a full DKG for `n` parties (`"party:0".."party:{n-1}"`).
    pub fn simulate<R: RngCore + CryptoRng>(n: usize, rng: &mut R) -> Self {
        let parties: Vec<DkgParty> = (0..n)
            .map(|i| DkgParty::generate(format!("party:{i}"), rng))
            .collect();
        let commitments: Vec<DkgCommitment> = parties.iter().map(|p| p.commitment()).collect();
        let shares: Vec<DkgShare> = parties.iter().map(|p| p.reveal(rng)).collect();
        let joint_key = parties
            .iter()
            .fold(RistrettoPoint::default(), |acc, p| acc + p.q_i);
        DkgRun {
            parties,
            joint_key,
            commitments,
            shares,
        }
    }
}
