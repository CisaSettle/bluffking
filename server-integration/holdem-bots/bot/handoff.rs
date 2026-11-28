//! Preflop range factors for the flop under the frozen preflop policy.
//! The factors include every dealt seat: folding conveys card information too.
//! A solver must condition on mutually disjoint holdings and the public board.

use super::{
    contestable_cap, hand_class, policy_for, Chips, DecisionContext, PublicAction, Street, CLASSES,
};
use engine::{BettingRound, HoleCards, PlayerAction, PlayerId};

pub(in crate::bot) struct FlopPlayer {
    pub seat: u8,
    pub folded: bool,
    /// Chips behind at the start of the flop, before its first action.
    pub stack: u32,
    /// Likelihood of this seat's own observed actions given a specific combo.
    pub weights: [f32; CLASSES],
}

pub(in crate::bot) struct FlopReach {
    /// Original preflop action order, including seats that folded.
    pub players: Vec<FlopPlayer>,
    pub dealer: u8,
    pub pot: u32,
}

impl FlopReach {
    pub(in crate::bot) fn weight(&self, seat: u8, hole: HoleCards) -> Option<f64> {
        if hole.card1 == hole.card2 {
            return None;
        }
        let (high, low, suited) = hand_class(hole);
        let (row, col) = if suited { (high, low) } else { (low, high) };
        let player = self.players.iter().find(|p| p.seat == seat)?;
        Some(f64::from(player.weights[row as usize * 13 + col as usize]))
    }
}

fn pid(seat: u8) -> PlayerId {
    PlayerId(u64::from(seat))
}

/// Replay an untrusted/incomplete projection with the same public rules used in
/// the live hand. Check zero-chip checks as well as wagers: a missing check can
/// otherwise leave sizes and stacks unchanged but imply a different range.
fn apply(round: &mut BettingRound, pot_at_start: u32, fact: &PublicAction) -> Option<()> {
    let id = pid(fact.seat);
    let before = round.current_bet().0;
    let minimum = round.min_raise_to().0;
    if fact.pot_before != pot_at_start.checked_add(round.pot_total().0)?
        || fact.to_call_before != round.to_call(id).0
        || matches!(fact.action, PlayerAction::Blind { .. })
    {
        return None;
    }
    let paid = round.apply_action(id, &fact.action).ok()?;
    let after = round.current_bet().0;
    (paid.0 == fact.amount
        && fact.committed_to == round.player_contributed(id).0
        && fact.increased_bet == (after > before)
        && fact.full_raise == (after > before && after >= minimum))
        .then_some(())
}

pub(in crate::bot) fn flop_reach(ctx: &DecisionContext<'_>) -> Option<FlopReach> {
    let seats = ctx.table.seats;
    let n = seats.len();
    let streets = [Street::Preflop, Street::Flop, Street::Turn, Street::River];
    let street = streets.iter().position(|&s| s == ctx.street)?;
    if !ctx.plain_cash
        || ctx.has_straddle
        || !(2..=9).contains(&n)
        || street == 0
        || ctx.board.count() != street + 2
        || ctx.board.turn.is_some() != (street >= 2)
        || ctx.board.river.is_some() != (street == 3)
        || ctx.big_blind < 2
        || !ctx.big_blind.is_multiple_of(2)
    {
        return None;
    }
    let board = ctx.board.all_cards();
    if board
        .iter()
        .enumerate()
        .any(|(i, c)| board[..i].contains(c))
    {
        return None;
    }
    let mut facts = ctx.public_history?;
    let dealer = seats.iter().position(|s| s.seat == ctx.table.dealer_seat)?;
    let (small, big) = engine::blind_positions(dealer, n);
    if ctx.sb_seat != seats[small].seat
        || ctx.bb_seat != seats[big].seat
        || seats
            .iter()
            .enumerate()
            .any(|(i, s)| seats[..i].iter().any(|p| p.seat == s.seat))
    {
        return None;
    }
    let unit = ctx.big_blind / 2;
    let ordered: Vec<_> = (0..n).map(|i| &seats[(big + 1 + i) % n]).collect();
    // stack now + every logged payment + the ordinary two blinds. The replay
    // must reconcile these hypotheses against each recorded pot and price.
    let mut original: Vec<_> = ordered.iter().map(|s| s.stack).collect();
    for fact in facts {
        let i = ordered.iter().position(|s| s.seat == fact.seat)?;
        original[i] = original[i].checked_add(fact.amount)?;
    }
    let sb = n - 2;
    let bb = n - 1;
    original[sb] = original[sb].checked_add(unit)?;
    original[bb] = original[bb].checked_add(ctx.big_blind)?;
    original
        .iter()
        .try_fold(0u32, |acc, &qty| acc.checked_add(qty))?;
    let policy = policy_for(&original, unit)?;
    let cap = contestable_cap(&original)?;
    let mut round = BettingRound::new_preflop(
        ordered
            .iter()
            .zip(&original)
            .map(|(p, &s)| (pid(p.seat), Chips(s)))
            .collect(),
        0,
        &[
            (pid(ctx.sb_seat), Chips(unit)),
            (pid(ctx.bb_seat), Chips(ctx.big_blind)),
        ],
        Chips(ctx.big_blind),
    );
    let mut players: Vec<_> = ordered
        .iter()
        .map(|p| FlopPlayer {
            seat: p.seat,
            folded: false,
            stack: 0,
            weights: [1.0; CLASSES],
        })
        .collect();
    let mut next = Some(policy.root);
    let mut closing_call: Option<&super::HandoffCall> = None;
    while let Some((fact, rest)) = facts
        .split_first()
        .filter(|(f, _)| f.street == Street::Preflop)
    {
        let Some(index) = next else {
            // This reference row describes one observed BB call. It has no
            // executable decision node and cannot lead to another action.
            let call = closing_call.take()?;
            if fact.seat != ordered.get(call.actor)?.seat
                || fact.action != PlayerAction::Call
                || fact.to_call_before == 0
                || fact.amount != fact.to_call_before
                || call.weights.len() != CLASSES
            {
                return None;
            }
            apply(&mut round, 0, fact)?;
            if !round.is_done() {
                return None;
            }
            for (weight, &p) in players[call.actor].weights.iter_mut().zip(&call.weights) {
                *weight *= p;
            }
            facts = rest;
            continue;
        };
        let node = policy.nodes.get(index)?;
        let actor = node.actor;
        let depth = u16::try_from(original[actor].min(cap) / unit).ok()?;
        let edge = node.edge_index(fact, ordered[actor].seat, unit, original[actor], depth)?;
        apply(&mut round, 0, fact)?;
        for (class, weight) in players[actor].weights.iter_mut().enumerate() {
            // Match Node::sample, including normalization of serialized f32 rows.
            let total: f64 = node.strategy[class..]
                .iter()
                .step_by(CLASSES)
                .map(|&x| f64::from(x))
                .sum();
            if total <= 0.0 {
                return None;
            }
            *weight *= (f64::from(node.strategy[edge * CLASSES + class]) / total) as f32;
        }
        next = node.edges[edge].next;
        if next.is_none() && !round.is_done() {
            closing_call = policy
                .handoff_calls
                .iter()
                .find(|c| c.parent == index && c.edge == edge);
        }
        facts = rest;
    }
    // A missing child may also be a pruned internal node. Only the engine's
    // completed betting round with multiple live players certifies a flop.
    if !round.is_done() || next.is_some() {
        return None;
    }
    for (p, state) in players.iter_mut().zip(round.player_states()) {
        p.folded = state.folded;
        p.stack = state.stack.0;
    }
    if players.iter().filter(|p| !p.folded && p.stack > 0).count() < 2 {
        return None;
    }
    let flop = FlopReach {
        players,
        pot: round.pot_total().0,
        dealer: ctx.table.dealer_seat,
    };
    let mut pot = flop.pot;
    let mut stacks: Vec<_> = (1..=n)
        .map(|d| seats[(dealer + d) % n].seat)
        .filter_map(|s| flop.players.iter().find(|p| p.seat == s && !p.folded))
        .map(|p| (pid(p.seat), Chips(p.stack)))
        .collect();

    // The returned factors stop at the flop. Later public actions are replayed
    // only for input validation; the downstream model assigns their likelihoods.
    for &s in &streets[1..=street] {
        round = BettingRound::new(stacks, 0, Chips::ZERO, Chips(ctx.big_blind));
        while let Some((fact, rest)) = facts.split_first().filter(|(f, _)| f.street == s) {
            apply(&mut round, pot, fact)?;
            facts = rest;
        }
        pot = pot.checked_add(round.pot_total().0)?;
        if s != ctx.street && !round.is_done() {
            return None;
        }
        stacks = round
            .player_states()
            .iter()
            .filter(|p| !p.folded)
            .map(|p| (p.player_id, p.stack))
            .collect();
    }
    let hero = pid(ctx.seat);
    if !facts.is_empty()
        || pot != ctx.pot.0
        || round.current_player() != Some(hero)
        || ctx.stack_chips != seats.iter().find(|p| p.seat == ctx.seat)?.stack
        || usize::from(ctx.num_active) != seats.iter().filter(|p| !p.folded && !p.all_in).count()
        || round.current_bet() != ctx.current_bet
        || round.to_call(hero) != ctx.to_call
        || round.player_contributed(hero) != ctx.committed_this_street
        || round.current_player_can_raise() != ctx.can_raise
        || ctx.min_raise_to != ctx.can_raise.then(|| round.min_raise_to())
        || seats.iter().any(|p| {
            round
                .player_states()
                .iter()
                .find(|q| q.player_id == pid(p.seat))
                .map_or(!p.folded || p.committed_this_street != 0, |q| {
                    q.stack.0 != p.stack
                        || q.folded != p.folded
                        || q.all_in != p.all_in
                        || q.contributed.0 != p.committed_this_street
                })
        })
    {
        return None;
    }
    Some(flop)
}

#[cfg(test)]
pub(super) mod tests {
    use super::*;
    use crate::bot::{public_action_history, shark::tests::opening, SeatPublic};
    use engine::{card::parse_card, GameHand, PokerRng};
    use rand::{rngs::StdRng, SeedableRng};

    fn hand(n: usize, seed: u8) -> GameHand {
        let seats = (0..n)
            .map(|i| (PlayerId(i as u64 + 20), Chips(1000), i as u8))
            .collect();
        let mut game = GameHand::new_with_rng(
            seats,
            n / 2,
            Chips(10),
            Chips(5),
            PokerRng::from_seed_bytes([seed; 32]),
        );
        game.start().unwrap();
        game
    }

    fn play(game: &mut GameHand, action: PlayerAction) {
        game.apply_action(game.snapshot().current_actor.unwrap(), action)
            .unwrap();
    }

    fn snapshot(game: &GameHand, change: impl FnOnce(&mut Vec<PublicAction>)) -> Option<FlopReach> {
        snapshot_with(game, change, |ctx| flop_reach(ctx))
    }

    pub(in crate::bot) fn snapshot_with<R>(
        game: &GameHand,
        change: impl FnOnce(&mut Vec<PublicAction>),
        f: impl FnOnce(&mut DecisionContext<'_>) -> R,
    ) -> R {
        let snap = game.snapshot();
        let seat = |id: PlayerId| {
            snap.players
                .iter()
                .find(|p| p.player_id == id)
                .map(|p| p.seat)
        };
        let mut facts = public_action_history(
            [Street::Preflop, Street::Flop, Street::Turn, Street::River]
                .map(|s| (s, game.street_action_facts(s))),
            seat,
        )
        .unwrap();
        change(&mut facts);
        let players: Vec<_> = snap
            .players
            .iter()
            .map(|p| SeatPublic {
                seat: p.seat,
                stack: p.stack.0,
                folded: p.folded,
                all_in: p.all_in,
                committed_this_street: p.committed_this_street.0,
            })
            .collect();
        let mut rng = StdRng::seed_from_u64(1);
        let mut ctx = opening(&mut rng, &snap.board, &players);
        let street_actions: Vec<_> = game
            .current_street_action_facts()
            .into_iter()
            .map(|a| (seat(a.player_id).unwrap(), a.strategy_action()))
            .collect();
        ctx.street_actions = &street_actions;
        ctx.public_history = Some(&facts);
        ctx.big_blind = 10;
        ctx.table.dealer_seat = snap.dealer_seat;
        let (small, big) = engine::blind_positions(
            players
                .iter()
                .position(|p| p.seat == snap.dealer_seat)
                .unwrap(),
            players.len(),
        );
        ctx.sb_seat = players[small].seat;
        ctx.bb_seat = players[big].seat;
        ctx.seat = seat(snap.current_actor.unwrap()).unwrap();
        ctx.hole = snap
            .players
            .iter()
            .find(|p| p.seat == ctx.seat)
            .unwrap()
            .hole_cards
            .unwrap();
        let r = game.betting_round_ref().unwrap();
        ctx.current_bet = snap.current_bet;
        ctx.min_raise_to = snap.min_raise_to;
        ctx.to_call = r.to_call(snap.current_actor.unwrap());
        ctx.can_raise = r.current_player_can_raise();
        let hero = players.iter().find(|p| p.seat == ctx.seat).unwrap();
        ctx.num_active = players.iter().filter(|p| !p.folded && !p.all_in).count() as u8;
        ctx.committed_this_street = Chips(hero.committed_this_street);
        ctx.stack_chips = hero.stack;
        ctx.street = snap.street;
        ctx.pot = snap.pot;
        f(&mut ctx)
    }

    #[test]
    fn handoff_transfers_both_mixes_including_final_call_once() {
        let candidate = |seed| {
            let mut game = hand(2, seed);
            play(&mut game, PlayerAction::Raise { amount: Chips(25) });
            play(&mut game, PlayerAction::Raise { amount: Chips(75) });
            play(&mut game, PlayerAction::Call);
            game
        };
        let mut game = candidate(91);
        let actual = snapshot(&game, |_| {}).unwrap();
        assert_eq!(actual.pot, 150);
        assert!(actual.players.iter().all(|p| p.stack == 925 && !p.folded));
        let other = snapshot(&candidate(51), |_| {}).unwrap();
        for (a, b) in actual.players.iter().zip(&other.players) {
            assert_eq!(a.weights, b.weights);
        }
        let source = &*super::super::HU100;
        let mut node = &source.nodes[source.root];
        let mut expected = [[1.0f64; CLASSES]; 2];
        for action in [
            super::super::Move::Raise(5),
            super::super::Move::Raise(15),
            super::super::Move::Call,
        ] {
            let (idx, edge) = node
                .edges
                .iter()
                .enumerate()
                .find(|(_, e)| e.play == action)
                .unwrap();
            for (c, expected) in expected[node.actor].iter_mut().enumerate() {
                *expected *= f64::from(node.strategy[idx * CLASSES + c])
                    / node.strategy[c..]
                        .iter()
                        .step_by(CLASSES)
                        .map(|&p| f64::from(p))
                        .sum::<f64>();
            }
            if let Some(next) = edge.next {
                node = &source.nodes[next];
            }
        }
        for (seat, index) in [(actual.dealer, 0), (1 - actual.dealer, 1)] {
            let player = actual.players.iter().find(|p| p.seat == seat).unwrap();
            for (e, &a) in expected[index].iter().zip(&player.weights) {
                assert!((e - f64::from(a)).abs() < 1e-7);
            }
            let ah = parse_card("Ah").unwrap();
            for (other, class) in [("As", 168), ("Kh", 12 * 13 + 11), ("Kc", 11 * 13 + 12)] {
                assert_eq!(
                    actual.weight(seat, HoleCards::new(ah, parse_card(other).unwrap())),
                    Some(f64::from(player.weights[class]))
                );
            }
        }
        // Real actions on later streets change the pot but not the flop prior.
        play(&mut game, PlayerAction::Check);
        play(&mut game, PlayerAction::Raise { amount: Chips(75) });
        play(&mut game, PlayerAction::Call);
        play(&mut game, PlayerAction::Check);
        let later = snapshot(&game, |_| {}).unwrap();
        assert_eq!(later.pot, actual.pot);
        for (a, b) in actual.players.iter().zip(later.players) {
            assert_eq!(a.weights, b.weights);
        }
        assert!(snapshot(&game, |h| {
            h.retain(|a| !(a.street == Street::Flop && a.action == PlayerAction::Check))
        })
        .is_none());
        assert!(snapshot(&game, |h| { h[1].full_raise = false }).is_none());
        assert!(snapshot(&game, |h| { h[2].pot_before += 1 }).is_none());
        assert!(snapshot(&game, |h| {
            h.remove(2);
        })
        .is_none());
    }

    fn open_called_by_every_remaining_seat(dealt: usize, live: usize, open: u32) -> GameHand {
        let mut game = hand(dealt, 38);
        for _ in 0..dealt - live {
            play(&mut game, PlayerAction::Fold);
        }
        play(
            &mut game,
            PlayerAction::Raise {
                amount: Chips(open),
            },
        );
        for _ in 1..live {
            play(&mut game, PlayerAction::Call);
        }
        assert_eq!(game.snapshot().street, Street::Flop);
        game
    }

    #[test]
    fn handoff_keeps_folded_blocker_factors_at_every_dealt_count() {
        // At 100 BB the five-dealt export retains both 2.5 and 3 BB opens
        // into two callers after two folds.
        for (n, open) in (2..=9).map(|n| (n, 25)).chain([(5, 30)]) {
            let live = n.min(3);
            let game = open_called_by_every_remaining_seat(n, live, open);
            let reach = snapshot(&game, |_| {}).unwrap_or_else(|| {
                panic!("expected an exported line for {n} dealt, {live} live, opening to {open}")
            });
            assert_eq!(reach.players.len(), n);
            assert_eq!(reach.pot, open * live as u32);
            // The handoff preserves preflop action order and a nonuniform
            // factor even for the early seats that folded.
            for (seat, p) in reach.players.iter().enumerate() {
                assert_eq!(p.folded, seat < n - live);
                assert!(p
                    .weights
                    .iter()
                    .all(|w| w.is_finite() && (0.0..=1.0).contains(w)));
                assert!(p.weights.iter().any(|w| *w > 0.05));
                assert!(p.weights.iter().any(|w| *w < 0.95));
            }
        }
    }

    #[test]
    fn four_live_observed_bb_call_reaches_the_flop_decision_without_admitting_bb_policy() {
        use crate::bot::{all_in::AllInPublic, table_decision, OpponentModel};
        let stats = std::collections::HashMap::new();
        let model = OpponentModel::new(&stats);
        // Two public prefixes in the equal 100 BB five-hand asset.
        // Compare a class from each independently re-exported BB call row.
        for (fold, open, ref_weight) in [(0, 30, 0.1625581), (1, 25, 0.86257404)] {
            let mut game = hand(5, 38);
            for i in 0..4 {
                let a = if i == fold {
                    PlayerAction::Fold
                } else if i == 1 - fold {
                    PlayerAction::Raise {
                        amount: Chips(open),
                    }
                } else {
                    PlayerAction::Call
                };
                play(&mut game, a);
            }
            snapshot_with(
                &game,
                |_| {},
                |ctx| {
                    assert_eq!(ctx.seat, ctx.bb_seat);
                    assert!(super::super::decision(ctx).is_none());
                    assert_eq!(table_decision(ctx, Some(&model)).2, "range_policy");
                },
            );
            let mut folded = game.clone();
            play(&mut folded, PlayerAction::Fold);
            assert_eq!(folded.snapshot().street, Street::Flop);
            assert!(snapshot(&folded, |_| {}).is_none());
            let mut raised = game.clone();
            play(
                &mut raised,
                PlayerAction::Raise {
                    amount: Chips(open * 2),
                },
            );
            for _ in 0..3 {
                play(&mut raised, PlayerAction::Call);
            }
            assert_eq!(raised.snapshot().street, Street::Flop);
            assert!(snapshot(&raised, |_| {}).is_none());

            play(&mut game, PlayerAction::Call);
            let reach = snapshot(&game, |_| {}).unwrap();
            assert_eq!(reach.pot, 4 * open);
            assert_eq!(reach.players.len(), 5);
            for (i, p) in reach.players.iter().enumerate() {
                assert_eq!(p.folded, i == fold);
                assert_eq!(p.stack, if p.folded { 1000 } else { 1000 - open });
                assert!(p.weights.iter().all(|w| (0.0..=1.0).contains(w)));
                assert!(p.weights.iter().any(|w| *w < 0.1));
                assert!(p.weights.iter().any(|w| *w > 0.1));
            }
            assert!((reach.players[4].weights[91] - ref_weight).abs() < 1e-6);
            assert!(snapshot(&game, |h| h.last_mut().unwrap().amount -= 1).is_none());

            // Verify the ordinary entry point consumes the reference on the
            // next live decision after an actual flop shove. The decision's
            // source remains the modeled range policy.
            play(&mut game, PlayerAction::AllIn);
            let snap = game.snapshot();
            snapshot_with(
                &game,
                |_| {},
                |ctx| {
                    ctx.all_in_public = Some(AllInPublic {
                        contributions: snap
                            .players
                            .iter()
                            .map(|p| (p.seat, 1000 - p.stack.0))
                            .collect(),
                        shared_ante: None,
                        squid: None,
                    });
                    let (action, _, source) = table_decision(ctx, Some(&model));
                    assert_eq!(source, "range_policy");
                    assert!(ctx.all_in_evidence.unwrap().blueprint_reach);
                    game.validate_action(snap.current_actor.unwrap(), &action)
                        .unwrap();
                },
            );
        }
    }

    #[test]
    fn handoff_rejects_real_flops_beyond_pruned_or_missing_preflop_actions() {
        // Both 2.5 BB opens with three callers, and the five-dealt 2 BB
        // opening with two callers, have a pruned child before the BB acts.
        for (dealt, live, open) in [(4, 4, 25), (5, 4, 25), (5, 3, 20)] {
            let game = open_called_by_every_remaining_seat(dealt, live, open);
            assert!(
                snapshot(&game, |_| {}).is_none(),
                "{dealt} dealt, {live} live, opening to {open}"
            );
        }
        let mut game = hand(2, 34);
        play(&mut game, PlayerAction::Raise { amount: Chips(31) });
        play(&mut game, PlayerAction::Call);
        assert!(snapshot(&game, |_| {}).is_none());
    }
}
