//! Frozen GTOpen preflop average policy for cash chip EV.
//! The offline game uses an explicit sizing menu and estimated flop values.
//! See scripts/export-shark-preflop.rs for the game and its best-response check.

use super::{
    call_or_all_in, hand_class, Chips, DecisionContext, PlayerAction, PublicAction, Street,
};
use flate2::read::GzDecoder;
use rand::Rng;
use serde::Deserialize;
use std::{borrow::Cow, io::Read, sync::LazyLock};

const CLASSES: usize = 169;
const MIN_HAND_REACH: f32 = 0.001;

#[path = "handoff.rs"]
mod handoff;
#[cfg(test)]
pub(super) use handoff::tests::snapshot_with;
pub(super) use handoff::{flop_reach, FlopReach};

#[derive(Clone, Copy, Debug, Deserialize, PartialEq, Eq)]
enum Move {
    Fold,
    Check,
    Call,
    Raise(u16),
}

#[derive(Deserialize)]
struct Edge {
    play: Move,
    next: Option<usize>,
}

#[derive(Deserialize)]
struct Node {
    actor: usize, // preflop action order
    posted: Vec<u16>,
    reach: Cow<'static, [f32]>, // lower bound on public path probability given this holding
    edges: Vec<Edge>,
    strategy: Cow<'static, [f32]>, // action-major
}

#[derive(Deserialize)]
struct HandoffCall {
    parent: usize,
    edge: usize,
    actor: usize,
    weights: Vec<f32>,
}

#[derive(Deserialize)]
struct Blueprint {
    root: usize,
    nodes: Vec<Node>,
    #[serde(default)]
    handoff_calls: Vec<HandoffCall>,
}

fn decode(bytes: &[u8]) -> Blueprint {
    let mut json = Vec::new();
    GzDecoder::new(bytes)
        .read_to_end(&mut json)
        .expect("embedded preflop policy gzip");
    serde_json::from_slice(&json).expect("embedded preflop policy")
}

static HU25: LazyLock<Blueprint> = LazyLock::new(|| decode(include_bytes!("hu25.json.gz")));
static HU50: LazyLock<Blueprint> = LazyLock::new(|| decode(include_bytes!("hu50.json.gz")));
static HU75: LazyLock<Blueprint> = LazyLock::new(|| decode(include_bytes!("hu75.json.gz")));
static HU100: LazyLock<Blueprint> = LazyLock::new(|| decode(include_bytes!("hu100.json.gz")));
static HU150: LazyLock<Blueprint> = LazyLock::new(|| decode(include_bytes!("hu150.json.gz")));
static HU200: LazyLock<Blueprint> = LazyLock::new(|| decode(include_bytes!("hu200.json.gz")));

static RING3: LazyLock<Blueprint> = LazyLock::new(|| decode(include_bytes!("ring3.json.gz")));
static RING4: LazyLock<Blueprint> = LazyLock::new(|| decode(include_bytes!("ring4.json.gz")));
static RING5: LazyLock<Blueprint> = LazyLock::new(|| decode(include_bytes!("ring5.json.gz")));
static RING6: LazyLock<Blueprint> = LazyLock::new(|| decode(include_bytes!("ring6.json.gz")));
static RING7: LazyLock<Blueprint> = LazyLock::new(|| decode(include_bytes!("ring7.json.gz")));
static RING8: LazyLock<Blueprint> = LazyLock::new(|| decode(include_bytes!("ring8.json.gz")));
static RING9: LazyLock<Blueprint> = LazyLock::new(|| decode(include_bytes!("ring9.json.gz")));

// Only the excess held by the single deepest player is uncallable before
// the deal. Preserve every other stack, including short seats that may
// later fold or create a side pot.
fn contestable_cap(original: &[u32]) -> Option<u32> {
    if !(2..=9).contains(&original.len()) {
        return None;
    }
    let (mut first, mut second) = (0, 0);
    for &chips in original {
        if chips >= first {
            second = first;
            first = chips;
        } else {
            second = second.max(chips);
        }
    }
    Some(second)
}

fn policy_for(original: &[u32], unit: u32) -> Option<&'static Blueprint> {
    let cap = contestable_cap(original)?;
    cap.checked_mul(original.len() as u32)?;
    let mut depths = [0; 9];
    for (&chips, depth) in original.iter().zip(&mut depths) {
        let chips = chips.min(cap);
        if unit == 0 || !chips.is_multiple_of(unit) {
            return None;
        }
        *depth = u16::try_from(chips / unit).ok()?;
    }
    let depths = &depths[..original.len()];
    if depths.iter().all(|&d| d == depths[0]) {
        match (depths.len(), depths[0]) {
            (2, 50) => return Some(&HU25),
            (2, 100) => return Some(&HU50),
            (2, 150) => return Some(&HU75),
            (2, 200) => return Some(&HU100),
            (2, 300) => return Some(&HU150),
            (2, 400) => return Some(&HU200),
            (3, 200) => return Some(&RING3),
            (4, 200) => return Some(&RING4),
            (5, 200) => return Some(&RING5),
            (6, 200) => return Some(&RING6),
            (7, 200) => return Some(&RING7),
            (8, 200) => return Some(&RING8),
            (9, 200) => return Some(&RING9),
            _ => {}
        }
    }
    stack_policies::lookup(depths)
}

mod stack_policies;

impl Node {
    fn edge_index(
        &self,
        fact: &PublicAction,
        seat: u8,
        unit: u32,
        original: u32,
        depth: u16,
    ) -> Option<usize> {
        let actor_post = u32::from(self.posted[self.actor]) * unit;
        let pot = self
            .posted
            .iter()
            .map(|&p| u32::from(p) * unit)
            .sum::<u32>();
        let bet = u32::from(*self.posted.iter().max()?) * unit;
        let price = bet - actor_post;
        if fact.street != Street::Preflop
            || fact.seat != seat
            || fact.pot_before != pot
            || fact.to_call_before != price
            || actor_post.checked_add(fact.amount)? != fact.committed_to
            || fact.increased_bet != (fact.committed_to > bet)
            || (fact.full_raise && !fact.increased_bet)
        {
            return None;
        }
        // The engine accepts an all-in entered as a raise-to amount as well
        // as an AllIn button action, including underraises and calls for less.
        let action = match &fact.action {
            PlayerAction::Raise { amount }
                if amount.0 == original && fact.committed_to == original =>
            {
                &PlayerAction::AllIn
            }
            action => action,
        };
        let play = match action {
            PlayerAction::Fold if fact.amount == 0 => Move::Fold,
            PlayerAction::Check if price == 0 && fact.amount == 0 => Move::Check,
            PlayerAction::Call
                if price > 0 && fact.amount == price.min(original.checked_sub(actor_post)?) =>
            {
                Move::Call
            }
            PlayerAction::Raise { amount }
                if amount.0 == fact.committed_to && fact.increased_bet =>
            {
                if !fact.full_raise || !amount.0.is_multiple_of(unit) {
                    return None;
                }
                Move::Raise(u16::try_from(amount.0 / unit).ok()?)
            }
            PlayerAction::AllIn
                if fact.committed_to == original && original == u32::from(depth) * unit =>
            {
                if fact.increased_bet {
                    Move::Raise(depth)
                } else {
                    Move::Call
                }
            }
            _ => return None,
        };
        self.edges.iter().position(|e| e.play == play)
    }

    fn following(
        &self,
        fact: &PublicAction,
        seat: u8,
        unit: u32,
        original: u32,
        depth: u16,
    ) -> Option<usize> {
        self.edges[self.edge_index(fact, seat, unit, original, depth)?].next
    }

    fn sample(&self, class: usize, draw: f64) -> Move {
        let weights = self.strategy[class..]
            .iter()
            .step_by(CLASSES)
            .map(|&p| f64::from(p));
        let mut target = draw * weights.clone().sum::<f64>();
        let mut last_positive = None;
        for (edge, weight) in self.edges.iter().zip(weights) {
            if weight == 0.0 {
                continue;
            }
            last_positive = Some(edge.play);
            if target < weight {
                return edge.play;
            }
            target -= weight;
        }
        last_positive.expect("preflop policy has positive mass")
    }
}

#[cfg(test)]
fn decision(ctx: &mut DecisionContext<'_>) -> Option<PlayerAction> {
    decision_with_opponents(ctx, None).map(|(play, _)| play)
}

pub(super) fn decision_with_opponents(
    ctx: &mut DecisionContext<'_>,
    model: Option<&super::OpponentModel<'_>>,
) -> Option<(PlayerAction, &'static str)> {
    let dealt = ctx.table.seats;
    let n = dealt.len();
    if !ctx.plain_cash
        || ctx.street != Street::Preflop
        || ctx.board.count() != 0
        || ctx.has_straddle
        || !(2..=9).contains(&n)
        || ctx.hole.card1 == ctx.hole.card2
        || ctx.big_blind < 2
        || !ctx.big_blind.is_multiple_of(2)
    {
        return None;
    }
    let history = ctx.public_history?;
    if history.len() != ctx.street_actions.len() {
        return None;
    }
    let dealer = dealt.iter().position(|p| p.seat == ctx.table.dealer_seat)?;
    let small = if n == 2 { dealer } else { (dealer + 1) % n };
    let big = (small + 1) % n;
    if dealt[small].seat != ctx.sb_seat || dealt[big].seat != ctx.bb_seat {
        return None;
    }
    // Array indices in the solve are the original preflop action order,
    // including players who subsequently folded. HU dealer posts the SB.
    let first = (big + 1) % n;
    let player = |i: usize| &dealt[(first + i) % n];
    let unit = ctx.big_blind / 2;
    let mut original = [0; 9];
    for (i, total) in original[..n].iter_mut().enumerate() {
        let p = player(i);
        if p.all_in != (p.stack == 0) {
            return None;
        }
        *total = p.stack.checked_add(p.committed_this_street)?;
    }
    let original = &original[..n];
    let stack = contestable_cap(original)?;
    let policy = policy_for(original, unit)?;
    let mut node = &policy.nodes[policy.root];
    let mut folded = [false; 9];
    for (fact, (seat, street_action)) in history.iter().zip(ctx.street_actions) {
        let projected = match &fact.action {
            PlayerAction::Raise { .. } | PlayerAction::AllIn if !fact.full_raise => {
                &PlayerAction::Call
            }
            action => action,
        };
        if *seat != fact.seat || street_action != projected {
            return None;
        }
        let actor = node.actor;
        let seat_cap = original[actor].min(stack) / unit;
        let next = node.following(
            fact,
            player(actor).seat,
            unit,
            original[actor],
            u16::try_from(seat_cap).ok()?,
        )?;
        folded[actor] = matches!(fact.action, PlayerAction::Fold);
        node = &policy.nodes[next];
    }
    let hero = player(node.actor);
    let posted = |i: usize| u32::from(node.posted[i]) * unit;
    let bet = u32::from(*node.posted.iter().max()?) * unit;
    if hero.seat != ctx.seat
        || hero.all_in
        || hero.folded
        || hero.stack != ctx.stack_chips
        || node.posted.len() != n
        || (0..n)
            .any(|i| player(i).folded != folded[i] || player(i).committed_this_street != posted(i))
        || ctx.committed_this_street.0 != hero.committed_this_street
        || ctx.current_bet.0 != bet
        || ctx.to_call.0 != bet - hero.committed_this_street
        || ctx.pot.0 != (0..n).map(posted).sum::<u32>()
    {
        return None;
    }
    let mut raises = false;
    for edge in &node.edges {
        if let Move::Raise(amount) = edge.play {
            raises = true;
            let to = u32::from(amount) * unit;
            if !ctx.can_raise
                || to <= bet
                || to > original[node.actor].min(stack)
                || (to < original[node.actor] && to < ctx.min_raise_to?.0)
            {
                return None;
            }
        }
    }
    if raises != ctx.can_raise {
        return None;
    }
    let (high, low, suited) = hand_class(ctx.hole);
    let (row, col) = if suited { (high, low) } else { (low, high) };
    let class = row as usize * 13 + col as usize;
    if node.reach[class] < MIN_HAND_REACH {
        return None;
    }
    let play = node.sample(class, ctx.rng.gen::<f64>());
    if let Some(model) = model.filter(|_| {
        play == Move::Fold
            && stack / unit == 200
            && ctx.seat == ctx.sb_seat
            && original.iter().all(|&s| s == stack)
    }) {
        // Ignore residual CFR mass on nominally available actions. Both moves
        // need meaningful support for this exact holding, with an admitted BB reply node.
        let supported = |play| {
            node.edges.iter().enumerate().any(|(i, e)| {
                e.play == play
                    && node.strategy[i * CLASSES + class] >= 0.01
                    && (play == Move::Fold || e.next.is_some())
            })
        };
        if supported(Move::Fold)
            && supported(Move::Raise(5))
            && super::blind_steal::extra_open(ctx, model)
        {
            return Some((
                Move::Raise(5).action(ctx, unit, original[node.actor]),
                super::blind_steal::OPEN_SOURCE,
            ));
        }
    }
    Some((
        play.action(ctx, unit, original[node.actor]),
        "gtopen_preflop",
    ))
}

impl Move {
    fn action(self, ctx: &DecisionContext<'_>, unit: u32, original: u32) -> PlayerAction {
        match self {
            Move::Fold => PlayerAction::Fold,
            Move::Check => PlayerAction::Check,
            Move::Call => call_or_all_in(ctx),
            Move::Raise(to) if u32::from(to) * unit == original => PlayerAction::AllIn,
            Move::Raise(to) => PlayerAction::Raise {
                amount: Chips(u32::from(to) * unit),
            },
        }
    }
}

#[cfg(test)]
#[path = "blind_steal_tests.rs"]
mod blind_steal_tests;

#[cfg(test)]
mod tests {
    use super::*;
    use crate::bot::{
        lookup_profile, public_action_history, table_decision, BotPersona, SeatPublic, TablePublic,
    };
    use engine::{Card, GameHand, HoleCards, PlayerId, PokerRng, Position, Rank, Suit};
    use rand::{rngs::StdRng, SeedableRng};

    #[test]
    fn blueprint_matches_engine_history_and_samples_legal_mixes() {
        // Non-adjacent chairs and real history, stacks, action rights and chip
        // amounts through the same projection the session uses.
        let small = PlayerId(71);
        let big = PlayerId(26);
        let mut hand = GameHand::new_with_rng(
            vec![(small, Chips(1000), 2), (big, Chips(1500), 7)],
            0,
            Chips(10),
            Chips(5),
            PokerRng::from_seed_bytes([1; 32]),
        );
        hand.start().unwrap();
        let open = PlayerAction::Raise { amount: Chips(25) };
        hand.apply_action(small, open).unwrap();
        let snap = hand.snapshot();
        assert_eq!(snap.current_actor, Some(big));
        let public = hand.current_street_action_facts();
        let seat_of = |id| {
            snap.players
                .iter()
                .find(|p| p.player_id == id)
                .map(|p| p.seat)
        };
        let pairs: Vec<_> = public
            .iter()
            .map(|f| (seat_of(f.player_id).unwrap(), f.strategy_action()))
            .collect();
        let facts = public_action_history([(Street::Preflop, public)], seat_of).unwrap();
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
        let mut rng = StdRng::seed_from_u64(14);
        let mut ctx = DecisionContext {
            seat: 7,
            position: Position::BigBlind,
            persona: BotPersona::Shark,
            profile: lookup_profile(BotPersona::Shark),
            rng: &mut rng,
            plain_cash: true,
            sb_seat: 2,
            bb_seat: 7,
            num_active: 2,
            stack_bb: 149,
            stack_chips: 1490,
            big_blind: 10,
            has_straddle: false,
            street: snap.street,
            board: &snap.board,
            hole: HoleCards::new(
                Card::new(Rank::Ace, Suit::Spades),
                Card::new(Rank::Ace, Suit::Hearts),
            ),
            pot: snap.pot,
            to_call: Chips(15),
            current_bet: snap.current_bet,
            can_raise: hand.betting_round_ref().unwrap().current_player_can_raise(),
            min_raise_to: snap.min_raise_to,
            committed_this_street: Chips(10),
            street_actions: &pairs,
            public_history: Some(&facts),
            last_aggressor_seat: Some(2),
            preflop_aggressor_seat: Some(2),
            num_callers_so_far: 0,
            flop_aggressor_seat: None,
            turn_aggressor_seat: None,
            squid_call_value: Default::default(),
            squid_defense: Default::default(),
            seven_deuce_bounty: 0.0,
            all_in_public: None,
            all_in_evidence: None,
            table: TablePublic {
                dealer_seat: snap.dealer_seat,
                seats: &seats,
            },
            line_pressure: 0.0,
            open_pressure: 0.0,
        };
        // BB covers SB. At 100BB effective, committing the last contested
        // half-blind is a raise for BB and an actual all-in for SB.
        assert_eq!(
            Move::Raise(200).action(&ctx, 5, 1500),
            PlayerAction::Raise {
                amount: Chips(1000)
            }
        );
        assert_eq!(Move::Raise(200).action(&ctx, 5, 1000), PlayerAction::AllIn);
        let (action, _, source) = table_decision(&mut ctx, None);
        assert_eq!(source, "gtopen_preflop");
        hand.apply_action(big, action).unwrap();
        ctx.can_raise = false;
        assert_eq!(decision(&mut ctx), None);
        ctx.can_raise = true;
        let variants =
            [(7, 25, 15), (2, 26, 15), (2, 25, 16)].map(|(seat, committed_to, pot_before)| {
                let mut wrong = facts.clone();
                wrong[0].seat = seat;
                wrong[0].committed_to = committed_to;
                wrong[0].amount = committed_to - 5;
                wrong[0].action = PlayerAction::Raise {
                    amount: Chips(committed_to),
                };
                wrong[0].pot_before = pot_before;
                wrong
            });
        for wrong in &variants {
            ctx.public_history = Some(wrong);
            assert_eq!(decision(&mut ctx), None);
        }
        ctx.public_history = Some(&facts);
        ctx.stack_chips -= 1;
        assert_eq!(decision(&mut ctx), None);
        ctx.stack_chips += 1;
        ctx.plain_cash = false;
        assert_eq!(decision(&mut ctx), None);

        let ring_players = vec![
            (PlayerId(1), Chips(1500), 2),
            (PlayerId(2), Chips(1000), 7),
            (PlayerId(3), Chips(1000), 8),
        ];
        let mut ring = GameHand::new_with_rng(
            ring_players,
            2,
            Chips(10),
            Chips(5),
            PokerRng::from_seed_bytes([2; 32]),
        );
        ring.start().unwrap();
        ring.apply_action(PlayerId(3), PlayerAction::Fold).unwrap();
        let ring_snap = ring.snapshot();
        let ring_seats: Vec<_> = ring_snap
            .players
            .iter()
            .map(|p| SeatPublic {
                seat: p.seat,
                stack: p.stack.0,
                committed_this_street: p.committed_this_street.0,
                folded: p.folded,
                all_in: p.all_in,
            })
            .collect();
        let ring_facts = public_action_history(
            [(Street::Preflop, ring.current_street_action_facts())],
            |id| {
                ring_snap
                    .players
                    .iter()
                    .find(|p| p.player_id == id)
                    .map(|p| p.seat)
            },
        )
        .unwrap();
        let ring_pairs = [(8, PlayerAction::Fold)];
        // The actual remaining players are SB and BB, in a three-handed
        // deal in which the BTN folded. Admit that precise three-hand path.
        ctx.plain_cash = true;
        ctx.seat = 2;
        ctx.position = Position::SmallBlind;
        ctx.table = TablePublic {
            dealer_seat: ring_snap.dealer_seat,
            seats: &ring_seats,
        };
        ctx.board = &ring_snap.board;
        ctx.stack_chips = 1495;
        ctx.stack_bb = 149;
        ctx.committed_this_street = Chips(5);
        ctx.current_bet = ring_snap.current_bet;
        ctx.to_call = Chips(5);
        ctx.pot = ring_snap.pot;
        ctx.min_raise_to = ring_snap.min_raise_to;
        ctx.street_actions = &ring_pairs;
        ctx.public_history = Some(&ring_facts);
        ctx.last_aggressor_seat = None;
        ctx.preflop_aggressor_seat = None;
        let cap = Move::Raise(200).action(&ctx, 5, 1500);
        assert_eq!(
            cap,
            PlayerAction::Raise {
                amount: Chips(1000)
            }
        );
        ring.clone().apply_action(PlayerId(1), cap).unwrap();
        let (play, _, source) = table_decision(&mut ctx, None);
        assert_eq!(source, "gtopen_preflop");
        ring.apply_action(PlayerId(1), play).unwrap();
        let mut two_deeper = ring_seats.clone();
        two_deeper[1].stack += 1;
        ctx.table.seats = &two_deeper;
        assert_eq!(decision(&mut ctx), None);
        let mut wrong_ring = ring_seats.clone();
        wrong_ring[2].folded = false;
        ctx.table.seats = &wrong_ring;
        assert_eq!(decision(&mut ctx), None);
    }

    #[test]
    fn blueprint_short_button_third_blind_open_and_cover_use_exact_public_state() {
        let original = [500, 1_500, 1_000];
        let unit = 5;
        let policy = policy_for(&original, unit).expect("short button, one covering player");
        #[cfg(target_endian = "little")]
        assert!(policy
            .nodes
            .iter()
            .all(|node| matches!(node.reach, Cow::Borrowed(_))
                && matches!(node.strategy, Cow::Borrowed(_))));
        assert!(std::ptr::eq(
            policy,
            policy_for(&[500, 1_000, 1_000], unit).unwrap()
        ));
        assert!(!std::ptr::eq(
            policy,
            policy_for(&[1_000, 500, 1_000], unit).unwrap()
        ));
        assert!(policy_for(&[501, 1_500, 1_000], unit).is_none());
        assert!(policy_for(&[500, 1_500, 1_001], unit).is_none());
        assert!(policy_for(&[990, 1_500], unit).is_some());
        assert!(policy_for(&[991, 1_500], unit).is_none());

        let players = original
            .iter()
            .enumerate()
            .map(|(i, &stack)| (PlayerId(i as u64 + 1), Chips(stack), i as u8))
            .collect();
        let mut hand = GameHand::new_with_rng(
            players,
            0,
            Chips(10),
            Chips(5),
            PokerRng::from_seed_bytes([4; 32]),
        );
        hand.start().unwrap();
        hand.apply_action(PlayerId(1), PlayerAction::Raise { amount: Chips(30) })
            .unwrap();
        let facts = public_action_history(
            [(Street::Preflop, hand.current_street_action_facts())],
            |id| Some((id.inner() - 1) as u8),
        )
        .unwrap();
        let root = &policy.nodes[policy.root];
        let next = root
            .following(&facts[0], 0, unit, 500, 100)
            .expect("3BB is in the solved tree");
        let sb = &policy.nodes[next];
        assert_eq!((sb.actor, &sb.posted[..]), (1, &[6, 1, 2][..]));
        assert!(sb.reach[168] >= MIN_HAND_REACH);
        let snap = hand.snapshot();
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
        let actions = [(0, PlayerAction::Raise { amount: Chips(30) })];
        let mut rng = StdRng::seed_from_u64(25);
        let mut ctx = crate::bot::shark::tests::opening(&mut rng, &snap.board, &seats);
        ctx.seat = 1;
        ctx.street = snap.street;
        ctx.sb_seat = 1;
        ctx.bb_seat = 2;
        ctx.table.dealer_seat = 0;
        ctx.big_blind = 10;
        ctx.position = Position::SmallBlind;
        ctx.stack_chips = 1495;
        ctx.stack_bb = 149;
        ctx.committed_this_street = Chips(5);
        ctx.current_bet = snap.current_bet;
        ctx.min_raise_to = snap.min_raise_to;
        ctx.can_raise = hand.betting_round_ref().unwrap().current_player_can_raise();
        ctx.pot = snap.pot;
        ctx.to_call = Chips(25);
        ctx.num_active = 3;
        ctx.street_actions = &actions;
        ctx.public_history = Some(&facts);
        ctx.hole = HoleCards::new(
            Card::new(Rank::Ace, Suit::Spades),
            Card::new(Rank::Ace, Suit::Hearts),
        );
        let (play, _, source) = table_decision(&mut ctx, None);
        assert_eq!(source, "gtopen_preflop");
        hand.apply_action(PlayerId(2), play).unwrap();
        // A 3BB open is also represented in the 100BB game. The short
        // button's independently solved opening range changes SB's reach.
        let equal_reply = RING3.nodes[RING3.root]
            .following(&facts[0], 0, unit, 1000, 200)
            .unwrap();
        assert_ne!(sb.reach[168], RING3.nodes[equal_reply].reach[168]);
    }

    #[test]
    fn blueprint_accepts_equivalent_engine_all_in_actions_for_short_seats() {
        for (short, wanted) in [(20, Move::Call), (35, Move::Raise(7))] {
            for action in [
                PlayerAction::Raise {
                    amount: Chips(short),
                },
                PlayerAction::AllIn,
            ] {
                let players = vec![
                    (PlayerId(1), Chips(1000), 0),
                    (PlayerId(2), Chips(short), 1),
                    (PlayerId(3), Chips(1000), 2),
                ];
                let mut hand = GameHand::new_with_rng(
                    players,
                    0,
                    Chips(10),
                    Chips(5),
                    PokerRng::from_seed_bytes([6; 32]),
                );
                hand.start().unwrap();
                hand.apply_action(PlayerId(1), PlayerAction::Raise { amount: Chips(25) })
                    .unwrap();
                hand.apply_action(PlayerId(2), action).unwrap();
                let facts = public_action_history(
                    [(Street::Preflop, hand.current_street_action_facts())],
                    |id| Some((id.inner() - 1) as u8),
                )
                .unwrap();
                let node = Node {
                    actor: 1,
                    posted: vec![5, 1, 2],
                    reach: Cow::Borrowed(&[]),
                    strategy: Cow::Borrowed(&[]),
                    edges: vec![Edge {
                        play: wanted,
                        next: Some(4),
                    }],
                };
                assert_eq!(
                    node.following(&facts[1], 1, 5, short, (short / 5) as u16),
                    Some(4)
                );
            }
        }
    }

    #[test]
    fn blueprint_public_trees_replay_every_exported_branch_on_engine() {
        fn visit(policy: &Blueprint, index: usize, hand: &GameHand, depths: &[u16]) {
            let node = &policy.nodes[index];
            let snap = hand.snapshot();
            let chips = u32::from(depths[node.actor]) * 5;
            let pid = PlayerId(node.actor as u64 + 1);
            assert_eq!(snap.current_actor, Some(pid));
            assert_eq!(snap.street, Street::Preflop);
            assert_eq!(
                snap.pot.0,
                node.posted.iter().copied().map(u32::from).sum::<u32>() * 5
            );
            for (i, p) in snap.players.iter().enumerate() {
                assert_eq!(p.committed_this_street.0, u32::from(node.posted[i]) * 5);
            }
            for (action_index, edge) in node.edges.iter().enumerate() {
                let play = match edge.play {
                    Move::Fold => PlayerAction::Fold,
                    Move::Check => PlayerAction::Check,
                    Move::Call => PlayerAction::Call,
                    Move::Raise(to) if u32::from(to) * 5 == chips => PlayerAction::AllIn,
                    Move::Raise(to) => PlayerAction::Raise {
                        amount: Chips(u32::from(to) * 5),
                    },
                };
                let mut next = hand.clone();
                next.apply_action(pid, play)
                    .unwrap_or_else(|e| panic!("node {index}, action {action_index}: {e}"));
                if let Some(child) = edge.next {
                    let facts = public_action_history(
                        [(Street::Preflop, next.street_action_facts(Street::Preflop))],
                        |id| Some((id.inner() - 1) as u8),
                    )
                    .unwrap();
                    assert_eq!(
                        node.following(
                            facts.last().unwrap(),
                            node.actor as u8,
                            5,
                            chips,
                            (chips / 5) as u16
                        ),
                        Some(child)
                    );
                    visit(policy, child, &next, depths);
                }
            }
        }
        let originals = [
            (2, 25, &*HU25),
            (2, 50, &*HU50),
            (2, 75, &*HU75),
            (2, 100, &*HU100),
            (2, 150, &*HU150),
            (2, 200, &*HU200),
            (3, 100, &*RING3),
            (4, 100, &*RING4),
            (5, 100, &*RING5),
            (6, 100, &*RING6),
            (7, 100, &*RING7),
            (8, 100, &*RING8),
            (9, 100, &*RING9),
        ];
        let profiles = originals
            .into_iter()
            .map(|(n, bb, p)| (vec![2 * bb; n], p))
            .chain(stack_policies::profiles().map(|(depths, p)| (depths.to_vec(), p)));
        for (depths, policy) in profiles {
            let n = depths.len();
            let players = depths
                .iter()
                .enumerate()
                .map(|(i, &d)| (PlayerId(i as u64 + 1), Chips(u32::from(d) * 5), i as u8))
                .collect();
            let mut hand = GameHand::new_with_rng(
                players,
                if n == 2 { 0 } else { n - 3 },
                Chips(10),
                Chips(5),
                PokerRng::from_seed_bytes([7; 32]),
            );
            hand.start().unwrap();
            visit(policy, policy.root, &hand, &depths);
        }
    }

    #[test]
    fn blueprint_selects_exact_stacks_after_the_blinds_change_hands() {
        for n in 3..=9 {
            // Starting from 100 BB, BTN is the previous SB and SB the previous BB.
            // Physical winning chips remain with the one covering seat.
            for (previous_winner, sb_stack) in [(n - 2, 1_005), (n - 3, 990), (n - 1, 990)] {
                let mut stacks = vec![1_000; n];
                stacks[n - 3] = if previous_winner == n - 3 { 1_010 } else { 995 };
                stacks[n - 2] = sb_stack;
                if previous_winner == n - 1 {
                    stacks[n - 1] = 1_015;
                }
                let policy = policy_for(&stacks, 5)
                    .unwrap_or_else(|| panic!("missing {n}-handed stack vector {stacks:?}"));
                let cap = contestable_cap(&stacks).unwrap();
                let normalized: Vec<_> = stacks.iter().map(|&s| (s.min(cap) / 5) as u16).collect();
                assert!(std::ptr::eq(
                    policy,
                    stack_policies::lookup(&normalized).unwrap()
                ));
                let short = (0..n).min_by_key(|&i| stacks[i]).unwrap();
                stacks[short] -= 1;
                assert!(policy_for(&stacks, 5).is_none(), "rounded {stacks:?}");
            }
        }
    }

    #[test]
    fn blueprint_policy_has_normalized_probabilities_and_hand_conditioned_reaches() {
        // Every supported raw depth must select its own policy, even when
        // the other chair covers it by a non-grid number of chips.
        for units in (20..=300).filter(|u| u % 2 == 0 || (160..=240).contains(u)) {
            assert!(
                policy_for(&[units * 5, 2603], 5).is_some(),
                "missing HU depth {units}/2"
            );
        }
        for n in 3..=9 {
            let first_short = if n <= 7 { 0 } else { n - 3 };
            for short in first_short..n {
                let mut stacks = vec![1000; n];
                stacks[short] = 500;
                assert!(
                    policy_for(&stacks, 5).is_some(),
                    "missing {n}-handed short seat {short}"
                );
            }
        }
        let started = std::time::Instant::now();
        let policy = decode(include_bytes!("hu100.json.gz"));
        eprintln!(
            "HU100 embedded policy decode in debug: {:?}",
            started.elapsed()
        );
        let root = &policy.nodes[policy.root];
        assert_eq!(root.posted, [1, 2]);
        assert!(root.reach.iter().all(|&p| p == 1.0));
        assert_eq!(root.sample(5, 0.5), Move::Fold); // 72o
        assert_ne!(root.sample(65, 0.5), Move::Fold); // 72s
        assert_ne!(root.sample(168, 0.2), root.sample(168, 0.8)); // AA mixes raise sizes
        let bb_vs_open = &HU100.nodes[root
            .edges
            .iter()
            .find(|e| e.play == Move::Raise(5))
            .unwrap()
            .next
            .unwrap()];
        assert!(bb_vs_open.reach.iter().all(|&p| p > 0.001 && p < 1.0));
        assert_ne!(bb_vs_open.reach[168], bb_vs_open.reach[5]); // BB blockers change SB opening mass
        for policy in [
            &*HU25, &*HU50, &*HU75, &*HU100, &*HU150, &*HU200, &*RING3, &*RING4, &*RING5, &*RING6,
            &*RING7, &*RING8, &*RING9,
        ]
        .into_iter()
        .chain(stack_policies::profiles().map(|(_, policy)| policy))
        {
            for (index, node) in policy.nodes.iter().enumerate() {
                assert!(node.actor < node.posted.len() && !node.edges.is_empty());
                assert_eq!(node.reach.len(), CLASSES);
                assert!(node.reach.iter().all(|p| (0.0..=1.0).contains(p)));
                assert_eq!(node.strategy.len(), CLASSES * node.edges.len());
                assert!(node.strategy.iter().all(|p| (0.0..=1.0).contains(p)));
                assert!(node
                    .edges
                    .iter()
                    .filter_map(|e| e.next)
                    .all(|next| next < index));
                for class in 0..CLASSES {
                    let sum: f64 = node.strategy[class..]
                        .iter()
                        .step_by(CLASSES)
                        .map(|&p| f64::from(p))
                        .sum();
                    assert!((sum - 1.0).abs() < 1e-5);
                }
            }
        }
    }
}
