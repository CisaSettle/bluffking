//! Integration tests for full hand orchestration (T5).

use engine::{
    action::PlayerAction,
    card::{Card, Rank, Suit},
    deck::Deck,
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

/// Play a hand to completion with a simple strategy: always check or call, never raise.
/// Returns `(chips_awarded_total, actions_count)`.
fn play_check_call_to_showdown(
    seed: u64,
    players: Vec<(PlayerId, Chips, u8)>,
    dealer_idx: usize,
    big_blind: Chips,
    small_blind: Chips,
) -> engine::game::HandResult {
    let total_chips: u32 = players.iter().map(|(_, c, _)| c.0).sum();

    let mut hand = GameHand::new_with_rng(
        players,
        dealer_idx,
        big_blind,
        small_blind,
        PokerRng::from_seed(seed),
    );

    hand.start().expect("start failed");

    let mut safety = 0u32;
    loop {
        if hand.is_done() {
            break;
        }
        safety += 1;
        assert!(safety < 200, "infinite loop detected");

        let snap = hand.snapshot();
        let actor = match snap.current_actor {
            Some(id) => id,
            None => break,
        };

        // Simple strategy: call if there's something to call, else check.
        let player_committed = snap
            .players
            .iter()
            .find(|p| p.player_id == actor)
            .map(|p| p.committed_this_street.0)
            .unwrap_or(0);
        let to_call = snap.current_bet.0.saturating_sub(player_committed);

        let action = if to_call > 0 {
            PlayerAction::Call
        } else {
            PlayerAction::Check
        };

        hand.apply_action(actor, action)
            .expect("apply_action failed");
    }

    let result = hand.finish();

    // Invariant: total chips awarded equals total chips put in the pot.
    let total_pot: u32 = result.pots.iter().map(|p| p.amount.0).sum();
    let total_awarded: u32 = result.chips_awarded.values().sum();
    assert_eq!(
        total_awarded, total_pot,
        "chips_awarded ({total_awarded}) must equal total pot ({total_pot})"
    );

    // Invariant: final stacks sum to starting stacks.
    let final_total: u32 = result.final_stacks.values().sum();
    assert_eq!(
        final_total, total_chips,
        "final stacks ({final_total}) must equal starting chips ({total_chips})"
    );

    result
}

/// T5 acceptance: scripted 3-player hand with fixed seed — result is deterministic.
#[test]
fn three_player_scripted_hand_deterministic() {
    let players = vec![
        (pid(1), c(1000), 0u8),
        (pid(2), c(1000), 1u8),
        (pid(3), c(1000), 2u8),
    ];

    let result = play_check_call_to_showdown(42, players.clone(), 0, c(20), c(10));

    // Deck seed recorded (widened to 256-bit since ADR-062 §2: the legacy u64
    // `42` lives in the low 8 little-endian bytes, the rest are zero).
    assert_eq!(result.deck_seed, engine::rng::widen(42));

    // Actions should include blind posts + betting actions.
    assert!(
        result.actions.len() >= 2,
        "must have at least the two blind actions"
    );

    // Run twice: same seed → same winner.
    let result2 = play_check_call_to_showdown(42, players, 0, c(20), c(10));
    let winner1: u64 = result
        .chips_awarded
        .iter()
        .max_by_key(|(_, &v)| v)
        .map(|(k, _)| *k)
        .unwrap();
    let winner2: u64 = result2
        .chips_awarded
        .iter()
        .max_by_key(|(_, &v)| v)
        .map(|(k, _)| *k)
        .unwrap();
    assert_eq!(winner1, winner2, "same seed should produce same winner");
}

/// T5 acceptance: 3-player hand where p2 has a short stack — verify side pot distribution.
#[test]
fn three_player_side_pot_short_stack() {
    // p1 and p3 have deep stacks; p2 is short.
    // Dealer=2, so SB=p1(idx0), BB=p2(idx1), UTG=p3(idx2).
    let players = vec![
        (pid(1), c(1000), 0u8),
        (pid(2), c(60), 1u8), // short stack: exactly 3 big blinds
        (pid(3), c(1000), 2u8),
    ];

    let result = play_check_call_to_showdown(200, players, 2, c(20), c(10));

    // With a short stack of 60 and BB=20, p2 might go all-in during preflop.
    // Verify there's at least one pot.
    assert!(!result.pots.is_empty());

    // Total chips must be conserved.
    let total_chips = 1000 + 60 + 1000; // 2060
    let final_total: u32 = result.final_stacks.values().sum();
    assert_eq!(final_total, total_chips);
}

/// T5 acceptance: heads-up both all-in preflop completes without panic.
#[test]
fn heads_up_both_all_in_preflop() {
    let players = vec![(pid(1), c(500), 0u8), (pid(2), c(500), 1u8)];

    let mut hand = GameHand::new_with_rng(players, 0, c(20), c(10), PokerRng::from_seed(77));
    hand.start().unwrap();

    loop {
        if hand.is_done() {
            break;
        }
        let snap = hand.snapshot();
        let actor = match snap.current_actor {
            Some(id) => id,
            None => break,
        };
        hand.apply_action(actor, PlayerAction::AllIn).unwrap();
    }

    let result = hand.finish();

    // All 1000 chips must be distributed.
    let total_awarded: u32 = result.chips_awarded.values().sum();
    let total_pot: u32 = result.pots.iter().map(|p| p.amount.0).sum();
    assert_eq!(total_awarded, total_pot);

    let final_total: u32 = result.final_stacks.values().sum();
    assert_eq!(final_total, 1000);
}

/// T5 acceptance: 3-player all-in with different stacks, verify two-pot distribution.
#[test]
fn three_player_all_in_side_pot_distribution() {
    // Short stack p2 must be eligible only for the main pot.
    let players = vec![
        (pid(1), c(300), 0u8),
        (pid(2), c(100), 1u8), // short
        (pid(3), c(300), 2u8),
    ];

    let mut hand = GameHand::new_with_rng(players, 0, c(20), c(10), PokerRng::from_seed(55));
    hand.start().unwrap();

    let mut safety = 0u32;
    loop {
        if hand.is_done() {
            break;
        }
        safety += 1;
        assert!(safety < 100, "infinite loop");
        let snap = hand.snapshot();
        let actor = match snap.current_actor {
            Some(id) => id,
            None => break,
        };
        hand.apply_action(actor, PlayerAction::AllIn).unwrap();
    }

    let result = hand.finish();

    let final_total: u32 = result.final_stacks.values().sum();
    assert_eq!(final_total, 700); // 300 + 100 + 300

    // p2 (short stack, 100 chips) cannot win more than 100*3 = 300 from the main pot.
    let p2_won = *result.chips_awarded.get(&2).unwrap_or(&0);
    // The total pot is at most 100+100+100=300 main pot (p2 eligible) + remaining side pot.
    // Even if p2 wins the main pot, they can't get more than 300.
    assert!(
        p2_won <= 300,
        "short stack cannot win more than 3x their stack from the main pot"
    );
}

/// 2026-05-28 BUG-B regression — heads-up all-in on the flop must auto-deal
/// turn + river WITHOUT prompting either player for further action.
///
/// User report: "HU, both all-in on flop. Engine asks the surviving player to
/// check/check the turn and river." Expected: server emits `street_revealed`
/// for turn + river in quick succession and reaches `HandFinished` without
/// surfacing an `await_action` for either remaining street.
///
/// Implementation contract: after the flop betting round closes with both
/// non-folded players all-in (or only one non-folded all-in plus a covering
/// player who is matched), `close_street_and_advance` must keep recursing
/// until `phase == Done`, producing a `StreetRevealed` event per fresh street
/// but never a `current_actor` waiting for input.
#[test]
fn hu_all_in_on_flop_auto_runs_out_turn_and_river() {
    use engine::card::Card;
    use engine::event::EngineEvent;
    use engine::hand::Street;

    // Equal stacks, deterministic seed.
    let players = vec![(pid(1), c(1000), 0u8), (pid(2), c(1000), 1u8)];
    let mut hand = GameHand::new_with_rng(players, 0, c(20), c(10), PokerRng::from_seed(20260528));
    hand.start().expect("start ok");

    // Preflop: limp + check to see a flop.
    // dealer=0 / SB=pid(1) / BB=pid(2). HU SB acts first preflop.
    let snap = hand.snapshot();
    let first_actor = snap.current_actor.expect("preflop actor exists");
    hand.apply_action(first_actor, PlayerAction::Call)
        .expect("SB call");
    let snap = hand.snapshot();
    let second_actor = snap.current_actor.expect("BB option exists");
    hand.apply_action(second_actor, PlayerAction::Check)
        .expect("BB check");

    // We are now on the flop. Drain events so the test below only inspects
    // events emitted by the all-in shove + call sequence.
    let snap = hand.snapshot();
    assert_eq!(snap.street, Street::Flop, "must be on the flop");
    let _flush = hand.drain_events();

    // Flop: both players shove all-in.
    let snap = hand.snapshot();
    let aggressor = snap.current_actor.expect("flop actor exists");
    hand.apply_action(aggressor, PlayerAction::AllIn)
        .expect("flop shove");
    // After the shove, snap might show the next actor; they call all-in.
    let snap = hand.snapshot();
    let caller = snap.current_actor.expect("caller exists");
    assert_ne!(caller, aggressor, "second actor differs from shover");
    hand.apply_action(caller, PlayerAction::Call)
        .expect("call all-in");

    // Engine MUST have auto-run the hand to completion.
    assert!(
        hand.is_done(),
        "hand must finish without further input after HU all-in on flop"
    );
    let snap = hand.snapshot();
    assert!(
        snap.current_actor.is_none(),
        "no actor pending after auto-runout"
    );

    // The single drain_events call below should contain street_revealed for
    // both Turn and River — proving the engine fast-forwarded.
    let events = hand.drain_events();
    let mut saw_turn = false;
    let mut saw_river = false;
    let mut board: Vec<Card> = Vec::new();
    for ev in &events {
        if let EngineEvent::StreetRevealed {
            street, new_cards, ..
        } = ev
        {
            match street {
                Street::Turn => {
                    saw_turn = true;
                    assert_eq!(new_cards.len(), 1, "turn reveals exactly 1 card");
                    board.extend_from_slice(new_cards);
                }
                Street::River => {
                    saw_river = true;
                    assert_eq!(new_cards.len(), 1, "river reveals exactly 1 card");
                    board.extend_from_slice(new_cards);
                }
                _ => {}
            }
        }
    }
    assert!(saw_turn, "turn must be auto-dealt during HU all-in runout");
    assert!(
        saw_river,
        "river must be auto-dealt during HU all-in runout"
    );

    // The full board must now have 5 cards (3 flop + 1 turn + 1 river).
    // We already drained the flop earlier, so we expect 2 here.
    assert_eq!(board.len(), 2, "turn + river dealt during runout");

    // No duplicates — sanity check the deck didn't double-deal.
    let mut sorted = board.clone();
    sorted.sort_by_key(|c| ((c.rank as u8) << 4) | (c.suit as u8));
    sorted.dedup();
    assert_eq!(sorted.len(), board.len(), "no duplicate cards in runout");

    // The finished hand must include both non-folded players at showdown,
    // i.e. revealed_hole_cards (server-side) populates 2 entries.
    let result = hand.finish();
    assert_eq!(
        result.showdown.len(),
        2,
        "both HU players present at showdown after all-in"
    );

    // 2026-05-28 BUG-D — show_order is populated for HU all-in.
    // No river action means the "first non-folded clockwise from button"
    // path runs: HU dealer=0 (button is pid 1's seat), so clockwise from
    // dealer+1 means BB seat = pid 2 → pid 1.
    assert_eq!(
        result.show_order.len(),
        2,
        "HU showdown show_order lists both seats"
    );
}

/// 2026-05-28 BUG-D regression — show_order computation for the four canonical
/// scenarios. Each scenario builds a hand from a deterministic seed, plays a
/// scripted action sequence, and asserts the order matches WSOP / TDA
/// convention.
#[test]
fn show_order_river_aggressor_then_turn_order() {
    // 3-player. River sees a bet from seat 2 followed by a call from seat 1.
    // Expected: show_order = [seat_2, seat_3 or seat_1, ...] starting with
    // the last river aggressor (seat 2), then turn order.
    //
    // Seat layout: dealer=0, so SB=1 (pid 2), BB=2 (pid 3), UTG=0 (pid 1).
    // We'll have pid 1 fold preflop so only pid 2 + pid 3 see the river.
    // River aggressor: pid 2 raises, pid 3 calls. show_order = [pid 2, pid 3].
    let players = vec![
        (pid(1), c(1000), 0u8),
        (pid(2), c(1000), 1u8),
        (pid(3), c(1000), 2u8),
    ];
    let mut hand =
        GameHand::new_with_rng(players, 0, c(20), c(10), PokerRng::from_seed(2026052801));
    hand.start().expect("start ok");

    // UTG (pid 1) folds preflop.
    let snap = hand.snapshot();
    let utg = snap.current_actor.expect("utg exists");
    assert_eq!(utg, pid(1), "UTG is pid 1 in seat 0");
    hand.apply_action(utg, PlayerAction::Fold).expect("fold");

    // SB (pid 2) completes, BB (pid 3) checks → flop.
    let actor = hand.snapshot().current_actor.expect("sb actor");
    hand.apply_action(actor, PlayerAction::Call)
        .expect("sb call");
    let actor = hand.snapshot().current_actor.expect("bb actor");
    hand.apply_action(actor, PlayerAction::Check)
        .expect("bb check");

    // Flop, turn — both check through.
    for _ in 0..4 {
        let snap = hand.snapshot();
        if hand.is_done() {
            break;
        }
        let actor = match snap.current_actor {
            Some(a) => a,
            None => break,
        };
        let player_committed = snap
            .players
            .iter()
            .find(|p| p.player_id == actor)
            .map(|p| p.committed_this_street.0)
            .unwrap_or(0);
        let to_call = snap.current_bet.0.saturating_sub(player_committed);
        if to_call > 0 {
            hand.apply_action(actor, PlayerAction::Call).expect("call");
        } else {
            hand.apply_action(actor, PlayerAction::Check)
                .expect("check");
        }
        if hand.snapshot().street == engine::hand::Street::River {
            break;
        }
    }

    // We should be on the river now.
    let snap = hand.snapshot();
    assert_eq!(snap.street, engine::hand::Street::River, "must be on river");

    // River: pid 2 bets 100, pid 3 calls.
    let actor = snap.current_actor.expect("river actor");
    hand.apply_action(actor, PlayerAction::Raise { amount: c(100) })
        .expect("river bet");
    let actor = hand.snapshot().current_actor.expect("river caller");
    hand.apply_action(actor, PlayerAction::Call)
        .expect("river call");

    assert!(hand.is_done(), "hand finished after river call");
    let result = hand.finish();

    // pid 1 folded so they are NOT in show_order.
    assert!(
        !result.show_order.contains(&pid(1)),
        "folded player must not appear in show_order"
    );
    // Two players in show_order (pid 2 + pid 3).
    assert_eq!(result.show_order.len(), 2, "two non-folded players");
    // pid 2 is the last river aggressor → shows first.
    assert_eq!(
        result.show_order[0],
        pid(2),
        "last river aggressor shows first"
    );
    // pid 3 shows second.
    assert_eq!(
        result.show_order[1],
        pid(3),
        "remaining player shows second"
    );
}

/// 2026-05-28 BUG-D — when the river checks through, show_order starts from
/// the first non-folded player clockwise from the button.
#[test]
fn show_order_river_checked_through_starts_from_left_of_button() {
    // 2 players, neither folded. Dealer=0 so HU button is pid 1.
    // Clockwise from button: pid 2 first (BB seat acts first post-flop in HU
    // and is also the first non-folded seat clockwise from the dealer when
    // the river checks through).
    let players = vec![(pid(1), c(1000), 0u8), (pid(2), c(1000), 1u8)];
    let mut hand =
        GameHand::new_with_rng(players, 0, c(20), c(10), PokerRng::from_seed(2026052802));
    hand.start().expect("start ok");

    // Run through preflop check-call to flop.
    let actor = hand.snapshot().current_actor.expect("preflop actor");
    hand.apply_action(actor, PlayerAction::Call).expect("call");
    let actor = hand.snapshot().current_actor.expect("bb option");
    hand.apply_action(actor, PlayerAction::Check)
        .expect("check");

    // Flop / turn / river all check through.
    for _ in 0..6 {
        if hand.is_done() {
            break;
        }
        let actor = match hand.snapshot().current_actor {
            Some(a) => a,
            None => break,
        };
        hand.apply_action(actor, PlayerAction::Check)
            .expect("check");
    }

    assert!(hand.is_done(), "hand finished after river check-check");
    let result = hand.finish();

    assert_eq!(result.show_order.len(), 2, "both players show");
    // Clockwise from dealer (pid 1 = seat 0) starts at seat 1 = pid 2.
    assert_eq!(
        result.show_order[0],
        pid(2),
        "first non-folded clockwise from button shows first"
    );
    assert_eq!(result.show_order[1], pid(1));
}

/// 2026-05-28 BUG-D — fold-around: show_order is empty when only one player
/// remains (no showdown).
#[test]
fn show_order_empty_on_fold_around() {
    let players = vec![(pid(1), c(1000), 0u8), (pid(2), c(1000), 1u8)];
    let mut hand =
        GameHand::new_with_rng(players, 0, c(20), c(10), PokerRng::from_seed(2026052803));
    hand.start().expect("start ok");

    // SB acts first preflop in HU; SB folds → fold-around.
    let actor = hand.snapshot().current_actor.expect("preflop actor");
    hand.apply_action(actor, PlayerAction::Fold).expect("fold");

    assert!(hand.is_done(), "fold-around finishes immediately");
    let result = hand.finish();
    assert!(
        result.show_order.is_empty(),
        "fold-around: show_order must be empty"
    );
}

// ===========================================================================
// QA coverage (Opus round 1) — poker edge cases that the deterministic harness
// (`cargo test -p engine`, always-on, no DB) did not previously exercise:
//   • split pot — exact tie + ODD-chip remainder
//   • all-in LADDER — 4 handed, 3 distinct sub-stacks → main + two side pots
//   • min-raise — a sub-minimum all-in does NOT reopen betting (TDA Rule 6),
//     driven through the real `GameHand::apply_action` (the rule was unit-tested
//     at the `BettingRound` level only, never end-to-end through a full hand).
//
// All three RIG the deck via `Deck::from_cards` so the showdown is fully
// deterministic — no seed-search, no flakiness. Deck consumption order
// (engine/src/game.rs): hole cards are dealt in two passes over the seats
// (seat0.c1, seat1.c1, …, then seat0.c2, seat1.c2, …), then the board is dealt
// with NO burns — flop = next 3, turn = next 1, river = next 1.
// ===========================================================================

/// Build a full, valid 52-card deck whose FIRST `prefix.len()` cards are exactly
/// `prefix` (the cards we want dealt first), with the remaining 52−N cards
/// appended in canonical order, de-duplicated against the prefix. `Deck::deal`
/// hands out `cards[0]` first, so `prefix` controls the hole cards + board.
fn rigged_deck(prefix: &[Card]) -> Deck {
    use std::collections::HashSet;
    let used: HashSet<Card> = prefix.iter().copied().collect();
    let mut cards: Vec<Card> = prefix.to_vec();
    for &rank in &Rank::ALL {
        for &suit in &Suit::ALL {
            let card = Card::new(rank, suit);
            if !used.contains(&card) {
                cards.push(card);
            }
        }
    }
    assert_eq!(cards.len(), 52, "rigged deck must be a full 52-card deck");
    Deck::from_cards(cards)
}

/// SPLIT POT — exact tie where both players play the board.
///
/// Heads-up, both players all-in preflop for an EQUAL pot, and the board makes
/// the hand for BOTH of them (quad aces + a K kicker on the board; neither
/// player's two hole cards can beat a board K kicker, so they play the board).
/// Equal heads-up all-ins necessarily make an even pot (2 × 1505 = 3010), so
/// this checks exact equal division. Odd-chip placement has a dedicated
/// three-handed unit test in `game.rs`.
///
/// Catches: a tie that mis-awards the whole pot to one seat (PartialEq winner
/// pick instead of an equal-rank split), or a split that drops a chip.
#[test]
fn split_pot_exact_tie_board_plays() {
    let stack = 1505u32;
    let players = vec![(pid(1), c(stack), 0u8), (pid(2), c(stack), 1u8)];

    // Hole cards: four LOW off-board cards (cannot improve past the board's K
    // kicker). Board (cards[4..9]): A♠ A♥ A♦ A♣ K♠ → both play quad aces + K.
    // Deal order for 2 seats: [p1.c1, p2.c1, p1.c2, p2.c2, flop×3, turn, river].
    let prefix = [
        Card::new(Rank::Two, Suit::Clubs),      // p1 hole 1
        Card::new(Rank::Three, Suit::Diamonds), // p2 hole 1
        Card::new(Rank::Four, Suit::Clubs),     // p1 hole 2
        Card::new(Rank::Five, Suit::Diamonds),  // p2 hole 2
        Card::new(Rank::Ace, Suit::Spades),     // flop
        Card::new(Rank::Ace, Suit::Hearts),
        Card::new(Rank::Ace, Suit::Diamonds),
        Card::new(Rank::Ace, Suit::Clubs),   // turn
        Card::new(Rank::King, Suit::Spades), // river
    ];

    let mut hand =
        GameHand::new_with_deck(players, 0, c(20), c(10), rigged_deck(&prefix), [0u8; 32]);
    hand.start().expect("start ok");

    // Drive both players all-in to the showdown.
    let mut safety = 0u32;
    while !hand.is_done() {
        safety += 1;
        assert!(safety < 100, "infinite loop");
        let Some(actor) = hand.snapshot().current_actor else {
            break;
        };
        // Shove if we can; otherwise call (the second player just calls the shove).
        if hand.apply_action(actor, PlayerAction::AllIn).is_err() {
            hand.apply_action(actor, PlayerAction::Call)
                .expect("call shove");
        }
    }

    let result = hand.finish();

    // Chip conservation: nothing minted or lost across the split.
    let final_total: u32 = result.final_stacks.values().sum();
    assert_eq!(final_total, stack * 2, "split pot must conserve chips");

    // BOTH seats must receive a share (it's a tie — not a single winner).
    let p1 = *result.chips_awarded.get(&1).unwrap_or(&0);
    let p2 = *result.chips_awarded.get(&2).unwrap_or(&0);
    assert!(
        p1 > 0 && p2 > 0,
        "exact tie: both seats must win a share (got p1={p1}, p2={p2})"
    );

    // Even pot of 3010 must split 1505/1505 and sum to the whole pot.
    let pot_total: u32 = result.pots.iter().map(|p| p.amount.0).sum();
    assert_eq!(
        p1 + p2,
        pot_total,
        "the two tied shares must sum to the whole pot"
    );
    let diff = p1.abs_diff(p2);
    assert_eq!(diff, 0, "even tied pot must split exactly equally");

    // The showdown must actually be a tie at the rank level (defensive: ensures
    // the board-plays setup held and we're not accidentally testing a fold).
    assert_eq!(result.show_order.len(), 2, "both players reach showdown");
}

/// A no-all-in hand has one canonical contestable pot across all streets.
/// Splitting street fragments independently can award the same odd chip more
/// than once and turn an exact 12/12 chop into 11/13.
#[test]
fn no_all_in_multistreet_tie_uses_one_canonical_pot() {
    let players = vec![
        (pid(1), c(100), 0u8),
        (pid(2), c(100), 1u8),
        (pid(3), c(100), 2u8),
    ];
    let prefix = [
        Card::new(Rank::Ten, Suit::Clubs),
        Card::new(Rank::Nine, Suit::Clubs),
        Card::new(Rank::Ten, Suit::Diamonds),
        Card::new(Rank::Three, Suit::Clubs),
        Card::new(Rank::Eight, Suit::Clubs),
        Card::new(Rank::Four, Suit::Clubs),
        Card::new(Rank::Ace, Suit::Spades),
        Card::new(Rank::King, Suit::Spades),
        Card::new(Rank::Queen, Suit::Hearts),
        Card::new(Rank::Jack, Suit::Hearts),
        Card::new(Rank::Two, Suit::Diamonds),
    ];
    let mut hand = GameHand::new_with_deck(players, 0, c(2), c(1), rigged_deck(&prefix), [7u8; 32]);
    hand.start().expect("start ok");

    // Preflop: all three match the big blind (pot 6).
    hand.apply_action(pid(1), PlayerAction::Call).unwrap();
    hand.apply_action(pid(2), PlayerAction::Call).unwrap();
    hand.apply_action(pid(3), PlayerAction::Check).unwrap();

    // Flop and turn: three chips each (two street fragments of 9).
    for _ in 0..2 {
        hand.apply_action(pid(2), PlayerAction::Raise { amount: c(3) })
            .unwrap();
        hand.apply_action(pid(3), PlayerAction::Call).unwrap();
        hand.apply_action(pid(1), PlayerAction::Call).unwrap();
    }

    // River checks through.
    hand.apply_action(pid(2), PlayerAction::Check).unwrap();
    hand.apply_action(pid(3), PlayerAction::Check).unwrap();
    hand.apply_action(pid(1), PlayerAction::Check).unwrap();
    assert!(hand.is_done());

    let result = hand.finish();
    let contestable: Vec<_> = result.pots.iter().filter(|pot| !pot.is_refund).collect();
    assert_eq!(
        contestable.len(),
        1,
        "no all-in hand must have one canonical pot"
    );
    assert_eq!(contestable[0].amount, c(24));
    assert_eq!(result.chips_awarded.get(&1), Some(&12));
    assert_eq!(result.chips_awarded.get(&3), Some(&12));
    assert_eq!(result.final_stacks.get(&1), Some(&104));
    assert_eq!(result.final_stacks.get(&3), Some(&104));
}

/// Folded players' unequal dead-money contributions do not create separate
/// pots when the surviving players have the same entitlement to every layer.
#[test]
fn folded_dead_money_layers_merge_before_odd_chip_split() {
    let players = (1u64..=5)
        .map(|n| (pid(n), c(100), (n - 1) as u8))
        .collect();
    let prefix = [
        Card::new(Rank::Two, Suit::Clubs),
        Card::new(Rank::Three, Suit::Clubs),
        Card::new(Rank::Four, Suit::Clubs),
        Card::new(Rank::Five, Suit::Clubs),
        Card::new(Rank::Six, Suit::Clubs),
        Card::new(Rank::Two, Suit::Diamonds),
        Card::new(Rank::Three, Suit::Diamonds),
        Card::new(Rank::Four, Suit::Diamonds),
        Card::new(Rank::Five, Suit::Diamonds),
        Card::new(Rank::Six, Suit::Diamonds),
        Card::new(Rank::Ace, Suit::Spades),
        Card::new(Rank::King, Suit::Spades),
        Card::new(Rank::Queen, Suit::Spades),
        Card::new(Rank::Jack, Suit::Spades),
        Card::new(Rank::Ten, Suit::Spades),
    ];
    let mut hand = GameHand::new_with_deck(players, 0, c(2), c(1), rigged_deck(&prefix), [8u8; 32]);
    hand.start().unwrap();

    hand.apply_action(pid(4), PlayerAction::Raise { amount: c(5) })
        .unwrap();
    hand.apply_action(pid(5), PlayerAction::Raise { amount: c(8) })
        .unwrap();
    hand.apply_action(pid(1), PlayerAction::Call).unwrap();
    hand.apply_action(pid(2), PlayerAction::Fold).unwrap();
    hand.apply_action(pid(3), PlayerAction::Fold).unwrap();
    hand.apply_action(pid(4), PlayerAction::Fold).unwrap();

    while !hand.is_done() {
        let actor = hand.snapshot().current_actor.expect("two live players act");
        hand.apply_action(actor, PlayerAction::Check).unwrap();
    }
    let snapshot = hand.snapshot();
    assert_eq!(snapshot.side_pots.len(), 1);
    assert_eq!(snapshot.side_pots[0].amount, c(24));
    assert_eq!(snapshot.side_pots[0].cap, c(8));
    assert_eq!(snapshot.side_pots[0].eligible, vec![pid(1), pid(5)]);
    let result = hand.finish();
    let contestable: Vec<_> = result.pots.iter().filter(|pot| !pot.is_refund).collect();
    assert_eq!(
        contestable.len(),
        1,
        "dead money must not fragment one entitlement"
    );
    assert_eq!(contestable[0].amount, c(24));
    assert_eq!(result.chips_awarded.get(&1), Some(&12));
    assert_eq!(result.chips_awarded.get(&5), Some(&12));
    assert_eq!(result.final_stacks.get(&1), Some(&104));
    assert_eq!(result.final_stacks.get(&5), Some(&104));
}

/// Contestable chips and a single-player uncalled overage may have the same
/// eligible seat, but `refund_to` makes them semantically different layers.
#[test]
fn fold_around_keeps_contested_pot_separate_from_uncalled_refund() {
    let players = vec![(pid(1), c(100), 0u8), (pid(2), c(100), 1u8)];
    let mut hand = GameHand::new_with_rng(players, 0, c(2), c(1), PokerRng::from_seed(11));
    hand.start().unwrap();
    hand.apply_action(pid(1), PlayerAction::Raise { amount: c(8) })
        .unwrap();
    hand.apply_action(pid(2), PlayerAction::Fold).unwrap();
    assert!(hand.is_done());

    let snapshot = hand.snapshot();
    assert_eq!(snapshot.side_pots.len(), 2);
    assert_eq!(snapshot.side_pots[0].amount, c(4));
    assert_eq!(snapshot.side_pots[0].cap, c(2));
    assert_eq!(snapshot.side_pots[0].eligible, vec![pid(1)]);
    assert!(snapshot.side_pots[0].refund_to.is_empty());
    assert_eq!(snapshot.side_pots[1].amount, c(6));
    assert_eq!(snapshot.side_pots[1].cap, c(8));
    assert_eq!(snapshot.side_pots[1].eligible, vec![pid(1)]);
    assert_eq!(snapshot.side_pots[1].refund_to, vec![pid(1)]);

    let result = hand.finish();
    assert_eq!(result.pots.len(), 2);
    assert!(!result.pots[0].is_refund);
    assert!(result.pots[1].is_refund);
    assert_eq!(result.pots[0].amount, c(4));
    assert_eq!(result.pots[1].amount, c(6));
    assert_eq!(result.final_stacks.get(&1), Some(&102));
    assert_eq!(result.final_stacks.get(&2), Some(&98));
}

/// The inclusive public chip-domain boundary is legal: a table totaling
/// exactly u32::MAX must settle without intermediate overflow.
#[test]
fn table_total_exactly_u32_max_settles_canonical_pot() {
    let deep = 1_717_986_916u32;
    let players = vec![
        (pid(1), c(deep), 0u8),
        (pid(2), c(2), 1u8),
        (pid(3), c(3), 2u8),
        (pid(4), c(858_993_458), 3u8),
        (pid(5), c(deep), 4u8),
    ];
    assert_eq!(
        players
            .iter()
            .map(|(_, stack, _)| u64::from(stack.0))
            .sum::<u64>(),
        u64::from(u32::MAX)
    );
    let prefix = [
        Card::new(Rank::Two, Suit::Clubs),
        Card::new(Rank::Three, Suit::Clubs),
        Card::new(Rank::Four, Suit::Clubs),
        Card::new(Rank::Five, Suit::Clubs),
        Card::new(Rank::Six, Suit::Clubs),
        Card::new(Rank::Two, Suit::Diamonds),
        Card::new(Rank::Three, Suit::Diamonds),
        Card::new(Rank::Four, Suit::Diamonds),
        Card::new(Rank::Five, Suit::Diamonds),
        Card::new(Rank::Six, Suit::Diamonds),
        Card::new(Rank::Ace, Suit::Spades),
        Card::new(Rank::King, Suit::Spades),
        Card::new(Rank::Queen, Suit::Spades),
        Card::new(Rank::Jack, Suit::Spades),
        Card::new(Rank::Ten, Suit::Spades),
    ];
    let mut hand = GameHand::new_with_deck(players, 0, c(2), c(1), rigged_deck(&prefix), [9u8; 32]);
    hand.start().unwrap();
    hand.apply_action(
        pid(4),
        PlayerAction::Raise {
            amount: c(858_993_457),
        },
    )
    .unwrap();
    hand.apply_action(pid(5), PlayerAction::Raise { amount: c(deep) })
        .unwrap();
    hand.apply_action(pid(1), PlayerAction::Call).unwrap();
    hand.apply_action(pid(2), PlayerAction::Fold).unwrap();
    hand.apply_action(pid(3), PlayerAction::Fold).unwrap();
    hand.apply_action(pid(4), PlayerAction::Fold).unwrap();
    assert!(hand.is_done());

    let snapshot = hand.snapshot();
    assert_eq!(snapshot.side_pots.len(), 1);
    assert_eq!(snapshot.side_pots[0].cap, c(deep));
    assert_eq!(snapshot.side_pots[0].amount, c(u32::MAX - 3));
    assert_eq!(snapshot.side_pots[0].eligible, vec![pid(1), pid(5)]);
    let result = hand.finish();
    assert_eq!(result.chips_awarded.get(&1), Some(&2_147_483_646));
    assert_eq!(result.chips_awarded.get(&5), Some(&2_147_483_646));
    assert_eq!(
        result
            .final_stacks
            .values()
            .map(|stack| u64::from(*stack))
            .sum::<u64>(),
        u64::from(u32::MAX)
    );
    for folded in [2u64, 3, 4] {
        assert_eq!(result.final_stacks.get(&folded), Some(&1));
    }
}

/// ALL-IN LADDER — 4 handed, three DISTINCT all-in stack sizes.
///
/// Four players shove preflop with stacks 100 / 200 / 300 / 400. That produces
/// a MAIN pot (all four eligible, capped at 100 each) plus TWO side pots:
///   • side pot 1: the 100-over-100 layer (p2,p3,p4 eligible)
///   • side pot 2: the 100-over-200 layer (p3,p4 eligible)
/// The shortest stack (p1, 100) can be eligible for the main pot ONLY.
///
/// Catches: a side-pot calculator that lets a 0-contribution seat into a higher
/// layer, or that mis-caps a pot — both silently mis-pay an all-in showdown.
/// The engine's prop test only exercises 3 handed (≤ 2 pots); this is the first
/// 4-handed, 3-distinct-stack ladder in the deterministic suite.
#[test]
fn all_in_ladder_four_handed_three_distinct_stacks_main_plus_two_side_pots() {
    // Dealer = p1 (idx 0). 4 handed: SB=p2, BB=p3, UTG=p4, dealer p1 acts last.
    let players = vec![
        (pid(1), c(100), 0u8), // shortest — main pot only
        (pid(2), c(200), 1u8),
        (pid(3), c(300), 2u8),
        (pid(4), c(400), 3u8), // deepest — covers everyone
    ];
    let total_start = 100 + 200 + 300 + 400;

    // Deterministic deck — the exact board doesn't matter for the pot-STRUCTURE
    // invariants (eligibility + caps + conservation), only that the hand runs
    // out to a showdown, which an all-four-all-in preflop hand always does.
    let mut hand = GameHand::new_with_rng(players, 0, c(20), c(10), PokerRng::from_seed(909));
    hand.start().expect("start ok");

    let mut safety = 0u32;
    while !hand.is_done() {
        safety += 1;
        assert!(safety < 100, "infinite loop");
        let Some(actor) = hand.snapshot().current_actor else {
            break;
        };
        if hand.apply_action(actor, PlayerAction::AllIn).is_err() {
            let _ = hand.apply_action(actor, PlayerAction::Call);
        }
    }
    assert!(hand.is_done(), "all four all-in preflop must conclude");
    let snapshot = hand.snapshot();
    assert_eq!(snapshot.side_pots.len(), 4);
    assert_eq!(snapshot.side_pots[0].cap, c(100));
    assert_eq!(snapshot.side_pots[0].amount, c(400));
    assert_eq!(
        snapshot.side_pots[0].eligible,
        vec![pid(1), pid(2), pid(3), pid(4)]
    );
    assert!(snapshot.side_pots[0].refund_to.is_empty());
    assert_eq!(snapshot.side_pots[1].cap, c(200));
    assert_eq!(snapshot.side_pots[1].amount, c(300));
    assert_eq!(snapshot.side_pots[1].eligible, vec![pid(2), pid(3), pid(4)]);
    assert!(snapshot.side_pots[1].refund_to.is_empty());
    assert_eq!(snapshot.side_pots[2].cap, c(300));
    assert_eq!(snapshot.side_pots[2].amount, c(200));
    assert_eq!(snapshot.side_pots[2].eligible, vec![pid(3), pid(4)]);
    assert!(snapshot.side_pots[2].refund_to.is_empty());
    assert_eq!(snapshot.side_pots[3].cap, c(400));
    assert_eq!(snapshot.side_pots[3].amount, c(100));
    assert_eq!(snapshot.side_pots[3].eligible, vec![pid(4)]);
    assert_eq!(snapshot.side_pots[3].refund_to, vec![pid(4)]);
    let result = hand.finish();

    // Chip conservation across the whole ladder.
    let final_total: u32 = result.final_stacks.values().sum();
    assert_eq!(
        final_total, total_start,
        "ladder run-out must conserve chips"
    );

    let non_empty: Vec<&engine::game::PotResult> =
        result.pots.iter().filter(|p| p.amount.0 > 0).collect();
    assert_eq!(
        non_empty.len(),
        4,
        "100/200/300/400 all-in ladder must build main + two side pots + deepest overage refund; got {:?}",
        result.pots
    );

    let mut layers: Vec<(u32, Vec<u64>)> = non_empty
        .iter()
        .map(|p| {
            let mut ids: Vec<u64> = p.eligible_player_ids.iter().map(|id| id.inner()).collect();
            ids.sort_unstable();
            (p.amount.0, ids)
        })
        .collect();
    layers.sort_by(|a, b| a.1.len().cmp(&b.1.len()).reverse().then(a.0.cmp(&b.0)));
    assert_eq!(
        layers,
        vec![
            (400, vec![1, 2, 3, 4]),
            (300, vec![2, 3, 4]),
            (200, vec![3, 4]),
            (100, vec![4]),
        ],
        "exact side-pot ladder caps/eligibility must match 100/200/300/400 contributions",
    );
    let deepest_overage = non_empty
        .iter()
        .find(|p| p.amount.0 == 100 && p.eligible_player_ids == vec![pid(4)])
        .expect("deepest unmatched layer must exist");
    assert!(
        deepest_overage.is_refund,
        "single-player unmatched all-in overage must be marked as a refund, not a pot win"
    );

    // The shortest stack (p1, 100) is eligible for the MAIN pot ONLY — it must
    // NOT appear in any pot whose eligible set is smaller than the main pot's.
    for pot in result.pots.iter().filter(|p| p.amount.0 > 0) {
        if pot.eligible_player_ids.len() < 4 {
            assert!(
                !pot.eligible_player_ids.contains(&pid(1)),
                "shortest all-in (p1=100) must be excluded from side pot {:?}",
                pot
            );
        }
        // Per-pot invariant: every winner is drawn from the eligible set.
        for w in &pot.winners {
            assert!(
                pot.eligible_player_ids.contains(&w.player_id),
                "pot {:?} winner {:?} not in eligible set",
                pot.index,
                w.player_id
            );
        }
    }

    // p1 can never win more than the main pot it was capped into. With four
    // equal 100-contributions to the main pot, that ceiling is 400.
    let p1_won = *result.chips_awarded.get(&1).unwrap_or(&0);
    assert!(
        p1_won <= 400,
        "shortest stack cannot win beyond the 4×100 main pot; won {p1_won}"
    );
}

/// MIN-RAISE — a sub-minimum all-in does NOT reopen the betting (TDA Rule 6),
/// driven END-TO-END through `GameHand::apply_action`.
///
/// The rule is unit-tested at the `BettingRound` level (engine/src/round.rs),
/// but never through a full `GameHand`, where the wiring between the round's
/// `has_acted` bookkeeping and the public `apply_action` API is what actually
/// ships. Here: UTG raises to a full size, a short stack shoves all-in for LESS
/// than a full re-raise increment, and the ORIGINAL raiser — who already acted —
/// must then be allowed to Call or Fold but NOT to Raise again.
#[test]
fn min_raise_sub_minimum_all_in_does_not_reopen_betting_end_to_end() {
    use engine::action::ActionError;

    // Dealer = p1 (idx 0). 3 handed: SB=p2, BB=p3, UTG=p1 (dealer acts first
    // preflop only heads-up; 3-handed UTG = seat after BB = p1). Stacks chosen so
    // p3 can only shove a sub-minimum re-raise over p1's raise.
    //   BB = 20. p1 (UTG) raises to 60 (a full raise: increment 40 over the 20 BB).
    //   A full RE-raise would need to make it 100 (another +40). p3 has only 80
    //   total, so its all-in to 80 is +20 over 60 — LESS than the +40 needed to
    //   reopen. p1 already acted, so facing the non-reopening shove p1 may only
    //   call/fold.
    let players = vec![
        (pid(1), c(1000), 0u8), // UTG / dealer — makes the first full raise
        (pid(2), c(1000), 1u8), // SB
        (pid(3), c(80), 2u8),   // BB-position short stack — sub-min shove
    ];

    let mut hand = GameHand::new_with_rng(players, 0, c(20), c(10), PokerRng::from_seed(4242));
    hand.start().expect("start ok");

    // p1 (UTG) acts first preflop in a 3-handed hand. Raise to a full 60.
    let a1 = hand.snapshot().current_actor.expect("preflop actor");
    assert_eq!(a1, pid(1), "3-handed UTG acts first preflop");
    hand.apply_action(a1, PlayerAction::Raise { amount: c(60) })
        .expect("p1 full raise to 60");

    // Next actor (p2, SB) folds out of the way so action returns to the short BB.
    let a2 = hand.snapshot().current_actor.expect("actor after p1 raise");
    hand.apply_action(a2, PlayerAction::Fold).expect("p2 fold");

    // p3 (short BB) shoves all-in for 80 — only +20 over 60, a SUB-MINIMUM raise
    // that must NOT reopen the betting for the already-acted p1.
    let a3 = hand.snapshot().current_actor.expect("short stack actor");
    assert_eq!(a3, pid(3), "action is on the short BB");
    hand.apply_action(a3, PlayerAction::AllIn)
        .expect("p3 sub-minimum all-in shove");

    // Action is back on p1. The min-raise rule (TDA Rule 6) says p1, having
    // already acted, may ONLY call or fold — raising must be closed.
    let back = hand.snapshot().current_actor;
    assert_eq!(
        back,
        Some(pid(1)),
        "after a non-reopening shove, action returns to the already-acted raiser"
    );
    {
        let snap = hand.snapshot();
        // The engine's POSITIVE signal that re-raising is closed: `min_raise_to`
        // is None (there is no legal raise-to amount to offer the UI).
        assert!(
            snap.min_raise_to.is_none(),
            "non-reopening all-in must close raising: min_raise_to should be None, got {:?}",
            snap.min_raise_to
        );
        // And the engine must REJECT a re-raise attempt — whether it reports the
        // refusal as BelowMinRaise or InvalidAction (raising categorically
        // unavailable), the invariant is that it does NOT succeed, even though
        // p1 has plenty of chips for a raise.
        let reraise = hand.apply_action(pid(1), PlayerAction::Raise { amount: c(120) });
        assert!(
            matches!(
                reraise,
                Err(ActionError::BelowMinRaise) | Err(ActionError::InvalidAction)
            ),
            "non-reopening sub-min all-in must NOT let the acted player re-raise; got {reraise:?}"
        );
        // But Call must still be allowed — p1 is not frozen out, just capped.
        hand.apply_action(pid(1), PlayerAction::Call)
            .expect("p1 may still CALL the non-reopening shove");
    }

    // Sanity: the hand still completes and conserves chips.
    let mut safety = 0u32;
    while !hand.is_done() {
        safety += 1;
        assert!(safety < 100, "infinite loop");
        let Some(actor) = hand.snapshot().current_actor else {
            break;
        };
        if hand.apply_action(actor, PlayerAction::Call).is_err() {
            let _ = hand.apply_action(actor, PlayerAction::Check);
        }
    }
    let result = hand.finish();
    let final_total: u32 = result.final_stacks.values().sum();
    assert_eq!(
        final_total,
        1000 + 1000 + 80,
        "chips conserved after non-reopening shove"
    );
}

/// QA coverage (Opus round 1) — SHORT / "DEAD" BLIND: a seat that cannot cover
/// the big blind posts a PARTIAL, all-in blind (engine clamps the posted blind
/// to the seat's stack, game.rs `bb_blind_amount = big_blind.min(bb_stack)`).
///
/// GAP: the deterministic suite covers side pots, the all-in ladder, the
/// odd-chip split, and the min-raise rule — but every one of those starts every
/// seat with a stack ≥ the blind. None exercise the blind-POSTING edge where a
/// seat is shorter than the blind it owes. This is the live "dead big blind"
/// case (a player rebought tiny, or a stack was whittled to < 1 BB): the engine
/// must post only what the seat has (a partial all-in blind), build the pot from
/// the clamped amount, and still conserve chips at showdown. A blind-posting bug
/// that debited the FULL blind from a short stack would either underflow the
/// stack (mint negative chips) or over-fund the pot.
///
/// Catches: a `post_blind` that ignores the stack clamp, or a side-pot builder
/// that lets the deep seats win MORE than the short blind actually contributed.
#[test]
fn short_big_blind_posts_partial_all_in_blind_and_conserves_chips() {
    // Dealer = p1 (idx 0). 3 handed → SB = p2 (idx 1), BB = p3 (idx 2).
    // The BB seat (p3) holds only 7 chips but owes a 20-chip big blind: it can
    // post at most 7 → a partial, all-in "dead" big blind. SB (p2) is also
    // short of the deep stack but covers its full 10-chip small blind.
    let big_blind = 20u32;
    let small_blind = 10u32;
    let short_bb = 7u32; // < big_blind → must post a PARTIAL all-in blind
    let players = vec![
        (pid(1), c(1000), 0u8),     // dealer — deep
        (pid(2), c(1000), 1u8),     // SB — deep, covers its blind in full
        (pid(3), c(short_bb), 2u8), // BB — CANNOT cover the big blind
    ];
    let total_start = 1000 + 1000 + short_bb;

    let mut hand = GameHand::new_with_rng(
        players,
        0,
        c(big_blind),
        c(small_blind),
        PokerRng::from_seed(4242),
    );
    hand.start()
        .expect("start must succeed even with a sub-BB stack");

    // After blinds: the short BB seat must be ALL-IN for exactly its 7 chips —
    // the engine posted a PARTIAL blind, not the full 20 (which would underflow).
    let snap = hand.snapshot();
    let bb_seat = snap
        .players
        .iter()
        .find(|p| p.player_id == pid(3))
        .expect("BB seat present in snapshot");
    assert_eq!(
        bb_seat.committed_this_street.0, short_bb,
        "short BB must post only its {short_bb} chips as a partial blind, got {}",
        bb_seat.committed_this_street.0
    );
    assert_eq!(
        bb_seat.stack.0, 0,
        "posting the partial blind must leave the short BB all-in (stack 0), got {}",
        bb_seat.stack.0
    );
    assert!(
        bb_seat.all_in,
        "a seat that posts its entire stack as a blind must be marked all-in"
    );

    // Drive the hand to completion: deeper seats call/check; the short BB has no
    // further action (already all-in). A partial-blind hand must still resolve.
    let mut safety = 0u32;
    while !hand.is_done() {
        safety += 1;
        assert!(safety < 100, "infinite loop driving the short-blind hand");
        let Some(actor) = hand.snapshot().current_actor else {
            break;
        };
        if hand.apply_action(actor, PlayerAction::Call).is_err() {
            let _ = hand.apply_action(actor, PlayerAction::Check);
        }
    }
    assert!(hand.is_done(), "short-blind hand must reach a conclusion");
    let result = hand.finish();

    // ── 1) Chip conservation — the partial blind must not mint or burn chips. ──
    let final_total: u32 = result.final_stacks.values().sum();
    assert_eq!(
        final_total, total_start,
        "partial/dead-blind hand must conserve chips ({final_total} != {total_start})"
    );
    let awarded: u32 = result.chips_awarded.values().sum();
    let pot_total: u32 = result.pots.iter().map(|p| p.amount.0).sum();
    assert_eq!(
        awarded, pot_total,
        "every chip in the pot(s) must be awarded ({awarded} != {pot_total})"
    );

    // ── 2) The short BB can only ever WIN the main pot it actually funded. ─────
    // It contributed exactly `short_bb` to the main pot, so two other seats
    // matching that builds a main pot of at most 3×short_bb; everything the deep
    // seats put in BEYOND short_bb belongs to a side pot the short BB is NOT
    // eligible for.
    for pot in result.pots.iter().filter(|p| p.amount.0 > 0) {
        for w in &pot.winners {
            assert!(
                pot.eligible_player_ids.contains(&w.player_id),
                "pot {:?} winner {:?} not in its eligible set",
                pot.index,
                w.player_id
            );
        }
    }
    let p3_won = *result.chips_awarded.get(&3).unwrap_or(&0);
    assert!(
        p3_won <= 3 * short_bb,
        "short BB cannot win beyond the 3×{short_bb} main pot it funded; won {p3_won}"
    );
}
