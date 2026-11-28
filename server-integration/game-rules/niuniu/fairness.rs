//! Niuniu uses Hold'em's pf2 shuffle + DLEQ proofs. Deal positions are five
//! consecutive cards per ascending seat, NOT Hold'em's two rounds and board.
//! Default dealing uses the shared player commit/reveal exchange.
//! Only completed hands can be opened, never the master.

use super::rules::Card;
use mental_poker::pf2;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use uuid::Uuid;

pub const LAYOUT: &str = "niuniu-five-v1";

/// Checkpoint and private database column only. Never serialize into a view.
#[derive(Clone, Serialize, Deserialize)]
pub struct Secret {
    pub hand_id: Uuid,
    master: [u8; 32],
    commit: String,
    seats: Vec<u8>,
    exchange: crate::zjh::fairness::Stored,
}
impl std::fmt::Debug for Secret {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("NiuniuProofSecret")
            .field("hand_id", &self.hand_id)
            .finish_non_exhaustive()
    }
}
impl Secret {
    pub(super) fn exchange_view(&self) -> Value {
        self.exchange.public()
    }

    pub fn commitment(&self) -> Value {
        json!({"scheme": pf2::SCHEME, "layout": LAYOUT, "hand_id": self.hand_id,
            "commit": self.commit, "dealt_seats": self.seats, "entropy": "players", "exchange": self.exchange.public()})
    }
}

pub(crate) fn card(id: u8) -> Card {
    // pf2: 2..A, spades/hearts/diamonds/clubs; Niuniu: A..K, spades/hearts/clubs/diamonds.
    Card {
        rank: if id / 4 == 12 { 1 } else { id / 4 + 2 },
        suit: [0, 1, 3, 2][(id % 4) as usize],
    }
}

pub fn deal(pending: &crate::zjh::fairness::Pending) -> Result<(Secret, Vec<Card>), &'static str> {
    if pending.phase != crate::zjh::fairness::Phase::Ready {
        return Err("entropy_not_ready");
    }
    let stored = &pending.stored;
    let locked = pf2::LockedDeck::new(&stored.master, stored.hand_id);
    if hex::encode(locked.commit()) != stored.commitment {
        return Err("commit_mismatch");
    }
    let deck = locked
        .final_card_ids(&stored.inputs.client_seeds)
        .into_iter()
        .map(card)
        .collect();
    Ok((
        Secret {
            hand_id: stored.hand_id,
            master: stored.master,
            commit: stored.commitment.clone(),
            seats: stored.dealt_seats.clone(),
            exchange: stored.clone(),
        },
        deck,
    ))
}

pub(super) fn build_proof(
    id: Uuid,
    secret: Secret,
    players: Value,
) -> Result<pf2::Bundle, &'static str> {
    if secret.hand_id != id
        || !(2..=6).contains(&secret.seats.len())
        || secret.seats.iter().any(|s| *s >= 6)
        || secret.seats.windows(2).any(|w| w[0] >= w[1])
    {
        return Err("invalid_proof_roster");
    }
    let locked = pf2::LockedDeck::new(&secret.master, id);
    if hex::encode(locked.commit()) != secret.commit {
        return Err("commit_mismatch");
    }
    let seeds = &secret.exchange.inputs.client_seeds;
    let ids = locked.final_card_ids(seeds);
    let players = players.as_array().ok_or("invalid_players")?;
    if players.len() != secret.seats.len() {
        return Err("roster_mismatch");
    }
    let mut seen = std::collections::BTreeSet::new();
    for p in players {
        let seat = p["seat"].as_u64().ok_or("invalid_seat")?;
        let pos = secret
            .seats
            .iter()
            .position(|s| u64::from(*s) == seat)
            .ok_or("roster_mismatch")?;
        if !seen.insert(seat) {
            return Err("roster_mismatch");
        }
        let actual = p["cards"].as_array().ok_or("missing_cards")?;
        let expected: Vec<String> = ids[pos * 5..pos * 5 + 5]
            .iter()
            .map(|id| card(*id).code())
            .collect();
        if actual.len() != 5
            || actual
                .iter()
                .zip(expected)
                .any(|(a, b)| a.as_str() != Some(b.as_str()))
        {
            return Err("card_mismatch");
        }
    }
    // All participants show at Niuniu completion. Undealt positions never open.
    let positions: Vec<usize> = (0..secret.seats.len() * 5).collect();
    locked
        .bundle(
            seeds,
            &secret.seats,
            &secret.exchange.withheld(),
            &positions,
            &mut rand::rngs::OsRng,
        )
        .map_err(|_| "proof_failed")
}
