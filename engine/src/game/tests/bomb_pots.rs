//! Bomb-pot rules, contribution accounting and sealed-dealing boundaries.
use super::*;

fn hand(stacks: &[u32]) -> GameHand {
    GameHand::new_with_rng(
        stacks
            .iter()
            .enumerate()
            .map(|(i, stack)| (pid(i as u64 + 1), c(*stack), i as u8))
            .collect(),
        0,
        c(20),
        c(10),
        PokerRng::from_seed(87),
    )
}

#[test]
fn bomb_pot_starts_on_flop_without_blinds_or_straddle() {
    let mut hand = hand(&[1000, 1000, 1000])
        .with_ante(c(5))
        .with_forced_straddle(c(40))
        .with_bomb_pot(c(60));
    let snap = hand.start().unwrap();
    assert_eq!(snap.street, Street::Flop);
    assert_eq!(snap.board.count(), 3);
    assert_eq!(snap.pot, c(180));
    assert_eq!(snap.current_bet, c(0));
    assert_eq!(snap.min_raise_to, Some(c(20)));
    assert_eq!(snap.current_actor, Some(pid(2)));
    assert!(snap
        .players
        .iter()
        .all(|p| p.stack == c(940) && p.committed_this_street == c(0)));
    assert!(hand.actions.is_empty());
    assert_eq!(hand.straddle_position(), None);
    assert_eq!(hand.ante(), c(60));
    assert_eq!(hand.bomb_pot_amount(), Some(c(60)));
    let events = hand.drain_events();
    assert!(matches!(
        events.first(),
        Some(EngineEvent::HandStarted {
            ante: 60,
            straddle_amount: None,
            ..
        })
    ));
    assert!(!events
        .iter()
        .any(|e| matches!(e, EngineEvent::ActionApplied { .. })));
    assert!(hand
        .apply_action(pid(2), PlayerAction::Raise { amount: c(19) })
        .is_err());
    hand.apply_action(pid(2), PlayerAction::Raise { amount: c(20) })
        .unwrap();
    assert_eq!(hand.actions[0].seq, 0);
    assert_eq!(hand.actions[0].street, Street::Flop);
}

#[test]
fn bomb_pot_heads_up_non_button_acts_first() {
    let mut hand = hand(&[1000, 1000]).with_bomb_pot(c(60));
    let snap = hand.start().unwrap();
    assert_eq!(snap.current_actor, Some(pid(2)));
    assert_eq!(snap.pot, c(120));
}

#[test]
fn bomb_pot_short_ante_sidepots_and_allin_runout_conserve_chips() {
    let mut hand = hand(&[20, 40, 100]).with_bomb_pot(c(60));
    let snap = hand.start().unwrap();
    assert!(
        hand.is_done(),
        "one funded player cannot bet into only all-in opponents"
    );
    assert_eq!(snap.board.count(), 5);
    assert_eq!(snap.pot, c(120));
    let pots = hand.hand_side_pots();
    assert_eq!(pots[0].amount, c(60));
    assert_eq!(pots[0].eligible.len(), 3);
    assert_eq!(pots[1].amount, c(40));
    assert_eq!(pots[1].eligible.len(), 2);
    assert!(!pots[1].eligible.contains(&pid(1)));
    let result = hand.finish();
    assert_eq!(result.final_stacks.values().sum::<u32>(), 160);
    assert_eq!(result.chips_awarded.values().sum::<u32>(), 120);
    assert!(result.final_stacks[&1] <= 60);
    assert!(
        result.final_stacks[&3] >= 60,
        "unmatched ante excess is returned"
    );
}

#[test]
fn bomb_pot_short_stack_is_skipped_in_flop_action() {
    let mut hand = hand(&[1000, 20, 1000]).with_bomb_pot(c(60));
    let snap = hand.start().unwrap();
    assert_eq!(snap.current_actor, Some(pid(3)));
    assert_eq!(snap.pot, c(140));
    assert!(snap.players[1].all_in);
    assert_eq!(snap.players[1].stack, c(0));
}

#[test]
fn bomb_pot_blind_mode_waits_for_each_verified_board_without_hole_leaks() {
    let mut hand = GameHand::new_blind(
        vec![(pid(1), c(20), 0), (pid(2), c(40), 1)],
        0,
        c(20),
        c(10),
        [0; 32],
    )
    .with_bomb_pot(c(60));
    let snap = hand.start().unwrap();
    assert_eq!(hand.pending_board_street(), Some(Street::Flop));
    assert_eq!(snap.board.count(), 0);
    assert!(!hand.is_done());
    assert!(snap.players.iter().all(|p| p.hole_cards.is_none()));
    assert!(!hand.drain_events().iter().any(|e| matches!(
        e,
        EngineEvent::HoleCardsDealt { .. } | EngineEvent::StreetRevealed { .. }
    )));
    hand.inject_board_for_street(&[
        card(Rank::Two, Suit::Clubs),
        card(Rank::Three, Suit::Clubs),
        card(Rank::Four, Suit::Clubs),
    ])
    .unwrap();
    assert_eq!(hand.pending_board_street(), Some(Street::Turn));
    hand.inject_board_for_street(&[card(Rank::Five, Suit::Clubs)])
        .unwrap();
    assert_eq!(hand.pending_board_street(), Some(Street::River));
    hand.inject_board_for_street(&[card(Rank::Six, Suit::Clubs)])
        .unwrap();
    assert!(hand.is_done());
    assert!(matches!(
        hand.finish_blind(),
        Err(BlindFinishError::MissingReveal { .. })
    ));
}

#[test]
fn bomb_pot_zero_keeps_standard_preflop_blinds() {
    let mut hand = hand(&[1000, 1000, 1000]).with_bomb_pot(c(0));
    let snap = hand.start().unwrap();
    assert_eq!(hand.bomb_pot_amount(), None);
    assert_eq!(snap.street, Street::Preflop);
    assert_eq!(snap.board.count(), 0);
    assert_eq!(snap.pot, c(30));
    assert_eq!(snap.current_bet, c(20));
    assert_eq!(hand.actions.len(), 2);
}
