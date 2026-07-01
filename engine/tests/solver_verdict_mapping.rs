//! ADR-043 §3.4.2 verdict mapping — regression suite added 2026-05-23
//! to lock in the audit fix B-1 (the prior code unconditionally pinned
//! every verdict to "Good" because `hero_action_taken` was dropped during
//! `PromptContext` → `SolverInput` projection).
//!
//! Each test fixes a concrete (hero, hero_action, gto_action) tuple and
//! asserts the verdict matches the ADR table. The point of these tests is
//! that simply re-introducing the `_ctx` underscore prefix bug would fail
//! one or more of them.

use engine::card::{Card, Rank, Suit};
use engine::hand::{BoardCards, HoleCards, Street};
use engine::player::Position;
use engine::solver::{analyze, PreflopAction, SolverAction, SolverInput, SolverVerdict, TableSize};

fn c(r: Rank, s: Suit) -> Card {
    Card::new(r, s)
}

fn h(c1: Card, c2: Card) -> HoleCards {
    HoleCards::new(c1, c2)
}

fn dry_paired_top_set_board() -> BoardCards {
    // 8s on a K-8-2 rainbow board — top set for hero holding 88, no draws.
    BoardCards {
        flop: Some([
            c(Rank::King, Suit::Hearts),
            c(Rank::Eight, Suit::Diamonds),
            c(Rank::Two, Suit::Clubs),
        ]),
        turn: None,
        river: None,
    }
}

fn preflop_base() -> SolverInput {
    SolverInput {
        street: Street::Preflop,
        position: Position::Utg,
        table_size: TableSize::SixMax,
        hero: h(c(Rank::Ace, Suit::Spades), c(Rank::Ace, Suit::Hearts)),
        board: BoardCards::empty(),
        pot_before: 15, // SB+BB
        to_call: 10,
        stack_before: 1000,
        num_players_in_hand: 6,
        last_aggressor_seat: None,
        hero_seat: 0,
        hero_action_taken: None,
        preflop_action: Some(PreflopAction::Rfi),
        actions_so_far_count: 0,
        seed: 42,
    }
}

// ---------------------------------------------------------------------------
// B-1 verdict regression — preflop
// ---------------------------------------------------------------------------

/// Hero holds AhAs, UTG, RFI spot. GTO is Raise. Hero **folded** →
/// `verdict == "mistake"`. The bug would return "good" because
/// `hero_action_taken` was always `None`.
#[test]
fn preflop_aa_utg_folded_is_mistake() {
    let mut input = preflop_base();
    input.hero_action_taken = Some(SolverAction::Fold);
    let out = analyze(&input).expect("analyze must succeed");
    assert_eq!(out.gto_action, SolverAction::Raise);
    assert_eq!(
        out.verdict,
        SolverVerdict::Mistake,
        "folding AA in UTG must be a Mistake; got {:?}",
        out.verdict
    );
}

/// Hero holds 7d2c, UTG, RFI spot. GTO is Fold. Hero **raised** →
/// `verdict == "mistake"` (raising the bottom of the range = -EV vs the
/// implied calling/3-betting range).
#[test]
fn preflop_72o_utg_raised_is_mistake() {
    let mut input = preflop_base();
    input.hero = h(c(Rank::Seven, Suit::Diamonds), c(Rank::Two, Suit::Clubs));
    input.hero_action_taken = Some(SolverAction::Raise);
    let out = analyze(&input).expect("analyze must succeed");
    assert_eq!(out.gto_action, SolverAction::Fold);
    assert_eq!(
        out.verdict,
        SolverVerdict::Mistake,
        "raising 72o in UTG must be a Mistake; got {:?}",
        out.verdict
    );
}

/// Hero holds AKs, CO, RFI spot. GTO is Raise. Hero **raised** → `Good`.
#[test]
fn preflop_aks_co_rfi_raised_is_good() {
    let mut input = preflop_base();
    input.position = Position::Cutoff;
    input.hero = h(c(Rank::Ace, Suit::Spades), c(Rank::King, Suit::Spades));
    input.hero_action_taken = Some(SolverAction::Raise);
    let out = analyze(&input).expect("analyze must succeed");
    assert_eq!(out.gto_action, SolverAction::Raise);
    assert_eq!(out.verdict, SolverVerdict::Good);
}

// ---------------------------------------------------------------------------
// B-1 verdict regression — postflop
// ---------------------------------------------------------------------------

/// Hero holds 88, top set on K-8-2 dry. GTO is Raise (R0/R1/R2 — committed
/// value line, eq ~90%+). Hero **raised** → `Good`.
#[test]
fn postflop_top_set_raised_is_good() {
    let input = SolverInput {
        street: Street::Flop,
        position: Position::Dealer,
        table_size: TableSize::SixMax,
        hero: h(c(Rank::Eight, Suit::Spades), c(Rank::Eight, Suit::Hearts)),
        board: dry_paired_top_set_board(),
        pot_before: 200,
        to_call: 100,
        stack_before: 1000,
        num_players_in_hand: 2,
        last_aggressor_seat: None,
        hero_seat: 0,
        hero_action_taken: Some(SolverAction::Raise),
        preflop_action: None,
        actions_so_far_count: 5,
        seed: 1,
    };
    let out = analyze(&input).expect("analyze must succeed");
    assert!(
        matches!(out.gto_action, SolverAction::Raise | SolverAction::AllIn),
        "top set is committed value; got {:?}",
        out.gto_action
    );
    assert_eq!(
        out.verdict,
        SolverVerdict::Good,
        "top set raised must be Good; got {:?}",
        out.verdict
    );
}

/// Hero holds 88, top set on K-8-2 dry. GTO is Raise. Hero **checked**
/// (can_check=true, not aggressor → R8 actually applies for GTO; but with
/// to_call > 0 GTO will be Raise/AllIn). Per ADR-043 §3.4.2 verdict table:
/// "Anything else" → `Ok`. Test pins to Ok (checking a monster facing a bet
/// is passive but not -EV against pot odds 0; nothing in the strong-mistake
/// gate fires).
#[test]
fn postflop_top_set_checked_facing_bet_is_ok() {
    let input = SolverInput {
        street: Street::Flop,
        position: Position::Dealer,
        table_size: TableSize::SixMax,
        hero: h(c(Rank::Eight, Suit::Spades), c(Rank::Eight, Suit::Hearts)),
        board: dry_paired_top_set_board(),
        pot_before: 200,
        to_call: 100,
        stack_before: 1000,
        num_players_in_hand: 2,
        last_aggressor_seat: None,
        hero_seat: 0,
        // Hero passive while GTO is committed value — adjacent (passive vs
        // aggressive), no strong-mistake gate fires → ADR §3.4.2 "Ok".
        hero_action_taken: Some(SolverAction::Check),
        preflop_action: None,
        actions_so_far_count: 5,
        seed: 1,
    };
    let out = analyze(&input).expect("analyze must succeed");
    // GTO must still be Raise/AllIn.
    assert!(matches!(
        out.gto_action,
        SolverAction::Raise | SolverAction::AllIn
    ));
    // Verdict per ADR-043 §3.4.2 row "Anything else" → Ok.
    assert_eq!(
        out.verdict,
        SolverVerdict::Ok,
        "passive-vs-aggressive without strong-mistake gate must be Ok; got {:?}",
        out.verdict
    );
}

// ---------------------------------------------------------------------------
// B-2 — FacingOpen bucket is distinct from RFI
// ---------------------------------------------------------------------------

/// Hero CO, UTG has opened (raises=1), hero holds 99. The bucket must be
/// `FacingOpen` (not `Rfi`). With preflop_v2 charts, 99 IS in CO.facing_open
/// → GTO recommends Raise (continue). Pre-fix this spot used the wider RFI
/// chart, which still listed 99 as Raise — but the regression value here is
/// that the bucket itself is FacingOpen.
#[test]
fn preflop_facing_open_bucket_used_when_one_raise_ahead() {
    let mut input = preflop_base();
    input.position = Position::Cutoff;
    input.hero = h(c(Rank::Nine, Suit::Spades), c(Rank::Nine, Suit::Hearts));
    input.preflop_action = Some(PreflopAction::FacingOpen);
    input.hero_action_taken = Some(SolverAction::Raise);
    let out = analyze(&input).expect("analyze must succeed");
    // 99 is a continue from CO facing an open in preflop_v2.
    assert_eq!(out.gto_action, SolverAction::Raise);
    assert_eq!(out.verdict, SolverVerdict::Good);
}

/// Hands NOT in `facing_open` (e.g. 53o from BTN facing CO open) must fold.
/// The pre-fix RFI bucket would have INCLUDED these wider hands → recommend
/// Raise → "good" verdict for raising trash. With the FacingOpen bucket
/// they are absent → fold.
#[test]
fn preflop_facing_open_trash_folds_not_raises() {
    let mut input = preflop_base();
    input.position = Position::Dealer;
    // 5d3c — present in BTN.RFI but absent in BTN.facing_open per
    // preflop_v2.json.
    input.hero = h(c(Rank::Five, Suit::Diamonds), c(Rank::Three, Suit::Clubs));
    input.preflop_action = Some(PreflopAction::FacingOpen);
    input.hero_action_taken = Some(SolverAction::Raise);
    let out = analyze(&input).expect("analyze must succeed");
    assert_eq!(
        out.gto_action,
        SolverAction::Fold,
        "53o BTN facing open must fold (not in preflop_v2 facing_open chart); got {:?}",
        out.gto_action
    );
    // Raising bottom-of-range trash in a facing-open spot — but pot odds
    // are 10/25=40%, equity ~30%; eq < pot_odds-5 strong-mistake gate
    // requires eq < 35; with ~30% equity vs random it likely triggers
    // Mistake. Accept Mistake OR Ok (close call) since equity varies by
    // seed.
    assert!(
        matches!(out.verdict, SolverVerdict::Mistake | SolverVerdict::Ok),
        "expected Mistake or Ok for raising trash; got {:?}",
        out.verdict
    );
}
