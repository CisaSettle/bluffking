//! Phase 6 H5 rebuild — variable-seat regression for HU through 9-max.
//!
//! The Phase 3 client felt is drawn for 2-9 seats and the SetupView wizard
//! exposes a 2-9 picker. The engine already supports the full range via
//! `Position::for_seat` + `blind_positions`, but this test pins the contract:
//!
//!   * `start()` does not panic for any 2..=9 player count.
//!   * Blind posting uses HU rules at n=2 (dealer = SB) and standard rotation
//!     for n>=3.
//!   * `Position::for_seat` returns a unique label for every dealt seat at all
//!     supported counts.
//!   * A full hand can be played to `is_done()` by check/calling through.

use std::collections::HashSet;

use engine::{Chips, GameHand, PlayerAction, PlayerId, PokerRng, Position};

fn pid(n: u64) -> PlayerId {
    PlayerId::new(n)
}

fn run_hand_with_seats(n: u8, dealer_idx: usize) {
    assert!((2..=9).contains(&n), "variable_seats covers 2..=9, got {n}");
    let players: Vec<(PlayerId, Chips, u8)> = (0..n)
        .map(|i| (pid(i as u64 + 1), Chips(1000), i))
        .collect();

    let mut hand = GameHand::new_with_rng(
        players.clone(),
        dealer_idx,
        Chips(20),
        Chips(10),
        PokerRng::from_seed((n as u64) << 16 | dealer_idx as u64),
    );

    hand.start().expect("start should not panic");

    // Drive the hand to completion by always check/calling.
    let mut safety = 0;
    while !hand.is_done() {
        safety += 1;
        if safety > 200 {
            panic!("hand did not terminate within 200 actions (n={n})");
        }
        let snap = hand.snapshot();
        let Some(actor) = snap.current_actor else {
            break;
        };
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
        hand.apply_action(actor, action).expect("apply ok");
    }

    assert!(hand.is_done(), "hand must be done (n={n})");
    let result = hand.finish();
    let awarded: u32 = result.chips_awarded.values().sum();
    // Everyone matched the BB (20) since we check/called; total pot is bb * n
    // less the SB shortfall if any. Just assert > 0.
    assert!(
        awarded > 0,
        "winner must take chips (n={n}, awarded={awarded})"
    );
}

#[test]
fn variable_seats_heads_up_2_seats() {
    run_hand_with_seats(2, 0);
}

#[test]
fn variable_seats_4_seats() {
    run_hand_with_seats(4, 0);
    run_hand_with_seats(4, 2);
}

#[test]
fn variable_seats_6_max() {
    run_hand_with_seats(6, 0);
    run_hand_with_seats(6, 3);
}

#[test]
fn variable_seats_9_max() {
    run_hand_with_seats(9, 0);
    run_hand_with_seats(9, 4);
}

/// HU dealer is the SB; that is enforced by `blind_positions`.
#[test]
fn variable_seats_hu_dealer_is_sb_and_acts_first_preflop() {
    let players = vec![(pid(1), Chips(1000), 0), (pid(2), Chips(1000), 1)];
    let mut hand =
        GameHand::new_with_rng(players, 0, Chips(20), Chips(10), PokerRng::from_seed(99));
    hand.start().expect("start ok");
    let snap = hand.snapshot();
    // In heads-up the dealer/SB acts first preflop.
    let actor = snap.current_actor.expect("HU preflop has an actor");
    assert_eq!(actor, pid(1), "HU: dealer (SB) acts first preflop");
    assert_eq!(snap.dealer_seat, 0);
}

/// Every supported seat count produces unique position labels for every dealt
/// seat — guards against silent collisions in `Position::for_seat`.
#[test]
fn variable_seats_unique_position_per_seat() {
    for n in 2..=9u8 {
        for dealer in 0..n {
            let active: Vec<u8> = (0..n).collect();
            let labels: Vec<&'static str> = active
                .iter()
                .map(|&s| Position::for_seat(s, dealer, &active).short_label())
                .collect();
            let unique: HashSet<&'static str> = labels.iter().copied().collect();
            assert_eq!(
                labels.len(),
                unique.len(),
                "duplicate position label in n={n} dealer={dealer}: {labels:?}"
            );
        }
    }
}

/// Snapshots include `dealer_seat`, `sb_seat`, `bb_seat` derived from the
/// rotation. They must be distinct for n>=3 and dealer==SB for n=2.
#[test]
fn variable_seats_blind_position_invariants() {
    use engine::blind_positions;

    for n in 2..=9usize {
        for dealer in 0..n {
            let (sb, bb) = blind_positions(dealer, n);
            if n == 2 {
                assert_eq!(sb, dealer, "HU: SB == dealer");
                assert_ne!(bb, sb, "HU: BB != SB");
            } else {
                assert_ne!(sb, dealer, "n>=3: SB != dealer");
                assert_ne!(bb, dealer, "n>=3: BB != dealer");
                assert_ne!(sb, bb, "SB != BB");
            }
        }
    }
}
