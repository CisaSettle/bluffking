//! Same-player opening shoves in ordinary two-player, 100 BB hands.
use super::{forced_blind, DecisionContext, OpponentModel, PlayerAction, ShoveRead, Street};
use rand::Rng;
use serde::{Deserialize, Serialize};
use uuid::Uuid;

/// Every voluntary first action by this SB in the matching game is an
/// opportunity, including a fold, limp or smaller open. Keep its account
/// identity even before a seat's cross-table profile finishes loading.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct OpenJams {
    owner: Uuid,
    read: ShoveRead,
}

impl OpenJams {
    pub(crate) fn observe(slot: &mut Option<Self>, owner: Uuid, jam: bool) {
        let mut read = slot
            .filter(|s| s.owner == owner)
            .map(|s| s.read)
            .unwrap_or_default();
        read.observe(jam);
        *slot = Some(Self { owner, read });
    }

    pub(super) fn merged(older: Option<Self>, newer: Option<Self>) -> Option<Self> {
        match (older, newer) {
            (Some(a), Some(b)) if a.owner == b.owner => Some(Self {
                owner: b.owner,
                read: a.read.then(b.read),
            }),
            (older, newer) => newer.or(older),
        }
    }
}

/// The baseline for this exact state has already chosen Fold. A sufficiently
/// wide modeled shove range may add a call. Bound the *incremental* expected
/// loss relative to that fold even if villain's range assumption is wrong.
pub(super) fn adjust_fold(ctx: &mut DecisionContext<'_>, model: &OpponentModel<'_>) -> bool {
    let Some(probability) = call_probability(ctx, model) else {
        return false;
    };
    ctx.rng.gen_bool(probability)
}

pub(super) fn matches(ctx: &DecisionContext<'_>) -> bool {
    call_price(ctx).is_some()
}

fn call_price(ctx: &DecisionContext<'_>) -> Option<f64> {
    let bb = ctx.big_blind;
    if !ctx.plain_cash
        || ctx.street != Street::Preflop
        || ctx.has_straddle
        || ctx.board.count() != 0
        || ctx.hole.card1 == ctx.hole.card2
        || bb < 2
        || !bb.is_multiple_of(2)
        || ctx.table.seats.len() != 2
        || ctx.seat != ctx.bb_seat
        || ctx.sb_seat == ctx.bb_seat
        || ctx.table.dealer_seat != ctx.sb_seat
        || ctx.can_raise
    {
        return None;
    }
    let total = bb.checked_mul(100)?;
    total.checked_mul(2)?;
    let opening_pot = bb.checked_add(bb / 2)?;
    let hero = ctx.table.seat_entry(ctx.bb_seat)?;
    let opener = ctx.table.seat_entry(ctx.sb_seat)?;
    let [open] = ctx.public_history? else {
        return None;
    };
    if !matches!(ctx.street_actions, [(seat, action)] if *seat == opener.seat && *action == open.action)
        || ctx.last_aggressor_seat != Some(opener.seat)
        || open.seat != opener.seat
        || open.street != Street::Preflop
        || !open.full_raise
        || !open.increased_bet
        || !(open.action == PlayerAction::AllIn
            || matches!(open.action, PlayerAction::Raise { amount } if amount.0 == total))
        || open.committed_to != total
        || open.amount != total - bb / 2
        || open.to_call_before != bb / 2
        || open.pot_before != opening_pot
        || !opener.all_in
        || opener.folded
        || opener.stack != 0
        || opener.committed_this_street != total
        || hero.folded
        || hero.all_in
        || hero.committed_this_street != bb
        || hero.stack != total - bb
        || ctx.stack_chips != hero.stack
        || ctx.committed_this_street.0 != bb
        || ctx.current_bet.0 != total
        || ctx.to_call.0 != hero.stack
        || ctx.pot.0 != total.checked_add(bb)?
    {
        return None;
    }
    Some(f64::from(hero.stack) / (2.0 * f64::from(total)))
}

fn call_probability(ctx: &DecisionContext<'_>, model: &OpponentModel<'_>) -> Option<f64> {
    let price = call_price(ctx)?;
    let owner = model.opening_sb_user?;
    let history = model.by_seat.get(&ctx.sb_seat)?.hu100_open_jams?;
    if history.owner != owner {
        return None;
    }
    let frequency = history.read.lower_frequency(model.min_sample.max(12))?;
    let (minimum, uniform) = forced_blind::equity_bounds(ctx.hole);
    bounded_mix(frequency, minimum, uniform, price)
}

fn bounded_mix(observed_lower: f64, minimum: f64, uniform: f64, price: f64) -> Option<f64> {
    // Conditioning on our two blockers removes 101 of 1,326 equally likely
    // opener combos. In the worst case *all* those hands would have jammed.
    let width = ((observed_lower * 1326.0 - 101.0) / 1225.0).clamp(0.0, 1.0);
    if width == 0.0 {
        return None;
    }
    // Assign a full win to every excluded holding. Remaining equity mass
    // divided by the retained width lower-bounds equity against the jams.
    let modeled = minimum.max(1.0 - (1.0 - uniform) / width);
    if modeled <= price + 0.005 {
        return None;
    }
    let risk = price - minimum;
    Some(if risk <= 0.02 { 1.0 } else { 0.02 / risk })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::bot::{
        lookup_profile, public_action_history, table_decision, BotPersona, OpponentStats,
        SeatPublic, TablePublic,
    };
    use engine::{card::parse_card, Chips, GameHand, HoleCards, PlayerId, PokerRng, Position};
    use rand::{rngs::StdRng, SeedableRng};
    use std::collections::HashMap;

    #[test]
    fn open_jam_read_preserves_opportunities_owner_and_risk_budget() {
        let first = Uuid::from_u128(1);
        let second = Uuid::from_u128(2);
        let mut slot = None;
        for _ in 0..32 {
            OpenJams::observe(&mut slot, first, true);
        }
        let wide = slot.unwrap().read.lower_frequency(12).unwrap();
        let (min, mean) = forced_blind::equity_bounds(HoleCards::new(
            parse_card("Kd").unwrap(),
            parse_card("Qd").unwrap(),
        ));
        let mixed = bounded_mix(wide, min, mean, 0.495).unwrap();
        assert!(mixed * (0.495 - min) <= 0.020000000000001);
        assert!(mixed * (1.0 - (1.0 - mean) / ((1326.0 * wide - 101.0) / 1225.0) - 0.495) > 0.0);
        let older = slot;
        for _ in 0..16 {
            OpenJams::observe(&mut slot, first, false);
        }
        assert!(bounded_mix(
            slot.unwrap().read.lower_frequency(12).unwrap(),
            min,
            mean,
            0.495
        )
        .is_none());
        OpenJams::observe(&mut slot, second, true);
        assert!(slot.unwrap().read.lower_frequency(12).is_none());
        assert_eq!(OpenJams::merged(older, slot), slot);
        let mut recent = None;
        OpenJams::observe(&mut recent, first, false);
        let merged = OpenJams::merged(older, recent).unwrap();
        assert_eq!(merged.read.recent, u32::MAX - 1);
    }

    #[test]
    fn engine_open_jam_matches_its_dealt_owner_and_changes_a_baseline_fold() {
        let small = PlayerId(85);
        let big = PlayerId(24);
        let mut hand = GameHand::new_with_rng(
            vec![(small, Chips(1000), 2), (big, Chips(1000), 7)],
            0,
            Chips(10),
            Chips(5),
            PokerRng::from_seed_bytes([1; 32]),
        );
        hand.start().unwrap();
        hand.apply_action(small, PlayerAction::AllIn).unwrap();
        let snap = hand.snapshot();
        assert_eq!(snap.current_actor, Some(big));
        let seat_of = |id| {
            snap.players
                .iter()
                .find(|p| p.player_id == id)
                .map(|p| p.seat)
        };
        let public = hand.current_street_action_facts();
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
        let mut rng = StdRng::seed_from_u64(201);
        let hero = snap.players.iter().find(|p| p.player_id == big).unwrap();
        let mut ctx = DecisionContext {
            seat: hero.seat,
            position: Position::BigBlind,
            persona: BotPersona::Shark,
            profile: lookup_profile(BotPersona::Shark),
            rng: &mut rng,
            plain_cash: true,
            sb_seat: snap.dealer_seat,
            bb_seat: hero.seat,
            num_active: 2,
            stack_bb: hero.stack.0 / 10,
            stack_chips: hero.stack.0,
            big_blind: 10,
            has_straddle: false,
            street: snap.street,
            board: &snap.board,
            hole: HoleCards::new(parse_card("Js").unwrap(), parse_card("Ts").unwrap()),
            pot: snap.pot,
            to_call: Chips(snap.current_bet.0 - hero.committed_this_street.0),
            current_bet: snap.current_bet,
            can_raise: hand.betting_round_ref().unwrap().current_player_can_raise(),
            min_raise_to: snap.min_raise_to,
            committed_this_street: hero.committed_this_street,
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
        assert_eq!(call_price(&ctx), Some(0.495));
        let owner = Uuid::from_u128(88);
        let mut stats = OpponentStats::default();
        for _ in 0..32 {
            OpenJams::observe(&mut stats.hu100_open_jams, owner, true);
        }
        let by_seat = HashMap::from([(ctx.sb_seat, stats)]);
        let mut model = OpponentModel::new(&by_seat);
        assert!(call_probability(&ctx, &model).is_none());
        model.opening_sb_user = Some(Uuid::nil());
        assert!(call_probability(&ctx, &model).is_none());
        assert_eq!(table_decision(&mut ctx, Some(&model)).0, PlayerAction::Fold);
        model.opening_sb_user = Some(owner);
        assert!(call_probability(&ctx, &model).is_some());
        let mut found_call = false;
        for _ in 0..256 {
            let (action, _, source) = table_decision(&mut ctx, Some(&model));
            if action == PlayerAction::Call {
                assert!(matches!(
                    source,
                    "range_hu_open_jam_read" | "gtopen_hu_open_jam_read"
                ));
                hand.validate_action(big, &action).unwrap();
                found_call = true;
                break;
            }
        }
        assert!(found_call);
        ctx.stack_chips += 1;
        assert!(!matches(&ctx));
        ctx.stack_chips -= 1;
        ctx.plain_cash = false;
        assert!(!matches(&ctx));
        ctx.plain_cash = true;
        ctx.public_history = None;
        assert!(!matches(&ctx));
    }
}
