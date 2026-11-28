//! A hidden seed committed before a named future drand pulse. The pulse is
//! mixed in before the eight-deck shuffle; the seed opens only on retirement.
use super::{rules, ShoeTree};
use crate::beacon::{self, Beacon};
use crate::proof::{seed_bytes, sha256_fields, Entropy};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};

pub const GAME: &str = "baccarat-shuffle";
const PROTOCOL: &str = "bluffking-baccarat-shuffle-v1";
pub const SHOE_LIFETIME_MS: i64 = 30 * 60_000;

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct Plan {
    pub game: String,
    pub protocol: String,
    pub draw_id: String,
    pub beacon_round: u64,
    pub beacon_at_ms: i64,
    pub commitment: String,
}

impl Plan {
    pub fn new(id: String, round: u64, seed: &[u8; 32]) -> Self {
        let commitment = hex::encode(sha256_fields(&[
            PROTOCOL.as_bytes(),
            id.as_bytes(),
            &round.to_be_bytes(),
            seed,
        ]));
        Self {
            game: GAME.into(),
            protocol: PROTOCOL.into(),
            draw_id: id,
            beacon_round: round,
            beacon_at_ms: beacon::round_time_ms(round),
            commitment,
        }
    }

    pub fn expires_at_ms(&self) -> i64 {
        self.beacon_at_ms.saturating_add(SHOE_LIFETIME_MS)
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Proof {
    pub kind: String,
    pub plan: Plan,
    pub beacon: Beacon,
    pub server_seed: String,
    pub shoe_root: String,
    pub card_count: usize,
}

fn derivation(
    plan: &Plan,
    beacon: &Beacon,
    seed: &[u8; 32],
) -> Result<([u8; 32], [u8; 32]), String> {
    if plan.beacon_round != beacon.round
        || *plan != Plan::new(plan.draw_id.clone(), beacon.round, seed)
    {
        return Err("Baccarat shuffle seed or beacon differs from the committed plan".into());
    }
    let randomness = beacon::verify(beacon)?;
    let order = sha256_fields(&[
        b"bluffking-baccarat-shuffle-v1/order",
        plan.draw_id.as_bytes(),
        seed,
        &randomness,
    ]);
    let salts = sha256_fields(&[
        b"bluffking-baccarat-shuffle-v1/salts",
        plan.draw_id.as_bytes(),
        seed,
        &randomness,
    ]);
    Ok((order, salts))
}

pub fn derive(
    plan: &Plan,
    beacon: &Beacon,
    seed: &[u8; 32],
) -> Result<(Vec<u8>, [u8; 32]), String> {
    let (order, salts) = derivation(plan, beacon, seed)?;
    let mut entropy = Entropy::new(order);
    let mut cards: Vec<u8> = (0..8).flat_map(|_| 0..52).collect();
    for i in (1..cards.len()).rev() {
        let j = entropy.below((i + 1) as u64) as usize;
        cards.swap(i, j);
    }
    rules::burn(&mut cards);
    Ok((cards, salts))
}

impl Proof {
    pub fn new(plan: Plan, beacon: Beacon, seed: &[u8; 32], root: [u8; 32], count: usize) -> Self {
        Self {
            kind: GAME.into(),
            plan,
            beacon,
            server_seed: hex::encode(seed),
            shoe_root: hex::encode(root),
            card_count: count,
        }
    }
}

pub fn verify(value: &Value) -> Result<Value, String> {
    let proof: Proof = serde_json::from_value(value.clone()).map_err(|error| error.to_string())?;
    if proof.kind != GAME {
        return Err("invalid Baccarat shuffle proof".into());
    }
    let seed = seed_bytes(&proof.server_seed)?;
    let (cards, salts) = derive(&proof.plan, &proof.beacon, &seed)?;
    let root = ShoeTree::new(&proof.plan.draw_id, &cards, &salts).root();
    if proof.card_count != cards.len() || proof.shoe_root != hex::encode(root) {
        return Err("Reconstructed Baccarat shoe differs from the committed shoe".into());
    }
    Ok(json!(proof))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::baccarat::Commitment;
    use crate::document::verify_document;

    #[test]
    fn signed_pulse_replays_the_linked_shoe_and_requires_a_pre_pulse_receipt() {
        let pulse = Beacon {
            round: 2,
            signature: "b6b6a585449b66eb12e875b64fcbab3799861a00e4dbf092d99e969a5eac57dd3f798acf61e705fe4f093db926626807".into(),
        };
        let seed = [43; 32];
        let plan = Plan::new("test-shoe".into(), pulse.round, &seed);
        let (cards, salts) = derive(&plan, &pulse, &seed).unwrap();
        assert!((405..=414).contains(&cards.len()));
        let tree = ShoeTree::new(&plan.draw_id, &cards, &salts);
        let proof = Proof::new(plan.clone(), pulse, &seed, tree.root(), cards.len());
        let mut remaining = cards.clone();
        let result = rules::deal(&mut remaining).unwrap();
        let round = Commitment::new(
            "round".into(),
            plan.draw_id.clone(),
            tree.root(),
            cards.len(),
            0,
        )
        .with_shuffle(plan.clone());
        let opened = tree.prove(round, &cards, &salts, cards.len() - remaining.len(), result);
        let receipt = json!({
            "protocol": "bluffking-receipt-v1",
            "observed_at_ms": plan.beacon_at_ms - 1,
            "terms": {"kind": GAME, "round_id": plan.draw_id},
            "commitment": plan,
        });
        let mut document =
            json!({"proof": opened, "shoe_shuffle": proof, "shoe_shuffle_receipt": receipt});
        let result = verify_document(&document, None).unwrap();
        assert_eq!(result["result"]["shoe_shuffle_receipt_matched"], false);
        let result = verify_document(&document, Some(&receipt)).unwrap();
        assert_eq!(result["result"]["shoe_shuffle_receipt_matched"], true);
        document["shoe_shuffle_receipt"]["observed_at_ms"] = json!(plan.beacon_at_ms);
        assert!(verify_document(&document, None).is_err());
        document
            .as_object_mut()
            .unwrap()
            .remove("shoe_shuffle_receipt");
        assert_eq!(
            verify_document(&document, None).unwrap()["result"]["shoe_shuffle_receipt_matched"],
            false
        );
        document["shoe_shuffle"]["server_seed"] = json!(hex::encode([42; 32]));
        assert!(verify_document(&document, None).is_err());
    }
}
