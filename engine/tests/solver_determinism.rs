//! ADR-043 §6 — same input ⇒ byte-identical output across calls.
//!
//! MANDATORY: cache stability invariant.

use engine::card::{Card, Rank, Suit};
use engine::hand::{BoardCards, HoleCards, Street};
use engine::player::Position;
use engine::solver::{analyze, PreflopAction, SolverAction, SolverInput, TableSize};

fn build_input(seed: u64) -> SolverInput {
    SolverInput {
        street: Street::Flop,
        position: Position::Dealer,
        table_size: TableSize::SixMax,
        hero: HoleCards::new(
            Card::new(Rank::Ace, Suit::Spades),
            Card::new(Rank::King, Suit::Spades),
        ),
        board: BoardCards {
            flop: Some([
                Card::new(Rank::Ace, Suit::Hearts),
                Card::new(Rank::Seven, Suit::Clubs),
                Card::new(Rank::Two, Suit::Diamonds),
            ]),
            turn: None,
            river: None,
        },
        pot_before: 200,
        to_call: 80,
        stack_before: 1500,
        num_players_in_hand: 2,
        last_aggressor_seat: Some(0),
        hero_seat: 0,
        hero_action_taken: Some(SolverAction::Raise),
        preflop_action: None,
        actions_so_far_count: 10,
        seed,
    }
}

#[test]
fn same_seed_byte_identical_output() {
    let input = build_input(42);
    let a = analyze(&input).unwrap();
    let b = analyze(&input).unwrap();
    assert_eq!(a.verdict.as_str(), b.verdict.as_str());
    assert_eq!(a.gto_action.as_str(), b.gto_action.as_str());
    assert_eq!(a.equity_estimate_pct, b.equity_estimate_pct);
    assert_eq!(a.reasoning_zh, b.reasoning_zh);
}

#[test]
fn many_seeds_each_deterministic() {
    // For 20 distinct seeds, each input ⇒ same output on two calls.
    // (Capped at 20 to keep per-test wall time < 30 s — each analyze runs a
    // 10k MC sim. The byte-stability invariant is the same regardless of count.)
    for seed in 0u64..20 {
        let input = build_input(seed);
        let a = analyze(&input).unwrap();
        let b = analyze(&input).unwrap();
        assert_eq!(
            a.reasoning_zh, b.reasoning_zh,
            "seed={seed}: reasoning_zh diverged"
        );
        assert_eq!(a.gto_action.as_str(), b.gto_action.as_str());
        assert_eq!(a.equity_estimate_pct, b.equity_estimate_pct);
    }
}

#[test]
fn different_seed_may_produce_different_reasoning() {
    // Sanity: at least SOME of the seeds produce visibly different output —
    // verifies that the seed actually plumbs through (template variant or MC noise).
    let mut distinct: std::collections::HashSet<String> = std::collections::HashSet::new();
    for seed in 0u64..30 {
        let input = build_input(seed);
        let out = analyze(&input).unwrap();
        distinct.insert(out.reasoning_zh.clone());
    }
    assert!(
        distinct.len() >= 2,
        "expected ≥2 distinct outputs across 30 seeds, got {}",
        distinct.len()
    );
}

#[test]
fn preflop_determinism() {
    let mut input = build_input(7);
    input.street = Street::Preflop;
    input.board = BoardCards::empty();
    input.preflop_action = Some(PreflopAction::Rfi);
    input.position = Position::Utg;
    input.hero = HoleCards::new(
        Card::new(Rank::Ace, Suit::Spades),
        Card::new(Rank::Ace, Suit::Hearts),
    );
    input.to_call = 0;
    let a = analyze(&input).unwrap();
    let b = analyze(&input).unwrap();
    assert_eq!(a.gto_action.as_str(), b.gto_action.as_str());
    assert_eq!(a.reasoning_zh, b.reasoning_zh);
}
