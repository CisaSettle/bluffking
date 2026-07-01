//! Integration tests for B-a1: EngineEvent emission from GameHand.
//!
//! These tests verify:
//! 1. Blind posting emits `ActionApplied{Blind}` x2 + `PotUpdated` x1.
//! 2. Hole cards emit `HoleCardsDealt` for each seat.
//! 3. Normal actions emit `ActionApplied` (+ optional `PotUpdated`).
//! 4. Street advances emit `StreetRevealed` in Flop → Turn → River order.
//! 5. All-in runout emits full burst from a single `apply_action` call.
//! 6. `finish()` emits `HandFinished`.
//! 7. `is_started()` is observable from outside the engine crate.
//! 8. `drain_events()` is idempotent — second call returns empty Vec.

use engine::{
    action::{BlindKind, PlayerAction},
    event::EngineEvent,
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

/// Helper: count events of a specific discriminant using a closure predicate.
fn count_events<F: Fn(&EngineEvent) -> bool>(events: &[EngineEvent], pred: F) -> usize {
    events.iter().filter(|e| pred(e)).count()
}

fn is_hole_cards_dealt(e: &EngineEvent) -> bool {
    matches!(e, EngineEvent::HoleCardsDealt { .. })
}

fn is_action_applied(e: &EngineEvent) -> bool {
    matches!(e, EngineEvent::ActionApplied { .. })
}

fn is_blind_action_applied(e: &EngineEvent) -> bool {
    matches!(
        e,
        EngineEvent::ActionApplied {
            action: PlayerAction::Blind { .. },
            ..
        }
    )
}

fn is_pot_updated(e: &EngineEvent) -> bool {
    matches!(e, EngineEvent::PotUpdated { .. })
}

fn is_street_revealed(e: &EngineEvent) -> bool {
    matches!(e, EngineEvent::StreetRevealed { .. })
}

fn is_hand_finished(e: &EngineEvent) -> bool {
    matches!(e, EngineEvent::HandFinished { .. })
}

fn street_of_revealed(e: &EngineEvent) -> Option<Street> {
    match e {
        EngineEvent::StreetRevealed { street, .. } => Some(*street),
        _ => None,
    }
}

// ---------------------------------------------------------------------------
// Test 1: blind posting + hole cards events after start()
// ---------------------------------------------------------------------------

/// After `start()` on a 2-player hand:
/// - 2x ActionApplied{Blind} (SB then BB)
/// - 1x PotUpdated (combined after both blinds)
/// - 2x HoleCardsDealt (one per seat)
/// - No StreetRevealed yet (still preflop)
/// - No HandFinished
#[test]
fn start_emits_blinds_and_hole_cards() {
    let mut hand = GameHand::new_with_rng(
        vec![(pid(1), c(1000), 0), (pid(2), c(1000), 1)],
        0,     // dealer_idx
        c(20), // big_blind
        c(10), // small_blind
        PokerRng::from_seed(100),
    );

    hand.start().expect("start failed");
    let events = hand.drain_events();

    // Exactly 2 blind ActionApplied events.
    assert_eq!(
        count_events(&events, is_blind_action_applied),
        2,
        "expected 2 ActionApplied{{Blind}} events; got: {events:?}"
    );

    // Exactly 1 PotUpdated after blinds.
    assert_eq!(
        count_events(&events, is_pot_updated),
        1,
        "expected 1 PotUpdated after blinds; got: {events:?}"
    );

    // Exactly 2 HoleCardsDealt (one per player).
    assert_eq!(
        count_events(&events, is_hole_cards_dealt),
        2,
        "expected 2 HoleCardsDealt; got: {events:?}"
    );

    // HoleCardsDealt must come AFTER both blinds (order: SB blind, BB blind, PotUpdated, then cards).
    let blind_positions: Vec<usize> = events
        .iter()
        .enumerate()
        .filter(|(_, e)| is_blind_action_applied(e))
        .map(|(i, _)| i)
        .collect();
    let hole_positions: Vec<usize> = events
        .iter()
        .enumerate()
        .filter(|(_, e)| is_hole_cards_dealt(e))
        .map(|(i, _)| i)
        .collect();

    assert!(
        blind_positions.iter().all(|&bi| hole_positions.iter().all(|&hi| bi < hi)),
        "all blind events must precede all HoleCardsDealt events; order: {blind_positions:?} vs {hole_positions:?}"
    );

    // No street-reveal events yet.
    assert_eq!(
        count_events(&events, is_street_revealed),
        0,
        "no StreetRevealed expected during start()"
    );

    // No hand-finished events.
    assert_eq!(
        count_events(&events, is_hand_finished),
        0,
        "no HandFinished expected during start()"
    );

    // SB blind event references seat 0 (dealer/SB in heads-up).
    let sb_event = events.iter().find(|e| {
        matches!(
            e,
            EngineEvent::ActionApplied {
                action: PlayerAction::Blind {
                    kind: BlindKind::Small,
                    ..
                },
                ..
            }
        )
    });
    assert!(sb_event.is_some(), "SB blind event missing");
    if let Some(EngineEvent::ActionApplied {
        seat, contributed, ..
    }) = sb_event
    {
        assert_eq!(*seat, 0, "SB must be seat 0 (dealer in heads-up)");
        assert_eq!(*contributed, 10, "SB blind amount must be 10");
    }

    // BB blind event references seat 1.
    let bb_event = events.iter().find(|e| {
        matches!(
            e,
            EngineEvent::ActionApplied {
                action: PlayerAction::Blind {
                    kind: BlindKind::Big,
                    ..
                },
                ..
            }
        )
    });
    assert!(bb_event.is_some(), "BB blind event missing");
    if let Some(EngineEvent::ActionApplied {
        seat, contributed, ..
    }) = bb_event
    {
        assert_eq!(*seat, 1, "BB must be seat 1");
        assert_eq!(*contributed, 20, "BB blind amount must be 20");
    }

    // PotUpdated after blinds should show 30 (10 + 20).
    let pot_event = events.iter().find(|e| is_pot_updated(e));
    assert!(pot_event.is_some(), "PotUpdated missing");
    if let Some(EngineEvent::PotUpdated { pot, .. }) = pot_event {
        assert_eq!(*pot, 30, "pot after SB+BB blinds must be 30");
    }

    // HoleCardsDealt carries 2-card arrays.
    for ev in events.iter().filter(|e| is_hole_cards_dealt(e)) {
        if let EngineEvent::HoleCardsDealt { cards, .. } = ev {
            assert_eq!(
                cards.len(),
                2,
                "each HoleCardsDealt must carry exactly 2 cards"
            );
        }
    }

    // drain_events() second call returns empty.
    let second = hand.drain_events();
    assert!(second.is_empty(), "second drain_events() must be empty");
}

// ---------------------------------------------------------------------------
// Test 2: hand all-fold preflop — verify full event sequence
// ---------------------------------------------------------------------------

/// All-fold preflop (3-player: UTG folds, SB folds, BB wins):
/// After `start()` + fold + fold:
/// - start() gives: 2 blind ActionApplied, 1 PotUpdated, 3 HoleCardsDealt
/// - each fold gives: 1 ActionApplied{Fold} (contributed=0, no PotUpdated)
/// - `finish()` gives: 1 HandFinished
#[test]
fn all_fold_preflop_event_sequence() {
    let mut hand = GameHand::new_with_rng(
        vec![
            (pid(1), c(500), 0),
            (pid(2), c(500), 1),
            (pid(3), c(500), 2),
        ],
        0,     // dealer_idx=0: dealer=pid(1), SB=pid(2) seat1, BB=pid(3) seat2
        c(20), // bb
        c(10), // sb
        PokerRng::from_seed(200),
    );

    hand.start().expect("start failed");
    let start_events = hand.drain_events();

    // start() should have 2 blinds + 1 PotUpdated + 3 HoleCardsDealt.
    assert_eq!(count_events(&start_events, is_blind_action_applied), 2);
    assert_eq!(count_events(&start_events, is_pot_updated), 1);
    assert_eq!(count_events(&start_events, is_hole_cards_dealt), 3);

    // UTG (pid(1), seat 0) acts first preflop in a 3-player game (dealer_idx=0).
    let snap = hand.snapshot();
    let utg = snap.current_actor.expect("should have current actor");
    hand.apply_action(utg, PlayerAction::Fold)
        .expect("UTG fold failed");
    let utg_events = hand.drain_events();

    assert_eq!(
        count_events(&utg_events, is_action_applied),
        1,
        "UTG fold should produce 1 ActionApplied"
    );
    // Fold contributes 0 chips — no PotUpdated.
    assert_eq!(
        count_events(&utg_events, is_pot_updated),
        0,
        "fold contributes 0 chips, no PotUpdated expected"
    );

    // SB folds (2nd action).
    let snap = hand.snapshot();
    let sb = snap
        .current_actor
        .expect("should have current actor after UTG fold");
    hand.apply_action(sb, PlayerAction::Fold)
        .expect("SB fold failed");
    let sb_events = hand.drain_events();
    assert_eq!(count_events(&sb_events, is_action_applied), 1);

    // Hand should now be done (only BB left).
    assert!(hand.is_done(), "hand should be done after all but BB fold");

    // finish() emits HandFinished.
    let result = hand.finish();
    let finish_events = hand.drain_events();
    assert_eq!(
        count_events(&finish_events, is_hand_finished),
        1,
        "finish() must emit exactly 1 HandFinished"
    );

    // Verify the HandFinished carries a result.
    if let Some(EngineEvent::HandFinished {
        result: inner_result,
    }) = finish_events.first()
    {
        assert_eq!(inner_result.deck_seed, result.deck_seed);
    }

    // No StreetRevealed events anywhere (preflop all-fold).
    for evs in [&start_events, &utg_events, &sb_events, &finish_events] {
        assert_eq!(
            count_events(evs, is_street_revealed),
            0,
            "no streets should be revealed in all-fold preflop hand"
        );
    }
}

// ---------------------------------------------------------------------------
// Test 3: hand to showdown — verify StreetRevealed order Flop → Turn → River
// ---------------------------------------------------------------------------

/// A full hand where both players always call/check to showdown.
/// Verifies that StreetRevealed events appear in Flop → Turn → River order
/// across all drained event batches combined.
#[test]
fn showdown_street_revealed_order() {
    let mut hand = GameHand::new_with_rng(
        vec![(pid(1), c(500), 0), (pid(2), c(500), 1)],
        0,
        c(20),
        c(10),
        PokerRng::from_seed(300),
    );

    hand.start().expect("start failed");
    let _ = hand.drain_events(); // discard start events

    let mut all_street_events: Vec<Street> = Vec::new();
    let mut safety = 0u32;

    loop {
        if hand.is_done() {
            break;
        }
        safety += 1;
        assert!(safety < 100, "infinite loop detected");

        let snap = hand.snapshot();
        let actor = match snap.current_actor {
            Some(a) => a,
            None => break,
        };

        // Always call/check.
        let committed = snap
            .players
            .iter()
            .find(|p| p.player_id == actor)
            .map(|p| p.committed_this_street.0)
            .unwrap_or(0);
        let to_call = snap.current_bet.0.saturating_sub(committed);
        let action = if to_call > 0 {
            PlayerAction::Call
        } else {
            PlayerAction::Check
        };

        hand.apply_action(actor, action)
            .expect("apply_action failed");
        let events = hand.drain_events();

        // Collect any StreetRevealed from this batch.
        for ev in &events {
            if let Some(s) = street_of_revealed(ev) {
                all_street_events.push(s);
            }
        }
    }

    // Must have seen Flop, Turn, River — in that order.
    assert_eq!(
        all_street_events.len(),
        3,
        "expected exactly 3 StreetRevealed events (Flop, Turn, River); got: {all_street_events:?}"
    );
    assert_eq!(
        all_street_events[0],
        Street::Flop,
        "first revealed street must be Flop"
    );
    assert_eq!(
        all_street_events[1],
        Street::Turn,
        "second revealed street must be Turn"
    );
    assert_eq!(
        all_street_events[2],
        Street::River,
        "third revealed street must be River"
    );

    // Finish and confirm HandFinished.
    hand.finish();
    let finish_events = hand.drain_events();
    assert_eq!(count_events(&finish_events, is_hand_finished), 1);
}

// ---------------------------------------------------------------------------
// Test 4: all-in runout — single apply_action produces full burst
// ---------------------------------------------------------------------------

/// When all players go all-in, the first `apply_action` that closes preflop
/// triggers an automatic runout: the engine advances Flop → Turn → River
/// in a single call, then transitions to Done.
///
/// A single `drain_events()` after that `apply_action` must return:
///   ActionApplied → PotUpdated → StreetRevealed(Flop) → StreetRevealed(Turn)
///   → StreetRevealed(River) → HandFinished
/// in exactly that order (HandFinished may appear within the burst because
/// `close_street_and_advance` recurses until Done).
#[test]
fn all_in_runout_single_apply_action_full_burst() {
    let mut hand = GameHand::new_with_rng(
        vec![(pid(1), c(1000), 0), (pid(2), c(1000), 1)],
        0,
        c(20),
        c(10),
        PokerRng::from_seed(400),
    );

    hand.start().expect("start failed");
    let _ = hand.drain_events(); // discard start events

    // SB (dealer/seat-0 in heads-up) acts first preflop and goes all-in.
    // This should trigger the runout.
    let snap = hand.snapshot();
    let actor = snap.current_actor.expect("should have current actor");

    hand.apply_action(actor, PlayerAction::AllIn)
        .expect("all-in failed");

    // After SB all-in, BB still needs to respond. Drain intermediate events.
    let _intermediate = hand.drain_events();

    assert!(
        !hand.is_done(),
        "hand should not be done after only one all-in in a 2-player 1000-chip game; \
         test setup invariant broken"
    );

    {
        // BB calls the all-in — this closes preflop and triggers the runout.
        let snap2 = hand.snapshot();
        let bb_actor = snap2.current_actor.expect("BB should be current actor");
        hand.apply_action(bb_actor, PlayerAction::AllIn)
            .expect("BB all-in failed");

        let burst_events = hand.drain_events();

        // The burst must contain the three street reveals.
        let revealed_streets: Vec<Street> =
            burst_events.iter().filter_map(street_of_revealed).collect();

        assert_eq!(
            revealed_streets.len(),
            3,
            "all-in runout must emit Flop, Turn, River StreetRevealed; got: {revealed_streets:?}"
        );
        assert_eq!(revealed_streets[0], Street::Flop, "Flop must be first");
        assert_eq!(revealed_streets[1], Street::Turn, "Turn must be second");
        assert_eq!(revealed_streets[2], Street::River, "River must be third");

        // HandFinished must also be in the burst (emitted by finish() or by close_street).
        // Note: HandFinished is NOT automatically emitted by close_street_and_advance —
        // it's emitted by the explicit finish() call. So we call finish() here.
        assert!(hand.is_done(), "hand must be done after all-in runout");

        hand.finish();
        let finish_burst = hand.drain_events();
        assert_eq!(
            count_events(&finish_burst, is_hand_finished),
            1,
            "finish() must produce 1 HandFinished"
        );

        // Verify FIFO order within the burst: streets must come before HandFinished.
        // (HandFinished is in the finish_burst, streets are in burst_events — ordering
        //  between batches is preserved because the server drains after each call.)
        let flop_pos = burst_events
            .iter()
            .position(|e| {
                matches!(
                    e,
                    EngineEvent::StreetRevealed {
                        street: Street::Flop,
                        ..
                    }
                )
            })
            .expect("Flop event missing");
        let turn_pos = burst_events
            .iter()
            .position(|e| {
                matches!(
                    e,
                    EngineEvent::StreetRevealed {
                        street: Street::Turn,
                        ..
                    }
                )
            })
            .expect("Turn event missing");
        let river_pos = burst_events
            .iter()
            .position(|e| {
                matches!(
                    e,
                    EngineEvent::StreetRevealed {
                        street: Street::River,
                        ..
                    }
                )
            })
            .expect("River event missing");

        assert!(
            flop_pos < turn_pos,
            "Flop must precede Turn in event buffer"
        );
        assert!(
            turn_pos < river_pos,
            "Turn must precede River in event buffer"
        );
    }
}

// ---------------------------------------------------------------------------
// Test 5: is_started() cross-crate visibility and lifecycle
// ---------------------------------------------------------------------------

/// `is_started()` must be observable from outside the engine crate.
/// - Returns `false` before `start()`.
/// - Returns `true` after `start()` (preflop).
/// - Returns `true` mid-hand (flop, turn, river).
/// - Returns `true` after the hand is done (`Phase::Done`).
///
/// This test lives in `engine/tests/` (integration test crate, not `src/`)
/// to prove the function is `pub` and callable across the crate boundary.
#[test]
fn is_started_cross_crate_visibility() {
    let mut hand = GameHand::new_with_rng(
        vec![(pid(1), c(500), 0), (pid(2), c(500), 1)],
        0,
        c(20),
        c(10),
        PokerRng::from_seed(500),
    );

    // Before start: not started.
    assert!(
        !hand.is_started(),
        "is_started() must be false before start()"
    );

    hand.start().expect("start failed");
    let _ = hand.drain_events();

    // After start: started.
    assert!(hand.is_started(), "is_started() must be true after start()");

    // Play through to done.
    let mut safety = 0u32;
    loop {
        if hand.is_done() {
            break;
        }
        safety += 1;
        assert!(safety < 100, "infinite loop");

        let snap = hand.snapshot();
        let actor = match snap.current_actor {
            Some(a) => a,
            None => break,
        };

        // is_started() must remain true throughout.
        assert!(
            hand.is_started(),
            "is_started() must remain true during active play (street={:?})",
            snap.street
        );

        let committed = snap
            .players
            .iter()
            .find(|p| p.player_id == actor)
            .map(|p| p.committed_this_street.0)
            .unwrap_or(0);
        let to_call = snap.current_bet.0.saturating_sub(committed);
        let action = if to_call > 0 {
            PlayerAction::Call
        } else {
            PlayerAction::Check
        };

        hand.apply_action(actor, action)
            .expect("apply_action failed");
        let _ = hand.drain_events();
    }

    // After done: still started (Phase::Done != Phase::NotStarted).
    assert!(
        hand.is_started(),
        "is_started() must remain true after Phase::Done"
    );

    // Also confirm is_done() is true.
    assert!(hand.is_done(), "hand must be done at end of loop");

    // finish() still works.
    hand.finish();
    assert!(
        hand.is_started(),
        "is_started() must remain true after finish()"
    );
}

// ---------------------------------------------------------------------------
// Test 6: drain_events() idempotency — second call returns empty Vec
// ---------------------------------------------------------------------------

#[test]
fn drain_events_second_call_is_empty() {
    let mut hand = GameHand::new_with_rng(
        vec![(pid(1), c(200), 0), (pid(2), c(200), 1)],
        0,
        c(20),
        c(10),
        PokerRng::from_seed(600),
    );

    hand.start().expect("start failed");

    let first = hand.drain_events();
    assert!(
        !first.is_empty(),
        "first drain after start() must be non-empty"
    );

    let second = hand.drain_events();
    assert!(
        second.is_empty(),
        "second drain_events() without any intermediate action must be empty"
    );

    // After an action, first drain is non-empty; second is again empty.
    let snap = hand.snapshot();
    if let Some(actor) = snap.current_actor {
        hand.apply_action(actor, PlayerAction::Fold).ok();
        let after_action = hand.drain_events();
        assert!(
            !after_action.is_empty(),
            "drain after apply_action must be non-empty"
        );
        let again = hand.drain_events();
        assert!(
            again.is_empty(),
            "second drain after apply_action must be empty"
        );
    }
}

// ---------------------------------------------------------------------------
// Test 7: ActionApplied.seat uses seat: u8, not PlayerId
// ---------------------------------------------------------------------------

/// Verify that the seat field in ActionApplied corresponds to the seat: u8
/// passed when constructing the hand, not the PlayerId (u64 value).
#[test]
fn action_applied_uses_seat_not_player_id() {
    // Use non-trivial seat numbers to distinguish them from PlayerId values.
    let mut hand = GameHand::new_with_rng(
        vec![(pid(10), c(500), 3), (pid(20), c(500), 7)], // seat 3 and seat 7
        0,
        c(20),
        c(10),
        PokerRng::from_seed(700),
    );

    hand.start().expect("start failed");
    let events = hand.drain_events();

    // Check that blind events use seat 3 and seat 7, not player IDs 10 and 20.
    let blind_seats: Vec<u8> = events
        .iter()
        .filter_map(|e| match e {
            EngineEvent::ActionApplied {
                seat,
                action: PlayerAction::Blind { .. },
                ..
            } => Some(*seat),
            _ => None,
        })
        .collect();

    assert_eq!(blind_seats.len(), 2, "must have 2 blind events");
    assert!(
        blind_seats.contains(&3),
        "SB blind must reference seat 3; got: {blind_seats:?}"
    );
    assert!(
        blind_seats.contains(&7),
        "BB blind must reference seat 7; got: {blind_seats:?}"
    );
    // Sanity: ensure we didn't accidentally use player_id values (10 / 20).
    assert!(
        !blind_seats.iter().any(|&s| s == 10 || s == 20),
        "blind event seats must not equal PlayerId values; got: {blind_seats:?}"
    );
}

// ---------------------------------------------------------------------------
// Test 8: ActionApplied carries current_bet, min_raise_to, next_actor_seat
// ---------------------------------------------------------------------------

/// After `start()`, the blind ActionApplied events carry the post-blind betting state:
/// - current_bet = bb_size (20 in this setup)
/// - min_raise_to = bb_size * 2 = 40  (or current_bet + last_raise_amount)
/// - next_actor_seat = UTG seat (seat 0 in 3-player with dealer_idx=0, UTG=pid(1)=seat 0)
///
/// In heads-up (n=2, dealer_idx=0), SB=seat 0 is first to act preflop.
/// Both blind events carry the SAME post-round state because they are emitted
/// after the round is constructed.
#[test]
fn blind_action_applied_events_carry_betting_state() {
    let mut hand = GameHand::new_with_rng(
        vec![(pid(1), c(1000), 0), (pid(2), c(1000), 1)],
        0,     // dealer_idx=0: heads-up → seat 0 is SB/dealer, seat 1 is BB
        c(20), // bb
        c(10), // sb
        PokerRng::from_seed(800),
    );
    hand.start().expect("start failed");
    let events = hand.drain_events();

    // Collect all ActionApplied{Blind} events.
    let blind_events: Vec<&engine::event::EngineEvent> = events
        .iter()
        .filter(|e| {
            matches!(
                e,
                EngineEvent::ActionApplied {
                    action: PlayerAction::Blind { .. },
                    ..
                }
            )
        })
        .collect();

    assert_eq!(blind_events.len(), 2, "must have 2 blind events");

    for ev in &blind_events {
        if let EngineEvent::ActionApplied {
            current_bet,
            min_raise_to,
            next_actor_seat,
            ..
        } = ev
        {
            // After blinds, current_bet = bb = 20.
            assert_eq!(
                *current_bet, 20,
                "blind event current_bet must be bb=20, got {}",
                current_bet
            );
            // min_raise_to must be Some (street is still open).
            assert!(
                min_raise_to.is_some(),
                "blind event min_raise_to must be Some mid-street"
            );
            // In heads-up preflop, SB acts first → next actor is seat 0.
            assert_eq!(
                *next_actor_seat,
                Some(0u8),
                "blind event next_actor_seat must be SB seat 0 (heads-up)"
            );
        }
    }
}

/// After a raise, ActionApplied carries updated current_bet and next_actor_seat.
#[test]
fn action_applied_after_raise_has_updated_betting_state() {
    let mut hand = GameHand::new_with_rng(
        vec![(pid(1), c(1000), 0), (pid(2), c(1000), 1)],
        0,
        c(20),
        c(10),
        PokerRng::from_seed(900),
    );
    hand.start().expect("start failed");
    let _ = hand.drain_events();

    // Heads-up: seat 0 (SB/dealer) acts first preflop.
    let snap = hand.snapshot();
    let actor = snap.current_actor.expect("actor exists");

    // Raise to 60.
    hand.apply_action(actor, PlayerAction::Raise { amount: c(60) })
        .expect("raise ok");
    let events = hand.drain_events();

    let aa = events
        .iter()
        .find(|e| matches!(e, EngineEvent::ActionApplied { .. }))
        .expect("ActionApplied must be in events");

    if let EngineEvent::ActionApplied {
        current_bet,
        min_raise_to,
        next_actor_seat,
        contributed,
        ..
    } = aa
    {
        assert_eq!(
            *current_bet, 60,
            "current_bet after raise to 60: got {}",
            current_bet
        );
        assert!(
            min_raise_to.is_some(),
            "min_raise_to must be Some mid-street"
        );
        assert!(
            next_actor_seat.is_some(),
            "next_actor_seat must be Some mid-street"
        );
        // BB (seat 1) must be next.
        assert_eq!(
            *next_actor_seat,
            Some(1u8),
            "BB seat 1 is next; got {:?}",
            next_actor_seat
        );
        // Raiser contributed 60 - 10 (already posted SB) = 50 chips.
        assert_eq!(
            *contributed, 50,
            "SB raise contributed: got {}",
            contributed
        );
    }
}

// ---------------------------------------------------------------------------
// Test: StreetRevealed carries next_actor_seat + current_bet=0 on flop
// ---------------------------------------------------------------------------

/// After preflop closes (both players call/check), the StreetRevealed event
/// for the flop must carry:
///   - `current_bet == 0`  (new street, no bet yet)
///   - `next_actor_seat == Some(seat)` (the first player to act post-flop)
///
/// This is the fix for the game-blocking bug where the frontend's
/// `currentActorSeat` stayed `None` after a street advance.
#[test]
fn street_revealed_carries_next_actor() {
    let mut hand = GameHand::new_with_rng(
        vec![(pid(1), c(1000), 0), (pid(2), c(1000), 1)],
        0,     // dealer_idx=0: heads-up → seat 0 is SB/dealer, seat 1 is BB
        c(20), // bb
        c(10), // sb
        PokerRng::from_seed(42),
    );
    hand.start().expect("start ok");
    let _ = hand.drain_events();

    // SB calls → preflop still open (BB can still act).
    let snap = hand.snapshot();
    let sb = snap
        .current_actor
        .expect("SB is first actor heads-up preflop");
    hand.apply_action(sb, PlayerAction::Call)
        .expect("sb call ok");
    let _ = hand.drain_events();

    // BB checks → closes preflop, triggers flop deal.
    let snap2 = hand.snapshot();
    let bb = snap2.current_actor.expect("BB acts after SB calls");
    hand.apply_action(bb, PlayerAction::Check)
        .expect("bb check ok");
    let events = hand.drain_events();

    // Find the StreetRevealed event for the Flop.
    let flop_event = events.iter().find(|e| {
        matches!(
            e,
            EngineEvent::StreetRevealed {
                street: Street::Flop,
                ..
            }
        )
    });

    assert!(
        flop_event.is_some(),
        "StreetRevealed{{Flop}} missing after preflop closes; events={events:?}"
    );

    if let Some(EngineEvent::StreetRevealed {
        street,
        new_cards,
        current_bet,
        min_raise_to,
        next_actor_seat,
    }) = flop_event
    {
        assert_eq!(*street, Street::Flop, "event must be Flop");
        assert_eq!(new_cards.len(), 3, "Flop must deal 3 cards");
        assert_eq!(
            *current_bet, 0,
            "current_bet at start of new street must be 0, got {current_bet}"
        );
        assert!(
            min_raise_to.is_some(),
            "min_raise_to must be Some on flop (players can still bet); got {min_raise_to:?}"
        );
        assert!(
            next_actor_seat.is_some(),
            "next_actor_seat must be Some on flop (not a runout); got {next_actor_seat:?}"
        );
    }
}

/// ActionApplied at end-of-street has next_actor_seat = None and min_raise_to = None.
#[test]
fn action_applied_closing_action_has_none_next_actor() {
    let mut hand = GameHand::new_with_rng(
        vec![(pid(1), c(1000), 0), (pid(2), c(1000), 1)],
        0,
        c(20),
        c(10),
        PokerRng::from_seed(901),
    );
    hand.start().expect("start failed");
    let _ = hand.drain_events();

    // SB calls → preflop still open (BB can still act).
    let snap = hand.snapshot();
    let sb = snap.current_actor.expect("SB actor");
    hand.apply_action(sb, PlayerAction::Call)
        .expect("sb call ok");
    let _ = hand.drain_events();

    // BB checks → this closes the preflop betting round.
    let snap2 = hand.snapshot();
    if let Some(bb) = snap2.current_actor {
        hand.apply_action(bb, PlayerAction::Check)
            .expect("bb check ok");
        let events = hand.drain_events();

        // The ActionApplied for BB's check (the closing action).
        let closing = events
            .iter()
            .find(|e| matches!(e, EngineEvent::ActionApplied { .. }))
            .expect("ActionApplied must be in events");

        if let EngineEvent::ActionApplied {
            next_actor_seat,
            min_raise_to,
            ..
        } = closing
        {
            assert!(
                next_actor_seat.is_none(),
                "next_actor_seat must be None after street closes; got {:?}",
                next_actor_seat
            );
            assert!(
                min_raise_to.is_none(),
                "min_raise_to must be None after street closes; got {:?}",
                min_raise_to
            );
        }
    }
}
