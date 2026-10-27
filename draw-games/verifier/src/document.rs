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
    let receipt = external_receipt
        .or_else(|| document.get("receipt"))
        .filter(|r| !r.is_null());
    if let Some(receipt) = receipt {
        match_receipt(proof, receipt)?;
    }
    Ok(json!({
        "outcome": "replayed",
        "receipt_match": if receipt.is_some() { "matched" } else { "not_provided" },
        "result": result,
        "scope": "Checks the published algorithm and supplied receipt. A local receipt is not a signed timestamp, ledger audit, availability guarantee, or proof of the deployed binary."
    }))
}

fn match_receipt(proof: &Value, receipt: &Value) -> Result<(), String> {
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
            return Err("Draw differs from saved pre-purchase plan".into());
        }
        if let Some(ticket) = proof.get("ticket") {
            let selections = terms["tickets"]
                .as_array()
                .ok_or("Missing saved selections")?;
            if terms["all_in"].as_bool() == Some(true) {
                if proof["plan"]["protocol"] != crate::lottery::PROTOCOL
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
                let copies = balance / game.unit();
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
                if base + i64::from(remainder > 0) > crate::lottery::MAX_ALL_IN_MULTIPLIER {
                    return Err("ALL-IN receipt exceeds ticket capacity".into());
                }
                let matches = selections.iter().enumerate().any(|(index, selection)| {
                    *selection == ticket["entry"]
                        && ticket["multiplier"].as_i64()
                            == Some(base + i64::from((index as i64) < remainder))
                });
                if !matches {
                    return Err("Ticket differs from saved ALL-IN selections or copies".into());
                }
            } else if ticket["multiplier"] != terms["multiplier"]
                || !selections.contains(&ticket["entry"])
            {
                return Err("Ticket differs from saved selections or multiplier".into());
            }
        }
    } else {
        let reveal: crate::proof::Reveal =
            serde_json::from_value(proof["reveal"].clone()).map_err(|e| e.to_string())?;
        if serde_json::to_value(&reveal.commitment).map_err(|e| e.to_string())?
            != receipt["commitment"]
        {
            return Err("Revealed seed differs from saved commitment".into());
        }
        if kind != "sicbo-room" && receipt["client_seed"] != reveal.client_seed {
            return Err("Client seed differs from saved challenge".into());
        }
        if kind == "scratch" {
            if terms["reward_version"] != proof["reward_version"] {
                return Err("Service reward version differs from saved purchase terms".into());
            }
            if terms["all_in"].as_bool() == Some(true) {
                let balance = terms["expected_balance"]
                    .as_i64()
                    .filter(|balance| *balance >= 100)
                    .ok_or("Missing or invalid scratch ALL-IN balance")?;
                let count = terms["count"].as_u64().ok_or("Missing scratch count")?;
                if terms["cost"].as_i64() != Some(100)
                    || count != (balance / 100) as u64
                    || !(1..=crate::scratch::MAX_BATCH_CARDS as u64).contains(&count)
                {
                    return Err("Scratch ALL-IN count differs from saved balance".into());
                }
            } else if ![1, 5, 10].contains(&terms["count"].as_u64().unwrap_or(0)) {
                return Err("Invalid ordinary scratch count".into());
            }
        }
        let fields: &[&str] = if kind == "scratch" {
            &["cost", "count"]
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
    }
    Ok(())
}
