//! Zha Jin Hua adapter for the Hold'em encrypted-deck proof. Three consecutive
//! positions per seat, no board, up to ten seats. The game, hand and frozen
//! roster are hashed into the deck's context, preventing cross-game/seat replay.
use crate::{hash::ds_hash, pf2};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use uuid::Uuid;

pub const SCHEME: &str = "zjh-pf-shuffle-v2";

pub fn context(hand_id: Uuid, seats: &[u8]) -> Result<Uuid, pf2::Pf2Error> {
    if !(2..=10).contains(&seats.len())
        || seats.iter().any(|s| *s >= 10)
        || seats.windows(2).any(|w| w[0] >= w[1])
    {
        return Err(pf2::Pf2Error::Seats);
    }
    let hash = ds_hash("pf2:zjh:context:v1", &[hand_id.as_bytes(), seats]);
    let mut id = [0; 16];
    id.copy_from_slice(&hash[..16]);
    Ok(Uuid::from_bytes(id))
}

pub fn positions(seats: &[u8], seat: u8) -> Option<[usize; 3]> {
    let p = seats.iter().position(|s| *s == seat)? * 3;
    Some([p, p + 1, p + 2])
}

pub fn seed_commitment(seed: &[u8; 32], hand_id: Uuid, seat: u8) -> [u8; 32] {
    ds_hash(
        "pf:client-seed-commit:v1",
        &[seed, hand_id.as_bytes(), &[seat]],
    )
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Bundle {
    pub scheme: String,
    pub hand_id: Uuid,
    pub commitments: BTreeMap<u8, String>,
    pub deck: pf2::Bundle,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct Verified {
    pub hand_id: Uuid,
    pub commit: String,
    pub dealt_seats: Vec<u8>,
    pub client_seeds: BTreeMap<String, String>,
    pub commitments: BTreeMap<u8, String>,
    pub withheld_seats: Vec<u8>,
    pub cards: BTreeMap<usize, u8>,
    pub hands: BTreeMap<u8, [Option<u8>; 3]>,
}

pub fn verify(bundle: &Bundle) -> Result<Verified, pf2::Pf2Error> {
    use pf2::Pf2Error as E;
    if bundle.scheme != SCHEME {
        return Err(E::Scheme);
    }
    let seats = &bundle.deck.dealt_seats;
    if bundle.deck.hand_id != context(bundle.hand_id, seats)?.to_string() {
        return Err(E::Malformed("hand_context"));
    }
    if bundle.deck.openings.is_empty()
        || bundle
            .deck
            .openings
            .iter()
            .any(|o| o.position >= seats.len() * 3)
    {
        return Err(E::Position);
    }
    for (seat, commitment) in &bundle.commitments {
        let mut bytes = [0; 32];
        if !seats.contains(seat)
            || hex::decode_to_slice(commitment, &mut bytes).is_err()
            || hex::encode(bytes) != *commitment
        {
            return Err(E::SeedSeat);
        }
    }
    for (seat, seed) in &bundle.deck.client_seeds {
        let s: u8 = seat.parse().map_err(|_| E::SeedSeat)?;
        let mut bytes = [0; 32];
        if s.to_string() != *seat
            || hex::decode_to_slice(seed, &mut bytes).is_err()
            || bundle.commitments.get(&s)
                != Some(&hex::encode(seed_commitment(&bytes, bundle.hand_id, s)))
        {
            return Err(E::SeedSeat);
        }
    }
    let withheld: Vec<u8> = bundle
        .commitments
        .keys()
        .copied()
        .filter(|s| !bundle.deck.client_seeds.contains_key(&s.to_string()))
        .collect();
    if withheld != bundle.deck.withheld_seats {
        return Err(E::SeedSeat);
    }
    let verified = pf2::verify_deck(&bundle.deck, 10)?;
    let hands = seats
        .iter()
        .map(|&seat| {
            let p = positions(seats, seat).expect("validated seat");
            (seat, p.map(|p| verified.cards.get(&p).copied()))
        })
        .collect();
    Ok(Verified {
        hand_id: bundle.hand_id,
        commit: verified.commit,
        dealt_seats: seats.clone(),
        client_seeds: verified.client_seeds,
        commitments: bundle.commitments.clone(),
        withheld_seats: withheld,
        cards: verified.cards,
        hands,
    })
}
