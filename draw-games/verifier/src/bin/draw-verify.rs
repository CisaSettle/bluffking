use serde_json::{json, Value};
use std::{env, fs::File, io::Read};

fn read(path: &str) -> Result<Value, String> {
    let mut data = Vec::new();
    File::open(path)
        .map_err(|e| e.to_string())?
        .take(1_048_577)
        .read_to_end(&mut data)
        .map_err(|e| e.to_string())?;
    if data.len() > 1_048_576 {
        return Err("Proof file exceeds 1 MiB".into());
    }
    serde_json::from_slice(&data).map_err(|e| e.to_string())
}

fn run() -> Result<Value, String> {
    let args: Vec<String> = env::args().skip(1).collect();
    if args.len() == 3 && args[0] == "--verify-lottery-purchase" {
        let evidence = serde_json::from_value(read(&args[1])?).map_err(|e| e.to_string())?;
        let receipt = draw_games::lottery::PurchaseReceipt::verify(&evidence, &args[2])?;
        return serde_json::to_value(receipt).map_err(|e| e.to_string());
    }
    if args.len() == 4 && args[0] == "--derive-lottery" {
        let game = draw_games::lottery::Game::parse(&args[1]).ok_or("Unknown lottery game")?;
        let period: i64 = args[2].parse().map_err(|_| "Invalid period number")?;
        let pulse = read(&args[3])?;
        let beacon = draw_games::beacon::Beacon {
            round: pulse["round"].as_u64().ok_or("Missing beacon round")?,
            signature: pulse["signature"]
                .as_str()
                .ok_or("Missing beacon signature")?
                .into(),
        };
        let proof = draw_games::lottery::DrawProof::new(
            draw_games::lottery::Plan::new(game, period)?,
            beacon,
        )?;
        return serde_json::to_value(proof).map_err(|e| e.to_string());
    }
    let usage = "Usage: draw-verify proof.json [--receipt pre-draw-receipt.json] [--evidence-key TRUSTED_KEY_HEX]\n       draw-verify --derive-lottery GAME PERIOD beacon.json\n       draw-verify --verify-lottery-purchase receipt.json TRUSTED_KEY_HEX";
    let (path, options) = args.split_first().ok_or(usage)?;
    if options.len() % 2 != 0 {
        return Err(usage.into());
    }
    let mut external_receipt = None;
    let mut pinned_key = None;
    for pair in options.chunks_exact(2) {
        match pair[0].as_str() {
            "--receipt" if external_receipt.is_none() => external_receipt = Some(read(&pair[1])?),
            "--evidence-key" if pinned_key.is_none() => pinned_key = Some(pair[1].as_str()),
            _ => return Err(usage.into()),
        }
    }
    let document = read(path)?;
    draw_games::document::verify_document_with_key(&document, external_receipt.as_ref(), pinned_key)
}

fn main() {
    match run() {
        Ok(result) => println!("{}", serde_json::to_string_pretty(&result).unwrap()),
        Err(error) => {
            eprintln!("{}", json!({"outcome":"rejected","error":error}));
            std::process::exit(1);
        }
    }
}
