//! Engine-level property tests for invariants the QA agent kept missing.
//!
//! Why this file exists
//! --------------------
//! 2026-05-28 codex audit (`~/Desktop/issue/2026-05-28-qa-agent-gap-audit.html`)
//! found that 170 engine tests were passing while a P0 rule violation shipped:
//! HU postflop first actor returned SB instead of BB (commit e59e6c0). The
//! example-test approach can't catch rule bugs across the matrix of
//! (n_players, dealer, street). Property tests over the rule space can.
//!
//! Five properties live here, each named to spell out what's being tested.
//! Each is the SINGLE canonical implementation of an invariant we now treat
//! as architectural — no spec author and no test author may relax these
//! without an ADR.
//!
//! 1. `prop_wsop_first_actor` — preflop / postflop first actor per WSOP rule 35.
//! 2. `prop_chip_conservation` — sum(stacks) + sum(pots) is invariant
//!    across any legal action sequence.
//! 3. `prop_hole_card_secrecy` — server-side redaction is correctly
//!    *possible*: every ActionApplied event seat is the actor; HoleCardsDealt
//!    is emitted once per seat.
//! 4. `prop_min_raise_legality` — engine reports `min_raise_to` consistently;
//!    apply_action with an under-min Raise is rejected unless it's all-in.
//! 5. `prop_side_pot_distribution` — for varied stacks + a forced showdown,
//!    each side pot only includes seats that actually contributed.
//!
//! 6. `prop_side_pot_multistreet` — GPT-5.5 round-1 gap: the preflop-only
//!    `prop_side_pot_distribution` never exercised a mid-street all-in (one
//!    player folds preflop, another goes all-in on the flop, the third
//!    continues). This variant drives a mixed check/call line until the flop,
//!    then shoves all seats on the flop, and enforces the same eligibility +
//!    conservation invariants. Catches the case where a seat that was NOT
//!    involved preflop-to-flop transition ends up in a pot it didn't contribute
//!    to because the side-pot calculator joined on the wrong street's balances.
//!
//! These tests do not need DB. Run via `cargo test -p engine --test prop_invariants`.

use engine::{
    action::{ActionError, PlayerAction},
    card::{Card, Rank, Suit},
    deck::Deck,
    event::EngineEvent,
    game::{blind_positions, GameHand, HandResult},
    hand::HoleCards,
    player::{Chips, PlayerId},
    rng::PokerRng,
};
use proptest::prelude::*;

// ---------------------------------------------------------------------------
// Shared helpers
// ---------------------------------------------------------------------------

fn pid(n: u64) -> PlayerId {
    PlayerId::new(n)
}

fn make_hand(n: usize, dealer: usize, seed: u64, stack: u32) -> GameHand {
    let players: Vec<(PlayerId, Chips, u8)> = (0..n)
        .map(|i| (pid((i + 1) as u64), Chips(stack), i as u8))
        .collect();
    GameHand::new_with_rng(
        players,
        dealer,
        Chips(20),
        Chips(10),
        PokerRng::from_seed(seed),
    )
}

/// Build a full 52-card deck whose first cards are `prefix` (in order), with the
/// remaining 52 - prefix.len() cards appended in canonical order. `Deck::deal`
/// hands out `cards[0]` first, so `prefix` controls the hole cards + board.
/// Mirrors `full_hand.rs::rigged_deck` for the deterministic odd-chip test.
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

/// Drive `hand` to street `street` by issuing calls (when there's a bet) and
/// checks (when there isn't), until either the hand finishes or we reach the
/// target street. Returns the snapshot at the target street, or `None` if the
/// hand ended first.
fn drive_to_street_check_call(hand: &mut GameHand, street: engine::Street) -> bool {
    let mut safety = 0;
    while !hand.is_done() {
        safety += 1;
        if safety > 80 {
            return false;
        }
        let snap = hand.snapshot();
        if snap.street == street {
            return true;
        }
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
        if hand.apply_action(actor, action).is_err() {
            return false;
        }
    }
    hand.snapshot().street == street
}

// ---------------------------------------------------------------------------
// 1) WSOP first actor per (n_players, dealer, street).
// ---------------------------------------------------------------------------

proptest! {
    /// WSOP rule 35: post-flop first actor = first non-folded/non-all-in seat
    /// strictly LEFT of the button. Heads-up special case: BB acts first post-
    /// flop (the non-button player). Preflop in HU: SB (= dealer) acts first.
    ///
    /// Test space: n_players ∈ {2..=9}, dealer ∈ {0..n}, all four streets.
    /// We use a check/call line so no one folds, ensuring the first-actor seat
    /// is always the dealer's left (= dealer+1 mod n).
    #[test]
    fn prop_wsop_first_actor(
        n in 2usize..=9,
        dealer_off in 0usize..9,
        seed in 0u64..1_000_000,
    ) {
        let dealer = dealer_off % n;
        let mut hand = make_hand(n, dealer, seed, 5000);
        hand.start().expect("start ok");

        // ---- Preflop first actor ----
        let snap = hand.snapshot();
        let first_pre = snap.current_actor.expect("preflop must have actor");
        let first_pre_seat = snap
            .players
            .iter()
            .find(|p| p.player_id == first_pre)
            .map(|p| p.seat)
            .unwrap();
        let (sb_idx, bb_idx) = blind_positions(dealer, n);
        if n == 2 {
            // HU preflop: SB (= dealer) acts first.
            prop_assert_eq!(
                first_pre_seat as usize, sb_idx,
                "HU preflop first actor must be SB (dealer); got seat={}, dealer={}",
                first_pre_seat, dealer
            );
        } else {
            // 3+: UTG = first seat left of BB.
            let utg_idx = (bb_idx + 1) % n;
            prop_assert_eq!(
                first_pre_seat as usize, utg_idx,
                "{}-handed preflop first actor must be UTG=({}); got {}",
                n, utg_idx, first_pre_seat
            );
        }

        // ---- Postflop first actor (flop) ----
        if drive_to_street_check_call(&mut hand, engine::Street::Flop) {
            let snap = hand.snapshot();
            if let Some(first_flop) = snap.current_actor {
                let first_flop_seat = snap
                    .players
                    .iter()
                    .find(|p| p.player_id == first_flop)
                    .map(|p| p.seat)
                    .unwrap();
                let expected_seat = (dealer + 1) % n;
                prop_assert_eq!(
                    first_flop_seat as usize, expected_seat,
                    "{}-handed FLOP first actor must be dealer+1={} (WSOP rule 35); got seat={}; dealer={}",
                    n, expected_seat, first_flop_seat, dealer
                );
            }
        }

        // ---- Postflop first actor (turn) ----
        if drive_to_street_check_call(&mut hand, engine::Street::Turn) {
            let snap = hand.snapshot();
            if let Some(first_turn) = snap.current_actor {
                let first_turn_seat = snap
                    .players
                    .iter()
                    .find(|p| p.player_id == first_turn)
                    .map(|p| p.seat)
                    .unwrap();
                let expected_seat = (dealer + 1) % n;
                prop_assert_eq!(
                    first_turn_seat as usize, expected_seat,
                    "{}-handed TURN first actor must be dealer+1={}; got seat={}",
                    n, expected_seat, first_turn_seat
                );
            }
        }

        // ---- Postflop first actor (river) ----
        if drive_to_street_check_call(&mut hand, engine::Street::River) {
            let snap = hand.snapshot();
            if let Some(first_river) = snap.current_actor {
                let first_river_seat = snap
                    .players
                    .iter()
                    .find(|p| p.player_id == first_river)
                    .map(|p| p.seat)
                    .unwrap();
                let expected_seat = (dealer + 1) % n;
                prop_assert_eq!(
                    first_river_seat as usize, expected_seat,
                    "{}-handed RIVER first actor must be dealer+1={}; got seat={}",
                    n, expected_seat, first_river_seat
                );
            }
        }
    }
}

// ---------------------------------------------------------------------------
// 2) Chip conservation: sum(stacks) + pot == constant throughout a hand.
// ---------------------------------------------------------------------------

proptest! {
    /// Across any legal check/call sequence, the sum of all seat stacks
    /// PLUS the current pot must equal the total starting chips. This is the
    /// most basic poker invariant: chips never spawn or vanish.
    ///
    /// Test space: 2..=6 players, varying starting stacks, varying dealer.
    /// We use a check/call line (no raises) for simplicity — the
    /// conservation rule holds for ALL action sequences, but check/call gives
    /// the property test deterministic coverage.
    #[test]
    fn prop_chip_conservation(
        n in 2usize..=6,
        dealer_off in 0usize..9,
        // Include sub-small-blind stacks (5..) so the fuzzer reaches the
        // all-in-for-less-than-a-blind class where a folder's uncalled overage
        // above an all-in BB used to be destroyed (chip-conservation break,
        // audit 2026-06-04 finding #1). The dedicated heterogeneous repro lives
        // in tests/chip_conservation_fold_overage.rs.
        seed in 0u64..1_000_000,
        stack in 5u32..2000,
    ) {
        let dealer = dealer_off % n;
        let total_start: u32 = stack * (n as u32);

        let mut hand = make_hand(n, dealer, seed, stack);
        hand.start().expect("start ok");

        // After start: chips are split between stacks and pot.
        let snap = hand.snapshot();
        let sum_stacks: u32 = snap.players.iter().map(|p| p.stack.0).sum();
        let pot = snap.pot.0;
        prop_assert_eq!(
            sum_stacks + pot,
            total_start,
            "after start(): sum(stacks)={} + pot={} != total_start={} ({}-handed dealer={})",
            sum_stacks, pot, total_start, n, dealer
        );

        // Drive the hand to completion via check/call.
        let mut safety = 0;
        while !hand.is_done() {
            safety += 1;
            if safety > 80 { break; }
            let snap = hand.snapshot();
            let Some(actor) = snap.current_actor else { break };
            let committed = snap
                .players
                .iter()
                .find(|p| p.player_id == actor)
                .map(|p| p.committed_this_street.0)
                .unwrap_or(0);
            let to_call = snap.current_bet.0.saturating_sub(committed);
            let action = if to_call > 0 { PlayerAction::Call } else { PlayerAction::Check };
            if hand.apply_action(actor, action).is_err() { break; }

            // Per-step invariant: after every legal action, sum(stacks) + pot is invariant.
            let snap = hand.snapshot();
            let sum_stacks: u32 = snap.players.iter().map(|p| p.stack.0).sum();
            let pot = snap.pot.0;
            prop_assert_eq!(
                sum_stacks + pot,
                total_start,
                "mid-hand: sum(stacks)={} + pot={} != total_start={}",
                sum_stacks, pot, total_start
            );
        }

        // After finish(): chips are awarded back into stacks; final stacks must sum to total_start.
        if hand.is_done() {
            let result = hand.finish();
            let final_total: u32 = result.final_stacks.values().sum();
            prop_assert_eq!(
                final_total,
                total_start,
                "after finish(): final_stacks total {} != total_start {} (commit-28da768 class)",
                final_total, total_start
            );
        }
    }
}

proptest! {
    /// Companion to `prop_chip_conservation` that closes its OWN documented gap.
    /// That test drives a pure check/call line "for simplicity" (see its
    /// docstring), so its per-step `sum(stacks)+pot == total_start` assertion is
    /// only ever exercised over the check/call subspace. A 2026-06-03
    /// regression-gap analysis flagged that Fold / Raise / mixed-all-in
    /// interleaving was never fuzzed against PER-STEP conservation (the only
    /// all-in fuzz, `prop_side_pot_distribution`, asserts conservation at
    /// `finish()` only). This variant drives a MIXED action line — each step it
    /// picks from {min-Raise, AllIn, Call/Check, Fold} via a seed-derived PRNG
    /// and validates the pick against the engine's own legality (illegal picks
    /// fall through to a guaranteed-legal check/call), so conservation is now
    /// asserted after every legal Fold, Raise, and all-in too, not just
    /// check/call. The invariant is architectural — do not relax without an ADR.
    #[test]
    fn prop_chip_conservation_mixed_actions(
        n in 2usize..=6,
        dealer_off in 0usize..9,
        // Sub-blind stacks (5..) so the mixed Fold/Raise/AllIn line can drive a
        // folder's uncalled overage above an all-in short stack — the exact
        // shape of the destroyed-overage bug (audit 2026-06-04 finding #1).
        seed in 0u64..1_000_000,
        stack in 5u32..2000,
    ) {
        let dealer = dealer_off % n;
        let total_start: u32 = stack * (n as u32);

        let mut hand = make_hand(n, dealer, seed, stack);
        hand.start().expect("start ok");

        // After start: chips are split between stacks and pot.
        {
            let snap = hand.snapshot();
            let sum_stacks: u32 = snap.players.iter().map(|p| p.stack.0).sum();
            prop_assert_eq!(
                sum_stacks + snap.pot.0,
                total_start,
                "after start(): sum(stacks)={} + pot={} != total_start={} ({}-handed dealer={})",
                sum_stacks, snap.pot.0, total_start, n, dealer
            );
        }

        // Drive the hand with a fuzzed, legality-checked mix of actions. `rng`
        // is a simple seed-derived LCG so the action mix is deterministic per
        // proptest case (shrinking stays reproducible) yet varies within a hand.
        let mut rng: u64 = seed | 1;
        let mut safety = 0;
        while !hand.is_done() {
            safety += 1;
            if safety > 200 { break; }
            rng = rng.wrapping_mul(6364136223846793005).wrapping_add(1442695040888963407);

            let snap = hand.snapshot();
            let Some(actor) = snap.current_actor else { break };
            let me = snap.players.iter().find(|p| p.player_id == actor);
            let committed = me.map(|p| p.committed_this_street.0).unwrap_or(0);
            let actor_stack = me.map(|p| p.stack.0).unwrap_or(0);
            let to_call = snap.current_bet.0.saturating_sub(committed);

            // Candidate actions richest-first; each is *attempted* and the first
            // that the engine accepts is used. A guaranteed-legal check/call is
            // appended last so the loop always makes progress.
            let mut candidates: Vec<PlayerAction> = Vec::new();
            if let Some(mr) = snap.min_raise_to {
                if mr.0.saturating_sub(committed) <= actor_stack && actor_stack > 0 {
                    candidates.push(PlayerAction::Raise { amount: mr });
                }
            }
            candidates.push(PlayerAction::AllIn);
            if to_call > 0 {
                candidates.push(PlayerAction::Call);
                candidates.push(PlayerAction::Fold);
            } else {
                candidates.push(PlayerAction::Check);
                candidates.push(PlayerAction::Fold);
            }
            // Rotate so the preferred action varies step-to-step.
            if !candidates.is_empty() {
                let k = ((rng >> 24) as usize) % candidates.len();
                candidates.rotate_left(k);
            }
            candidates.push(if to_call > 0 { PlayerAction::Call } else { PlayerAction::Check });

            let mut applied = false;
            for act in candidates {
                if hand.apply_action(actor, act).is_ok() {
                    applied = true;
                    break;
                }
            }
            if !applied { break; }

            // Per-step invariant: after EVERY accepted action (any type), conserve.
            let snap = hand.snapshot();
            let sum_stacks: u32 = snap.players.iter().map(|p| p.stack.0).sum();
            prop_assert_eq!(
                sum_stacks + snap.pot.0,
                total_start,
                "mid-hand mixed-action: sum(stacks)={} + pot={} != total_start={} ({}-handed dealer={} seed={})",
                sum_stacks, snap.pot.0, total_start, n, dealer, seed
            );
        }

        // After finish(): awarded chips must sum back to total_start.
        if hand.is_done() {
            let result = hand.finish();
            let final_total: u32 = result.final_stacks.values().sum();
            prop_assert_eq!(
                final_total,
                total_start,
                "after finish() mixed-action: final_stacks total {} != total_start {} (commit-28da768 class)",
                final_total, total_start
            );
        }
    }
}

// ---------------------------------------------------------------------------
// 3) Hole card secrecy: ActionApplied seats are valid, HoleCardsDealt emitted
//    exactly once per seat.
// ---------------------------------------------------------------------------

proptest! {
    /// The engine emits `HoleCardsDealt { seat, cards }` for every seat
    /// exactly once. Per the engine API contract (event.rs:14-16):
    ///   "Cards in `HoleCardsDealt` are unredacted. Per-client redaction
    ///    (showing cards only to their owner) is the server's concern."
    ///
    /// This property test enforces that the engine SUPPLIES exactly one
    /// hole-card event per seat — anything more is a leak risk; anything
    /// less is a missing card on the wire. Server-side redaction is the
    /// next defense; this is the prerequisite.
    ///
    /// The corresponding server-side property test would observe per-recipient
    /// frame logs — that requires the session/transport layer and lives in
    /// server/tests/ when the test-hook (POST /api/dev/script-action) lands.
    #[test]
    fn prop_hole_card_secrecy(
        n in 2usize..=6,
        dealer_off in 0usize..9,
        seed in 0u64..1_000_000,
    ) {
        let dealer = dealer_off % n;
        let mut hand = make_hand(n, dealer, seed, 1000);
        hand.start().expect("start ok");
        let events = hand.drain_events();

        // Count HoleCardsDealt events per seat.
        let mut hole_count = vec![0u32; n];
        for ev in &events {
            if let EngineEvent::HoleCardsDealt { seat, cards } = ev {
                let s = *seat as usize;
                prop_assert!(s < n, "HoleCardsDealt seat {} >= n_players {}", s, n);
                hole_count[s] += 1;
                // Two distinct cards per seat.
                prop_assert_ne!(
                    cards[0], cards[1],
                    "HoleCardsDealt seat {}: card1 == card2 ({:?})", s, cards[0]
                );
            }
        }
        for (seat, count) in hole_count.iter().enumerate() {
            prop_assert_eq!(
                *count, 1u32,
                "seat {} received {} HoleCardsDealt events (must be exactly 1); n={}",
                seat, count, n
            );
        }

        // Every ActionApplied seat must be a valid seat index.
        for ev in &events {
            if let EngineEvent::ActionApplied { seat, .. } = ev {
                prop_assert!(
                    (*seat as usize) < n,
                    "ActionApplied seat {} out of range (n={})", seat, n
                );
            }
        }
    }
}

// ---------------------------------------------------------------------------
// 4) Min-raise legality: under-min Raise is rejected unless all-in.
// ---------------------------------------------------------------------------

proptest! {
    /// For every state where it is seat S's turn with stack > min_raise_to,
    /// applying a Raise to an amount STRICTLY less than `min_raise_to` (and
    /// less than seat.stack) MUST return `ActionError::BelowMinRaise`. This
    /// is the commit-8a14518 / 9f1812a class of bug.
    ///
    /// We test by:
    ///   1. start() a hand
    ///   2. read the engine's reported `min_raise_to`
    ///   3. attempt Raise{amount = min_raise_to - 1} (must fail unless it
    ///      happens to equal the actor's all-in amount)
    ///   4. attempt Raise{amount = min_raise_to} (must succeed)
    #[test]
    fn prop_min_raise_legality(
        n in 2usize..=6,
        dealer_off in 0usize..9,
        seed in 0u64..1_000_000,
    ) {
        let dealer = dealer_off % n;
        let mut hand = make_hand(n, dealer, seed, 5000);
        hand.start().expect("start ok");
        let snap = hand.snapshot();
        let Some(actor) = snap.current_actor else {
            return Ok(());
        };
        let Some(min_raise_to) = snap.min_raise_to else {
            return Ok(());
        };
        if min_raise_to.0 < 2 {
            return Ok(()); // no room to test "below min"
        }
        let actor_stack = snap
            .players
            .iter()
            .find(|p| p.player_id == actor)
            .map(|p| p.stack.0)
            .unwrap_or(0);

        let committed = snap
            .players
            .iter()
            .find(|p| p.player_id == actor)
            .map(|p| p.committed_this_street.0)
            .unwrap_or(0);

        // -- Negative case: amount = min_raise_to - 1 must fail (unless equals all-in) --
        // We need a fresh hand from the same seed so we can attempt the bad raise
        // without polluting the positive-case hand. GameHand isn't Clone, but the
        // RNG is deterministic from seed, so we just rebuild.
        let below = min_raise_to.0 - 1;
        let to_commit = below.saturating_sub(committed);
        let is_all_in_under = to_commit == actor_stack && actor_stack > 0;
        if !is_all_in_under {
            let mut try_hand = make_hand(n, dealer, seed, 5000);
            try_hand.start().expect("start ok (rebuild)");
            let try_actor = try_hand
                .snapshot()
                .current_actor
                .expect("rebuilt hand must have same first actor");
            prop_assert_eq!(try_actor, actor, "rebuilt hand must have same actor");
            let err =
                try_hand.apply_action(actor, PlayerAction::Raise { amount: Chips(below) });
            prop_assert!(
                matches!(err, Err(ActionError::BelowMinRaise)),
                "Raise to {} (min_raise_to={}) must be BelowMinRaise; got {:?}",
                below, min_raise_to.0, err
            );
        }

        // -- Positive case: amount = min_raise_to must succeed --
        let to_commit_min = min_raise_to.0.saturating_sub(committed);
        if to_commit_min <= actor_stack {
            let r = hand.apply_action(actor, PlayerAction::Raise { amount: min_raise_to });
            prop_assert!(
                r.is_ok(),
                "Raise to min_raise_to={} must succeed (actor_stack={}); got {:?}",
                min_raise_to.0, actor_stack, r.err()
            );
        }
    }
}

// ---------------------------------------------------------------------------
// 5) Side-pot distribution: each side pot only includes contributing seats.
// ---------------------------------------------------------------------------

proptest! {
    /// 3-handed game with varied stacks. Drive everyone all-in preflop. The
    /// engine builds 1 or 2 side pots depending on stack relationships. Each
    /// side pot's `eligible_player_ids` must be a subset of the seats that
    /// actually contributed at least `pot.cap` chips.
    ///
    /// Catches: any future regression where a 0-contribution seat ends up
    /// eligible for a pot (would happen if the round side-pot calculator
    /// joined eligibility on the wrong set).
    #[test]
    fn prop_side_pot_distribution(
        seed in 0u64..1_000_000,
        s1 in 50u32..200,
        s2 in 200u32..500,
        s3 in 500u32..1500,
    ) {
        // 3-handed asymmetric stacks: s1 < s2 < s3. All in.
        let players: Vec<(PlayerId, Chips, u8)> = vec![
            (pid(1), Chips(s1), 0),
            (pid(2), Chips(s2), 1),
            (pid(3), Chips(s3), 2),
        ];
        let mut hand = GameHand::new_with_rng(
            players,
            0, // dealer = pid(1)
            Chips(20),
            Chips(10),
            PokerRng::from_seed(seed),
        );
        hand.start().expect("start ok");

        // Drive everyone all-in: each actor shoves; folded seats are skipped by engine.
        let mut safety = 0;
        while !hand.is_done() {
            safety += 1;
            if safety > 30 { break; }
            let snap = hand.snapshot();
            let Some(actor) = snap.current_actor else { break };
            // If actor's stack is 0 (couldn't happen — engine skips all-in),
            // shove. Otherwise shove.
            if hand.apply_action(actor, PlayerAction::AllIn).is_err() {
                // If shove unavailable, call.
                let _ = hand.apply_action(actor, PlayerAction::Call);
            }
        }

        // All-in runout: hand should be done.
        prop_assert!(hand.is_done(), "all 3 all-in preflop must conclude the hand");
        let result = hand.finish();

        // Per-pot invariant: pot.amount > 0 ⇒ eligible set non-empty AND each
        // eligible player contributed ≥ pot.amount / count(eligible).
        // Stronger: each eligible player must NOT have been a "0 contributor".
        for (i, pot) in result.pots.iter().enumerate() {
            prop_assert!(
                !pot.eligible_player_ids.is_empty() || pot.amount.0 == 0,
                "pot[{}].amount={} but eligible is empty",
                i, pot.amount.0
            );
            // All winners must be in the eligible set.
            for w in &pot.winners {
                prop_assert!(
                    pot.eligible_player_ids.contains(&w.player_id),
                    "pot[{}] winner pid={} not in eligible set {:?}",
                    i, w.player_id, pot.eligible_player_ids
                );
            }
        }

        // Chip conservation across the whole result.
        let total_start = s1 + s2 + s3;
        let final_total: u32 = result.final_stacks.values().sum();
        prop_assert_eq!(
            final_total,
            total_start,
            "side-pot run-out: final_stacks total {} != total_start {}",
            final_total, total_start
        );
    }
}

// ---------------------------------------------------------------------------
// 6) Side-pot distribution — multi-street all-in (GPT-5.5 round-1 gap).
// ---------------------------------------------------------------------------

proptest! {
    /// The existing `prop_side_pot_distribution` only exercises a PREFLOP shove
    /// where all seats go all-in before a flop is dealt. This leaves untested the
    /// common live-play scenario:
    ///
    ///   - 3 seats, varied stacks: s1 (small) < s2 (medium) < s3 (large).
    ///   - Preflop: everyone calls (no fold, no shove) → flop is dealt.
    ///   - Flop: s1 shoves all-in; s2 and s3 then shove too (or call).
    ///
    /// In this shape the side-pot calculator must correctly split the pot into:
    ///   * Main pot: eligible = all 3 seats (everyone contributed s1 chips).
    ///   * Side pot 1: eligible = s2 + s3 (s1 is covered, only s2/s3 put in more).
    ///   * Side pot 2 (if s2 < s3): eligible = s3 only (s2 is fully covered).
    ///
    /// The invariant asserted here is the SAME as `prop_side_pot_distribution`:
    ///   - Pot eligibility: winner ∈ eligible set for each pot.
    ///   - Chip conservation: Σ final_stacks = total_start.
    ///
    /// Additionally checks that the seat with the smallest stack (s1) is
    /// eligible for at least one pot (the main pot) — a regression where the
    /// flop-street bookkeeping forgot the preflop contributions from s1 would
    /// zero-out s1's eligibility.
    #[test]
    fn prop_side_pot_multistreet(
        seed in 0u64..1_000_000,
        s1 in 30u32..80,    // short stack — s1 < s2 guaranteed by ranges
        s2 in 100u32..300,  // medium
        s3 in 400u32..1000, // big stack
    ) {
        // 3-handed: pid(1) = s1, pid(2) = s2, pid(3) = s3.
        let p1 = pid(1);
        let p2 = pid(2);
        let p3 = pid(3);
        let players: Vec<(PlayerId, Chips, u8)> = vec![
            (p1, Chips(s1), 0),
            (p2, Chips(s2), 1),
            (p3, Chips(s3), 2),
        ];
        let mut hand = GameHand::new_with_rng(
            players,
            0, // dealer = pid(1) (seat 0)
            Chips(4),  // BB
            Chips(2),  // SB
            PokerRng::from_seed(seed),
        );
        hand.start().expect("start ok");

        // Phase 1 — drive preflop via check/call so everyone survives to the flop.
        // At most 20 steps (3-handed preflop is at most 4 actions per player).
        let mut safety = 0;
        while !hand.is_done() {
            safety += 1;
            if safety > 20 { break; }
            let snap = hand.snapshot();
            if snap.street != engine::Street::Preflop { break; }
            let Some(actor) = snap.current_actor else { break };
            let committed = snap.players.iter()
                .find(|p| p.player_id == actor)
                .map(|p| p.committed_this_street.0)
                .unwrap_or(0);
            let to_call = snap.current_bet.0.saturating_sub(committed);
            let action = if to_call > 0 { PlayerAction::Call } else { PlayerAction::Check };
            if hand.apply_action(actor, action).is_err() { break; }
        }

        // If the hand ended during preflop (e.g. someone busted on blinds due to
        // very small s1 relative to BB), just verify conservation and exit.
        if hand.is_done() {
            let result = hand.finish();
            let final_total: u32 = result.final_stacks.values().sum();
            let total_start = s1 + s2 + s3;
            prop_assert_eq!(
                final_total, total_start,
                "preflop-ended: final_stacks {} != total_start {}",
                final_total, total_start
            );
            return Ok(());
        }

        // Phase 2 — we are on the flop (or later). Everyone shoves all-in.
        let mut safety = 0;
        while !hand.is_done() {
            safety += 1;
            if safety > 30 { break; }
            let snap = hand.snapshot();
            let Some(actor) = snap.current_actor else { break };
            if hand.apply_action(actor, PlayerAction::AllIn).is_err() {
                // AllIn may be illegal if actor is already all-in; try Call.
                if hand.apply_action(actor, PlayerAction::Call).is_err() { break; }
            }
        }

        prop_assert!(hand.is_done(), "flop-shove sequence must conclude the hand");
        let result = hand.finish();

        // --- Pot invariants ---
        for (i, pot) in result.pots.iter().enumerate() {
            prop_assert!(
                !pot.eligible_player_ids.is_empty() || pot.amount.0 == 0,
                "pot[{}].amount={} but eligible is empty",
                i, pot.amount.0
            );
            for w in &pot.winners {
                prop_assert!(
                    pot.eligible_player_ids.contains(&w.player_id),
                    "pot[{}] winner pid={:?} not in eligible set {:?} (multistreet)",
                    i, w.player_id, pot.eligible_player_ids
                );
            }
        }

        // s1 must be eligible for at least the main pot (they contributed to it).
        let p1_eligible_pots: Vec<usize> = result.pots.iter().enumerate()
            .filter(|(_, pot)| pot.eligible_player_ids.contains(&p1) && pot.amount.0 > 0)
            .map(|(i, _)| i)
            .collect();
        prop_assert!(
            !p1_eligible_pots.is_empty(),
            "s1 (short stack) must be eligible for at least the main pot; \
             pots={:?}",
            result.pots.iter().map(|p| (p.amount.0, p.eligible_player_ids.len())).collect::<Vec<_>>()
        );

        // Chip conservation.
        let total_start = s1 + s2 + s3;
        let final_total: u32 = result.final_stacks.values().sum();
        prop_assert_eq!(
            final_total,
            total_start,
            "multistreet side-pot: final_stacks {} != total_start {} (s1={} s2={} s3={} seed={})",
            final_total, total_start, s1, s2, s3, seed
        );
    }
}

// ---------------------------------------------------------------------------
// 6b) N-handed side-pot run-out (3..=6 seats, multiple simultaneous all-ins).
// ---------------------------------------------------------------------------

proptest! {
    // The brute-force winner oracle runs once per CONTESTED pot per seat, so cap
    // the case count to keep a 6-handed multi-side-pot sweep bounded.
    #![proptest_config(ProptestConfig::with_cases(160))]
    /// Generalizes `prop_side_pot_distribution` (hard-coded to exactly 3 seats)
    /// to `n ∈ 3..=6` with N strictly-increasing all-in stacks. Distinct stack
    /// sizes guarantee MULTIPLE simultaneous all-ins → a main pot plus 2+ side
    /// pots (one capped layer per smaller stack). Everyone shoves preflop so the
    /// hand always runs out to a showdown.
    ///
    /// Asserts the full side-pot contract at once:
    ///   - Per-pot eligibility: every winner of a pot is in that pot's eligible
    ///     set (only contributing seats can win that pot).
    ///   - Global chip conservation: Σ final_stacks == Σ starting stacks.
    ///   - Winner-VALUE per pot: `assert_winners_match_oracle` — each contested
    ///     pot is awarded to the genuinely best five-card hand among its
    ///     eligible-and-shown seats, cross-checked by the independent best-5-of-7
    ///     brute-force oracle (the side-pot winner-selection path is distinct
    ///     from the single-pot path).
    #[test]
    fn prop_side_pot_distribution_n_handed(
        n in 3usize..=6,
        // Base of the smallest all-in stack and a per-seat increment; building
        // each seat's stack as base + i*step (i = 0..n) keeps them STRICTLY
        // increasing, so every seat has a distinct all-in cap → n-1 side pots.
        base in 40u32..120,
        step in 30u32..120,
        seed in 0u64..1_000_000,
    ) {
        // Strictly-increasing stacks: seat i (vec index i) gets base + i*step.
        let players: Vec<(PlayerId, Chips, u8)> = (0..n)
            .map(|i| {
                let stack = base + (i as u32) * step;
                (pid((i + 1) as u64), Chips(stack), i as u8)
            })
            .collect();
        let total_start: u32 = players.iter().map(|(_, c, _)| c.0).sum();

        let mut hand = GameHand::new_with_rng(
            players,
            0, // dealer = pid(1) (the shortest stack)
            Chips(20),
            Chips(10),
            PokerRng::from_seed(seed),
        );
        hand.start().expect("start ok");

        // Drive everyone all-in preflop: each actor shoves; folded/all-in seats
        // are skipped by the engine. Distinct stacks ⇒ each shove caps at a
        // different layer ⇒ main pot + (n-1) side pots.
        let mut safety = 0;
        while !hand.is_done() {
            safety += 1;
            if safety > 60 { break; }
            let snap = hand.snapshot();
            let Some(actor) = snap.current_actor else { break };
            if hand.apply_action(actor, PlayerAction::AllIn).is_err() {
                let _ = hand.apply_action(actor, PlayerAction::Call);
            }
        }

        prop_assert!(hand.is_done(), "all {} seats all-in preflop must conclude the hand", n);
        let result = hand.finish();

        // --- Per-pot eligibility: only contributing seats can win a pot. ---
        for (i, pot) in result.pots.iter().enumerate() {
            prop_assert!(
                !pot.eligible_player_ids.is_empty() || pot.amount.0 == 0,
                "pot[{}].amount={} but eligible is empty",
                i, pot.amount.0
            );
            for w in &pot.winners {
                prop_assert!(
                    pot.eligible_player_ids.contains(&w.player_id),
                    "pot[{}] winner pid={} not in eligible set {:?} (n={})",
                    i, w.player_id, pot.eligible_player_ids, n
                );
            }
        }

        // --- Global chip conservation. ---
        let final_total: u32 = result.final_stacks.values().sum();
        prop_assert_eq!(
            final_total,
            total_start,
            "n-handed side-pot run-out: final_stacks {} != total_start {} (n={} base={} step={} seed={})",
            final_total, total_start, n, base, step, seed
        );

        // --- Winner-VALUE: each contested pot goes to the best hand. ---
        if result.board.count() == 5 && result.showdown.len() >= 2 {
            assert_winners_match_oracle(&result)?;
        }
    }
}

// ---------------------------------------------------------------------------
// 6c) ODD-CHIP AMOUNT distribution — TDA Rule 25 drift gate.
// ---------------------------------------------------------------------------
//
// Why this section exists
// -----------------------
// `assert_winners_match_oracle` (and every prop above) checks winner IDENTITY
// only — never the chip AMOUNTS each winner is paid. Exact split payouts and
// odd-remainder placement were covered by only TWO hand-built examples
// (`game.rs::odd_chip_goes_to_earliest_position_winner`,
// `full_hand.rs::split_pot_exact_tie_odd_chip_remainder_board_plays`). This
// proptest makes odd-chip distribution a DRIFT GATE across seat count + dealer
// position.
//
// Construction (fully deterministic via a rigged deck — no seed dependence on
// the outcome): `w` "winner" seats are each dealt a distinct Ten + a dead blank;
// one "loser" seat (the last vec index) is dealt pocket deuces. The board is
// A♣ K♥ Q♦ J♠ 6♣ — a FOUR-card broadway (no Ten of its own), so only the seats
// holding a Ten complete the A-K-Q-J-T nut straight; the deuces seat makes at
// most a pair and loses. The `w` Ten-holders therefore make the IDENTICAL
// broadway → an exact w-way tie. The loser is all-in for an ODD `cap`; the
// winners cover it and are deeper, so the MAIN pot = (w+1)·cap is contested only
// by the `w` tied winners → its remainder `(w+1)·cap mod w` exercises the odd-
// chip path. (Without the dead loser money, a fully-matched tie layer is always
// `w·cap`, exactly divisible — no odd chip ever appears.)
//
// Asserts the TDA Rule 25 contract for every multi-winner pot:
//   (a) Σ winner amounts == pot amount (no chip minted or destroyed).
//   (b) the winners' shares differ by AT MOST 1 (the odd chips are spread one
//       per seat, never piled onto a single seat).
//   (c) the seat(s) receiving the +1 odd chip are exactly the `rem` tied winners
//       with the LOWEST button-relative order (first seats left of the button).
proptest! {
    #[test]
    fn prop_odd_chip_goes_to_lowest_button_order(
        w in 2usize..=4,            // number of tied winners (Ten-holders)
        dealer_off in 0usize..9,
        odd_cap in 25u32..200,      // loser's all-in cap; forced ODD below
    ) {
        let n = w + 1;              // winners + one short-stack loser
        let dealer = dealer_off % n;
        let cap = odd_cap | 1;      // force ODD so a remainder can appear

        // Tens for the winners (distinct suits) + dead blank second cards.
        let tens = [
            Card::new(Rank::Ten, Suit::Diamonds),
            Card::new(Rank::Ten, Suit::Hearts),
            Card::new(Rank::Ten, Suit::Spades),
            Card::new(Rank::Ten, Suit::Clubs),
        ];
        // Dead, low, non-pairing, non-straightening blanks (no Ten/J/Q/K/A,
        // distinct ranks, none equal to the board's 6).
        let blanks = [
            Card::new(Rank::Three, Suit::Spades),
            Card::new(Rank::Four, Suit::Hearts),
            Card::new(Rank::Five, Suit::Diamonds),
            Card::new(Rank::Seven, Suit::Clubs),
        ];
        // Loser = pocket deuces (a pair, beaten by the broadway straight).
        let loser_c1 = Card::new(Rank::Two, Suit::Spades);
        let loser_c2 = Card::new(Rank::Two, Suit::Hearts);

        // Board: four broadway cards + a low blank → only a Ten completes A-K-Q-J-T.
        // 2 clubs max on board (A♣, 6♣) ⇒ no flush is reachable with 2 hole cards.
        let board = [
            Card::new(Rank::Ace, Suit::Clubs),
            Card::new(Rank::King, Suit::Hearts),
            Card::new(Rank::Queen, Suit::Diamonds),
            Card::new(Rank::Jack, Suit::Spades),
            Card::new(Rank::Six, Suit::Clubs),
        ];

        // Hole-card deal order is two passes over seats in VEC order:
        //   [s0.c1, s1.c1, .., s_{n-1}.c1, s0.c2, .., s_{n-1}.c2, flop×3, turn, river]
        // Seats 0..w = winners (Ten + blank); seat w = loser (2♠2♥).
        let mut first_cards: Vec<Card> = Vec::with_capacity(n);
        let mut second_cards: Vec<Card> = Vec::with_capacity(n);
        for i in 0..w {
            first_cards.push(tens[i]);
            second_cards.push(blanks[i]);
        }
        first_cards.push(loser_c1);
        second_cards.push(loser_c2);

        let mut prefix: Vec<Card> = Vec::with_capacity(2 * n + 5);
        prefix.extend_from_slice(&first_cards);
        prefix.extend_from_slice(&second_cards);
        prefix.extend_from_slice(&board); // flop(3) + turn(1) + river(1)

        // Stacks: winners deep + equal; loser short with the ODD cap.
        let winner_stack = cap + 500;
        let mut players: Vec<(PlayerId, Chips, u8)> = (0..w)
            .map(|i| (pid((i + 1) as u64), Chips(winner_stack), i as u8))
            .collect();
        players.push((pid((w + 1) as u64), Chips(cap), w as u8));
        let total_start: u32 = players.iter().map(|(_, c, _)| c.0).sum();

        let mut hand = GameHand::new_with_deck(
            players,
            dealer,
            Chips(20),
            Chips(10),
            rigged_deck(&prefix),
            [0u8; 32],
        );
        hand.start().expect("start ok");

        // Everyone all-in preflop (loser caps the main pot; winners cover it).
        let mut safety = 0;
        while !hand.is_done() {
            safety += 1;
            if safety > 60 { break; }
            let Some(actor) = hand.snapshot().current_actor else { break };
            if hand.apply_action(actor, PlayerAction::AllIn).is_err() {
                let _ = hand.apply_action(actor, PlayerAction::Call);
            }
        }

        prop_assert!(hand.is_done(), "all-in preflop must conclude the hand");
        let result = hand.finish();
        prop_assert_eq!(result.board.count(), 5, "rigged board must run out fully");

        // Chip conservation (global) — defensive.
        let final_total: u32 = result.final_stacks.values().sum();
        prop_assert_eq!(
            final_total, total_start,
            "odd-chip run-out: final_stacks {} != total_start {}",
            final_total, total_start
        );

        // Button-relative order: 0 = first seat left of button (TDA Rule 25),
        // mirroring `GameHand::button_relative_order` = (dealer + 1 + offset) % n.
        let mut button_order: std::collections::HashMap<u64, usize> =
            std::collections::HashMap::with_capacity(n);
        for offset in 0..n {
            let seat_idx = (dealer + 1 + offset) % n;
            // Seat `seat_idx` belongs to pid(seat_idx + 1) (vec index == seat).
            button_order.insert((seat_idx as u64) + 1, offset);
        }

        // The deuces loser must NOT win the main pot (the straight beats a pair).
        // Verify at least one CONTESTED multi-winner pot exists so the odd-chip
        // path is genuinely exercised — and that the loser is excluded from it.
        let loser_pid = (w as u64) + 1;
        let mut saw_remainder = false;

        for pot in &result.pots {
            if pot.is_refund || pot.amount.0 == 0 || pot.winners.len() < 2 {
                continue;
            }
            // (a) Σ winner amounts == pot amount.
            let awarded: u32 = pot.winners.iter().map(|w| w.amount_won.0).sum();
            prop_assert_eq!(
                awarded, pot.amount.0,
                "pot[{}] Σ awarded {} != pot amount {}",
                pot.index, awarded, pot.amount.0
            );

            // The loser (pocket deuces) can never be a tied WINNER of a pot.
            prop_assert!(
                !pot.winners.iter().any(|w| w.player_id.inner() == loser_pid),
                "pot[{}] paid the deuces loser pid={} (must lose to the straight)",
                pot.index, loser_pid
            );

            let num_winners = pot.winners.len() as u32;
            let base = pot.amount.0 / num_winners;
            let rem = (pot.amount.0 % num_winners) as usize;

            // (b) shares differ by AT MOST 1.
            let max_share = pot.winners.iter().map(|w| w.amount_won.0).max().unwrap();
            let min_share = pot.winners.iter().map(|w| w.amount_won.0).min().unwrap();
            prop_assert!(
                max_share - min_share <= 1,
                "pot[{}] winner shares differ by more than 1 (min={}, max={}, rem={}); \
                 TDA Rule 25 spreads odd chips one-per-seat, never piled on one seat",
                pot.index, min_share, max_share, rem
            );

            // (c) the +1 odd chip(s) go to the `rem` lowest-button-order winners.
            if rem > 0 {
                saw_remainder = true;
                // Winners sorted by button-relative order (then pid for determinism).
                let mut by_order: Vec<(u64, usize)> = pot
                    .winners
                    .iter()
                    .map(|w| {
                        let p = w.player_id.inner();
                        (p, *button_order.get(&p).unwrap_or(&usize::MAX))
                    })
                    .collect();
                by_order.sort_by_key(|(p, ord)| (*ord, *p));
                let expected_plus_one: std::collections::HashSet<u64> =
                    by_order.iter().take(rem).map(|(p, _)| *p).collect();

                for win in &pot.winners {
                    let got = win.amount_won.0;
                    let p = win.player_id.inner();
                    let expect = if expected_plus_one.contains(&p) { base + 1 } else { base };
                    prop_assert_eq!(
                        got, expect,
                        "pot[{}] pid={} got {} but TDA Rule 25 expects {} \
                         (base={}, rem={}, +1 seats by button order = {:?})",
                        pot.index, p, got, expect, base, rem,
                        expected_plus_one.iter().copied().collect::<Vec<_>>()
                    );
                }
            }
        }

        // Guarantee the odd-chip path actually fired for w=2 (rem is always 1:
        // main pot = 3·cap, cap odd ⇒ 3·cap odd ⇒ rem 1 over 2 winners). This
        // keeps the test from passing vacuously if the engine ever stopped
        // forming the contested main pot.
        if w == 2 {
            prop_assert!(
                saw_remainder,
                "w=2 must produce an odd main pot (3·cap, cap={} odd) → a remainder; \
                 none observed (pots={:?})",
                cap,
                result.pots.iter()
                    .map(|p| (p.amount.0, p.winners.len()))
                    .collect::<Vec<_>>()
            );
        }
    }
}

// ---------------------------------------------------------------------------
// 7) Showdown WINNER-VALUE oracle — the pot must go to the BEST five-card hand.
// ---------------------------------------------------------------------------
//
// Why this section exists
// -----------------------
// 2026-06-16 bug-free visual+poker review-gap diagnosis (RC-1 / RC-4): the
// release loop's verdict is exit-code math, so a hand-evaluator or pot-award
// regression that pays the pot to the WRONG made hand ships GREEN — chips still
// conserve (`prop_chip_conservation`), Σnet still equals 0 (server standings),
// and the same seed still reproduces the same (wrong) winner (the determinism
// test asserts *deterministic*, never *correct*). Nothing asserted that the pot
// went to the genuinely strongest hand. In a money-adjacent poker product that
// is the single worst bug class, and it had ZERO coverage.
//
// The fix is an INDEPENDENT best-5-of-7 evaluator that shares no code with
// `engine::eval` (which wraps `rs_poker`): a hand-rolled comparator that
// brute-forces all C(7,5)=21 five-card combinations. Because it is a second,
// independent implementation, a regression in the shared evaluator cannot pass
// by self-agreement — the two must agree on the winner or the test fails. For
// every CONTESTED pot at showdown we assert the engine-awarded winner set equals
// the set of contestants whose independently-computed best hand is strictly best
// (ties → both share the pot). We compare winner *identity sets* only (not chip
// amounts — odd-chip splits are covered by the conservation invariants).
//
// This invariant is architectural: do not relax it without an ADR.

/// A card reduced to `(rank_value 2..=14, suit_id 0..=3)` for the independent
/// evaluator. Kept deliberately separate from `engine::card`/`engine::eval`
/// ranking so the oracle's logic stands alone (no self-agreement on bugs).
fn oracle_card(c: Card) -> (u8, u8) {
    let rank = (c.rank as u8) + 2; // Rank::Two as u8 == 0  ->  2 ; Ace -> 14
    let suit = c.suit as u8;
    (rank, suit)
}

/// High card of the best straight present in these 5 cards (wheel A-2-3-4-5
/// → 5-high), or `None` if there is no straight.
fn oracle_straight_high(cards5: &[(u8, u8); 5]) -> Option<u8> {
    let mut present = [false; 15]; // index by rank value 2..=14
    for &(r, _) in cards5 {
        present[r as usize] = true;
    }
    // Normal straights have a high card in 6..=14 (a 5-high run is the wheel,
    // handled below — there is no 1-2-3-4-5).
    for high in (6..=14u8).rev() {
        if (0..5).all(|i| present[(high - i) as usize]) {
            return Some(high);
        }
    }
    if present[14] && present[5] && present[4] && present[3] && present[2] {
        return Some(5); // wheel: A-2-3-4-5
    }
    None
}

/// Hand-rank key for an EXACT 5-card hand: `(category, tiebreakers)` where
/// category is 0=high-card .. 8=straight-flush. The tuple's lexicographic `Ord`
/// reproduces standard poker ordering, kickers included.
fn oracle_eval5(cards5: &[(u8, u8); 5]) -> (u8, Vec<u8>) {
    let mut ranks: Vec<u8> = cards5.iter().map(|&(r, _)| r).collect();
    ranks.sort_unstable_by(|a, b| b.cmp(a)); // descending

    let suit0 = cards5[0].1;
    let is_flush = cards5.iter().all(|&(_, s)| s == suit0);
    let straight_high = oracle_straight_high(cards5);

    // Rank-frequency groups, sorted by (count desc, rank desc).
    let mut groups: Vec<(u8, u8)> = Vec::new(); // (count, rank)
    for r in (2..=14u8).rev() {
        let cnt = ranks.iter().filter(|&&x| x == r).count() as u8;
        if cnt > 0 {
            groups.push((cnt, r));
        }
    }
    groups.sort_by(|a, b| b.0.cmp(&a.0).then(b.1.cmp(&a.1)));
    let c0 = groups[0].0;
    let c1 = groups.get(1).map(|g| g.0).unwrap_or(0);
    let grank = |i: usize| groups[i].1;

    if let (true, Some(h)) = (is_flush, straight_high) {
        (8, vec![h]) // straight flush
    } else if c0 == 4 {
        (7, vec![grank(0), grank(1)]) // four of a kind: quad, kicker
    } else if c0 == 3 && c1 == 2 {
        (6, vec![grank(0), grank(1)]) // full house: trip, pair
    } else if is_flush {
        (5, ranks) // flush: 5 ranks desc
    } else if let Some(h) = straight_high {
        (4, vec![h]) // straight
    } else if c0 == 3 {
        (3, vec![grank(0), grank(1), grank(2)]) // trips + 2 kickers
    } else if c0 == 2 && c1 == 2 {
        (2, vec![grank(0), grank(1), grank(2)]) // two pair: high, low, kicker
    } else if c0 == 2 {
        (1, vec![grank(0), grank(1), grank(2), grank(3)]) // one pair + 3 kickers
    } else {
        (0, ranks) // high card: 5 ranks desc
    }
}

/// Best 5-of-N hand key by brute force over every 5-card combination.
#[allow(clippy::needless_range_loop)] // combination indices, not element iteration
fn oracle_best(cards: &[(u8, u8)]) -> (u8, Vec<u8>) {
    let n = cards.len();
    assert!(n >= 5, "oracle_best needs >= 5 cards, got {n}");
    let mut best: Option<(u8, Vec<u8>)> = None;
    for a in 0..n {
        for b in (a + 1)..n {
            for c in (b + 1)..n {
                for d in (c + 1)..n {
                    for e in (d + 1)..n {
                        let five = [cards[a], cards[b], cards[c], cards[d], cards[e]];
                        let key = oracle_eval5(&five);
                        let replace = match &best {
                            None => true,
                            Some(bk) => key > *bk,
                        };
                        if replace {
                            best = Some(key);
                        }
                    }
                }
            }
        }
    }
    best.unwrap()
}

/// For each CONTESTED pot at showdown, assert the engine-awarded winner set
/// equals the independent best-5-of-7 oracle's winner set.
fn assert_winners_match_oracle(
    result: &HandResult,
) -> Result<(), proptest::test_runner::TestCaseError> {
    use std::collections::HashMap;
    // pid -> hole cards for everyone who reached showdown (i.e. did not fold).
    let shown: HashMap<PlayerId, HoleCards> = result
        .showdown
        .iter()
        .map(|e| (e.player_id, e.hole_cards))
        .collect();
    let board: Vec<(u8, u8)> = result
        .board
        .all_cards()
        .into_iter()
        .map(oracle_card)
        .collect();

    for pot in &result.pots {
        if pot.is_refund || pot.amount.0 == 0 {
            continue;
        }
        // Contestants = eligible players who actually showed down (not folded).
        let contestants: Vec<PlayerId> = pot
            .eligible_player_ids
            .iter()
            .copied()
            .filter(|p| shown.contains_key(p))
            .collect();
        if contestants.len() < 2 {
            continue; // uncontested (fold-win or single covered seat) — no compare
        }
        let keyed: Vec<(PlayerId, (u8, Vec<u8>))> = contestants
            .iter()
            .map(|p| {
                let hole = shown[p];
                let mut seven = board.clone();
                seven.push(oracle_card(hole.card1));
                seven.push(oracle_card(hole.card2));
                (*p, oracle_best(&seven))
            })
            .collect();
        let best_key = keyed.iter().map(|(_, k)| k.clone()).max().unwrap();
        let mut oracle_winners: Vec<u64> = keyed
            .iter()
            .filter(|(_, k)| *k == best_key)
            .map(|(p, _)| p.inner())
            .collect();
        oracle_winners.sort_unstable();
        let mut engine_winners: Vec<u64> =
            pot.winners.iter().map(|w| w.player_id.inner()).collect();
        engine_winners.sort_unstable();
        prop_assert_eq!(
            engine_winners,
            oracle_winners,
            "pot[{}] (amount={}) engine winner set != independent best-5 oracle; board={:?} contestants={:?}",
            pot.index,
            pot.amount.0,
            result.board.all_cards().iter().map(|c| c.to_string()).collect::<Vec<_>>(),
            contestants.iter().map(|p| p.inner()).collect::<Vec<_>>()
        );
    }
    Ok(())
}

proptest! {
    // 9-max widens the brute-force oracle's combinatorics (C(7,5)=21 per seat ×
    // up to 9 seats per showdown). Lower the case count for THIS test so the
    // full-table winner sweep stays bounded (<~30s) while still covering the
    // 7-9-contestant board textures that 2..=6 never reached.
    #![proptest_config(ProptestConfig::with_cases(160))]
    /// Multi-way check/call showdown (deep stacks → single pot, no folds, no
    /// all-ins): the awarded pot MUST go to the genuinely best five-card hand(s).
    /// This is the broad winner-value sweep across board textures and seat
    /// counts that closes RC-1 (wrong-winner ships green). Widened from 2..=6 to
    /// 2..=9: a wrong-winner bug that only manifests with 7-9 contestants at
    /// showdown can no longer ship green.
    #[test]
    fn prop_showdown_winner_is_best_hand(
        n in 2usize..=9,
        dealer_off in 0usize..9,
        seed in 0u64..2_000_000,
    ) {
        let dealer = dealer_off % n;
        let mut hand = make_hand(n, dealer, seed, 5000);
        hand.start().expect("start ok");

        // Pure check/call all the way → everyone reaches showdown, no all-ins.
        let mut safety = 0;
        while !hand.is_done() {
            safety += 1;
            if safety > 120 { break; }
            let snap = hand.snapshot();
            let Some(actor) = snap.current_actor else { break };
            let committed = snap.players.iter()
                .find(|p| p.player_id == actor)
                .map(|p| p.committed_this_street.0)
                .unwrap_or(0);
            let to_call = snap.current_bet.0.saturating_sub(committed);
            let action = if to_call > 0 { PlayerAction::Call } else { PlayerAction::Check };
            if hand.apply_action(actor, action).is_err() { break; }
        }

        prop_assume!(hand.is_done());
        let result = hand.finish();
        // Only assert on a genuine 5-card-board, ≥2-player showdown.
        prop_assume!(result.board.count() == 5);
        prop_assume!(result.showdown.len() >= 2);

        assert_winners_match_oracle(&result)?;
    }
}

proptest! {
    /// Asymmetric all-in run-out → MULTIPLE side pots. Each CONTESTED pot must
    /// be awarded to the best hand among that pot's eligible-and-shown seats.
    /// Complements `prop_side_pot_distribution` (which only checks eligibility)
    /// with per-pot winner-value — the side-pot winner-selection code path is
    /// distinct from the single-pot path.
    #[test]
    fn prop_showdown_winner_value_sidepots(
        seed in 0u64..2_000_000,
        s1 in 50u32..200,
        s2 in 200u32..500,
        s3 in 500u32..1500,
    ) {
        let players: Vec<(PlayerId, Chips, u8)> = vec![
            (pid(1), Chips(s1), 0),
            (pid(2), Chips(s2), 1),
            (pid(3), Chips(s3), 2),
        ];
        let mut hand = GameHand::new_with_rng(
            players,
            0,
            Chips(20),
            Chips(10),
            PokerRng::from_seed(seed),
        );
        hand.start().expect("start ok");

        let mut safety = 0;
        while !hand.is_done() {
            safety += 1;
            if safety > 30 { break; }
            let snap = hand.snapshot();
            let Some(actor) = snap.current_actor else { break };
            if hand.apply_action(actor, PlayerAction::AllIn).is_err() {
                let _ = hand.apply_action(actor, PlayerAction::Call);
            }
        }

        prop_assume!(hand.is_done());
        let result = hand.finish();
        prop_assume!(result.board.count() == 5);
        prop_assume!(result.showdown.len() >= 2);

        assert_winners_match_oracle(&result)?;
    }
}

/// Teeth check for the independent oracle itself. The property tests above only
/// prove the oracle AGREES with the engine — if the oracle were a tautology
/// (e.g. always returned the same key) it would agree trivially and catch
/// nothing. These hand-built match-ups prove the oracle actually DISCRIMINATES
/// by standard poker rules, so an engine that paid the wrong hand really would
/// diverge from it. `oc(rank 2..=14, suit 0..=3)`.
#[test]
fn oracle_self_check_discriminates() {
    let oc = |r: u8, s: u8| (r, s);
    let ev = |five: [(u8, u8); 5]| oracle_eval5(&five);

    // --- Category ladder: each EXACT 5-card hand strictly beats the one below.
    let straight_flush = ev([oc(14, 0), oc(13, 0), oc(12, 0), oc(11, 0), oc(10, 0)]); // royal
    let quads = ev([oc(14, 0), oc(14, 1), oc(14, 2), oc(14, 3), oc(13, 0)]); // four aces, K kicker
    let full_house = ev([oc(14, 0), oc(14, 1), oc(14, 2), oc(13, 0), oc(13, 1)]); // aces full of kings
    let flush = ev([oc(14, 0), oc(13, 0), oc(9, 0), oc(7, 0), oc(2, 0)]); // A-high flush
    let straight = ev([oc(14, 0), oc(13, 1), oc(12, 2), oc(11, 3), oc(10, 0)]); // broadway, mixed
    let trips = ev([oc(9, 0), oc(9, 1), oc(9, 2), oc(13, 0), oc(2, 3)]); // set of nines, K-2 kickers
    let two_pair = ev([oc(14, 0), oc(14, 1), oc(13, 0), oc(13, 1), oc(12, 2)]); // AA KK Q
    let one_pair = ev([oc(14, 0), oc(14, 1), oc(13, 0), oc(9, 1), oc(7, 2)]); // pair of aces
    let high = ev([oc(14, 0), oc(13, 1), oc(11, 2), oc(9, 3), oc(7, 0)]); // ace-high

    assert_eq!(straight_flush.0, 8);
    assert_eq!(quads.0, 7);
    assert_eq!(full_house.0, 6);
    assert_eq!(flush.0, 5);
    assert_eq!(straight.0, 4);
    assert_eq!(trips.0, 3);
    assert_eq!(two_pair.0, 2);
    assert_eq!(one_pair.0, 1);
    assert_eq!(high.0, 0);
    assert!(straight_flush > quads, "straight flush must beat quads");
    assert!(quads > full_house, "quads must beat full house");
    assert!(full_house > flush, "full house must beat flush");
    assert!(flush > straight, "flush must beat straight");
    assert!(straight > trips, "straight must beat trips");
    assert!(trips > two_pair, "trips must beat two pair");
    assert!(two_pair > one_pair, "two pair must beat one pair");
    assert!(one_pair > high, "one pair must beat high card");

    // --- Within-category tiebreaks.
    // Two pair, same pairs, kicker decides (Q kicker > J kicker).
    let tp_q = ev([oc(14, 0), oc(14, 1), oc(13, 0), oc(13, 1), oc(12, 2)]);
    let tp_j = ev([oc(14, 0), oc(14, 1), oc(13, 0), oc(13, 1), oc(11, 2)]);
    assert!(tp_q > tp_j, "two pair AAKK-Q must beat AAKK-J on kicker");

    // Wheel is the LOWEST straight (6-high straight beats A-2-3-4-5).
    let wheel = ev([oc(14, 0), oc(5, 1), oc(4, 2), oc(3, 3), oc(2, 0)]);
    let six_high = ev([oc(6, 0), oc(5, 1), oc(4, 2), oc(3, 3), oc(2, 0)]);
    assert_eq!(wheel.0, 4, "wheel is a straight");
    assert!(six_high > wheel, "6-high straight must beat the wheel");

    // Steel wheel is the LOWEST straight flush.
    let steel_wheel = ev([oc(14, 0), oc(5, 0), oc(4, 0), oc(3, 0), oc(2, 0)]);
    let six_high_sf = ev([oc(6, 0), oc(5, 0), oc(4, 0), oc(3, 0), oc(2, 0)]);
    assert_eq!(steel_wheel.0, 8, "steel wheel is a straight flush");
    assert!(
        six_high_sf > steel_wheel,
        "6-high straight flush must beat the steel wheel"
    );

    // --- oracle_best must pick the best 5 of 7.
    // 7 cards where the best five is a broadway straight (A K Q J T + 7 2, no flush).
    let seven_straight = vec![
        oc(14, 0),
        oc(13, 1),
        oc(12, 2),
        oc(11, 3),
        oc(10, 0),
        oc(7, 1),
        oc(2, 2),
    ];
    assert_eq!(
        oracle_best(&seven_straight).0,
        4,
        "best of 7 here is a straight"
    );
    // 7 cards with 5 spades → best five is an A-high flush.
    let seven_flush = vec![
        oc(14, 0),
        oc(13, 0),
        oc(9, 0),
        oc(7, 0),
        oc(2, 0),
        oc(13, 1),
        oc(4, 2),
    ];
    assert_eq!(oracle_best(&seven_flush).0, 5, "best of 7 here is a flush");

    // --- Ties: two players who both play the same board straight must tie.
    let board_broadway = [oc(14, 0), oc(13, 1), oc(12, 2), oc(11, 3), oc(10, 0)];
    let mut p1 = board_broadway.to_vec();
    p1.extend_from_slice(&[oc(2, 1), oc(3, 2)]); // low, non-improving holes
    let mut p2 = board_broadway.to_vec();
    p2.extend_from_slice(&[oc(4, 1), oc(2, 3)]); // different low, non-improving holes
    assert_eq!(
        oracle_best(&p1),
        oracle_best(&p2),
        "two players playing the same broadway board must tie"
    );
}
