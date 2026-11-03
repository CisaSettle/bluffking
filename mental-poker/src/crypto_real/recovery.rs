//! Card-scoped recovery. Scalar evaluations NEVER leave the producer: holders
//! receive only f(j)*C1, encrypted to their registered key, with a DLEQ proof.
//! Feldman commitments bind every quorum to the same point. Own-hole backups
//! have an additional owner-only mask; publishing that mask changes secrecy to
//! the recovery threshold, even if the associated action is later rejected.

use super::{
    decrypt::{dleq_prove, dleq_verify, DleqProof},
    dkg::DkgParty,
    ec::{is_identity_pubkey, point_from_hex, point_to_hex, Ct, CtWire},
};
use chacha20poly1305::{
    aead::{Aead, KeyInit, Payload},
    ChaCha20Poly1305, Nonce,
};
use curve25519_dalek::{
    constants::RISTRETTO_BASEPOINT_POINT as G, ristretto::RistrettoPoint, scalar::Scalar,
};
use hkdf::Hkdf;
use rand_core::{CryptoRng, RngCore};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RecoveryKey {
    pub party_id: String,
    pub q_i: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RecoveryCore {
    /// Hash of the session, hand, committed deck and authenticated directory.
    pub binding: String,
    pub dealer: String,
    pub deck_index: u32,
    pub ct: CtWire,
    pub threshold: u8,
    pub keys: Vec<RecoveryKey>,
    pub coefficients: Vec<String>,
    /// Present ONLY for the producer's own two hole positions.
    pub mask_key: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RecoveryPacket {
    pub ephemeral: String,
    pub nonce: String,
    pub ciphertext: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RecoveryBundle {
    pub core: RecoveryCore,
    /// Dense recipient order, identical to core.keys.
    pub packets: Vec<RecoveryPacket>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RecoveryEvaluation {
    pub recipient: u8,
    pub point: String,
    pub proof: DleqProof,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RecoveryMask {
    pub point: String,
    pub proof: DleqProof,
}

/// A distinct proof, never represented as an original owner's DLEQ. The
/// application additionally pins the admitted bundle and release authorization.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RecoveryEvidence {
    pub core: RecoveryCore,
    pub evaluations: Vec<RecoveryEvaluation>,
    pub mask: Option<RecoveryMask>,
}

pub fn threshold(n: usize) -> Option<usize> {
    (3..=9).contains(&n).then(|| (2 * n).div_ceil(3))
}

pub fn digest<T: Serialize>(value: &T) -> Result<String, String> {
    let bytes =
        crate::hash::canonical_json(&serde_json::to_value(value).map_err(|_| "recovery_encoding")?);
    let mut hash = Sha256::new();
    hash.update(b"bluffking-card-recovery-v1\0");
    hash.update(bytes);
    Ok(hex::encode(hash.finalize()))
}

fn point(value: &str) -> Result<RistrettoPoint, String> {
    if value.len() != 64
        || value
            .bytes()
            .any(|c| !c.is_ascii_digit() && !(b'a'..=b'f').contains(&c))
    {
        return Err("recovery_point_encoding".into());
    }
    point_from_hex(value).ok_or_else(|| "recovery_point_encoding".into())
}

fn evaluation_key(coefficients: &[RistrettoPoint], recipient: usize) -> RistrettoPoint {
    let x = Scalar::from((recipient + 1) as u64);
    coefficients
        .iter()
        .rev()
        .fold(RistrettoPoint::default(), |sum, c| sum * x + c)
}

fn eval_id(core: &RecoveryCore, recipient: usize) -> Result<String, String> {
    Ok(format!(
        "recovery-evaluation-v1:{}:{recipient}",
        digest(core)?
    ))
}
fn mask_id(core: &RecoveryCore) -> Result<String, String> {
    Ok(format!("recovery-mask-v1:{}", digest(core)?))
}

pub fn validate_core(core: &RecoveryCore) -> Result<(), String> {
    let n = core.keys.len();
    let t = threshold(n).ok_or("recovery_party_count")?;
    if core.threshold as usize != t
        || core.coefficients.len() != t
        || core.binding.len() != 64
        || hex::decode(&core.binding).is_err()
    {
        return Err("recovery_parameters".into());
    }
    let ct = Ct::from_wire(&core.ct).ok_or("recovery_ciphertext")?;
    if is_identity_pubkey(&ct.c1) {
        return Err("recovery_ciphertext_identity".into());
    }
    let mut dealer = None;
    for (seat, key) in core.keys.iter().enumerate() {
        if key.party_id != format!("party:{seat}") || is_identity_pubkey(&point(&key.q_i)?) {
            return Err("recovery_directory".into());
        }
        if key.party_id == core.dealer {
            dealer = Some(seat);
        }
    }
    let dealer = dealer.ok_or("recovery_dealer")?;
    let index = core.deck_index as usize;
    let is_hole = index == dealer || index == dealer + n;
    if core.mask_key.is_some() != is_hole || (!is_hole && !(2 * n..2 * n + 5).contains(&index)) {
        return Err("recovery_card_purpose".into());
    }
    let mask = match &core.mask_key {
        Some(k) => {
            let p = point(k)?;
            if is_identity_pubkey(&p) {
                return Err("recovery_mask_identity".into());
            }
            p
        }
        None => RistrettoPoint::default(),
    };
    let coefficients: Vec<_> = core
        .coefficients
        .iter()
        .map(|v| point(v))
        .collect::<Result<_, _>>()?;
    if coefficients[0] != point(&core.keys[dealer].q_i)? + mask
        || is_identity_pubkey(&coefficients[0])
        || (0..n).any(|j| is_identity_pubkey(&evaluation_key(&coefficients, j)))
    {
        return Err("recovery_commitment".into());
    }
    Ok(())
}

pub fn validate_bundle(bundle: &RecoveryBundle) -> Result<(), String> {
    validate_core(&bundle.core)?;
    if bundle.packets.len() != bundle.core.keys.len() {
        return Err("recovery_packets".into());
    }
    for packet in &bundle.packets {
        if is_identity_pubkey(&point(&packet.ephemeral)?)
            || packet.nonce.len() != 24
            || hex::decode(&packet.nonce).is_err()
            || packet.ciphertext.len() > 2048
            || packet.ciphertext.len() < 32
            || hex::decode(&packet.ciphertext).is_err()
        {
            return Err("recovery_packet_encoding".into());
        }
    }
    Ok(())
}

fn packet_key(
    shared: RistrettoPoint,
    core: &RecoveryCore,
    recipient: usize,
    ephemeral: &str,
) -> Result<([u8; 32], Vec<u8>), String> {
    let aad = format!("{}:{recipient}:{ephemeral}", digest(core)?).into_bytes();
    let hkdf = Hkdf::<Sha256>::new(
        Some(b"bluffking-recovery-aead-v1"),
        shared.compress().as_bytes(),
    );
    let mut key = [0; 32];
    hkdf.expand(&aad, &mut key).map_err(|_| "recovery_kdf")?;
    Ok((key, aad))
}

/// Produce one card's backup. The returned mask stays exclusively on the
/// owner's device until it authorizes all-in or a genuine showdown.
pub fn create<R: RngCore + CryptoRng>(
    party: &DkgParty,
    binding: String,
    index: u32,
    ct: &Ct,
    keys: Vec<RecoveryKey>,
    masked: bool,
    rng: &mut R,
) -> Result<(RecoveryBundle, Option<RecoveryMask>), String> {
    let t = threshold(keys.len()).ok_or("recovery_party_count")?;
    for _ in 0..8 {
        let mask = if masked {
            Scalar::random(rng)
        } else {
            Scalar::ZERO
        };
        let mut coefficients = vec![party.x_i + mask];
        coefficients.extend((1..t).map(|_| Scalar::random(rng)));
        let core = RecoveryCore {
            binding: binding.clone(),
            dealer: party.party_id.clone(),
            deck_index: index,
            ct: ct.to_wire(),
            threshold: t as u8,
            keys: keys.clone(),
            coefficients: coefficients
                .iter()
                .map(|a| point_to_hex(&(a * G)))
                .collect(),
            mask_key: masked.then(|| point_to_hex(&(mask * G))),
        };
        if validate_core(&core).is_err() {
            continue;
        }
        let mut packets = Vec::new();
        for (recipient, key) in keys.iter().enumerate() {
            let x = Scalar::from((recipient + 1) as u64);
            let value = coefficients
                .iter()
                .rev()
                .fold(Scalar::ZERO, |sum, c| sum * x + c);
            let evaluated = value * ct.c1;
            let proof = dleq_prove(
                &eval_id(&core, recipient)?,
                index,
                &value,
                &(value * G),
                ct,
                &evaluated,
                rng,
            );
            let evaluation = RecoveryEvaluation {
                recipient: recipient as u8,
                point: point_to_hex(&evaluated),
                proof,
            };
            let secret = Scalar::random(rng);
            if secret == Scalar::ZERO {
                return Err("recovery_random_identity".into());
            }
            let ephemeral = point_to_hex(&(secret * G));
            let (key, aad) = packet_key(secret * point(&key.q_i)?, &core, recipient, &ephemeral)?;
            let mut nonce = [0; 12];
            rng.fill_bytes(&mut nonce);
            let bytes = serde_json::to_vec(&evaluation).map_err(|_| "recovery_encoding")?;
            let encrypted = ChaCha20Poly1305::new((&key).into())
                .encrypt(
                    Nonce::from_slice(&nonce),
                    Payload {
                        msg: &bytes,
                        aad: &aad,
                    },
                )
                .map_err(|_| "recovery_encrypt")?;
            packets.push(RecoveryPacket {
                ephemeral,
                nonce: hex::encode(nonce),
                ciphertext: hex::encode(encrypted),
            });
        }
        let release = if masked {
            let p = mask * ct.c1;
            Some(RecoveryMask {
                point: point_to_hex(&p),
                proof: dleq_prove(&mask_id(&core)?, index, &mask, &(mask * G), ct, &p, rng),
            })
        } else {
            None
        };
        return Ok((RecoveryBundle { core, packets }, release));
    }
    Err("recovery_invalid_parameters".into())
}

pub fn verify_evaluation(
    core: &RecoveryCore,
    evaluation: &RecoveryEvaluation,
) -> Result<(), String> {
    validate_core(core)?;
    let recipient = evaluation.recipient as usize;
    if recipient >= core.keys.len() {
        return Err("recovery_recipient".into());
    }
    let coefficients: Vec<_> = core
        .coefficients
        .iter()
        .map(|c| point(c))
        .collect::<Result<_, _>>()?;
    let ct = Ct::from_wire(&core.ct).ok_or("recovery_ciphertext")?;
    if !dleq_verify(
        &eval_id(core, recipient)?,
        core.deck_index,
        &evaluation_key(&coefficients, recipient),
        &ct,
        &point(&evaluation.point)?,
        &evaluation.proof,
    ) {
        return Err("recovery_evaluation_proof".into());
    }
    Ok(())
}

pub fn receive(party: &DkgParty, bundle: &RecoveryBundle) -> Result<RecoveryEvaluation, String> {
    validate_bundle(bundle)?;
    let recipient = bundle
        .core
        .keys
        .iter()
        .position(|k| k.party_id == party.party_id && k.q_i == point_to_hex(&party.q_i))
        .ok_or("recovery_recipient")?;
    let packet = &bundle.packets[recipient];
    let (key, aad) = packet_key(
        party.x_i * point(&packet.ephemeral)?,
        &bundle.core,
        recipient,
        &packet.ephemeral,
    )?;
    let nonce = hex::decode(&packet.nonce).map_err(|_| "recovery_nonce")?;
    let ciphertext = hex::decode(&packet.ciphertext).map_err(|_| "recovery_ciphertext")?;
    let bytes = ChaCha20Poly1305::new((&key).into())
        .decrypt(
            Nonce::from_slice(&nonce),
            Payload {
                msg: &ciphertext,
                aad: &aad,
            },
        )
        .map_err(|_| "recovery_decrypt")?;
    let evaluation: RecoveryEvaluation =
        serde_json::from_slice(&bytes).map_err(|_| "recovery_payload")?;
    if evaluation.recipient as usize != recipient {
        return Err("recovery_recipient".into());
    }
    verify_evaluation(&bundle.core, &evaluation)?;
    Ok(evaluation)
}

pub fn verify_mask(core: &RecoveryCore, mask: &RecoveryMask) -> Result<(), String> {
    validate_core(core)?;
    let key = point(core.mask_key.as_ref().ok_or("recovery_unmasked")?)?;
    let ct = Ct::from_wire(&core.ct).ok_or("recovery_ciphertext")?;
    if !dleq_verify(
        &mask_id(core)?,
        core.deck_index,
        &key,
        &ct,
        &point(&mask.point)?,
        &mask.proof,
    ) {
        return Err("recovery_mask_proof".into());
    }
    Ok(())
}

pub fn reconstruct(evidence: &RecoveryEvidence) -> Result<RistrettoPoint, String> {
    let core = &evidence.core;
    validate_core(core)?;
    let count = evidence.evaluations.len();
    if count < core.threshold as usize || count > core.keys.len() {
        return Err("recovery_quorum".into());
    }
    let mut seen = [false; 9];
    for evaluation in &evidence.evaluations {
        verify_evaluation(core, evaluation)?;
        let seat = evaluation.recipient as usize;
        if seen[seat] {
            return Err("recovery_duplicate".into());
        }
        seen[seat] = true;
    }
    let mut recovered = RistrettoPoint::default();
    for evaluation in &evidence.evaluations {
        let x = Scalar::from(u64::from(evaluation.recipient) + 1);
        let mut weight = Scalar::ONE;
        for other in &evidence.evaluations {
            if other.recipient != evaluation.recipient {
                let y = Scalar::from(u64::from(other.recipient) + 1);
                weight *= y * (y - x).invert();
            }
        }
        recovered += weight * point(&evaluation.point)?;
    }
    match (&core.mask_key, &evidence.mask) {
        (None, None) => {}
        (Some(_), Some(mask)) => {
            verify_mask(core, mask)?;
            recovered -= point(&mask.point)?;
        }
        _ => return Err("recovery_mask_required".into()),
    }
    Ok(recovered)
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RecoveryActivation {
    pub bundle_id: String,
    pub mask: RecoveryMask,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "stage", rename_all = "snake_case")]
pub enum RecoveryClientMessage {
    Bundle {
        bundle: RecoveryBundle,
        signature: String,
        initial_mask: Option<RecoveryMask>,
    },
    Ready {
        manifest: String,
        signature: String,
    },
    Release {
        bundle_id: String,
        evaluation: RecoveryEvaluation,
    },
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "stage", rename_all = "snake_case")]
pub enum RecoveryServerMessage {
    Prepare {
        binding: String,
    },
    Bundle {
        bundle: Box<RecoveryBundle>,
        signature: String,
        initial_mask: Option<RecoveryMask>,
    },
    Ready {
        manifest: String,
    },
    Request {
        bundle_ids: Vec<String>,
    },
}
