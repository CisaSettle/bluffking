use super::*;
use crate::bot::preflop_blueprint::snapshot_with;
use engine::{GameHand, PlayerId, PokerRng};
use rand::SeedableRng;
use std::time::Duration;

fn play(game: &mut GameHand, action: PlayerAction) {
    game.apply_action(game.snapshot().current_actor.unwrap(), action)
        .unwrap();
}

fn game_at_river(seed: u8) -> GameHand {
    let mut game = GameHand::new_with_rng(
        vec![
            (PlayerId(100), Chips(1000), 0),
            (PlayerId(101), Chips(1000), 1),
        ],
        0,
        Chips(10),
        Chips(5),
        PokerRng::from_seed_bytes([seed; 32]),
    );
    game.start().unwrap();
    play(&mut game, PlayerAction::Raise { amount: Chips(25) });
    play(&mut game, PlayerAction::Call);
    while game.snapshot().street != Street::River {
        play(&mut game, PlayerAction::Check);
    }
    game
}

fn input(game: &GameHand) -> Option<(Request, Query)> {
    snapshot_with(game, |_| {}, |ctx| Request::from_context(ctx))
}

#[test]
fn ordinary_river_reuses_public_policy_across_actions_and_private_queries() {
    let mut game = game_at_river(7);
    let (request, mut query) = input(&game).unwrap();
    let started = Instant::now();
    let solved = request
        .clone()
        .solve(started + Duration::from_secs(3))
        .unwrap();
    let (iterations, gaps) = solved.quality();
    eprintln!(
        "ordinary HU river kernel+rank+line fit {:?}, iterations {iterations}, gaps {gaps:?}",
        started.elapsed()
    );
    assert!(gaps.iter().all(|&g| g < 0.0025));
    assert_eq!(solved.rows.len(), 1081);
    let weights_for = |node| {
        solved
            .likelihoods(node)
            .filter(|(r, _)| r.weight[node % 2] > 0.001)
            .min_by(|a, b| a.1.total_cmp(&b.1))
            .map(|(r, _)| HoleCards::new(card(r.cards[0]), card(r.cards[1])))
            .unwrap()
    };
    // Root must remain identical even if a different hero hand is queried.
    let other = snapshot_with(
        &game,
        |_| {},
        |ctx| {
            ctx.hole = weights_for(OOP_OPEN);
            Request::from_context(ctx).unwrap().0
        },
    );
    assert!(request.same_root(&other));
    let mut rng = StdRng::seed_from_u64(4);
    query.hole = weights_for(OOP_OPEN);
    let action = solved.decision(query, &mut rng, None).unwrap().0;
    assert!(game
        .validate_action(game.snapshot().current_actor.unwrap(), &action)
        .is_ok());
    play(&mut game, PlayerAction::Check);
    let (next, mut query) = input(&game).unwrap();
    assert!(request.same_root(&next));
    assert_eq!(query.node, IP_OPEN);
    query.hole = weights_for(IP_OPEN);
    let action = solved.decision(query, &mut rng, None).unwrap().0;
    assert!(game
        .validate_action(game.snapshot().current_actor.unwrap(), &action)
        .is_ok());
    let mut off_tree = game.clone();
    play(&mut off_tree, PlayerAction::Raise { amount: Chips(26) });
    assert!(input(&off_tree).is_none());
    play(&mut game, PlayerAction::Raise { amount: Chips(25) });
    let (next, mut query) = input(&game).unwrap();
    assert!(request.same_root(&next));
    assert_eq!(query.node, OOP_CALL);
    // An OOP hand that reaches the observed check can defend this bet.
    query.hole = weights_for(OOP_OPEN);
    let action = solved.decision(query, &mut rng, None).unwrap().0;
    assert!(matches!(action, PlayerAction::Call | PlayerAction::Fold));
    assert!(game
        .validate_action(game.snapshot().current_actor.unwrap(), &action)
        .is_ok());
    let different = game_at_river(8);
    let (elsewhere, query) = input(&different).unwrap();
    assert!(!request.same_root(&elsewhere));
    assert!(solved.decision(query, &mut rng, None).is_none());
}
#[test]
fn mixed_real_river_policy_requires_the_observed_responder_and_price_to_deviate() {
    use crate::bot::OpponentStats;
    use std::collections::HashMap;
    let (req, _) = input(&game_at_river(7)).unwrap();
    let mut solved = req.solve(Instant::now() + Duration::from_secs(3)).unwrap();
    let node = OOP_OPEN;
    let villain = solved.root.seats[1];
    let owner = uuid::Uuid::from_u128(9);
    let stats = |fold, count, n| {
        let mut s = OpponentStats {
            profile_owner: Some(owner),
            ..Default::default()
        };
        for _ in 0..count {
            s.observe_river_half_pot(n, fold);
        }
        HashMap::from([(villain, s)])
    };
    let folds = stats(true, 32, 2);
    let continues = stats(false, 32, 2);
    let few = stats(true, 19, 2);
    let wrong_n = stats(true, 32, 3);
    let read = |stats, user| {
        let mut model = OpponentModel::new(stats);
        model.river_responder = Some((villain, user));
        model
    };
    fn result(
        solved: &Solved,
        hole: HoleCards,
        seed: u64,
        model: Option<&OpponentModel<'_>>,
    ) -> Option<(PlayerAction, &'static str)> {
        solved.decision(
            Query {
                hole,
                node: OOP_OPEN,
                root: solved.root.clone(),
            },
            &mut StdRng::seed_from_u64(seed),
            model,
        )
    }
    // Find actual mixed pockets in this frozen full-range solution; draw
    // between adjacent frequencies so an overlay can change the action.
    let mut examples = [None; 3];
    for (i, (row, p)) in solved.likelihoods(node).enumerate() {
        if row.weight[0] <= 0. || !(0.10..=0.90).contains(&p) {
            continue;
        }
        let hole = HoleCards::new(card(row.cards[0]), card(row.cards[1]));
        for (j, model, label, shift) in [
            (0, read(&folds, owner), "hu_river_overfolder_bluff", 0.1),
            (
                1,
                read(&continues, owner),
                "hu_river_station_restrain",
                -0.1,
            ),
            (2, read(&continues, owner), "hu_river_station_value", 0.1),
        ] {
            if examples[j].is_some() {
                continue;
            }
            let seed = (0..1000u64)
                .find(|&seed| {
                    let draw: f64 = StdRng::seed_from_u64(seed).gen();
                    (draw < p) != (draw < p + shift)
                })
                .unwrap();
            if result(&solved, hole, seed, Some(&model)).unwrap().1 == label {
                assert_ne!(
                    result(&solved, hole, seed, None).unwrap().0,
                    result(&solved, hole, seed, Some(&model)).unwrap().0
                );
                examples[j] = Some((i, seed));
            }
        }
        if examples.iter().all(Option::is_some) {
            break;
        }
    }
    assert!(examples.iter().all(Option::is_some), "{examples:?}");
    let (i, seed) = examples[0].unwrap();
    let hole = HoleCards::new(card(solved.rows[i].cards[0]), card(solved.rows[i].cards[1]));
    let baseline = result(&solved, hole, seed, None).unwrap();
    for model in [
        read(&folds, uuid::Uuid::from_u128(10)),
        read(&few, owner),
        read(&wrong_n, owner),
    ] {
        assert_eq!(result(&solved, hole, seed, Some(&model)).unwrap(), baseline);
    }
    let original_bet = solved.root.bet;
    Arc::make_mut(&mut solved.root).bet = original_bet + 1;
    assert_eq!(
        result(&solved, hole, seed, Some(&read(&folds, owner))).unwrap(),
        result(&solved, hole, seed, None).unwrap()
    );
    let root = Arc::make_mut(&mut solved.root);
    root.bet = original_bet;
    root.stacks[1] = original_bet;
    assert_eq!(
        result(&solved, hole, seed, Some(&read(&folds, owner))).unwrap(),
        result(&solved, hole, seed, None).unwrap()
    );
}

#[test]
fn folded_river_uses_previous_folder_and_authenticated_cached_path() {
    check_folded_river(Duration::from_secs(3));
}

#[test]
#[ignore = "explicit measurement with the live worker deadline"]
fn folded_river_worker_budget() {
    check_folded_river(Duration::from_millis(400));
}

fn check_folded_river(budget: Duration) {
    for (dealt, button_folds) in [(3, true), (3, false), (4, true), (4, false)] {
        let players = (0..dealt)
            .map(|s| (PlayerId(100 + u64::from(s)), Chips(1000), s))
            .collect();
        let mut game = GameHand::new_with_rng(
            players,
            0,
            Chips(10),
            Chips(5),
            PokerRng::from_seed_bytes([7; 32]),
        );
        game.start().unwrap();
        if dealt == 4 {
            play(&mut game, PlayerAction::Fold);
        }
        if button_folds {
            play(&mut game, PlayerAction::Fold);
        }
        play(&mut game, PlayerAction::Raise { amount: Chips(25) });
        if !button_folds {
            play(&mut game, PlayerAction::Fold);
        }
        play(&mut game, PlayerAction::Call);
        while game.snapshot().street != Street::River {
            play(&mut game, PlayerAction::Check);
        }
        let start = Instant::now();
        let (req, query) = input(&game).unwrap();
        let solved = req.clone().solve(start + budget).unwrap();
        eprintln!(
            "{dealt} dealt, two live, button folds={button_folds} {:?} cert {:?}",
            start.elapsed(),
            solved.quality()
        );
        assert_eq!(solved.root.pot, if button_folds { 50 } else { 55 });
        assert!(solved.quality().1.iter().all(|x| *x < 0.0025));
        let mut rng = StdRng::seed_from_u64(1);
        assert_eq!(
            solved.decision(query, &mut rng, None).unwrap().1,
            "hu_river_folded_range_model"
        );
        play(&mut game, PlayerAction::Check);
        let (later, q) = input(&game).unwrap();
        assert!(req.same_root(&later));
        assert!(solved.decision(q, &mut rng, None).is_some());
    }
}
