//! Selective evidence for the tournament engine's deterministic deal.
//! The seed stays private. Positions are two seat-order passes, then boards.
use std::collections::{BTreeMap, HashSet};

use serde::{Deserialize, Serialize};
use serde_json::{json, Value};

use crate::merkle_cards::{Domain, Opening, Tree};
use crate::proof::{seed_bytes, sha256_fields};

pub const GAME: &str = "tournament-holdem-deal";
const PROTOCOL: &str = "bluffking-tournament-holdem-v1";
const DEPTH: usize = 6;
const DOMAIN: Domain = Domain {
    salt: b"bluffking-tournament-holdem-v1/salt",
    card: b"bluffking-tournament-holdem-v1/card",
    node: b"bluffking-tournament-holdem-v1/node",
    padding: b"bluffking-tournament-holdem-v1/padding",
};

#[derive(Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct Plan {
    pub game: String,
    pub protocol: String,
    pub draw_id: String,
    pub seats: Vec<u8>,
    pub deck_root: String,
    pub commitment: String,
}

impl Plan {
    fn new(id: &str, seats: &[u8], root: &[u8; 32]) -> Self {
        Self {
            game: GAME.into(),
            protocol: PROTOCOL.into(),
            draw_id: id.into(),
            seats: seats.to_vec(),
            deck_root: hex::encode(root),
            commitment: hex::encode(sha256_fields(&[
                PROTOCOL.as_bytes(),
                id.as_bytes(),
                seats,
                root,
            ])),
        }
    }
}

pub struct Deal {
    pub plan: Plan,
    tree: Tree,
}

impl Deal {
    pub fn new(id: &str, seats: &[u8], cards: &[u8], seed: &[u8; 32]) -> Self {
        let tree = Tree::new(&DOMAIN, DEPTH, id, cards.iter().copied(), seed);
        let plan = Plan::new(id, seats, &tree.root());
        Self { plan, tree }
    }

    pub fn open(&self, index: usize, card: u8, seed: &[u8; 32]) -> Opening {
        self.tree.open(&DOMAIN, index, card, seed)
    }
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Proof {
    pub kind: String,
    pub plan: Plan,
    pub board: Vec<Opening>,
    /// Only the additional cards on the second run. Its shared prefix is on board 1.
    pub second: Vec<Opening>,
    pub holes: BTreeMap<u8, [Option<Opening>; 2]>,
}

pub fn card_code(id: u8) -> Option<String> {
    (id < 52).then(|| {
        let rank = char::from(b"23456789TJQKA"[usize::from(id / 4)]);
        let suit = char::from(b"shdc"[usize::from(id % 4)]);
        format!("{rank}{suit}")
    })
}

pub fn card_id(code: &str) -> Option<u8> {
    let [rank, suit] = *code.as_bytes() else {
        return None;
    };
    let rank = b"23456789TJQKA".iter().position(|&r| r == rank)?;
    let suit = b"shdc".iter().position(|&s| s == suit)?;
    Some((rank * 4 + suit) as u8)
}

pub fn verify(value: &Value) -> Result<Value, String> {
    let proof: Proof = serde_json::from_value(value.clone()).map_err(|error| error.to_string())?;
    let root = seed_bytes(&proof.plan.deck_root)?;
    let seats = &proof.plan.seats;
    let n = seats.len();
    if proof.kind != GAME
        || !(2..=9).contains(&n)
        || seats.iter().any(|&seat| seat >= 9)
        || seats.iter().collect::<std::collections::HashSet<_>>().len() != seats.len()
        || proof.plan != Plan::new(&proof.plan.draw_id, seats, &root)
        || !matches!(proof.board.len(), 0 | 3 | 4 | 5)
        || !matches!(proof.second.len(), 0 | 1 | 2 | 5)
        || (!proof.second.is_empty() && proof.board.len() != 5)
        || proof.holes.is_empty()
    {
        return Err("invalid tournament deal layout".into());
    }
    let mut seen = HashSet::new();
    let mut check = |opening: &Opening, position: usize| {
        if opening.index != position
            || !seen.insert(opening.card)
            || !opening.verifies(&DOMAIN, DEPTH, &proof.plan.draw_id, &root)
        {
            return Err("invalid tournament card opening");
        }
        card_code(opening.card).ok_or("invalid tournament card")
    };
    let board: Vec<_> = proof
        .board
        .iter()
        .enumerate()
        .map(|(i, card)| check(card, 2 * n + i))
        .collect::<Result<_, _>>()?;
    let second: Vec<_> = proof
        .second
        .iter()
        .enumerate()
        .map(|(i, card)| check(card, 2 * n + 5 + i))
        .collect::<Result<_, _>>()?;
    let mut holes = BTreeMap::new();
    for (seat, [first, second]) in &proof.holes {
        let i = seats
            .iter()
            .position(|s| s == seat)
            .ok_or("seat was not dealt in")?;
        let first = first.as_ref().map(|card| check(card, i)).transpose()?;
        let second = second.as_ref().map(|card| check(card, n + i)).transpose()?;
        if first.is_none() && second.is_none() {
            return Err("empty tournament hole opening".into());
        }
        holes.insert(seat, [first, second]);
    }
    let board2 = (!second.is_empty()).then(|| {
        board[..5 - second.len()]
            .iter()
            .cloned()
            .chain(second)
            .collect::<Vec<_>>()
    });
    Ok(json!({"kind": GAME, "plan": proof.plan, "board": board, "board2": board2, "holes": holes}))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::document::verify_document;

    #[test]
    fn wrapped_seats_second_run_and_one_card_show_are_bound_to_positions() {
        let seed = [31; 32];
        let cards: Vec<_> = (0..52).collect();
        let deal = Deal::new("hand", &[6, 0, 2], &cards, &seed);
        let open = |i: usize| deal.open(i, cards[i], &seed);
        let proof = Proof {
            kind: GAME.into(),
            plan: deal.plan.clone(),
            board: (6..11).map(open).collect(),
            second: (11..13).map(open).collect(),
            holes: BTreeMap::from([
                (6, [Some(open(0)), Some(open(3))]),
                (2, [None, Some(open(5))]),
            ]),
        };
        let receipt = json!({
            "protocol": "bluffking-receipt-v1",
            "commitment": deal.plan,
            "terms": {"kind": GAME, "round_id": "hand", "seat": 6, "cards": ["2s", "2c"]},
            "tournament_cards": {
                "board": ["3d", "3c", "4s", "4h", "4d"],
                "board2": ["3d", "3c", "4s", "4c", "5s"],
                "holes": {"6": ["2s", "2c"], "2": [null, "3h"]},
            },
        });
        let mut doc = json!({"proof": proof, "receipt": receipt});
        let result = verify_document(&doc, None).unwrap();
        assert_eq!(result["receipt_match"], "matched");
        assert_eq!(result["result"]["holes"]["2"], json!([null, "3h"]));
        assert!(result["result"]["holes"].get("0").is_none());
        assert_eq!(
            result["result"]["board2"],
            json!(["3d", "3c", "4s", "4c", "5s"])
        );
        let mut omitted = doc.clone();
        omitted["proof"]["second"] = json!([]);
        assert!(verify_document(&omitted, None).is_err());
        doc["proof"]["holes"]["2"][1]["index"] = json!(4);
        assert!(verify_document(&doc, None).is_err());
    }
}
