//! `pf_verify` — offline verifier for Hold'em verifiable dealing (ADR-064).
//!
//! **v2 (`pf-shuffle-v2`, current).** Reads the verification bundle a
//! participant gets from `GET /api/history/hands-fairness?id=<uuid>` and checks it with
//! [`mental_poker::pf2::verify`]: the commitment opens to the dealer key and the
//! encrypted deck; the shuffle proof shows that deck is exactly one standard
//! 52-card deck (input pinned to the self-computed standard deck); the deal order
//! is recomputed from the public player seeds; and every opened card carries a
//! valid decryption proof at the position the verifier derives itself. Prints
//! the proven board and hole cards. Cards that were never opened stay encrypted.
//!
//! **v1 (legacy record).** A JSON `HandRecord` with a revealed `server_seed`
//! (hands dealt before v2) is still recomputed with [`mental_poker::pf`].
//!
//! ```text
//! cargo run -p mental-poker --bin pf_demo_hand \
//!   | cargo run -p mental-poker --bin pf_verify -- -      # → OK
//!
//! pf_verify --hand path/to/bundle.json   verify a saved bundle / record
//! pf_verify -                            read the JSON from stdin
//! ```
//!
//! Exit code 0 = verified, 1 = verification failed (or no commit-reveal data),
//! 2 = usage / IO / parse error.
//!
//! A passing check proves the opened cards came from one standard deck that was
//! locked before the player seeds were known; it does not make the dealer blind
//! (the dealer shuffled the deck and knows it).

use std::io::Read;

use mental_poker::card_id::id_to_card;
use mental_poker::pf::{verify_hand, HandRecord};
use mental_poker::pf2::{self, Bundle};

fn main() {
    let args: Vec<String> = std::env::args().collect();
    std::process::exit(run(&args));
}

fn run(args: &[String]) -> i32 {
    match args.get(1).map(String::as_str) {
        Some("--hand") => match args.get(2) {
            Some(path) => verify_path(path),
            None => {
                print_usage();
                2
            }
        },
        Some("-") => verify_stdin(),
        // U66 (dual-AI OSS review): explicitly-requested help is a SUCCESS
        // (exit 0); only a genuine usage error (no argument, or `--hand`
        // missing its path above) exits 2.
        Some("-h") | Some("--help") => {
            print_usage();
            0
        }
        None => {
            print_usage();
            2
        }
        // Bare path argument for convenience.
        Some(path) => verify_path(path),
    }
}

fn print_usage() {
    eprintln!("usage:");
    eprintln!(
        "  pf_verify --hand <bundle.json>   verify a saved verification bundle (pf-shuffle-v2)"
    );
    eprintln!("                                   or a legacy v1 hand record");
    eprintln!("  pf_verify -                      read the JSON from stdin");
}

fn verify_path(path: &str) -> i32 {
    let json = match std::fs::read_to_string(path) {
        Ok(j) => j,
        Err(e) => {
            eprintln!("error: cannot read {path}: {e}");
            return 2;
        }
    };
    verify_json(&json)
}

fn verify_stdin() -> i32 {
    let mut buf = String::new();
    if let Err(e) = std::io::stdin().read_to_string(&mut buf) {
        eprintln!("error: cannot read stdin: {e}");
        return 2;
    }
    verify_json(&buf)
}

fn verify_json(json: &str) -> i32 {
    let value: serde_json::Value = match serde_json::from_str(json) {
        Ok(v) => v,
        Err(e) => {
            eprintln!("error: not valid JSON: {e}");
            return 2;
        }
    };
    if value.get("scheme").is_some() {
        verify_v2(value)
    } else {
        verify_v1(value)
    }
}

fn card_str(id: Option<u8>) -> String {
    match id.and_then(id_to_card) {
        Some(card) => card.to_string(),
        None => "??".to_string(),
    }
}

fn verify_v2(value: serde_json::Value) -> i32 {
    let bundle: Bundle = match serde_json::from_value(value) {
        Ok(b) => b,
        Err(e) => {
            eprintln!("error: not a valid {} bundle: {e}", pf2::SCHEME);
            return 2;
        }
    };
    match pf2::verify(&bundle) {
        Ok(v) => {
            println!(
                "OK: shuffle proof and every opened card verified ({})",
                pf2::SCHEME
            );
            println!("  hand_id        : {}", v.hand_id);
            println!("  commitment     : {}", v.commit);
            println!("  dealt seats    : {:?}", v.dealt_seats);
            let seeded: Vec<&str> = v.client_seeds.keys().map(String::as_str).collect();
            println!("  player seeds   : {} (seats {:?})", seeded.len(), seeded);
            if !v.withheld_seats.is_empty() {
                println!(
                    "  WITHHELD seats : {:?} (committed but did not reveal)",
                    v.withheld_seats
                );
            }
            let board: Vec<String> = v.board.iter().map(|c| card_str(*c)).collect();
            println!("  board          : {}", board.join(" "));
            for (seat, [a, b]) in &v.holes {
                if a.is_some() || b.is_some() {
                    println!("  seat {seat:<10}: {} {}", card_str(*a), card_str(*b));
                }
            }
            println!(
                "  opened cards   : {} of 52 (?? = not opened, stays encrypted)",
                v.cards.len()
            );
            0
        }
        Err(e) => {
            println!("REJECTED");
            println!("  {e}");
            1
        }
    }
}

fn verify_v1(value: serde_json::Value) -> i32 {
    let rec: HandRecord = match serde_json::from_value(value) {
        Ok(r) => r,
        Err(e) => {
            eprintln!(
                "error: not a {} bundle or a valid v1 hand record: {e}",
                pf2::SCHEME
            );
            return 2;
        }
    };
    if rec.server_seed.is_empty() {
        // Legacy / MP-dealt hand: no commit-reveal secrets to verify.
        println!(
            "SKIPPED: hand {} has no server_seed (not a commit-reveal hand)",
            rec.hand_id
        );
        return 1;
    }
    match verify_hand(&rec) {
        Ok(report) => {
            println!("OK (legacy v1 record): deck reproduced, commit verified");
            println!("  hand_id           : {}", rec.hand_id);
            println!("  recomputed deck_seed: {}", report.deck_seed_hex);
            println!("  hole seats checked : {}", report.hole_seats_checked);
            println!("  board cards checked: {}", report.board_cards_checked);
            println!(
                "  client seeds mixed : {} (anti-grind when >0)",
                rec.client_seeds.len()
            );
            0
        }
        Err(e) => {
            println!("REJECTED");
            println!("  {e}");
            1
        }
    }
}
