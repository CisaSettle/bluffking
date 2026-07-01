//! Distributed Key Generation (n-of-n) — **PROTOTYPE, pending external audit
//! (ADR-063 §2, spec §2)**.
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
//! NOT audited, NOT shipped, NOT wired into production (ADR-063 cage). The
//! increment-1 harness simulates all `n` parties locally with real `OsRng`; the
//! interactive WS choreography is out of scope.

use crate::crypto_real::ec::{
    is_identity_pubkey, point_from_hex, point_to_hex, scalar_from_hex, scalar_to_hex,
};
use curve25519_dalek::constants::RISTRETTO_BASEPOINT_POINT as G;
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
    let r_commit = k * G;
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
    s * G == r_commit + c * q
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
    let r_commit = k * G;
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
    s * G == r_commit + c * q
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
#[derive(Debug, Clone)]
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

impl DkgParty {
    /// DKG-1: pick `x_i ← OsRng`, compute `Q_i`, a blind, and the commitment.
    pub fn generate<R: RngCore + CryptoRng>(party_id: impl Into<String>, rng: &mut R) -> Self {
        let x_i = Scalar::random(rng);
        let blind = Scalar::random(rng);
        let q_i = x_i * G;
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

/// The coordinator-authored joint-key event (verifier recomputes the sum).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct JointKeyCommitted {
    /// `Q = Σ Q_i` as 64-hex point.
    pub joint_pubkey: String,
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

#[cfg(test)]
mod tests {
    use super::*;
    use rand::rngs::OsRng;

    /// RT-1: DKG produces `Q == Σ Q_i`, every Schnorr PoK verifies, and every
    /// commitment opens to its revealed `Q_i`.
    #[test]
    fn rt1_dkg_round_trip() {
        let mut rng = OsRng;
        let run = DkgRun::simulate(3, &mut rng);
        // Joint key recomputed by an independent verifier equals Σ Q_i.
        let q = verify_dkg(&run.commitments, &run.shares).expect("DKG verifies");
        assert_eq!(q, run.joint_key);
        // Σ x_i · G == Q (the joint secret, never assembled in the real proto).
        let x: Scalar = run.parties.iter().map(|p| p.x_i).sum();
        assert_eq!(x * G, q);
        // Each Schnorr PoK independently verifies (under its own party id).
        for (party, share) in run.parties.iter().zip(&run.shares) {
            assert!(schnorr_verify(&party.party_id, &party.q_i, &share.pok));
        }
    }

    /// Schnorr PoK round-trips and rejects a wrong statement.
    #[test]
    fn schnorr_round_trip_and_reject() {
        let mut rng = OsRng;
        let x = Scalar::random(&mut rng);
        let q = x * G;
        let pok = schnorr_prove("party:0", &x, &q, &mut rng);
        assert!(schnorr_verify("party:0", &q, &pok));
        // A different Q (rogue key) is rejected.
        let q2 = (x + Scalar::ONE) * G;
        assert!(!schnorr_verify("party:0", &q2, &pok));
        // Corrupting s fails.
        let mut bad = pok.clone();
        bad.s = scalar_to_hex(&(scalar_from_hex(&pok.s).unwrap() + Scalar::ONE));
        assert!(!schnorr_verify("party:0", &q, &bad));
    }

    /// ADR-078 §5.1 / TH-K: a message-binding Schnorr signature
    /// (`schnorr_sign_bound`) verifies under the RIGHT `q_i` over the RIGHT
    /// message, and is REJECTED under (a) a different `q_i` (a coordinator that
    /// minted its OWN vk over a COPIED real `q_i` cannot produce a passing
    /// `vk_dlog_binding` — it lacks the real `x_i`), (b) a tampered message (the
    /// `ed25519_vk` inside `bindClaim_v2` was swapped), and (c) a copied-`q_i` /
    /// mint-vk forge attempt (signing under a DIFFERENT `x_i` over the SAME
    /// message, then presenting it against the honest `q_i`).
    #[test]
    fn schnorr_bound_round_trip_and_th_k_rejects() {
        let mut rng = OsRng;
        // The honest seat-1 party: secret x_i, public q_i = x_i·G.
        let x = Scalar::random(&mut rng);
        let q = x * G;
        let msg = br#"{"v":"mp:sigkey-bind:v2","ed25519_vk":"aa..","q_i":"<honest>"}"#;

        // (round-trip) a bound sig verifies under the right q_i + message.
        let sig = schnorr_sign_bound("party:1", &x, &q, msg, &mut rng);
        assert!(
            schnorr_verify_bound("party:1", &q, msg, &sig),
            "an honest vk_dlog_binding must verify under its own q_i + message"
        );

        // (a) REJECT under a DIFFERENT q_i (the TH-K copy-real-q_i + mint-vk
        // shape: the coordinator pairs vk' with a q_i it does NOT hold x_i for).
        let q_other = (x + Scalar::ONE) * G;
        assert!(
            !schnorr_verify_bound("party:1", &q_other, msg, &sig),
            "a vk_dlog_binding must NOT verify against a different q_i (TH-K)"
        );

        // (b) REJECT over a TAMPERED message (ed25519_vk swapped inside the claim).
        let tampered = br#"{"v":"mp:sigkey-bind:v2","ed25519_vk":"FORGED","q_i":"<honest>"}"#;
        assert!(
            !schnorr_verify_bound("party:1", &q, tampered, &sig),
            "a vk_dlog_binding must NOT verify over a tampered message (TH-K)"
        );

        // (c) FORGE attempt: the coordinator signs under its OWN x' over the SAME
        // message, then presents that sig against the HONEST q_i. It verifies only
        // against q' = x'·G, never against the honest q_i.
        let x_forge = Scalar::random(&mut rng);
        let forged = schnorr_sign_bound("party:1", &x_forge, &(x_forge * G), msg, &mut rng);
        assert!(
            !schnorr_verify_bound("party:1", &q, msg, &forged),
            "a sig produced under a different x_i must NOT verify against the honest q_i (TH-K)"
        );

        // Malformed fields are a clean reject (no panic, T9).
        let bad = SchnorrPok {
            r: "zz".repeat(32),
            s: sig.s.clone(),
        };
        assert!(!schnorr_verify_bound("party:1", &q, msg, &bad));
    }

    /// ADR-078 §5.1 domain separation: a `key_pok` (`schnorr_prove`, label
    /// `mp:schnorr:v1`) does NOT verify as a `vk_dlog_binding`
    /// (`schnorr_verify_bound`, label `mp:schnorr-bind:v1`) and vice-versa — even
    /// over the SAME `q_i`. This is what lets the TH-K close hold: copying the
    /// honest `key_pok` cannot satisfy the field-(2) `vk_dlog_binding` check.
    #[test]
    fn schnorr_bound_domain_separated_from_key_pok() {
        let mut rng = OsRng;
        let x = Scalar::random(&mut rng);
        let q = x * G;
        let msg = b"mp:sigkey-bind:v2 over this exact claim";

        // A key_pok over q (no message binding).
        let key_pok = schnorr_prove("party:0", &x, &q, &mut rng);
        // A bound sig over q + message.
        let bound = schnorr_sign_bound("party:0", &x, &q, msg, &mut rng);

        // Each verifies under its OWN scheme.
        assert!(schnorr_verify("party:0", &q, &key_pok));
        assert!(schnorr_verify_bound("party:0", &q, msg, &bound));

        // Cross-scheme MUST fail (distinct FS challenge labels).
        assert!(
            !schnorr_verify_bound("party:0", &q, msg, &key_pok),
            "a key_pok must NOT verify as a vk_dlog_binding (domain separation)"
        );
        assert!(
            !schnorr_verify("party:0", &q, &bound),
            "a vk_dlog_binding must NOT verify as a key_pok (domain separation)"
        );
    }

    /// ADR-078 §5.1: the bound signature is party-bound — a sig produced as
    /// party:0 must NOT verify when relabelled to party:1 (same q_i, same msg),
    /// mirroring the F4 key_pok attribution guarantee.
    #[test]
    fn schnorr_bound_party_attribution() {
        let mut rng = OsRng;
        let x = Scalar::random(&mut rng);
        let q = x * G;
        let msg = b"claim";
        let sig = schnorr_sign_bound("party:0", &x, &q, msg, &mut rng);
        assert!(schnorr_verify_bound("party:0", &q, msg, &sig));
        assert!(
            !schnorr_verify_bound("party:1", &q, msg, &sig),
            "a bound sig produced as party:0 must not verify as party:1"
        );
    }

    /// F4 (PoK attribution): a Schnorr PoK valid for party A's share is REJECTED
    /// when presented as party B's. Without binding `party_id` into the
    /// Fiat–Shamir challenge, the untrusted coordinator could relabel one party's
    /// PoK as another's (then attribute its DKG contribution to an identity it
    /// controls). Binding the party id makes the relabelled proof squeeze a
    /// different challenge → reject.
    #[test]
    fn f4_pok_bound_to_party_rejected_under_other_party() {
        let mut rng = OsRng;
        let x = Scalar::random(&mut rng);
        let q = x * G;
        // A genuine PoK for party A over Q.
        let pok = schnorr_prove("party:A", &x, &q, &mut rng);
        // It verifies as party A…
        assert!(schnorr_verify("party:A", &q, &pok));
        // …but is REJECTED when presented as party B (same Q, same proof bytes).
        assert!(
            !schnorr_verify("party:B", &q, &pok),
            "a PoK bound to party A must not verify as party B's (F4)"
        );
    }

    /// F4 (DKG-level): a PoK proven under party:0's label is REJECTED when its
    /// reveal is relabelled to party:1 — even when both reveals carry the SAME
    /// public share `Q_i` (so the only thing distinguishing the two PoKs is the
    /// bound party id). This isolates the party-binding: with the same `(Q, R)`,
    /// an unbound challenge would accept the relabelled PoK; the party-bound
    /// challenge rejects it. (The untrusted coordinator re-attributing a
    /// contribution to an identity it controls.)
    #[test]
    fn f4_dkg_relabelled_pok_with_same_q_rejected() {
        let mut rng = OsRng;
        // Two parties that (adversarially) share the same secret → same Q_i.
        let x = Scalar::random(&mut rng);
        let q_i = x * G;
        let mk = |id: &str, blind: Scalar| DkgParty {
            party_id: id.to_string(),
            x_i: x,
            blind,
            q_i,
        };
        let p0 = mk("party:0", Scalar::random(&mut rng));
        let p1 = mk("party:1", Scalar::random(&mut rng));
        let commitments = vec![p0.commitment(), p1.commitment()];

        // party:0's HONEST reveal (PoK bound to "party:0").
        let s0 = p0.reveal(&mut rng);
        // party:1's reveal but with party:0's PoK relabelled onto it. The
        // commitment still opens (same Q_i; p1's own blind), so only the PoK's
        // party binding can catch the relabel.
        let mut s1 = p1.reveal(&mut rng);
        s1.pok = s0.pok.clone(); // PoK produced under "party:0", presented as "party:1"

        let err = verify_dkg(&commitments, &[s0, s1]).unwrap_err();
        assert_eq!(
            err,
            DkgError::BadPok("party:1".into()),
            "a PoK proven as party:0 must not verify when relabelled as party:1 (F4)"
        );
    }

    /// TR-10: a biased DKG — a party that tries to CHANGE its share after
    /// committing (e.g. to cancel another's contribution) fails the commitment
    /// open. Commit-before-reveal prevents the bias.
    #[test]
    fn tr10_late_changed_share_fails_commitment_open() {
        let mut rng = OsRng;
        let run = DkgRun::simulate(3, &mut rng);
        // Party:0 publishes a DIFFERENT Q_i than it committed to (a late change
        // to bias the joint key), keeping the old (committed) blind.
        let mut tampered = run.shares.clone();
        let new_x = Scalar::random(&mut rng);
        let new_q = new_x * G;
        tampered[0].pubkey_share = point_to_hex(&new_q);
        // It must regenerate a PoK for the new Q to even get past the PoK check…
        tampered[0].pok = schnorr_prove("party:0", &new_x, &new_q, &mut rng);
        // …but the commitment (fixed in DKG-1) no longer opens → rejected.
        let err = verify_dkg(&run.commitments, &tampered).unwrap_err();
        assert_eq!(err, DkgError::CommitmentMismatch("party:0".into()));
    }

    /// TR-10b: a rogue key with a forged/absent PoK is rejected even if the
    /// attacker also re-commits (so the commitment opens): the Schnorr PoK
    /// proves knowledge of x_i, which a rogue (chosen to cancel others) lacks.
    #[test]
    fn tr10_rogue_key_without_pok_rejected() {
        let mut rng = OsRng;
        let mut run = DkgRun::simulate(3, &mut rng);
        // Attacker sets Q_0 = (Σ_{j≠0} Q_j) negated minus a target — a rogue key.
        // It does NOT know the discrete log, so it cannot produce a valid PoK.
        // Simulate by attaching a PoK for a *different* secret.
        let fake_secret = Scalar::random(&mut rng);
        // Keep the published Q_0 (committed) but attach a PoK for fake_secret.
        run.shares[0].pok = schnorr_prove("party:0", &fake_secret, &(fake_secret * G), &mut rng);
        let err = verify_dkg(&run.commitments, &run.shares).unwrap_err();
        assert_eq!(err, DkgError::BadPok("party:0".into()));
    }

    /// F1 (HIGH soundness): `verify_dkg` MUST reject a share set containing two
    /// shares from the SAME party (which would break the n-of-n property — one
    /// party's contribution would be counted twice and another party's never
    /// required, so a coalition smaller than n could control the joint secret).
    /// A complete, distinct-party set must still be accepted.
    #[test]
    fn f1_duplicate_party_share_rejected() {
        let mut rng = OsRng;
        let run = DkgRun::simulate(3, &mut rng);

        // Sanity: the honest, distinct-party set is accepted.
        assert!(verify_dkg(&run.commitments, &run.shares).is_ok());

        // Forge a share set with party:0 appearing twice and party:2 absent.
        // It is the same LENGTH as the commitment set (so the len() guard does
        // not catch it) and every share opens its (matching-party) commitment
        // and carries a valid PoK — only the duplicate is the defect.
        let mut dup_shares = run.shares.clone();
        dup_shares[2] = run.shares[0].clone(); // party:0 now appears at idx 0 and 2

        let err = verify_dkg(&run.commitments, &dup_shares).unwrap_err();
        assert_eq!(
            err,
            DkgError::PartySetMismatch,
            "a duplicated-party share set MUST be rejected (n-of-n hole)"
        );
    }

    /// F1 (round-1 re-audit note — invariant guard, NOT a fix): a DUPLICATED
    /// party in the COMMITMENT set is rejected via the existing bijection logic.
    /// With commitments `[P0, P1, P0]` (P0 duplicated into the P2 slot) and the
    /// honest distinct shares `[P0, P1, P2]`, the P2 share's commitment lookup
    /// fails → `PartySetMismatch`. This is the re-audit's stated reason the
    /// commitment-dup is a non-hole; the test pins that behavior so a future
    /// refactor cannot silently reopen it. (No code change accompanies this test:
    /// it documents an existing guarantee, not a new fix.)
    #[test]
    fn f1_duplicate_commitment_party_rejected() {
        let mut rng = OsRng;
        let run = DkgRun::simulate(3, &mut rng);
        // Sanity: the honest set is accepted.
        assert!(verify_dkg(&run.commitments, &run.shares).is_ok());

        // Duplicate party:0's commitment into the party:2 slot.
        let mut dup_commitments = run.commitments.clone();
        dup_commitments[2] = run.commitments[0].clone(); // party:0 now at idx 0 and 2

        assert_eq!(
            verify_dkg(&dup_commitments, &run.shares).unwrap_err(),
            DkgError::PartySetMismatch,
            "a duplicated-party COMMITMENT set must be rejected (P2's share has no commitment)"
        );
    }

    /// F1: an unknown-party share (one whose party_id is not in the commitment
    /// set) is rejected even when the lengths match.
    #[test]
    fn f1_unknown_party_share_rejected() {
        let mut rng = OsRng;
        let run = DkgRun::simulate(3, &mut rng);

        let mut shares = run.shares.clone();
        shares[1].party_id = "party:99".into(); // not in the commitment set
                                                // (its commitment lookup will fail → PartySetMismatch)
        assert_eq!(
            verify_dkg(&run.commitments, &shares).unwrap_err(),
            DkgError::PartySetMismatch
        );
    }

    /// Malformed DKG fields are clean rejects (no panic).
    #[test]
    fn malformed_fields_clean_reject() {
        let mut rng = OsRng;
        let mut run = DkgRun::simulate(2, &mut rng);
        run.shares[0].pubkey_share = "ff".repeat(32); // non-decompressable
        assert!(matches!(
            verify_dkg(&run.commitments, &run.shares),
            Err(DkgError::Malformed(_))
        ));
    }
}
