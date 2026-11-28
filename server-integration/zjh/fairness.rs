//! Private durable deal state. Public projections never contain the master
//! seed or plaintext deck. Expensive crypto runs on the blocking pool.
use mental_poker::{pf2, pf2_zjh as zk};
use rand::RngCore;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::collections::BTreeMap;
use uuid::Uuid;

const WINDOW_MS: i64 = 1500;

#[derive(Clone, Serialize, Deserialize)]
pub struct Stored {
    pub scheme: String,
    pub hand_id: Uuid,
    pub master: [u8; 32],
    pub commitment: String,
    pub dealt_seats: Vec<u8>,
    #[serde(flatten)]
    pub inputs: crate::pf_dealing::SeedInputs,
}
impl std::fmt::Debug for Stored {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Stored")
            .field("hand_id", &self.hand_id)
            .finish_non_exhaustive()
    }
}
impl Stored {
    pub fn withheld(&self) -> Vec<u8> {
        self.inputs.withheld()
    }
    pub fn public(&self) -> Value {
        let seeds: BTreeMap<_, _> = self
            .inputs
            .client_seeds
            .iter()
            .map(|(s, seed)| (s, hex::encode(seed)))
            .collect();
        json!({"scheme": self.scheme, "hand_id": self.hand_id,
            "commitment": self.commitment, "dealt_seats": self.dealt_seats,
            "commitments": self.inputs.commitments, "client_seeds": seeds,
            "withheld_seats": self.withheld()})
    }
    pub fn deck(&self) -> Result<pf2::LockedDeck, String> {
        if self.scheme != zk::SCHEME {
            return Err("unsupported deal scheme".into());
        }
        let deck = pf2::LockedDeck::new(
            &self.master,
            zk::context(self.hand_id, &self.dealt_seats).map_err(|e| e.to_string())?,
        );
        if hex::encode(deck.commit()) != self.commitment {
            return Err("stored deal commitment mismatch".into());
        }
        Ok(deck)
    }
}

#[derive(Clone, Serialize, Deserialize)]
pub struct Pending {
    pub stored: Stored,
    pub eligible: Vec<(usize, u64)>,
    /// Identity frozen before any seed. Replacements cannot inherit a seat's contribution.
    pub owners: BTreeMap<usize, Option<Uuid>>,
    pub expected: Vec<u8>,
    pub phase: Phase,
    pub deadline_ms: i64,
    pub card_deal: Option<crate::card_proofs::Deal>,
}
#[derive(Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Phase {
    Commit,
    Reveal,
    Ready,
}
impl Pending {
    pub fn new(
        eligible: Vec<(usize, u64)>,
        owners: BTreeMap<usize, Option<Uuid>>,
        expected: Vec<u8>,
    ) -> Result<Self, String> {
        let hand_id = Uuid::now_v7();
        let dealt_seats: Vec<u8> = eligible.iter().map(|(s, _)| *s as u8).collect();
        let mut master = [0; 32];
        rand::rngs::OsRng.fill_bytes(&mut master);
        let deck = pf2::LockedDeck::new(
            &master,
            zk::context(hand_id, &dealt_seats).map_err(|e| e.to_string())?,
        );
        Ok(Self {
            stored: Stored {
                scheme: zk::SCHEME.into(),
                hand_id,
                master,
                commitment: hex::encode(deck.commit()),
                dealt_seats,
                inputs: crate::pf_dealing::SeedInputs::default(),
            },
            eligible,
            owners,
            expected,
            phase: Phase::Commit,
            deadline_ms: crate::room_clock::now_ms() + WINDOW_MS,
            card_deal: None,
        })
    }
    /// Shared exchange for native decks; uses the same identity/deadline/hash
    /// checks as ZJH. The locked deck stays private until the exchange finishes.
    pub fn for_cards(
        deal: crate::card_proofs::Deal,
        owners: BTreeMap<usize, Option<Uuid>>,
        expected: Vec<u8>,
    ) -> Self {
        Self {
            stored: Stored {
                scheme: deal.commitment.scheme.clone(),
                hand_id: deal.secret.context.hand_id,
                master: deal.secret.master,
                commitment: deal.commitment.commitment.clone(),
                dealt_seats: deal.secret.context.seats.clone(),
                inputs: crate::pf_dealing::SeedInputs::default(),
            },
            eligible: owners.keys().map(|s| (*s, 0)).collect(),
            owners,
            expected,
            phase: Phase::Commit,
            deadline_ms: crate::room_clock::now_ms() + WINDOW_MS,
            card_deal: Some(deal),
        }
    }
    pub fn for_niuniu(
        eligible: Vec<(usize, u64)>,
        owners: BTreeMap<usize, Option<Uuid>>,
        expected: Vec<u8>,
    ) -> Self {
        let hand_id = Uuid::now_v7();
        let mut master = [0; 32];
        rand::rngs::OsRng.fill_bytes(&mut master);
        let deck = pf2::LockedDeck::new(&master, hand_id);
        Self {
            stored: Stored {
                scheme: pf2::SCHEME.into(),
                hand_id,
                master,
                commitment: hex::encode(deck.commit()),
                dealt_seats: eligible.iter().map(|(s, _)| *s as u8).collect(),
                inputs: crate::pf_dealing::SeedInputs::default(),
            },
            eligible,
            owners,
            expected,
            phase: Phase::Commit,
            deadline_ms: crate::room_clock::now_ms() + WINDOW_MS,
            card_deal: None,
        }
    }
    pub fn finish_cards(&self) -> Option<crate::card_proofs::Deal> {
        if self.phase != Phase::Ready {
            return None;
        }
        let mut deal = self.card_deal.clone()?;
        deal.cards = mental_poker::card_proof::reorder_cards(
            &deal.commitment,
            &deal.cards,
            &deal.secret.client_seeds,
            &self.stored.inputs.client_seeds,
        )
        .ok()?;
        deal.secret.client_seeds = self.stored.inputs.client_seeds.clone();
        deal.commitments = Some(self.stored.inputs.commitments.clone());
        Some(deal)
    }
    pub fn public(&self) -> Value {
        let mut value = self.stored.public();
        value["phase"] = json!(self.phase);
        value
    }
    pub fn complete(&self) -> bool {
        self.phase == Phase::Ready
            || self.stored.inputs.complete(
                self.phase == Phase::Reveal,
                self.expected.iter().copied(),
                |_| false,
            )
    }
    pub fn advance(&mut self) {
        if !self.complete() && crate::room_clock::now_ms() < self.deadline_ms {
            return;
        }
        self.phase = match self.phase {
            Phase::Commit => Phase::Reveal,
            _ => Phase::Ready,
        };
        self.deadline_ms = crate::room_clock::now_ms() + WINDOW_MS;
    }
    pub fn submit(
        &mut self,
        user: Uuid,
        seat: usize,
        hand: Uuid,
        value: &str,
        reveal: bool,
    ) -> bool {
        if hand != self.stored.hand_id
            || self.owners.get(&seat) != Some(&Some(user))
            || !self.expected.contains(&(seat as u8))
            || crate::room_clock::now_ms() > self.deadline_ms
        {
            return false;
        }
        let mut bytes = [0; 32];
        if hex::decode_to_slice(value, &mut bytes).is_err() || hex::encode(bytes) != value {
            return false;
        }
        if (reveal && self.phase != Phase::Reveal) || (!reveal && self.phase != Phase::Commit) {
            return false;
        }
        self.stored.inputs.submit(hand, seat as u8, bytes, reveal)
    }
}

/// Only already masked history cards may become openings. No hidden cards,
/// seeds or cached requester-specific proof bundles enter a public response.
pub fn bundle(stored: Stored, players: Value) -> Result<zk::Bundle, String> {
    let deck = stored.deck()?;
    let ids = deck.final_card_ids(&stored.inputs.client_seeds);
    let rows = players.as_array().ok_or("invalid stored players")?;
    let mut seats = Vec::new();
    let mut positions = Vec::new();
    for p in rows {
        let seat = p["seat"]
            .as_u64()
            .filter(|s| *s < 10)
            .ok_or("invalid stored seat")? as u8;
        seats.push(seat);
        let slots = zk::positions(&stored.dealt_seats, seat).ok_or("unknown stored seat")?;
        if p["cards"].is_null() {
            continue;
        }
        let cards = p["cards"].as_array().ok_or("invalid stored cards")?;
        if cards.len() != 3 {
            return Err("invalid stored cards".into());
        }
        for (card, pos) in cards.iter().zip(slots) {
            let code = mental_poker::card_id::id_to_card(ids[pos])
                .ok_or("invalid card")?
                .to_string();
            if card.as_str() != Some(code.as_str()) {
                return Err("stored cards disagree with proof".into());
            }
            positions.push(pos);
        }
    }
    seats.sort_unstable();
    if seats != stored.dealt_seats {
        return Err("stored roster mismatch".into());
    }
    let proof = zk::Bundle {
        scheme: zk::SCHEME.into(),
        hand_id: stored.hand_id,
        commitments: stored.inputs.commitments.clone(),
        deck: deck
            .bundle(
                &stored.inputs.client_seeds,
                &stored.dealt_seats,
                &stored.withheld(),
                &positions,
                &mut rand::rngs::OsRng,
            )
            .map_err(|e| e.to_string())?,
    };
    Ok(proof)
}

impl std::fmt::Debug for Pending {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Pending")
            .field("hand_id", &self.stored.hand_id)
            .finish_non_exhaustive()
    }
}
