//! GameHand regression tests: action facts.

use super::*;

/// discovery-r1 BUG-236 — the hand-level aggressor survives the street
/// boundary. UTG opens, BB calls; on the flop the BB checks to the raiser.
/// The CURRENT-street facts carry no wager increase (nobody has bet the
/// flop yet), but `last_aggressor_player_id` must still name the preflop
/// raiser — that is the c-bet owner ADR-043 R3 reads. Feeding the
/// current-street-only reading to the solver made `can_check &&
/// is_aggressor` unreachable.
#[test]
fn last_aggressor_carries_the_preflop_raiser_onto_a_checked_flop() {
    let mut hand = GameHand::new_with_rng(
        vec![
            (pid(1), c(1000), 0), // BTN/UTG in 3-max
            (pid(2), c(1000), 1), // SB
            (pid(3), c(1000), 2), // BB
        ],
        0,
        c(20),
        c(10),
        PokerRng::from_seed(49),
    );
    hand.start().expect("hand starts");
    assert_eq!(
        hand.last_aggressor_player_id(),
        None,
        "blind posts are never aggression"
    );

    hand.apply_action(pid(1), PlayerAction::Raise { amount: c(60) })
        .expect("open is legal");
    hand.apply_action(pid(2), PlayerAction::Fold)
        .expect("fold is legal");
    hand.apply_action(pid(3), PlayerAction::Call)
        .expect("call is legal");
    // Flop dealt; BB is first to act and checks to the raiser.
    assert_eq!(
        hand.last_aggressor_player_id(),
        Some(pid(1)),
        "at the start of the flop the preflop raiser is still the aggressor"
    );
    hand.apply_action(pid(3), PlayerAction::Check)
        .expect("check is legal");

    // The street-local projection resets…
    assert!(
        hand.current_street_action_facts()
            .iter()
            .all(|fact| !fact.increased_bet),
        "no flop wager increase yet"
    );
    // …the hand-level aggressor does NOT.
    assert_eq!(hand.last_aggressor_player_id(), Some(pid(1)));

    // A flop bet by the BB donk moves the aggressor identity forward.
    // (Re-run the spot: raiser bets, BB raises.)
    hand.apply_action(pid(1), PlayerAction::Raise { amount: c(40) })
        .expect("c-bet is legal");
    hand.apply_action(pid(3), PlayerAction::Raise { amount: c(120) })
        .expect("check-raise is legal");
    assert_eq!(
        hand.last_aggressor_player_id(),
        Some(pid(3)),
        "the most recent wager increase wins, on the current street"
    );
}

/// BUG-236 companion — a limped pot checked down has NO aggressor on any
/// street, so the solver must not invent one.
#[test]
fn last_aggressor_is_none_in_a_limped_pot_checked_down() {
    let mut hand = GameHand::new_with_rng(
        vec![
            (pid(1), c(1000), 0),
            (pid(2), c(1000), 1),
            (pid(3), c(1000), 2),
        ],
        0,
        c(20),
        c(10),
        PokerRng::from_seed(50),
    );
    hand.start().expect("hand starts");
    hand.apply_action(pid(1), PlayerAction::Call).expect("limp");
    hand.apply_action(pid(2), PlayerAction::Call)
        .expect("complete");
    hand.apply_action(pid(3), PlayerAction::Check)
        .expect("option");
    // Flop: SB, BB check.
    hand.apply_action(pid(2), PlayerAction::Check)
        .expect("check");
    hand.apply_action(pid(3), PlayerAction::Check)
        .expect("check");
    assert_eq!(hand.last_aggressor_player_id(), None);
    // `street_action_facts` for a not-yet-dealt street is empty.
    assert!(hand.street_action_facts(Street::River).is_empty());
}

/// First voluntary action must get seq=2 (not seq=1 as before the fix).
#[test]
fn first_voluntary_action_seq_is_2_after_blinds() {
    let mut hand = GameHand::new_with_rng(
        vec![
            (pid(1), c(1000), 0),
            (pid(2), c(1000), 1),
            (pid(3), c(1000), 2),
        ],
        0,
        c(20),
        c(10),
        PokerRng::from_seed(43),
    );
    hand.start().unwrap();
    // UTG (pid(1)) is first to act preflop in 3-player.
    let actor = hand.snapshot().current_actor.unwrap();
    hand.apply_action(actor, PlayerAction::Fold).unwrap();
    let result_actions = &hand.actions;
    assert_eq!(
        result_actions.len(),
        3,
        "after one voluntary action, 3 total"
    );
    assert_eq!(
        result_actions[2].seq, 2,
        "first voluntary action must have seq=2"
    );
    // All three seqs must be distinct.
    let seqs: Vec<u16> = result_actions.iter().map(|r| r.seq).collect();
    let unique: std::collections::HashSet<u16> = seqs.iter().copied().collect();
    assert_eq!(seqs.len(), unique.len(), "all action seqs must be unique");
}

#[test]
fn street_action_facts_distinguish_all_in_call_short_raise_and_full_raise() {
    struct Case {
        all_in_total: u32,
        increased_bet: bool,
        full_raise: bool,
        call_like: bool,
        expected_last_aggressor: PlayerId,
    }

    let opener = pid(4);
    let all_in_player = pid(1);
    let cases = [
        Case {
            all_in_total: 50,
            increased_bet: false,
            full_raise: false,
            call_like: true,
            expected_last_aggressor: opener,
        },
        Case {
            all_in_total: 100,
            increased_bet: false,
            full_raise: false,
            call_like: true,
            expected_last_aggressor: opener,
        },
        Case {
            all_in_total: 150,
            increased_bet: true,
            full_raise: false,
            call_like: false,
            expected_last_aggressor: all_in_player,
        },
        Case {
            all_in_total: 180,
            increased_bet: true,
            full_raise: true,
            call_like: false,
            expected_last_aggressor: all_in_player,
        },
    ];

    for case in cases {
        // Four handed, dealer pid1: pid2 posts SB, pid3 posts BB, pid4
        // opens first to 100, then pid1 commits the case's exact stack.
        let mut hand = GameHand::new_with_rng(
            vec![
                (all_in_player, c(case.all_in_total), 0),
                (pid(2), c(1_000), 1),
                (pid(3), c(1_000), 2),
                (opener, c(1_000), 3),
            ],
            0,
            c(20),
            c(10),
            PokerRng::from_seed(u64::from(case.all_in_total)),
        );
        hand.start().expect("start hand");
        assert_eq!(hand.snapshot().current_actor, Some(opener));
        hand.apply_action(opener, PlayerAction::Raise { amount: c(100) })
            .expect("opening raise");
        assert_eq!(hand.snapshot().current_actor, Some(all_in_player));
        hand.apply_action(all_in_player, PlayerAction::AllIn)
            .expect("all-in case is legal");

        let facts = hand.current_street_action_facts();
        assert_eq!(facts.len(), 2);
        assert!(facts[0].increased_bet);
        assert!(facts[0].full_raise);
        assert_eq!(facts[1].increased_bet, case.increased_bet);
        assert_eq!(facts[1].full_raise, case.full_raise);
        assert_eq!(facts[1].is_call_like(), case.call_like);

        let projected = facts[1].strategy_action();
        if case.full_raise {
            assert_eq!(projected, PlayerAction::AllIn);
        } else {
            assert_eq!(projected, PlayerAction::Call);
        }
        assert_eq!(
            facts
                .iter()
                .rev()
                .find(|fact| fact.increased_bet)
                .map(|fact| fact.player_id),
            Some(case.expected_last_aggressor)
        );
    }
}

#[test]
fn call_like_fact_counts_calls_but_not_short_raises() {
    let fact = |action, increased_bet| StreetActionFact {
        player_id: pid(1),
        action,
        increased_bet,
        full_raise: false,
    };

    assert!(fact(PlayerAction::Call, false).is_call_like());
    assert!(fact(PlayerAction::AllIn, false).is_call_like());
    assert!(fact(PlayerAction::Raise { amount: c(100) }, false).is_call_like());
    assert!(!fact(PlayerAction::AllIn, true).is_call_like());
    assert!(!fact(PlayerAction::Raise { amount: c(150) }, true).is_call_like());
}

/// C6 review: a `Call` issued in a checkable spot (`to_call == 0`) must be
/// recorded/emitted as a `Check`, not as an illegal zero-chip `call` — but
/// it must NOT error (a client may legitimately send Call there). 0 chips
/// move either way and the pot/state transition is unchanged; only the
/// recorded label differs.
#[test]
fn zero_chip_call_recorded_as_check() {
    // HU, dealer=0: pid(1)=SB acts first preflop, pid(2)=BB.
    let mut hand = GameHand::new_with_rng(
        vec![(pid(1), c(1000), 0), (pid(2), c(1000), 1)],
        0,
        c(20), // big blind
        c(10), // small blind
        PokerRng::from_seed(9),
    );
    hand.start().unwrap();

    // SB completes (calls 10 → matches the 20 big blind).
    let sb = hand.snapshot().current_actor.unwrap();
    assert_eq!(sb, pid(1), "SB acts first preflop in HU");
    hand.apply_action(sb, PlayerAction::Call).unwrap();

    // BB now faces to_call == 0 (already in for the full current bet).
    let snap_before = hand.snapshot();
    let bb = snap_before.current_actor.unwrap();
    assert_eq!(bb, pid(2), "BB has the option to check or close the street");
    let pot_before = snap_before.pot;
    let bb_stack_before = snap_before
        .players
        .iter()
        .find(|p| p.player_id == bb)
        .unwrap()
        .stack;

    // BB sends Call in a checkable spot — must succeed (not rejected).
    hand.apply_action(bb, PlayerAction::Call)
        .expect("zero-chip Call must be accepted");

    // It must be RECORDED as a Check, never as a zero-chip call.
    let last = hand.actions.last().unwrap();
    assert_eq!(last.player_id, bb);
    assert_eq!(
        last.action,
        PlayerAction::Check,
        "zero-chip Call must be relabelled to Check in action history"
    );
    assert_eq!(last.amount, Chips::ZERO, "zero chips move either way");

    // Pot and BB stack are unchanged by the zero-chip action.
    assert_eq!(
        last.pot_before, last.pot_after,
        "pot unchanged by zero-chip action"
    );
    let snap_after = hand.snapshot();
    let bb_stack_after = snap_after
        .players
        .iter()
        .find(|p| p.player_id == bb)
        .unwrap()
        .stack;
    assert_eq!(bb_stack_before, bb_stack_after, "BB stack unchanged");
    // Total pot is unchanged from the moment before BB acted.
    assert!(
        snap_after.pot >= pot_before,
        "pot must not shrink: {} -> {}",
        pot_before.0,
        snap_after.pot.0
    );
    assert_eq!(
        snap_after.pot, pot_before,
        "zero-chip action must not change the pot"
    );
}

#[test]
fn last_action_resets_at_each_street() {
    let mut hand = GameHand::new_with_rng(
        vec![(pid(1), c(1000), 0), (pid(2), c(1000), 1)],
        0,
        c(20),
        c(10),
        PokerRng::from_seed(9),
    );
    hand.start().unwrap();

    // Preflop: SB raises, BB calls → flop.
    let sb = hand.snapshot().current_actor.unwrap();
    hand.apply_action(sb, PlayerAction::Raise { amount: c(60) })
        .unwrap();
    let bb = hand.snapshot().current_actor.unwrap();
    hand.apply_action(bb, PlayerAction::Call).unwrap();

    let flop = hand.snapshot();
    assert_eq!(flop.street, Street::Flop);
    assert!(
        flop.players.iter().all(|p| p.last_action.is_none()),
        "a preflop action must not survive into the flop snapshot"
    );

    // A flop action shows only for the seat that took it.
    let first = flop.current_actor.unwrap();
    hand.apply_action(first, PlayerAction::Check).unwrap();
    let snap = hand.snapshot();
    for p in &snap.players {
        let expected = (p.player_id == first).then_some(PlayerAction::Check);
        assert_eq!(p.last_action, expected);
    }
}
