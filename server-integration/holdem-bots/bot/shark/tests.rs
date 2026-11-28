use super::*;
use engine::card::parse_card;
use rand::SeedableRng;
use std::collections::HashMap;

fn board(cards: [&str; 5]) -> BoardCards {
    let [a, b, c, d, e] = cards.map(|s| parse_card(s).unwrap());
    BoardCards {
        flop: Some([a, b, c]),
        turn: Some(d),
        river: Some(e),
    }
}

pub(in crate::bot) fn opening<'a>(
    rng: &'a mut StdRng,
    board: &'a BoardCards,
    seats: &'a [SeatPublic],
) -> DecisionContext<'a> {
    DecisionContext {
        rng,
        plain_cash: true,
        seat: 0,
        position: Position::BigBlind,
        persona: BotPersona::Shark,
        profile: lookup_profile(BotPersona::Shark),
        sb_seat: 1,
        bb_seat: 0,
        num_active: 2,
        stack_bb: 100,
        stack_chips: 200,
        big_blind: 2,
        has_straddle: false,
        street: Street::River,
        board,
        hole: HoleCards::new(parse_card("6c").unwrap(), parse_card("4h").unwrap()),
        pot: Chips(100),
        to_call: Chips::ZERO,
        current_bet: Chips::ZERO,
        can_raise: true,
        min_raise_to: Some(Chips(2)),
        committed_this_street: Chips::ZERO,
        street_actions: &[],
        public_history: None,
        last_aggressor_seat: None,
        preflop_aggressor_seat: None,
        num_callers_so_far: 0,
        flop_aggressor_seat: None,
        turn_aggressor_seat: None,
        squid_call_value: Default::default(),
        squid_defense: Default::default(),
        seven_deuce_bounty: 0.0,
        all_in_public: None,
        all_in_evidence: None,
        table: TablePublic {
            dealer_seat: 1,
            seats,
        },
        line_pressure: 0.0,
        open_pressure: 0.0,
    }
}

fn owned_model(stats: &HashMap<u8, OpponentStats>) -> OpponentModel<'_> {
    let mut model = OpponentModel::new(stats);
    model.river_responder = Some((1, uuid::Uuid::from_u128(1)));
    model
}

fn with_folds(
    ctx: &mut DecisionContext<'_>,
    seat: u8,
    street: Street,
    size: u32,
    folds: u32,
) -> (PlayerAction, &'static str) {
    let mut stats = OpponentStats {
        profile_owner: Some(uuid::Uuid::from_u128(seat.into())),
        ..Default::default()
    };
    if street == Street::River && size == 50 {
        for _ in 0..folds {
            stats.observe_river_half_pot(2, true);
        }
    }
    let (street, size) = bet_response_bucket(street, size, 100).unwrap();
    stats.bet_responses[street][size] = BetResponseStats {
        faced: folds,
        folded: folds,
    };
    let by_seat = HashMap::from([(seat, stats)]);
    let (action, _, source) = table_decision(ctx, Some(&owned_model(&by_seat)));
    (action, source)
}

#[test]
fn split_board_targets_only_the_matched_villain_and_calls_their_raise() {
    let board = board(["As", "Kd", "Qh", "Jc", "Ts"]);
    let seats = [0, 1].map(|seat| SeatPublic {
        seat,
        stack: 200,
        ..Default::default()
    });
    let mut rng = StdRng::seed_from_u64(4);
    let mut ctx = opening(&mut rng, &board, &seats);
    let (action, _, source) = table_decision(&mut ctx, None);
    let baseline = (PlayerAction::Check, "split_board");
    assert_eq!((action, source), baseline);
    for (seat, street, size, folds) in [
        (1, Street::River, 50, 1),
        (1, Street::Turn, 50, 60),
        (1, Street::River, 150, 60),
        (3, Street::River, 50, 60),
    ] {
        assert_eq!(with_folds(&mut ctx, seat, street, size, folds), baseline);
    }
    assert_eq!(
        with_folds(&mut ctx, 1, Street::River, 50, 60),
        (
            PlayerAction::Raise { amount: Chips(50) },
            "split_board_overfolder"
        )
    );

    let mut raised = seats;
    raised[0].committed_this_street = 50;
    raised[0].stack = 150;
    raised[1].committed_this_street = 125;
    raised[1].stack = 75;
    let actions = [
        (0, PlayerAction::Raise { amount: Chips(50) }),
        (1, PlayerAction::Raise { amount: Chips(125) }),
    ];
    ctx.table.seats = &raised;
    ctx.stack_bb = 75;
    ctx.stack_chips = 150;
    ctx.pot = Chips(275);
    ctx.current_bet = Chips(125);
    ctx.committed_this_street = Chips(50);
    ctx.to_call = Chips(75);
    ctx.min_raise_to = Some(Chips(200));
    ctx.last_aggressor_seat = Some(1);
    ctx.street_actions = &actions;
    assert_eq!(
        with_folds(&mut ctx, 1, Street::River, 50, 60),
        (PlayerAction::Call, "split_board")
    );
}

#[test]
fn multiway_split_board_reads_only_the_first_responder_at_that_player_count() {
    let board = board(["As", "Kd", "Qh", "Jc", "Ts"]);
    let mut rng = StdRng::seed_from_u64(12);
    for players in 3..=9 {
        let seats: Vec<_> = (0..players)
            .map(|s| SeatPublic {
                seat: s as u8,
                stack: 200,
                ..Default::default()
            })
            .collect();
        let mut ctx = opening(&mut rng, &board, &seats);
        ctx.num_active = players as u8;
        ctx.pot = Chips(players as u32 * 10);
        let check = (PlayerAction::Check, "split_board");
        let (action, _, source) = table_decision(&mut ctx, None);
        assert_eq!((action, source), check);
        assert_eq!(with_folds(&mut ctx, 1, Street::River, 50, 60), check);
        let mut read = OpponentStats {
            profile_owner: Some(uuid::Uuid::from_u128(1)),
            ..Default::default()
        };
        for _ in 0..32 {
            read.observe_river_half_pot(players, true);
        }
        let wrong = HashMap::from([(2, read)]);
        let (action, _, source) = table_decision(&mut ctx, Some(&owned_model(&wrong)));
        assert_eq!((action, source), check);

        let correct = HashMap::from([(1, read)]);
        let mut model = owned_model(&correct);
        model.river_responder = Some((1, uuid::Uuid::from_u128(2)));
        assert_eq!(
            table_decision(&mut ctx, Some(&model)).0,
            PlayerAction::Check
        );
        model.river_responder = Some((1, uuid::Uuid::from_u128(1)));
        ctx.pot.0 += 1;
        assert_eq!(
            table_decision(&mut ctx, Some(&model)).0,
            PlayerAction::Check
        );
        ctx.pot.0 -= 1;
        let (action, _, source) = table_decision(&mut ctx, Some(&model));
        assert_eq!(
            (action, source),
            (
                PlayerAction::Raise {
                    amount: Chips(players as u32 * 5)
                },
                "split_board_overfolder"
            )
        );

        let mut other_count = OpponentStats {
            profile_owner: Some(uuid::Uuid::from_u128(1)),
            ..Default::default()
        };
        for _ in 0..32 {
            other_count.observe_river_half_pot(if players == 3 { 4 } else { 3 }, true);
        }
        let wrong_count = HashMap::from([(1, other_count)]);
        assert_eq!(
            table_decision(&mut ctx, Some(&owned_model(&wrong_count))).0,
            PlayerAction::Check
        );
    }
}

#[test]
fn multiway_half_pot_read_requires_an_actual_first_response() {
    use engine::rng::PokerRng;
    use engine::{GameHand, PlayerId};
    let seats = (0..3)
        .map(|s| (PlayerId(s + 1), Chips(200), s as u8))
        .collect();
    let mut hand = GameHand::new_with_rng(
        seats,
        0,
        Chips(2),
        Chips(1),
        PokerRng::from_seed_bytes([7; 32]),
    );
    hand.start().unwrap();
    fn check_to_river(hand: &mut GameHand) {
        loop {
            let snap = hand.snapshot();
            if snap.street == Street::River {
                break;
            }
            let id = snap.current_actor.unwrap();
            let actor = snap.players.iter().find(|p| p.player_id == id).unwrap();
            let action = if actor.committed_this_street < snap.current_bet {
                PlayerAction::Call
            } else {
                PlayerAction::Check
            };
            hand.apply_action(id, action).unwrap();
        }
    }

    let mut heads_up = hand.clone();
    let first = heads_up.snapshot().current_actor.unwrap();
    heads_up.apply_action(first, PlayerAction::Fold).unwrap();
    check_to_river(&mut heads_up);
    let first = heads_up.snapshot().current_actor.unwrap();
    heads_up.apply_action(first, PlayerAction::Check).unwrap();
    let snap = heads_up.snapshot();
    let amount = Chips(snap.pot.0.div_ceil(2));
    heads_up
        .apply_action(snap.current_actor.unwrap(), PlayerAction::Raise { amount })
        .unwrap();
    assert_eq!(
        river_half_pot_opportunity(
            &heads_up.snapshot(),
            &heads_up.current_street_action_facts()
        ),
        Some(2),
    );
    assert_eq!(
        heads_up_bet_response(
            &heads_up.snapshot(),
            &heads_up.current_street_action_facts()
        ),
        bet_response_bucket(Street::River, amount.0, snap.pot.0),
    );

    check_to_river(&mut hand);
    let snap = hand.snapshot();
    let bet = snap.pot.0.div_ceil(2);
    hand.apply_action(
        snap.current_actor.unwrap(),
        PlayerAction::Raise { amount: Chips(bet) },
    )
    .unwrap();
    let snap = hand.snapshot();
    let facts = hand.current_street_action_facts();
    assert_eq!(river_half_pot_opportunity(&snap, &facts), Some(3));
    assert_eq!(heads_up_bet_response(&snap, &facts), None);
    let mut folded = hand.clone();
    folded
        .apply_action(snap.current_actor.unwrap(), PlayerAction::Fold)
        .unwrap();
    let after_fold = folded.snapshot();
    assert_eq!(after_fold.players.iter().filter(|p| !p.folded).count(), 2);
    assert!(after_fold.current_actor.is_some());
    assert_eq!(
        heads_up_bet_response(&after_fold, &folded.current_street_action_facts()),
        None,
    );
    assert_eq!(
        river_half_pot_opportunity(&after_fold, &folded.current_street_action_facts()),
        None,
    );
    let mut other_size = facts.clone();
    other_size.last_mut().unwrap().amount.0 += 1;
    assert_eq!(river_half_pot_opportunity(&snap, &other_size), None);
    hand.apply_action(snap.current_actor.unwrap(), PlayerAction::Call)
        .unwrap();
    assert_eq!(
        river_half_pot_opportunity(&hand.snapshot(), &hand.current_street_action_facts()),
        None
    );
}

#[test]
fn multiway_flop_prices_the_guaranteed_share_and_all_hidden_cards() {
    let card = |s| parse_card(s).unwrap();
    let mut rng = StdRng::seed_from_u64(66);
    for (players, call, hole, flop, certified) in [
        (3, 50, ["Ah", "Kh"], ["As", "Ad", "Ks"], true),
        (9, 50, ["Ah", "Kh"], ["As", "Ad", "Ks"], false),
        (9, 1, ["Ah", "Kh"], ["As", "Ad", "Ks"], true),
        (9, 50, ["5s", "8s"], ["4s", "6s", "7s"], true),
        (9, 50, ["8c", "9c"], ["Tc", "Jc", "Qc"], false),
        (9, 50, ["As", "Ah"], ["Ac", "Ad", "7s"], true),
    ] {
        let seats: Vec<_> = (0..players)
            .map(|s| SeatPublic {
                seat: s,
                stack: if s == 0 { call } else { 200 },
                committed_this_street: if s == 1 { call } else { 0 },
                ..Default::default()
            })
            .collect();
        let board = BoardCards {
            flop: Some(flop.map(card)),
            ..Default::default()
        };
        let mut ctx = opening(&mut rng, &board, &seats);
        let [a, b] = hole.map(card);
        ctx.hole = HoleCards::new(a, b);
        ctx.street = Street::Flop;
        ctx.pot = Chips(200);
        ctx.to_call = Chips(call);
        ctx.current_bet = Chips(call);
        ctx.stack_chips = call;
        ctx.stack_bb = call / ctx.big_blind;
        ctx.can_raise = false;
        ctx.min_raise_to = None;
        ctx.num_active = players;
        let (action, _, source) = table_decision(&mut ctx, None);
        assert_eq!(
            source == "showdown_bound",
            certified,
            "{players} players {call} call {hole:?} / {flop:?}"
        );
        if certified {
            assert_eq!(action, PlayerAction::Call);
        }
    }
}

#[test]
fn multiway_turn_closing_call_prices_all_other_hidden_cards() {
    let card = |s| parse_card(s).unwrap();
    let draw = BoardCards {
        flop: Some([card("2s"), card("5s"), card("Jc")]),
        turn: Some(card("Qd")),
        river: None,
    };
    let quads = BoardCards {
        flop: Some([card("Ac"), card("As"), card("7s")]),
        turn: Some(card("4c")),
        river: None,
    };
    let mut rng = StdRng::seed_from_u64(15);
    for players in 3..=9 {
        let seats: Vec<_> = (0..players)
            .map(|s| SeatPublic {
                seat: s,
                stack: if s == 0 { 1 } else { 100 },
                committed_this_street: if s == 1 { 1 } else { 0 },
                ..Default::default()
            })
            .collect();
        for (board, hero, expected) in [
            (&quads, ["Ah", "Ad"], true),
            (&draw, ["As", "Ks"], players == 3),
        ] {
            let mut ctx = opening(&mut rng, board, &seats);
            ctx.street = Street::Turn;
            ctx.hole = HoleCards::new(card(hero[0]), card(hero[1]));
            ctx.pot = Chips(200);
            ctx.stack_chips = 1;
            ctx.stack_bb = 0;
            ctx.to_call = Chips(1);
            ctx.current_bet = Chips(1);
            ctx.can_raise = false;
            ctx.min_raise_to = None;
            ctx.num_active = players;
            let (action, _, source) = table_decision(&mut ctx, None);
            if expected {
                assert_eq!(
                    (action, source),
                    (PlayerAction::Call, "showdown_bound"),
                    "{players} {hero:?}"
                );
            } else if players == 9 {
                assert_ne!(source, "showdown_bound");
            }
        }
    }
}

#[test]
fn multiway_river_accounts_for_distinct_pockets_and_side_pot_eligibility() {
    let board = board(["As", "Kc", "Qd", "6h", "4d"]);
    let mut rng = StdRng::seed_from_u64(32);
    for (players, short) in [(3, false), (4, false), (5, false), (9, false), (5, true)] {
        let seats: Vec<_> = (0..players)
            .map(|s| SeatPublic {
                seat: s,
                stack: if s == 0 { 100 } else { 0 },
                all_in: s != 0,
                committed_this_street: if s == 0 {
                    0
                } else if short && s == 1 {
                    80
                } else {
                    100
                },
                folded: false,
            })
            .collect();
        let mut ctx = opening(&mut rng, &board, &seats);
        ctx.hole = HoleCards::new(parse_card("2c").unwrap(), parse_card("3c").unwrap());
        ctx.stack_chips = 100;
        ctx.stack_bb = 50;
        ctx.num_active = 1;
        ctx.current_bet = Chips(100);
        ctx.to_call = Chips(100);
        ctx.pot = Chips(50 + seats.iter().map(|s| s.committed_this_street).sum::<u32>());
        ctx.can_raise = false;
        ctx.min_raise_to = None;
        // Only a remaining 2 plus a remaining 3 can avoid beating hero.
        // Three of each remain. Three covering opponents can all tie;
        // four covering opponents force at least one winner over hero.
        let (action, _, source) = table_decision(&mut ctx, None);
        if players >= 5 && !short {
            assert_eq!((action, source), (PlayerAction::Fold, "showdown_bound"));
        } else {
            assert_ne!(source, "showdown_bound");
        }
    }
}

#[test]
fn split_board_rejects_improvable_boards_and_nonstandard_chip_utility() {
    let seats = [0, 1].map(|seat| SeatPublic {
        seat,
        stack: 200,
        ..Default::default()
    });
    let mut rng = StdRng::seed_from_u64(4);
    for (cards, plain_cash, expected_source) in [
        (["As", "Ks", "Qs", "Jh", "Tc"], true, "range_policy"),
        (["As", "Ac", "Ad", "Ah", "Qs"], true, "range_policy"),
        (["2s", "2c", "2d", "2h", "Ks"], true, "range_policy"),
        (["9s", "Ts", "Js", "Qs", "Ks"], true, "range_policy"),
        (["As", "Ks", "Qs", "Js", "Ts"], true, "split_board"),
        (["As", "Ac", "Ad", "Ah", "Ks"], true, "split_board"),
        (["2s", "2c", "2d", "2h", "As"], true, "split_board"),
        (["As", "Kd", "Qh", "Jc", "Ts"], false, "range_policy"),
    ] {
        let board = board(cards);
        let mut ctx = opening(&mut rng, &board, &seats);
        ctx.plain_cash = plain_cash;
        let (_, _, source) = table_decision(&mut ctx, None);
        assert_eq!(
            source, expected_source,
            "{cards:?}, plain_cash={plain_cash}"
        );
    }
}

#[test]
fn six_player_locked_river_pays_the_checked_share_and_profits_from_a_fold() {
    use engine::{GameHand, PlayerId, PokerRng};
    let hero = PlayerId(2);
    let first_responder = PlayerId(3);
    let user = uuid::Uuid::from_u128(44);
    let mut stats = OpponentStats {
        profile_owner: Some(user),
        ..Default::default()
    };
    for _ in 0..25 {
        stats.observe_river_half_pot(6, true);
    }
    let reads = HashMap::from([(2, stats)]);
    let mut model = OpponentModel::new(&reads);
    model.river_responder = Some((2, user));
    for (first, net) in [
        (None, 0),
        (Some(PlayerAction::Call), 0),
        (Some(PlayerAction::Fold), 1),
        (Some(PlayerAction::Raise { amount: Chips(18) }), 0),
    ] {
        let seats = (0..6)
            .map(|s| (PlayerId(s + 1), Chips(200), s as u8))
            .collect();
        let mut seed = [0; 32];
        seed[..8].copy_from_slice(&4173u64.to_le_bytes());
        let mut hand = GameHand::new_with_rng(
            seats,
            0,
            Chips(2),
            Chips(1),
            PokerRng::from_seed_bytes(seed),
        );
        hand.start().unwrap();
        let mut rng = StdRng::seed_from_u64(57);
        while !hand.is_done() {
            let snap = hand.snapshot();
            let id = snap.current_actor.unwrap();
            let actor = snap.players.iter().find(|p| p.player_id == id).unwrap();
            let debt = Chips(snap.current_bet.0 - actor.committed_this_street.0);
            let action = if snap.street == Street::River && id == hero {
                let seats: Vec<_> = snap
                    .players
                    .iter()
                    .map(|p| SeatPublic {
                        seat: p.seat,
                        stack: p.stack.0,
                        all_in: p.all_in,
                        folded: p.folded,
                        committed_this_street: p.committed_this_street.0,
                    })
                    .collect();
                let mut ctx = opening(&mut rng, &snap.board, &seats);
                ctx.seat = actor.seat;
                ctx.sb_seat = 1;
                ctx.bb_seat = 2;
                ctx.position = Position::SmallBlind;
                ctx.table.dealer_seat = snap.dealer_seat;
                ctx.num_active = seats.iter().filter(|s| !s.folded && !s.all_in).count() as u8;
                ctx.hole = actor.hole_cards.unwrap();
                ctx.stack_chips = actor.stack.0;
                ctx.stack_bb = actor.stack.0 / 2;
                ctx.pot = snap.pot;
                ctx.to_call = debt;
                ctx.current_bet = snap.current_bet;
                ctx.committed_this_street = actor.committed_this_street;
                ctx.min_raise_to = snap.min_raise_to;
                ctx.can_raise = hand.betting_round_ref().unwrap().current_player_can_raise();
                let (selected, _, source) =
                    table_decision(&mut ctx, first.as_ref().map(|_| &model));
                let expected = if debt.0 > 0 {
                    (PlayerAction::Call, "split_board")
                } else if first.is_some() {
                    (
                        PlayerAction::Raise { amount: Chips(6) },
                        "split_board_overfolder",
                    )
                } else {
                    (PlayerAction::Check, "split_board")
                };
                assert_eq!((selected.clone(), source), expected);
                selected
            } else if snap.street == Street::River && id == first_responder && debt == Chips(6) {
                first.clone().unwrap()
            } else if debt.0 > 0 {
                PlayerAction::Call
            } else {
                PlayerAction::Check
            };
            hand.apply_action(id, action).unwrap();
        }
        assert_eq!(hand.finish().final_stacks[&hero.inner()], 200 + net);
    }
}

#[test]
fn squid_river_size_read_changes_opening_decisions() {
    let board = board(["Ks", "9d", "5h", "3c", "2s"]);
    let seats = [0, 1].map(|seat| SeatPublic {
        seat,
        stack: 200,
        ..Default::default()
    });
    let mut stats = OpponentStats {
        profile_owner: Some(uuid::Uuid::from_u128(1)),
        ..Default::default()
    };
    stats.bet_responses[2][0] = BetResponseStats {
        folded: 3,
        faced: 9,
    };
    stats.bet_responses[2][1] = BetResponseStats {
        folded: 22,
        faced: 33,
    };
    let by_seat = HashMap::from([(1, stats)]);
    let old = OpponentModel::new(&by_seat);
    let matched = owned_model(&by_seat);
    let run = |seed, model: &OpponentModel<'_>| {
        let mut rng = StdRng::seed_from_u64(seed);
        let mut ctx = opening(&mut rng, &board, &seats);
        ctx.plain_cash = false;
        ctx.squid_defense.bonus = 12.0;
        ctx.squid_defense.history = Some(Vec::new());
        ctx.squid_defense.reads = vec![SquidRead::observed(1, &stats)];
        ctx.hole = HoleCards::new(parse_card("Qc").unwrap(), parse_card("Th").unwrap());
        table_decision(&mut ctx, Some(model)).0
    };
    // Same board, opponent history and draw. The matched profile checks the
    // poorly performing small bluff and takes the larger bluff opportunity.
    assert_eq!(run(0, &old), PlayerAction::Raise { amount: Chips(33) });
    assert_eq!(run(0, &matched), PlayerAction::Check);
    assert_eq!(run(5, &old), PlayerAction::Check);
    assert_eq!(run(5, &matched), PlayerAction::Raise { amount: Chips(75) });
}
