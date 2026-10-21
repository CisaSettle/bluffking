//! Threshold ElGamal decryption + Chaum–Pedersen (DLEQ) proofs — **cross-vendor
//! AI-audited (ADR-076/077/078); open-source + verifiable (ADR-063 §4, spec §4)**.
//!
//! This is the **server-blind core**: the deck is ElGamal ciphertext under the
//! joint key `Q = Σ x_i·G` from the DKG (crypto_real::dkg). To recover a card's
//! message point `M`, **all n** parties each contribute a partial decryption
//! `D_i = x_i·C1`, and `M = C2 − Σ_i D_i`. Because all n shares are required:
//!
//! - the **coordinator holds no share**, so it can decrypt nothing (threat T1);
//! - a **sub-quorum** (n−1 of n) cannot decrypt (threat T4 / TR-12).
//!
//! Each `D_i` carries a **Chaum–Pedersen / DLEQ proof** that it used the *same*
//! secret `x_i` as the party's DKG public share `Q_i = x_i·G`, i.e.
//! `log_G(Q_i) == log_{C1}(D_i)` (threat T7). A wrong/forged share fails the
//! proof and is rejected.
//!
//! ## Hole vs board (the confidentiality boundary, T2)
//!
//! The DLEQ machinery is identical for every card; **who combines the shares
//! differs** (spec §4.3a):
//!
//! - **Hole card → owner only.** Every party sends its `D_i` + DLEQ to the
//!   *owner* (out-of-band in the live protocol; only to the owner's local state
//!   in this harness). The owner combines and reads its card; the transcript
//!   carries only the share-validity proofs, with `card_id` omitted/null until
//!   showdown. The coordinator-only view (no shares) cannot combine.
//! - **Board card → all by street.** All parties publish `D_i` + DLEQ; anyone
//!   combines `M` and reads the card.
//! - **Mucked (folded, uncontested) hole cards are never revealed** — their
//!   `D_i` are never published, so they stay ElGamal forever.
//!
//! ## Status
//!
//! Cross-vendor AI-audited (ADR-076/077/078); open-source + verifiable. GA'd for
//! the engine-blind table class by ADR-070 (which lifted the ADR-063 cage); in
//! production these run ONLY for engine-blind sessions (`resolve_mp_crypto_mode`).

use crate::crypto_real::dkg::challenge_scalar;
use crate::crypto_real::dkg::DkgParty;
use crate::crypto_real::ec::{
    batch_weight, card_id_from_point, is_identity_pubkey, point_from_hex, point_to_hex,
    scalar_from_hex, scalar_to_hex, Ct,
};
use curve25519_dalek::constants::RISTRETTO_BASEPOINT_POINT as G;
// PERF: fixed-base precomputed table for G (constant-time; safe on prover paths).
use curve25519_dalek::constants::RISTRETTO_BASEPOINT_TABLE as GT;
use curve25519_dalek::ristretto::RistrettoPoint;
use curve25519_dalek::scalar::Scalar;
use curve25519_dalek::traits::{Identity, VartimeMultiscalarMul};
use merlin::Transcript;
use rand_core::{CryptoRng, RngCore};
use serde::{Deserialize, Serialize};

/// Scheme identifier for the threshold-decryption transcript field.
pub const SCHEME: &str = "cp-threshold-ristretto-v1";

// ---------------------------------------------------------------------------
// §4.2 / §4.3 — Chaum–Pedersen (DLEQ) proof for one partial decryption
// ---------------------------------------------------------------------------

/// A Chaum–Pedersen / DLEQ proof that a partial decryption `D_i = x_i·C1` used
/// the same secret `x_i` as the public share `Q_i = x_i·G`.
///
/// Wire form `{"a": "<64hex point>", "b": "<64hex point>", "s": "<64hex
/// scalar>"}` (spec §4.5). `A = k·G`, `B = k·C1`, `s = k + c·x_i`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DleqProof {
    /// `A = k·G`.
    pub a: String,
    /// `B = k·C1`.
    pub b: String,
    /// `s = k + c·x_i`.
    pub s: String,
}

/// Bind the DLEQ statement into a merlin transcript and squeeze the challenge
/// (spec §4.3). Binding `(party, deck_index, the statement points, the two
/// commitments)` makes the opening non-replayable to another card/index/party.
///
/// F3 (round-1 re-audit residual): the challenge ALSO absorbs `C2`, the second
/// ElGamal coordinate. Although the DLEQ relation itself only constrains `C1`
/// (`D_i = x_i·C1`), the per-share opening travels in the transcript alongside
/// the full ciphertext `(C1, C2)`; absorbing `C2` binds the share proof to the
/// EXACT ciphertext it opens, so a proof produced for one `(C1, C2)` cannot be
/// transplanted onto a different ciphertext that happens to share `C1`. Combined
/// with the verifier's expected-ciphertext anchor (`verify_and_open`'s `ct`
/// argument tied to the committed final deck), the threshold open is
/// self-standing rather than resting solely on the card-commit layer.
#[allow(clippy::too_many_arguments)]
fn dleq_challenge(
    party_id: &str,
    deck_index: u32,
    q_i: &RistrettoPoint,
    c1: &RistrettoPoint,
    c2: &RistrettoPoint,
    d_i: &RistrettoPoint,
    a: &RistrettoPoint,
    b: &RistrettoPoint,
) -> Scalar {
    let mut t = Transcript::new(b"mp:dleq:v1");
    t.append_message(b"party", party_id.as_bytes());
    t.append_u64(b"deck_index", deck_index as u64);
    t.append_message(b"G", G.compress().as_bytes());
    t.append_message(b"Q_i", q_i.compress().as_bytes());
    t.append_message(b"C1", c1.compress().as_bytes());
    t.append_message(b"C2", c2.compress().as_bytes());
    t.append_message(b"D_i", d_i.compress().as_bytes());
    t.append_message(b"A", a.compress().as_bytes());
    t.append_message(b"B", b.compress().as_bytes());
    challenge_scalar(&mut t, b"c")
}

/// Prove `D_i = x_i·C1` is correct (Chaum–Pedersen) for the ciphertext
/// `ct = (C1, C2)`. `q_i = x_i·G` is the party's public share. The full
/// ciphertext is bound into the challenge (F3 — see [`dleq_challenge`]).
pub fn dleq_prove<R: RngCore + CryptoRng>(
    party_id: &str,
    deck_index: u32,
    x_i: &Scalar,
    q_i: &RistrettoPoint,
    ct: &Ct,
    d_i: &RistrettoPoint,
    rng: &mut R,
) -> DleqProof {
    let k = Scalar::random(rng);
    let a = &k * GT;
    let b = k * ct.c1;
    let c = dleq_challenge(party_id, deck_index, q_i, &ct.c1, &ct.c2, d_i, &a, &b);
    let s = k + c * x_i;
    DleqProof {
        a: point_to_hex(&a),
        b: point_to_hex(&b),
        s: scalar_to_hex(&s),
    }
}

/// Verify a Chaum–Pedersen DLEQ proof: `s·G == A + c·Q_i` AND `s·C1 == B + c·D_i`
/// (spec §4.2), with the challenge bound to the full ciphertext `(C1, C2)` (F3).
/// A wrong/forged share (different `x_i`, or `D_i` not derived from `C1`) fails
/// both checks; a proof transplanted onto a different ciphertext (different `C2`)
/// squeezes a different challenge and fails. Returns `false` on any malformed
/// field (clean reject, no panic).
pub fn dleq_verify(
    party_id: &str,
    deck_index: u32,
    q_i: &RistrettoPoint,
    ct: &Ct,
    d_i: &RistrettoPoint,
    proof: &DleqProof,
) -> bool {
    let a = match point_from_hex(&proof.a) {
        Some(p) => p,
        None => return false,
    };
    let b = match point_from_hex(&proof.b) {
        Some(p) => p,
        None => return false,
    };
    let s = match scalar_from_hex(&proof.s) {
        Some(s) => s,
        None => return false,
    };
    let c = dleq_challenge(party_id, deck_index, q_i, &ct.c1, &ct.c2, d_i, &a, &b);
    (&s * GT == a + c * q_i) && (s * ct.c1 == b + c * d_i)
}

// ---------------------------------------------------------------------------
// §4.5 — per-party decryption share + the multi-party opening proof
// ---------------------------------------------------------------------------

/// One party's contribution to opening a card: `D_i = x_i·C1` and its DLEQ.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DecryptionShare {
    /// Party identifier (must match a DKG public share `Q_i`).
    pub party_id: String,
    /// `D_i = x_i·C1` as 64-hex point.
    pub d_i: String,
    /// Chaum–Pedersen proof that `D_i` is correct.
    pub dleq: DleqProof,
}

/// The full opening proof for one card: one share per party (spec §4.5).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ThresholdDecryptionProof {
    /// Scheme identifier (`cp-threshold-ristretto-v1`).
    pub scheme: String,
    /// One decryption share per party (all `n` required for n-of-n).
    pub shares: Vec<DecryptionShare>,
}

/// Compute a single party's decryption share `D_i = x_i·C1` + its DLEQ proof.
pub fn partial_decrypt<R: RngCore + CryptoRng>(
    party: &DkgParty,
    deck_index: u32,
    ct: &Ct,
    rng: &mut R,
) -> DecryptionShare {
    let d_i = party.x_i * ct.c1;
    let dleq = dleq_prove(
        &party.party_id,
        deck_index,
        &party.x_i,
        &party.q_i,
        ct,
        &d_i,
        rng,
    );
    DecryptionShare {
        party_id: party.party_id.clone(),
        d_i: point_to_hex(&d_i),
        dleq,
    }
}

/// Errors from threshold opening (all clean rejects, never panics).
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum OpenError {
    /// A share / Q_i field was malformed (T9).
    #[error("malformed decryption field for {0}")]
    Malformed(String),
    /// A DLEQ share proof failed (T7 / TR-4 / TR-5).
    #[error("DLEQ proof failed for {0}")]
    BadProof(String),
    /// The party set of the shares did not match the DKG public shares (e.g. a
    /// sub-quorum, T4 / TR-12, or a duplicate/missing party).
    #[error("share set does not match the n DKG parties")]
    QuorumMismatch,
    /// The recovered point was not one of the 52 card points.
    #[error("recovered point is not a valid card")]
    NotACard,
}

/// Verify one authenticated party's partial decryption against the committed
/// ciphertext and that party's published DKG public key. This is the safe
/// coordinator-side primitive for relays: it validates routing and DLEQ
/// soundness without combining shares or materializing the plaintext card.
pub fn verify_decryption_share(
    deck_index: u32,
    ct: &Ct,
    party_pubkeys: &[(String, RistrettoPoint)],
    share: &DecryptionShare,
) -> Result<(), OpenError> {
    let q_i = party_pubkeys
        .iter()
        .find(|(id, _)| id == &share.party_id)
        .map(|(_, q)| *q)
        .ok_or(OpenError::QuorumMismatch)?;

    // Keep the same defense-in-depth identity-key rejection as the full
    // threshold verifier. A caller-supplied directory is not trusted merely
    // because it has the right party id.
    if is_identity_pubkey(&q_i) {
        return Err(OpenError::BadProof(share.party_id.clone()));
    }
    let d_i =
        point_from_hex(&share.d_i).ok_or_else(|| OpenError::Malformed(share.party_id.clone()))?;
    if !dleq_verify(&share.party_id, deck_index, &q_i, ct, &d_i, &share.dleq) {
        return Err(OpenError::BadProof(share.party_id.clone()));
    }
    Ok(())
}

/// Verify every share's DLEQ against the published DKG public shares `Q_i`
/// (`party_id → Q_i`) and recover the card id by combining `M = C2 − Σ D_i`.
///
/// **n-of-n quorum check (T4 / TR-12):** the share set must be exactly the DKG
/// party set — same parties, no missing party, no duplicate. A sub-quorum is a
/// `QuorumMismatch`, NOT a silent partial recovery.
pub fn verify_and_open(
    deck_index: u32,
    ct: &Ct,
    party_pubkeys: &[(String, RistrettoPoint)],
    proof: &ThresholdDecryptionProof,
) -> Result<u8, OpenError> {
    if proof.scheme != SCHEME {
        return Err(OpenError::BadProof("scheme".into()));
    }
    // n-of-n: exactly one share per DKG party, no missing, no duplicate.
    if proof.shares.len() != party_pubkeys.len() {
        return Err(OpenError::QuorumMismatch);
    }
    // PERF: the n DLEQ proofs are checked as ONE random-weighted batch instead of
    // 2n separate double-base equalities. Per share i the two equations are
    //     (1)  s_i·G  − A_i − c_i·Q_i == O        (2)  s_i·C1 − B_i − c_i·D_i == O
    // and each gets its OWN fresh independent 128-bit weight (ρ_i, σ_i), so the
    // batch rejects whenever ANY share's DLEQ is wrong except with probability
    // ≈ 2⁻¹²⁸ (see `ec::batch_weight`). Per-share binding is untouched: each c_i
    // is still squeezed over that share's own party id, deck index and points, so
    // shares cannot be swapped between parties or ciphertexts. Weights are
    // verifier-local OS randomness (a prover cannot grind them) and every batched
    // input is public, so the variable-time multiscalar leaks nothing.
    //
    // Structural rejects (unknown / duplicate party, identity Q_i, malformed
    // field) stay EAGER and in the same order as `verify_decryption_share`, so the
    // returned error variants are unchanged. If only the batch fails we re-run the
    // per-share verifier to name the offending party — an attack-only path.
    let mut sum_d = RistrettoPoint::default(); // identity
    let mut seen: Vec<&str> = Vec::with_capacity(proof.shares.len());
    let mut rng = rand::rngs::OsRng;
    let mut g_coeff = Scalar::ZERO; // coefficient of G
    let mut c1_coeff = Scalar::ZERO; // coefficient of C1
    let mut pts: Vec<RistrettoPoint> = Vec::with_capacity(4 * proof.shares.len());
    let mut coeffs: Vec<Scalar> = Vec::with_capacity(4 * proof.shares.len());
    for share in &proof.shares {
        if seen.contains(&share.party_id.as_str()) {
            return Err(OpenError::QuorumMismatch); // duplicate party
        }
        seen.push(&share.party_id);
        let q_i = party_pubkeys
            .iter()
            .find(|(id, _)| id == &share.party_id)
            .map(|(_, q)| *q)
            .ok_or(OpenError::QuorumMismatch)?;
        // Same defense-in-depth identity-key rejection as `verify_decryption_share`.
        if is_identity_pubkey(&q_i) {
            return Err(OpenError::BadProof(share.party_id.clone()));
        }
        let d_i = point_from_hex(&share.d_i)
            .ok_or_else(|| OpenError::Malformed(share.party_id.clone()))?;
        // A malformed DLEQ field is a `false` from `dleq_verify`, i.e. BadProof.
        let a = point_from_hex(&share.dleq.a)
            .ok_or_else(|| OpenError::BadProof(share.party_id.clone()))?;
        let b = point_from_hex(&share.dleq.b)
            .ok_or_else(|| OpenError::BadProof(share.party_id.clone()))?;
        let s = scalar_from_hex(&share.dleq.s)
            .ok_or_else(|| OpenError::BadProof(share.party_id.clone()))?;
        let c = dleq_challenge(
            &share.party_id,
            deck_index,
            &q_i,
            &ct.c1,
            &ct.c2,
            &d_i,
            &a,
            &b,
        );

        let rho = batch_weight(&mut rng); // weights equation (1)
        g_coeff += rho * s;
        pts.push(a);
        coeffs.push(-rho);
        pts.push(q_i);
        coeffs.push(-(rho * c));

        let sigma = batch_weight(&mut rng); // weights equation (2)
        c1_coeff += sigma * s;
        pts.push(b);
        coeffs.push(-sigma);
        pts.push(d_i);
        coeffs.push(-(sigma * c));

        sum_d += d_i;
    }
    let batch = RistrettoPoint::vartime_multiscalar_mul(
        [g_coeff, c1_coeff].into_iter().chain(coeffs),
        [G, ct.c1].into_iter().chain(pts),
    );
    if batch != RistrettoPoint::identity() {
        // At least one DLEQ is bad; find which one so the error names the party.
        for share in &proof.shares {
            verify_decryption_share(deck_index, ct, party_pubkeys, share)?;
        }
        // Unreachable unless the batch and the per-share path disagree.
        return Err(OpenError::BadProof("batch".into()));
    }
    let m = ct.c2 - sum_d;
    card_id_from_point(&m).ok_or(OpenError::NotACard)
}

/// Combine raw partial-decryption points (no proof check) — for the owner's
/// local recovery once it has verified each share. `M = C2 − Σ D_i`.
pub fn combine(ct: &Ct, shares: &[RistrettoPoint]) -> Option<u8> {
    let sum_d: RistrettoPoint = shares.iter().sum();
    card_id_from_point(&(ct.c2 - sum_d))
}

// ---------------------------------------------------------------------------
// F3 — the offline-verifier seam: a real DecryptionProvider for the
// cp-threshold-ristretto scheme.
// ---------------------------------------------------------------------------

/// Wire form carried in [`DecryptionProof::attestation`] for the real
/// threshold-decryption scheme (canonical-JSON-then-hex). It bundles the card's
/// ElGamal ciphertext with the n-of-n threshold opening so the offline verifier
/// is fully self-contained (the trait only passes `(party, deck_index, card_id,
/// salt)`, not the ciphertext).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ThresholdOpenWire {
    /// The card's ElGamal ciphertext `(C1, C2)`.
    pub ct: crate::crypto_real::ec::CtWire,
    /// The n-of-n threshold opening (one DLEQ-proved share per DKG party).
    pub threshold: ThresholdDecryptionProof,
}

/// Serialize a [`ThresholdOpenWire`] into the `attestation` hex string.
pub fn encode_threshold_attestation(open: &ThresholdOpenWire) -> String {
    let bytes =
        crate::hash::canonical_json(&serde_json::to_value(open).expect("serialize threshold open"));
    hex::encode(bytes)
}

fn decode_threshold_attestation(att: &str) -> Option<ThresholdOpenWire> {
    let bytes = hex::decode(att).ok()?;
    serde_json::from_slice(&bytes).ok()
}

/// A real [`DecryptionProvider`](crate::crypto::DecryptionProvider) for the
/// `cp-threshold-ristretto-v1` scheme — **cross-vendor AI-audited
/// (ADR-076/077/078); open-source + verifiable (ADR-063, audit F3)**.
///
/// `verify_decryption` decodes the ciphertext + n-of-n threshold opening from the
/// attestation, verifies **every** share's Chaum–Pedersen DLEQ against the
/// DKG-published party public keys (held by the provider), enforces the n-of-n
/// quorum (exactly one share per party, no dup/missing), combines `M = C2 − Σ D_i`,
/// and checks the recovered card id equals the claimed `card_id`. A forged or
/// sub-quorum opening is rejected — so an exported real-crypto transcript is
/// end-to-end checkable offline (F3).
///
/// The provider holds the party public keys `Q_i` (the DKG public shares); the
/// offline verifier reconstructs them from the transcript's `key_registered`
/// events (`Σ` of which is the joint key the shuffle is bound to — F2).
///
/// F3 (round-1 re-audit residual): the provider can ALSO hold the verified final
/// ciphertext deck (`expected_deck`). When present, `verify_decryption` anchors
/// the attestation's self-supplied `open.ct` to `expected_deck[deck_index]` and
/// rejects any mismatch — so the threshold open is checked against the EXACT
/// ciphertext the committed final deck froze, not a ciphertext the prover chose.
/// Without the anchor (`None`), the open's ciphertext is unconstrained and card
/// integrity rests solely on the card-commit layer; the anchor closes that gap
/// whenever the final ciphertext deck is available to the verifier (the
/// real-shuffle path, where the deck travels in the shuffle attestation).
pub struct RealThresholdDecryptionProvider {
    party_pubkeys: Vec<(String, RistrettoPoint)>,
    expected_deck: Option<Vec<Ct>>,
}

impl RealThresholdDecryptionProvider {
    /// Build the verifier-side provider from the DKG party public keys (no
    /// ciphertext anchor — card integrity then rests on the card-commit layer).
    pub fn new(party_pubkeys: Vec<(String, RistrettoPoint)>) -> Self {
        RealThresholdDecryptionProvider {
            party_pubkeys,
            expected_deck: None,
        }
    }

    /// Build the verifier-side provider WITH the verified final ciphertext deck
    /// (F3 anchor). Every opened card's attestation ciphertext must equal
    /// `expected_deck[deck_index]` — a prover cannot open a ciphertext other than
    /// the one the committed final deck froze.
    pub fn with_expected_deck(
        party_pubkeys: Vec<(String, RistrettoPoint)>,
        expected_deck: Vec<Ct>,
    ) -> Self {
        RealThresholdDecryptionProvider {
            party_pubkeys,
            expected_deck: Some(expected_deck),
        }
    }
}

impl crate::crypto::DecryptionProvider for RealThresholdDecryptionProvider {
    fn scheme(&self) -> &'static str {
        SCHEME
    }

    fn prove_decryption(
        &self,
        _party: &str,
        _deck_index: u32,
        _card_id: u8,
        _salt: &crate::crypto::Salt,
    ) -> crate::crypto::DecryptionProof {
        // The real opening is produced by the parties (each contributes a DLEQ
        // share); this verifier-side provider does not synthesize proofs.
        // Transcript builders call `encode_threshold_attestation` directly.
        unimplemented!(
            "RealThresholdDecryptionProvider is verify-only; build the attestation \
             with encode_threshold_attestation from the parties' shares"
        )
    }

    fn verify_decryption(
        &self,
        _party: &str,
        deck_index: u32,
        card_id: u8,
        _salt: &crate::crypto::Salt,
        proof: &crate::crypto::DecryptionProof,
    ) -> bool {
        if proof.scheme != SCHEME {
            return false;
        }
        let open = match decode_threshold_attestation(&proof.attestation) {
            Some(o) => o,
            None => return false,
        };
        let ct = match Ct::from_wire(&open.ct) {
            Some(c) => c,
            None => return false,
        };
        // F3 anchor: when the verifier knows the committed final ciphertext deck,
        // the attestation's self-supplied ciphertext MUST equal the deck entry at
        // this index. A prover cannot substitute a ciphertext it can open more
        // conveniently — the open is checked against the EXACT frozen ciphertext.
        if let Some(deck) = &self.expected_deck {
            match deck.get(deck_index as usize) {
                Some(expected) if expected.c1 == ct.c1 && expected.c2 == ct.c2 => {}
                _ => return false,
            }
        }
        // Verify every DLEQ + n-of-n quorum and recover the card id.
        match verify_and_open(deck_index, &ct, &self.party_pubkeys, &open.threshold) {
            // The recovered card must equal the claimed card_id (which the state
            // machine has independently pinned to the committed deck via
            // card_commit(card_id, salt)). A mismatch means the threshold open
            // does not actually decrypt to the card the transcript claims.
            Ok(recovered) => recovered == card_id,
            Err(_) => false,
        }
    }
}
