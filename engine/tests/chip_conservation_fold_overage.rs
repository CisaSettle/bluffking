//! Regression: a lone live player who already fully covers a short all-in blind
//! must NOT be prompted to fold/call a phantom bet, and any uncalled overage must
//! be refunded, never destroyed (chip-conservation break).
//!
//! History:
//! * 2026-06-04 (finding #1): when a player folded after contributing strictly
//!   MORE chips than the largest contribution among the still-live players, the
//!   slice above the top eligible contribution was allocated to NO pot and
//!   silently vanished (chip destruction). Fixed — overage is now refunded.
//! * 2026-07-10 (dual-AI audit, finding C11): the engine was ALSO *offering that
//!   fold in the first place*. `BettingRound::new_preflop` floors `current_bet`
//!   to a full big blind even when the BB is all-in for LESS than a full blind,
//!   so `check_done` believed the lone SB "owed a call" against a bet nobody had
//!   actually made. The SB (or a disconnect/timeout auto-fold on the server) was
//!   forced to fold-or-call and could forfeit chips already covering the all-in.
//!   Correct NL behaviour: when the lone live player has met or exceeded every
//!   live opponent's ACTUAL contribution, the round closes, the uncalled overage
//!   is refunded, and the board runs out to showdown — no fold decision exists.
//!
//! Reachable in prod because the next-hand deal seats any player with a positive
//! stack (no minimum-playable-stack guard): a short stack left with fewer chips
//! than a full blind sits in the BB all-in for less than the blind. These tests
//! now assert the lone covered player is never prompted, and chips conserve
//! whatever the (seed-determined) showdown outcome.

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
/// (bb=20, sb=10). SB posts 10, BB posts 9 (all-in).
///
/// The SB has already put in 10 ≥ the BB's all-in of 9, so there is no live
/// bettor and NO fold decision: the round must close automatically and the hand
/// run out to showdown. The SB's uncalled 1-chip overage (10 - 9) is refunded;
/// total chips conserve (500 + 9 = 509) regardless of who wins at showdown.
#[test]
fn hu_short_blind_allin_lone_sb_not_prompted() {
    let players = vec![(pid(1), c(500), 0u8), (pid(2), c(9), 1u8)];
    let start_total: u32 = 500 + 9;

    let mut hand = GameHand::new_with_rng(players, 0, c(20), c(10), PokerRng::from_seed(2));
    hand.start().expect("start ok");

    // C11: the lone covered SB must NOT be handed a fold-or-call decision.
    assert!(
        hand.snapshot().current_actor.is_none(),
        "lone SB already covers the BB all-in — engine must not prompt it to act"
    );
    assert!(
        hand.is_done(),
        "no live betting decision remains — hand runs out to showdown automatically"
    );

    let result = hand.finish();

    let final_total: u32 = result.final_stacks.values().sum();
    assert_eq!(
        final_total, start_total,
        "chip conservation: SB's uncalled 1-chip overage must be refunded, not \
         destroyed (got {final_total}, expected {start_total})"
    );
    assert!(
        result.pots.iter().any(|pot| pot.is_refund),
        "SB's uncalled overage must be marked as a refund"
    );

    let p1 = *result.final_stacks.get(&pid(1).inner()).unwrap();
    let p2 = *result.final_stacks.get(&pid(2).inner()).unwrap();
    assert_eq!(p1 + p2, start_total, "two-seat conservation");
    // The SB always keeps its refunded overage: it is never forced below 491
    // (=500-9 matched) by a phantom fold, and reaches a real showdown for the
    // 18-chip contested pot, so it is either 491 (lost) or 509 (won).
    assert!(
        p1 == 491 || p1 == 509,
        "SB reaches showdown for the covered pot (491 lost / 509 won), never a \
         phantom-fold forfeit (got {p1})"
    );
    assert!(
        p2 == 0 || p2 == 18,
        "BB all-in for 9 either busts (0) or scoops 18 (got {p2})"
    );
}

/// Larger overage magnitude scales exactly: bb=100/sb=50, BB all-in for 10.
/// SB posts 50, BB all-in 10; SB's uncalled 40 (50 - 10) is refunded and no fold
/// is offered.
#[test]
fn hu_short_blind_allin_large_overage() {
    let players = vec![(pid(1), c(500), 0u8), (pid(2), c(10), 1u8)];
    let start_total: u32 = 500 + 10;

    let mut hand = GameHand::new_with_rng(players, 0, c(100), c(50), PokerRng::from_seed(7));
    hand.start().expect("start ok");

    assert!(
        hand.snapshot().current_actor.is_none(),
        "lone SB already covers the BB all-in — no prompt"
    );
    assert!(hand.is_done());

    let result = hand.finish();

    let final_total: u32 = result.final_stacks.values().sum();
    assert_eq!(
        final_total, start_total,
        "chip conservation: 40-chip uncalled SB overage must be refunded"
    );
    assert!(result.pots.iter().any(|pot| pot.is_refund));

    let p1 = *result.final_stacks.get(&pid(1).inner()).unwrap();
    let p2 = *result.final_stacks.get(&pid(2).inner()).unwrap();
    assert_eq!(p1 + p2, start_total);
    // SB matched only 10; overage 40 refunded → floor 490; showdown for 20 pot.
    assert!(
        p1 == 490 || p1 == 510,
        "SB keeps 490 (500-10 matched) then wins/loses the 20 pot (got {p1})"
    );
    assert!(
        p2 == 0 || p2 == 20,
        "BB all-in for 10 busts (0) or scoops 20 (got {p2})"
    );
}

/// 3-handed: P1=button/dealer(500), P2=SB(500), P3=BB all-in from a 9-chip stack
/// (bb=20, sb=10). P1 (UTG, acts first 3-handed) CAN act and folds. That leaves
/// the SB (P2) as the lone live player already covering the 9-chip all-in BB —
/// so the SB must NOT be prompted; the round closes and runs out to showdown.
#[test]
fn three_handed_short_blind_allin_lone_sb_not_prompted() {
    let players = vec![
        (pid(1), c(500), 0u8),
        (pid(2), c(500), 1u8),
        (pid(3), c(9), 2u8),
    ];
    let start_total: u32 = 500 + 500 + 9;

    let mut hand = GameHand::new_with_rng(players, 0, c(20), c(10), PokerRng::from_seed(11));
    hand.start().expect("start ok");

    // 3-handed preflop: UTG = the button (P1) acts first and legitimately folds.
    let a1 = hand
        .snapshot()
        .current_actor
        .expect("first preflop actor (UTG can act)");
    hand.apply_action(a1, PlayerAction::Fold).expect("fold 1");

    // C11: with only the covered SB (P2) and the all-in BB (P3) left, the SB must
    // NOT be prompted — the earlier code folded it into a phantom call.
    assert!(
        hand.snapshot().current_actor.is_none(),
        "lone SB already covers the all-in BB — no second fold decision exists"
    );
    assert!(
        hand.is_done(),
        "two live seats, one all-in and one covering → showdown"
    );

    let result = hand.finish();

    let final_total: u32 = result.final_stacks.values().sum();
    assert_eq!(
        final_total, start_total,
        "chip conservation: 3-handed short-blind overage must be refunded \
         (got {final_total}, expected {start_total})"
    );
    assert!(result.pots.iter().any(|pot| pot.is_refund));

    let p1 = *result.final_stacks.get(&pid(1).inner()).unwrap();
    let p2 = *result.final_stacks.get(&pid(2).inner()).unwrap();
    let p3 = *result.final_stacks.get(&pid(3).inner()).unwrap();
    assert_eq!(p1, 500, "button folded pre, contributed nothing");
    assert_eq!(
        p2 + p3,
        start_total - 500,
        "SB+BB conserve the contested 18 + refunded 1"
    );
    assert!(
        p2 == 491 || p2 == 509,
        "SB matched 9 (overage 1 refunded), then wins/loses showdown (got {p2})"
    );
    assert!(
        p3 == 0 || p3 == 18,
        "BB all-in for 9 busts (0) or scoops 18 (got {p3})"
    );
}

/// Exact reproduction of the dual-AI audit report scenario (C11): HU, blinds
/// 10/20, SB stack 1000, BB whittled to 8 chips. Before the fix, `start()` left
/// the SB as current_actor with `to_call = 10` and rejected `Check`, so a
/// disconnect/timeout auto-fold (or a confused click) forfeited the 8 chips the
/// SB had already committed to cover the all-in. After the fix the SB is never
/// prompted; the hand auto-resolves.
#[test]
fn c11_report_repro_sb_covers_bb_short_allin() {
    let players = vec![(pid(1), c(1000), 0u8), (pid(2), c(8), 1u8)];
    let start_total: u32 = 1000 + 8;

    let mut hand = GameHand::new_with_rng(players, 0, c(20), c(10), PokerRng::from_seed(3));
    hand.start().expect("start ok");

    assert!(
        hand.snapshot().current_actor.is_none(),
        "C11: SB (posted 10) already covers the BB all-in for 8 — must not be \
         forced to call-or-fold a phantom 20-chip bet"
    );
    assert!(hand.is_done());

    let result = hand.finish();

    let final_total: u32 = result.final_stacks.values().sum();
    assert_eq!(
        final_total, start_total,
        "chip conservation (got {final_total})"
    );
    assert!(
        result.pots.iter().any(|pot| pot.is_refund),
        "SB's uncalled 2-chip overage (10-8) must be refunded"
    );

    let p1 = *result.final_stacks.get(&pid(1).inner()).unwrap();
    let p2 = *result.final_stacks.get(&pid(2).inner()).unwrap();
    assert_eq!(p1 + p2, start_total);
    // The SB must never end below 992 (=1000-8 matched); the old bug let a fold
    // forfeit the covered 8 with the SB otherwise entitled to a showdown.
    assert!(
        p1 == 992 || p1 == 1008,
        "SB keeps ≥992 and reaches showdown for the 16 pot (992 lost / 1008 won), \
         never a phantom-fold forfeit (got {p1})"
    );
    assert!(
        p2 == 0 || p2 == 16,
        "BB all-in for 8 busts (0) or scoops 16 (got {p2})"
    );
}
