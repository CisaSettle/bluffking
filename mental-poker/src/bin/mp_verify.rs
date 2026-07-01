//! `mp-verify` — offline Mental Poker transcript verifier.
//!
//! ```text
//! mp-verify <transcript.json>       verify an exported transcript
//! mp-verify --demo                  deal a sample hand, verify it
//! mp-verify --demo <out.json>       ...and also write the transcript to a file
//! ```
//!
//! Exit codes:
//!   0 = verified AND provably fair (real audited crypto)
//!   1 = verification failed (the transcript does not replay consistently)
//!   2 = usage / IO error
//!   3 = replays consistently but NOT provably fair (dev-only mock crypto)
//!
//! BUG-108 (fail-closed): a dev-mock transcript replays consistently, so it is
//! NOT exit 1 — but it is NOT a provable-fairness guarantee either, so it exits
//! 3 (a distinct non-zero code) rather than 0. A consumer that gates on exit 0
//! therefore can never treat a mock replay as a passed fairness check.

use engine::Card;
use mental_poker::card_id::id_to_card;
use mental_poker::{
    verify, DealRequest, DealingProvider, MentalPokerDealingProvider, SchemeSoundness, Transcript,
};

fn main() {
    let args: Vec<String> = std::env::args().collect();
    let code = match args.get(1).map(String::as_str) {
        Some("--demo") => run_demo(args.get(2).map(String::as_str)),
        Some("-h") | Some("--help") | None => {
            print_usage();
            2
        }
        Some(path) => verify_file(path),
    };
    std::process::exit(code);
}

fn print_usage() {
    eprintln!("usage:");
    eprintln!("  mp-verify <transcript.json>     verify an exported transcript");
    eprintln!("  mp-verify --demo [out.json]     deal a sample hand and verify it");
}

fn run_demo(out_path: Option<&str>) -> i32 {
    let request = DealRequest {
        hand_id: "demo-hand-0001".to_string(),
        table_id: "demo-table".to_string(),
        num_players: 3,
        button_seat: 0,
        big_blind: 20,
        small_blind: 10,
    };
    let provider = MentalPokerDealingProvider::deterministic();
    let dealt = provider.deal(&request);
    let transcript = dealt
        .transcript
        .expect("mental poker provider always produces a transcript");

    println!(
        "dealt {}-player hand '{}'",
        request.num_players, request.hand_id
    );
    println!("  provider           : {}", provider.name());
    println!("  transcript events  : {}", transcript.events.len());
    println!("  deck (deal order)  : {}", render_deck(&dealt.deck));

    if let Some(path) = out_path {
        match std::fs::write(path, transcript.to_json()) {
            Ok(()) => println!("  exported transcript: {path}"),
            Err(e) => {
                eprintln!("error: could not write {path}: {e}");
                return 2;
            }
        }
    }

    report(&transcript)
}

fn verify_file(path: &str) -> i32 {
    let json = match std::fs::read_to_string(path) {
        Ok(j) => j,
        Err(e) => {
            eprintln!("error: cannot read {path}: {e}");
            return 2;
        }
    };
    let transcript = match Transcript::from_json(&json) {
        Ok(t) => t,
        Err(e) => {
            eprintln!("error: {path} is not a valid transcript: {e}");
            return 2;
        }
    };
    report(&transcript)
}

fn report(transcript: &Transcript) -> i32 {
    match verify(transcript) {
        Ok(r) => {
            // BUG-108: a `verify()` Ok on a DEV-ONLY MOCK transcript (the
            // production `mental_poker_prefer` path: mock-shuffle-v1 /
            // mock-decrypt-v1 / mock signing) means only that the transcript
            // REPLAYS consistently — the mock proofs do NOT prove a true shuffle
            // or honest decryption, so it is NOT a provable-fairness guarantee.
            // Never present such a result as "✓" without the loud caveat.
            // BUG-108 (fail-closed): the exit code — the machine-readable verdict
            // automation gates on — must distinguish a SOUND deal (0) from a
            // dev-mock replay (3). The loud printed caveat below is for humans;
            // the exit code is for scripts/exports, so a mock replay is NEVER 0.
            let code = match r.soundness {
                SchemeSoundness::Sound => {
                    println!("VERIFIED ✓");
                    0
                }
                SchemeSoundness::DevMock => {
                    println!("VERIFIED (replay only) ⚠");
                    println!();
                    println!(
                        "  ⚠ DEV-ONLY MOCK CRYPTO — this is NOT a provable-fairness guarantee."
                    );
                    println!(
                        "    Schemes: shuffle='{}' decryption='{}'{}.",
                        transcript.shuffle_scheme,
                        transcript.decryption_scheme,
                        if transcript.key_directory.is_mock {
                            " signing=mock"
                        } else {
                            ""
                        }
                    );
                    println!(
                        "    The transcript replays consistently, but the mock proofs do NOT prove"
                    );
                    println!(
                        "    the deck was a true shuffle/permutation or that decryptions were honest,"
                    );
                    println!("    so they CANNOT prove the deal was not rigged by the server.");
                    println!("    Exiting 3 (NOT provably fair) — do not treat this as verified.");
                    println!();
                    3
                }
            };
            println!("  events checked : {}", r.events_checked);
            println!("  players        : {}", r.num_players);
            println!("  final phase    : {:?}", r.final_phase);
            println!(
                "  cards revealed : {} ({})",
                r.revealed_card_ids.len(),
                render_ids(&r.revealed_card_ids),
            );
            code
        }
        Err(e) => {
            println!("REJECTED ✗");
            println!("  {e}");
            1
        }
    }
}

fn render_cards(cards: impl IntoIterator<Item = Card>) -> String {
    cards
        .into_iter()
        .map(|c| c.to_string())
        .collect::<Vec<_>>()
        .join(" ")
}

fn render_deck(deck: &[Card]) -> String {
    render_cards(deck.iter().copied())
}

fn render_ids(ids: &[u8]) -> String {
    render_cards(ids.iter().filter_map(|&id| id_to_card(id)))
}
