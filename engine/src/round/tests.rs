//! Betting-round unit tests (moved out of `round.rs`: the inline test module
//! was more than half the file). Same module path, same `use super::*`.

use super::*;
use crate::action::PlayerAction;

fn pid(n: u64) -> PlayerId {
    PlayerId::new(n)
}

fn c(n: u32) -> Chips {
    Chips(n)
}

fn two_players(s1: u32, s2: u32) -> Vec<(PlayerId, Chips)> {
    vec![(pid(1), c(s1)), (pid(2), c(s2))]
}

fn three_players(s1: u32, s2: u32, s3: u32) -> Vec<(PlayerId, Chips)> {
    vec![(pid(1), c(s1)), (pid(2), c(s2)), (pid(3), c(s3))]
}

fn four_players(s1: u32, s2: u32, s3: u32, s4: u32) -> Vec<(PlayerId, Chips)> {
    vec![
        (pid(1), c(s1)),
        (pid(2), c(s2)),
        (pid(3), c(s3)),
        (pid(4), c(s4)),
    ]
}

// U05 (dual-AI OSS review): a `Blind` is never a voluntary in-turn action —
// blinds post internally during setup. Accepting it here would bypass every
// betting rule, and the variant is wire-deserializable.
#[test]
fn blind_action_is_rejected_as_voluntary_action() {
    use crate::action::BlindKind;
    let mut round = BettingRound::new(two_players(1000, 1000), 0, c(100), c(20));
    // Under-min "blind" post that previously slipped through unvalidated.
    let err = round
        .apply_action(
            pid(1),
            &PlayerAction::Blind {
                kind: BlindKind::Small,
                amount: c(1),
            },
        )
        .unwrap_err();
    assert_eq!(err, ActionError::InvalidAction);
    // State untouched: it is still P1's turn, nothing contributed.
    assert_eq!(round.current_player(), Some(pid(1)));
    assert_eq!(round.pot_total(), c(0));
}

// U09 (dual-AI OSS review) / TDA Rule 43: two short all-ins that SUM to at
// least a full raise reopen the betting for a player who has already acted.
#[test]
fn cumulative_short_all_ins_reopen_action() {
    // P1 opens to 100 (full raise, last_raise_amount = 100).
    let mut round = BettingRound::new(four_players(5000, 150, 220, 5000), 0, c(0), c(20));
    round
        .apply_action(pid(1), &PlayerAction::Raise { amount: c(100) })
        .unwrap();
    // P2 all-in 150 (+50, short) and P3 all-in 220 (+70, short): neither alone
    // reopens, but together they raise the bet 120 over the 100 full raise.
    round.apply_action(pid(2), &PlayerAction::AllIn).unwrap();
    round.apply_action(pid(3), &PlayerAction::AllIn).unwrap();
    // P4 is still a live stack and calls, so it can respond if P1 re-raises.
    // Without P4, P1 would be the sole chip-holder and could only call/fold.
    round.apply_action(pid(4), &PlayerAction::Call).unwrap();
    // Action returns to P1 facing a cumulative full raise → may re-raise.
    assert_eq!(round.current_player(), Some(pid(1)));
    assert!(
        round.current_player_can_raise(),
        "cumulative short all-ins totalling a full raise must reopen P1's action"
    );
    round
        .apply_action(pid(1), &PlayerAction::Raise { amount: c(400) })
        .expect("re-raise must be legal when action is reopened cumulatively");
    assert_eq!(round.current_bet(), c(400));
}

/// A sole live stack facing an all-in opponent has a fold/call decision,
/// never a right to manufacture an unmatched overage. This is the exact HU
/// 10/20 failure that previously accepted RaiseTo(200) / AllIn(1000), then
/// created a meaningless side-pot layer that was immediately refunded.
#[test]
fn sole_live_stack_facing_all_in_cannot_raise_but_can_call() {
    let mut round = BettingRound::new_preflop(
        two_players(100, 1000),
        0,
        &[(pid(1), c(10)), (pid(2), c(20))],
        c(20),
    );
    round.apply_action(pid(1), &PlayerAction::AllIn).unwrap();
    assert_eq!(round.current_player(), Some(pid(2)));
    assert_eq!(round.current_bet(), c(100));
    assert!(!round.current_player_can_raise());

    assert_eq!(
        round
            .apply_action(pid(2), &PlayerAction::Raise { amount: c(200) })
            .unwrap_err(),
        ActionError::InvalidAction
    );
    assert_eq!(
        round
            .apply_action(pid(2), &PlayerAction::AllIn)
            .unwrap_err(),
        ActionError::InvalidAction,
        "an all-in above the call is still an illegal dry-side-pot raise"
    );

    round.apply_action(pid(2), &PlayerAction::Call).unwrap();
    assert!(round.is_done());
    assert_eq!(round.player_contributed(pid(1)), c(100));
    assert_eq!(round.player_contributed(pid(2)), c(100));
    assert_eq!(round.pot_total(), c(200));
}

#[test]
fn all_in_call_shapes_remain_legal_when_no_raise_is_possible() {
    // Exact all-in call: P2 has exactly the 80 chips needed after its blind.
    let mut exact = BettingRound::new_preflop(
        two_players(100, 100),
        0,
        &[(pid(1), c(10)), (pid(2), c(20))],
        c(20),
    );
    exact.apply_action(pid(1), &PlayerAction::AllIn).unwrap();
    exact
        .apply_action(pid(2), &PlayerAction::AllIn)
        .expect("exact all-in call remains legal");
    assert!(exact.is_done());

    // Partial all-in call: P2 cannot cover the 100 bet but may commit its
    // remaining 50; total_committed stays below current_bet.
    let mut partial = BettingRound::new(two_players(100, 50), 0, c(0), c(20));
    partial.apply_action(pid(1), &PlayerAction::AllIn).unwrap();
    partial
        .apply_action(pid(2), &PlayerAction::AllIn)
        .expect("partial all-in call remains legal");
    assert!(partial.is_done());
    assert_eq!(partial.player_contributed(pid(2)), c(50));
}

#[test]
fn all_in_raises_remain_legal_with_a_live_responder() {
    let mut short = BettingRound::new(three_players(150, 1000, 1000), 0, c(100), c(100));
    assert!(short.current_player_can_raise());
    short
        .apply_action(pid(1), &PlayerAction::AllIn)
        .expect("short all-in raise is legal while two live stacks can respond");
    assert_eq!(short.current_bet(), c(150));

    let mut full = BettingRound::new(three_players(300, 1000, 1000), 0, c(100), c(100));
    assert!(full.current_player_can_raise());
    full.apply_action(pid(1), &PlayerAction::AllIn)
        .expect("full all-in raise is legal while two live stacks can respond");
    assert_eq!(full.current_bet(), c(300));
}

// Negative control: a SINGLE sub-minimum all-in must NOT reopen action.
#[test]
fn single_short_all_in_does_not_reopen_action() {
    let mut round = BettingRound::new(three_players(5000, 130, 5000), 0, c(0), c(20));
    round
        .apply_action(pid(1), &PlayerAction::Raise { amount: c(100) })
        .unwrap();
    round.apply_action(pid(2), &PlayerAction::AllIn).unwrap(); // +30, short
                                                               // Action to P3 (fresh) then back to P1: P1 already acted, faces only a 30
                                                               // increase over the 100 full raise → cannot re-raise.
    round.apply_action(pid(3), &PlayerAction::Call).unwrap();
    assert_eq!(round.current_player(), Some(pid(1)));
    assert!(
        !round.current_player_can_raise(),
        "a single 30-chip short all-in must not reopen P1's action"
    );
    assert_eq!(
        round
            .apply_action(pid(1), &PlayerAction::Raise { amount: c(300) })
            .unwrap_err(),
        ActionError::InvalidAction
    );
}

#[test]
fn fold_check_standard() {
    let mut round = BettingRound::new(two_players(1000, 1000), 0, c(0), c(20));
    round.apply_action(pid(1), &PlayerAction::Fold).unwrap();
    assert!(
        round.is_done(),
        "round should be done after last player folds"
    );
    assert!(round.player_folded(pid(1)));
}

#[test]
fn check_check_closes_round() {
    let mut round = BettingRound::new(two_players(1000, 1000), 0, c(0), c(20));
    assert_eq!(round.current_player(), Some(pid(1)));
    round.apply_action(pid(1), &PlayerAction::Check).unwrap();
    assert_eq!(round.current_player(), Some(pid(2)));
    round.apply_action(pid(2), &PlayerAction::Check).unwrap();
    assert!(round.is_done());
}

#[test]
fn call_basic() {
    let mut round = BettingRound::new(two_players(1000, 1000), 0, c(20), c(20));
    let moved = round.apply_action(pid(1), &PlayerAction::Call).unwrap();
    assert_eq!(moved, c(20));
}

#[test]
fn raise_and_call() {
    let mut round = BettingRound::new(two_players(1000, 1000), 0, c(0), c(20));
    round
        .apply_action(pid(1), &PlayerAction::Raise { amount: c(60) })
        .unwrap();
    assert_eq!(round.current_bet(), c(60));
    let moved = round.apply_action(pid(2), &PlayerAction::Call).unwrap();
    assert_eq!(moved, c(60));
    assert!(round.is_done(), "round should be done after call of raise");
    assert_eq!(round.pot_total(), c(120));
}

#[test]
fn raise_below_minimum_rejected() {
    let mut round = BettingRound::new(two_players(1000, 1000), 0, c(20), c(20));
    // current_bet=20, min_raise = 20+20=40. Raise to 30 is invalid.
    let err = round
        .apply_action(pid(1), &PlayerAction::Raise { amount: c(30) })
        .unwrap_err();
    assert_eq!(err, ActionError::BelowMinRaise);
}

/// Regression test: postflop bet 40, opponent raise-to 55 must be rejected.
///
/// Scenario (Bug 1 repro):
///   - New postflop street: current_bet=0, big_blind=10 → last_raise_amount=10
///   - P1 bets 40 (Raise{amount:40}): 40 >= 10 → OK, last_raise_amount becomes 40
///   - P2 raises to 55: min_raise = 40+40=80, 55 < 80 → must return BelowMinRaise
#[test]
fn postflop_bet_40_reraise_to_55_rejected() {
    // New postflop street: current_bet=0, big_blind=10
    let mut round = BettingRound::new(two_players(1000, 1000), 0, c(0), c(10));
    assert_eq!(round.current_player(), Some(pid(1)));

    // P1 bets 40 (valid: 40 >= big_blind=10, last_raise_amount becomes 40)
    round
        .apply_action(pid(1), &PlayerAction::Raise { amount: c(40) })
        .unwrap();
    assert_eq!(round.current_bet(), c(40));
    assert_eq!(round.min_raise_to(), c(80)); // 40+40=80

    // P2 attempts raise to 55 — must be rejected (55 < 80)
    let err = round
        .apply_action(pid(2), &PlayerAction::Raise { amount: c(55) })
        .unwrap_err();
    assert_eq!(
        err,
        ActionError::BelowMinRaise,
        "raise to 55 must be rejected when min raise to is 80"
    );
}

/// Regression (audit 2026-06-03): a Raise whose total exceeds the player's
/// stack is an affordability failure, not a min-raise violation. A player
/// with a 200 stack facing current_bet 50 who raises to 1000 (well ABOVE the
/// minimum) must be rejected with `InvalidAction`, not the misleading
/// `BelowMinRaise`.
#[test]
fn over_the_top_unaffordable_raise_is_invalid_not_below_min() {
    // p1 stack 200, p2 stack 1000. current_bet 50, bb 20 → min_raise_to 70.
    let mut round = BettingRound::new(two_players(200, 1000), 0, c(50), c(20));
    let err = round
        .apply_action(pid(1), &PlayerAction::Raise { amount: c(1000) })
        .unwrap_err();
    assert_eq!(
        err,
        ActionError::InvalidAction,
        "unaffordable over-the-top raise must report InvalidAction, not BelowMinRaise"
    );
    // State untouched after the rejection.
    assert_eq!(round.current_bet(), c(50));
}

#[test]
fn not_your_turn_rejected() {
    let mut round = BettingRound::new(two_players(1000, 1000), 0, c(0), c(20));
    let err = round
        .apply_action(pid(2), &PlayerAction::Check)
        .unwrap_err();
    assert_eq!(err, ActionError::NotYourTurn);
}

#[test]
fn action_after_round_done_rejected() {
    let mut round = BettingRound::new(two_players(1000, 1000), 0, c(0), c(20));
    round.apply_action(pid(1), &PlayerAction::Check).unwrap();
    round.apply_action(pid(2), &PlayerAction::Check).unwrap();
    assert!(round.is_done());
    let err = round
        .apply_action(pid(1), &PlayerAction::Check)
        .unwrap_err();
    assert_eq!(err, ActionError::HandFinished);
}

#[test]
fn all_in_heads_up_equal_stacks() {
    let mut round = BettingRound::new(two_players(1000, 1000), 0, c(0), c(20));
    round.apply_action(pid(1), &PlayerAction::AllIn).unwrap();
    round.apply_action(pid(2), &PlayerAction::AllIn).unwrap();
    assert!(round.is_done());
    assert!(round.player_all_in(pid(1)));
    assert!(round.player_all_in(pid(2)));
    assert_eq!(round.pot_total(), c(2000));
}

#[test]
fn all_in_heads_up_unequal_stacks() {
    let mut round = BettingRound::new(two_players(500, 1000), 0, c(0), c(20));
    round.apply_action(pid(1), &PlayerAction::AllIn).unwrap();
    round.apply_action(pid(2), &PlayerAction::Call).unwrap();
    assert!(round.is_done());

    let pots = round.side_pots();
    let total: u32 = pots.iter().map(|p| p.amount.0).sum();
    assert_eq!(total, 1000); // p1 puts in 500, p2 calls 500
}

#[test]
fn three_player_all_in_different_stacks() {
    let mut round = BettingRound::new(three_players(300, 600, 1000), 0, c(0), c(20));
    round.apply_action(pid(1), &PlayerAction::AllIn).unwrap();
    round.apply_action(pid(2), &PlayerAction::AllIn).unwrap();
    // P3 is now the sole live stack. It may call 600 but cannot shove an
    // unmatched extra 400 into a dry side pot.
    round.apply_action(pid(3), &PlayerAction::Call).unwrap();
    assert!(round.is_done());

    let pots = round.side_pots();
    let total: u32 = pots.iter().map(|p| p.amount.0).sum();
    assert_eq!(total, 1500);
    let p3 = round
        .player_states()
        .iter()
        .find(|player| player.player_id == pid(3))
        .unwrap();
    assert_eq!(p3.stack, c(400));
}

/// REGRESSION (2026-05-29): a freshly-opened street with exactly ONE player
/// who can act (everyone else all-in) and nothing owed must close
/// immediately — there is no betting decision, so the board runs out. A
/// stack-0 player is marked all-in by `BettingRound::new`.
#[test]
fn all_but_one_all_in_closes_round_when_nothing_owed() {
    // pid(1) deep (492), pid(2) all-in (stack 0). Post-flop: current_bet 0.
    let round = BettingRound::new(vec![(pid(1), c(492)), (pid(2), c(0))], 0, c(0), c(20));
    assert!(
        round.player_all_in(pid(2)),
        "a stack-0 player must be flagged all-in at construction"
    );
    assert!(
        round.is_done(),
        "round with one live stack owing nothing must close (auto-runout)"
    );
    assert!(
        round.current_player().is_none(),
        "no actor when the round is already done"
    );
}

/// NEGATIVE companion to the above: when the lone live player still OWES a
/// call to a larger all-in, fold-vs-call is a real decision, so the round
/// must stay open and expose them as the actor.
#[test]
fn lone_live_player_owing_a_call_stays_open() {
    // pid(1) short (50) shoves; pid(2) deep (1000) now owes a 50 call.
    let mut round = BettingRound::new(two_players(50, 1000), 0, c(0), c(20));
    round.apply_action(pid(1), &PlayerAction::AllIn).unwrap();
    assert!(
        !round.is_done(),
        "round must stay open while the lone live stack still owes a call"
    );
    assert_eq!(
        round.current_player(),
        Some(pid(2)),
        "the owing live player is still the actor"
    );
    // Once they call, nothing is owed and the round closes.
    round.apply_action(pid(2), &PlayerAction::Call).unwrap();
    assert!(
        round.is_done(),
        "round closes once the lone live stack matches the all-in"
    );
}

#[test]
fn lone_live_player_calls_only_actual_short_blind_contribution() {
    // Nominal blinds are 10/20, but the BB can post only 8. UTG folds,
    // leaving the SB (10 already posted) fully covering the all-in: no
    // phantom action against the nominal 20.
    let mut covered = BettingRound::new_preflop(
        three_players(100, 100, 8),
        0,
        &[(pid(2), c(10)), (pid(3), c(20))],
        c(20),
    );
    covered.apply_action(pid(1), &PlayerAction::Fold).unwrap();
    assert!(covered.is_done());
    assert_eq!(covered.current_player(), None);

    // With an SB contribution of only 5, the same short BB leaves a real
    // three-chip call. The effective bet must become 8, not stay at the
    // nominal 20 floor.
    let mut owing = BettingRound::new_preflop(
        three_players(100, 100, 8),
        0,
        &[(pid(2), c(5)), (pid(3), c(20))],
        c(20),
    );
    owing.apply_action(pid(1), &PlayerAction::Fold).unwrap();
    assert!(!owing.is_done());
    assert_eq!(owing.current_player(), Some(pid(2)));
    assert_eq!(owing.current_bet(), c(8));
    assert!(!owing.current_player_can_raise());
    owing.apply_action(pid(2), &PlayerAction::Call).unwrap();
    assert_eq!(owing.player_contributed(pid(2)), c(8));
    assert!(owing.is_done());
}

#[test]
fn check_when_bet_outstanding_rejected() {
    let mut round = BettingRound::new(two_players(1000, 1000), 0, c(20), c(20));
    let err = round
        .apply_action(pid(1), &PlayerAction::Check)
        .unwrap_err();
    assert_eq!(err, ActionError::InvalidAction);
}

#[test]
fn fold_after_fold_rejected() {
    let mut round = BettingRound::new(three_players(1000, 1000, 1000), 0, c(0), c(20));
    round.apply_action(pid(1), &PlayerAction::Fold).unwrap();
    round.apply_action(pid(2), &PlayerAction::Fold).unwrap();
    assert!(round.is_done());
    let err = round
        .apply_action(pid(3), &PlayerAction::Check)
        .unwrap_err();
    assert_eq!(err, ActionError::HandFinished);
}

/// U-01 regression: an under-min raise must NOT update `last_raise_amount`,
/// so the next legal min re-raise stays anchored to the prior full increment.
///
/// Scenario (engine-level, blinds-stripped to focus on the round):
///   - current_bet=$30 (someone opened to $30 above a $10 BB → last_raise_amount=$20)
///   - villain goes all-in for $40 (delta=$10 < $20 → under-min)
///   - Expected: current_bet=$40, last_raise_amount UNCHANGED at $20,
///     so `min_raise_to = $60`. The user-reported bug had this collapse to
///     $50 (= 40 + 10), which would mean `last_raise_amount` was wrongly
///     overwritten with the under-min delta.
#[test]
fn under_min_raise_does_not_update_min_reraise_tracker() {
    // Build a round that mirrors the live state after a $30 open above a $10 BB.
    // last_raise_amount must equal $20 (the prior raise increment); we set it
    // by constructing the round with current_bet=$10 (BB), big_blind=$10, then
    // having the first actor raise to $30 — same path the live engine takes.
    let mut round = BettingRound::new(vec![(pid(1), c(1000)), (pid(2), c(40))], 0, c(10), c(10));

    // Hero raises to $30 (delta=$20, full raise).
    round
        .apply_action(pid(1), &PlayerAction::Raise { amount: c(30) })
        .unwrap();
    assert_eq!(round.current_bet(), c(30));
    assert_eq!(
        round.min_raise_to(),
        c(50),
        "after $30 open the min re-raise is $30+$20=$50"
    );

    // Villain all-in for $40 total (their entire stack). Delta=$10 < $20 → under-min.
    round.apply_action(pid(2), &PlayerAction::AllIn).unwrap();

    assert_eq!(
        round.current_bet(),
        c(40),
        "under-min all-in still raises the current bet to $40"
    );
    assert_eq!(
        round.min_raise_to(),
        c(60),
        "U-01: under-min all-in must NOT update last_raise_amount; min_raise_to \
             must remain $40+$20=$60, NOT collapse to $40+$10=$50"
    );
}

/// U-01 regression: an under-min `Raise` (non-all-in) must be rejected,
/// and the engine's state must be untouched (current_bet and
/// last_raise_amount stay at their pre-action values).
#[test]
fn under_min_raise_rejected_leaves_state_intact() {
    let mut round = BettingRound::new(two_players(1000, 1000), 0, c(10), c(10));

    // Hero raises to $30 (delta=$20).
    round
        .apply_action(pid(1), &PlayerAction::Raise { amount: c(30) })
        .unwrap();
    assert_eq!(round.min_raise_to(), c(50));

    // Villain attempts Raise{$40}: 40 < min_raise=50, NOT an all-in
    // (villain has $1000 stack), must reject.
    let err = round
        .apply_action(pid(2), &PlayerAction::Raise { amount: c(40) })
        .unwrap_err();
    assert_eq!(err, ActionError::BelowMinRaise);

    // State must be unchanged after rejection.
    assert_eq!(round.current_bet(), c(30));
    assert_eq!(
        round.min_raise_to(),
        c(50),
        "rejected under-min raise must not mutate last_raise_amount"
    );
}

/// U-01: an under-min all-in submitted via `Raise{amount}` (e.g. short-stack
/// shoving via the raise button) must be treated the same as an `AllIn`
/// under-min: current_bet rises, but last_raise_amount stays.
#[test]
fn under_min_raise_as_all_in_does_not_update_tracker() {
    // Villain stack = $40 exactly.
    let mut round = BettingRound::new(vec![(pid(1), c(1000)), (pid(2), c(40))], 0, c(10), c(10));
    round
        .apply_action(pid(1), &PlayerAction::Raise { amount: c(30) })
        .unwrap();
    assert_eq!(round.min_raise_to(), c(50));

    // Villain "raises" to $40 — this is below min ($50) but it's their
    // entire remaining stack ($40 - $0 contributed = $40 = stack).
    // Engine must accept (legal under-min all-in) but NOT update tracker.
    round
        .apply_action(pid(2), &PlayerAction::Raise { amount: c(40) })
        .unwrap();

    assert!(round.player_all_in(pid(2)));
    assert_eq!(round.current_bet(), c(40));
    assert_eq!(
        round.min_raise_to(),
        c(60),
        "U-01: under-min Raise that is also an all-in must NOT update \
             last_raise_amount; min_raise_to must stay at $40+$20=$60"
    );
}

#[test]
fn side_pots_three_way_correct_eligibility() {
    let mut round = BettingRound::new(three_players(300, 600, 1000), 0, c(0), c(20));
    round.apply_action(pid(1), &PlayerAction::AllIn).unwrap(); // 300
    round.apply_action(pid(2), &PlayerAction::AllIn).unwrap(); // 600
    round.apply_action(pid(3), &PlayerAction::Call).unwrap(); // 600
    assert!(round.is_done());

    let pots = round.side_pots();
    let total: u32 = pots.iter().map(|p| p.amount.0).sum();
    assert_eq!(total, 1500); // 300*3 + 300*2

    let first = &pots[0];
    assert_eq!(first.eligible.len(), 3);

    let second = &pots[1];
    assert_eq!(second.eligible.len(), 2);
    assert!(!second.eligible.contains(&pid(1)));
}

// ----------------------------------------------------------------------
// Regression: short all-in via the Raise action must never LOWER the
// current bet (P0 — 2026-05-29 prod bug hunt). A player raising "to" an
// amount below the current bet while shoving their whole (short) stack is
// an all-in-for-less and must be treated like AllIn: commit the stack,
// mark all-in, but leave current_bet (and therefore everyone's to_call)
// untouched.
// ----------------------------------------------------------------------
#[test]
fn short_all_in_via_raise_does_not_lower_current_bet() {
    // p1 1000, p2 only 30 chips, p3 1000. BB = 20.
    let mut round = BettingRound::new(three_players(1000, 30, 1000), 0, c(0), c(20));
    // p1 raises to 100.
    round
        .apply_action(pid(1), &PlayerAction::Raise { amount: c(100) })
        .unwrap();
    assert_eq!(round.current_bet().0, 100);
    // p2 (stack 30) "raises to 30" — a short all-in for LESS than the bet.
    round
        .apply_action(pid(2), &PlayerAction::Raise { amount: c(30) })
        .unwrap();
    // current_bet MUST stay at 100, not drop to 30.
    assert_eq!(
        round.current_bet().0,
        100,
        "short all-in via raise must not lower current_bet"
    );
    assert!(round.player_all_in(pid(2)), "p2 should be all-in");
    // p3 still owes the full 100, not 30.
    assert_eq!(round.current_player(), Some(pid(3)));
    round.apply_action(pid(3), &PlayerAction::Call).unwrap();
    assert_eq!(round.player_contributed(pid(3)).0, 100);
    // Round closes: p1 already matched 100 and the short all-in did not
    // reopen action, so p1 is NOT re-prompted.
    assert!(
        round.is_done(),
        "round should close without re-prompting p1"
    );
    // Chip conservation: 100 + 30 + 100 = 230 in the pot this street.
    assert_eq!(round.pot_total().0, 230);
}

// ----------------------------------------------------------------------
// Regression: a player who already acted may not re-raise after a
// non-reopening (sub-minimum) all-in (P2 — TDA Rule 6, 2026-05-29 hunt).
// ----------------------------------------------------------------------
#[test]
fn non_reopening_all_in_blocks_reraise_from_acted_player() {
    // Post-flop 3-way, all deep except p3 who is short.
    // p1 1000, p2 1000, p3 130. current_bet starts 0, BB 20.
    let mut round = BettingRound::new(three_players(1000, 1000, 130), 0, c(0), c(100));
    // p1 bets 100 (full).
    round
        .apply_action(pid(1), &PlayerAction::Raise { amount: c(100) })
        .unwrap();
    // p2 calls 100.
    round.apply_action(pid(2), &PlayerAction::Call).unwrap();
    // p3 all-in for 130 — delta 30 < min-raise 100 => does NOT reopen.
    round.apply_action(pid(3), &PlayerAction::AllIn).unwrap();
    assert_eq!(round.current_bet().0, 130);
    // Action back to p1, who already acted. p1 owes 30 — may call or fold,
    // but a re-raise is illegal (action not reopened).
    assert_eq!(round.current_player(), Some(pid(1)));
    let illegal = round.apply_action(pid(1), &PlayerAction::Raise { amount: c(300) });
    assert!(
        matches!(illegal, Err(ActionError::InvalidAction)),
        "acted player may not re-raise a non-reopening all-in, got {illegal:?}"
    );
    // But p1 may legally call the extra 30.
    round.apply_action(pid(1), &PlayerAction::Call).unwrap();
    assert_eq!(round.player_contributed(pid(1)).0, 130);
}

/// A short all-in call may arrive through the `Raise(to=...)` wire shape.
/// It must be accepted for an already-acted player when it does not exceed
/// the table bet, while a genuine re-raise remains blocked above.
#[test]
fn acted_player_may_submit_raise_form_for_short_all_in_call() {
    let mut round = BettingRound::new(three_players(130, 1000, 130), 0, c(0), c(100));
    round
        .apply_action(pid(1), &PlayerAction::Raise { amount: c(100) })
        .unwrap();
    round.apply_action(pid(2), &PlayerAction::Call).unwrap();
    round.apply_action(pid(3), &PlayerAction::AllIn).unwrap();

    assert_eq!(round.current_bet().0, 130);
    assert_eq!(round.current_player(), Some(pid(1)));
    round
        .apply_action(pid(1), &PlayerAction::Raise { amount: c(130) })
        .unwrap();
    assert!(round.player_all_in(pid(1)));

    // The remaining deep player still owes the short all-in difference.
    assert_eq!(round.current_player(), Some(pid(2)));
    round.apply_action(pid(2), &PlayerAction::Call).unwrap();
    assert!(round.is_done());
}

/// Regression (audit 2026-06-03): `current_player_can_raise()` must report
/// `false` for an already-acted player facing only a non-reopening all-in,
/// so the engine does not advertise a `min_raise_to` it would then reject.
#[test]
fn current_player_cannot_raise_after_non_reopening_all_in() {
    let mut round = BettingRound::new(three_players(1000, 1000, 130), 0, c(0), c(100));
    round
        .apply_action(pid(1), &PlayerAction::Raise { amount: c(100) })
        .unwrap();
    round.apply_action(pid(2), &PlayerAction::Call).unwrap();
    round.apply_action(pid(3), &PlayerAction::AllIn).unwrap();
    // Action back to p1 (already acted, facing a non-reopening all-in).
    assert_eq!(round.current_player(), Some(pid(1)));
    assert!(
        !round.current_player_can_raise(),
        "p1 may only call/fold here — must not be told it can raise"
    );
}

/// A player who has NOT yet acted (fresh street) can raise.
#[test]
fn current_player_can_raise_on_fresh_street() {
    let round = BettingRound::new(two_players(1000, 1000), 0, c(0), c(20));
    assert!(
        round.current_player_can_raise(),
        "first actor on a fresh street may raise"
    );
}

/// A short all-in keeps the round open until both earlier callers respond.
#[test]
fn short_all_in_requires_both_earlier_callers_to_respond() {
    // Postflop 3-way, current_bet 0, bb 100. P1 bets 100, P2 calls 100,
    // P3 short all-in to 150 (delta 50 < 100 min → non-reopening).
    let mut round = BettingRound::new(three_players(1000, 1000, 150), 0, c(0), c(100));
    round
        .apply_action(pid(1), &PlayerAction::Raise { amount: c(100) })
        .unwrap();
    round.apply_action(pid(2), &PlayerAction::Call).unwrap();
    round.apply_action(pid(3), &PlayerAction::AllIn).unwrap();
    assert_eq!(round.current_bet().0, 150, "all-in raised the bet to 150");
    // Round stays open; action is back on P1 who owes 50.
    assert!(
        !round.is_done(),
        "P1 and P2 still owe 50 — round must stay open"
    );
    assert_eq!(round.current_player(), Some(pid(1)));
    round.apply_action(pid(1), &PlayerAction::Call).unwrap();
    assert!(!round.is_done(), "P2 still owes the additional 50 chips");
    assert_eq!(round.current_player(), Some(pid(2)));
    round.apply_action(pid(2), &PlayerAction::Call).unwrap();
    assert!(round.is_done());
    assert_eq!(round.pot_total(), c(450));
}

// A FULL raise must still reopen action for a player who already acted.
#[test]
fn full_raise_reopens_action_for_acted_player() {
    let mut round = BettingRound::new(three_players(1000, 1000, 1000), 0, c(0), c(20));
    round
        .apply_action(pid(1), &PlayerAction::Raise { amount: c(100) })
        .unwrap(); // p1 opens
    round.apply_action(pid(2), &PlayerAction::Call).unwrap(); // p2 calls
                                                              // p3 makes a FULL raise to 300 (delta 200 >= 100) — reopens.
    round
        .apply_action(pid(3), &PlayerAction::Raise { amount: c(300) })
        .unwrap();
    // p1 (already acted) is reopened and MAY re-raise legally.
    assert_eq!(round.current_player(), Some(pid(1)));
    let ok = round.apply_action(pid(1), &PlayerAction::Raise { amount: c(700) });
    assert!(ok.is_ok(), "full raise must reopen action, got {ok:?}");
}
