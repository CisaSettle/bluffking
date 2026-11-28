//! Shared, network-free document and pre-draw receipt verification.
use crate::{evidence::SignedEvidence, sicbo};
use serde_json::{json, Value};

/// Accept a bare proof or the `{proof, receipt}` download envelope. A separately
/// supplied receipt takes precedence, matching the offline CLI's `--receipt`.
/// Callers must bound input bytes before JSON parsing.
pub fn verify_document(
    document: &Value,
    external_receipt: Option<&Value>,
) -> Result<Value, String> {
    verify_document_with_key(document, external_receipt, None)
}

pub fn verify_document_with_key(
    document: &Value,
    external_receipt: Option<&Value>,
    pinned_key: Option<&str>,
) -> Result<Value, String> {
    let recovery = is_wager_recovery(document);
    let pending = (recovery && document["proof"].is_null()).then(|| {
        if document["roster"].is_null() {
            json!({"kind": sicbo::ACCEPTANCE_KIND, "acceptance": document["acceptance"]})
        } else {
            json!({"kind": sicbo::ROSTER_KIND, "roster": document["roster"]})
        }
    });
    let proof = pending
        .as_ref()
        .unwrap_or_else(|| document.get("proof").unwrap_or(document));
    let signed_sicbo = has_signed_sicbo(document);
    let result = if signed_sicbo {
        crate::sicbo::verify_with_key(proof, pinned_key)?
    } else {
        if pinned_key.is_some() {
            return Err("Proof does not contain signed Sic Bo evidence".into());
        }
        crate::verify(proof)?
    };
    let mut result = replay_completion(document, proof, result)?;
    if let Some(opening) = document.get("private_roster") {
        if proof["kind"] != "lucky-draw" {
            return Err("Private roster opening requires a Lucky Draw proof".into());
        }
        result["verified_roster"] = crate::bean_tools::open_roster(&result, opening)?;
    }
    let observed_match = match_observed(proof, &result, document.get("observed"))?;
    let receipt = external_receipt
        .or_else(|| document.get("receipt"))
        .filter(|r| !r.is_null());
    let wager = if recovery {
        if document.get("proof").is_none()
            || document.get("roster").is_none()
            || document["acceptance"].is_null()
        {
            return Err("Incomplete Sic Bo wager recovery document".into());
        }
        let roster = roster_evidence(proof);
        if &document["roster"] != roster {
            return Err("Recovered roster differs from the proof".into());
        }
        Some(verify_wager_status(
            proof,
            &document["acceptance"],
            pinned_key,
        )?)
    } else {
        None
    };
    let receipt_match = match receipt {
        Some(receipt) if receipt["kind"] == crate::sicbo::ACCEPTANCE_KIND => {
            match_signed_wager(proof, receipt)?
        }
        Some(receipt) => match_receipt(proof, receipt)?,
        None => wager
            .as_ref()
            .map_or("not_provided", |(_, status)| wager_match(status)),
    };
    let mut response = json!({
        "outcome": "replayed",
        "receipt_match": receipt_match,
        "observed_match": if observed_match { "matched" } else { "not_provided" },
        "result": result,
        "evidence_key": if signed_sicbo {
            if pinned_key.is_some() { "pinned" } else { "supplied_with_proof" }
        } else { "not_applicable" },
        "scope": "Checks the published algorithm and supplied receipt. A local receipt is not a signed timestamp, ledger audit, availability guarantee, or proof of the deployed binary."
    });
    if let Some((revision, status)) = wager {
        response["wager_revision"] = json!(revision);
        response["wager_status"] = json!(status);
    }
    Ok(response)
}

fn replay_completion(document: &Value, proof: &Value, mut result: Value) -> Result<Value, String> {
    let Some(completion) = document.get("completion") else {
        return Ok(result);
    };
    if proof["kind"] != "2048" {
        return Err("Completion replays require a 2048 proof".into());
    }
    let actions = completion["actions"]
        .as_array()
        .filter(|a| !a.is_empty() && a.len() <= 8192)
        .ok_or("Invalid 2048 action count")?;
    let actions: Vec<String> = actions
        .iter()
        .map(|a| a.as_str().map(str::to_owned).ok_or("Invalid 2048 action"))
        .collect::<Result<_, _>>()?;
    let seed = result["seed"]
        .as_u64()
        .and_then(|seed| u32::try_from(seed).ok())
        .ok_or("Invalid 2048 seed")?;
    if !crate::puzzle2048::completed(seed, &actions) {
        return Err("The supplied 2048 replay did not validly reach its target".into());
    }
    if completion["refund"] != crate::puzzle2048::COMPLETION_REFUND {
        return Err("Displayed refund differs from the 2048 rules".into());
    }
    result["completion"] = json!({
        "target_reached": true,
        "actions": actions.len(),
        "expected_refund": crate::puzzle2048::COMPLETION_REFUND,
    });
    Ok(result)
}

fn is_wager_recovery(document: &Value) -> bool {
    document["kind"] == sicbo::RECOVERY_KIND
        || (document.get("roster").is_some() && document.get("proof").is_some())
}

pub fn has_signed_sicbo(document: &Value) -> bool {
    if is_wager_recovery(document) {
        return true;
    }
    let proof = document.get("proof").unwrap_or(document);
    let kind = proof["kind"].as_str();
    matches!(
        kind,
        Some(crate::sicbo::VOID_KIND | crate::sicbo::ACCEPTANCE_KIND | crate::sicbo::ROSTER_KIND)
    ) || kind == Some("sicbo-room") && proof.get("audit").is_some()
}

fn roster_evidence(proof: &Value) -> &Value {
    match proof["kind"].as_str() {
        Some("sicbo-room") => &proof["audit"]["roster"],
        Some(sicbo::ROSTER_KIND | sicbo::VOID_KIND) => &proof["roster"],
        _ => &Value::Null,
    }
}

fn signed_evidence(value: &Value) -> Result<SignedEvidence, String> {
    serde_json::from_value(value.clone()).map_err(|error| error.to_string())
}

fn verify_wager_status(
    proof: &Value,
    value: &Value,
    pinned_key: Option<&str>,
) -> Result<(u64, &'static str), String> {
    let acceptance = signed_evidence(value)?;
    let roster_value = roster_evidence(proof);
    let proof_key = match proof["kind"].as_str() {
        Some(sicbo::VOID_KIND) => proof["terminal"]["public_key"].as_str(),
        Some(sicbo::ACCEPTANCE_KIND) => proof["acceptance"]["public_key"].as_str(),
        _ => roster_value["public_key"].as_str(),
    };
    let key = pinned_key
        .or(proof_key)
        .ok_or("Missing Sic Bo evidence key")?;
    let claim = sicbo::verify_acceptance(&acceptance, key)?;
    if let Some(bettor) = proof["audit"]
        .get("acceptance")
        .filter(|bettor| !bettor.is_null())
    {
        if bettor != value {
            return Err("Settled wager differs from the supplied signed acceptance".into());
        }
    }
    if proof["kind"] == sicbo::VOID_KIND {
        let terminal: sicbo::Terminal = signed_evidence(&proof["terminal"])?.verify(key)?;
        if claim.round_id != terminal.round_id
            || claim.commitment != terminal.commitment
            || claim.accepted_at_ms > terminal.recorded_at_ms
        {
            return Err("Void differs from the signed acceptance".into());
        }
    }
    let status = if !roster_value.is_null() {
        let roster = sicbo::verify_roster(&signed_evidence(roster_value)?, key)?;
        if sicbo::acceptance_in_roster(&acceptance, &claim, &roster)? {
            "active"
        } else if claim.wager.is_none() && claim.accepted_at_ms <= roster.locked_at_ms {
            "accepted_cancellation"
        } else {
            "not_in_locked_roster"
        }
    } else if proof["kind"] == sicbo::VOID_KIND {
        "void_without_roster"
    } else if proof["kind"] == sicbo::ACCEPTANCE_KIND && proof["acceptance"] == *value {
        "pending_roster"
    } else {
        return Err("Signed Sic Bo acceptance requires matching round evidence".into());
    };
    Ok((claim.revision, status))
}

fn wager_match(status: &str) -> &'static str {
    if status == "active" {
        "matched"
    } else {
        "commitment_only"
    }
}

fn match_signed_wager(proof: &Value, receipt: &Value) -> Result<&'static str, String> {
    let (_, status) = verify_wager_status(proof, &receipt["acceptance"], None)?;
    if status == "not_in_locked_roster" {
        return Err("Signed Sic Bo acceptance is absent from this roster".into());
    }
    Ok(wager_match(status))
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
        "call-fold" => &["request_id", "cost", "face", "reward"],
        "lucky-draw" => &["request_id", "cost", "roster", "index", "reward"],
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
        "yuxiaxie-room" | crate::yuxiaxie::BEACON_GAME => {
            &["dice", "faces", "bets", "stake", "won", "delta"]
        }
        crate::baccarat::GAME => &["result", "wager"],
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
    if kind == crate::sicbo::VOID_KIND {
        let terminal: crate::evidence::SignedEvidence =
            serde_json::from_value(proof["terminal"].clone()).map_err(|error| error.to_string())?;
        let claim: crate::sicbo::Terminal = terminal.verify(&terminal.public_key)?;
        return if terms["kind"] == "sicbo-room"
            && receipt["commitment"]["commitment"] == claim.commitment
            && receipt["commitment"]["draw_id"] == claim.round_id
        {
            Ok("commitment_only")
        } else {
            Err("Void differs from the saved commitment".into())
        };
    }
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
    } else if matches!(kind, crate::yuxiaxie::BEACON_GAME | crate::baccarat::GAME) {
        if receipt["commitment"] != proof["plan"] || terms["round_id"] != proof["plan"]["draw_id"] {
            return Err("Round differs from the plan saved before betting".into());
        }
        if kind == crate::yuxiaxie::BEACON_GAME {
            let observed = receipt["observed_at_ms"]
                .as_i64()
                .ok_or("Missing receipt time")?;
            let round = proof["plan"]["beacon_round"]
                .as_u64()
                .ok_or("Missing beacon round")?;
            if observed >= crate::beacon::round_time_ms(round) {
                return Err("Receipt was saved after the beacon round was published".into());
            }
            if proof["stake"].as_i64().is_some() {
                if terms["bets"] != proof["bets"] || terms["stake"] != proof["stake"] {
                    return Err("Wager differs from the terms saved before betting".into());
                }
                return Ok("matched");
            }
        }
        if kind == crate::baccarat::GAME {
            if let Some(wager) = proof.get("wager").filter(|wager| wager.is_object()) {
                if terms["bets"] != wager["bets"] || terms["stake"] != wager["stake"] {
                    return Err("Wager differs from the terms saved before betting".into());
                }
                return Ok("matched");
            }
        }
        Ok("commitment_only")
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
            if let Some(audit) = proof.get("audit") {
                let acceptance = signed_evidence(&audit["acceptance"])?;
                let key = audit["roster"]["public_key"]
                    .as_str()
                    .ok_or("Missing Sic Bo evidence key")?;
                let claim = sicbo::verify_acceptance(&acceptance, key)?;
                if terms["round_id"] != claim.round_id
                    || terms["bet_revision"]
                        .as_u64()
                        .and_then(|revision| revision.checked_add(1))
                        != Some(claim.revision)
                    || terms["unit"] != json!(claim.unit)
                    || receipt["client_seed"] != json!(claim.client_seed)
                {
                    return Err(
                        "Sic Bo proof differs from the saved wager revision, unit or seed".into(),
                    );
                }
            }
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
    fn completion_replays_client_moves_and_checks_the_refund() {
        // Winning sequence generated with the H5 engine on the proof's seed.
        let moves = concat!(
            "rrdudddrdddldlllldddldrlddlddldrdldldddrrdlddrlldldrddrdrrrddddllldrdldrlddrddlldddrdrrdrdddlldrddld",
            "drdrldrdrrddrdddrdrrdllllldlddrdrdddlrrddlddrdldrldldddddldlddrdlrdrrdrddrrrddrrdllddldrdldddrdrdrdr",
            "rrddrrdllddrdrrdrrrrdrdrrrddrlrrlldrdrdrrdldllddrdrldrdldlddrdrdrrrlddrrddldrlldlddrdlrrddddddrrdlld",
            "dldrdldrdddllllrldrdrrrdllddrrddlldrrdddddrrrdrrrrdrrdrddlldlrddrdlldlddrdddddrdldlrdrrdrddddrrlrdrr",
            "lldllddrrdlrdlrdddldrddlddlrrddrllddrdrrrrdrddrddldrdrdrrddrdldlddrrrdrldlrrdlllldlddddddrldlddddlll",
            "drdlldddrldddrddrdlddrdldlrlddrdllllddrdrdrdldrdlrdldrdldlrdrrdrrddrrdrrrdrrldlddrdrlrddlldrddrddddl",
            "lllddrdrrrdldrddrdrddrrrdrrrrrdddldrddldrrdldddrddrrdrrddddldrdddrrdlddrrdldrddrldrdldrdrrdrrdllddrr",
            "ddddlrdrdrrrrddrldrdrdldrddlldrrdrdrdrrdllldrdlrddrdlllddddrrddrdrrdrrrddrlrrdrdldrrdddddddlldrlrddr",
            "drdrddrddlddrdrddrddrddrrrddrlddrrdrrrrrlrrdrrdrdddllldlldrdldrrrdrrrrrrrrrdddrdldrdrdrdldrlrrddldld",
            "rdlrdrldrddlrdlddldldrrrdlll"
        );
        let actions: Vec<_> = moves
            .bytes()
            .map(|step| match step {
                b'u' => "up",
                b'd' => "down",
                b'l' => "left",
                b'r' => "right",
                _ => unreachable!(),
            })
            .collect();
        let request_id = "00000000-0000-4000-8000-000000000001";
        let server_seed = "00".repeat(32);
        let client_seed = "01".repeat(32);
        let commitment = commit("2048", request_id, &server_seed).unwrap();
        let mut document = json!({
            "proof": {
                "kind": "2048", "request_id": request_id,
                "reveal": Reveal { commitment: commitment.clone(), server_seed, client_seed: client_seed.clone() },
                "result": {"cost": 1, "seed": 1693791216}
            },
            "receipt": {
                "protocol": "bluffking-receipt-v1", "commitment": commitment, "client_seed": client_seed,
                "terms": {"kind": "2048", "request_id": request_id, "cost": 1}
            },
            "completion": {"actions": actions, "refund": 1}
        });
        let checked = verify_document(&document, None).unwrap();
        assert_eq!(checked["receipt_match"], "matched");
        assert_eq!(checked["result"]["completion"]["target_reached"], true);

        document["completion"]["refund"] = json!(2);
        assert!(verify_document(&document, None).is_err());
        document["completion"]["refund"] = json!(1);
        document["completion"]["actions"][0] = json!("undo");
        assert!(verify_document(&document, None).is_err());
    }

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
