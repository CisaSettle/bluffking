use super::*;
use crate::bot::{
    blind_steal, lookup_profile, public_action_history, shark::tests::opening, table_decision,
    BotPersona, OpponentModel, OpponentStats, SeatPublic,
};
use engine::{Card, GameHand, HoleCards, PlayerId, PokerRng, Position, Rank, Suit};
use rand::{rngs::StdRng, SeedableRng};
use std::collections::HashMap;
use uuid::Uuid;

#[test]
fn sb_steal_read_moves_only_supported_folds_and_tags_only_extra_opens() {
    let owner = Uuid::from_u128(18);
    for (n, policy) in [
        (2, &*HU100),
        (3, &*RING3),
        (4, &*RING4),
        (5, &*RING5),
        (6, &*RING6),
        (7, &*RING7),
        (8, &*RING8),
        (9, &*RING9),
    ] {
        let dealer = if n == 2 { 0 } else { n - 3 };
        let (sb, bb) = engine::blind_positions(dealer, n);
        let players = (0..n)
            .map(|s| (PlayerId(s as u64 + 1), Chips(1000), s as u8))
            .collect();
        let mut hand = GameHand::new_with_rng(
            players,
            dealer,
            Chips(10),
            Chips(5),
            PokerRng::from_seed_bytes([1; 32]),
        );
        hand.start().unwrap();
        let mut node = &policy.nodes[policy.root];
        for _ in 0..n - 2 {
            let actor = hand.snapshot().current_actor.unwrap();
            hand.apply_action(actor, PlayerAction::Fold).unwrap();
            node = &policy.nodes[node
                .edges
                .iter()
                .find(|e| e.play == Move::Fold)
                .unwrap()
                .next
                .unwrap()];
        }
        let snap = hand.snapshot();
        assert_eq!(snap.current_actor, Some(PlayerId(sb as u64 + 1)));
        let facts = public_action_history(
            [(Street::Preflop, hand.current_street_action_facts())],
            |id| Some((id.0 - 1) as u8),
        )
        .unwrap();
        let actions: Vec<_> = facts.iter().map(|f| (f.seat, f.action.clone())).collect();
        let seats: Vec<_> = snap
            .players
            .iter()
            .map(|p| SeatPublic {
                seat: p.seat,
                stack: p.stack.0,
                committed_this_street: p.committed_this_street.0,
                all_in: p.all_in,
                folded: p.folded,
            })
            .collect();
        let mut rng = StdRng::seed_from_u64(19);
        let mut ctx = opening(&mut rng, &snap.board, &seats);
        ctx.seat = sb as u8;
        ctx.table.dealer_seat = dealer as u8;
        ctx.sb_seat = sb as u8;
        ctx.bb_seat = bb as u8;
        ctx.big_blind = 10;
        ctx.position =
            Position::for_seat(ctx.seat, dealer as u8, &(0..n as u8).collect::<Vec<_>>());
        ctx.stack_chips = 995;
        ctx.stack_bb = 99;
        ctx.street = Street::Preflop;
        ctx.current_bet = snap.current_bet;
        ctx.pot = snap.pot;
        ctx.to_call = Chips(5);
        ctx.committed_this_street = Chips(5);
        ctx.min_raise_to = snap.min_raise_to;
        ctx.street_actions = &actions;
        ctx.public_history = Some(&facts);
        let fold = node
            .edges
            .iter()
            .position(|e| e.play == Move::Fold)
            .unwrap();
        let raise = node
            .edges
            .iter()
            .position(|e| e.play == Move::Raise(5))
            .unwrap();
        let class = (0..CLASSES)
            .filter(|&c| {
                node.strategy[raise * CLASSES + c] >= 0.01 && node.reach[c] >= MIN_HAND_REACH
            })
            .max_by(|&a, &b| {
                node.strategy[fold * CLASSES + a].total_cmp(&node.strategy[fold * CLASSES + b])
            })
            .unwrap();
        let (row, col) = (class / 13, class % 13);
        ctx.hole = HoleCards::new(
            Card::new(Rank::ALL[row], Suit::Spades),
            Card::new(
                Rank::ALL[col],
                if row > col { Suit::Spades } else { Suit::Clubs },
            ),
        );
        let mut stats = OpponentStats {
            profile_owner: Some(owner),
            ..Default::default()
        };
        for _ in 0..32 {
            stats.sb_steal_folds.get_or_insert_with(Default::default)[n - 2].observe(true);
        }
        let reads = HashMap::from([(bb as u8, stats)]);
        let mut model = OpponentModel::new(&reads);
        model.steal_bb = Some((bb as u8, owner));
        let tag_seed = (0..8192)
            .find(|&seed| {
                *ctx.rng = StdRng::seed_from_u64(seed);
                table_decision(&mut ctx, Some(&model)).2 == blind_steal::OPEN_SOURCE
            })
            .expect("at least one admitted fold shifts to its existing 2.5 BB open");
        let expected = PlayerAction::Raise { amount: Chips(25) };
        let choose = |ctx: &mut super::DecisionContext<'_>, model: Option<&OpponentModel<'_>>| {
            *ctx.rng = StdRng::seed_from_u64(tag_seed);
            let (play, _, source) = table_decision(ctx, model);
            (play, source)
        };
        assert_eq!(
            choose(&mut ctx, None),
            (PlayerAction::Fold, "gtopen_preflop")
        );
        assert_eq!(
            choose(&mut ctx, Some(&model)),
            (expected.clone(), blind_steal::OPEN_SOURCE)
        );
        model.steal_bb = Some((bb as u8, Uuid::nil()));
        assert_eq!(
            choose(&mut ctx, Some(&model)),
            (PlayerAction::Fold, "gtopen_preflop")
        );
        let mut other_count = stats;
        other_count
            .sb_steal_folds
            .as_mut()
            .unwrap()
            .swap(n - 2, if n == 2 { 1 } else { 0 });
        let reads = HashMap::from([(bb as u8, other_count)]);
        let mut cold = OpponentModel::new(&reads);
        cold.steal_bb = Some((bb as u8, owner));
        assert_eq!(
            choose(&mut ctx, Some(&cold)),
            (PlayerAction::Fold, "gtopen_preflop")
        );
        let (small, big) = (sb as u8, bb as u8);
        let watch = blind_steal::Hand {
            seats: Some((small, big, owner)),
            ..Default::default()
        };
        assert!(!watch.faced(&hand, &snap, 10, big));
        hand.apply_action(PlayerId(sb as u64 + 1), expected)
            .unwrap();
        assert!(watch.faced(&hand, &hand.snapshot(), 10, big));
        assert!(!watch.faced(&hand, &hand.snapshot(), 10, small));
        hand.apply_action(
            PlayerId(bb as u64 + 1),
            PlayerAction::Raise { amount: Chips(80) },
        )
        .unwrap();
        ctx.to_call = Chips(55);
        ctx.committed_this_street = Chips(25);
        ctx.current_bet = Chips(80);
        model.extra_steal_opener = Some(small);
        assert_eq!(
            choose(&mut ctx, Some(&model)),
            (PlayerAction::Fold, "sb_steal_continuation")
        );
        hand.apply_action(PlayerId(sb as u64 + 1), PlayerAction::Fold)
            .unwrap();
        assert_eq!(hand.finish().final_stacks[&(sb as u64 + 1)], 975); // only 20 chips extra vs folding SB
    }
}

#[test]
fn sb_steal_continuation_releases_air_and_values_an_unbeatable_hand() {
    let card = |s| engine::card::parse_card(s).unwrap();
    let board = engine::BoardCards {
        flop: Some([card("As"), card("Ad"), card("Ah")]),
        turn: Some(card("2d")),
        river: Some(card("9s")),
    };
    let seats = [0, 1].map(|seat| SeatPublic {
        seat,
        stack: 200,
        ..Default::default()
    });
    let mut rng = StdRng::seed_from_u64(1);
    let mut ctx = opening(&mut rng, &board, &seats);
    ctx.profile = lookup_profile(BotPersona::Shark);
    ctx.hole = HoleCards::new(card("Ac"), card("3h"));
    let reads = HashMap::new();
    let mut model = OpponentModel::new(&reads);
    model.extra_steal_opener = Some(ctx.seat);
    assert_eq!(
        table_decision(&mut ctx, Some(&model)).0,
        PlayerAction::Raise { amount: Chips(50) }
    );
    ctx.to_call = Chips(25);
    ctx.current_bet = Chips(25);
    assert_eq!(table_decision(&mut ctx, Some(&model)).0, PlayerAction::Call);
    ctx.hole = HoleCards::new(card("3h"), card("4h"));
    assert_eq!(table_decision(&mut ctx, Some(&model)).0, PlayerAction::Fold);
    ctx.to_call = Chips::ZERO;
    ctx.current_bet = Chips::ZERO;
    assert_eq!(
        table_decision(&mut ctx, Some(&model)).0,
        PlayerAction::Check
    );
}
