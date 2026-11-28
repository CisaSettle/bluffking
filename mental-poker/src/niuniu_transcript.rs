//! Offline verification of Niuniu's player-shuffled deck and dealt positions.
use std::collections::HashSet;

use serde::{Deserialize, Serialize};
use uuid::Uuid;

use crate::crypto_real::{
    decrypt::{verify_and_open, DecryptionShare, ThresholdDecryptionProof, SCHEME as OPEN_SCHEME},
    dkg::{schnorr_verify, schnorr_verify_bound, SchnorrPok},
    ec::{
        canonical_starting_deck, deck_hash, is_identity_pubkey, point_from_hex, point_to_hex, Ct,
    },
    shuffle::{output_deck_from_proof, verify_shuffle, ShuffleProof},
};

pub const SCHEME: &str = "niuniu-encrypted-deal-v1";
pub const CARDS_PER_SEAT: usize = 5;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Registration {
    pub party_id: String,
    pub q_i: String,
    pub pok: SchnorrPok,
}

#[derive(Serialize, Deserialize)]
pub struct Bundle {
    pub scheme: String,
    pub hand_id: Uuid,
    pub seats: Vec<usize>,
    pub registry: Vec<Registration>,
    pub shuffles: Vec<ShuffleProof>,
    pub final_hash: String,
    pub shuffle_signatures: Vec<SchnorrPok>,
    pub ack_signatures: Vec<SchnorrPok>,
    /// One row per party, in dealt-position order. Only direct shares are accepted.
    pub shares: Vec<Vec<DecryptionShare>>,
    pub cards: Vec<u8>,
}

pub fn party_id(hand_id: Uuid, seat: usize) -> String {
    format!("niuniu:{hand_id}:{seat}")
}

pub fn shuffle_message(round: usize, proof: &ShuffleProof) -> String {
    format!(
        "niuniu-shuffle-v1:{round}:{}:{}",
        proof.input_deck_hash, proof.output_deck_hash
    )
}

pub fn ack_message(hash: &str) -> String {
    format!("niuniu-final-deck-v1:{hash}")
}

fn hash_bytes(value: &str) -> Option<[u8; 32]> {
    let bytes = hex::decode(value).ok()?;
    bytes.try_into().ok()
}

pub fn verify(bundle: &Bundle) -> Result<(), &'static str> {
    let n = bundle.seats.len();
    let count = n * CARDS_PER_SEAT;
    if bundle.scheme != SCHEME
        || !(2..=6).contains(&n)
        || bundle.seats.iter().any(|&seat| seat >= 6)
        || bundle.seats.windows(2).any(|seats| seats[0] >= seats[1])
        || bundle.registry.len() != n
        || bundle.shuffles.len() != n
        || bundle.shuffle_signatures.len() != n
        || bundle.ack_signatures.len() != n
        || bundle.shares.len() != n
        || bundle.shares.iter().any(|row| row.len() != count)
        || bundle.cards.len() != count
        || bundle.cards.iter().copied().collect::<HashSet<_>>().len() != count
    {
        return Err("invalid Niuniu transcript layout");
    }
    let mut keys = Vec::with_capacity(n);
    let mut unique_keys = HashSet::with_capacity(n);
    for (index, reg) in bundle.registry.iter().enumerate() {
        let q = point_from_hex(&reg.q_i).ok_or("invalid Niuniu key")?;
        if reg.party_id != party_id(bundle.hand_id, bundle.seats[index])
            || is_identity_pubkey(&q)
            || point_to_hex(&q) != reg.q_i
            || !unique_keys.insert(&reg.q_i)
            || !schnorr_verify(&reg.party_id, &q, &reg.pok)
        {
            return Err("invalid Niuniu key proof");
        }
        keys.push((reg.party_id.clone(), q));
    }
    let joint = keys.iter().map(|(_, point)| point).sum();
    if is_identity_pubkey(&joint) {
        return Err("identity Niuniu joint key");
    }
    let joint_key = point_to_hex(&joint);
    let mut digest = deck_hash(&canonical_starting_deck());
    for (round, proof) in bundle.shuffles.iter().enumerate() {
        let next = hash_bytes(&proof.output_deck_hash).ok_or("invalid Niuniu shuffle hash")?;
        let (id, q) = &keys[round];
        if !schnorr_verify_bound(
            id,
            q,
            shuffle_message(round, proof).as_bytes(),
            &bundle.shuffle_signatures[round],
        ) || !verify_shuffle(
            &keys[round].0,
            round as u32,
            &digest,
            &next,
            &joint_key,
            proof,
        ) {
            return Err("invalid Niuniu shuffle");
        }
        digest = next;
    }
    let ack = ack_message(&bundle.final_hash);
    if keys
        .iter()
        .zip(&bundle.ack_signatures)
        .any(|((id, q), sig)| !schnorr_verify_bound(id, q, ack.as_bytes(), sig))
    {
        return Err("invalid Niuniu final deck acknowledgement");
    }
    let deck: Vec<Ct> = bundle
        .shuffles
        .last()
        .and_then(output_deck_from_proof)
        .ok_or("invalid Niuniu final deck")?;
    if deck.len() != 52
        || hex::encode(digest) != bundle.final_hash
        || deck_hash(&deck) != digest
        || !crate::blind_transcript::independently_openable(&deck)
    {
        return Err("invalid Niuniu final deck");
    }
    for (index, ct) in deck.iter().take(count).enumerate() {
        let shares = bundle
            .shares
            .iter()
            .zip(&keys)
            .map(|(row, (id, _))| {
                let share = &row[index];
                (share.party_id == *id && share.recovery.is_none()).then(|| share.clone())
            })
            .collect::<Option<Vec<_>>>()
            .ok_or("invalid Niuniu share set")?;
        let proof = ThresholdDecryptionProof {
            scheme: OPEN_SCHEME.into(),
            shares,
        };
        if verify_and_open(index as u32, ct, &keys, &proof).ok() != Some(bundle.cards[index]) {
            return Err("invalid Niuniu card opening");
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::crypto_real::{
        decrypt::partial_decrypt,
        dkg::{schnorr_prove, schnorr_sign_bound, DkgParty},
        shuffle::Shuffle,
    };
    use rand::{rngs::StdRng, SeedableRng};

    #[test]
    fn directly_checks_each_device_shuffle_and_card_position() {
        let mut rng = StdRng::seed_from_u64(819);
        let hand_id = Uuid::from_u128(17);
        let parties = [2, 5].map(|seat| DkgParty::generate(party_id(hand_id, seat), &mut rng));
        let joint = parties.iter().map(|party| party.q_i).sum();
        let registry = parties
            .iter()
            .map(|party| Registration {
                party_id: party.party_id.clone(),
                q_i: point_to_hex(&party.q_i),
                pok: schnorr_prove(&party.party_id, &party.x_i, &party.q_i, &mut rng),
            })
            .collect();
        let mut deck = canonical_starting_deck();
        let mut shuffles = Vec::new();
        let mut shuffle_signatures = Vec::new();
        for (round, party) in parties.iter().enumerate() {
            let shuffle = Shuffle::perform(deck, &joint, &mut rng);
            let proof = shuffle.prove(&party.party_id, round as u32, &mut rng);
            shuffle_signatures.push(schnorr_sign_bound(
                &party.party_id,
                &party.x_i,
                &party.q_i,
                shuffle_message(round, &proof).as_bytes(),
                &mut rng,
            ));
            shuffles.push(proof);
            deck = shuffle.output;
        }
        let dealt = &deck[..10];
        let shares: Vec<Vec<_>> = parties
            .iter()
            .map(|party| {
                dealt
                    .iter()
                    .enumerate()
                    .map(|(index, ct)| partial_decrypt(party, index as u32, ct, &mut rng))
                    .collect()
            })
            .collect();
        let cards = dealt
            .iter()
            .enumerate()
            .map(|(index, ct)| {
                let opens: Vec<_> = shares
                    .iter()
                    .map(|row| point_from_hex(&row[index].d_i).unwrap())
                    .collect();
                crate::crypto_real::decrypt::combine(ct, &opens).unwrap()
            })
            .collect();
        let final_hash = hex::encode(deck_hash(&deck));
        let ack = ack_message(&final_hash);
        let ack_signatures = parties
            .iter()
            .map(|party| {
                schnorr_sign_bound(
                    &party.party_id,
                    &party.x_i,
                    &party.q_i,
                    ack.as_bytes(),
                    &mut rng,
                )
            })
            .collect();
        let mut bundle = Bundle {
            scheme: SCHEME.into(),
            hand_id,
            seats: vec![2, 5],
            registry,
            shuffles,
            final_hash,
            shuffle_signatures,
            ack_signatures,
            shares,
            cards,
        };
        assert_eq!(verify(&bundle), Ok(()));
        bundle.hand_id = Uuid::from_u128(18);
        assert_eq!(verify(&bundle), Err("invalid Niuniu key proof"));
        bundle.hand_id = hand_id;
        bundle.ack_signatures.swap(0, 1);
        assert_eq!(
            verify(&bundle),
            Err("invalid Niuniu final deck acknowledgement")
        );
        bundle.ack_signatures.swap(0, 1);
        bundle.cards.swap(0, 5);
        assert_eq!(verify(&bundle), Err("invalid Niuniu card opening"));
    }
}
