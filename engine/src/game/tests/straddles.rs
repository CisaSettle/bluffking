//! GameHand regression tests: straddles.

use super::*;

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
