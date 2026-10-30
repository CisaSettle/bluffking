//! `pf_demo_hand` — emit a REAL, end-to-end verifiable `pf-shuffle-v2`
//! verification bundle (ADR-064 v2), so `pf_verify` has a working, shipped
//! fixture source.
//!
//! It locks a deck from a fixed master seed exactly as the dealer does, mixes in
//! fixed player seeds, and opens what a participant of a showdown would see: the
//! five board cards and every dealt seat's hole cards. Pipe it straight into the
//! verifier:
//!
//! ```text
//! cargo run -p mental-poker --bin pf_demo_hand \
//!   | cargo run -p mental-poker --bin pf_verify -- -
//! # → OK: shuffle proof and every opened card verified (pf-shuffle-v2)
//! ```
//!
//! The deck, commitment and order are deterministic (fixed seed + hand_id); the
//! proofs use fresh OS randomness on every run, as in production, so the JSON
//! bytes differ between runs while the proven cards stay the same.

use std::collections::BTreeMap;

use mental_poker::pf2::{board_positions, hole_positions, LockedDeck};
use rand::rngs::OsRng;

fn main() {
    // Fixed, illustrative-but-REAL inputs. A production bundle comes from
    // `GET /api/history/hands-fairness?id=<uuid>` instead.
    let master_seed = [0x11u8; 32];
    let hand_id = uuid::Uuid::from_bytes([0xABu8; 16]);
    let dealt_seats: Vec<u8> = vec![0, 2, 5];
    let client_seeds: BTreeMap<u8, [u8; 32]> =
        BTreeMap::from([(0, [0x22u8; 32]), (5, [0x33u8; 32])]);
    let withheld_seats: Vec<u8> = Vec::new();

    let deck = LockedDeck::new(&master_seed, hand_id);
    let mut positions: Vec<usize> = board_positions(&dealt_seats).to_vec();
    for &seat in &dealt_seats {
        positions.extend(hole_positions(&dealt_seats, seat).expect("dealt seat"));
    }
    let bundle = deck
        .bundle(
            &client_seeds,
            &dealt_seats,
            &withheld_seats,
            &positions,
            &mut OsRng,
        )
        .expect("build bundle");

    println!(
        "{}",
        serde_json::to_string_pretty(&bundle).expect("serialize bundle")
    );
}
