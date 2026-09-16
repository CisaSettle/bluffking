//! Blind-mode (ADR-066 P1) `GameHand` tests (moved out of `game.rs`: the
//! inline test modules were more than half the file). Same module path, same
//! `use super::*`.

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
fn hole(a: Card, b: Card) -> HoleCards {
    HoleCards::new(a, b)
}

/// Drive blind-mode betting to completion, injecting boards as the engine
/// requests them (`pending_board_street`). Returns once `is_done()`.
/// `act` decides the action for the current actor.
fn run_blind_betting(
    hand: &mut GameHand,
    board_cards: &[Card],
    mut act: impl FnMut(&GameSnapshot, PlayerId) -> PlayerAction,
) {
    // board_cards is the full 5-card board in flop(3)/turn(1)/river(1) order;
    // we feed slices as each street's board is requested.
    let mut board_offset = 0usize;
    let mut safety = 0;
    loop {
        if hand.is_done() {
            break;
        }
        safety += 1;
        assert!(safety < 200, "blind betting loop runaway");

        // Inject any pending board before acting on the new street.
        if let Some(street) = hand.pending_board_street() {
            let n = cards_for_street(street);
            let slice = &board_cards[board_offset..board_offset + n];
            board_offset += n;
            hand.inject_board_for_street(slice).expect("inject board");
            continue;
        }

        let snap = hand.snapshot();
        let actor = match snap.current_actor {
            Some(a) => a,
            None => break,
        };
        let action = act(&snap, actor);
        hand.apply_action(actor, action).expect("apply action");
    }
    // Drain any trailing pending board (all-in runout can leave the last
    // street's board pending with the round already done).
    while let Some(street) = hand.pending_board_street() {
        let n = cards_for_street(street);
        let slice = &board_cards[board_offset..board_offset + n];
        board_offset += n;
        hand.inject_board_for_street(slice)
            .expect("inject trailing board");
    }
}

/// A standard distinct 5-card board (no overlap with the hole cards used in
/// these tests). Order: flop[0..3], turn[3], river[4].
fn standard_board() -> [Card; 5] {
    [
        card(Rank::Two, Suit::Clubs),
        card(Rank::Seven, Suit::Diamonds),
        card(Rank::Nine, Suit::Spades),
        card(Rank::Jack, Suit::Hearts),
        card(Rank::Four, Suit::Clubs),
    ]
}

// ---- 1. blind-mode betting runs a full hand with no hole values ----

/// A full blind-mode hand runs its entire betting + board reveals with the
/// engine holding NO plaintext hole values: every seat stays `Opaque` and
/// every snapshot redacts `hole_cards` to `None` until a reveal is injected.
#[test]
fn blind_full_hand_no_hole_values() {
    let mut hand = GameHand::new_blind(
        vec![(pid(1), c(1000), 0), (pid(2), c(1000), 1)],
        0,
        c(20),
        c(10),
        DeckSeed::default(),
    );
    hand.start().unwrap();

    // Blind start emits NO HoleCardsDealt event at all (no plaintext value
    // ever leaves the engine; no redacted variant is introduced either).
    let evs = hand.drain_events();
    assert!(
        evs.iter()
            .all(|e| !matches!(e, EngineEvent::HoleCardsDealt { .. })),
        "blind start must NOT emit any HoleCardsDealt event"
    );

    // Every snapshot during betting: hole_cards is None for all seats.
    let snap = hand.snapshot();
    assert!(
        snap.players.iter().all(|p| p.hole_cards.is_none()),
        "no plaintext hole values in a blind snapshot before showdown"
    );

    let board = standard_board();
    run_blind_betting(&mut hand, &board, |snap, actor| {
        // Everyone checks/calls to showdown.
        let committed = snap
            .players
            .iter()
            .find(|p| p.player_id == actor)
            .map(|p| p.committed_this_street.0)
            .unwrap_or(0);
        let to_call = snap.current_bet.0.saturating_sub(committed);
        if to_call > 0 {
            PlayerAction::Call
        } else {
            PlayerAction::Check
        }
    });

    assert!(hand.is_done());
    // Full board got revealed via injection.
    let snap = hand.snapshot();
    assert_eq!(snap.board.count(), 5, "full board revealed via injection");
    // Still no plaintext holes (no reveals injected yet).
    assert!(snap.players.iter().all(|p| p.hole_cards.is_none()));

    // STRUCTURAL: right up to showdown, EVERY seat slot is still `Opaque` —
    // no `Plain`/`Revealed` was ever written in blind mode. There is no code
    // path that produces a plaintext hole value before an injected reveal.
    assert!(
        hand.seats.iter().all(|s| s.hole == HoleSlot::Opaque),
        "every blind seat remains Opaque before any showdown reveal"
    );

    // Two contenders → finish_blind requires reveals; without them it errors.
    let err = hand.finish_blind().unwrap_err();
    assert!(matches!(err, BlindFinishError::MissingReveal { .. }));
}

// ---- 2. fold-around / last-man-standing: NO reveal, NO rank_hand ----

/// Heads-up fold pre-flop: the lone survivor takes the pot with NO injected
/// reveal and an EMPTY showdown — structural zero-leak (rank_hand never runs
/// because finish_blind takes the uncontested path).
#[test]
fn blind_fold_around_awards_without_reveal() {
    let mut hand = GameHand::new_blind(
        vec![(pid(1), c(1000), 0), (pid(2), c(1000), 1)],
        0,
        c(20),
        c(10),
        DeckSeed::default(),
    );
    hand.start().unwrap();
    // SB (button, pid1) acts first heads-up — fold.
    let actor = hand.snapshot().current_actor.unwrap();
    assert_eq!(actor, pid(1));
    hand.apply_action(actor, PlayerAction::Fold).unwrap();
    assert!(hand.is_done());

    // No reveal injected. finish_blind awards the lone survivor and produces
    // an empty showdown.
    let result = hand.finish_blind().expect("uncontested finish");
    assert!(
        result.showdown.is_empty(),
        "uncontested blind hand has an EMPTY showdown (zero leak)"
    );
    assert!(
        result.show_order.is_empty(),
        "no show order for an uncontested hand"
    );
    // Winner is pid(2) (BB), takes SB(10)+BB(20)=30.
    let total: u32 = result.chips_awarded.values().sum();
    assert_eq!(total, 30, "pot fully awarded");
    assert_eq!(
        *result.chips_awarded.get(&pid(2).inner()).unwrap(),
        30,
        "lone survivor (BB) wins the whole pot"
    );
    // Board must be empty — fold-around reveals no community cards.
    assert_eq!(result.board.count(), 0);
}

/// 3-handed: two fold, the lone survivor wins with no reveal and no board.
#[test]
fn blind_last_man_standing_three_handed() {
    let mut hand = GameHand::new_blind(
        vec![
            (pid(1), c(500), 0),
            (pid(2), c(500), 1),
            (pid(3), c(500), 2),
        ],
        0,
        c(20),
        c(10),
        DeckSeed::default(),
    );
    hand.start().unwrap();
    // UTG (pid1) folds, SB (pid2) folds → BB (pid3) wins.
    let a = hand.snapshot().current_actor.unwrap();
    hand.apply_action(a, PlayerAction::Fold).unwrap();
    let a = hand.snapshot().current_actor.unwrap();
    hand.apply_action(a, PlayerAction::Fold).unwrap();
    assert!(hand.is_done());

    let result = hand.finish_blind().expect("uncontested finish");
    assert!(result.showdown.is_empty());
    assert_eq!(
        *result.chips_awarded.get(&pid(3).inner()).unwrap(),
        30,
        "lone survivor wins SB+BB"
    );
}

// ---- 3. 2-player showdown with injected reveals ranks the correct winner ----

/// Heads-up showdown: with both reveals injected, finish_blind ranks the
/// correct winner from ONLY the injected reveals.
#[test]
fn blind_two_player_showdown_ranks_correct_winner() {
    let mut hand = GameHand::new_blind(
        vec![(pid(1), c(1000), 0), (pid(2), c(1000), 1)],
        0,
        c(20),
        c(10),
        DeckSeed::default(),
    );
    hand.start().unwrap();

    let board = standard_board(); // 2c 7d 9s | Jh | 4c
    run_blind_betting(&mut hand, &board, |snap, actor| {
        let committed = snap
            .players
            .iter()
            .find(|p| p.player_id == actor)
            .map(|p| p.committed_this_street.0)
            .unwrap_or(0);
        let to_call = snap.current_bet.0.saturating_sub(committed);
        if to_call > 0 {
            PlayerAction::Call
        } else {
            PlayerAction::Check
        }
    });
    assert!(hand.is_done());

    // pid(1) holds a pair of nines (9h+9? — use 9d+Ah → pair of nines, A kicker)
    // pid(2) holds A-K offsuit → ace-high only. pid(1) must win with the pair.
    let p1 = hole(
        card(Rank::Nine, Suit::Diamonds),
        card(Rank::Ace, Suit::Hearts),
    );
    let p2 = hole(card(Rank::Ace, Suit::Spades), card(Rank::King, Suit::Clubs));

    hand.inject_showdown_reveal(pid(1), p1).expect("reveal p1");
    hand.inject_showdown_reveal(pid(2), p2).expect("reveal p2");

    let result = hand.finish_blind().expect("contested finish");
    assert_eq!(
        result.showdown.len(),
        2,
        "two reveals → two showdown entries"
    );
    // The pot (BB*2 = 40) goes entirely to pid(1) (pair of 9s > ace high).
    assert_eq!(
        *result.chips_awarded.get(&pid(1).inner()).unwrap(),
        40,
        "pair of nines beats ace-high"
    );
    assert_eq!(*result.chips_awarded.get(&pid(2).inner()).unwrap(), 0);
    // show_order is populated for a contested showdown.
    assert_eq!(result.show_order.len(), 2);
}

// ---- 4. 3-way all-in with side pots + injected reveals distributes correctly ----

/// Three players with unequal stacks all-in pre-flop → a main pot + side pot.
/// With all three reveals injected, finish_blind distributes both layers to
/// the correct winners.
#[test]
fn blind_three_way_all_in_side_pots() {
    // Stacks 100 / 200 / 300 → all-in creates layered pots.
    let mut hand = GameHand::new_blind(
        vec![
            (pid(1), c(100), 0), // short
            (pid(2), c(200), 1), // mid
            (pid(3), c(300), 2), // deep
        ],
        0,
        c(20),
        c(10),
        DeckSeed::default(),
    );
    hand.start().unwrap();

    // The short and mid stacks shove; the deep stack is then the sole live
    // stack and calls the 200 wager, retaining its unmatched 100 instead of
    // manufacturing a dry side pot. Betting closes and the board runs out.
    let board = standard_board();
    run_blind_betting(&mut hand, &board, |_snap, actor| {
        if actor == pid(3) {
            PlayerAction::Call
        } else {
            PlayerAction::AllIn
        }
    });
    assert!(hand.is_done());
    let snap = hand.snapshot();
    assert_eq!(snap.board.count(), 5, "all-in runout reveals full board");

    // Inject reveals. Give the SHORT stack (pid1) the best hand so it scoops
    // the main pot it is eligible for; pid3 (deep) beats pid2 for the side
    // pot. Board: 2c 7d 9s Jh 4c.
    //   pid1: Js Jd → trips jacks (Jh on board) → strongest.
    //   pid3: 9h 9c → trips nines (9s on board) → second.
    //   pid2: A-K   → ace high → weakest.
    let p1 = hole(
        card(Rank::Jack, Suit::Spades),
        card(Rank::Jack, Suit::Diamonds),
    );
    let p2 = hole(
        card(Rank::Ace, Suit::Diamonds),
        card(Rank::King, Suit::Hearts),
    );
    let p3 = hole(
        card(Rank::Nine, Suit::Hearts),
        card(Rank::Nine, Suit::Clubs),
    );
    hand.inject_showdown_reveal(pid(1), p1).unwrap();
    hand.inject_showdown_reveal(pid(2), p2).unwrap();
    hand.inject_showdown_reveal(pid(3), p3).unwrap();

    let result = hand.finish_blind().expect("contested side-pot finish");

    // Chip conservation: 100 + 200 + 300 = 600 total in play.
    let total_final: u32 = result.final_stacks.values().sum();
    assert_eq!(total_final, 600, "chip conservation across blind side pots");

    // pid1 (trips jacks) wins the main pot (all three contribute 100 → 300).
    // pid3 (trips nines) beats pid2 for the 200-chip second layer (pid2,pid3
    // contribute 100 each above the 100 cap). pid3's unmatched top 100 never
    // enters the pot.
    let p1_award = *result.chips_awarded.get(&pid(1).inner()).unwrap();
    let p3_award = *result.chips_awarded.get(&pid(3).inner()).unwrap();
    let p2_award = *result.chips_awarded.get(&pid(2).inner()).unwrap();
    assert_eq!(p1_award, 300, "short stack scoops the 300 main pot");
    assert_eq!(p2_award, 0, "mid stack (ace high) wins nothing");
    assert_eq!(p3_award, 200, "deep stack wins only the contested side pot");
    assert_eq!(
        result.final_stacks.get(&pid(3).inner()),
        Some(&300),
        "deep stack retains 100 and wins the 200 side pot"
    );
}

// ---- 4b. forfeit economics: a side pot eligible ONLY to forfeiters must
//          NOT be refunded to the forfeiters (ADR-066 §5 T-LIVE / audit F3) ----

/// Three players with unequal stacks go all-in pre-flop → a main pot
/// (eligible to all three) + a side pot (eligible only to the two deep
/// stacks). At showdown ONLY the short stack reveals; both deep stacks
/// FORFEIT (withhold their reveal).
///
/// The bug (audit F3): the side pot's eligible set is exactly the two
/// forfeiters, so `distribute_pots` finds no contender for it and refunds
/// the layer to its eligible contributors — i.e. it hands the forfeiters
/// their side-pot chips back. ADR-066 §5 forbids that: a forfeiter's
/// committed chips "stay in the pot and are awarded to the contenders who DID
/// reveal." The honest revealer (the short stack) must receive the forfeited
/// side pot, NOT the forfeiters. Chips are conserved either way, so the
/// conservation assert cannot catch this — only an explicit per-player award
/// check does.
#[test]
fn blind_forfeited_side_pot_goes_to_revealer_not_refunded() {
    // Stacks 100 / 1000 / 1000. Short stack (pid1) all-in for 100; the two
    // deep stacks (pid2, pid3) call all-in for 1000 each. Layers:
    //   main pot  = 100 * 3 = 300, eligible {pid1, pid2, pid3}
    //   side pot  = 900 * 2 = 1800, eligible {pid2, pid3} only
    let mut hand = GameHand::new_blind(
        vec![
            (pid(1), c(100), 0),  // short
            (pid(2), c(1000), 1), // deep
            (pid(3), c(1000), 2), // deep
        ],
        0,
        c(20),
        c(10),
        DeckSeed::default(),
    );
    hand.start().unwrap();

    let board = standard_board();
    run_blind_betting(&mut hand, &board, |_snap, _actor| PlayerAction::AllIn);
    assert!(hand.is_done());
    assert_eq!(hand.snapshot().board.count(), 5);

    // ONLY the short stack reveals. Give it any hand (it is the sole honest
    // revealer, so it wins the main pot it is eligible for, and per ADR-066
    // §5 must also receive the forfeited side pot). pid2 / pid3 WITHHOLD.
    let p1 = hole(
        card(Rank::Jack, Suit::Spades),
        card(Rank::Jack, Suit::Diamonds),
    );
    hand.inject_showdown_reveal(pid(1), p1).unwrap();
    // pid2 and pid3 do NOT reveal → forfeit at finish_blind_with_forfeits.

    let result = hand
        .finish_blind_with_forfeits()
        .expect("a single honest revealer wins by forfeit, not void");

    // Chip conservation (cannot catch the bug, but must always hold).
    let total_final: u32 = result.final_stacks.values().sum();
    assert_eq!(total_final, 2100, "chip conservation: 100 + 1000 + 1000");

    let p1_award = *result.chips_awarded.get(&pid(1).inner()).unwrap_or(&0);
    let p2_award = *result.chips_awarded.get(&pid(2).inner()).unwrap_or(&0);
    let p3_award = *result.chips_awarded.get(&pid(3).inner()).unwrap_or(&0);

    // The forfeiters get NOTHING — not even their side-pot stake back.
    assert_eq!(
        p2_award, 0,
        "forfeiter pid2 must be awarded ZERO (no side-pot refund)"
    );
    assert_eq!(
        p3_award, 0,
        "forfeiter pid3 must be awarded ZERO (no side-pot refund)"
    );
    // The lone honest revealer receives the ENTIRE pot: main (300) + the
    // forfeited side pot (1800) = 2100.
    assert_eq!(
        p1_award, 2100,
        "the honest revealer wins the main pot AND the forfeited side pot \
             (forfeit-not-refund)"
    );
    // And its final stack reflects winning everyone's chips.
    let p1_final = *result.final_stacks.get(&pid(1).inner()).unwrap_or(&0);
    assert_eq!(p1_final, 2100, "the sole revealer scoops the whole table");
}

// ---- 4c. LIVE-F1: a board-share withholder FORFEITS its committed (all-in)
//          stake to the honest survivor — never a blameless refund ----

/// Two players go all-in pre-flop; the FLOP is injected, but the TURN board
/// cannot be revealed because pid2 withholds its board share. The coordinator
/// attributes the failure to pid2 and calls `finish_blind_forfeit_board`.
/// pid2 must FORFEIT its committed all-in stake to pid1 (the honest survivor),
/// NOT recover it via a void+refund. Chips conserved.
#[test]
fn blind_board_withholder_forfeits_committed_stake() {
    let mut hand = GameHand::new_blind(
        vec![(pid(1), c(1000), 0), (pid(2), c(1000), 1)],
        0,
        c(20),
        c(10),
        DeckSeed::default(),
    );
    hand.start().unwrap();

    // Both shove all-in pre-flop (heads-up: SB=pid1 acts first).
    let a0 = hand.snapshot().current_actor.unwrap();
    hand.apply_action(a0, PlayerAction::AllIn).unwrap();
    let a1 = hand.snapshot().current_actor.unwrap();
    hand.apply_action(a1, PlayerAction::AllIn).unwrap();

    // Inject ONLY the flop (the board reveal is interleaved after betting).
    let board = standard_board();
    assert_eq!(hand.pending_board_street(), Some(Street::Flop));
    hand.inject_board_for_street(&board[0..3])
        .expect("inject flop");
    // The TURN board is now pending and pid2 withholds its share.
    assert_eq!(hand.pending_board_street(), Some(Street::Turn));
    assert!(!hand.is_done(), "hand stuck awaiting the turn board");

    // pid2 is the attributable board withholder → forfeit-board resolution.
    let result = hand
        .finish_blind_forfeit_board(&[pid(2)])
        .expect("≥1 honest survivor → Completed, not void");

    // Chip conservation: 1000 + 1000 = 2000 total, no chip created/destroyed.
    let total_final: u32 = result.final_stacks.values().sum();
    assert_eq!(
        total_final, 2000,
        "chip conservation across deal → all-in → board-withhold forfeit"
    );
    // pid1 (honest survivor) wins the WHOLE pot; pid2 (withholder) gets 0.
    let p1_award = *result.chips_awarded.get(&pid(1).inner()).unwrap_or(&0);
    let p2_award = *result.chips_awarded.get(&pid(2).inner()).unwrap_or(&0);
    assert_eq!(p1_award, 2000, "honest survivor scoops the pot");
    assert_eq!(
        p2_award, 0,
        "the board withholder is awarded ZERO (forfeit-not-refund)"
    );
    // The withholder LOSES its committed all-in stake (final 0), it is NOT
    // refunded to ~1000 as the old void+refund path would have done.
    let p2_final = *result
        .final_stacks
        .get(&pid(2).inner())
        .unwrap_or(&u32::MAX);
    assert_eq!(
        p2_final, 0,
        "the all-in withholder must lose its whole committed stake (not be refunded)"
    );
    assert_eq!(
        *result.final_stacks.get(&pid(1).inner()).unwrap_or(&0),
        2000,
        "the honest survivor ends with the whole table"
    );
    assert!(
        result.showdown.is_empty(),
        "board-forfeit settlement must not fabricate a showdown"
    );
    assert!(
        result.show_order.is_empty(),
        "board-forfeit settlement must not expose a showdown order"
    );
}

/// LIVE-F1 residual: when ≥2 HONEST contenders remain after the withholder is
/// force-folded, the board still cannot rank them — they SPLIT the contested
/// pot equally (the withholder is excluded, never refunded). Chips conserved.
#[test]
fn blind_board_withholder_residual_splits_among_honest() {
    // 3 players, equal stacks all-in pre-flop → a single 3000 main pot.
    let mut hand = GameHand::new_blind(
        vec![
            (pid(1), c(1000), 0),
            (pid(2), c(1000), 1),
            (pid(3), c(1000), 2),
        ],
        0,
        c(20),
        c(10),
        DeckSeed::default(),
    );
    hand.start().unwrap();
    let board = standard_board();
    // Everyone shoves; drive only the PRE-FLOP betting (stop before injecting
    // the flop) so the flop board is pending and pid3 can withhold it.
    let mut safety = 0;
    while let Some(actor) = hand.snapshot().current_actor {
        hand.apply_action(actor, PlayerAction::AllIn).unwrap();
        safety += 1;
        assert!(safety < 50, "preflop runaway");
    }
    assert_eq!(hand.pending_board_street(), Some(Street::Flop));
    // Inject the flop, then pid3 withholds the TURN board → 2 honest remain.
    hand.inject_board_for_street(&board[0..3]).unwrap();
    assert_eq!(hand.pending_board_street(), Some(Street::Turn));

    let result = hand
        .finish_blind_forfeit_board(&[pid(3)])
        .expect("2 honest survivors → Completed (equal chop), not void");

    // Chip conservation: 3000 total.
    let total_final: u32 = result.final_stacks.values().sum();
    assert_eq!(total_final, 3000, "chip conservation in the residual split");
    // The withholder (pid3) gets nothing; pid1 + pid2 split the 3000 pot.
    let p1 = *result.chips_awarded.get(&pid(1).inner()).unwrap_or(&0);
    let p2 = *result.chips_awarded.get(&pid(2).inner()).unwrap_or(&0);
    let p3 = *result.chips_awarded.get(&pid(3).inner()).unwrap_or(&0);
    assert_eq!(p3, 0, "the withholder is excluded (forfeit-not-refund)");
    assert_eq!(
        p1 + p2,
        3000,
        "the two honest survivors split the whole pot"
    );
    assert_eq!(p1, 1500, "equal chop: pid1 half");
    assert_eq!(p2, 1500, "equal chop: pid2 half");
    // The withholder loses its committed stake (final 0), never refunded.
    assert_eq!(
        *result
            .final_stacks
            .get(&pid(3).inner())
            .unwrap_or(&u32::MAX),
        0,
        "the withholder loses its full committed stake"
    );
}

/// LIVE-F1 audit F5 (HIGH): the board-withhold→refund exploit SURVIVING in
/// the side-pot layer. An honest short all-in survivor (pid1) keeps the hand
/// out of `NoSurvivor`, but a side pot whose eligible set is ENTIRELY the two
/// deep withholders (pid2, pid3) was previously refunded back to {pid2, pid3}
/// at `distribute_pots_among_survivors` — recovering their matched side-pot
/// stake by stalling the turn board. That makes withholding a side-pot board
/// share +EV. The fix awards that withholder-only side-pot layer to the honest
/// overall survivor (pid1) instead; the withholders never recover a matched
/// stake. Only `refund_to` (uncalled overage) ever refunds. Chips conserved.
#[test]
fn blind_board_withholder_only_side_pot_goes_to_survivor_not_refunded() {
    // Stacks 100 / 1000 / 1000 — pid1 short all-in; pid2/pid3 deep. All shove
    // pre-flop. Layers:
    //   main pot = 100 * 3 = 300, eligible {pid1, pid2, pid3}
    //   side pot = 900 * 2 = 1800, eligible {pid2, pid3} ONLY
    let mut hand = GameHand::new_blind(
        vec![
            (pid(1), c(100), 0),  // short, honest
            (pid(2), c(1000), 1), // deep, withholder
            (pid(3), c(1000), 2), // deep, withholder
        ],
        0,
        c(20),
        c(10),
        DeckSeed::default(),
    );
    hand.start().unwrap();
    let board = standard_board();
    // Everyone shoves; drive ONLY the pre-flop betting so the flop board is
    // pending and pid2/pid3 can withhold a later street's board.
    let mut safety = 0;
    while let Some(actor) = hand.snapshot().current_actor {
        hand.apply_action(actor, PlayerAction::AllIn).unwrap();
        safety += 1;
        assert!(safety < 50, "preflop runaway");
    }
    assert_eq!(hand.pending_board_street(), Some(Street::Flop));
    // Inject the flop, then pid2 + pid3 withhold the TURN board. pid1 (the
    // short all-in) is the only honest survivor; the side pot is eligible to
    // {pid2, pid3} only — exactly the withholders.
    hand.inject_board_for_street(&board[0..3]).unwrap();
    assert_eq!(hand.pending_board_street(), Some(Street::Turn));

    let result = hand
        .finish_blind_forfeit_board(&[pid(2), pid(3)])
        .expect("pid1 honest survivor → Completed, not NoSurvivor");

    // Chip conservation: 100 + 1000 + 1000 = 2100, nothing created/destroyed.
    let total_final: u32 = result.final_stacks.values().sum();
    assert_eq!(
        total_final, 2100,
        "chip conservation across deal → all-in → side-pot board-withhold forfeit"
    );

    let p1_award = *result.chips_awarded.get(&pid(1).inner()).unwrap_or(&0);
    let p2_award = *result.chips_awarded.get(&pid(2).inner()).unwrap_or(&0);
    let p3_award = *result.chips_awarded.get(&pid(3).inner()).unwrap_or(&0);

    // BEFORE the F5 fix: the withholder-only side pot (1800) was refunded to
    // {pid2, pid3} (900 each) — recovering their matched stake. AFTER: that
    // layer goes to the honest survivor pid1, and the withholders get ZERO.
    assert_eq!(
        p2_award, 0,
        "withholder pid2 must be awarded ZERO (no side-pot refund)"
    );
    assert_eq!(
        p3_award, 0,
        "withholder pid3 must be awarded ZERO (no side-pot refund)"
    );
    // pid1 scoops the main pot (300) AND the forfeited withholder-only side
    // pot (1800) = 2100.
    assert_eq!(
        p1_award, 2100,
        "the honest survivor wins the main pot AND the withholder-only side pot \
             (forfeit-not-refund)"
    );

    // The withholders end BELOW their starting stack (they committed 1000 and
    // recovered nothing).
    let p2_final = *result
        .final_stacks
        .get(&pid(2).inner())
        .unwrap_or(&u32::MAX);
    let p3_final = *result
        .final_stacks
        .get(&pid(3).inner())
        .unwrap_or(&u32::MAX);
    assert!(
        p2_final < 1000,
        "withholder pid2 must end BELOW its 1000 starting stack (final {p2_final})"
    );
    assert!(
        p3_final < 1000,
        "withholder pid3 must end BELOW its 1000 starting stack (final {p3_final})"
    );
    assert_eq!(
        p2_final, 0,
        "withholder pid2 forfeits its whole committed stake"
    );
    assert_eq!(
        p3_final, 0,
        "withholder pid3 forfeits its whole committed stake"
    );
    assert_eq!(
        *result.final_stacks.get(&pid(1).inner()).unwrap_or(&0),
        2100,
        "the honest survivor ends with the whole table"
    );
}

/// LIVE-F1 edge: if EVERY contender is named a withholder, there is no honest
/// survivor — `finish_blind_forfeit_board` returns `NoSurvivor` so the server
/// VOIDs with a blameless stake-return (never invents a winner).
#[test]
fn blind_board_all_withhold_returns_no_survivor() {
    let mut hand = GameHand::new_blind(
        vec![(pid(1), c(1000), 0), (pid(2), c(1000), 1)],
        0,
        c(20),
        c(10),
        DeckSeed::default(),
    );
    hand.start().unwrap();
    let a0 = hand.snapshot().current_actor.unwrap();
    hand.apply_action(a0, PlayerAction::AllIn).unwrap();
    let a1 = hand.snapshot().current_actor.unwrap();
    hand.apply_action(a1, PlayerAction::AllIn).unwrap();
    let board = standard_board();
    hand.inject_board_for_street(&board[0..3]).unwrap();

    let err = hand
        .finish_blind_forfeit_board(&[pid(1), pid(2)])
        .expect_err("no honest survivor → NoSurvivor (caller VOIDs)");
    assert_eq!(err, BlindFinishError::NoSurvivor);
}

// ---- 5. rejection: reveal for folded seat, wrong-count board ----

/// Injecting a reveal for a FOLDED seat is rejected (folded cards stay sealed).
#[test]
fn blind_reject_reveal_for_folded_seat() {
    let mut hand = GameHand::new_blind(
        vec![
            (pid(1), c(500), 0),
            (pid(2), c(500), 1),
            (pid(3), c(500), 2),
        ],
        0,
        c(20),
        c(10),
        DeckSeed::default(),
    );
    hand.start().unwrap();
    // UTG (pid1) folds.
    let a = hand.snapshot().current_actor.unwrap();
    assert_eq!(a, pid(1));
    hand.apply_action(a, PlayerAction::Fold).unwrap();
    // SB calls, BB checks → flop. Then both check down to showdown.
    let board = standard_board();
    run_blind_betting(&mut hand, &board, |snap, actor| {
        let committed = snap
            .players
            .iter()
            .find(|p| p.player_id == actor)
            .map(|p| p.committed_this_street.0)
            .unwrap_or(0);
        let to_call = snap.current_bet.0.saturating_sub(committed);
        if to_call > 0 {
            PlayerAction::Call
        } else {
            PlayerAction::Check
        }
    });
    assert!(hand.is_done());

    // Try to inject a reveal for the folded seat (pid1) → rejected.
    let p1 = hole(
        card(Rank::Two, Suit::Hearts),
        card(Rank::Three, Suit::Hearts),
    );
    let err = hand.inject_showdown_reveal(pid(1), p1).unwrap_err();
    assert_eq!(err, BlindInjectError::PlayerFolded(pid(1)));
}

/// Injecting the wrong number of board cards for a street is rejected.
#[test]
fn blind_reject_wrong_board_count() {
    let mut hand = GameHand::new_blind(
        vec![(pid(1), c(1000), 0), (pid(2), c(1000), 1)],
        0,
        c(20),
        c(10),
        DeckSeed::default(),
    );
    hand.start().unwrap();
    // Heads-up: SB calls, BB checks → flop is pending.
    let a = hand.snapshot().current_actor.unwrap();
    hand.apply_action(a, PlayerAction::Call).unwrap();
    let a = hand.snapshot().current_actor.unwrap();
    hand.apply_action(a, PlayerAction::Check).unwrap();

    assert_eq!(hand.pending_board_street(), Some(Street::Flop));
    // Flop needs 3 cards; inject 1 → WrongBoardCount.
    let one = [card(Rank::Two, Suit::Clubs)];
    let err = hand.inject_board_for_street(&one).unwrap_err();
    assert_eq!(
        err,
        BlindInjectError::WrongBoardCount {
            street: Street::Flop,
            expected: 3,
            got: 1
        }
    );
    // The correct count is accepted.
    let flop = [
        card(Rank::Two, Suit::Clubs),
        card(Rank::Seven, Suit::Diamonds),
        card(Rank::Nine, Suit::Spades),
    ];
    hand.inject_board_for_street(&flop)
        .expect("correct flop count");
    assert_eq!(hand.snapshot().board.count(), 3);
}

/// Injecting a reveal for an unknown player id is rejected.
#[test]
fn blind_reject_reveal_unknown_player() {
    let mut hand = GameHand::new_blind(
        vec![(pid(1), c(1000), 0), (pid(2), c(1000), 1)],
        0,
        c(20),
        c(10),
        DeckSeed::default(),
    );
    hand.start().unwrap();
    let a = hand.snapshot().current_actor.unwrap();
    hand.apply_action(a, PlayerAction::Fold).unwrap();
    assert!(hand.is_done());
    let h = hole(
        card(Rank::Two, Suit::Hearts),
        card(Rank::Three, Suit::Hearts),
    );
    let err = hand.inject_showdown_reveal(pid(99), h).unwrap_err();
    assert_eq!(err, BlindInjectError::UnknownPlayer(pid(99)));
}

/// Cross-mode guards: blind injectors reject a plaintext hand; `finish` and
/// `finish_blind` are mode-exclusive.
#[test]
fn blind_injectors_reject_plaintext_hand() {
    let mut hand = GameHand::new_with_rng(
        vec![(pid(1), c(1000), 0), (pid(2), c(1000), 1)],
        0,
        c(20),
        c(10),
        PokerRng::from_seed(1),
    );
    hand.start().unwrap();
    let flop = [
        card(Rank::Two, Suit::Clubs),
        card(Rank::Seven, Suit::Diamonds),
        card(Rank::Nine, Suit::Spades),
    ];
    assert_eq!(
        hand.inject_board_for_street(&flop).unwrap_err(),
        BlindInjectError::NotBlindMode
    );
    let h = hole(
        card(Rank::Two, Suit::Hearts),
        card(Rank::Three, Suit::Hearts),
    );
    assert_eq!(
        hand.inject_showdown_reveal(pid(1), h).unwrap_err(),
        BlindInjectError::NotBlindMode
    );
}

/// `inject_showdown_reveal` before the hand is done is rejected — reveals are
/// a showdown-only concept.
#[test]
fn blind_reject_reveal_before_showdown() {
    let mut hand = GameHand::new_blind(
        vec![(pid(1), c(1000), 0), (pid(2), c(1000), 1)],
        0,
        c(20),
        c(10),
        DeckSeed::default(),
    );
    hand.start().unwrap();
    // Still preflop, not done.
    let h = hole(
        card(Rank::Two, Suit::Hearts),
        card(Rank::Three, Suit::Hearts),
    );
    assert_eq!(
        hand.inject_showdown_reveal(pid(1), h).unwrap_err(),
        BlindInjectError::NotAtShowdown
    );
}

/// `new_blind` deals nothing: every seat starts `Opaque` and no plaintext
/// is reachable through the public seat accessor.
#[test]
fn blind_new_blind_seats_start_opaque() {
    let hand = GameHand::new_blind(
        vec![(pid(1), c(1000), 0), (pid(2), c(1000), 1)],
        0,
        c(20),
        c(10),
        DeckSeed::default(),
    );
    for seat in &hand.seats {
        assert!(seat.hole.is_opaque(), "blind seat must start Opaque");
        assert!(
            seat.hole_cards().is_none(),
            "no plaintext reachable for an Opaque seat"
        );
    }
}

// ---- F1: injected cards must be unique (the engine is the authoritative
//          ranker and must NOT trust injected cards). ----

/// Drive a heads-up blind hand to the point where the flop board is pending,
/// returning the started+pre-flop-closed hand. Both players check/call to the
/// flop street so `pending_board_street() == Some(Flop)`.
fn heads_up_to_pending_flop() -> GameHand {
    let mut hand = GameHand::new_blind(
        vec![(pid(1), c(1000), 0), (pid(2), c(1000), 1)],
        0,
        c(20),
        c(10),
        DeckSeed::default(),
    );
    hand.start().unwrap();
    // SB (button) calls, BB checks → flop pending.
    let a = hand.snapshot().current_actor.unwrap();
    hand.apply_action(a, PlayerAction::Call).unwrap();
    let a = hand.snapshot().current_actor.unwrap();
    hand.apply_action(a, PlayerAction::Check).unwrap();
    assert_eq!(hand.pending_board_street(), Some(Street::Flop));
    hand
}

/// A board card equal to another card in the SAME injected flop is rejected
/// with `DuplicateCard` — the engine never stores a deck with a repeated card.
#[test]
fn blind_reject_duplicate_within_injected_flop() {
    let mut hand = heads_up_to_pending_flop();
    let dup = card(Rank::Two, Suit::Clubs);
    let flop = [dup, card(Rank::Seven, Suit::Diamonds), dup];
    let err = hand.inject_board_for_street(&flop).unwrap_err();
    assert_eq!(err, BlindInjectError::DuplicateCard { card: dup });
    // Board untouched — rejection happens before any card is stored.
    assert_eq!(hand.snapshot().board.count(), 0);
}

/// A turn card equal to an existing board (flop) card is rejected.
#[test]
fn blind_reject_board_card_equals_existing_board_card() {
    let mut hand = heads_up_to_pending_flop();
    let flop = [
        card(Rank::Two, Suit::Clubs),
        card(Rank::Seven, Suit::Diamonds),
        card(Rank::Nine, Suit::Spades),
    ];
    hand.inject_board_for_street(&flop).expect("valid flop");
    // Heads-up flop betting: SB(BB position?) → both check to turn.
    let a = hand.snapshot().current_actor.unwrap();
    hand.apply_action(a, PlayerAction::Check).unwrap();
    let a = hand.snapshot().current_actor.unwrap();
    hand.apply_action(a, PlayerAction::Check).unwrap();
    assert_eq!(hand.pending_board_street(), Some(Street::Turn));
    // Turn duplicates the existing flop card 2c → DuplicateCard.
    let dup = card(Rank::Two, Suit::Clubs);
    let err = hand.inject_board_for_street(&[dup]).unwrap_err();
    assert_eq!(err, BlindInjectError::DuplicateCard { card: dup });
    // Board still just the flop — the duplicate turn was not stored.
    assert_eq!(hand.snapshot().board.count(), 3);
}

/// Drive a heads-up blind hand to a contested showdown (both check down a
/// distinct board), ready for showdown reveals. Returns the done hand.
fn heads_up_to_contested_showdown() -> GameHand {
    let mut hand = GameHand::new_blind(
        vec![(pid(1), c(1000), 0), (pid(2), c(1000), 1)],
        0,
        c(20),
        c(10),
        DeckSeed::default(),
    );
    hand.start().unwrap();
    let board = standard_board();
    run_blind_betting(&mut hand, &board, |snap, actor| {
        let committed = snap
            .players
            .iter()
            .find(|p| p.player_id == actor)
            .map(|p| p.committed_this_street.0)
            .unwrap_or(0);
        let to_call = snap.current_bet.0.saturating_sub(committed);
        if to_call > 0 {
            PlayerAction::Call
        } else {
            PlayerAction::Check
        }
    });
    assert!(hand.is_done());
    hand
}

/// Two seats revealing the SAME card is rejected with `DuplicateCard`. The
/// first reveal is stored; the second (colliding) reveal is rejected.
#[test]
fn blind_reject_two_seats_reveal_same_card() {
    let mut hand = heads_up_to_contested_showdown();
    let shared = card(Rank::Queen, Suit::Spades);
    // pid(1) reveals Qs + Kd (distinct from the board 2c 7d 9s Jh 4c).
    let p1 = hole(shared, card(Rank::King, Suit::Diamonds));
    hand.inject_showdown_reveal(pid(1), p1).expect("reveal p1");
    // pid(2) tries to reveal the SAME Qs → DuplicateCard.
    let p2 = hole(shared, card(Rank::Ten, Suit::Diamonds));
    let err = hand.inject_showdown_reveal(pid(2), p2).unwrap_err();
    assert_eq!(err, BlindInjectError::DuplicateCard { card: shared });
    // pid(2) was NOT mutated — it stays Opaque (no partial reveal stored).
    let seat2 = hand.seats.iter().find(|s| s.player_id == pid(2)).unwrap();
    assert!(seat2.hole.is_opaque(), "rejected reveal leaves seat Opaque");
}

/// A reveal card equal to a board card is rejected with `DuplicateCard`.
#[test]
fn blind_reject_reveal_card_equals_board_card() {
    let mut hand = heads_up_to_contested_showdown();
    // Board is 2c 7d 9s Jh 4c; reveal a hole containing the board card 9s.
    let board_card = card(Rank::Nine, Suit::Spades);
    let p1 = hole(board_card, card(Rank::King, Suit::Diamonds));
    let err = hand.inject_showdown_reveal(pid(1), p1).unwrap_err();
    assert_eq!(err, BlindInjectError::DuplicateCard { card: board_card });
    let seat1 = hand.seats.iter().find(|s| s.player_id == pid(1)).unwrap();
    assert!(seat1.hole.is_opaque(), "rejected reveal leaves seat Opaque");
}

/// A reveal whose two hole cards are equal to each other is rejected.
#[test]
fn blind_reject_reveal_with_internal_duplicate() {
    let mut hand = heads_up_to_contested_showdown();
    let dup = card(Rank::Queen, Suit::Spades);
    let p1 = hole(dup, dup);
    let err = hand.inject_showdown_reveal(pid(1), p1).unwrap_err();
    assert_eq!(err, BlindInjectError::DuplicateCard { card: dup });
}

// ---- F2: finish_blind returns typed errors instead of panicking on a
//          wrong phase / wrong mode. ----

/// `finish_blind` before the hand is done returns `NotDone` (NOT a panic) so
/// the server can void the hand rather than crash the room.
#[test]
fn blind_finish_before_done_returns_not_done() {
    let mut hand = GameHand::new_blind(
        vec![(pid(1), c(1000), 0), (pid(2), c(1000), 1)],
        0,
        c(20),
        c(10),
        DeckSeed::default(),
    );
    hand.start().unwrap();
    // Still preflop — not done.
    assert!(!hand.is_done());
    let err = hand.finish_blind().unwrap_err();
    assert_eq!(err, BlindFinishError::NotDone);
}

/// `finish_blind` on a plaintext-mode hand returns `NotBlindMode` (NOT a
/// panic).
#[test]
fn blind_finish_on_plaintext_hand_returns_not_blind_mode() {
    let mut hand = GameHand::new_with_rng(
        vec![(pid(1), c(1000), 0), (pid(2), c(1000), 1)],
        0,
        c(20),
        c(10),
        PokerRng::from_seed(1),
    );
    hand.start().unwrap();
    // Fold to done so phase == Done but mode == Plaintext.
    let a = hand.snapshot().current_actor.unwrap();
    hand.apply_action(a, PlayerAction::Fold).unwrap();
    assert!(hand.is_done());
    let err = hand.finish_blind().unwrap_err();
    assert_eq!(err, BlindFinishError::NotBlindMode);
}

// ---- F3: the contested rank path uses ONLY injected `Revealed` reveals;
//          a `Plain` slot (unreachable in prod) counts as a missing reveal. ----

/// If a non-folded contender somehow holds a `Plain` slot (which blind mode
/// never writes), the contested rank path treats it as a MISSING reveal —
/// only `Revealed` feeds the ranker. This guards the "ranking uses only
/// injected `Revealed`" invariant against a stray `Plain`.
#[test]
fn blind_contested_treats_plain_slot_as_missing_reveal() {
    let mut hand = heads_up_to_contested_showdown();
    // Legitimately reveal pid(2).
    let p2 = hole(card(Rank::Ace, Suit::Spades), card(Rank::King, Suit::Clubs));
    hand.inject_showdown_reveal(pid(2), p2).expect("reveal p2");
    // Force pid(1) into a `Plain` slot (an out-of-protocol state). The
    // contested path must NOT treat this as a reveal.
    let stray = hole(
        card(Rank::Queen, Suit::Hearts),
        card(Rank::Queen, Suit::Diamonds),
    );
    let seat1 = hand
        .seats
        .iter_mut()
        .find(|s| s.player_id == pid(1))
        .unwrap();
    seat1.hole = HoleSlot::Plain(stray);
    let err = hand.finish_blind().unwrap_err();
    assert_eq!(
        err,
        BlindFinishError::MissingReveal {
            player_id: pid(1),
            seat: 0,
        }
    );
}
