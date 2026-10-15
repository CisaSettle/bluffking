//! GameHand regression tests: previews.

use super::*;

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
