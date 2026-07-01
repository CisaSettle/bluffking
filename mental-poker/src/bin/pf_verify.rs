//! `pf_verify` — offline commit-reveal provably-fair verifier (ADR-064).
//!
//! Recomputes the deck of any `existing_server` hand from its revealed secrets
//! and confirms it matches the persisted cards. Reads a JSON `HandRecord`.
//!
//! For a working, end-to-end verifiable fixture, use the sibling demo binary
//! (`dump_hand_detail`'s pf fields are illustrative-only and do NOT verify):
//!
//! ```text
//! cargo run -p mental-poker --bin pf_demo_hand \
//!   | cargo run -p mental-poker --bin pf_verify -- -      # → OK ✓
//!
//! pf_verify --hand path/to/hand.json     verify a persisted hand
//! pf_verify -                            read the JSON record from stdin
//! ```
//!
//! Exit code 0 = verified, 1 = verification failed (or no commit-reveal data),
//! 2 = usage / IO / parse error.
//!
//! This does NOT make the server blind (the server always saw the cards). It
//! proves the deck was committed before the deal and not altered after — i.e.
//! "可验证公平 / provably fair (verify any hand)". Where a human contributed a
//! client_seed, it additionally proves the server could not grind the shuffle.

use std::io::Read;

use mental_poker::pf::{verify_hand, HandRecord};

fn main() {
    let args: Vec<String> = std::env::args().collect();
    let code = match args.get(1).map(String::as_str) {
        Some("--hand") => match args.get(2) {
            Some(path) => verify_path(path),
            None => {
                print_usage();
                2
            }
        },
        Some("-") => verify_stdin(),
        Some("-h") | Some("--help") | None => {
            print_usage();
            2
        }
        // Bare path argument for convenience.
        Some(path) => verify_path(path),
    };
    std::process::exit(code);
}

fn print_usage() {
    eprintln!("usage:");
    eprintln!("  pf_verify --hand <hand.json>   verify a persisted commit-reveal hand");
    eprintln!("  pf_verify -                    read the JSON hand record from stdin");
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
    let rec: HandRecord = match serde_json::from_str(json) {
        Ok(r) => r,
        Err(e) => {
            eprintln!("error: not a valid hand record: {e}");
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
            println!("OK: deck reproduced, commit verified ✓ (provably fair)");
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
            println!("REJECTED ✗");
            println!("  {e}");
            1
        }
    }
}
