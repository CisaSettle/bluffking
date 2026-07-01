//! Regression: a folded player's UNCALLED blind/raise overage must be refunded,
//! never destroyed (chip-conservation break).
//!
//! Prod-found defect (adversarial audit 2026-06-04, finding #1): when a player
//! folds after contributing strictly MORE chips than the largest contribution
//! among the still-live (non-folded) players, the slice between the top eligible
//! contribution and the folder's contribution used to be allocated to NO pot, so
//! those chips silently vanished and the table's books no longer balanced.
//!
//! Reachable in prod because the next-hand deal seats any player with a positive
//! stack (no minimum-playable-stack guard): a short stack left with fewer chips
//! than a full blind sits in the BB all-in for less than the blind, and when the
//! SB folds preflop the SB's uncalled overage above the BB's all-in amount was
//! destroyed.

use engine::{
    action::PlayerAction,
    game::GameHand,
    player::{Chips, PlayerId},
    rng::PokerRng,
};

fn pid(n: u64) -> PlayerId {
    PlayerId::new(n)
}
fn c(n: u32) -> Chips {
    Chips(n)
}

/// Heads-up: P1 = dealer/SB with 500, P2 = BB all-in from a 9-chip blind
/// (bb=20, sb=10). SB posts 10, BB posts 9 (all-in). SB folds preflop.
///
/// SB's uncalled overage (10 - 9 = 1 chip) must be returned to the SB. Total
/// chips must conserve: 500 + 9 = 509 before and after.
#[test]
fn hu_folded_sb_uncalled_overage_refunded() {
    let players = vec![(pid(1), c(500), 0u8), (pid(2), c(9), 1u8)];
    let start_total: u32 = 500 + 9;

    let mut hand = GameHand::new_with_rng(players, 0, c(20), c(10), PokerRng::from_seed(2));
    hand.start().expect("start ok");

    // SB acts first preflop in HU; SB folds → fold-around.
    let actor = hand.snapshot().current_actor.expect("preflop actor");
    assert_eq!(actor, pid(1), "SB (dealer) acts first preflop in HU");
    hand.apply_action(actor, PlayerAction::Fold).expect("fold");

    assert!(hand.is_done(), "fold-around finishes immediately");
    let result = hand.finish();

    let final_total: u32 = result.final_stacks.values().sum();
    assert_eq!(
        final_total, start_total,
        "chip conservation: folded SB's uncalled 1-chip overage must be refunded, \
         not destroyed (got {final_total}, expected {start_total})"
    );

    // Concretely: SB gets its overage back (500 - 9 called = 491), BB scoops the
    // 18-chip main pot (own 9 + SB's called 9).
    assert_eq!(
        *result.final_stacks.get(&pid(1).inner()).unwrap(),
        491,
        "SB keeps 491: 500 - 9 called (its 10th SB chip was uncalled and refunded)"
    );
    assert_eq!(
        *result.final_stacks.get(&pid(2).inner()).unwrap(),
        18,
        "BB all-in for 9 scoops the 18-chip contested main pot"
    );
    assert!(
        result.pots.iter().any(|pot| pot.is_refund),
        "SB's returned uncalled chip must be marked as a refund, not a pot win"
    );
}

/// Larger overage magnitude scales exactly: bb=100/sb=50, BB all-in for 10.
/// SB folds preflop → SB's uncalled 40 (50 - 10) must be refunded.
#[test]
fn hu_folded_sb_large_overage_refunded() {
    let players = vec![(pid(1), c(500), 0u8), (pid(2), c(10), 1u8)];
    let start_total: u32 = 500 + 10;

    let mut hand = GameHand::new_with_rng(players, 0, c(100), c(50), PokerRng::from_seed(7));
    hand.start().expect("start ok");

    let actor = hand.snapshot().current_actor.expect("preflop actor");
    hand.apply_action(actor, PlayerAction::Fold).expect("fold");

    assert!(hand.is_done());
    let result = hand.finish();

    let final_total: u32 = result.final_stacks.values().sum();
    assert_eq!(
        final_total, start_total,
        "chip conservation: 40-chip uncalled SB overage must be refunded"
    );
    assert_eq!(
        *result.final_stacks.get(&pid(1).inner()).unwrap(),
        490,
        "SB keeps 490: 500 - 10 called (40 of its 50 SB was uncalled and refunded)"
    );
    assert_eq!(
        *result.final_stacks.get(&pid(2).inner()).unwrap(),
        20,
        "BB all-in for 10 scoops the 20-chip contested pot"
    );
}

/// 3-handed variant: dealer=0 (P1), P2=SB, P3=BB all-in from a 9-chip stack
/// (bb=20, sb=10). P1 (UTG/button, acts first in 3-handed preflop) folds, then
/// P2 (SB) folds preflop, leaving the 9-chip all-in BB P3 uncontested. P2's
/// uncalled overage above P3's 9 must be refunded.
#[test]
fn three_handed_folded_sb_uncalled_overage_refunded() {
    // Seats: P1=button/dealer(500), P2=SB(500), P3=BB(9, all-in from blind).
    let players = vec![
        (pid(1), c(500), 0u8),
        (pid(2), c(500), 1u8),
        (pid(3), c(9), 2u8),
    ];
    let start_total: u32 = 500 + 500 + 9;

    let mut hand = GameHand::new_with_rng(players, 0, c(20), c(10), PokerRng::from_seed(11));
    hand.start().expect("start ok");

    // 3-handed preflop: action starts UTG = seat after BB = the button (P1).
    let a1 = hand.snapshot().current_actor.expect("first preflop actor");
    hand.apply_action(a1, PlayerAction::Fold).expect("fold 1");

    // Next live actor is the SB (P2); fold it too, leaving only the all-in BB.
    let a2 = hand.snapshot().current_actor.expect("second preflop actor");
    hand.apply_action(a2, PlayerAction::Fold).expect("fold 2");

    assert!(
        hand.is_done(),
        "two folds leave a single live (all-in) player"
    );
    let result = hand.finish();

    let final_total: u32 = result.final_stacks.values().sum();
    assert_eq!(
        final_total, start_total,
        "chip conservation: 3-handed folded-SB overage must be refunded \
         (got {final_total}, expected {start_total})"
    );

    // The folded button never put chips in (it folded before posting anything
    // beyond nothing): keeps 500. The folded SB gets its uncalled chip back.
    // The BB scoops the contested main pot. Whatever the exact split, no chip
    // may be destroyed — the sum invariant above is the load-bearing assertion;
    // these pin the expected per-seat outcome.
    let p1 = *result.final_stacks.get(&pid(1).inner()).unwrap();
    let p2 = *result.final_stacks.get(&pid(2).inner()).unwrap();
    let p3 = *result.final_stacks.get(&pid(3).inner()).unwrap();
    assert_eq!(p1, 500, "button folded pre, contributed nothing");
    assert_eq!(
        p2, 491,
        "SB folded: 500 - 9 called, uncalled 1-chip overage refunded"
    );
    assert_eq!(p3, 18, "BB all-in for 9 scoops the 18-chip contested pot");
}
