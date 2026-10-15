//! GameHand regression tests: settlement.

use super::*;

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
