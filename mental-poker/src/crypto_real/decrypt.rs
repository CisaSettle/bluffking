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
    card_id_from_point, is_identity_pubkey, point_from_hex, point_to_hex, scalar_from_hex,
    scalar_to_hex, Ct,
};
use curve25519_dalek::constants::RISTRETTO_BASEPOINT_POINT as G;
use curve25519_dalek::ristretto::RistrettoPoint;
use curve25519_dalek::scalar::Scalar;
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
    let a = k * G;
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
    (s * G == a + c * q_i) && (s * ct.c1 == b + c * d_i)
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
    let mut sum_d = RistrettoPoint::default(); // identity
    let mut seen: Vec<&str> = Vec::with_capacity(proof.shares.len());
    for share in &proof.shares {
        if seen.contains(&share.party_id.as_str()) {
            return Err(OpenError::QuorumMismatch); // duplicate party
        }
        seen.push(&share.party_id);
        verify_decryption_share(deck_index, ct, party_pubkeys, share)?;
        let d_i =
            point_from_hex(&share.d_i).expect("verify_decryption_share accepted canonical d_i");
        sum_d += d_i;
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::crypto_real::dkg::{DkgParty, DkgRun};
    use crate::crypto_real::ec::{card_point, DECK_SIZE};
    use rand::rngs::OsRng;

    /// Build a full proof for one card by having all parties partial-decrypt.
    fn open_proof(
        run: &DkgRun,
        deck_index: u32,
        ct: &Ct,
        rng: &mut OsRng,
    ) -> ThresholdDecryptionProof {
        let shares = run
            .parties
            .iter()
            .map(|p| partial_decrypt(p, deck_index, ct, rng))
            .collect();
        ThresholdDecryptionProof {
            scheme: SCHEME.to_string(),
            shares,
        }
    }

    fn pubkeys(run: &DkgRun) -> Vec<(String, RistrettoPoint)> {
        run.parties
            .iter()
            .map(|p| (p.party_id.clone(), p.q_i))
            .collect()
    }

    /// Relay verification is intentionally single-share (it must not combine or
    /// reveal a card), but it still binds the authenticated party, deck index,
    /// exact committed ciphertext, partial decryption, and DKG key. Any change
    /// to one of those fields must fail before the coordinator counts the share.
    #[test]
    fn verify_decryption_share_binds_sender_index_ciphertext_and_proof() {
        let mut rng = OsRng;
        let run = DkgRun::simulate(3, &mut rng);
        let pks = pubkeys(&run);
        let deck_index = 17;
        let ct = Ct::encrypt_card(31, &run.joint_key, &Scalar::random(&mut rng));
        let share = partial_decrypt(&run.parties[1], deck_index, &ct, &mut rng);

        assert_eq!(
            verify_decryption_share(deck_index, &ct, &pks, &share),
            Ok(()),
            "a genuine opaque share must verify without opening the card"
        );
        assert!(matches!(
            verify_decryption_share(deck_index + 1, &ct, &pks, &share),
            Err(OpenError::BadProof(party)) if party == "party:1"
        ));

        let mut wrong_sender = share.clone();
        wrong_sender.party_id = "party:0".into();
        assert!(matches!(
            verify_decryption_share(deck_index, &ct, &pks, &wrong_sender),
            Err(OpenError::BadProof(party)) if party == "party:0"
        ));

        let mut unknown_sender = share.clone();
        unknown_sender.party_id = "party:99".into();
        assert_eq!(
            verify_decryption_share(deck_index, &ct, &pks, &unknown_sender),
            Err(OpenError::QuorumMismatch)
        );

        let mut changed_ct = ct;
        changed_ct.c2 += G;
        assert!(matches!(
            verify_decryption_share(deck_index, &changed_ct, &pks, &share),
            Err(OpenError::BadProof(party)) if party == "party:1"
        ));

        let mut changed_share = share.clone();
        changed_share.d_i = point_to_hex(&(point_from_hex(&share.d_i).unwrap() + G));
        assert!(matches!(
            verify_decryption_share(deck_index, &ct, &pks, &changed_share),
            Err(OpenError::BadProof(party)) if party == "party:1"
        ));

        let mut malformed = share;
        malformed.d_i = "not-a-point".into();
        assert!(matches!(
            verify_decryption_share(deck_index, &ct, &pks, &malformed),
            Err(OpenError::Malformed(party)) if party == "party:1"
        ));
    }

    /// Code-review #6 (F-CRYPTO-15 at the consuming verifier): a malicious party
    /// registering an IDENTITY public key (`x_i = 0`) produces a zero decryption
    /// share whose DLEQ verifies trivially. WITHOUT the guard the remaining
    /// honest parties recover every card without it — n-of-n silently degraded
    /// to (n-1)-of-(n-1). `verify_and_open` must reject the identity `Q_i`.
    #[test]
    fn verify_and_open_rejects_identity_pubkey_zero_share() {
        let mut rng = OsRng;
        // Two HONEST parties form the effective decryption key.
        let run = DkgRun::simulate(2, &mut rng);
        // A malicious zero-share party: x=0 ⇒ Q=identity, D_i = 0·C1 = identity,
        // and its DLEQ (equality of two zero discrete logs) verifies.
        let evil = DkgParty {
            party_id: "evil".into(),
            x_i: Scalar::from(0u64),
            blind: Scalar::from(0u64),
            q_i: RistrettoPoint::default(), // identity
        };
        // The 3-party joint key equals the 2 honest parties' joint (identity adds
        // nothing), so the honest joint encrypts the card.
        let card = 21u8;
        let r = Scalar::random(&mut rng);
        let ct = Ct::encrypt_card(card, &run.joint_key, &r);

        // n=3 shares: 2 honest + the zero-share. pks lists all three Q_i.
        let mut shares: Vec<DecryptionShare> = run
            .parties
            .iter()
            .map(|p| partial_decrypt(p, 7, &ct, &mut rng))
            .collect();
        shares.push(partial_decrypt(&evil, 7, &ct, &mut rng));
        let proof = ThresholdDecryptionProof {
            scheme: SCHEME.to_string(),
            shares,
        };
        let mut pks = pubkeys(&run);
        pks.push((evil.party_id.clone(), evil.q_i));

        // Pre-guard this WOULD open (the zero share contributes identity and the
        // two honest shares recover the card — the n-of-n→(n-1) downgrade). The
        // guard must reject the identity key instead.
        let got = verify_and_open(7, &ct, &pks, &proof);
        assert!(
            matches!(&got, Err(OpenError::BadProof(p)) if p == "evil"),
            "identity Q_i must be rejected (F-CRYPTO-15), got {got:?}"
        );
    }

    /// RT-2: encrypt all 52 card_point(j) under Q, n-of-n partial-decrypt,
    /// recover all 52 ids 0..51 via the DL table, each exactly once.
    #[test]
    fn rt2_encrypt_recover_all_52() {
        let mut rng = OsRng;
        let run = DkgRun::simulate(3, &mut rng);
        let pks = pubkeys(&run);
        let mut recovered = [false; DECK_SIZE];
        for id in 0..DECK_SIZE as u8 {
            let r = Scalar::random(&mut rng);
            let ct = Ct::encrypt_card(id, &run.joint_key, &r);
            let proof = open_proof(&run, id as u32, &ct, &mut rng);
            let got = verify_and_open(id as u32, &ct, &pks, &proof).expect("opens");
            assert_eq!(got, id, "card {id} mis-recovered");
            assert!(!recovered[got as usize], "card {got} recovered twice");
            recovered[got as usize] = true;
        }
        assert!(
            recovered.iter().all(|&b| b),
            "all 52 recovered exactly once"
        );
    }

    /// RT-4 (hole→owner / board→all) + TR-11 (server-blindness, non-negotiable)
    /// + TR-12 (sub-quorum cannot decrypt).
    ///
    /// THIS IS THE HEADLINE CONFIDENTIALITY TEST. It proves three things at once:
    ///   1. the owner (with all n shares) recovers its hole card;
    ///   2. a coordinator-only view (transcript + ciphertext + public keys, ZERO
    ///      secret shares) cannot recover ANY card — server-blindness;
    ///   3. a sub-quorum (n−1 of n) cannot recover the card.
    #[test]
    fn rt4_tr11_tr12_server_blind_and_quorum() {
        let mut rng = OsRng;
        let n = 3;
        let run = DkgRun::simulate(n, &mut rng);
        let pks = pubkeys(&run);

        // A secret "hole card" id encrypted under the joint key.
        let hole_id = 37u8;
        let r = Scalar::random(&mut rng);
        let ct = Ct::encrypt_card(hole_id, &run.joint_key, &r);

        // (1) Owner combines all n verified shares → recovers the card.
        let proof = open_proof(&run, 5, &ct, &mut rng);
        let opened = verify_and_open(5, &ct, &pks, &proof).expect("owner opens");
        assert_eq!(opened, hole_id, "owner must recover its hole card");

        // (2) TR-11 — SERVER-BLINDNESS (the non-negotiable). A view holding ONLY
        // the coordinator's data — the ciphertext, the public keys, and the
        // transcript — but ZERO secret shares cannot recover the card. We model
        // "the coordinator tries everything it has": it has C1, C2, Q, every Q_i.
        // The ONLY way to get M is C2 − Σ x_i·C1, and it has no x_i. Concretely:
        //   - it cannot compute any D_i (needs x_i);
        //   - Σ Q_i = Q tells it nothing about Σ x_i·C1 (different base C1≠G).
        // Assert the recovered-point search over everything-the-coordinator-has
        // never yields the card point. We brute-check the obvious wrong combines:
        let coordinator_guesses = [
            ct.c2,                 // C2 alone (= M + r·Q)
            ct.c2 - run.joint_key, // subtract Q
            ct.c2 - ct.c1,         // subtract C1
            ct.c2 - run.joint_key * Scalar::from(1u64),
            ct.c1,
        ];
        for g in coordinator_guesses {
            assert_ne!(
                card_id_from_point(&g),
                Some(hole_id),
                "coordinator-only view must NOT recover the hole card id"
            );
        }
        // And the strong statement: without ANY x_i, C2 − (anything the
        // coordinator can build from public data) ≠ card_point(hole_id) unless
        // it solves a discrete log. We assert the only correct combiner needs
        // the shares: replacing one real D_i with the public Q_i breaks it.
        let mut public_only_sum = RistrettoPoint::default();
        for (_, q_i) in &pks {
            public_only_sum += *q_i; // = Q, public — NOT Σ x_i·C1
        }
        assert_ne!(
            card_id_from_point(&(ct.c2 - public_only_sum)),
            Some(hole_id),
            "combining public Q_i (not x_i·C1) must not open the card"
        );

        // (3) TR-12 — a sub-quorum (drop party:2's share) cannot decrypt. The
        // quorum check rejects it (n−1 ≠ n), and even if it didn't, the combined
        // M would be wrong (missing x_2·C1) → not a card.
        let mut subquorum = proof.clone();
        subquorum.shares.pop(); // n−1 shares
        assert_eq!(
            verify_and_open(5, &ct, &pks, &subquorum),
            Err(OpenError::QuorumMismatch),
            "n−1 of n must be rejected by the quorum check"
        );
        // Direct combine of only n−1 raw shares yields a wrong point.
        let n_minus_1: Vec<RistrettoPoint> =
            run.parties[..n - 1].iter().map(|p| p.x_i * ct.c1).collect();
        assert_ne!(
            combine(&ct, &n_minus_1),
            Some(hole_id),
            "n−1 pooled shares must not recover the card"
        );

        // (4) F5 — STRONG server-blindness: NO strict subset of the n secret
        // shares opens the card. We enumerate ALL 2^n − 1 proper subsets of the
        // real per-party partial decryptions D_i = x_i·C1 (the empty subset = the
        // coordinator's own view, which holds zero shares) and assert none of them
        // combines to the hole card. Only the FULL set (all n shares) recovers it.
        // This rules out any sub-coalition (including the server) decrypting,
        // beyond the single n−1 case above.
        let all_d: Vec<RistrettoPoint> = run.parties.iter().map(|p| p.x_i * ct.c1).collect();
        for mask in 0u32..(1u32 << n) {
            let subset: Vec<RistrettoPoint> = (0..n)
                .filter(|i| mask & (1 << i) != 0)
                .map(|i| all_d[i])
                .collect();
            let opened = combine(&ct, &subset);
            if (mask as usize).count_ones() as usize == n {
                // The full set is the ONLY combiner that recovers the card.
                assert_eq!(
                    opened,
                    Some(hole_id),
                    "the full n-of-n set must recover the card"
                );
            } else {
                assert_ne!(
                    opened,
                    Some(hole_id),
                    "strict subset {mask:#b} of shares (incl. the empty/server view) must NOT open the card"
                );
            }
        }

        // (5) F5 — the server cannot fabricate a valid opening either: with only
        // PUBLIC data (Q_i) it cannot produce even ONE valid partial-decryption
        // proof, because the DLEQ binds D_i to the SECRET x_i via Q_i = x_i·G and
        // D_i = x_i·C1. A server-built share D' = q_i (a public point) with any
        // proof fails verify_and_open. We confirm the only accepted proof is the
        // genuine secret-share one.
        let mut forged = proof.clone();
        // Replace party:0's real D_i with its PUBLIC Q_0 (server has this) and
        // keep the genuine DLEQ (which was for the real D_0): mismatch → reject.
        forged.shares[0].d_i = point_to_hex(&run.parties[0].q_i);
        assert!(
            matches!(
                verify_and_open(5, &ct, &pks, &forged),
                Err(OpenError::BadProof(_)) | Err(OpenError::NotACard)
            ),
            "a server-forged share from public data must be rejected"
        );
    }

    /// TR-4: corrupt one field (a, b, or s) of a DLEQ share → reject.
    #[test]
    fn tr4_corrupt_dleq_field_rejected() {
        let mut rng = OsRng;
        let run = DkgRun::simulate(3, &mut rng);
        let pks = pubkeys(&run);
        let r = Scalar::random(&mut rng);
        let ct = Ct::encrypt_card(9, &run.joint_key, &r);
        let proof = open_proof(&run, 2, &ct, &mut rng);

        // Corrupt `s`.
        let mut bad_s = proof.clone();
        let s = scalar_from_hex(&bad_s.shares[0].dleq.s).unwrap();
        bad_s.shares[0].dleq.s = scalar_to_hex(&(s + Scalar::ONE));
        assert_eq!(
            verify_and_open(2, &ct, &pks, &bad_s),
            Err(OpenError::BadProof("party:0".into()))
        );

        // Corrupt `a`.
        let mut bad_a = proof.clone();
        bad_a.shares[1].dleq.a =
            point_to_hex(&(point_from_hex(&proof.shares[1].dleq.a).unwrap() + G));
        assert_eq!(
            verify_and_open(2, &ct, &pks, &bad_a),
            Err(OpenError::BadProof("party:1".into()))
        );

        // Corrupt `b`.
        let mut bad_b = proof.clone();
        bad_b.shares[2].dleq.b =
            point_to_hex(&(point_from_hex(&proof.shares[2].dleq.b).unwrap() + G));
        assert_eq!(
            verify_and_open(2, &ct, &pks, &bad_b),
            Err(OpenError::BadProof("party:2".into()))
        );
    }

    /// TR-5: publish a partial decryption from a DIFFERENT x_i than the DKG
    /// share → reject. A malicious party uses a wrong secret to compute D_i (and
    /// even attaches a self-consistent DLEQ for that wrong secret), but the DLEQ
    /// is checked against the *DKG-published* Q_i, so it fails.
    #[test]
    fn tr5_wrong_secret_share_rejected() {
        let mut rng = OsRng;
        let run = DkgRun::simulate(3, &mut rng);
        let pks = pubkeys(&run);
        let r = Scalar::random(&mut rng);
        let ct = Ct::encrypt_card(15, &run.joint_key, &r);

        // party:1 cheats: uses wrong_x instead of its real x_1.
        let wrong_x = Scalar::random(&mut rng);
        let wrong_d = wrong_x * ct.c1;
        // A DLEQ that is self-consistent for (wrong_x·G, wrong_x·C1)…
        let wrong_q = wrong_x * G;
        let cheat_dleq = dleq_prove("party:1", 4, &wrong_x, &wrong_q, &ct, &wrong_d, &mut rng);

        let mut shares: Vec<DecryptionShare> = run
            .parties
            .iter()
            .map(|p| partial_decrypt(p, 4, &ct, &mut rng))
            .collect();
        // Replace party:1's honest share with the cheating one.
        shares[1] = DecryptionShare {
            party_id: "party:1".into(),
            d_i: point_to_hex(&wrong_d),
            dleq: cheat_dleq,
        };
        let proof = ThresholdDecryptionProof {
            scheme: SCHEME.to_string(),
            shares,
        };
        // …fails because it is verified against the REAL DKG Q_1, not wrong_q.
        assert_eq!(
            verify_and_open(4, &ct, &pks, &proof),
            Err(OpenError::BadProof("party:1".into())),
            "a share from a different x_i than the DKG share must be rejected"
        );
    }

    /// F3: the real `DecryptionProvider` impl (`RealThresholdDecryptionProvider`)
    /// — the offline-verifier seam — accepts an honest threshold attestation and
    /// rejects (a) a tampered share, (b) a wrong claimed card_id, (c) a
    /// sub-quorum opening.
    #[test]
    fn f3_real_threshold_decryption_provider_verifies_and_rejects() {
        use crate::crypto::DecryptionProvider;
        use crate::crypto_real::decrypt::{
            encode_threshold_attestation, RealThresholdDecryptionProvider, ThresholdOpenWire,
        };
        let mut rng = OsRng;
        let run = DkgRun::simulate(3, &mut rng);
        let provider = RealThresholdDecryptionProvider::new(pubkeys(&run));
        let salt = [0u8; 32];

        let card_id = 23u8;
        let r = Scalar::random(&mut rng);
        let ct = Ct::encrypt_card(card_id, &run.joint_key, &r);
        let make = |shares: Vec<DecryptionShare>| -> crate::crypto::DecryptionProof {
            crate::crypto::DecryptionProof {
                scheme: SCHEME.to_string(),
                attestation: encode_threshold_attestation(&ThresholdOpenWire {
                    ct: ct.to_wire(),
                    threshold: ThresholdDecryptionProof {
                        scheme: SCHEME.to_string(),
                        shares,
                    },
                }),
            }
        };
        let honest_shares: Vec<DecryptionShare> = run
            .parties
            .iter()
            .map(|p| partial_decrypt(p, 9, &ct, &mut rng))
            .collect();

        // (a) honest open verifies for the right card_id at the right index.
        let good = make(honest_shares.clone());
        assert!(provider.verify_decryption("party:0", 9, card_id, &salt, &good));

        // (b) a WRONG claimed card_id is rejected (recovered id != claimed).
        assert!(!provider.verify_decryption("party:0", 9, card_id + 1, &salt, &good));

        // (c) a sub-quorum (n−1 shares) is rejected (n-of-n quorum check).
        let mut sub = honest_shares.clone();
        sub.pop();
        assert!(!provider.verify_decryption("party:0", 9, card_id, &salt, &make(sub)));

        // (d) a tampered share's DLEQ is rejected.
        let mut bad = honest_shares;
        let s = scalar_from_hex(&bad[1].dleq.s).unwrap();
        bad[1].dleq.s = scalar_to_hex(&(s + Scalar::ONE));
        assert!(!provider.verify_decryption("party:0", 9, card_id, &salt, &make(bad)));

        // (e) wrong scheme id on the proof is rejected.
        let mut wrong_scheme = good.clone();
        wrong_scheme.scheme = "mock-decrypt-v1".into();
        assert!(!provider.verify_decryption("party:0", 9, card_id, &salt, &wrong_scheme));
    }

    /// F3 (round-1 re-audit residual — ciphertext anchor). When the verifier
    /// knows the committed final ciphertext deck, a `RealThresholdDecryptionProvider`
    /// built with `with_expected_deck` MUST reject an opening whose self-supplied
    /// `open.ct` is NOT the deck's ciphertext at that index — even if that opening
    /// is internally consistent (valid n-of-n DLEQ shares recovering a real card)
    /// for the substituted ciphertext. Without the anchor the prover could open a
    /// ciphertext it never committed to; the anchor ties the open to the EXACT
    /// frozen ciphertext.
    #[test]
    fn f3_provider_anchors_open_to_committed_ciphertext() {
        use crate::crypto::DecryptionProvider;
        let mut rng = OsRng;
        let run = DkgRun::simulate(3, &mut rng);
        let salt = [0u8; 32];

        // The committed final deck: index 9 holds ciphertext `ct_committed`.
        let card_committed = 23u8;
        let ct_committed =
            Ct::encrypt_card(card_committed, &run.joint_key, &Scalar::random(&mut rng));
        // Build a 52-entry deck where index 9 is the committed ciphertext; the
        // rest are arbitrary valid ciphertexts (irrelevant to this test).
        let mut deck: Vec<Ct> = (0..DECK_SIZE as u8)
            .map(|id| Ct::encrypt_card(id, &run.joint_key, &Scalar::random(&mut rng)))
            .collect();
        deck[9] = ct_committed;

        let provider =
            RealThresholdDecryptionProvider::with_expected_deck(pubkeys(&run), deck.clone());

        // Helper: a fully valid n-of-n opening of an ARBITRARY ciphertext `ct`.
        let open_for = |ct: &Ct, idx: u32, rng: &mut OsRng| -> crate::crypto::DecryptionProof {
            let shares = run
                .parties
                .iter()
                .map(|p| partial_decrypt(p, idx, ct, rng))
                .collect();
            crate::crypto::DecryptionProof {
                scheme: SCHEME.to_string(),
                attestation: encode_threshold_attestation(&ThresholdOpenWire {
                    ct: ct.to_wire(),
                    threshold: ThresholdDecryptionProof {
                        scheme: SCHEME.to_string(),
                        shares,
                    },
                }),
            }
        };

        // (a) An honest open of the COMMITTED ciphertext at index 9 verifies.
        let good = open_for(&ct_committed, 9, &mut rng);
        assert!(
            provider.verify_decryption("party:0", 9, card_committed, &salt, &good),
            "the committed ciphertext's honest open must verify"
        );

        // (b) A DIFFERENT ciphertext (a fresh encryption of some card), opened
        //     fully validly with real n-of-n DLEQ shares, is REJECTED at index 9
        //     because its ciphertext ≠ deck[9]. This is the attack the anchor
        //     closes: a prover substituting a ciphertext it never committed to.
        let card_substituted = 41u8;
        let ct_substituted =
            Ct::encrypt_card(card_substituted, &run.joint_key, &Scalar::random(&mut rng));
        let substituted = open_for(&ct_substituted, 9, &mut rng);
        assert!(
            !provider.verify_decryption("party:0", 9, card_substituted, &salt, &substituted),
            "an open of a ciphertext other than deck[index] must be rejected (F3 anchor)"
        );

        // (c) Sanity: WITHOUT the anchor (plain `new`), the very same substituted
        //     open is (wrongly, by itself) accepted — proving the anchor is what
        //     rejects it, not some other check.
        let no_anchor = RealThresholdDecryptionProvider::new(pubkeys(&run));
        assert!(
            no_anchor.verify_decryption("party:0", 9, card_substituted, &salt, &substituted),
            "without the deck anchor a self-consistent open of any ciphertext passes \
             (this is exactly the gap the anchor closes)"
        );

        // (d) An out-of-range index with an anchor is rejected (no deck entry).
        let oob = open_for(&ct_committed, 99, &mut rng);
        assert!(
            !provider.verify_decryption("party:0", 99, card_committed, &salt, &oob),
            "an index past the committed deck must be rejected by the anchor"
        );
    }

    /// DLEQ round-trips and rejects cross-index replay (T6-style binding).
    #[test]
    fn dleq_round_trip_and_index_binding() {
        let mut rng = OsRng;
        let run = DkgRun::simulate(2, &mut rng);
        let r = Scalar::random(&mut rng);
        let ct = Ct::encrypt_card(3, &run.joint_key, &r);
        let p0 = &run.parties[0];
        let d0 = p0.x_i * ct.c1;
        let proof = dleq_prove(&p0.party_id, 10, &p0.x_i, &p0.q_i, &ct, &d0, &mut rng);
        // Verifies at the bound index against the bound ciphertext.
        assert!(dleq_verify(&p0.party_id, 10, &p0.q_i, &ct, &d0, &proof));
        // Lifting onto a DIFFERENT deck_index changes the challenge → rejects.
        assert!(!dleq_verify(&p0.party_id, 11, &p0.q_i, &ct, &d0, &proof));
        // A different party label rejects.
        assert!(!dleq_verify("party:1", 10, &p0.q_i, &ct, &d0, &proof));
        // F3: a proof transplanted onto a DIFFERENT ciphertext sharing the SAME
        // C1 but a different C2 squeezes a different challenge → rejects. (Forge
        // a sibling ciphertext with the same C1, a mutated C2.)
        let ct_diff_c2 = Ct {
            c1: ct.c1,
            c2: ct.c2 + G,
        };
        assert!(
            !dleq_verify(&p0.party_id, 10, &p0.q_i, &ct_diff_c2, &d0, &proof),
            "a DLEQ bound to (C1,C2) must not verify against a different C2 (F3)"
        );
    }

    /// KAT-5: a fixed DLEQ proof for fixed inputs is deterministic (merlin
    /// Fiat–Shamir determinism). The prover commitment `k` is random, so two
    /// proofs differ, but a FIXED proof verifies and the CHALLENGE is fixed by
    /// the (deterministic) transcript binding. We pin the challenge derivation.
    #[test]
    fn kat5_dleq_challenge_is_deterministic() {
        let q_i = card_point(1); // any fixed point
        let c1 = card_point(2);
        let c2 = card_point(7);
        let d_i = card_point(3);
        let a = card_point(4);
        let b = card_point(5);
        let chal_a = dleq_challenge("party:0", 7, &q_i, &c1, &c2, &d_i, &a, &b);
        let chal_b = dleq_challenge("party:0", 7, &q_i, &c1, &c2, &d_i, &a, &b);
        assert_eq!(chal_a, chal_b, "merlin challenge must be deterministic");
        // Any change in the bound statement changes the challenge.
        assert_ne!(
            chal_a,
            dleq_challenge("party:0", 8, &q_i, &c1, &c2, &d_i, &a, &b)
        );
        assert_ne!(
            chal_a,
            dleq_challenge("party:1", 7, &q_i, &c1, &c2, &d_i, &a, &b)
        );
        assert_ne!(
            chal_a,
            dleq_challenge("party:0", 7, &q_i, &c1, &c2, &d_i, &a, &card_point(6))
        );
        // F3: changing C2 (with everything else fixed) changes the challenge.
        assert_ne!(
            chal_a,
            dleq_challenge("party:0", 7, &q_i, &c1, &card_point(8), &d_i, &a, &b),
            "C2 must be bound into the DLEQ challenge (F3)"
        );
    }
}
