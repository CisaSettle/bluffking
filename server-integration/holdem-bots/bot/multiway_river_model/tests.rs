use super::*;
use crate::bot::preflop_blueprint::snapshot_with;
use engine::{GameHand, PlayerId, PokerRng};
use rand::SeedableRng;
use std::time::Duration;

fn play(game: &mut GameHand, action: PlayerAction) {
    game.apply_action(game.snapshot().current_actor.unwrap(), action)
        .unwrap();
}
fn at_river(n: usize, seed: u8) -> GameHand {
    let seats = (0..n)
        .map(|i| (PlayerId(80 + i as u64), Chips(1000), i as u8))
        .collect();
    let mut g = GameHand::new_with_rng(
        seats,
        0,
        Chips(10),
        Chips(5),
        PokerRng::from_seed_bytes([seed; 32]),
    );
    g.start().unwrap();
    for _ in 3..n {
        play(&mut g, PlayerAction::Fold);
    }
    play(&mut g, PlayerAction::Raise { amount: Chips(25) });
    play(&mut g, PlayerAction::Call);
    play(&mut g, PlayerAction::Call);
    while g.snapshot().street != Street::River {
        play(&mut g, PlayerAction::Check);
    }
    g
}
fn input(game: &GameHand) -> Option<(Request, Query)> {
    snapshot_with(game, |_| {}, |ctx| Request::from_context(ctx))
}
#[test]
fn three_way_ordinary_engine_nodes_share_public_root_and_all_three_ranges() {
    let mut g = at_river(3, 38);
    assert!(input(&g).is_none());
    play(&mut g, PlayerAction::Check);
    assert!(input(&g).is_none());
    play(&mut g, PlayerAction::Check);
    let (req, query) = input(&g).unwrap();
    assert_eq!(query.node, 0);
    assert_eq!(req.root.pot, 75);
    assert_eq!(req.root.bet, 38);
    let time = Instant::now();
    let solved = req.clone().solve(time + Duration::from_secs(4)).unwrap();
    let (iterations, gaps) = solved.quality();
    let support: [usize; 3] =
        std::array::from_fn(|p| solved.rows.iter().filter(|r| r.weight[p] > 0.).count());
    eprintln!("three-way river: {:?}, {iterations} iterations, {gaps:?}, supported hands {support:?}, board {:?}",time.elapsed(),req.root.board);
    assert!(gaps.into_iter().all(|g| g <= 0.0025));
    let mut rng = StdRng::seed_from_u64(21);
    let choose = |q: Query, g: &GameHand, rng: &mut StdRng| {
        let (action, source) = solved.decision(q, rng, None).unwrap();
        assert_eq!(source, "three_way_river_bucket_model");
        assert!(g
            .validate_action(g.snapshot().current_actor.unwrap(), &action)
            .is_ok());
    };
    choose(query, &g, &mut rng);
    let mut off = g.clone();
    play(&mut off, PlayerAction::Raise { amount: Chips(37) });
    assert!(input(&off).is_none());
    play(&mut g, PlayerAction::Raise { amount: Chips(38) });
    let (next, q) = input(&g).unwrap();
    assert!(req.same_root(&next));
    assert_eq!(q.node, 1);
    choose(q, &g, &mut rng);
    for (r, node) in [(PlayerAction::Fold, 2), (PlayerAction::Call, 3)] {
        let mut next = g.clone();
        play(&mut next, r);
        let (same, q) = input(&next).unwrap();
        assert!(req.same_root(&same));
        assert_eq!(q.node, node);
        choose(q, &next, &mut rng);
    }
    let mut four = at_river(4, 38);
    play(&mut four, PlayerAction::Check);
    play(&mut four, PlayerAction::Check);
    assert!(input(&four).is_none());
    assert!(req.solve(Instant::now()).is_none());
}

#[test]
fn reduction_and_best_responses_match_private_deal_enumeration_with_odd_chips() {
    // Several overlapping pockets, rank ties inside and across ranges, and
    // asymmetrical weights make independent-card approximations incorrect.
    let rows: Vec<Hand> = [
        ([0, 1], 1, 0, [0.4, 0.2, 0.1]),
        ([0, 2], 1, 0, [0.3, 0.1, 0.5]),
        ([2, 3], 2, 0, [0.1, 0.3, 0.4]),
        ([2, 4], 3, 0, [0.2, 0.5, 0.3]),
        ([4, 5], 4, 1, [0.5, 0.4, 0.1]),
        ([6, 7], 4, 1, [0.5, 0.2, 0.4]),
        ([1, 5], 5, 1, [0.1, 0.3, 0.2]),
        ([8, 9], 6, 1, [0.3, 0.1, 0.7]),
    ]
    .into_iter()
    .map(|(cards, rank, bucket, weight)| Hand {
        cards,
        rank,
        bucket,
        weight,
    })
    .collect();
    let due = Instant::now() + Duration::from_secs(2);
    let got = chance::joint(&rows, 1, due).unwrap();
    let mut exact = vec![[0.; 13]; K * K * K];
    let mut deals = Vec::new();
    let mut mass = 0.;
    for a in &rows {
        for b in &rows {
            for c in &rows {
                if a.cards
                    .iter()
                    .any(|x| b.cards.contains(x) || c.cards.contains(x))
                    || b.cards.iter().any(|x| c.cards.contains(x))
                {
                    continue;
                }
                let rank = [a.rank, b.rank, c.rank];
                let id = chance::RANKS
                    .iter()
                    .position(|r| {
                        (0..3).all(|i| (0..3).all(|j| r[i].cmp(&r[j]) == rank[i].cmp(&rank[j])))
                    })
                    .unwrap();
                let w = a.weight[0] * b.weight[1] * c.weight[2];
                exact[(a.bucket * K + b.bucket) * K + c.bucket][id] += w;
                deals.push((
                    [a.bucket, b.bucket, c.bucket],
                    [a.cards, b.cards, c.cards],
                    rank,
                    w,
                ));
                mass += w;
            }
        }
    }
    for (a, b) in got.iter().flatten().zip(exact.iter().flatten()) {
        assert!((a - b).abs() < 1e-12);
    }
    let p = game::solve(&got, 11, 6, due).unwrap();
    let certificate = audit::evaluate(&rows, &p.p, 11, 6, due).unwrap();
    // Enumerate the four pure policies of the single-decision seats and
    // all 16 for the last responder (two buckets at each of two nodes).
    let ev = |strategy: &[[f64; K]; 4], change: Option<(usize, [u8; 2], f64)>| {
        let mut ev = [0.; 3];
        for &([a, b, c], cards, r, w) in &deals {
            let mut moves = [
                strategy[0][c],
                strategy[1][a],
                strategy[2][b],
                strategy[3][b],
            ];
            if let Some((n, hand, action)) = change {
                if hand == cards[game::ACTOR[n]] {
                    moves[n] = action;
                }
            }
            let [bet, call, l0, l1] = moves;
            for (contributors, price, prob) in [
                ([true, true, true], 0, 1. - bet),
                ([false, false, true], 6, bet * (1. - call) * (1. - l0)),
                ([false, true, true], 6, bet * (1. - call) * l0),
                ([true, false, true], 6, bet * call * (1. - l1)),
                ([true, true, true], 6, bet * call * l1),
            ] {
                let win = (0..3)
                    .filter(|&i| contributors[i])
                    .map(|i| r[i])
                    .max()
                    .unwrap();
                let tied: Vec<_> = (0..3).filter(|&i| contributors[i] && r[i] == win).collect();
                let chips = 11 + price * contributors.into_iter().filter(|&v| v).count();
                for i in 0..3 {
                    if contributors[i] {
                        ev[i] -= w * prob * price as f64;
                    }
                }
                for (order, &i) in tied.iter().enumerate() {
                    ev[i] += w
                        * prob
                        * (chips / tied.len() + usize::from(order < chips % tied.len())) as f64;
                }
            }
        }
        ev.map(|v| v / (mass * 11.))
    };
    let opened: f64 = deals.iter().map(|(b, _, _, w)| w * p.p[0][b[2]]).sum();
    let continued: f64 = deals
        .iter()
        .map(|(b, _, _, w)| w * p.p[0][b[2]] * p.p[1][b[0]])
        .sum();
    assert!((certificate.first_call_rate - continued / opened).abs() < 1e-11);
    for (i, h) in rows.iter().enumerate() {
        let pocket_mass: f64 = deals
            .iter()
            .filter(|(_, cards, _, _)| cards[2] == h.cards)
            .map(|(_, _, _, w)| w)
            .sum();
        let [check, bet] = certificate.opening[i];
        let difference = ev(&p.p, Some((0, h.cards, 1.)))[2] - ev(&p.p, Some((0, h.cards, 0.)))[2];
        assert!(((bet - check) * pocket_mass / mass - difference).abs() < 1e-10);
    }
    let base = ev(&p.p, None);
    for (player, &base_value) in base.iter().enumerate() {
        let nodes: Vec<_> = (0..4).filter(|&i| game::ACTOR[i] == player).collect();
        let best = (0..1 << (2 * nodes.len()))
            .map(|mask| {
                let mut choice = p.p;
                for (n, &node) in nodes.iter().enumerate() {
                    for (k, action) in choice[node][..2].iter_mut().enumerate() {
                        *action = f64::from(mask & (1 << (2 * n + k)) != 0);
                    }
                }
                ev(&choice, None)[player]
            })
            .fold(base_value, f64::max);
        assert!((p.gaps[player] - (best - base_value)).abs() < 1e-10);
        assert!((certificate.bucket_gaps[player] - p.gaps[player]).abs() < 1e-10);
        let mut full = 0.;
        for node in nodes {
            for h in &rows {
                let a = ev(&p.p, Some((node, h.cards, 0.)))[player];
                let b = ev(&p.p, Some((node, h.cards, 1.)))[player];
                full += a.max(b) - base_value;
            }
        }
        assert!((certificate.gaps[player] - full).abs() < 1e-10);
    }
}

#[test]
#[ignore = "measure the normal 400ms worker budget across public boards"]
fn measure_three_way_admission() {
    let mut elapsed = Vec::new();
    let mut admitted = 0;
    let mut worst = 0_f64;
    for seed in 1..=48 {
        let mut g = at_river(3, seed);
        play(&mut g, PlayerAction::Check);
        play(&mut g, PlayerAction::Check);
        let (r, _) = input(&g).unwrap();
        let t = Instant::now();
        let got = r.solve(t + Duration::from_millis(400));
        elapsed.push(t.elapsed());
        if let Some(s) = got {
            admitted += 1;
            worst = worst.max(s.quality().1.into_iter().fold(0., f64::max));
        }
    }
    elapsed.sort();
    eprintln!(
        "{admitted}/48 admitted, full-pocket worst={worst:.6}, debug median={:?} p95={:?} max={:?}",
        elapsed[23], elapsed[45], elapsed[47]
    );
    assert!(admitted > 0 && worst <= 0.0025);
}

#[test]
fn only_named_three_way_overfolder_changes_a_modeled_positive_gain_open() {
    use crate::bot::{OpponentModel, OpponentStats};
    use std::collections::HashMap;
    let owner = uuid::Uuid::from_u128(41);
    let mut example = None;
    for seed in [38, 1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11] {
        let mut game = at_river(3, seed);
        play(&mut game, PlayerAction::Check);
        play(&mut game, PlayerAction::Check);
        let (req, _) = input(&game).unwrap();
        let deadline = Instant::now() + Duration::from_secs(3);
        let Some(mut solved) = req.solve(deadline) else {
            continue;
        };
        solved.prepare_overfolder(deadline);
        let Some(shifts) = &solved.overfolder_shift else {
            continue;
        };
        if let Some(i) = shifts.iter().position(|s| s.abs() > 0. && s.is_finite()) {
            example = Some((solved, i));
            break;
        }
    }
    let (solved, i) =
        example.expect("admitted mixed pocket with a positive gain under the named read");
    let first = solved.root.seats[0];
    let stats = |players, samples| {
        let mut s = OpponentStats {
            profile_owner: Some(owner),
            ..Default::default()
        };
        for _ in 0..samples {
            s.observe_river_half_pot(players, true);
        }
        HashMap::from([(first, s)])
    };
    let read = |stats, seat, user| {
        let mut model = OpponentModel::new(stats);
        model.river_responder = Some((seat, user));
        model
    };
    let data = stats(3, 32);
    let row = &solved.rows[i];
    let base = solved.policy.p[0][row.bucket];
    let shift = solved.overfolder_shift.as_ref().unwrap()[i];
    let seed = (0..20000)
        .find(|&seed| {
            let t = StdRng::seed_from_u64(seed).gen::<f64>();
            (t < base) != (t < base + shift)
        })
        .unwrap();
    let sample = |model: Option<&OpponentModel<'_>>| {
        solved
            .decision(
                Query {
                    root: solved.root.clone(),
                    node: 0,
                    hole: HoleCards::new(card(row.cards[0]), card(row.cards[1])),
                },
                &mut StdRng::seed_from_u64(seed),
                model,
            )
            .unwrap()
    };
    let ordinary = sample(None);
    let adjusted = sample(Some(&read(&data, first, owner)));
    assert_ne!(ordinary.0, adjusted.0);
    assert_eq!(adjusted.1, "three_way_river_overfolder");
    let wrong_count = stats(2, 32);
    let sparse = stats(3, 12);
    for rejected in [
        read(&data, solved.root.seats[1], owner),
        read(&data, first, uuid::Uuid::nil()),
        read(&wrong_count, first, owner),
        read(&sparse, first, owner),
    ] {
        assert_eq!(ordinary, sample(Some(&rejected)));
    }
}
