//! ADR-064 §6.2 / §7 — conformance test for the commit-reveal verifier.
//!
//! Mirrors the spec's "generate a real hand, then verify it" requirement: we
//! deal a genuine `engine` hand from a derived `deck_seed` (the exact path the
//! server's `pf_dealing` uses), serialize a `HandRecord` shaped like the
//! `dump_hand_detail` fixture, then assert `pf::verify_hand` accepts it and
//! rejects mutated variants. Fixtures are generated from real engine output,
//! never hand-authored (per [[lock-interface-spec-first]]).

use std::collections::BTreeMap;

use engine::{Card, Chips, Deck, GameHand, PlayerId, PokerRng};
use mental_poker::hash::ds_hash;
use mental_poker::pf::{derive_deck_seed, verify_hand, HandRecord, DS_SEED_COMMIT};

/// Deal a real hand from `(server_seed, client_seeds, hand_id)` and build the
/// JSON-shaped record a verifier would receive from `dump_hand_detail`.
fn record_for(
    server_seed: [u8; 32],
    client_seeds: BTreeMap<u8, [u8; 32]>,
    hand_id: uuid::Uuid,
    n: usize,
) -> HandRecord {
    let deck_seed = derive_deck_seed(&server_seed, &client_seeds, hand_id.as_bytes());

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
    hand.start().expect("hand start");

    let snap = hand.snapshot();
    let mut hole: BTreeMap<String, Vec<Card>> = BTreeMap::new();
    for p in &snap.players {
        if let Some(hc) = p.hole_cards {
            hole.insert(p.seat.to_string(), vec![hc.card1, hc.card2]);
        }
    }

    // Board = the next 5 cards after the 2*n hole cards (no burns).
    let mut rng = PokerRng::from_seed_bytes(deck_seed);
    let deck = Deck::new(&mut rng);
    let cards = deck.cards();
    let board: Vec<Card> = (0..5).map(|i| cards[2 * n + i]).collect();

    let client_seeds_hex: BTreeMap<String, String> = client_seeds
        .iter()
        .map(|(s, seed)| (s.to_string(), hex::encode(seed)))
        .collect();

    HandRecord {
        hand_id: hand_id.to_string(),
        server_seed: hex::encode(server_seed),
        seed_commit: hex::encode(ds_hash(DS_SEED_COMMIT, &[&server_seed])),
        client_seeds: client_seeds_hex,
        num_players: n,
        // Contiguous fixture: deal_pos == seat (seats 0..n).
        dealt_seats: (0..n as u8).collect(),
        hole_cards: hole,
        community: board,
    }
}

/// Round-trip via JSON (proves the wire/fixture shape deserializes), then verify.
#[test]
fn pf_verify_accepts_a_real_hand_round_tripped_through_json() {
    let rec = record_for(
        [0x5A; 32],
        BTreeMap::new(),
        uuid::Uuid::from_bytes([0x42; 16]),
        6,
    );
    let json = serde_json::to_string(&serde_json::json!({
        "hand_id": rec.hand_id,
        "server_seed": rec.server_seed,
        "seed_commit": rec.seed_commit,
        "client_seeds": rec.client_seeds,
        "num_players": rec.num_players,
        "hole_cards": rec.hole_cards,
        "community": rec.community,
    }))
    .expect("serialize fixture");

    let parsed: HandRecord = serde_json::from_str(&json).expect("fixture deserializes");
    let report = verify_hand(&parsed).expect("real hand must verify");
    assert!(report.hole_seats_checked >= 6);
    assert_eq!(report.board_cards_checked, 5);
}

#[test]
fn pf_verify_accepts_hand_with_client_entropy() {
    let mut cs = BTreeMap::new();
    cs.insert(1u8, [0xE1; 32]);
    cs.insert(4u8, [0xE4; 32]);
    let rec = record_for([0x33; 32], cs, uuid::Uuid::from_bytes([0x77; 16]), 5);
    verify_hand(&rec).expect("hand with client seeds must verify");
}

#[test]
fn pf_verify_rejects_mutated_seed() {
    let mut rec = record_for(
        [0x5A; 32],
        BTreeMap::new(),
        uuid::Uuid::from_bytes([0x42; 16]),
        6,
    );
    let mut bytes = hex::decode(&rec.server_seed).unwrap();
    bytes[31] ^= 0x01;
    rec.server_seed = hex::encode(bytes);
    let err = verify_hand(&rec).expect_err("mutated seed must be rejected");
    assert!(err.0.contains("commit mismatch"), "got: {}", err.0);
}

#[test]
fn pf_verify_rejects_partial_record_with_only_user_seat() {
    // The per-user `hands` row persists only that user's hole cards. The verifier
    // must still anchor on the board + that one seat. Build the full record, then
    // strip hole_cards down to a single seat — verification must still pass.
    let mut rec = record_for(
        [0x10; 32],
        BTreeMap::new(),
        uuid::Uuid::from_bytes([0x99; 16]),
        4,
    );
    let only: BTreeMap<String, Vec<Card>> = rec
        .hole_cards
        .iter()
        .filter(|(k, _)| k.as_str() == "2")
        .map(|(k, v)| (k.clone(), v.clone()))
        .collect();
    assert_eq!(only.len(), 1, "fixture should have seat 2");
    rec.hole_cards = only;
    let report = verify_hand(&rec).expect("single-seat record must still verify");
    assert_eq!(report.hole_seats_checked, 1);
    assert_eq!(report.board_cards_checked, 5);
}
