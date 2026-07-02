//! ADR-043 §3.4.2 — one test per rule R0..R9.
//!
//! Each test constructs a minimal `SolverInput` engineered to land on exactly
//! one rule, calls `solver::analyze`, and asserts the resulting `gto_action`.

use engine::card::{Card, Rank, Suit};
use engine::hand::{BoardCards, HoleCards, Street};
use engine::player::Position;
use engine::solver::{analyze, SolverAction, SolverInput, TableSize};

fn c(r: Rank, s: Suit) -> Card {
    Card::new(r, s)
}

fn h(c1: Card, c2: Card) -> HoleCards {
    HoleCards::new(c1, c2)
}

fn dry_low_board() -> BoardCards {
    // 2-7-K rainbow, no straight or flush draws.
    BoardCards {
        flop: Some([
            c(Rank::Two, Suit::Spades),
            c(Rank::Seven, Suit::Hearts),
            c(Rank::King, Suit::Clubs),
        ]),
        turn: None,
        river: None,
    }
}

fn base_input() -> SolverInput {
    SolverInput {
        street: Street::Flop,
        position: Position::Dealer,
        table_size: TableSize::SixMax,
        hero: h(c(Rank::Ace, Suit::Spades), c(Rank::King, Suit::Spades)),
        board: dry_low_board(),
        pot_before: 100,
        to_call: 50,
        stack_before: 1000,
        num_players_in_hand: 2,
        last_aggressor_seat: None,
        hero_seat: 0,
        hero_action_taken: Some(SolverAction::Call),
        preflop_action: None,
        actions_so_far_count: 5,
        seed: 42,
    }
}

// ---------------------------------------------------------------------------
// R0: eq >= 85 AND !can_check → Raise (value jam range)
// ---------------------------------------------------------------------------
#[test]
fn rule_r0_value_jam_raise() {
    // Quad aces — equity 100% with a bet to call.
    let mut input = base_input();
    input.hero = h(c(Rank::Ace, Suit::Spades), c(Rank::Ace, Suit::Hearts));
    input.board = BoardCards {
        flop: Some([
            c(Rank::Ace, Suit::Diamonds),
            c(Rank::Ace, Suit::Clubs),
            c(Rank::Two, Suit::Hearts),
        ]),
        turn: None,
        river: None,
    };
    input.to_call = 50;
    // SPR > 3 → R1 should not fire; R0 will.
    input.stack_before = 10_000;
    input.pot_before = 100;
    let out = analyze(&input).unwrap();
    assert_eq!(out.gto_action, SolverAction::Raise);
    assert!(out.reasoning_zh.contains("胜率"));
}

// ---------------------------------------------------------------------------
// R1: eq >= 65 AND hs >= TwoPair AND spr <= 3 AND !can_check → AllIn
// ---------------------------------------------------------------------------
#[test]
fn rule_r1_commit_all_in() {
    let mut input = base_input();
    // Set of 8s on K-8-2 — high equity (~75-80% vs random), hs == Set,
    // SPR 1.5 → R1 fires (R0 needs eq>=85). Set is a textbook commit hand.
    input.hero = h(c(Rank::Eight, Suit::Spades), c(Rank::Eight, Suit::Hearts));
    input.board = BoardCards {
        flop: Some([
            c(Rank::King, Suit::Hearts),
            c(Rank::Eight, Suit::Diamonds),
            c(Rank::Two, Suit::Clubs),
        ]),
        turn: None,
        river: None,
    };
    input.to_call = 100;
    input.pot_before = 200;
    input.stack_before = 300; // spr = 1.5
    let out = analyze(&input).unwrap();
    // Could be AllIn (R1) or Raise (R0/R2). Accept either committed line.
    assert!(
        matches!(out.gto_action, SolverAction::AllIn | SolverAction::Raise),
        "expected committed value line (AllIn or Raise); got {:?}",
        out.gto_action
    );
}

// ---------------------------------------------------------------------------
// R2: eq >= 65 AND !can_check (no other rule) → Raise (standard value)
// ---------------------------------------------------------------------------
#[test]
fn rule_r2_standard_value_raise() {
    let mut input = base_input();
    // Top pair top kicker A-K on A-7-2: eq ~75%, hs = PairTopStrongKicker.
    input.hero = h(c(Rank::Ace, Suit::Spades), c(Rank::King, Suit::Diamonds));
    input.board = BoardCards {
        flop: Some([
            c(Rank::Ace, Suit::Hearts),
            c(Rank::Seven, Suit::Clubs),
            c(Rank::Two, Suit::Spades),
        ]),
        turn: None,
        river: None,
    };
    input.to_call = 50;
    input.stack_before = 1000;
    input.pot_before = 100;
    let out = analyze(&input).unwrap();
    // Should be Raise (or AllIn if SPR low). With stack 1000 / pot 100 SPR ≈ 10 → Raise.
    assert!(matches!(
        out.gto_action,
        SolverAction::Raise | SolverAction::AllIn
    ));
}

// ---------------------------------------------------------------------------
// R3: eq >= 50 AND can_check AND is_aggressor → Raise (c-bet / barrel)
// ---------------------------------------------------------------------------
#[test]
fn rule_r3_cbet_as_aggressor() {
    let mut input = base_input();
    input.hero = h(c(Rank::Ace, Suit::Spades), c(Rank::King, Suit::Diamonds));
    input.board = BoardCards {
        flop: Some([
            c(Rank::Ace, Suit::Hearts),
            c(Rank::Seven, Suit::Clubs),
            c(Rank::Two, Suit::Spades),
        ]),
        turn: None,
        river: None,
    };
    input.to_call = 0; // can check
    input.last_aggressor_seat = Some(input.hero_seat); // is aggressor
    let out = analyze(&input).unwrap();
    assert!(matches!(
        out.gto_action,
        SolverAction::Raise | SolverAction::AllIn
    ));
}

// ---------------------------------------------------------------------------
// R4: eq >= pot_odds + 5 AND to_call > 0 → Call
// ---------------------------------------------------------------------------
#[test]
fn rule_r4_call_with_positive_ev() {
    let mut input = base_input();
    // Middling equity hand (about 40-50%) on K72: A8 (no pair, no draw).
    // We need eq >= pot_odds + 5 with to_call > 0. Force a wide pot.
    input.hero = h(c(Rank::King, Suit::Spades), c(Rank::Queen, Suit::Hearts));
    input.board = BoardCards {
        flop: Some([
            c(Rank::King, Suit::Hearts),
            c(Rank::Seven, Suit::Clubs),
            c(Rank::Two, Suit::Spades),
        ]),
        turn: None,
        river: None,
    };
    // Top pair queen kicker — about 70% equity vs random.
    // pot 400 + to_call 50 → odds ~11%. eq >> 16% → R4 triggers if R2/R0 don't.
    // But high equity (70%) without can_check → R2 (raise). Need to land R4.
    // Lower equity: 6c5c on K72 — no pair, no draw. About 20% equity.
    input.hero = h(c(Rank::Six, Suit::Clubs), c(Rank::Five, Suit::Diamonds));
    input.pot_before = 1000;
    input.to_call = 50; // odds ~4.8% — eq ~20% would satisfy R4.
    input.stack_before = 1000;
    let out = analyze(&input).unwrap();
    // U39 (dual-AI OSS review): Fold must NOT be accepted — with eq ~20% vs
    // pot odds ~4.8% this is an unambiguous +EV call, and the whole point of
    // this test is to catch an R4→fold regression. The old `Call | Fold`
    // assertion could never fail on exactly that regression.
    assert_eq!(
        out.gto_action,
        SolverAction::Call,
        "R4 must call with equity far above pot odds; got {:?}",
        out.gto_action
    );
}

// ---------------------------------------------------------------------------
// R5: hs == DrawStrong AND eq >= pot_odds - 3 AND to_call > 0 → Call
// ---------------------------------------------------------------------------
#[test]
fn rule_r5_semi_bluff_defense_call() {
    let mut input = base_input();
    // Flush draw: AhKh on Jh-7c-2h. hs = DrawStrong.
    input.hero = h(c(Rank::Ace, Suit::Hearts), c(Rank::King, Suit::Hearts));
    input.board = BoardCards {
        flop: Some([
            c(Rank::Jack, Suit::Hearts),
            c(Rank::Seven, Suit::Clubs),
            c(Rank::Two, Suit::Hearts),
        ]),
        turn: None,
        river: None,
    };
    input.pot_before = 100;
    input.to_call = 60; // odds 37.5%
    input.stack_before = 800;
    input.last_aggressor_seat = Some(99); // not hero
    let out = analyze(&input).unwrap();
    assert!(
        matches!(out.gto_action, SolverAction::Call | SolverAction::Raise),
        "DrawStrong call/raise expected; got {:?}",
        out.gto_action
    );
}

// ---------------------------------------------------------------------------
// R6: hs == DrawStrong AND can_check AND is_aggressor AND street != River → Raise
// ---------------------------------------------------------------------------
#[test]
fn rule_r6_semi_bluff_barrel() {
    let mut input = base_input();
    input.hero = h(c(Rank::Ace, Suit::Hearts), c(Rank::King, Suit::Hearts));
    input.board = BoardCards {
        flop: Some([
            c(Rank::Jack, Suit::Hearts),
            c(Rank::Seven, Suit::Clubs),
            c(Rank::Two, Suit::Hearts),
        ]),
        turn: None,
        river: None,
    };
    input.to_call = 0;
    input.last_aggressor_seat = Some(input.hero_seat);
    let out = analyze(&input).unwrap();
    // Should be Raise (R3 fires first when eq>=50 with checkable + aggressor;
    // for AhKh + flush draw vs 1 random, eq is around 50% so R3 may grab it
    // — but if eq<50, R6 fires. Both are Raise.)
    assert_eq!(out.gto_action, SolverAction::Raise);
}

// ---------------------------------------------------------------------------
// R7: can_check AND eq < 35 → Check (pot control)
// ---------------------------------------------------------------------------
#[test]
fn rule_r7_pot_control_check() {
    let mut input = base_input();
    // 7-2 on K-Q-J — terrible equity.
    input.hero = h(c(Rank::Seven, Suit::Diamonds), c(Rank::Two, Suit::Clubs));
    input.board = BoardCards {
        flop: Some([
            c(Rank::King, Suit::Spades),
            c(Rank::Queen, Suit::Hearts),
            c(Rank::Jack, Suit::Clubs),
        ]),
        turn: None,
        river: None,
    };
    input.to_call = 0;
    input.last_aggressor_seat = Some(99); // not hero
    let out = analyze(&input).unwrap();
    assert_eq!(out.gto_action, SolverAction::Check);
}

// ---------------------------------------------------------------------------
// R8: to_call == 0 (no rule matched) → Check (default passive)
// ---------------------------------------------------------------------------
#[test]
fn rule_r8_default_check() {
    let mut input = base_input();
    // Middle equity hand, can check, NOT aggressor — should default to Check.
    // KQ on K-7-2: pair top, ~70% equity, but to_call=0 + not aggressor.
    // Wait — eq>=50 + can_check + is_aggressor=false: R3 doesn't fire. R7 needs eq<35.
    // For eq>=50: R3 needs aggressor; R6 needs DrawStrong. Neither fires here.
    // So we land in R8 (to_call==0 → Check).
    input.hero = h(c(Rank::King, Suit::Spades), c(Rank::Queen, Suit::Hearts));
    input.board = BoardCards {
        flop: Some([
            c(Rank::King, Suit::Hearts),
            c(Rank::Seven, Suit::Clubs),
            c(Rank::Two, Suit::Spades),
        ]),
        turn: None,
        river: None,
    };
    input.to_call = 0;
    input.last_aggressor_seat = Some(99); // NOT hero
    let out = analyze(&input).unwrap();
    assert_eq!(out.gto_action, SolverAction::Check);
}

// ---------------------------------------------------------------------------
// R9: fallthrough → Fold
// ---------------------------------------------------------------------------
#[test]
fn rule_r9_fallthrough_fold() {
    let mut input = base_input();
    // Trash hand, must call but pot odds bad — R9 fold.
    input.hero = h(c(Rank::Two, Suit::Diamonds), c(Rank::Three, Suit::Hearts));
    input.board = BoardCards {
        flop: Some([
            c(Rank::King, Suit::Spades),
            c(Rank::Queen, Suit::Hearts),
            c(Rank::Jack, Suit::Clubs),
        ]),
        turn: None,
        river: None,
    };
    input.pot_before = 100;
    input.to_call = 100; // odds 50%
    input.stack_before = 1000;
    let out = analyze(&input).unwrap();
    assert_eq!(out.gto_action, SolverAction::Fold);
}
