//! `pf_demo_hand` — emit a REAL, end-to-end verifiable commit-reveal hand record
//! (ADR-064), so `pf_verify` has a working, shipped fixture source.
//!
//! Unlike `server/examples/dump_hand_detail` (whose pf fields are illustrative
//! placeholders in a different — replay-mock — JSON shape, and therefore do NOT
//! satisfy the verifier), this binary deals a *real* engine hand from a fixed
//! `server_seed`, captures the actual hole cards + board, and prints a
//! `HandRecord` whose `seed_commit`, `deck_seed`, and cards all reproduce. Pipe
//! it straight into the verifier to prove the dealing was fair:
//!
//! ```text
//! cargo run -p mental-poker --bin pf_demo_hand \
//!   | cargo run -p mental-poker --bin pf_verify -- -
//! # → OK: deck reproduced, commit verified ✓ (provably fair)
//! ```
//!
//! It is deterministic (fixed seed + hand_id) so it doubles as a stable golden
//! fixture. This is the SERVER-COMMIT-ONLY case (no client entropy); a hand with
//! a human client_seed would carry a non-empty `client_seeds` map, which the
//! verifier folds into `deck_seed` identically.

use std::collections::BTreeMap;

use engine::{Card, Chips, Deck, GameHand, PlayerId, PokerRng};
use mental_poker::hash::ds_hash;
use mental_poker::pf::{derive_deck_seed, HandRecord, DS_SEED_COMMIT};

fn main() {
    // Fixed, illustrative-but-REAL inputs (deterministic golden). A production
    // dump would substitute the persisted `server_seed`/`hand_id` of the hand.
    let server_seed = [0x11u8; 32];
    let hand_id = uuid::Uuid::from_bytes([0xABu8; 16]);
    let client_seeds: BTreeMap<u8, [u8; 32]> = BTreeMap::new(); // server-commit-only
    let n: usize = 3;

    let hid = *hand_id.as_bytes();
    let deck_seed = derive_deck_seed(&server_seed, &client_seeds, &hid);

    // Deal a real engine hand from this exact seed — mirrors the server's
    // `existing_server` path (GameHand::new_with_rng + start()).
    let players: Vec<(PlayerId, Chips, u8)> = (0..n)
        .map(|i| (PlayerId::new(i as u64), Chips(1000), i as u8))
        .collect();
    let mut hand = GameHand::new_with_rng(
        players,
        0,
        Chips(20),
        Chips(10),
        PokerRng::from_seed_bytes(deck_seed),
    );
    hand.start().expect("deal a real hand");

    // Capture the actual hole cards the engine dealt, keyed by seat.
    let snap = hand.snapshot();
    let mut hole_cards: BTreeMap<String, Vec<Card>> = BTreeMap::new();
    for p in &snap.players {
        if let Some(hc) = p.hole_cards {
            hole_cards.insert(p.seat.to_string(), vec![hc.card1, hc.card2]);
        }
    }

    // The full 5-card board (deal order: flop[0..3], turn[3], river[4]) lives at
    // deck positions 2n..2n+5 — read it straight off the reproduced deck.
    let mut rng2 = PokerRng::from_seed_bytes(deck_seed);
    let deck = Deck::new(&mut rng2);
    let cards = deck.cards();
    let community: Vec<Card> = (0..5).map(|i| cards[2 * n + i]).collect();

    let client_seeds_hex: BTreeMap<String, String> = client_seeds
        .iter()
        .map(|(s, seed)| (s.to_string(), hex::encode(seed)))
        .collect();

    let record = HandRecord {
        hand_id: hand_id.to_string(),
        server_seed: hex::encode(server_seed),
        seed_commit: hex::encode(ds_hash(DS_SEED_COMMIT, &[&server_seed])),
        client_seeds: client_seeds_hex,
        num_players: n,
        // Contiguous-from-0 table: deal position == seat.
        dealt_seats: (0..n as u8).collect(),
        hole_cards,
        community,
    };

    println!(
        "{}",
        serde_json::to_string_pretty(&record).expect("serialize HandRecord")
    );
}
