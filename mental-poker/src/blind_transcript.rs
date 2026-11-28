//! Offline verification of the player-shuffled encrypted deck and public
//! openings. The transcript contains no private hole relay or recovery packet.

use std::collections::{BTreeMap, HashSet};

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use uuid::Uuid;

use crate::{
    card_id::{self, PokerDeck},
    crypto_real::{
        decrypt::{verify_and_open, ThresholdDecryptionProof},
        dkg::{schnorr_verify, schnorr_verify_bound, SchnorrPok},
        ec::{
            canonical_poker_deck, deck_hash, is_identity_pubkey, point_from_hex, point_to_hex, Ct,
            CtWire,
        },
        recovery,
        shuffle::{verify_shuffle, ShuffleProof},
        Ed25519SignatureProvider,
    },
    events::party_id,
    hash::{canonical_json, hex_hash, Hash},
};

pub const SCHEME: &str = "mp-blind-transcript-v1";
pub const SHOWDOWN_POLICY: &str = "reveal_if_terminal_non_folded";

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Party {
    /// Dense seat identifier, independent of table seat and account identity.
    pub party_id: String,
    pub q_i: String,
    pub key_pok: SchnorrPok,
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub ed25519_vk: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub ed25519_vk_binding: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub vk_dlog_binding: Option<SchnorrPok>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ShowdownCommit {
    pub seat: u8,
    pub party_id: String,
    pub signature: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Opening {
    pub deck_index: u32,
    pub card_id: u8,
    pub proof: ThresholdDecryptionProof,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Bundle {
    pub scheme: String,
    pub session_id: Uuid,
    pub hand_id: Uuid,
    pub variant: PokerDeck,
    pub parties: Vec<Party>,
    pub showdown_commits: Vec<ShowdownCommit>,
    /// One encrypted shuffle from each party, in dense seat order.
    pub shuffles: Vec<ShuffleProof>,
    pub final_deck_hash: String,
    pub deck_acknowledgments: Vec<String>,
    pub final_deck: Vec<CtWire>,
    pub board: Vec<Opening>,
    /// Only holdings whose opening was accepted for public reveal.
    pub shown_holes: BTreeMap<u8, [Opening; 2]>,
}

#[derive(Debug, PartialEq, Eq, Serialize)]
pub struct Verified {
    pub hand_id: Uuid,
    pub final_deck_hash: String,
    pub party_fingerprint: String,
    pub board: Vec<u8>,
    pub shown_holes: BTreeMap<u8, [u8; 2]>,
}

pub fn sigkey_bind_claim(
    session_id: &str,
    hand_id: &str,
    party_id: &str,
    q_i: &str,
    ed25519_vk: &str,
) -> Vec<u8> {
    canonical_json(&serde_json::json!({
        "v": "mp:sigkey-bind:v2",
        "session_id": session_id,
        "hand_id": hand_id,
        "party_id": party_id,
        "q_i": q_i,
        "ed25519_vk": ed25519_vk,
    }))
}

pub fn showdown_claim(
    session_id: &str,
    hand_id: &str,
    party_set_hash: &str,
    seat: u8,
    party_id: &str,
) -> Vec<u8> {
    canonical_json(&serde_json::json!({
        "v": "mp:showdown-precommit:v1",
        "session_id": session_id,
        "hand_id": hand_id,
        "party_set_hash": party_set_hash,
        "seat": seat,
        "party_id": party_id,
        "policy": SHOWDOWN_POLICY,
    }))
}

pub fn deck_ack_claim(
    session_id: &str,
    hand_id: &str,
    party_id: &str,
    final_deck_hash: &str,
) -> Vec<u8> {
    canonical_json(&serde_json::json!({
        "v": "mp:deck-ack:v1",
        "session_id": session_id,
        "hand_id": hand_id,
        "party_id": party_id,
        "final_deck_hash": final_deck_hash,
    }))
}

/// An opening must not disclose another card via a shared ElGamal nonce.
pub fn independently_openable(deck: &[Ct]) -> bool {
    let mut nonces = HashSet::with_capacity(deck.len());
    deck.iter()
        .all(|card| !is_identity_pubkey(&card.c1) && nonces.insert(card.c1.compress().to_bytes()))
}

pub fn verify(bundle: &Bundle) -> Result<Verified, &'static str> {
    let n = bundle.parties.len();
    if bundle.scheme != SCHEME
        || !(2..=9).contains(&n)
        || bundle.showdown_commits.len() != n
        || bundle.shuffles.len() != n
        || bundle.deck_acknowledgments.len() != n
        || bundle.final_deck.len() != card_id::deck_size(bundle.variant)
        || !matches!(bundle.board.len(), 0 | 3 | 4 | 5)
        || bundle.shown_holes.len() > n
    {
        return Err("invalid transcript layout");
    }
    let session_id = bundle.session_id.to_string();
    let hand_id = bundle.hand_id.to_string();
    let party_set = bundle
        .parties
        .iter()
        .map(|p| format!("{}|{}", p.party_id, p.q_i))
        .collect::<Vec<_>>()
        .join("\n");
    let party_set_hash = hex::encode(Sha256::digest(party_set.as_bytes()));
    let mut pubkeys = Vec::with_capacity(n);
    for (seat, (party, commit)) in bundle
        .parties
        .iter()
        .zip(&bundle.showdown_commits)
        .enumerate()
    {
        let pid = party_id(seat as u8);
        if party.party_id != pid || commit.party_id != pid || usize::from(commit.seat) != seat {
            return Err("invalid party order");
        }
        let q = point_from_hex(&party.q_i).ok_or("invalid DKG key")?;
        if is_identity_pubkey(&q)
            || point_to_hex(&q) != party.q_i
            || !schnorr_verify(&pid, &q, &party.key_pok)
        {
            return Err("invalid DKG proof");
        }
        let vk_hex = party.ed25519_vk.as_deref().ok_or("missing signing key")?;
        let vk = parse_hash(vk_hex).ok_or("invalid signing key")?;
        let binding = party
            .ed25519_vk_binding
            .as_ref()
            .ok_or("missing signing key binding")?;
        let dlog_binding = party
            .vk_dlog_binding
            .as_ref()
            .ok_or("missing DKG binding")?;
        let signatures = Ed25519SignatureProvider::new()
            .with_verifier(pid.clone(), &vk)
            .ok_or("invalid verification key")?;
        let claim = sigkey_bind_claim(&session_id, &hand_id, &pid, &party.q_i, vk_hex);
        if !signatures.verify(&pid, &claim, binding)
            || !schnorr_verify_bound(&pid, &q, &claim, dlog_binding)
        {
            return Err("invalid signing key binding");
        }
        let claim = showdown_claim(&session_id, &hand_id, &party_set_hash, seat as u8, &pid);
        if !signatures.verify(&pid, &claim, &commit.signature) {
            return Err("invalid showdown commitment");
        }
        let claim = deck_ack_claim(&session_id, &hand_id, &pid, &bundle.final_deck_hash);
        if !signatures.verify(&pid, &claim, &bundle.deck_acknowledgments[seat]) {
            return Err("invalid final deck acknowledgment");
        }
        pubkeys.push((pid, q));
    }
    let joint_key = point_to_hex(
        &pubkeys
            .iter()
            .map(|(_, q)| q)
            .sum::<curve25519_dalek::ristretto::RistrettoPoint>(),
    );
    let mut digest = deck_hash(&canonical_poker_deck(bundle.variant));
    for (round, proof) in bundle.shuffles.iter().enumerate() {
        let next = parse_hash(&proof.output_deck_hash).ok_or("invalid shuffle hash")?;
        if !verify_shuffle(
            &party_id(round as u8),
            round as u32,
            &digest,
            &next,
            &joint_key,
            proof,
        ) {
            return Err("invalid encrypted shuffle");
        }
        digest = next;
    }
    let deck = bundle
        .final_deck
        .iter()
        .map(Ct::from_wire)
        .collect::<Option<Vec<_>>>()
        .ok_or("invalid encrypted deck")?;
    if hex_hash(&digest) != bundle.final_deck_hash
        || deck_hash(&deck) != digest
        || !independently_openable(&deck)
    {
        return Err("invalid final encrypted deck");
    }
    let recovery_binding = recovery::hand_binding(
        session_id.as_str(),
        hand_id.as_str(),
        bundle.final_deck_hash.as_str(),
        party_set_hash.as_str(),
    )
    .map_err(|_| "invalid recovery binding")?;
    let mut opened = HashSet::new();
    let mut check_open = |opening: &Opening, expected_index: usize| {
        let index = opening.deck_index;
        if index as usize != expected_index {
            return Err("unexpected opening position");
        }
        if opening
            .proof
            .shares
            .iter()
            .any(|share| !share.matches_recovery_binding(&recovery_binding))
        {
            return Err("invalid recovery binding");
        }
        let card = verify_and_open(index, &deck[expected_index], &pubkeys, &opening.proof)
            .map_err(|_| "invalid card opening")?;
        if card != opening.card_id
            || !card_id::contains(bundle.variant, card)
            || !opened.insert(card)
        {
            return Err("invalid opened card");
        }
        Ok(card)
    };
    let board = bundle
        .board
        .iter()
        .enumerate()
        .map(|(offset, opening)| check_open(opening, 2 * n + offset))
        .collect::<Result<Vec<_>, _>>()?;
    let mut shown_holes = BTreeMap::new();
    for (&seat, [first, second]) in &bundle.shown_holes {
        if usize::from(seat) >= n {
            return Err("invalid revealed seat");
        }
        shown_holes.insert(
            seat,
            [
                check_open(first, usize::from(seat))?,
                check_open(second, usize::from(seat) + n)?,
            ],
        );
    }
    Ok(Verified {
        hand_id: bundle.hand_id,
        final_deck_hash: bundle.final_deck_hash.clone(),
        party_fingerprint: party_set_hash,
        board,
        shown_holes,
    })
}

fn parse_hash(hex: &str) -> Option<Hash> {
    let mut bytes = [0; 32];
    hex::decode_to_slice(hex, &mut bytes).ok()?;
    Some(bytes)
}
