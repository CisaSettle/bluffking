//! Evidence-limited adjustments to the Shark's range-based strategy.

use super::*;

/// Number of live players for a first responder facing an ordinary river
/// half-pot bet. A later caller/folder faced a different decision.
pub fn river_half_pot_opportunity(
    snap: &engine::GameSnapshot,
    actions: &[engine::StreetActionFact],
) -> Option<usize> {
    if snap.street != Street::River {
        return None;
    }
    let (bet, preceding) = actions.split_last()?;
    let live = snap.players.iter().filter(|p| !p.folded);
    let players = live.clone().count();
    ((2..=9).contains(&players)
        && bet.increased_bet
        && bet.to_call_before.0 == 0
        && bet.amount.0 > 0
        && bet.amount.0 == bet.pot_before.0.div_ceil(2)
        && (players == 2 || bet.pot_before.0.is_multiple_of(players as u32))
        && preceding.iter().all(|f| !f.increased_bet)
        && live
            .clone()
            .all(|p| !p.all_in && (p.player_id == bet.player_id || p.stack.0 > bet.amount.0))
        && live.clone().any(|p| p.player_id == bet.player_id)
        && snap
            .current_actor
            .is_some_and(|id| id != bet.player_id && live.clone().any(|p| p.player_id == id)))
    .then_some(players)
}

/// On a board shared by every possible hand, all players checking or calling
/// is an equilibrium. Every additional matched chip comes back even in a side
/// pot. A fold surrenders the folder's share to the remaining contenders.
pub(super) fn split_board_decision(
    ctx: &DecisionContext<'_>,
    opponents: Option<&OpponentModel<'_>>,
) -> Option<(PlayerAction, &'static str)> {
    if ctx.street != Street::River || ctx.table.seats.len() > 9 || !ctx.table.is_known() {
        return None;
    }
    let hero = ctx.table.seat_entry(ctx.seat)?;
    if hero.folded || hero.all_in {
        return None;
    }
    let villains = ctx
        .table
        .seats
        .iter()
        .filter(|s| s.seat != ctx.seat && !s.folded);
    let players = villains.clone().count() + 1;
    if players < 2 {
        return None;
    }
    let rank = rank_board_only(ctx.board)?;
    let locked = if matches!(rank, HandRank::Straight(9)) {
        let mut suits = [0; 4];
        for card in ctx.board.all_cards() {
            suits[card.suit as usize] += 1;
        }
        suits.into_iter().all(|count| count <= 2)
    } else {
        board_made_hand_is_unbeatable(rank, ctx.board)
    };
    if !locked {
        return None;
    }
    if ctx.to_call.0 > 0 {
        return Some((PlayerAction::Call, "split_board"));
    }
    if ctx.can_raise && ctx.current_bet.0 == 0 && ctx.committed_this_street.0 == 0 {
        let bet = river_bet_chips(ctx, 0.5);
        if let Some(model) = opponents.filter(|_| {
            ctx.min_raise_to.is_some_and(|min| bet >= min.0)
                && bet < ctx.stack_chips
                && villains.clone().all(|v| !v.all_in && bet < v.stack)
        }) {
            let next = ctx.table.first_river_responder(ctx.seat)?;
            // With three or more live seats, preserve the even starting
            // split across future side pots and odd-chip allocations.
            let overfolds = (players == 2 || ctx.pot.0.is_multiple_of(players as u32))
                && bet == ctx.pot.0.div_ceil(2)
                && model
                    .river_response_stats(next.seat)
                    .and_then(|s| s.river_half_pot_folds.as_ref())
                    .and_then(|reads| {
                        reads[players - 2].lower_frequency(model.min_cbet_sample.max(20))
                    })
                    .is_some_and(|lower| lower > 0.62);
            if overfolds {
                return Some((
                    PlayerAction::Raise { amount: Chips(bet) },
                    "split_board_overfolder",
                ));
            }
        }
    }
    Some((PlayerAction::Check, "split_board"))
}

/// Adapt only to voluntary heads-up responses observed on this same street
/// and at a comparable opening-bet size. Facing bets keeps the range/price
/// strategy; aggression alone never supplies evidence of bluffing.
pub(super) fn decide_shark(
    ctx: &mut DecisionContext<'_>,
    model: &OpponentModel<'_>,
    evidence: &mut Option<LimpDecisionEvidence>,
) -> PlayerAction {
    if let Some(action) = super::all_in::defend(ctx, model) {
        // Keep minimum defense for bets up to the pot. Overbet and squid
        // decisions follow the all-in model's own valuation.
        let squid = ctx
            .all_in_public
            .as_ref()
            .is_some_and(|p| p.squid.is_some());
        let call = ctx.to_call.0.min(ctx.stack_chips);
        let overbet = call > ctx.pot.0.saturating_sub(ctx.to_call.0);
        if action == PlayerAction::Fold && !squid && !overbet && river_min_defence_calls(ctx) {
            return call_or_all_in(ctx);
        }
        return action;
    }
    if let Some(action) = defend_limp_reraise(ctx, model, evidence) {
        return action;
    }
    if let Some(action) = defend_frequent_open_shove(ctx, model) {
        return action;
    }
    if ctx.street == Street::Preflop
        || ctx.to_call.0 != 0
        || ctx.current_bet.0 != 0
        || !ctx.can_raise
        || !ctx.table.is_known()
        || ctx.table.live_opponents(ctx.seat) != Some(1)
    {
        return chart_action(ctx);
    }

    let Some(villain) = ctx
        .table
        .seats
        .iter()
        .find(|seat| seat.seat != ctx.seat && !seat.folded)
        .copied()
    else {
        return chart_action(ctx);
    };
    if villain.all_in || villain.stack == 0 {
        return chart_action(ctx);
    }
    // Observations exclude all-in wagers and forced short-stack responses.
    // Do not extrapolate ordinary-bet reads to those different decisions.
    let read = |bet: u32| {
        if bet >= ctx.stack_chips || bet >= villain.stack {
            OpponentType::Unknown
        } else {
            classify_bet_response(model, villain.seat, ctx.street, bet, ctx.pot.0)
        }
    };
    let made = made_hand_bucket(ctx.hole, ctx.board);
    let base = if ctx.street == Street::River
        && ctx.turn_aggressor_seat == Some(ctx.seat)
        && model.river_response_stats(villain.seat).is_some()
    {
        let overfolds = |fraction| read(river_bet_chips(ctx, fraction)) == OpponentType::OverFolder;
        let boosts = [overfolds(RIVER_THIN_SIZE), overfolds(RIVER_POLAR_SIZE)];
        let reach = reach_profile(ctx);
        river_polar_bet_with_frequency(ctx, &reach, made, boosts, RIVER_THIN_VALUE_PERCENTILE)
    } else {
        chart_action(ctx)
    };

    if let PlayerAction::Raise { amount } = base {
        let bet = amount.0.saturating_sub(ctx.committed_this_street.0);
        if bet < ctx.stack_chips
            && bet < villain.stack
            && classify_bet_response(model, villain.seat, ctx.street, bet, ctx.pot.0)
                == OpponentType::Station
            && made == MadeHand::Air
            && draw_strength(ctx.hole, ctx.board) < 8
            && ctx.rng.gen_bool(0.5)
        {
            // Preserve value and live draws; a sticky caller only reduces the
            // existing no-equity bluff budget, never adds speculative calls.
            return PlayerAction::Check;
        }
        return base;
    }
    if base != PlayerAction::Check || ctx.street == Street::River {
        return base;
    }

    let can_semi_bluff = made == MadeHand::Air
        && draw_strength(ctx.hole, ctx.board) >= 8
        && (is_in_position(ctx)
            || (ctx.street == Street::Flop && ctx.preflop_aggressor_seat == Some(ctx.seat))
            || (ctx.street == Street::Turn && ctx.flop_aggressor_seat == Some(ctx.seat)));
    if can_semi_bluff {
        let fraction = if ctx.street == Street::Flop {
            1.0 / 3.0
        } else {
            2.0 / 3.0
        };
        let candidate = emit_bet(ctx, (f64::from(ctx.pot.0) * fraction).round() as u32);
        if let PlayerAction::Raise { amount } = candidate {
            let bet = amount.0.saturating_sub(ctx.committed_this_street.0);
            if bet < ctx.stack_chips
                && bet < villain.stack
                && classify_bet_response(model, villain.seat, ctx.street, bet, ctx.pot.0)
                    == OpponentType::OverFolder
                && ctx.rng.gen_bool(0.10)
            {
                return candidate;
            }
        }
    }
    base
}

/// Counter repeated open/three-bet shoves using a conservative public-history range.
/// This is an authored model, not a solved equilibrium or knowledge of cards.
/// Overcalls keep the base strategy. Waiting players require an extra poker
/// margin and a conservative comparison of possible squid winners.
fn defend_frequent_open_shove(
    ctx: &DecisionContext<'_>,
    model: &OpponentModel<'_>,
) -> Option<PlayerAction> {
    if ctx.street != Street::Preflop || ctx.to_call.0 == 0 || !ctx.table.is_known() {
        return None;
    }
    let aggressor = ctx.last_aggressor_seat?;
    let villain = ctx.table.seat_entry(aggressor)?;
    if villain.folded || !villain.all_in {
        return None;
    }
    let shove_index = ctx.street_actions.iter().rposition(|(seat, action)| {
        *seat == aggressor && matches!(action, PlayerAction::Raise { .. } | PlayerAction::AllIn)
    })?;
    // No calls, short raises or further bets after this shover.
    if ctx.street_actions[shove_index + 1..]
        .iter()
        .any(|(_, action)| !matches!(action, PlayerAction::Fold))
    {
        return None;
    }
    let bucket = shove_opportunity_bucket(
        villain.committed_this_street,
        ctx.big_blind,
        ctx.has_straddle,
        aggressor,
        ctx.street_actions[..shove_index].iter().cloned(),
    )?;
    let waiting = ctx
        .table
        .seats
        .iter()
        .filter(|s| s.seat != ctx.seat && s.seat != aggressor && !s.folded)
        .collect::<Vec<_>>();
    if waiting.iter().any(|s| s.all_in) {
        return None;
    }
    let read = model.by_seat.get(&aggressor)?.shove_read(bucket);
    let width = read.lower_frequency(model.min_sample)?;
    if width < 0.35 {
        return None;
    }

    let call = ctx.to_call.0.min(ctx.stack_chips);
    let hero_total = ctx.committed_this_street.0.saturating_add(ctx.stack_chips);
    // No speculative credit for an uncallable excess or another side pot.
    if ctx
        .table
        .seats
        .iter()
        .any(|s| s.seat != aggressor && s.committed_this_street > hero_total)
    {
        return None;
    }
    let refund = villain.committed_this_street.saturating_sub(hero_total);
    let pot = f64::from(ctx.pot.0.saturating_sub(refund)) + f64::from(call);
    if call == 0 || pot <= 0.0 {
        return None;
    }
    let estimate = shove_range_equity(ctx.hole, width);
    // 512 trials plus integer output: retain >5pp safety before widening.
    // Waiting players may wake up with value, so each costs another 4.5pp.
    // Counterfactual side-game value considers every live opponent as a
    // possible winner. Charge the full worst-case spread between those
    // outcomes; folding may itself allow another opponent to win a token.
    // The poker waiting-player margin remains an authored approximation.
    let bonus = if ctx.squid_call_value.win_bonus.is_finite() {
        ctx.squid_call_value.win_bonus.clamp(0.0, pot)
    } else {
        0.0
    };
    let loss_risk = if ctx.squid_call_value.loss_risk.is_finite() {
        ctx.squid_call_value.loss_risk.max(0.0)
    } else {
        return None;
    };
    let margin = 0.055 + waiting.len() as f64 * 0.045;
    // Chops earn a pot share, not squid tokens; the pending award is unchanged.
    let value = f64::from(estimate.combined_pct) / 100.0 * pot
        + f64::from(estimate.win_pct) / 100.0 * bonus
        - loss_risk;
    (value >= f64::from(call) + margin * (pot + bonus)).then(|| call_or_all_in(ctx))
}

pub(super) fn shove_range_equity(
    hole: HoleCards,
    width: f64,
) -> engine::solver::equity::EquityResult {
    use engine::solver::equity::{equity, EquityInput, OpponentSpec, RangeBucket};
    let order = rfi_priority_order(OpenBucket::Button);
    let range = order
        .iter()
        .filter_map(|&(high, low, suited)| {
            let weight = class_mix_in_order(order, (high, low, suited), width * 100.0);
            (weight > 0.0).then(|| RangeBucket {
                hand_key: if high == low {
                    format!("{high}{low}")
                } else {
                    format!("{high}{low}{}", if suited { "s" } else { "o" })
                },
                weight: weight as f32,
            })
        })
        .collect();
    // A separate fixed simulation seed neither consumes the action RNG nor
    // depends on the real deck, the dealer seed or any opponent's hole cards.
    equity(EquityInput {
        hero: hole,
        board: BoardCards::default(),
        opponents: OpponentSpec::Range(range),
        trials: 512,
        seed: 0x5348_4f56_455f_4556,
        early_stop: None,
    })
}

#[cfg(test)]
pub(super) mod tests;
