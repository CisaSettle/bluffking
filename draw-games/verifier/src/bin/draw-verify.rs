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
    if args.len() != 1 && !(args.len() == 3 && args[1] == "--receipt") {
        return Err("Usage: draw-verify proof.json [--receipt pre-draw-receipt.json]\n       draw-verify --derive-lottery GAME PERIOD beacon.json".into());
    }
    let document = read(&args[0])?;
    let external_receipt = if args.len() == 3 {
        Some(read(&args[2])?)
    } else {
        None
    };
    draw_games::document::verify_document(&document, external_receipt.as_ref())
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
