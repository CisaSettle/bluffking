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
use mental_poker::pf2::{self, Bundle};

fn main() {
    let args: Vec<String> = std::env::args().collect();
    std::process::exit(run(&args));
}

fn run(args: &[String]) -> i32 {
    let (input, options) = match args.get(1).map(String::as_str) {
        Some("-h" | "--help") => {
            print_usage();
            return 0;
        }
        Some("--hand") => (args.get(2).map(String::as_str), args.get(3..)),
        input => (input, args.get(2..)),
    };
    let Some(input) = input else {
        print_usage();
        return 2;
    };
    let expected_fingerprint = match options.unwrap_or_default() {
        [] => None,
        [flag, fingerprint]
            if flag == "--party-fingerprint"
                && fingerprint.len() == 64
                && fingerprint.bytes().all(|byte| byte.is_ascii_hexdigit()) =>
        {
            Some(fingerprint.as_str())
        }
        _ => {
            print_usage();
            return 2;
        }
    };
    if input == "-" {
        verify_stdin(expected_fingerprint)
    } else {
        verify_path(input, expected_fingerprint)
    }
}

fn print_usage() {
    eprintln!("usage:");
    eprintln!(
        "  pf_verify --hand <bundle.json>   verify a saved dealing proof or encrypted-deal transcript"
    );
    eprintln!("  pf_verify -                      read the JSON from stdin");
    eprintln!("  Add --party-fingerprint <64-hex> to match an independently recorded directory fingerprint.");
}

fn verify_path(path: &str, expected_fingerprint: Option<&str>) -> i32 {
    let json = match std::fs::read_to_string(path) {
        Ok(j) => j,
        Err(e) => {
            eprintln!("error: cannot read {path}: {e}");
            return 2;
        }
    };
    verify_json(&json, expected_fingerprint)
}

fn verify_stdin(expected_fingerprint: Option<&str>) -> i32 {
    let mut buf = String::new();
    if let Err(e) = std::io::stdin().read_to_string(&mut buf) {
        eprintln!("error: cannot read stdin: {e}");
        return 2;
    }
    verify_json(&buf, expected_fingerprint)
}

fn verify_json(json: &str, expected_fingerprint: Option<&str>) -> i32 {
    let value: serde_json::Value = match serde_json::from_str(json) {
        Ok(v) => v,
        Err(e) => {
            eprintln!("error: not valid JSON: {e}");
            return 2;
        }
    };
    let value = value.get("proof").cloned().unwrap_or(value);
    if value["scheme"] == mental_poker::niuniu_transcript::SCHEME {
        use mental_poker::niuniu_transcript as niuniu;
        let result = serde_json::from_value::<niuniu::Bundle>(value)
            .map_err(|error| error.to_string())
            .and_then(|bundle| {
                niuniu::verify(&bundle)
                    .map_err(str::to_owned)
                    .map(|()| bundle)
            });
        return match result {
            Ok(bundle) if expected_fingerprint.is_none() => {
                println!(
                    "OK {}",
                    serde_json::json!({
                        "hand_id": bundle.hand_id, "seats": bundle.seats,
                        "cards": bundle.cards, "final_hash": bundle.final_hash,
                    })
                );
                0
            }
            Ok(_) => {
                eprintln!("Use the retained Niuniu final deck hash to compare this transcript.");
                2
            }
            Err(error) => {
                eprintln!("FAIL {error}");
                1
            }
        };
    }
    if value["scheme"] == mental_poker::blind_transcript::SCHEME {
        let result = serde_json::from_value::<mental_poker::blind_transcript::Bundle>(value)
            .map_err(|e| e.to_string())
            .and_then(|bundle| {
                mental_poker::blind_transcript::verify(&bundle).map_err(str::to_owned)
            });
        return match result {
            Ok(verified) => {
                if expected_fingerprint.is_some_and(|expected| {
                    !expected.eq_ignore_ascii_case(&verified.party_fingerprint)
                }) {
                    eprintln!("FAIL party-directory fingerprint mismatch");
                    return 1;
                }
                if expected_fingerprint.is_some() {
                    eprintln!("Party-directory fingerprint matched.");
                } else {
                    eprintln!("Signatures checked against the bundle's keys. Supply an independently recorded party-directory fingerprint to authenticate that directory.");
                }
                eprintln!("Verification covers supplied openings. Compare the board and shown seats with your hand record to check for omissions.");
                println!("OK {}", serde_json::to_string(&verified).unwrap());
                0
            }
            Err(error) => {
                eprintln!("FAIL {error}");
                1
            }
        };
    }
    if expected_fingerprint.is_some() {
        eprintln!("error: --party-fingerprint requires an encrypted-deal transcript");
        return 2;
    }
    if value["scheme"] == mental_poker::pf2_zjh::SCHEME {
        let result = serde_json::from_value::<mental_poker::pf2_zjh::Bundle>(value)
            .map_err(|e| e.to_string())
            .and_then(|b| mental_poker::pf2_zjh::verify(&b).map_err(|e| e.to_string()));
        return match result {
            Ok(v) => {
                println!("OK {}", serde_json::to_string(&v).unwrap());
                0
            }
            Err(e) => {
                eprintln!("FAIL {e}");
                1
            }
        };
    }
    verify_v2(value)
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
