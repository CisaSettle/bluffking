//! Selective openings for an eight-deck Baccarat shoe in deal order.
//! This binds revealed cards to the pre-bet root without exposing future cards.
pub mod rules;

use crate::proof::{seed_bytes, sha256_fields};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};

pub const GAME: &str = "baccarat-shoe";
const PROTOCOL: &str = "bluffking-baccarat-shoe-v1";
const DEPTH: usize = 9;
const LEAVES: usize = 1 << DEPTH;
// 416 cards, with the exposed first card and 1–10 more burned.
const MIN_CARDS: usize = 405;
const MAX_CARDS: usize = 414;

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct Commitment {
    pub game: String,
    pub protocol: String,
    pub draw_id: String,
    pub shoe_id: String,
    pub shoe_root: String,
    pub card_count: usize,
    pub start_index: usize,
    pub commitment: String,
}

impl Commitment {
    pub fn new(
        round_id: String,
        shoe_id: String,
        root: [u8; 32],
        card_count: usize,
        start_index: usize,
    ) -> Self {
        let commitment = sha256_fields(&[
            b"bluffking-baccarat-shoe-v1/round",
            round_id.as_bytes(),
            shoe_id.as_bytes(),
            &root,
            &(card_count as u64).to_be_bytes(),
            &(start_index as u64).to_be_bytes(),
        ]);
        Self {
            game: GAME.into(),
            protocol: PROTOCOL.into(),
            draw_id: round_id,
            shoe_id,
            shoe_root: hex::encode(root),
            card_count,
            start_index,
            commitment: hex::encode(commitment),
        }
    }

    fn verify(&self) -> Result<[u8; 32], String> {
        if !(MIN_CARDS..=MAX_CARDS).contains(&self.card_count)
            || self.start_index > self.card_count - rules::MIN_SHOE_CARDS
        {
            return Err("invalid Baccarat shoe length or deal position".into());
        }
        let root = seed_bytes(&self.shoe_root)?;
        if *self
            != Self::new(
                self.draw_id.clone(),
                self.shoe_id.clone(),
                root,
                self.card_count,
                self.start_index,
            )
        {
            return Err("Baccarat round commitment mismatch".into());
        }
        Ok(root)
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct CardOpening {
    index: usize,
    card: u8,
    salt: String,
    path: [String; DEPTH],
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DealProof {
    kind: String,
    plan: Commitment,
    cards: Vec<CardOpening>,
    result: rules::Outcome,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    wager: Option<WagerProof>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct WagerProof {
    bets: rules::Bets,
    stake: i64,
    delta: i64,
}

impl DealProof {
    /// Attach only this viewer's settled wager to their private history proof.
    pub fn with_wager(&self, bets: rules::Bets, delta: i64) -> Option<Self> {
        let stake = bets.total()?;
        Some(Self {
            wager: Some(WagerProof { bets, stake, delta }),
            ..self.clone()
        })
    }
}

fn card_salt(secret: &[u8; 32], index: usize) -> [u8; 32] {
    sha256_fields(&[
        b"bluffking-baccarat-shoe-v1/salt",
        secret,
        &(index as u64).to_be_bytes(),
    ])
}

fn leaf(shoe_id: &str, index: usize, card: u8, salt: &[u8; 32]) -> [u8; 32] {
    sha256_fields(&[
        b"bluffking-baccarat-shoe-v1/card",
        shoe_id.as_bytes(),
        &(index as u64).to_be_bytes(),
        &[card],
        salt,
    ])
}

fn parent(left: &[u8; 32], right: &[u8; 32]) -> [u8; 32] {
    sha256_fields(&[b"bluffking-baccarat-shoe-v1/node", left, right])
}

pub struct ShoeTree {
    nodes: Vec<[u8; 32]>,
}

impl ShoeTree {
    /// `pop_order` is reversed: the server removes its next card with `pop`.
    pub fn new(shoe_id: &str, pop_order: &[u8], secret: &[u8; 32]) -> Self {
        let mut nodes = vec![[0; 32]; LEAVES * 2];
        let mut cards = pop_order.iter().rev();
        for (index, node) in nodes[LEAVES..].iter_mut().enumerate() {
            *node = match cards.next() {
                Some(&card) => leaf(shoe_id, index, card, &card_salt(secret, index)),
                None => sha256_fields(&[
                    b"bluffking-baccarat-shoe-v1/padding",
                    shoe_id.as_bytes(),
                    &(index as u64).to_be_bytes(),
                ]),
            };
        }
        for index in (1..LEAVES).rev() {
            nodes[index] = parent(&nodes[2 * index], &nodes[2 * index + 1]);
        }
        Self { nodes }
    }

    pub fn root(&self) -> [u8; 32] {
        self.nodes[1]
    }

    pub fn prove(
        &self,
        plan: Commitment,
        pop_order: &[u8],
        secret: &[u8; 32],
        count: usize,
        result: rules::Outcome,
    ) -> DealProof {
        let cards = (plan.start_index..plan.start_index + count)
            .map(|index| {
                let mut node = LEAVES + index;
                let path = std::array::from_fn(|_| {
                    let sibling = hex::encode(self.nodes[node ^ 1]);
                    node /= 2;
                    sibling
                });
                CardOpening {
                    index,
                    card: pop_order[pop_order.len() - 1 - index],
                    salt: hex::encode(card_salt(secret, index)),
                    path,
                }
            })
            .collect();
        DealProof {
            kind: GAME.into(),
            plan,
            cards,
            result,
            wager: None,
        }
    }
}

/// Verifies each consecutive opening and replays the exact same dealing rules.
pub fn verify(value: &Value) -> Result<Value, String> {
    let proof: DealProof = serde_json::from_value(value.clone()).map_err(|e| e.to_string())?;
    if proof.kind != GAME || !(4..=6).contains(&proof.cards.len()) {
        return Err("invalid Baccarat deal proof".into());
    }
    let root = proof.plan.verify()?;
    let mut cards = Vec::with_capacity(6);
    for (offset, opening) in proof.cards.iter().enumerate() {
        let index = proof.plan.start_index + offset;
        if opening.index != index || index >= proof.plan.card_count || opening.card >= 52 {
            return Err("invalid Baccarat card or position".into());
        }
        let salt = seed_bytes(&opening.salt)?;
        let mut node = leaf(&proof.plan.shoe_id, index, opening.card, &salt);
        for (depth, sibling) in opening.path.iter().enumerate() {
            let sibling = seed_bytes(sibling)?;
            node = if index & (1 << depth) == 0 {
                parent(&node, &sibling)
            } else {
                parent(&sibling, &node)
            };
        }
        if node != root {
            return Err("Baccarat card does not open the committed shoe".into());
        }
        cards.push(opening.card);
    }
    cards.reverse();
    // The shared dealer requires six available cards before starting, even
    // when the tableau only calls for four or five. A valid replay consumes
    // precisely the supplied card openings and leaves any placeholders alone.
    let padding = 6 - cards.len();
    for _ in 0..padding {
        cards.insert(0, 0);
    }
    let result = rules::deal(&mut cards).ok_or("invalid Baccarat deal")?;
    if cards.len() != padding || result != proof.result {
        return Err("Baccarat outcome differs from the opened cards".into());
    }
    if let Some(wager) = &proof.wager {
        if wager.bets.total() != Some(wager.stake) || wager.bets.delta(&result) != wager.delta {
            return Err("Baccarat payout differs from the wager and opened cards".into());
        }
    }
    Ok(json!(proof))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::document::verify_document;

    #[test]
    fn receipt_and_observation_bind_the_personal_wager_and_deal() {
        let cards = vec![0; 405];
        let secret = [42; 32];
        let tree = ShoeTree::new("shoe", &cards, &secret);
        let plan = Commitment::new("round".into(), "shoe".into(), tree.root(), cards.len(), 0);
        let mut dealt = vec![0; 6];
        let outcome = rules::deal(&mut dealt).unwrap();
        let proof = tree.prove(
            plan.clone(),
            &cards,
            &secret,
            6 - dealt.len(),
            outcome.clone(),
        );
        let bets = rules::Bets {
            banker: 20,
            any_pair: 20,
            ..Default::default()
        };
        let receipt = json!({
            "protocol": "bluffking-receipt-v1",
            "commitment": plan,
            "terms": {"kind": GAME, "round_id": "round", "bets": bets, "stake": 40},
        });
        let public = json!(proof);
        assert!(public.get("wager").is_none());
        assert_eq!(
            verify_document(&public, Some(&receipt)).unwrap()["receipt_match"],
            "commitment_only"
        );
        let mut public_with_null_wager = public.clone();
        public_with_null_wager["wager"] = Value::Null;
        let plan_only = json!({
            "protocol": receipt["protocol"],
            "commitment": receipt["commitment"],
            "terms": {"kind": GAME, "round_id": "round"}
        });
        assert_eq!(
            verify_document(&public_with_null_wager, Some(&plan_only)).unwrap()["receipt_match"],
            "commitment_only"
        );
        let mut private = json!(proof.with_wager(bets, bets.delta(&outcome)).unwrap());
        let observed = json!({"result": outcome, "wager": private["wager"]});
        let verified = verify_document(
            &json!({"proof": private, "observed": observed}),
            Some(&receipt),
        )
        .unwrap();
        assert_eq!(verified["receipt_match"], "matched");
        assert_eq!(verified["observed_match"], "matched");

        let mut other_wager = receipt;
        other_wager["terms"]["bets"]["player"] = json!(20);
        other_wager["terms"]["bets"]["banker"] = json!(0);
        assert!(verify_document(&private, Some(&other_wager)).is_err());

        let mut wrong_observation = observed;
        wrong_observation["wager"]["delta"] = json!(-40);
        assert!(verify_document(
            &json!({"proof": private, "observed": wrong_observation}),
            None
        )
        .is_err());

        private["wager"]["delta"] = json!(-40);
        assert!(verify_document(&private, None).is_err());
    }
}
