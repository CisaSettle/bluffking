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
            if terms["all_in"].as_bool() == Some(true) {
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
        if kind == "zjh" {
            let (hand_id, _) = crate::zjh::parse_draw_id(&reveal.commitment.draw_id)?;
            if terms["hand_id"].as_str() != Some(hand_id) {
                return Err("Saved receipt hand differs from the committed hand".into());
            }
        }
        if !matches!(kind, "sicbo-room" | "yuxiaxie-room" | "zjh")
            && receipt["client_seed"] != reveal.client_seed
        {
            return Err("Client seed differs from saved challenge".into());
        }
        // A room wager's receipt keeps the seed the bettor's browser sent with
        // it; a proof asserting a settled wager must list that seed among those
        // it folded in. A room-wide proof (wager cancelled or refused) need not.
        if matches!(kind, "sicbo-room" | "yuxiaxie-room") && proof.get("stake").is_some() {
            let receipt_seed = receipt.get("client_seed").filter(|seed| !seed.is_null());
            if kind == "yuxiaxie-room"
                && proof.get("player_seeds").is_some()
                && receipt_seed.is_none()
            {
                return Err("Saved wager receipt is missing its player contribution".into());
            }
            if let Some(seed) = receipt_seed {
                let listed = proof["player_seeds"]
                    .as_array()
                    .is_some_and(|seeds| seeds.contains(seed));
                if !listed {
                    return Err(
                        "Round does not include the player seed saved with the wager".into(),
                    );
                }
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
    }
    Ok(())
}
