//! Big-pot discipline for every persona.
//!
//! The persona strategies decide by pot fractions and fixed defence shares
//! that were modelled at 100 big blinds. Once a bot that busts re-enters as
//! deep as the table, the same decision commits five to ten times the chips:
//! a pair that calls three streets, a cold call of a five-bet or a second
//! barrel into a check-raiser loses a 100 bb stack per mistake instead of a
//! few blinds.
//!
//! This layer reviews only an action that adds at least [`START_BB`] big
//! blinds, and only ever makes it smaller: a call may become a fold, a
//! postflop raise a call or a fold, a postflop opening bet a check. It prices
//! the action against the range each opponent's public line keeps
//! ([`squid_defense::showdown`], the same per-player reads and online line
//! pressure the token defence uses):
//!
//! * a call must be worth a multiple of its price that grows with the chips
//!   it adds (well below break-even at the threshold, where the persona's
//!   own read stands; above it for a full-size call, because an error of the
//!   range model costs in proportion to the chips), with less of the showdown
//!   chance realised the deeper the stack behind a hand that is rarely best,
//!   and more asked for every re-raise the pot has already seen;
//! * an opening bet or a raise by a hand that is ahead of the ranges in the
//!   pot is a value bet and is left alone; any other must pay through the
//!   folds it can expect, where the player who raised on the previous street
//!   folds half as often as its usual share.
//!
//! The review follows the online read of the line faced
//! ([`adapt::facing_pressure`], which reads a big wager on its own line): a
//! player whose big wagers keep winning when called has that line's range
//! narrowed, and one whose big wagers keep taking pots uncontested has it
//! widened and the ask relieved, so the bots call again once big bluffs
//! start to pay.
//!
//! Inputs are the bot's own cards, the board, public stacks and actions and
//! the public reads; no opponent card and no action-RNG draw. Below the size
//! threshold, without the public history (tournaments, self-play), preflop
//! raises, and an all-in the Shark already priced exactly are returned
//! unchanged.

use super::squid_defense::{call_field, showdown, CallField, SquidRead, MAX_FIELD, WAITING_FACTOR};
use super::*;

/// Chips added by one action below this many big blinds are never reviewed.
/// Also the size from which a human wager is read as its own "big" line.
pub const START_BB: f64 = 20.0;
/// The showdown share asked of a continue, as a multiple of its pot odds,
/// grows linearly from [`ASK`]`.0` at [`START_BB`] to [`ASK`]`.1` at this
/// many big blinds added by the action.
const FULL_BB: f64 = 150.0;
/// At the threshold only a continue the estimate puts far below its price is
/// overruled (the persona's read of a medium pot stands); a full-size one
/// must clear its price with room for the estimate's own error.
const ASK: (f64, f64) = (0.55, 1.25);
/// Share of the ask a fully positive online read of the line faced removes.
const ASK_RELIEF: f64 = 0.40;
/// Share of a call's showdown chance still realised against later betting,
/// preflop/flop/turn/river, with nothing left behind ...
const REALISED: [f64; 4] = [0.75, 0.80, 0.90, 1.0];
/// ... and what a stack many times the pot still behind takes from a hand
/// that is rarely best: the caller of a big wager faces the next, bigger one
/// with a capped hand. A hand that is usually best loses nothing to depth.
const DEEP_DISCOUNT: [f64; 4] = [0.30, 0.30, 0.20, 0.0];
/// An opening bet or a raise with at least this showdown share against the
/// range of one opponent is a value bet and is never reviewed further;
/// against more opponents see [`value_share`].
const VALUE_SHARE: f64 = 0.50;
/// Added to the ask's factor per re-raise on the street; see [`reraised`].
const RERAISE_ASK: f64 = 0.20;
/// Share of its usual fold share to an opening bet kept by the player who
/// made the previous street's last wager increase: its range is the part
/// that bet or raised, whether or not it has checked since.
const INITIATIVE_FOLD: f64 = 0.50;
/// The token of a squid round, with any 7-2 bounty, counts for at most this
/// many pots here: a big pot is decided by its chips.
const BONUS_POTS: f64 = 1.0;
/// Share of its range the player a raise is made over continues with at a
/// neutral read, and how far a fully positive / negative read moves it.
const RAISE_CONTINUES: f64 = 0.50;
const RAISE_CONTINUES_SWING: (f64, f64) = (0.20, 0.30);
/// Share of the answers to an opening bet that raise it off the pot.
const BET_RAISED: f64 = 0.15;
/// Prior fold share to an opening bet when a read is unusable.
const FOLD_PRIOR: f64 = 0.40;

/// Review a big action; see the module docs. Anything else is unchanged.
pub(super) fn discipline(
    ctx: &mut DecisionContext<'_>,
    action: PlayerAction,
    opponents: Option<&OpponentModel<'_>>,
) -> PlayerAction {
    if ctx.squid_defense.history.is_none()
        || !ctx.table.is_known()
        || ctx.all_in_evidence.is_some()
        || ctx.big_blind == 0
        || ctx.stack_chips == 0
    {
        return action;
    }
    let committed = ctx.committed_this_street.0;
    let call = ctx.to_call.0.min(ctx.stack_chips);
    let risk = match action {
        PlayerAction::Fold | PlayerAction::Check | PlayerAction::Blind { .. } => return action,
        PlayerAction::Call => call,
        PlayerAction::AllIn => ctx.stack_chips,
        PlayerAction::Raise { amount } => amount.0.saturating_sub(committed).min(ctx.stack_chips),
    };
    let bb = f64::from(ctx.big_blind);
    if f64::from(risk) < START_BB * bb {
        return action;
    }
    let live: Vec<&SeatPublic> = ctx
        .table
        .seats
        .iter()
        .filter(|s| s.seat != ctx.seat && !s.folded)
        .collect();
    if live.is_empty() {
        return action;
    }
    if call == 0 {
        return if ctx.street == Street::Preflop || opening_bet_pays(ctx, &live, risk, opponents) {
            action
        } else {
            PlayerAction::Check
        };
    }
    // What calling is worth beyond its price (None = no usable estimate).
    let Some(call_edge) = call_edge(ctx, &live, call) else {
        return action;
    };
    let call_reviewed = f64::from(call) >= START_BB * bb;
    let call_ok = !call_reviewed || call_edge >= 0.0;
    if risk <= call {
        // A call, or an all-in that only calls.
        return if call_ok { action } else { PlayerAction::Fold };
    }
    // A raise over a wager. Preflop re-raises stay with the range charts.
    if ctx.street == Street::Preflop {
        return action;
    }
    if raise_pays(ctx, &live, risk, call, call_edge.max(0.0)) {
        action
    } else if call_ok {
        super::preflop::call_or_all_in(ctx)
    } else {
        PlayerAction::Fold
    }
}

/// The value-bet share against `opponents` ranges at once: the same edge
/// over an even split of the pot that [`VALUE_SHARE`] is heads-up.
fn value_share(opponents: usize) -> f64 {
    2.0 * VALUE_SHARE / (opponents.max(1) + 1) as f64
}

/// Street as an index: preflop 0 through river 3.
pub(super) fn street_index(street: Street) -> usize {
    match street {
        Street::Preflop => 0,
        Street::Flop => 1,
        Street::Turn => 2,
        Street::River => 3,
    }
}

/// Multiple of the break-even price asked of `risk` chips: by size, then
/// relieved by a positive online read `s` of the line faced (`0` = none). A
/// negative read already narrows that line's range.
fn ask(risk: f64, bb: f64, s: f64) -> f64 {
    let size = ((risk / bb - START_BB) / (FULL_BB - START_BB)).clamp(0.0, 1.0);
    let s = if s.is_finite() {
        s.clamp(0.0, 1.0)
    } else {
        0.0
    };
    (ASK.0 + (ASK.1 - ASK.0) * size) * (1.0 - ASK_RELIEF * s)
}

/// Share of the showdown chance a continue realises with `behind` chips left
/// to bet into a pot of `pot` on later streets, for a hand best `share` of
/// the time.
fn realised(street: usize, behind: f64, pot: f64, closed: bool, share: f64) -> f64 {
    if closed || behind <= 0.0 || pot <= 0.0 {
        return 1.0;
    }
    let capped = (1.0 - share / VALUE_SHARE).clamp(0.0, 1.0);
    REALISED[street] - DEEP_DISCOUNT[street] * capped * behind / (behind + pot)
}

fn bonus(ctx: &DecisionContext<'_>, pot: f64) -> f64 {
    let bonus = seven_deuce::win_bonus(ctx);
    if bonus.is_finite() && bonus > 0.0 {
        bonus.min(BONUS_POTS * pot)
    } else {
        0.0
    }
}

/// Value of calling `call` minus the asked multiple of the call.
fn call_edge(ctx: &DecisionContext<'_>, live: &[&SeatPublic], call: u32) -> Option<f64> {
    let CallField {
        pot,
        aggressor,
        in_pot,
        unmodelled,
    } = call_field(ctx, live, call)?;
    let (win, share) = showdown(ctx, &in_pot, aggressor, None)?;
    let street = street_index(ctx.street);
    let behind = f64::from(ctx.stack_chips.saturating_sub(call));
    let closed = street == 3 || behind <= 0.0 || live.iter().all(|s| s.all_in);
    let factor =
        WAITING_FACTOR.powi(unmodelled as i32) * realised(street, behind, pot, closed, share);
    let value = factor * (share * pot + win * bonus(ctx, pot));
    let call = f64::from(call);
    let bb = f64::from(ctx.big_blind);
    Some(value - call * ask(call, bb, ctx.line_pressure) * reraised(ctx))
}

/// Factor on the ask for each wager increase on this street after the first:
/// the range model narrows a re-raiser once, however often the pot has been
/// re-raised, so a four- or five-bet is stronger than it prices.
fn reraised(ctx: &DecisionContext<'_>) -> f64 {
    let raises = ctx
        .squid_defense
        .history
        .as_deref()
        .unwrap_or_default()
        .iter()
        .filter(|a| a.street == ctx.street && a.increased_bet)
        .count();
    1.0 + RERAISE_ASK * raises.saturating_sub(1) as f64
}

fn read(ctx: &DecisionContext<'_>, seat: u8) -> SquidRead {
    ctx.squid_defense
        .reads
        .iter()
        .find(|r| r.seat == seat)
        .copied()
        .unwrap_or_else(|| SquidRead::prior(seat))
}

/// Whether an opening bet of `bet` is worth at least a check: every opponent
/// folds its own public share to an opening bet on this street (moved by the
/// online result of the bots' opening bets into the humans in the pot), the
/// rest continue with the strongest part of their ranges, and part of the
/// answers raise the bet off the pot.
fn opening_bet_pays(
    ctx: &DecisionContext<'_>,
    live: &[&SeatPublic],
    bet: u32,
    opponents: Option<&OpponentModel<'_>>,
) -> bool {
    let mut field: Vec<&SeatPublic> = live.to_vec();
    field.truncate(MAX_FIELD);
    let street = street_index(ctx.street);
    let previous = [Street::Preflop, Street::Preflop, Street::Flop, Street::Turn][street];
    let initiative = ctx
        .squid_defense
        .history
        .as_deref()
        .unwrap_or_default()
        .iter()
        .rev()
        .find(|a| a.street == previous && a.increased_bet)
        .map(|a| a.seat);
    let folds: Vec<f64> = field
        .iter()
        .map(|s| {
            if s.all_in {
                return 0.0;
            }
            let fold = read(ctx, s.seat).opening_bet_fold(ctx, s, live.len(), bet, opponents);
            if Some(s.seat) == initiative {
                fold * INITIATIVE_FOLD
            } else {
                fold
            }
        })
        .collect();
    let fold: f64 = folds.iter().product::<f64>()
        * WAITING_FACTOR.powi(live.len().saturating_sub(field.len()) as i32);
    let Some((win, share)) = showdown(ctx, &field, None, None) else {
        return true;
    };
    if share >= value_share(field.len()) {
        return true;
    }
    let continues: Vec<f64> = folds.iter().map(|f| 1.0 - f).collect();
    let Some((called_win, called_share)) = showdown(ctx, &field, None, Some(&continues)) else {
        return true;
    };
    let pot = f64::from(ctx.pot.0);
    let b = f64::from(bet);
    let token = bonus(ctx, pot);
    let behind = f64::from(ctx.stack_chips);
    let closed = street == 3;
    let check = realised(street, behind, pot, closed, share) * (share * pot + win * token);
    let after = realised(
        street,
        behind - b,
        pot + 2.0 * b,
        closed || behind <= b,
        called_share,
    );
    let called = after * (called_share * (pot + 2.0 * b) + called_win * token) - b;
    let answered = (1.0 - BET_RAISED) * called - BET_RAISED * b;
    let bet_value = fold * (pot + token) + (1.0 - fold) * answered;
    bet_value >= check
}

/// Whether raising `risk` (the call included) over the wager faced is worth
/// at least the better of calling and folding.
fn raise_pays(
    ctx: &DecisionContext<'_>,
    live: &[&SeatPublic],
    risk: u32,
    call: u32,
    call_edge: f64,
) -> bool {
    let aggressor = ctx
        .last_aggressor_seat
        .filter(|seat| live.iter().any(|s| s.seat == *seat));
    let mut field: Vec<&SeatPublic> = live.to_vec();
    field.sort_by_key(|s| Some(s.seat) != aggressor);
    field.truncate(MAX_FIELD);
    let street = street_index(ctx.street);
    let s = if ctx.line_pressure.is_finite() {
        ctx.line_pressure.clamp(-1.0, 1.0)
    } else {
        0.0
    };
    let continues: Vec<f64> = field
        .iter()
        .map(|seat| {
            if seat.all_in {
                1.0
            } else if Some(seat.seat) == aggressor {
                if s > 0.0 {
                    RAISE_CONTINUES - RAISE_CONTINUES_SWING.0 * s
                } else {
                    RAISE_CONTINUES - RAISE_CONTINUES_SWING.1 * s
                }
            } else {
                let fold = read(ctx, seat.seat).folds[street - 1];
                1.0 - if fold.is_finite() {
                    fold.clamp(0.0, 1.0)
                } else {
                    FOLD_PRIOR
                }
            }
        })
        .collect();
    match showdown(ctx, &field, aggressor, None) {
        Some((_, share)) if share < value_share(field.len()) => {}
        _ => return true,
    }
    let fold: f64 = continues.iter().map(|c| 1.0 - c).product();
    let Some((called_win, called_share)) = showdown(ctx, &field, aggressor, Some(&continues))
    else {
        return true;
    };
    let pot = f64::from(ctx.pot.0);
    let risk = f64::from(risk);
    // One opponent matches the raise; further callers only add dead money.
    let final_pot = pot + risk + (risk - f64::from(call));
    let token = bonus(ctx, final_pot);
    let behind = f64::from(ctx.stack_chips) - risk;
    let after = realised(
        street,
        behind,
        final_pot,
        street == 3 || behind <= 0.0,
        called_share,
    );
    // The chips a continued raise loses carry the ask of a call of that
    // size (never less than their face value) and of the re-raises already
    // in, so the chips it risks are never priced more leniently than a
    // call's; its fold equity is the only extra credit.
    let bb = f64::from(ctx.big_blind);
    let price = risk * ask(risk, bb, s).max(1.0) * reraised(ctx);
    let called = after * (called_share * final_pot + called_win * token) - price;
    let raise = fold * (pot + token) + (1.0 - fold) * called;
    raise >= call_edge
}
