//! `GameHand` unit tests (moved out of `game.rs`: the inline test modules were
//! more than half the file). Same module path, same `use super::*`.

use super::pots::distribute_pots;
use super::*;
use crate::card::{Card, Rank, Suit};

fn pid(n: u64) -> PlayerId {
    PlayerId::new(n)
}
fn c(n: u32) -> Chips {
    Chips(n)
}
fn card(r: Rank, s: Suit) -> Card {
    Card::new(r, s)
}

/// The pre-optimization `preview_action_transition`, verbatim: a full deep
/// `GameHand::clone()` plus a second speculative apply on a cloned betting
/// round. Kept in the test module as the oracle the cheap preview must
/// reproduce byte-for-byte.
fn reference_preview(
    hand: &GameHand,
    player_id: PlayerId,
    action: &PlayerAction,
) -> Result<ActionTransitionPreview, ActionError> {
    match &hand.phase {
        Phase::NotStarted => return Err(ActionError::HandNotStarted),
        Phase::Done => return Err(ActionError::HandFinished),
        Phase::Betting(_) => {}
    }
    if !hand.seats.iter().any(|seat| seat.player_id == player_id) {
        return Err(ActionError::NotInHand);
    }
    let mut round_after_action = hand
        .betting_round
        .clone()
        .ok_or(ActionError::HandFinished)?;
    let before = round_after_action
        .player_states()
        .iter()
        .find(|player| player.player_id == player_id)
        .ok_or(ActionError::NotInHand)?;
    let stack_before = before.stack;
    let committed_before = before.contributed;
    let current_bet_before = round_after_action.current_bet();
    let min_raise_to_before = round_after_action
        .current_player_can_raise()
        .then(|| round_after_action.min_raise_to());

    round_after_action.apply_action(player_id, action)?;
    let round_closed_after = round_after_action.is_done();

    let mut hand_after = hand.clone();
    let after_snapshot = hand_after.apply_action(player_id, action.clone())?;
    let after = after_snapshot
        .players
        .iter()
        .find(|player| player.player_id == player_id)
        .expect("previewed player remains in hand snapshot");
    let next_actor = after_snapshot.current_actor;
    let next_actor_can_raise_after = next_actor.is_some()
        && hand_after
            .betting_round_ref()
            .is_some_and(BettingRound::current_player_can_raise);
    let betting_terminal_after = hand_after.is_done()
        || hand_after
            .betting_round_ref()
            .is_none_or(BettingRound::is_done);
    Ok(ActionTransitionPreview {
        stack_before,
        committed_before,
        current_bet_before,
        min_raise_to_before,
        stack_after: after.stack,
        committed_after: after.committed_this_street,
        current_bet_after: after_snapshot.current_bet,
        min_raise_to_after: after_snapshot.min_raise_to,
        all_in_after: after.all_in,
        round_closed_after,
        next_actor,
        next_actor_can_raise_after,
        street_after: after_snapshot.street,
        betting_terminal_after,
        hand_done_after: hand_after.is_done(),
    })
}

/// Fix (2): the cheap preview (no deep clone of the action/event history,
/// one speculative apply instead of two) must be byte-identical to the old
/// clone-everything implementation — for every candidate action at every
/// decision point of a set of scripted hands, including the rejected ones.
#[test]
fn preview_action_transition_matches_full_clone_and_apply() {
    let mut compared = 0usize;
    for seed in 0u64..12 {
        let seats = if seed % 3 == 0 {
            vec![(pid(1), c(400), 0), (pid(2), c(400), 1)]
        } else {
            vec![
                (pid(1), c(400), 0),
                (pid(2), c(250), 1),
                (pid(3), c(1000), 2),
            ]
        };
        let mut hand = GameHand::new_with_rng(
            seats,
            (seed as usize) % 3,
            c(20),
            c(10),
            PokerRng::from_seed(seed),
        );
        hand.start().expect("start ok");

        for step in 0..40 {
            if hand.is_done() {
                break;
            }
            let Some(actor) = hand.snapshot().current_actor else {
                break;
            };
            let candidates = [
                PlayerAction::Fold,
                PlayerAction::Check,
                PlayerAction::Call,
                PlayerAction::AllIn,
                PlayerAction::Raise { amount: c(40) },
                PlayerAction::Raise { amount: c(120) },
                PlayerAction::Raise { amount: c(7) },
            ];
            // Probe the acting player AND a non-actor / unknown player, so
            // the rejection paths are compared too.
            for probe in [actor, pid(3), pid(99)] {
                for candidate in &candidates {
                    assert_eq!(
                            hand.preview_action_transition(probe, candidate),
                            reference_preview(&hand, probe, candidate),
                            "preview diverged (seed {seed}, step {step}, player {probe:?}, action {candidate:?})"
                        );
                    compared += 1;
                }
            }
            // Advance with a legal action, favouring lines that close
            // streets and reach showdown.
            let chosen = match step % 4 {
                0 => PlayerAction::Call,
                1 => PlayerAction::Check,
                2 => PlayerAction::Raise { amount: c(60) },
                _ => PlayerAction::Call,
            };
            let applied = hand
                .apply_action(actor, chosen)
                .or_else(|_| hand.apply_action(actor, PlayerAction::Call))
                .or_else(|_| hand.apply_action(actor, PlayerAction::Check));
            if applied.is_err() {
                hand.apply_action(actor, PlayerAction::Fold)
                    .expect("fold is always legal for the actor");
            }
        }
    }
    assert!(
        compared > 500,
        "expected a broad comparison, got {compared}"
    );
}

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

#[test]
fn live_utg_straddle_posts_and_moves_first_action_left() {
    let mut hand = GameHand::new_with_rng(
        vec![
            (pid(1), c(1000), 0),
            (pid(2), c(1000), 1),
            (pid(3), c(1000), 2),
            (pid(4), c(1000), 3),
        ],
        0,
        c(20),
        c(10),
        PokerRng::from_seed(43),
    )
    .with_forced_straddle(c(40));

    let snap = hand.start().expect("straddled hand starts");
    assert_eq!(hand.actions.len(), 3);
    assert!(matches!(
        hand.actions[2].action,
        PlayerAction::Blind {
            kind: BlindKind::Straddle,
            amount: Chips(40)
        }
    ));
    assert_eq!(hand.actions[2].seq, 2);
    assert_eq!(snap.pot, c(70));
    assert_eq!(snap.current_bet, c(40));
    assert_eq!(snap.min_raise_to, Some(c(80)));
    assert_eq!(
        snap.current_actor,
        Some(pid(1)),
        "action opens left of UTG straddler"
    );

    let events = hand.drain_events();
    let action_seqs: Vec<u16> = events
        .iter()
        .filter_map(|event| match event {
            EngineEvent::ActionApplied { action_seq, .. } => Some(*action_seq),
            _ => None,
        })
        .collect();
    assert_eq!(
        action_seqs,
        vec![0, 1, 2],
        "forced posts stay chronological"
    );
    assert!(matches!(
        events.first(),
        Some(EngineEvent::HandStarted {
            straddle_seat: Some(3),
            straddle_amount: Some(40),
            ..
        })
    ));
    assert!(events.iter().any(|event| matches!(
        event,
        EngineEvent::ActionApplied {
            seat: 3,
            action: PlayerAction::Blind {
                kind: BlindKind::Straddle,
                ..
            },
            current_bet: 40,
            min_raise_to: Some(80),
            next_actor_seat: Some(0),
            action_seq: 2,
            ..
        }
    )));
}

/// discovery-r1 BUG-175 — an all-in that exactly MATCHES the straddle is a
/// call, and must not be projected as a raise.
///
/// This is the discriminating case. With a 2 BB straddle the opening wager
/// is 40; seeding `current_bet` and `last_full_raise` from `big_blind` (20)
/// made a 40 all-in look like a 20-over-20 raise — `increased_bet` AND
/// `full_raise` — so `strategy_action()` handed the bots and the coach a
/// 3-bet that never happened.
#[test]
fn straddled_all_in_matching_the_bring_in_is_a_call_not_a_raise() {
    let mut hand = GameHand::new_with_rng(
        vec![
            (pid(1), c(40), 0), // exactly the straddle: an all-in CALL
            (pid(2), c(1000), 1),
            (pid(3), c(1000), 2),
            (pid(4), c(1000), 3),
        ],
        0,
        c(20),
        c(10),
        PokerRng::from_seed(45),
    )
    .with_forced_straddle(c(40));

    let snap = hand.start().expect("straddled hand starts");
    // The engine's own view: the opening wager IS the straddle. (No
    // `min_raise_to` assertion here — the first actor holds exactly 40, so
    // the engine correctly offers them no raise at all.)
    assert_eq!(snap.current_bet, c(40));

    hand.apply_action(pid(1), PlayerAction::AllIn)
        .expect("all-in for exactly the straddle is legal");

    let facts = hand.current_street_action_facts();
    let all_in_fact = facts
        .iter()
        .find(|fact| fact.player_id == pid(1))
        .expect("the all-in is projected");
    assert!(
        !all_in_fact.increased_bet,
        "40 only MATCHES the 40 straddle — it does not increase the wager; \
             the big-blind baseline (20) reported an increase"
    );
    assert!(
        !all_in_fact.full_raise,
        "an all-in call is never a full raise; the big-blind baseline made \
             its phantom 20 delta clear a phantom 20 increment"
    );
    assert_eq!(
        all_in_fact.strategy_action(),
        PlayerAction::Call,
        "a non-full-raise projects to Call — this is the fabricated raise \
             level the bots and coach were fed (BUG-175)"
    );
    assert!(all_in_fact.is_call_like(), "an all-in call is call-like");
}

/// BUG-175 companion — an all-in strictly BETWEEN the big blind and the
/// straddle is an under-call, so it must not read as a raise either.
#[test]
fn straddled_short_all_in_below_the_bring_in_is_not_an_increase() {
    let mut hand = GameHand::new_with_rng(
        vec![
            (pid(1), c(30), 0), // short stack: can only reach 30 < 40
            (pid(2), c(1000), 1),
            (pid(3), c(1000), 2),
            (pid(4), c(1000), 3),
        ],
        0,
        c(20),
        c(10),
        PokerRng::from_seed(46),
    )
    .with_forced_straddle(c(40));

    hand.start().expect("straddled hand starts");
    hand.apply_action(pid(1), PlayerAction::AllIn)
        .expect("short all-in call is legal");

    let facts = hand.current_street_action_facts();
    let all_in_fact = facts
        .iter()
        .find(|fact| fact.player_id == pid(1))
        .expect("the all-in is projected");
    assert!(
        !all_in_fact.increased_bet,
        "30 < the 40 straddle bring-in, so this is an under-call, not a \
             raise; the big-blind baseline (20) wrongly flagged it increased"
    );
    assert!(all_in_fact.is_call_like(), "an under-call is call-like");
}

/// discovery-r1 BUG-196 (same defect as BUG-175, reported with the raise
/// ladder) — a legal 3-bet over a straddled open must stay a FULL raise.
///
/// BB 20 / straddle 40. An open to 100 is a 60 increment, so the next full
/// raise must be to >= 170. Measuring from the big blind instead of the
/// straddle recorded that open as an 80 increment, so the legal 3-bet to 170
/// (delta 70) fell short of the phantom 80 and was classified
/// `full_raise = false` — `strategy_action()` then handed the bot and the
/// coach a `Call` where a 3-bet had happened, collapsing a whole level of
/// preflop chart depth.
#[test]
fn straddled_three_bet_keeps_full_raise_depth() {
    let mut hand = GameHand::new_with_rng(
        vec![
            (pid(1), c(1000), 0),
            (pid(2), c(1000), 1),
            (pid(3), c(1000), 2),
            (pid(4), c(1000), 3),
        ],
        0,
        c(20),
        c(10),
        PokerRng::from_seed(48),
    )
    .with_forced_straddle(c(40));

    hand.start().expect("straddled hand starts");
    // pid(1) opens to 100 (a 60 increment over the 40 bring-in).
    hand.apply_action(pid(1), PlayerAction::Raise { amount: c(100) })
        .expect("open to 100 is legal");
    // pid(2) 3-bets to 170 — exactly the minimum (70 >= 60 increment).
    hand.apply_action(pid(2), PlayerAction::Raise { amount: c(170) })
        .expect("min 3-bet to 170 is legal");

    let facts = hand.current_street_action_facts();
    let three_bet = facts
        .iter()
        .find(|fact| fact.player_id == pid(2))
        .expect("the 3-bet is projected");
    assert!(three_bet.increased_bet, "170 > 100 increases the wager");
    assert!(
        three_bet.full_raise,
        "delta 70 >= the real 60 increment — a full 3-bet; the big-blind \
             baseline inflated the open's increment to 80 and lost this"
    );
    assert_eq!(
        three_bet.strategy_action(),
        PlayerAction::Raise { amount: c(170) },
        "a full 3-bet must NOT be projected down to Call (BUG-196)"
    );
}

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

/// BUG-175 guard — the fix must not disturb the NO-straddle baseline, and
/// postflop keeps the big blind as its raise increment (a straddle only
/// raises the PREFLOP opening wager).
#[test]
fn unstraddled_preflop_facts_keep_the_big_blind_baseline() {
    let mut hand = GameHand::new_with_rng(
        vec![
            (pid(1), c(1000), 0),
            (pid(2), c(1000), 1),
            (pid(3), c(1000), 2),
        ],
        0,
        c(20),
        c(10),
        PokerRng::from_seed(47),
    );

    let snap = hand.start().expect("plain hand starts");
    assert_eq!(snap.current_bet, c(20), "no straddle → BB is the bring-in");

    hand.apply_action(pid(1), PlayerAction::AllIn)
        .expect("all-in is legal");
    let facts = hand.current_street_action_facts();
    let fact = facts
        .iter()
        .find(|fact| fact.player_id == pid(1))
        .expect("the all-in is projected");
    assert!(
        fact.increased_bet && fact.full_raise,
        "a 1000 all-in over a 20 big blind is still a full raise"
    );
}

#[test]
fn configured_straddle_is_suppressed_heads_up() {
    let mut hand = GameHand::new_with_rng(
        vec![(pid(1), c(1000), 0), (pid(2), c(1000), 1)],
        0,
        c(20),
        c(10),
        PokerRng::from_seed(44),
    )
    .with_forced_straddle(c(40));

    let snap = hand.start().expect("heads-up starts without straddle");
    assert_eq!(hand.actions.len(), 2);
    assert_eq!(snap.pot, c(30));
    assert_eq!(snap.current_bet, c(20));
    assert_eq!(snap.min_raise_to, Some(c(40)));
    assert!(matches!(
        hand.drain_events().first(),
        Some(EngineEvent::HandStarted {
            straddle_seat: None,
            straddle_amount: None,
            ..
        })
    ));
}

#[test]
fn short_all_in_straddle_does_not_inflate_the_bring_in() {
    let mut hand = GameHand::new_with_rng(
        vec![
            (pid(1), c(15), 0),
            (pid(2), c(1000), 1),
            (pid(3), c(1000), 2),
        ],
        0,
        c(20),
        c(10),
        PokerRng::from_seed(45),
    )
    .with_forced_straddle(c(40));

    let snap = hand.start().expect("short straddled hand starts");
    assert_eq!(snap.pot, c(45));
    assert_eq!(snap.current_bet, c(20));
    assert_eq!(snap.min_raise_to, Some(c(40)));
    assert_eq!(
        snap.current_actor,
        Some(pid(2)),
        "action still opens left of straddler"
    );
    let short = snap.players.iter().find(|p| p.player_id == pid(1)).unwrap();
    assert_eq!(short.stack, c(0));
    assert!(short.all_in);
    assert!(matches!(
        hand.actions[2].action,
        PlayerAction::Blind {
            kind: BlindKind::Straddle,
            amount: Chips(15)
        }
    ));
}

#[test]
fn validate_action_is_pure_for_valid_and_invalid_inputs() {
    let mut hand = GameHand::new_with_rng(
        vec![(pid(1), c(1000), 0), (pid(2), c(1000), 1)],
        0,
        c(20),
        c(10),
        PokerRng::from_seed(43),
    );
    let before = hand.start().expect("start ok");
    let actor = before.current_actor.expect("preflop actor");
    let wrong_actor = if actor == pid(1) { pid(2) } else { pid(1) };
    let seq = hand.next_action_seq();
    let actions = hand.actions.len();
    let events = hand.events.len();
    let snapshot_before = hand.snapshot();

    hand.validate_action(actor, &PlayerAction::Call)
        .expect("current actor call validates");
    assert!(matches!(
        hand.validate_action(wrong_actor, &PlayerAction::Call),
        Err(ActionError::NotYourTurn)
    ));

    let snapshot_after = hand.snapshot();
    assert_eq!(hand.next_action_seq(), seq);
    assert_eq!(hand.actions.len(), actions);
    assert_eq!(hand.events.len(), events);
    assert_eq!(snapshot_after.current_actor, snapshot_before.current_actor);
    assert_eq!(snapshot_after.pot, snapshot_before.pot);
    assert_eq!(snapshot_after.current_bet, snapshot_before.current_bet);
    assert_eq!(
        snapshot_after
            .players
            .iter()
            .map(|player| (player.player_id, player.stack, player.committed_this_street))
            .collect::<Vec<_>>(),
        snapshot_before
            .players
            .iter()
            .map(|player| (player.player_id, player.stack, player.committed_this_street))
            .collect::<Vec<_>>()
    );
}

#[test]
fn hu_all_in_opponent_exposes_call_only_and_stabilized_runout_preview() {
    let mut hand = GameHand::new_blind(
        vec![(pid(1), c(100), 0), (pid(2), c(1000), 1)],
        0,
        c(20),
        c(10),
        DeckSeed::default(),
    );
    hand.start().expect("start blind hand");
    assert_eq!(hand.snapshot().current_actor, Some(pid(1)));
    hand.apply_action(pid(1), PlayerAction::AllIn)
        .expect("button/SB shove");

    let snap = hand.snapshot();
    assert_eq!(snap.current_actor, Some(pid(2)));
    assert_eq!(snap.current_bet, c(100));
    assert_eq!(
        snap.min_raise_to, None,
        "the sole live stack may fold/call but cannot build a dry side pot"
    );
    assert_eq!(
        hand.preview_action_transition(pid(2), &PlayerAction::Raise { amount: c(200) })
            .unwrap_err(),
        ActionError::InvalidAction
    );
    assert_eq!(
        hand.preview_action_transition(pid(2), &PlayerAction::AllIn)
            .unwrap_err(),
        ActionError::InvalidAction,
        "an over-call all-in is still an aggressive dry-side-pot wager"
    );

    let call = hand
        .preview_action_transition(pid(2), &PlayerAction::Call)
        .expect("call remains legal");
    assert_eq!(call.min_raise_to_before, None);
    assert!(call.round_closed_after, "the call closes preflop");
    assert_eq!(call.street_after, Street::Flop);
    assert_eq!(call.committed_after, c(0));
    assert_eq!(call.current_bet_after, c(0));
    assert_eq!(call.min_raise_to_after, None);
    assert_eq!(call.next_actor, None, "all-in runout has no flop actor");
    assert!(!call.next_actor_can_raise_after);
    assert!(call.betting_terminal_after);
}

#[test]
fn transition_preview_stabilizes_street_closing_action_into_flop_state() {
    let mut hand = GameHand::new_blind(
        vec![(pid(1), c(1000), 0), (pid(2), c(1000), 1)],
        0,
        c(20),
        c(10),
        DeckSeed::default(),
    );
    hand.start().expect("start blind hand");
    hand.apply_action(pid(1), PlayerAction::Call)
        .expect("SB completes");

    let transition = hand
        .preview_action_transition(pid(2), &PlayerAction::Check)
        .expect("BB option checks");
    assert!(transition.round_closed_after, "check closes preflop");
    assert_eq!(transition.street_after, Street::Flop);
    assert_eq!(transition.stack_after, c(980));
    assert_eq!(transition.committed_after, c(0));
    assert_eq!(transition.current_bet_after, c(0));
    assert_eq!(transition.min_raise_to_after, Some(c(20)));
    assert_eq!(
        transition.next_actor,
        Some(pid(2)),
        "the non-button/BB acts first postflop heads-up"
    );
    assert!(transition.next_actor_can_raise_after);
    assert!(!transition.betting_terminal_after);
}

#[test]
fn short_big_blind_transition_uses_actual_live_contribution() {
    // 3-way 10/20: button folds, SB's posted 10 already covers the BB's
    // short all-in 8. No SB action exists; betting is terminal and only the
    // board runout remains.
    let mut covered = GameHand::new_blind(
        vec![(pid(1), c(100), 0), (pid(2), c(100), 1), (pid(3), c(8), 2)],
        0,
        c(20),
        c(10),
        DeckSeed::default(),
    );
    covered.start().unwrap();
    assert_eq!(covered.snapshot().current_actor, Some(pid(1)));
    let closes = covered
        .preview_action_transition(pid(1), &PlayerAction::Fold)
        .unwrap();
    assert!(closes.round_closed_after);
    assert!(closes.betting_terminal_after);
    assert_eq!(closes.next_actor, None);
    assert_eq!(closes.min_raise_to_after, None);

    // Negative companion: with a 5-chip SB, the short BB's actual 8 leaves
    // a three-chip decision. The fold transition stays preflop, exposes the
    // SB as actor, and advertises no dry-side-pot raise.
    let mut owing = GameHand::new_blind(
        vec![(pid(1), c(100), 0), (pid(2), c(100), 1), (pid(3), c(8), 2)],
        0,
        c(20),
        c(5),
        DeckSeed::default(),
    );
    owing.start().unwrap();
    let remains = owing
        .preview_action_transition(pid(1), &PlayerAction::Fold)
        .unwrap();
    assert!(!remains.round_closed_after);
    assert!(!remains.betting_terminal_after);
    assert_eq!(remains.street_after, Street::Preflop);
    assert_eq!(remains.current_bet_after, c(8));
    assert_eq!(remains.next_actor, Some(pid(2)));
    assert_eq!(remains.min_raise_to_after, None);
    assert!(!remains.next_actor_can_raise_after);
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

#[test]
fn heads_up_fold_preflop() {
    let mut hand = GameHand::new_with_rng(
        vec![(pid(1), c(1000), 0), (pid(2), c(1000), 1)],
        0,
        c(20),
        c(10),
        PokerRng::from_seed(2),
    );
    hand.start().unwrap();
    let actor = hand.snapshot().current_actor.unwrap();
    hand.apply_action(actor, PlayerAction::Fold).unwrap();
    assert!(hand.is_done());
    let result = hand.finish();
    // SB=10 + BB=20 = 30 total. Winner gets 30.
    let total: u32 = result.chips_awarded.values().sum();
    assert_eq!(total, 30);
}

#[test]
fn undealt_board_returns_the_real_preflop_runout_without_advancing_deck() {
    let mut hand = GameHand::new_with_rng(
        vec![(pid(1), c(1000), 0), (pid(2), c(1000), 1)],
        0,
        c(20),
        c(10),
        PokerRng::from_seed(2),
    );
    hand.start().unwrap();
    let actor = hand.snapshot().current_actor.unwrap();
    hand.apply_action(actor, PlayerAction::Fold).unwrap();
    assert!(hand.is_done());

    let before = hand.deck.remaining();
    let runout = hand.undealt_board();

    assert_eq!(runout.len(), 5, "preflop fold leaves five real board cards");
    assert_eq!(
        hand.deck.remaining(),
        before,
        "rabbit hunt must not consume cards"
    );
    assert_eq!(
        runout[0],
        hand.deck.cards()[hand.deck.cards().len() - before]
    );
}

#[test]
fn undealt_board_is_empty_after_a_complete_river() {
    let mut hand = GameHand::new_with_rng(
        vec![(pid(1), c(1000), 0), (pid(2), c(1000), 1)],
        0,
        c(20),
        c(10),
        PokerRng::from_seed(42),
    );
    hand.start().unwrap();

    // Both players check/call through every street. The engine deals the
    // flop, turn, and river before reaching Done.
    loop {
        let actor = hand.snapshot().current_actor;
        let Some(actor) = actor else { break };
        let snap = hand.snapshot();
        let action = if snap.current_bet
            > snap
                .players
                .iter()
                .find(|p| p.player_id == actor)
                .map(|p| p.committed_this_street)
                .unwrap_or(Chips::ZERO)
        {
            PlayerAction::Call
        } else {
            PlayerAction::Check
        };
        hand.apply_action(actor, action).unwrap();
    }
    assert!(hand.is_done());
    assert_eq!(hand.snapshot().board.count(), 5);
    assert!(hand.undealt_board().is_empty());
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

    // The relabelled Check must also appear as the last_action for BB.
    assert_eq!(
        hand.last_action.get(&bb.inner()),
        Some(&PlayerAction::Check),
        "last_action must reflect the relabelled Check"
    );
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

/// Regression (audit 2026-06-03): a fold-around HandResult's lone showdown
/// entry must carry a non-empty `rank_name` (consistent with the contested
/// branch), not a real `rank` next to an empty string.
#[test]
fn fold_around_showdown_entry_has_rank_name() {
    let mut hand = GameHand::new_with_rng(
        vec![(pid(1), c(1000), 0), (pid(2), c(1000), 1)],
        0,
        c(20),
        c(10),
        PokerRng::from_seed(5),
    );
    hand.start().unwrap();
    let actor = hand.snapshot().current_actor.unwrap();
    hand.apply_action(actor, PlayerAction::Fold).unwrap();
    assert!(hand.is_done());
    let result = hand.finish();
    assert_eq!(
        result.showdown.len(),
        1,
        "fold-around has one showdown entry"
    );
    let entry = &result.showdown[0];
    assert!(
        !entry.rank_name.is_empty(),
        "lone showdown entry must carry a rank_name"
    );
    assert_eq!(
        entry.rank_name,
        entry.rank.name(),
        "rank_name must match the computed rank"
    );
}

#[test]
fn heads_up_all_in_preflop_completes_without_panic() {
    let mut hand = GameHand::new_with_rng(
        vec![(pid(1), c(1000), 0), (pid(2), c(1000), 1)],
        0,
        c(20),
        c(10),
        PokerRng::from_seed(1),
    );
    hand.start().unwrap();
    loop {
        if hand.is_done() {
            break;
        }
        let actor = match hand.snapshot().current_actor {
            Some(a) => a,
            None => break,
        };
        hand.apply_action(actor, PlayerAction::AllIn).unwrap();
    }
    let result = hand.finish();
    let total: u32 = result.chips_awarded.values().sum();
    assert_eq!(total, 2000);
}

/// REGRESSION (bug-sweep 2026-05-29, finding #5): a side pot whose entire
/// eligible set folded (no contender reaches showdown) must NOT silently
/// destroy chips — `distribute_pots` returns the uncontested layer equally
/// to its eligible contributors. Chip conservation (chips in == chips out)
/// is the canonical engine invariant and must hold in this degenerate "no
/// contest" case.
///
/// This exercises the `contenders.is_empty()` branch of `distribute_pots`
/// DIRECTLY. It used to be driven through a full HU+side-pot hand where both
/// deep stacks folded the side pot away on the turn — but the 2026-05-29
/// all-in-runout fix in `BettingRound::check_done` now closes betting the
/// instant only one live stack remains owing nothing, so that black-box path
/// can no longer reach a fully-folded side pot (see
/// `all_but_one_all_in_runs_out_to_showdown`). Testing the function directly
/// keeps the refund branch covered regardless of black-box reachability.
#[test]
fn orphaned_side_pot_does_not_destroy_chips() {
    // A 200-chip side pot whose entire eligible set {A=pid(2), B=pid(3)}
    // folded: neither appears in the showdown card set, so the pot has zero
    // contenders. (The only other player — an all-in short stack — is not
    // eligible for this higher side-pot layer.)
    let side_pots = vec![SidePot {
        cap: c(100),
        amount: c(200),
        eligible: vec![pid(2), pid(3)],
        refund_to: Vec::new(),
    }];
    let no_contenders: Vec<(PlayerId, HoleCards)> = Vec::new();
    let button_order = HashMap::new();
    let results = distribute_pots(
        &side_pots,
        &no_contenders,
        &BoardCards::empty(),
        &button_order,
        &HashSet::new(),
    );

    // The pot must be RETURNED, not dropped: one PotResult covering the full
    // 200, split equally between the two eligible contributors.
    assert_eq!(
        results.len(),
        1,
        "uncontested side pot must be refunded, not dropped"
    );
    let refunded: u32 = results[0].winners.iter().map(|w| w.amount_won.0).sum();
    assert_eq!(refunded, 200, "chip conservation: full pot amount refunded");
    let mut winners: Vec<u64> = results[0]
        .winners
        .iter()
        .map(|w| w.player_id.inner())
        .collect();
    winners.sort_unstable();
    assert_eq!(
        winners,
        vec![pid(2).inner(), pid(3).inner()],
        "refund goes to the eligible contributors"
    );
}

/// REGRESSION (2026-05-29): all-but-one all-in must run the board out
/// automatically instead of prompting the lone chip-holder street by street.
///
/// Reproduces the reported production hand: heads-up, the short stack shoves
/// all-in pre-flop and the deep stack CALLS. There is no betting decision
/// left (only one player can act and owes nothing), so the engine must NOT
/// expose a `current_actor` on the flop — it must deal flop/turn/river to
/// showdown on its own. Before the `check_done` fix the deep stack was asked
/// to Fold/Raise/Check on every street. Distinct from
/// `heads_up_all_in_preflop_completes_without_panic`, where BOTH players are
/// all-in (n_can_act == 0); here the deep stack still has chips behind
/// (n_can_act == 1).
#[test]
fn all_but_one_all_in_runs_out_to_showdown() {
    // dealer=0 => pid(1) is the button/SB and acts first pre-flop heads-up.
    let villain_start = 100u32; // short — will shove
    let hero_start = 1000u32; // deep — calls and keeps chips behind
    let mut hand = GameHand::new_with_rng(
        vec![
            (pid(1), c(villain_start), 0), // villain — SB, short
            (pid(2), c(hero_start), 1),    // hero — BB, deep
        ],
        0,
        c(20), // big blind
        c(10), // small blind
        PokerRng::from_seed(7),
    );
    hand.start().unwrap();
    let total_start = villain_start + hero_start;

    // Pre-flop: villain (SB) shoves all-in 100, hero (BB) calls to 100.
    assert_eq!(
        hand.snapshot().current_actor,
        Some(pid(1)),
        "button acts first heads-up pre-flop"
    );
    hand.apply_action(pid(1), PlayerAction::AllIn).unwrap();
    let hero = hand.snapshot().current_actor.unwrap();
    assert_eq!(hero, pid(2), "hero (BB) acts after the shove");
    hand.apply_action(hero, PlayerAction::Call).unwrap();

    // The instant the call matches the shove, betting is closed for the rest
    // of the hand: the board runs out and the hand completes with NO actor.
    assert!(
        hand.is_done(),
        "all-but-one all-in must run out to showdown, not prompt the deep stack"
    );
    assert!(
        hand.snapshot().current_actor.is_none(),
        "no current_actor once only one live stack remains with nothing owed"
    );

    // Chip conservation across the auto-runout.
    let result = hand.finish();
    let total_final: u32 = result.final_stacks.values().sum();
    assert_eq!(
        total_final, total_start,
        "chip conservation across all-in runout: {total_final} vs {total_start}"
    );
}

/// B-M4-001: After one player folds preflop in a 3-player game, that player
/// must NOT appear as current_actor on the flop, turn, or river.
#[test]
fn three_player_fold_preflop_actor_never_revisited() {
    // dealer=0, SB=pid(2) (seat 1), BB=pid(3) (seat 2), UTG=pid(1) (seat 0) acts first preflop.
    let mut hand = GameHand::new_with_rng(
        vec![
            (pid(1), c(500), 0),
            (pid(2), c(500), 1),
            (pid(3), c(500), 2),
        ],
        0, // dealer_idx = 0
        c(20),
        c(10),
        PokerRng::from_seed(7),
    );
    hand.start().unwrap();

    // pid(1) is UTG — fold preflop.
    let snap = hand.snapshot();
    assert_eq!(
        snap.current_actor,
        Some(pid(1)),
        "UTG should act first preflop"
    );
    hand.apply_action(pid(1), PlayerAction::Fold).unwrap();

    // SB (pid(2)) and BB (pid(3)) complete preflop.
    let snap = hand.snapshot();
    let actor = snap.current_actor.unwrap();
    hand.apply_action(actor, PlayerAction::Call).unwrap(); // SB calls
    let snap = hand.snapshot();
    if let Some(actor) = snap.current_actor {
        hand.apply_action(actor, PlayerAction::Check).unwrap(); // BB checks
    }

    // Now on flop (or hand could be done in edge cases — either way, pid(1) must never be actor).
    let mut safety = 0;
    loop {
        if hand.is_done() {
            break;
        }
        safety += 1;
        if safety > 30 {
            panic!("too many iterations");
        }
        let snap = hand.snapshot();
        let actor = match snap.current_actor {
            Some(a) => a,
            None => break,
        };
        assert_ne!(
            actor,
            pid(1),
            "folded player pid(1) must never be current_actor; street={:?}",
            snap.street
        );
        // Check/call to advance.
        let committed = snap
            .players
            .iter()
            .find(|p| p.player_id == actor)
            .map(|p| p.committed_this_street.0)
            .unwrap_or(0);
        let to_call = snap.current_bet.0.saturating_sub(committed);
        if to_call > 0 {
            hand.apply_action(actor, PlayerAction::Call).unwrap();
        } else {
            hand.apply_action(actor, PlayerAction::Check).unwrap();
        }
    }
    let result = hand.finish();
    // Total pot = 10 (SB) + 20 (BB) + 20 (UTG call... wait UTG folded, so only SB+BB = 30).
    // Actually SB called (20) + BB checked: pot = 20 + 20 = 40. pid(1) folded for 0.
    let total: u32 = result.chips_awarded.values().sum();
    assert!(total > 0, "winner must receive chips");
    // Verify pid(1) is not in showdown winners.
    for pot in &result.pots {
        for winner in &pot.winners {
            assert_ne!(winner.player_id, pid(1), "folded player cannot win");
        }
    }
}

/// P0-1 regression: when all players fold preflop except one,
/// the engine must immediately end the hand with NO community cards.
/// Board length == 0 (no flop/turn/river).
#[test]
fn fold_around_preflop_skips_board() {
    // dealer=pid(1), SB=pid(2), BB=pid(3), UTG=pid(1) acts first preflop.
    let mut hand = GameHand::new_with_rng(
        vec![
            (pid(1), c(500), 0),
            (pid(2), c(500), 1),
            (pid(3), c(500), 2),
        ],
        0, // dealer_idx = 0
        c(20),
        c(10),
        PokerRng::from_seed(42),
    );
    hand.start().unwrap();

    // UTG (pid1) folds first.
    let snap = hand.snapshot();
    let actor = snap.current_actor.unwrap();
    assert_eq!(actor, pid(1), "UTG should act first preflop");
    hand.apply_action(actor, PlayerAction::Fold).unwrap();

    // SB (pid2) folds.
    let snap = hand.snapshot();
    let actor = snap.current_actor.unwrap();
    assert_eq!(actor, pid(2), "SB should act next");
    hand.apply_action(actor, PlayerAction::Fold).unwrap();

    // Only BB (pid3) remains — hand must terminate immediately.
    assert!(
        hand.is_done(),
        "fold-around: hand must end immediately when only 1 player remains"
    );

    let snap = hand.snapshot();
    // Board must be empty — no community cards when fold-around.
    assert!(
        snap.board.flop.is_none(),
        "fold-around preflop: flop must NOT be dealt"
    );
    assert!(
        snap.board.turn.is_none(),
        "fold-around preflop: turn must NOT be dealt"
    );
    assert!(
        snap.board.river.is_none(),
        "fold-around preflop: river must NOT be dealt"
    );
}

/// Regression (audit 2026-06-03, TDA Rule 25): the odd chip of a split pot
/// goes to the tied winner in the EARLIEST position (lowest button-relative
/// order / first seat left of the button), not to whoever happens to be
/// first in `pot.eligible` (action) order.
#[test]
fn odd_chip_goes_to_earliest_position_winner() {
    // Board is a Broadway straight both players play → exact tie.
    let board = BoardCards {
        flop: Some([
            card(Rank::Ace, Suit::Clubs),
            card(Rank::King, Suit::Hearts),
            card(Rank::Queen, Suit::Diamonds),
        ]),
        turn: Some(card(Rank::Jack, Suit::Clubs)),
        river: Some(card(Rank::Ten, Suit::Spades)),
    };
    // Both hole sets are dominated by the board (play-the-board straight).
    let p1_hole = HoleCards::new(
        card(Rank::Two, Suit::Hearts),
        card(Rank::Three, Suit::Hearts),
    );
    let p3_hole = HoleCards::new(
        card(Rank::Four, Suit::Diamonds),
        card(Rank::Five, Suit::Diamonds),
    );
    let eligible = vec![(pid(1), p1_hole), (pid(3), p3_hole)];
    // 101-chip pot, eligible in action order [pid(1), pid(3)].
    let pots = vec![SidePot {
        cap: c(101),
        amount: c(101),
        eligible: vec![pid(1), pid(3)],
        refund_to: Vec::new(),
    }];

    // Case A: pid(3) is earliest position (order 0) → it gets the odd chip
    // even though pid(1) is first in pot.eligible.
    let mut order_a = HashMap::new();
    order_a.insert(pid(3).inner(), 0usize);
    order_a.insert(pid(1).inner(), 1usize);
    let res_a = distribute_pots(&pots, &eligible, &board, &order_a, &HashSet::new());
    assert_eq!(res_a.len(), 1);
    let p3_won_a = res_a[0]
        .winners
        .iter()
        .find(|w| w.player_id == pid(3))
        .map(|w| w.amount_won.0)
        .unwrap();
    let p1_won_a = res_a[0]
        .winners
        .iter()
        .find(|w| w.player_id == pid(1))
        .map(|w| w.amount_won.0)
        .unwrap();
    assert_eq!(p3_won_a, 51, "earliest-position pid(3) gets the odd chip");
    assert_eq!(p1_won_a, 50);
    assert_eq!(p3_won_a + p1_won_a, 101, "chip conservation");

    // Case B: pid(1) is earliest position (order 0) → it gets the odd chip.
    let mut order_b = HashMap::new();
    order_b.insert(pid(1).inner(), 0usize);
    order_b.insert(pid(3).inner(), 1usize);
    let res_b = distribute_pots(&pots, &eligible, &board, &order_b, &HashSet::new());
    let p1_won_b = res_b[0]
        .winners
        .iter()
        .find(|w| w.player_id == pid(1))
        .map(|w| w.amount_won.0)
        .unwrap();
    assert_eq!(p1_won_b, 51, "earliest-position pid(1) gets the odd chip");
}

#[test]
fn three_player_full_hand_no_panic() {
    let mut hand = GameHand::new_with_rng(
        vec![
            (pid(1), c(500), 0),
            (pid(2), c(500), 1),
            (pid(3), c(500), 2),
        ],
        0,
        c(20),
        c(10),
        PokerRng::from_seed(99),
    );
    hand.start().unwrap();
    // Everyone checks/calls to showdown.
    let mut safety = 0;
    loop {
        if hand.is_done() {
            break;
        }
        safety += 1;
        if safety > 50 {
            panic!("too many iterations");
        }
        let snap = hand.snapshot();
        let actor = match snap.current_actor {
            Some(a) => a,
            None => break,
        };
        let to_call = snap.current_bet.0.saturating_sub(
            snap.players
                .iter()
                .find(|p| p.player_id == actor)
                .map(|p| p.committed_this_street.0)
                .unwrap_or(0),
        );
        if to_call > 0 {
            hand.apply_action(actor, PlayerAction::Call).unwrap();
        } else {
            hand.apply_action(actor, PlayerAction::Check).unwrap();
        }
    }
    let result = hand.finish();
    let total: u32 = result.chips_awarded.values().sum();
    assert_eq!(total, 60); // 3 * 20
}
