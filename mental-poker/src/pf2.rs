//! Verifiable Hold'em dealing v2: an encrypted, verifiably shuffled deck whose
//! final order the players' seeds decide, opened card by card with decryption
//! proofs. Replaces publishing the server seed (`pf.rs`, v1), which let every
//! participant recompute folded and undealt cards.
//!
//! Protocol (one hand, one dealer key):
//!
//! 1. **Lock.** From a 32-byte `master_seed` the dealer derives a one-time key
//!    `x` (`Q = x·G`) and a secret shuffle of the public standard deck
//!    [`canonical_starting_deck`] (trivial encryptions of card ids `0..52`),
//!    re-encrypting every card under `Q`. Before any player seed is disclosed it
//!    publishes `commit = ds_hash("pf2:commit:v2", [hand_id, Q, deck_hash(D)])`.
//! 2. **Order.** The players' seeds (two-round exchange, unchanged) and the
//!    commitment derive a public permutation `σ`; the final deck is
//!    `F[k] = D[σ[k]]`. The dealer fixed `D` before it could know `σ`.
//! 3. **Open.** On request the dealer proves `D` is a permutation of the
//!    standard deck (re-encryption shuffle proof against the pinned canonical
//!    input and the committed `Q`) and opens chosen positions of `F` with DLEQ
//!    decryption proofs. Unopened positions stay ciphertexts.
//!
//! Only the deck is deterministic in `master_seed` (so the server can rebuild
//! it from the stored seed). Every proof draws fresh randomness from the caller
//! (production: `OsRng`); proof nonces are never derived from the seed, so a
//! nonce is never reused across two different statements.
//!
//! Deal positions are unchanged from v1: with `n` dealt seats in ascending order
//! (`dealt_seats[p] = seat`), deal position `p` holds `F[p]` and `F[n+p]`; the
//! board is `F[2n..2n+5]`; no burns.

use std::collections::BTreeMap;

use curve25519_dalek::constants::RISTRETTO_BASEPOINT_POINT;
use curve25519_dalek::ristretto::RistrettoPoint;
use curve25519_dalek::scalar::Scalar;
use engine::{Deck, PokerRng};
use rand_chacha::rand_core::SeedableRng;
use rand_chacha::ChaCha20Rng;
use rand_core::{CryptoRng, RngCore};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

use crate::card_id::card_to_id;
use crate::crypto::{ShuffleProof, ShuffleProofProvider};
use crate::crypto_real::decrypt::{
    partial_decrypt, verify_and_open, ThresholdDecryptionProof, SCHEME as DECRYPT_SCHEME,
};
use crate::crypto_real::dkg::DkgParty;
use crate::crypto_real::ec::{
    canonical_starting_deck, deck_hash, point_from_hex, point_to_bytes, point_to_hex, Ct,
    DECK_SIZE,
};
use crate::crypto_real::shuffle::{output_deck_from_proof, RealShuffleProofProvider, Shuffle};
use crate::hash::ds_hash;

/// Bundle scheme identifier (wire `scheme`).
pub const SCHEME: &str = "pf-shuffle-v2";
/// Version byte prefixed to the master seed in `hands.server_seed`
/// (33 bytes = `0x02 ‖ master_seed`; legacy v1 rows hold a bare 32-byte seed).
pub const SEED_VERSION: u8 = 2;
/// Party id bound into the shuffle proof and every DLEQ challenge.
pub const DEALER_PARTY: &str = "pf2:dealer";
/// Shuffle round bound into the shuffle proof transcript.
const SHUFFLE_ROUND: u32 = 0;

const DS_KEY: &str = "pf2:key:v2";
const DS_SHUFFLE: &str = "pf2:shuffle:v2";
const DS_COMMIT: &str = "pf2:commit:v2";
const DS_ORDER: &str = "pf2:order:v2";
const DS_ENGINE_SEED: &str = "pf2:engine-seed:v2";

/// Encode a master seed for `hands.server_seed`.
pub fn encode_stored_seed(master_seed: &[u8; 32]) -> Vec<u8> {
    let mut out = Vec::with_capacity(33);
    out.push(SEED_VERSION);
    out.extend_from_slice(master_seed);
    out
}

/// Decode a `hands.server_seed` value. `None` for a legacy v1 (32-byte) or
/// malformed value.
pub fn decode_stored_seed(stored: &[u8]) -> Option<[u8; 32]> {
    match stored {
        [SEED_VERSION, rest @ ..] if rest.len() == 32 => rest.try_into().ok(),
        _ => None,
    }
}

/// The dealer's locked deck for one hand. Holds the secret key and shuffle
/// witness; never serialize it.
pub struct LockedDeck {
    hand_id: Uuid,
    key: Scalar,
    public_key: RistrettoPoint,
    shuffle: Shuffle,
    commit: [u8; 32],
}

impl std::fmt::Debug for LockedDeck {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("LockedDeck")
            .field("hand_id", &self.hand_id)
            .field("commit", &hex::encode(self.commit))
            .finish_non_exhaustive()
    }
}

fn seeded_rng(domain: &str, master_seed: &[u8; 32], hand_id: Uuid) -> ChaCha20Rng {
    ChaCha20Rng::from_seed(ds_hash(domain, &[master_seed, hand_id.as_bytes()]))
}

fn commit_of(hand_id: Uuid, public_key: &RistrettoPoint, deck: &[Ct]) -> [u8; 32] {
    ds_hash(
        DS_COMMIT,
        &[
            hand_id.as_bytes(),
            &point_to_bytes(public_key),
            &deck_hash(deck),
        ],
    )
}

impl LockedDeck {
    /// Rebuild the locked deck from its master seed. Deterministic: the same
    /// `(master_seed, hand_id)` always yields the same key, deck and commit.
    pub fn new(master_seed: &[u8; 32], hand_id: Uuid) -> Self {
        let key = Scalar::random(&mut seeded_rng(DS_KEY, master_seed, hand_id));
        let public_key = key * RISTRETTO_BASEPOINT_POINT;
        let shuffle = Shuffle::perform(
            canonical_starting_deck(),
            &public_key,
            &mut seeded_rng(DS_SHUFFLE, master_seed, hand_id),
        );
        let commit = commit_of(hand_id, &public_key, &shuffle.output);
        Self {
            hand_id,
            key,
            public_key,
            shuffle,
            commit,
        }
    }

    /// The commitment published before any player seed is disclosed.
    pub fn commit(&self) -> [u8; 32] {
        self.commit
    }

    /// Card ids of the final deck in deal order (`F[k]` plaintexts).
    pub fn final_card_ids(&self, client_seeds: &BTreeMap<u8, [u8; 32]>) -> Vec<u8> {
        public_order(&self.commit, client_seeds, self.hand_id)
            .into_iter()
            .map(|j| self.shuffle.pi[j] as u8)
            .collect()
    }

    /// Build the verification bundle opening `positions` of the final deck.
    /// `rng` supplies fresh proof randomness (production: `OsRng`).
    pub fn bundle<R: RngCore + CryptoRng>(
        &self,
        client_seeds: &BTreeMap<u8, [u8; 32]>,
        dealt_seats: &[u8],
        withheld_seats: &[u8],
        positions: &[usize],
        rng: &mut R,
    ) -> Result<Bundle, Pf2Error> {
        let order = public_order(&self.commit, client_seeds, self.hand_id);
        let party = DkgParty {
            party_id: DEALER_PARTY.to_string(),
            x_i: self.key,
            blind: Scalar::ZERO,
            q_i: self.public_key,
        };
        let mut positions = positions.to_vec();
        positions.sort_unstable();
        positions.dedup();
        let mut openings = Vec::with_capacity(positions.len());
        for position in positions {
            let index = *order.get(position).ok_or(Pf2Error::Position)?;
            let ct = &self.shuffle.output[index];
            let share = partial_decrypt(&party, position as u32, ct, rng);
            openings.push(Opening {
                position,
                card: self.shuffle.pi[index] as u8,
                proof: ThresholdDecryptionProof {
                    scheme: DECRYPT_SCHEME.to_string(),
                    shares: vec![share],
                },
            });
        }
        Ok(Bundle {
            scheme: SCHEME.to_string(),
            hand_id: self.hand_id.to_string(),
            commit: hex::encode(self.commit),
            public_key: point_to_hex(&self.public_key),
            shuffle_proof: self.shuffle.prove(DEALER_PARTY, SHUFFLE_ROUND, rng),
            client_seeds: client_seeds
                .iter()
                .map(|(seat, seed)| (seat.to_string(), hex::encode(seed)))
                .collect(),
            dealt_seats: dealt_seats.to_vec(),
            withheld_seats: withheld_seats.to_vec(),
            openings,
        })
    }
}

/// The server-internal engine/bot seed for a v2 hand. Never published; it does
/// NOT determine the deck (the deck comes from [`LockedDeck`]).
pub fn engine_seed(
    master_seed: &[u8; 32],
    client_seeds: &BTreeMap<u8, [u8; 32]>,
    hand_id: Uuid,
) -> [u8; 32] {
    let mut parts: Vec<&[u8]> = vec![master_seed];
    parts.extend(client_seeds.values().map(|s| s.as_slice()));
    parts.push(hand_id.as_bytes());
    ds_hash(DS_ENGINE_SEED, &parts)
}

/// The public permutation `σ`: `F[k] = D[σ[k]]`. Seeds are bound in ascending
/// seat order (the `BTreeMap` order), then the hand id.
pub fn public_order(
    commit: &[u8; 32],
    client_seeds: &BTreeMap<u8, [u8; 32]>,
    hand_id: Uuid,
) -> Vec<usize> {
    let mut parts: Vec<&[u8]> = vec![commit];
    let seats: Vec<[u8; 1]> = client_seeds.keys().map(|s| [*s]).collect();
    for (seat, seed) in seats.iter().zip(client_seeds.values()) {
        parts.push(seat);
        parts.push(seed);
    }
    parts.push(hand_id.as_bytes());
    let seed = ds_hash(DS_ORDER, &parts);
    // The engine's unbiased Fisher-Yates; card ids are a fixed bijection onto
    // 0..52, so the shuffled ids are a uniform permutation of 0..52.
    Deck::new(&mut PokerRng::from_seed_bytes(seed))
        .cards()
        .iter()
        .map(|card| card_to_id(*card) as usize)
        .collect()
}

/// Deal positions of the cards a seat holds: `(p, n+p)`.
pub fn hole_positions(dealt_seats: &[u8], seat: u8) -> Option<[usize; 2]> {
    let n = dealt_seats.len();
    let p = dealt_seats.iter().position(|s| *s == seat)?;
    Some([p, n + p])
}

/// Deal positions of the five board cards: `2n..2n+5`.
pub fn board_positions(dealt_seats: &[u8]) -> [usize; 5] {
    let base = 2 * dealt_seats.len();
    [base, base + 1, base + 2, base + 3, base + 4]
}

/// One opened position of the final deck.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Opening {
    pub position: usize,
    /// Card id (`card_id` encoding) the proof opens to.
    pub card: u8,
    pub proof: ThresholdDecryptionProof,
}

/// Everything a verifier needs for one hand. Public except for the openings
/// of a requester's own hole cards, which the server serves only to them.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Bundle {
    pub scheme: String,
    pub hand_id: String,
    /// 64-hex commitment (the `SeedCommit.seed_commit` players received).
    pub commit: String,
    /// 64-hex dealer public key `Q`.
    pub public_key: String,
    pub shuffle_proof: ShuffleProof,
    /// seat (decimal string) → 64-hex player seed mixed into the order.
    pub client_seeds: BTreeMap<String, String>,
    /// Ascending dealt seats (`dealt_seats[p]` = seat at deal position `p`).
    pub dealt_seats: Vec<u8>,
    /// Committed seats that did not reveal (excluded from the order).
    pub withheld_seats: Vec<u8>,
    pub openings: Vec<Opening>,
}

/// What a valid bundle proves.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Verified {
    pub hand_id: String,
    pub commit: String,
    pub client_seeds: BTreeMap<String, String>,
    pub dealt_seats: Vec<u8>,
    pub withheld_seats: Vec<u8>,
    /// Final-deck position → proven card id.
    pub cards: BTreeMap<usize, u8>,
    /// Board slots `0..5` (None = not opened).
    pub board: Vec<Option<u8>>,
    /// seat → its two hole slots (None = not opened).
    pub holes: BTreeMap<u8, [Option<u8>; 2]>,
}

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum Pf2Error {
    #[error("unknown scheme")]
    Scheme,
    #[error("malformed field: {0}")]
    Malformed(&'static str),
    #[error("dealt seats must be 2..=9 distinct ascending seats")]
    Seats,
    #[error("player seed for a seat that was not dealt or was withheld")]
    SeedSeat,
    #[error("commitment does not match the shuffled deck and key")]
    Commit,
    #[error("shuffle proof is invalid")]
    Shuffle,
    #[error("opening position is out of range or repeated")]
    Position,
    #[error("decryption proof is invalid at position {0}")]
    Opening(usize),
}

fn hex32(s: &str, what: &'static str) -> Result<[u8; 32], Pf2Error> {
    let mut out = [0u8; 32];
    hex::decode_to_slice(s, &mut out).map_err(|_| Pf2Error::Malformed(what))?;
    if hex::encode(out) != s {
        return Err(Pf2Error::Malformed(what));
    }
    Ok(out)
}

/// Verify a bundle end to end. The shuffle input is the self-computed standard
/// deck; the key is the committed one; every opened ciphertext is taken from the
/// self-recomputed final deck at a position the verifier derives itself.
pub fn verify(bundle: &Bundle) -> Result<Verified, Pf2Error> {
    if bundle.scheme != SCHEME {
        return Err(Pf2Error::Scheme);
    }
    let hand_id = Uuid::parse_str(&bundle.hand_id).map_err(|_| Pf2Error::Malformed("hand_id"))?;
    if hand_id.to_string() != bundle.hand_id {
        return Err(Pf2Error::Malformed("hand_id"));
    }
    let commit = hex32(&bundle.commit, "commit")?;
    let public_key =
        point_from_hex(&bundle.public_key).ok_or(Pf2Error::Malformed("public_key"))?;
    if point_to_hex(&public_key) != bundle.public_key {
        return Err(Pf2Error::Malformed("public_key"));
    }
    let seats = &bundle.dealt_seats;
    if !(2..=9).contains(&seats.len()) || seats.windows(2).any(|w| w[0] >= w[1]) {
        return Err(Pf2Error::Seats);
    }
    let mut client_seeds = BTreeMap::new();
    for (seat, seed) in &bundle.client_seeds {
        let seat: u8 = seat.parse().map_err(|_| Pf2Error::Malformed("client_seeds"))?;
        if !seats.contains(&seat) || bundle.withheld_seats.contains(&seat) {
            return Err(Pf2Error::SeedSeat);
        }
        if client_seeds.insert(seat, hex32(seed, "client_seeds")?).is_some() {
            return Err(Pf2Error::Malformed("client_seeds"));
        }
    }

    // The shuffled deck, bound to the commitment and the pinned standard input.
    let deck = output_deck_from_proof(&bundle.shuffle_proof).ok_or(Pf2Error::Shuffle)?;
    if deck.len() != DECK_SIZE || commit_of(hand_id, &public_key, &deck) != commit {
        return Err(Pf2Error::Commit);
    }
    let input_hash = deck_hash(&canonical_starting_deck());
    let output_hash = deck_hash(&deck);
    let q_hex = point_to_hex(&public_key);
    if !RealShuffleProofProvider::verifier_with_expected_key(q_hex.clone()).verify_shuffle(
        DEALER_PARTY,
        SHUFFLE_ROUND,
        &input_hash,
        &output_hash,
        Some(&q_hex),
        &bundle.shuffle_proof,
    ) {
        return Err(Pf2Error::Shuffle);
    }

    let order = public_order(&commit, &client_seeds, hand_id);
    let dealer = [(DEALER_PARTY.to_string(), public_key)];
    let mut cards = BTreeMap::new();
    for opening in &bundle.openings {
        let index = *order.get(opening.position).ok_or(Pf2Error::Position)?;
        let card = verify_and_open(opening.position as u32, &deck[index], &dealer, &opening.proof)
            .map_err(|_| Pf2Error::Opening(opening.position))?;
        if card != opening.card {
            return Err(Pf2Error::Opening(opening.position));
        }
        if cards.insert(opening.position, card).is_some() {
            return Err(Pf2Error::Position);
        }
    }
    let board = board_positions(seats)
        .iter()
        .map(|p| cards.get(p).copied())
        .collect();
    let holes = seats
        .iter()
        .filter_map(|&seat| {
            let [a, b] = hole_positions(seats, seat)?;
            Some((seat, [cards.get(&a).copied(), cards.get(&b).copied()]))
        })
        .collect();
    Ok(Verified {
        hand_id: bundle.hand_id.clone(),
        commit: bundle.commit.clone(),
        client_seeds: bundle.client_seeds.clone(),
        dealt_seats: seats.clone(),
        withheld_seats: bundle.withheld_seats.clone(),
        cards,
        board,
        holes,
    })
}
