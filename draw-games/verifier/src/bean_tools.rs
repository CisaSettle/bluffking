//! Paid Call / Fold and Lucky Draw rules. A Lucky Draw sees only a salted
//! commitment to the local entrant list and its length.
use crate::proof::{seed_bytes, sha256_fields, Entropy};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};

pub const CALL_FOLD_COST: i64 = 1;
pub const LUCKY_DRAW_COST: i64 = 2;
const MAX_ENTRANTS: usize = 10_000;

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Roster {
    pub size: usize,
    pub commitment: String,
}

impl Roster {
    pub fn is_valid(&self) -> bool {
        (1..=MAX_ENTRANTS).contains(&self.size) && seed_bytes(&self.commitment).is_ok()
    }
}

pub fn call_fold(entropy: &mut Entropy) -> Value {
    let call = entropy.below(100) < 49;
    json!({
        "cost": CALL_FOLD_COST,
        "face": if call { "call" } else { "fold" },
        "reward": {"beans": if call { 2 } else { 0 }, "jackpot": false}
    })
}

pub fn lucky_draw(roster: &Roster, entropy: &mut Entropy) -> Result<Value, String> {
    if !roster.is_valid() {
        return Err("Invalid Lucky Draw roster".into());
    }
    let pick = entropy.below(10_000);
    Ok(json!({
        "cost": LUCKY_DRAW_COST,
        "roster": roster,
        "index": entropy.below(roster.size as u64),
        "reward": {"beans": lucky_prize(pick), "jackpot": pick == 9999}
    }))
}

fn lucky_prize(pick: u64) -> i64 {
    match pick {
        0..=2999 => 0,
        3000..=6999 => 1,
        7000..=8999 => 2,
        9000..=9899 => 4,
        9900..=9998 => 20,
        _ => 100,
    }
}

/// Voluntary offline opening of a private roster. The exact serialized list
/// bytes, including the order, are checked with the player's stored nonce.
pub fn open_roster(result: &Value, opening: &Value) -> Result<Value, String> {
    let roster: Roster =
        serde_json::from_value(result["roster"].clone()).map_err(|e| e.to_string())?;
    let nonce = seed_bytes(opening["nonce"].as_str().ok_or("Missing roster nonce")?)?;
    let text = opening["list"].as_str().ok_or("Missing roster list")?;
    if text.len() > 100_000 {
        return Err("Lucky Draw roster is too large".into());
    }
    let names: Vec<String> = serde_json::from_str(text).map_err(|e| e.to_string())?;
    if names.len() != roster.size
        || hex::encode(sha256_fields(&[
            b"bluffking-lucky-roster-v1",
            &nonce,
            text.as_bytes(),
        ])) != roster.commitment
    {
        return Err("Private Lucky Draw roster differs from the saved commitment".into());
    }
    let index = result["index"]
        .as_u64()
        .and_then(|i| usize::try_from(i).ok())
        .ok_or("Missing Lucky Draw result index")?;
    let name = names.get(index).ok_or("Missing selected entrant")?;
    if opening["winner"] != *name {
        return Err("Displayed entrant differs from the verified draw".into());
    }
    Ok(json!({ "winner": name }))
}

#[cfg(test)]
mod tests {
    use super::lucky_prize;
    use std::collections::BTreeMap;

    #[test]
    fn lucky_draw_preserves_every_published_prize_probability() {
        let mut counts = BTreeMap::new();
        for pick in 0..10_000 {
            *counts.entry(lucky_prize(pick)).or_insert(0) += 1;
        }
        assert_eq!(
            counts,
            BTreeMap::from([
                (0, 3000),
                (1, 4000),
                (2, 2000),
                (4, 900),
                (20, 99),
                (100, 1)
            ])
        );
    }
}
