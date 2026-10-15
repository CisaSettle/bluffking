//! GameHand regression tests: blinds and positions.

use super::*;

#[test]
fn start_rejects_table_chip_total_above_public_u32_pot_range() {
    let mut hand = GameHand::new_with_rng(
        vec![(pid(1), c(u32::MAX), 0), (pid(2), c(1), 1)],
        0,
        c(2),
        c(1),
        PokerRng::from_seed(1),
    );
    assert!(matches!(hand.start(), Err(ActionError::InvalidAction)));
    assert!(
        hand.actions.is_empty(),
        "oversized table must fail before blinds"
    );
}

// ---- P0 regression: blind seq uniqueness (M6-6 bug) ----

/// SB blind must get seq=0, BB blind must get seq=1.
///
/// Before the fix, both were seq=0, which caused the DB unique index
/// `hand_actions_hand_seq` to reject the second INSERT, leaving
/// `hand_actions` empty for every hand.
#[test]
fn blinds_have_distinct_seq_after_start() {
    let mut hand = GameHand::new_with_rng(
        vec![
            (pid(1), c(1000), 0),
            (pid(2), c(1000), 1),
            (pid(3), c(1000), 2),
        ],
        0,
        c(20),
        c(10),
        PokerRng::from_seed(42),
    );
    hand.start().expect("start ok");
    let result_actions = &hand.actions;
    assert_eq!(
        result_actions.len(),
        2,
        "start() must record exactly 2 blind actions"
    );
    assert_eq!(result_actions[0].seq, 0, "SB blind must have seq=0");
    assert_eq!(result_actions[1].seq, 1, "BB blind must have seq=1");
    // Verify uniqueness — the core invariant the DB index enforces.
    assert_ne!(
        result_actions[0].seq, result_actions[1].seq,
        "SB and BB blind seq must be distinct (was the P0 bug)"
    );
}

/// Defensive hardening (audit 2026-06-03): an out-of-range `dealer_idx`
/// must wrap (`% n`) like `blind_positions` instead of panicking with
/// index-out-of-bounds in `start()` / `snapshot()`.
#[test]
fn out_of_range_dealer_idx_wraps_without_panic() {
    // 3 seats but dealer_idx = 5 (>= n). Should wrap to 5 % 3 == 2.
    let mut hand = GameHand::new_with_rng(
        vec![
            (pid(1), c(1000), 0),
            (pid(2), c(1000), 1),
            (pid(3), c(1000), 2),
        ],
        5, // out-of-range dealer index
        c(20),
        c(10),
        PokerRng::from_seed(11),
    );
    // Must not panic.
    hand.start().expect("start with wrapped dealer index");
    let snap = hand.snapshot();
    // dealer_idx 5 % 3 == 2 → seat 2 is the button.
    assert_eq!(
        snap.dealer_seat, 2,
        "out-of-range dealer index must wrap to seat 2"
    );
}

/// C6 review: the heads-up (`n == 2`) branch of `blind_positions` must also
/// apply `% n` to the dealer/SB. `start()` indexes `self.seats[sb_idx]`
/// BEFORE the `% n` guard on the HandStarted event, so an out-of-range
/// `dealer_idx` (>= 2) would panic without this. Mirrors the 3+ test above.
#[test]
fn heads_up_out_of_range_dealer_idx_wraps_without_panic() {
    // 2 seats but dealer_idx = 4 (>= n). Should wrap to 4 % 2 == 0.
    let mut hand = GameHand::new_with_rng(
        vec![(pid(1), c(1000), 0), (pid(2), c(1000), 1)],
        4, // out-of-range dealer index
        c(20),
        c(10),
        PokerRng::from_seed(11),
    );
    // Must not panic.
    hand.start().expect("HU start with wrapped dealer index");
    let snap = hand.snapshot();
    // dealer_idx 4 % 2 == 0 → seat 0 is the button (HU: dealer is SB).
    assert_eq!(
        snap.dealer_seat, 0,
        "out-of-range HU dealer index must wrap to seat 0"
    );
    // An odd out-of-range index must wrap to seat 1.
    let mut hand2 = GameHand::new_with_rng(
        vec![(pid(1), c(1000), 0), (pid(2), c(1000), 1)],
        5, // 5 % 2 == 1
        c(20),
        c(10),
        PokerRng::from_seed(12),
    );
    hand2
        .start()
        .expect("HU start with wrapped odd dealer index");
    assert_eq!(
        hand2.snapshot().dealer_seat,
        1,
        "out-of-range odd HU dealer index must wrap to seat 1"
    );
}

#[test]
fn usize_max_dealer_idx_wraps_through_a_complete_hand() {
    assert_eq!(blind_positions(usize::MAX, 2), (usize::MAX % 2, 0));
    let dealer = usize::MAX % 3;
    assert_eq!(
        blind_positions(usize::MAX, 3),
        ((dealer + 1) % 3, (dealer + 2) % 3)
    );

    let mut hand = GameHand::new_with_rng(
        vec![
            (pid(1), c(1_000), 0),
            (pid(2), c(1_000), 1),
            (pid(3), c(1_000), 2),
        ],
        usize::MAX,
        c(20),
        c(10),
        PokerRng::from_seed(13),
    );
    hand.start().expect("usize::MAX dealer starts safely");

    while !hand.is_done() {
        let snapshot = hand.snapshot();
        let actor = snapshot.current_actor.expect("live hand has an actor");
        let player = snapshot
            .players
            .iter()
            .find(|player| player.player_id == actor)
            .expect("actor exists");
        let action = if snapshot.current_bet.0 > player.committed_this_street.0 {
            PlayerAction::Call
        } else {
            PlayerAction::Check
        };
        hand.apply_action(actor, action)
            .expect("call/check runout remains legal");
    }

    let result = hand.finish();
    assert_eq!(result.show_order.len(), 3);
}

/// A-002: In heads-up (n=2) the dealer is the SB and the non-dealer is the BB.
/// After start(), dealer.stack == initial - small_blind,
/// non_dealer.stack == initial - big_blind.
#[test]
fn heads_up_dealer_is_small_blind() {
    let initial = 1000u32;
    let sb_amt = 10u32;
    let bb_amt = 20u32;
    // dealer_idx = 0, so seat 0 is dealer/SB, seat 1 is BB.
    let mut hand = GameHand::new_with_rng(
        vec![(pid(1), c(initial), 0), (pid(2), c(initial), 1)],
        0, // dealer_idx = 0
        c(bb_amt),
        c(sb_amt),
        PokerRng::from_seed(3),
    );
    hand.start().unwrap();
    let snap = hand.snapshot();
    let dealer_player = snap.players.iter().find(|p| p.player_id == pid(1)).unwrap();
    let non_dealer_player = snap.players.iter().find(|p| p.player_id == pid(2)).unwrap();
    // Dealer (seat 0, dealer_idx=0) must have posted the small blind.
    assert_eq!(
        dealer_player.stack.0,
        initial - sb_amt,
        "dealer should have posted small blind"
    );
    // Non-dealer (seat 1) must have posted the big blind.
    assert_eq!(
        non_dealer_player.stack.0,
        initial - bb_amt,
        "non-dealer should have posted big blind"
    );
}

/// A-002: 3-way: dealer stack unchanged, seat after dealer is SB, next is BB.
#[test]
fn three_way_standard_blind_positions() {
    let initial = 1000u32;
    let sb_amt = 10u32;
    let bb_amt = 20u32;
    // dealer_idx = 0: dealer=pid(1), SB=pid(2), BB=pid(3)
    let mut hand = GameHand::new_with_rng(
        vec![
            (pid(1), c(initial), 0),
            (pid(2), c(initial), 1),
            (pid(3), c(initial), 2),
        ],
        0, // dealer_idx = 0
        c(bb_amt),
        c(sb_amt),
        PokerRng::from_seed(4),
    );
    hand.start().unwrap();
    let snap = hand.snapshot();
    let dealer_p = snap.players.iter().find(|p| p.player_id == pid(1)).unwrap();
    let sb_p = snap.players.iter().find(|p| p.player_id == pid(2)).unwrap();
    let bb_p = snap.players.iter().find(|p| p.player_id == pid(3)).unwrap();
    assert_eq!(
        dealer_p.stack.0, initial,
        "dealer should not post any blind"
    );
    assert_eq!(sb_p.stack.0, initial - sb_amt, "seat after dealer is SB");
    assert_eq!(
        bb_p.stack.0,
        initial - bb_amt,
        "seat two after dealer is BB"
    );
}

#[test]
fn ante_posts_dead_money_before_blinds() {
    let mut hand = GameHand::new_with_rng(
        vec![
            (pid(1), c(1000), 0),
            (pid(2), c(1000), 1),
            (pid(3), c(1000), 2),
        ],
        0,
        c(20),
        c(10),
        PokerRng::from_seed(7),
    )
    .with_ante(c(5));

    let snap = hand.start().expect("ante hand starts");
    assert_eq!(snap.pot, c(45), "3 antes + SB + BB");
    assert_eq!(snap.current_bet, c(20), "antes do not raise the bet level");
    assert_eq!(snap.min_raise_to, Some(c(40)));
    assert_eq!(snap.current_actor, Some(pid(1)));
    assert_eq!(hand.actions.len(), 2, "antes are not action records");
    assert_eq!(hand.actions[0].seq, 0);
    assert_eq!(hand.actions[0].pot_before, c(15));
    assert_eq!(hand.actions[1].pot_after, c(45));

    let events = hand.drain_events();
    assert!(matches!(
        events.first(),
        Some(EngineEvent::HandStarted { ante: 5, .. })
    ));
    assert!(events
        .iter()
        .any(|e| matches!(e, EngineEvent::PotUpdated { pot: 45, .. })));

    hand.apply_action(pid(1), PlayerAction::Fold).unwrap();
    hand.apply_action(pid(2), PlayerAction::Fold).unwrap();
    assert!(hand.is_done());
    let result = hand.finish();
    assert_eq!(result.final_stacks[&1], 995);
    assert_eq!(result.final_stacks[&2], 985);
    assert_eq!(result.final_stacks[&3], 1020);
    assert_eq!(result.final_stacks.values().sum::<u32>(), 3000);
}

#[test]
fn ante_short_stack_is_all_in_and_only_contests_the_ante_layer() {
    let mut hand = GameHand::new_with_rng(
        vec![
            (pid(1), c(3), 0),
            (pid(2), c(1000), 1),
            (pid(3), c(1000), 2),
        ],
        0,
        c(20),
        c(10),
        PokerRng::from_seed(11),
    )
    .with_ante(c(5));

    let snap = hand.start().expect("ante hand starts");
    assert_eq!(snap.pot, c(3 + 5 + 5 + 10 + 20));
    assert_ne!(
        snap.current_actor,
        Some(pid(1)),
        "a seat emptied by the ante never acts"
    );
    while !hand.is_done() {
        let Some(actor) = hand.snapshot().current_actor else {
            break;
        };
        if hand.apply_action(actor, PlayerAction::Call).is_err() {
            hand.apply_action(actor, PlayerAction::Check).unwrap();
        }
    }
    let result = hand.finish();
    assert_eq!(result.final_stacks.values().sum::<u32>(), 2003);
    assert!(
        result.final_stacks[&1] <= 9,
        "the all-in short stack can win at most 3 x its 3-chip ante"
    );
}

#[test]
fn zero_ante_is_the_unchanged_blind_only_hand() {
    let mut hand = GameHand::new_with_rng(
        vec![
            (pid(1), c(1000), 0),
            (pid(2), c(1000), 1),
            (pid(3), c(1000), 2),
        ],
        0,
        c(20),
        c(10),
        PokerRng::from_seed(7),
    )
    .with_ante(c(0));
    let snap = hand.start().unwrap();
    assert_eq!(snap.pot, c(30));
    assert!(matches!(
        hand.drain_events().first(),
        Some(EngineEvent::HandStarted { ante: 0, .. })
    ));
}

/// Tournament dead-button rule: with a dead small blind nobody posts it, the
/// big blind is left of the dealer, preflop opens left of the big blind and
/// postflop action still starts left of the dealer.
#[test]
fn dead_small_blind_posts_only_the_big_blind() {
    let mut hand = GameHand::new_with_rng(
        vec![
            (pid(1), c(1000), 0),
            (pid(2), c(1000), 3),
            (pid(3), c(1000), 5),
            (pid(4), c(1000), 7),
        ],
        0,
        c(20),
        c(10),
        PokerRng::from_seed(11),
    )
    .with_dead_small_blind();
    let snap = hand.start().unwrap();
    let stack = |snap: &GameSnapshot, id: u64| {
        snap.players
            .iter()
            .find(|p| p.player_id == pid(id))
            .unwrap()
            .stack
            .0
    };
    assert_eq!(stack(&snap, 1), 1000, "dealer posts nothing");
    assert_eq!(
        stack(&snap, 2),
        980,
        "seat left of the dealer posts the big blind"
    );
    assert_eq!(stack(&snap, 3), 1000, "no small blind is posted");
    assert_eq!(snap.pot.0, 20);
    assert_eq!(
        snap.current_actor,
        Some(pid(3)),
        "preflop opens left of the big blind"
    );
    // The validation dry run (preview clone) must keep the dead-blind layout.
    hand.validate_action(pid(3), &PlayerAction::Call).unwrap();
    for id in [3, 4, 1] {
        hand.apply_action(pid(id), PlayerAction::Call).unwrap();
    }
    let snap = hand.apply_action(pid(2), PlayerAction::Check).unwrap();
    assert_eq!(snap.street, Street::Flop);
    assert_eq!(
        snap.current_actor,
        Some(pid(2)),
        "postflop starts left of the dealer"
    );
    assert_eq!(snap.pot.0, 80);
    for street in 0..3 {
        for id in [2, 3, 4, 1] {
            if hand.is_done() {
                break;
            }
            hand.apply_action(pid(id), PlayerAction::Check).unwrap();
        }
        let _ = street;
    }
    assert!(hand.is_done());
    let result = hand.finish();
    assert_eq!(
        result.final_stacks.values().map(|v| *v as u64).sum::<u64>(),
        4000
    );
}

/// Heads-up ignores the dead-small-blind flag: the dealer always posts it.
#[test]
fn dead_small_blind_is_ignored_heads_up() {
    let mut hand = GameHand::new_with_rng(
        vec![(pid(1), c(1000), 0), (pid(2), c(1000), 4)],
        0,
        c(20),
        c(10),
        PokerRng::from_seed(12),
    )
    .with_dead_small_blind();
    let snap = hand.start().unwrap();
    let dealer = snap.players.iter().find(|p| p.player_id == pid(1)).unwrap();
    assert_eq!(dealer.stack.0, 990);
    assert_eq!(snap.current_actor, Some(pid(1)));
}
