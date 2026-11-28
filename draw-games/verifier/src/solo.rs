//! One commitment, reveal and receipt format for instant solo games.
use crate::{bean_tools, claw, fruit, proof::Reveal, puzzle2048, roulette, slots};
use serde_json::{json, Value};

pub fn supports(game: &str) -> bool {
    matches!(
        game,
        "roulette" | "slots" | "fruit" | "claw" | "2048" | "call-fold" | "lucky-draw"
    )
}

pub fn result(game: &str, detail: &Value) -> Value {
    match game {
        "claw" => json!({"aim": detail["aim"], "caught": detail["caught"],
            "jackpot": detail["jackpot"], "payout": detail["payout"]}),
        "2048" => json!({"cost": detail["cost"], "seed": detail["seed"]}),
        "call-fold" => {
            json!({"cost": detail["cost"], "face": detail["face"], "reward": detail["reward"]})
        }
        "lucky-draw" => json!({"cost": detail["cost"], "roster": detail["roster"],
            "index": detail["index"], "reward": detail["reward"]}),
        _ => detail.clone(),
    }
}

pub(crate) fn verify(proof: &Value) -> Result<Value, String> {
    let reveal: Reveal =
        serde_json::from_value(proof["reveal"].clone()).map_err(|error| error.to_string())?;
    let kind = proof["kind"].as_str().ok_or("Missing game")?;
    if reveal.commitment.game != kind || proof["request_id"] != reveal.commitment.draw_id {
        return Err("The reveal belongs to another game or request".into());
    }
    let mut entropy = reveal.entropy()?;
    let mut claimed = proof["result"].clone();
    let replayed = match kind {
        "roulette" => {
            let wagers: Vec<roulette::Wager> = serde_json::from_value(claimed["wagers"].clone())
                .map_err(|error| error.to_string())?;
            roulette::settle(&wagers, &mut entropy)?
        }
        "slots" => slots::settle(&mut entropy),
        "fruit" => {
            let bet = serde_json::from_value(claimed["play"].clone())
                .map_err(|error| error.to_string())?;
            fruit::settle(&bet, &mut entropy)?
        }
        "claw" => {
            let aim = serde_json::from_value::<claw::Aim>(claimed["aim"].clone())
                .map_err(|error| error.to_string())?;
            // Browsers serialize integral coordinates such as 0.0 as 0.
            claimed["aim"] = json!(aim);
            serde_json::to_value(claw::settle(aim, &mut entropy)?)
                .map_err(|error| error.to_string())?
        }
        "2048" => json!({"cost": puzzle2048::COST, "seed": puzzle2048::draw_seed(&mut entropy)}),
        "call-fold" => bean_tools::call_fold(&mut entropy),
        "lucky-draw" => {
            let roster =
                serde_json::from_value(claimed["roster"].clone()).map_err(|e| e.to_string())?;
            bean_tools::lucky_draw(&roster, &mut entropy)?
        }
        _ => return Err("Unsupported solo draw".into()),
    };
    if replayed != claimed {
        return Err("The result differs from the replayed draw or payout".into());
    }
    Ok(replayed)
}

pub(crate) fn match_terms(proof: &Value, terms: &Value) -> Result<(), String> {
    let wager = match proof["kind"].as_str() {
        Some("roulette") => Some("wagers"),
        Some("fruit") => Some("play"),
        Some("claw") => Some("aim"),
        Some("2048" | "call-fold") => Some("cost"),
        Some("lucky-draw") if proof["result"]["cost"] == terms["cost"] => Some("roster"),
        Some("lucky-draw") => return Err("Lucky Draw price differs from the saved receipt".into()),
        _ => None,
    };
    let mismatch = match wager {
        Some("aim") => {
            let aim = |value: &Value| {
                serde_json::from_value::<claw::Aim>(value["aim"].clone())
                    .map_err(|error| error.to_string())
            };
            aim(&proof["result"])? != aim(terms)?
        }
        Some(field) => proof["result"][field] != terms[field],
        None => false,
    };
    if proof["request_id"] != terms["request_id"] || mismatch {
        return Err("The draw differs from terms saved before play".into());
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use crate::{document, proof::commit, proof::Reveal};
    use serde_json::{json, Value};

    const ID: &str = "00000000-0000-4000-8000-000000000001";

    fn validate(game: &str, terms: Value, result: Value) {
        let server_seed = "00".repeat(32);
        let client_seed = "01".repeat(32);
        let commitment = commit(game, ID, &server_seed).unwrap();
        let reveal = Reveal {
            commitment: commitment.clone(),
            server_seed,
            client_seed: client_seed.clone(),
        };
        let mut terms = terms;
        terms["request_id"] = json!(ID);
        terms["kind"] = json!(game);
        let proof = json!({"kind":game, "request_id":ID, "reveal":reveal, "result":result});
        let receipt = json!({"protocol":"bluffking-receipt-v1", "commitment":commitment, "client_seed":client_seed, "terms":terms});
        let bundle = json!({"proof":proof, "receipt":receipt});
        assert_eq!(
            document::verify_document(&bundle, None).unwrap()["receipt_match"],
            "matched"
        );
        let mut tampered = bundle.clone();
        tampered["proof"]["result"]["payout"] = json!(-1);
        assert!(document::verify_document(&tampered, None).is_err());
        tampered = bundle.clone();
        tampered["receipt"]["client_seed"] = json!("02".repeat(32));
        assert!(document::verify_document(&tampered, None).is_err());
        tampered = bundle;
        tampered["receipt"]["terms"]["request_id"] = json!("wrong");
        assert!(document::verify_document(&tampered, None).is_err());
    }

    #[test]
    fn solo_results_match_fixed_seed_vectors_and_saved_receipts() {
        let wagers = json!([{"selection":{"kind":"number","value":12},"amount":2}]);
        validate(
            "roulette",
            json!({"wagers":wagers}),
            json!({"wagers":wagers,"number":12,"bet":2,"payout":72}),
        );
        validate(
            "slots",
            json!({}),
            json!({"bet":1,"payout":0,"reels":[1,0,2],"jackpot_bonus":0}),
        );
        let guess = json!({"kind":"guess","side":"big","amount":5});
        validate(
            "fruit",
            json!({"play":guess}),
            json!({"play":guess,"bet":5,"payout":10,"number":13}),
        );
        validate(
            "claw",
            json!({"aim":{"x":0,"z":-1.1}}),
            json!({"aim":{"x":0,"z":-1.1},"caught":1,"jackpot":false,"payout":2}),
        );
        validate(
            "2048",
            json!({"cost":1}),
            json!({"cost":1,"seed":1693791216}),
        );
    }
}

#[cfg(test)]
mod tools_tests {
    use crate::{
        bean_tools, document,
        proof::{commit, sha256_fields, Reveal},
    };
    use serde_json::json;

    #[test]
    fn lucky_draw_checks_receipt_and_optionally_opens_the_private_roster() {
        let game = "lucky-draw";
        let request_id = "00000000-0000-4000-8000-000000000002";
        let server_seed = "ab".repeat(32);
        let client_seed = "cd".repeat(32);
        let commitment = commit(game, request_id, &server_seed).unwrap();
        let reveal = Reveal {
            commitment: commitment.clone(),
            server_seed,
            client_seed: client_seed.clone(),
        };
        let list = r#"["小林","Anne Marie","Maya"]"#;
        let nonce = [0x42; 32];
        let roster = bean_tools::Roster {
            size: 3,
            commitment: hex::encode(sha256_fields(&[
                b"bluffking-lucky-roster-v1",
                &nonce,
                list.as_bytes(),
            ])),
        };
        let settlement = bean_tools::lucky_draw(&roster, &mut reveal.entropy().unwrap()).unwrap();
        let winner = ["小林", "Anne Marie", "Maya"][settlement["index"].as_u64().unwrap() as usize];
        let mut bundle = json!({
            "proof": {"kind":game, "request_id":request_id, "reveal":reveal, "result":settlement},
            "receipt": {"protocol":"bluffking-receipt-v1", "commitment":commitment, "client_seed":client_seed,
                "terms":{"kind":game, "request_id":request_id, "cost":2, "roster":roster}},
        });
        let public = document::verify_document(&bundle, None).unwrap();
        assert_eq!(public["receipt_match"], "matched");
        assert!(!public.to_string().contains(winner));

        bundle["private_roster"] =
            json!({"nonce":hex::encode(nonce), "list":list, "winner":winner});
        let opened = document::verify_document(&bundle, None).unwrap();
        assert_eq!(opened["result"]["verified_roster"]["winner"], winner);
        let mut tampered = bundle.clone();
        tampered["private_roster"]["winner"] = json!("Different winner");
        assert!(document::verify_document(&tampered, None).is_err());
        tampered = bundle.clone();
        tampered["private_roster"]["list"] = json!(r#"["Anne Marie","小林","Maya"]"#);
        assert!(document::verify_document(&tampered, None).is_err());
        bundle["receipt"]["terms"]["roster"]["commitment"] = json!("0a".repeat(32));
        assert!(document::verify_document(&bundle, None).is_err());
    }
}
