//! Shared, network-free document and pre-draw receipt verification.
use serde_json::{json, Value};

/// Accept a bare proof or the `{proof, receipt}` download envelope. A separately
/// supplied receipt takes precedence, matching the offline CLI's `--receipt`.
/// Callers must bound input bytes before JSON parsing.
pub fn verify_document(
    document: &Value,
    external_receipt: Option<&Value>,
) -> Result<Value, String> {
    let proof = document.get("proof").unwrap_or(document);
    let result = crate::verify(proof)?;
    let observed_match = match_observed(proof, &result, document.get("observed"))?;
    let receipt = external_receipt
        .or_else(|| document.get("receipt"))
        .filter(|r| !r.is_null());
    let receipt_match = match receipt {
        Some(receipt) => match_receipt(proof, receipt)?,
        None => "not_provided",
    };
    Ok(json!({
        "outcome": "replayed",
        "receipt_match": receipt_match,
        "observed_match": if observed_match { "matched" } else { "not_provided" },
        "result": result,
        "scope": "Checks the published algorithm and supplied receipt. A local receipt is not a signed timestamp, ledger audit, availability guarantee, or proof of the deployed binary."
    }))
}

/// Match the independently supplied game display against the replay.
fn match_observed(proof: &Value, result: &Value, observed: Option<&Value>) -> Result<bool, String> {
    let Some(observed) = observed else {
        return Ok(false);
    };
    let kind = proof["kind"].as_str().ok_or("Missing proof kind")?;
    let fields: &[&str] = match kind {
        "roulette" => &["request_id", "wagers", "number", "bet", "payout"],
        "fruit" => &[
            "request_id",
            "play",
            "cell",
            "lucky",
            "number",
            "bet",
            "payout",
        ],
        "slots" => &["request_id", "reels", "bet", "payout", "jackpot_bonus"],
        "claw" => &["request_id", "aim", "caught", "jackpot", "payout"],
        "2048" => &["request_id", "cost", "seed"],
        "scratch" => &[
            "cost",
            "lucky",
            "numbers",
            "prize",
            "reward_version",
            "reward",
            "reward_beans",
        ],
        "sicbo-room" => &[
            "dice",
            "total",
            "triple",
            "side",
            "stake",
            "won",
            "delta",
            "jackpot_bonus",
        ],
        "yuxiaxie-room" => &["dice", "faces", "bets", "stake", "won", "delta"],
        _ => return Err("Unsupported displayed-outcome check".into()),
    };
    for &field in fields {
        let actual = observed
            .get(field)
            .ok_or("Missing displayed outcome field")?;
        let expected = if field == "request_id" {
            &proof[field]
        } else {
            &result[field]
        };
        let matches = match (kind, field) {
            ("sicbo-room", "jackpot_bonus") if expected.is_null() || expected == 0 => {
                actual.is_null() || actual == 0
            }
            ("claw", "aim") => {
                let parse =
                    |value: &Value| serde_json::from_value::<crate::claw::Aim>(value.clone());
                parse(actual).ok() == parse(expected).ok()
            }
            _ => actual == expected,
        };
        if !matches {
            return Err(format!("Displayed outcome differs from the proof: {field}"));
        }
    }
    Ok(true)
}

fn match_receipt(proof: &Value, receipt: &Value) -> Result<&'static str, String> {
    if receipt["protocol"] != "bluffking-receipt-v1" {
        return Err("Unsupported saved receipt format".into());
    }
    let kind = proof["kind"].as_str().ok_or("Missing proof kind")?;
    let terms = &receipt["terms"];
    if terms["kind"] != kind {
        return Err("Saved receipt game does not match".into());
    }
    if kind == "lottery" {
        if receipt["commitment"] != proof["plan"] || terms["period"] != proof["plan"]["period"] {
            return Err("Draw differs from the plan saved with the purchase receipt".into());
        }
        // The receipt must have been saved before the preselected round existed.
        let round = proof["plan"]["beacon_round"]
            .as_u64()
            .ok_or("Missing beacon round")?;
        let observed = receipt["observed_at_ms"]
            .as_i64()
            .ok_or("Missing receipt time")?;
        if observed >= crate::beacon::round_time_ms(round) {
            return Err("Receipt was saved after the beacon round was published".into());
        }
        if let Some(ticket) = proof.get("ticket") {
            let selections = terms["tickets"]
                .as_array()
                .ok_or("Missing saved selections")?;
            // Purchase responses and retries preserve the server's entry order.
            let saved_index = receipt
                .get("ticket_ids")
                .map(|ids| {
                    let ids = ids
                        .as_array()
                        .filter(|ids| ids.len() == selections.len())
                        .ok_or("Saved ticket IDs do not match the purchase size")?;
                    let id = ticket["id"].as_str().ok_or("Missing ticket ID")?;
                    let mut matches = ids
                        .iter()
                        .enumerate()
                        .filter(|(_, saved)| saved.as_str() == Some(id));
                    let (index, _) = matches
                        .next()
                        .ok_or("Ticket ID was not saved with this purchase")?;
                    if matches.next().is_some() {
                        return Err("Ticket ID appears more than once in the saved purchase");
                    }
                    Ok(index)
                })
                .transpose()?;
            let (base, remainder) = if terms["all_in"].as_bool() == Some(true) {
                if !proof["plan"]["protocol"]
                    .as_str()
                    .is_some_and(crate::lottery::known_protocol)
                    || terms["multiplier"].as_i64() != Some(1)
                {
                    return Err("Invalid ALL-IN receipt mode or protocol".into());
                }
                let game = crate::lottery::Game::parse(
                    proof["plan"]["game"]
                        .as_str()
                        .ok_or("Missing lottery game")?,
                )
                .ok_or("Unknown lottery game")?;
                let balance = terms["expected_balance"]
                    .as_i64()
                    .filter(|balance| *balance >= game.unit())
                    .ok_or("Missing or invalid ALL-IN balance")?;
                // A balance above the cap buys the capped number of stakes.
                let copies = (balance / game.unit()).min(crate::lottery::MAX_ALL_IN_STAKES);
                let groups = selections.len() as i64;
                if groups != copies.min(crate::lottery::MAX_ALL_IN_GROUPS as i64) || groups == 0 {
                    return Err("Invalid ALL-IN selection count".into());
                }
                for selection in selections {
                    let entry = crate::lottery::Entry::parse(game, selection)
                        .map_err(|_| "Invalid saved ALL-IN entry")?;
                    if entry.stakes() != 1 || entry.to_value() != *selection {
                        return Err("ALL-IN requires normalized basic selections".into());
                    }
                }
                let base = copies / groups;
                let remainder = copies % groups;
                if base + i64::from(remainder > 0) > crate::lottery::MAX_MULTIPLIER {
                    return Err("ALL-IN receipt exceeds ticket capacity".into());
                }
                (base, remainder)
            } else {
                (
                    terms["multiplier"]
                        .as_i64()
                        .ok_or("Missing saved multiplier")?,
                    0,
                )
            };
            let matches = selections.iter().enumerate().any(|(index, selection)| {
                saved_index.is_none_or(|saved| saved == index)
                    && *selection == ticket["entry"]
                    && ticket["multiplier"].as_i64()
                        == Some(base + i64::from((index as i64) < remainder))
            });
            if !matches {
                return Err("Ticket differs from saved selections or multiplier".into());
            }
            Ok(if saved_index.is_some() {
                "matched"
            } else {
                "terms_only"
            })
        } else {
            Ok("commitment_only")
        }
    } else {
        let reveal: crate::proof::Reveal =
            serde_json::from_value(proof["reveal"].clone()).map_err(|e| e.to_string())?;
        if serde_json::to_value(&reveal.commitment).map_err(|e| e.to_string())?
            != receipt["commitment"]
        {
            return Err("Revealed seed differs from saved commitment".into());
        }
        if !matches!(kind, "sicbo-room" | "yuxiaxie-room")
            && receipt["client_seed"] != reveal.client_seed
        {
            return Err("Client seed differs from saved challenge".into());
        }
        if matches!(kind, "sicbo-room" | "yuxiaxie-room") {
            // A public proof has no wager to compare, including when one was refused.
            if proof.get("stake").is_none() {
                return Ok("commitment_only");
            }
            let seed = receipt["client_seed"]
                .as_str()
                .ok_or("Missing player seed saved with the wager")?;
            let included = proof["player_seeds"]
                .as_array()
                .is_some_and(|seeds| seeds.iter().any(|entry| entry.as_str() == Some(seed)));
            if !included {
                return Err("Round does not include the player seed saved with the wager".into());
            }
        }
        if kind == "sicbo-room" && proof.get("stake").is_some() {
            match (
                terms["unit"].as_str(),
                proof.get("jackpot_seed"),
                proof.get("jackpot_bonus"),
            ) {
                (Some("beans"), Some(seed), Some(_))
                    if receipt.get("client_seed") == Some(seed) => {}
                (unit, None, None) if unit != Some("beans") => {}
                _ => return Err("Jackpot proof differs from the saved wager unit or seed".into()),
            }
        }
        if crate::solo::supports(kind) {
            crate::solo::match_terms(proof, terms)?;
        }
        if kind == "scratch" {
            if terms["reward_version"] != proof["reward_version"] {
                return Err("Service reward version differs from saved purchase terms".into());
            }
            if terms["all_in"].as_bool() == Some(true) {
                let balance = terms["expected_balance"]
                    .as_i64()
                    .filter(|balance| *balance >= crate::scratch::COST)
                    .ok_or("Missing or invalid scratch ALL-IN balance")?;
                let count = terms["count"].as_u64().ok_or("Missing scratch count")?;
                if terms["cost"].as_i64() != Some(crate::scratch::COST)
                    || count != crate::scratch::all_in_count(balance) as u64
                {
                    return Err("Scratch ALL-IN count differs from saved balance".into());
                }
            } else if ![1, 5, 10].contains(&terms["count"].as_u64().unwrap_or(0)) {
                return Err("Invalid ordinary scratch count".into());
            }
        }
        let fields: &[&str] = if kind == "scratch" {
            &["cost", "count"]
        } else if kind == "yuxiaxie-room" && proof.get("stake").is_some() {
            &["bets", "stake"]
        } else if proof.get("stake").is_some() {
            &["side", "stake"]
        } else {
            &[]
        };
        for field in fields {
            if terms[*field].is_null() || terms[*field] != proof[*field] {
                return Err(format!("Saved wager parameter differs: {field}"));
            }
        }
        Ok("matched")
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::proof::{commit, Reveal};

    #[test]
    fn room_receipt_distinguishes_the_round_from_a_settled_wager() {
        let player_seed = "22".repeat(32);
        let server_seed = "11".repeat(32);
        let commitment = commit("sicbo-room", "receipt-test", &server_seed).unwrap();
        let mut proof = json!({
            "kind": "sicbo-room",
            "reveal": Reveal { commitment: commitment.clone(), server_seed, client_seed: String::new() },
            "player_seeds": [player_seed],
        });
        let mut receipt = json!({
            "protocol": "bluffking-receipt-v1",
            "commitment": commitment,
            "terms": {"kind": "sicbo-room", "side": "big", "stake": 20}
        });
        assert_eq!(match_receipt(&proof, &receipt).unwrap(), "commitment_only");

        proof["side"] = json!("big");
        proof["stake"] = json!(20);
        assert!(match_receipt(&proof, &receipt).is_err());
        receipt["client_seed"] = json!(player_seed);
        assert_eq!(match_receipt(&proof, &receipt).unwrap(), "matched");
        receipt["client_seed"] = json!("33".repeat(32));
        assert!(match_receipt(&proof, &receipt).is_err());
    }

    #[test]
    fn lottery_receipt_distinguishes_a_plan_terms_and_ticket_id() {
        let plan = json!({"period": 1, "beacon_round": 1});
        let entry = json!({"numbers": [1, 2, 3]});
        let other_entry = json!({"numbers": [3, 4, 5]});
        let mut proof = json!({"kind": "lottery", "plan": plan});
        let mut receipt = json!({
            "protocol": "bluffking-receipt-v1",
            "commitment": plan,
            "observed_at_ms": 1,
            "terms": {"kind": "lottery", "period": 1, "tickets": [entry, other_entry], "multiplier": 1}
        });
        assert_eq!(match_receipt(&proof, &receipt).unwrap(), "commitment_only");

        proof["ticket"] = json!({"id": "ticket-a", "entry": entry, "multiplier": 1});
        assert_eq!(match_receipt(&proof, &receipt).unwrap(), "terms_only");
        receipt["ticket_ids"] = json!(["ticket-b", "ticket-a"]);
        assert!(match_receipt(&proof, &receipt).is_err());
        receipt["ticket_ids"] = json!(["ticket-a", "ticket-b"]);
        assert_eq!(match_receipt(&proof, &receipt).unwrap(), "matched");
        proof["ticket"]["entry"] = other_entry;
        assert!(match_receipt(&proof, &receipt).is_err());
        proof["ticket"]["id"] = json!("ticket-b");
        assert_eq!(match_receipt(&proof, &receipt).unwrap(), "matched");
    }
    #[test]
    fn lottery_all_in_checks_the_multiplier_for_the_ticket_position() {
        let plan = json!({
            "period": 1,
            "beacon_round": 1,
            "game": "ssq",
            "protocol": crate::lottery::PROTOCOL,
        });
        let entry = json!({"red": [1, 2, 3, 4, 5, 6], "blue": [7]});
        let mut proof = json!({
            "kind": "lottery",
            "plan": plan,
            "ticket": {"id": "ticket-0", "entry": entry, "multiplier": 2}
        });
        let receipt = json!({
            "protocol": "bluffking-receipt-v1",
            "commitment": plan,
            "observed_at_ms": 1,
            "ticket_ids": (0..10).map(|i| format!("ticket-{i}")).collect::<Vec<_>>(),
            "terms": {"kind": "lottery", "period": 1, "tickets": vec![entry; 10],
                       "multiplier": 1, "all_in": true, "expected_balance": 22}
        });
        assert_eq!(match_receipt(&proof, &receipt).unwrap(), "matched");
        proof["ticket"]["id"] = json!("ticket-1");
        assert!(match_receipt(&proof, &receipt).is_err());
        proof["ticket"]["multiplier"] = json!(1);
        assert_eq!(match_receipt(&proof, &receipt).unwrap(), "matched");
    }
}
