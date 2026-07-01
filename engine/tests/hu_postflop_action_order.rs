//! Regression: HU (heads-up) post-flop action order.
//!
//! WSOP/TDA standard:
//! - Pre-flop, the dealer/button (= SB in HU) acts first.
//! - Post-flop (flop, turn, river), the **non-button** acts first.  In HU
//!   that is the BB.
//!
//! For 3+ players the SB (dealer+1) is both "first non-button left of
//! button" and the SB itself, so the rule is uniform: post-flop first
//! actor = first non-folded/non-all-in seat strictly after the button.
//!
//! Caught by PM-002/003/004 + AUD-R1-001.  Bug was that the post-flop
//! first-actor walk started AT `sb_idx`; in HU `sb_idx == dealer_idx`,
//! so the dealer/SB was returned as the first post-flop actor.

use engine::{
    action::PlayerAction,
    game::GameHand,
    hand::Street,
    player::{Chips, PlayerId},
    rng::PokerRng,
};

fn pid(n: u64) -> PlayerId {
    PlayerId::new(n)
}

fn c(n: u32) -> Chips {
    Chips(n)
}

/// Drive a HU hand to the start of the named post-flop street, using
/// `Call` from SB then `Check` from BB on every street until we reach
/// the target street.  Returns the snapshot whose `current_actor` is
/// the first post-flop actor on `target`.
///
/// Pre-flop sequence: SB calls (matching BB), BB checks → flop is dealt.
/// Post-flop sequence (flop / turn): first actor checks, second actor
/// checks → next street is dealt.
fn drive_to_street(target: Street) -> GameHand {
    let mut hand = GameHand::new_with_rng(
        vec![(pid(1), c(1000), 0), (pid(2), c(1000), 1)],
        0,     // dealer_idx=0 → seat 0 = dealer/SB, seat 1 = BB in HU
        c(20), // big_blind
        c(10), // small_blind
        PokerRng::from_seed(20260526),
    );
    hand.start().expect("start ok");

    // ---- Pre-flop: SB (dealer) calls, BB checks ----
    let pf_first = hand
        .snapshot()
        .current_actor
        .expect("HU pre-flop must have a current actor");
    assert_eq!(
        pf_first,
        pid(1),
        "HU pre-flop: SB (seat 0 / dealer) must act first"
    );
    hand.apply_action(pf_first, PlayerAction::Call)
        .expect("SB call ok");

    let pf_second = hand
        .snapshot()
        .current_actor
        .expect("BB must act after SB call");
    assert_eq!(pf_second, pid(2), "HU pre-flop: BB acts second");
    hand.apply_action(pf_second, PlayerAction::Check)
        .expect("BB check closes preflop");

    if target == Street::Flop {
        return hand;
    }

    // ---- Flop: check / check ----
    advance_street_check_check(&mut hand);
    if target == Street::Turn {
        return hand;
    }

    // ---- Turn: check / check ----
    advance_street_check_check(&mut hand);
    if target == Street::River {
        return hand;
    }

    panic!("unsupported target street: {:?}", target);
}

/// Issue `Check` from whoever is current actor, twice — closing one
/// post-flop street into the next.  Does NOT assert ordering (caller
/// asserts it on the snapshot immediately after this returns).
fn advance_street_check_check(hand: &mut GameHand) {
    let a = hand
        .snapshot()
        .current_actor
        .expect("post-flop must have actor");
    hand.apply_action(a, PlayerAction::Check)
        .expect("first check ok");
    let b = hand
        .snapshot()
        .current_actor
        .expect("post-flop second actor");
    hand.apply_action(b, PlayerAction::Check)
        .expect("second check ok");
}

// ---------------------------------------------------------------------------
// Pre-flop: SB acts first (sanity / no regression)
// ---------------------------------------------------------------------------

#[test]
fn hu_preflop_sb_acts_first() {
    let mut hand = GameHand::new_with_rng(
        vec![(pid(1), c(1000), 0), (pid(2), c(1000), 1)],
        0,
        c(20),
        c(10),
        PokerRng::from_seed(1),
    );
    hand.start().expect("start ok");
    let snap = hand.snapshot();
    assert_eq!(
        snap.current_actor,
        Some(pid(1)),
        "HU pre-flop: SB (dealer, seat 0) must act first; got {:?}",
        snap.current_actor
    );
    assert_eq!(snap.street, Street::Preflop);
}

// ---------------------------------------------------------------------------
// Post-flop streets: BB acts first
// ---------------------------------------------------------------------------

#[test]
fn hu_flop_bb_acts_first() {
    let hand = drive_to_street(Street::Flop);
    let snap = hand.snapshot();
    assert_eq!(snap.street, Street::Flop, "must be on flop");
    assert_eq!(
        snap.current_actor,
        Some(pid(2)),
        "HU flop: BB (non-button, seat 1) must act first per WSOP/TDA; got {:?}",
        snap.current_actor
    );
}

#[test]
fn hu_turn_bb_acts_first() {
    let hand = drive_to_street(Street::Turn);
    let snap = hand.snapshot();
    assert_eq!(snap.street, Street::Turn, "must be on turn");
    assert_eq!(
        snap.current_actor,
        Some(pid(2)),
        "HU turn: BB (non-button, seat 1) must act first per WSOP/TDA; got {:?}",
        snap.current_actor
    );
}

#[test]
fn hu_river_bb_acts_first() {
    let hand = drive_to_street(Street::River);
    let snap = hand.snapshot();
    assert_eq!(snap.street, Street::River, "must be on river");
    assert_eq!(
        snap.current_actor,
        Some(pid(2)),
        "HU river: BB (non-button, seat 1) must act first per WSOP/TDA; got {:?}",
        snap.current_actor
    );
}

// ---------------------------------------------------------------------------
// Regression guard: 3-way post-flop SB acts first
// ---------------------------------------------------------------------------

#[test]
fn three_way_flop_sb_acts_first() {
    // dealer_idx=0: dealer=pid(1)/seat0, SB=pid(2)/seat1, BB=pid(3)/seat2.
    // Pre-flop order: UTG=dealer in 3-way (action starts left of BB which is
    // back to seat 0 = pid(1)), then SB, then BB.
    let mut hand = GameHand::new_with_rng(
        vec![
            (pid(1), c(1000), 0),
            (pid(2), c(1000), 1),
            (pid(3), c(1000), 2),
        ],
        0,
        c(20),
        c(10),
        PokerRng::from_seed(3),
    );
    hand.start().expect("start ok");

    // Drive pre-flop to the flop: UTG calls, SB calls, BB checks.
    let utg = hand.snapshot().current_actor.expect("UTG present");
    assert_eq!(utg, pid(1), "3-way pre-flop: UTG (seat 0) acts first");
    hand.apply_action(utg, PlayerAction::Call).unwrap();

    let sb = hand.snapshot().current_actor.expect("SB present");
    assert_eq!(sb, pid(2), "3-way pre-flop: SB acts after UTG");
    hand.apply_action(sb, PlayerAction::Call).unwrap();

    let bb = hand.snapshot().current_actor.expect("BB present");
    assert_eq!(bb, pid(3), "3-way pre-flop: BB acts last");
    hand.apply_action(bb, PlayerAction::Check).unwrap();

    // Now on the flop — SB must act first (first seat left of button).
    let snap = hand.snapshot();
    assert_eq!(snap.street, Street::Flop, "must be on flop");
    assert_eq!(
        snap.current_actor,
        Some(pid(2)),
        "3-way flop: SB (seat 1, first left of button) must act first; got {:?}",
        snap.current_actor
    );
}
