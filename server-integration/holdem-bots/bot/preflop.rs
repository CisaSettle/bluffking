//! Preflop for bot.

use super::*;

pub(super) fn decide_preflop(ctx: &mut DecisionContext<'_>) -> PlayerAction {
    let action = decide_preflop_chart(ctx);
    defend_cheap_preflop_price(ctx, action)
}

/// Full-raise depth this street (see `decide_preflop_chart`).
pub(super) fn preflop_raise_depth(ctx: &DecisionContext<'_>) -> usize {
    ctx.street_actions
        .iter()
        .filter(|(_, a)| matches!(a, PlayerAction::Raise { .. } | PlayerAction::AllIn))
        .count()
}

/// Assumed top-of-range width (fraction of combos) for the player whose
/// raise we face, by full-raise depth. Deliberately tight: a value-heavy
/// estimate lowers our equity, so the price floor below only fires when the
/// price is clearly too good to fold even against a strong range. An open
/// uses the reference (ClubRegular) opening width for the opener's seat.
fn price_floor_villain_width(ctx: &DecisionContext<'_>, depth: usize) -> f64 {
    match depth {
        0 | 1 => preflop_opener_seat(ctx)
            .and_then(|seat| ctx.table.position_of(seat))
            .filter(|&p| p != Position::BigBlind)
            .map_or(0.30, |p| {
                f64::from(lookup_profile(BotPersona::ClubRegular).rfi_pct[rfi_knob_index(p)])
                    / 100.0
            }),
        2 => 0.12,
        3 => 0.06,
        _ => 0.035,
    }
}

/// Required equity above which the floor never runs: every hand the charts
/// already continue with covers these prices, so no simulation is needed.
const PRICE_FLOOR_MAX_REQUIRED: f64 = 0.42;

/// Price floor (owner 2026-11-11 exploit report): the depth charts are
/// standard 100bb, ante-free, standard-sizing ranges. A minimum re-raise,
/// or any raise into a pot swollen by antes/straddles, offered a price at
/// which those charts still folded ~90% of the time, so min-raising every
/// hand and min-4-betting every 3-bet won uncontested pots (and squid marks)
/// almost always. A chart `Fold` facing a raise is overturned into a call
/// when the hand's estimated equity against a tight range for that depth,
/// discounted for realisation, extra callers and players still to act,
/// clears the pot odds. Never adds a raise; never touches a non-fold.
fn defend_cheap_preflop_price(ctx: &DecisionContext<'_>, action: PlayerAction) -> PlayerAction {
    if action != PlayerAction::Fold || ctx.to_call.0 == 0 {
        return action;
    }
    let Some(aggressor) = ctx.last_aggressor_seat.filter(|&s| s != ctx.seat) else {
        return action;
    };
    let depth = preflop_raise_depth(ctx);
    if depth == 0 {
        return action;
    }
    let call = ctx.to_call.0.min(ctx.stack_chips);
    if call == 0 {
        return action;
    }
    let hero_total = ctx.committed_this_street.0.saturating_add(ctx.stack_chips);
    // Another seat covering us also puts chips we cannot win into the pot;
    // pricing that side pot is out of scope, so keep the chart decision.
    if ctx.table.seats.iter().any(|s| {
        s.seat != aggressor
            && s.seat != ctx.seat
            && !s.folded
            && s.committed_this_street > hero_total
    }) {
        return action;
    }
    // Any seat's chips above our total (the aggressor's uncallable excess,
    // or a folded raiser's) sit outside the pot this call can win.
    let refund: u32 = ctx
        .table
        .seats
        .iter()
        .filter(|s| s.seat != ctx.seat)
        .map(|s| s.committed_this_street.saturating_sub(hero_total))
        .fold(0u32, u32::saturating_add);
    let pot_after_call = f64::from(ctx.pot.0.saturating_sub(refund)) + f64::from(call);
    if pot_after_call <= 0.0 {
        return action;
    }
    let required = f64::from(call) / pot_after_call;
    let pressure = ctx.line_pressure;
    if required >= adapt::floor_cap(PRICE_FLOOR_MAX_REQUIRED, pressure) {
        return action;
    }

    // Other live seats: callers already in this pot lower our equity; seats
    // still to act may re-raise behind us.
    let mut callers_in = 0u32;
    let mut waiting = 0u32;
    for s in ctx.table.seats.iter() {
        if s.seat == ctx.seat || s.seat == aggressor || s.folded {
            continue;
        }
        if s.all_in || s.committed_this_street >= ctx.current_bet.0 {
            callers_in += 1;
        } else {
            waiting += 1;
        }
    }
    // Facing a single open, defence is shared by every seat still to act:
    // the table as a whole denies the steal, so each early seat may keep its
    // chart fold. Only the seat closing the action prices the open alone —
    // unless this open has been stealing from the table (positive pressure).
    if depth == 1 && waiting > 0 && pressure <= 0.0 {
        return action;
    }
    let aggressor_all_in = preflop_aggressor_is_all_in(ctx);
    let showdown = (call >= ctx.stack_chips || aggressor_all_in) && waiting == 0 && callers_in == 0;
    let realisation = if showdown {
        1.0
    } else if is_in_position(ctx) {
        0.80
    } else {
        0.70
    };
    let width = adapt::range_width(price_floor_villain_width(ctx, depth), pressure);
    let equity = f64::from(shove_range_equity(ctx.hole, width).combined_pct) / 100.0;
    let effective = equity * realisation * 0.72_f64.powi(callers_in as i32);
    let margin = 0.02 + 0.015 * f64::from(waiting);
    if effective >= required + margin {
        call_or_all_in(ctx)
    } else {
        action
    }
}

fn decide_preflop_chart(ctx: &mut DecisionContext<'_>) -> PlayerAction {
    let bb = ctx.big_blind;

    // Count how many aggressive actions have occurred this street (excluding the
    // initial open which is raise_depth=0).
    // depth 0 = no raise yet (RFI or BB option)
    // depth 1 = one open raise (facing an open)
    // depth 2 = open + 3-bet (facing a 3-bet)
    // depth 3 = open + 3-bet + 4-bet (facing a 4-bet)
    // depth 4+ = cap → no further aggression (AA/KK call, the rest fold)
    //
    // Production `street_actions` is already normalized by the engine: only a
    // full all-in raise retains `AllIn`; all-in calls and non-full short raises
    // project to `Call`. Counting `Raise | AllIn` is therefore full-raise depth,
    // while `last_aggressor_seat` independently retains a short raiser.
    let raise_depth = preflop_raise_depth(ctx);

    let last_raiser_is_us = ctx.last_aggressor_seat == Some(ctx.seat);

    // Route by raise depth. Only route into 3-bet/4-bet handlers when we are
    // NOT the last aggressor (i.e., the raise was not our own).
    if !last_raiser_is_us {
        match raise_depth {
            0 => {
                // No open yet — RFI or BB option.
            }
            1 => {
                // Facing an open raise.
                return decide_facing_open(ctx);
            }
            2 => {
                // Facing a 3-bet.
                return decide_vs_3bet(ctx);
            }
            3 => {
                // Facing a 4-bet (ADR-024 §4.1.3 cap): sized 5-bet when deep,
                // jam when shallow; see `decide_vs_4bet`.
                return decide_vs_4bet(ctx);
            }
            _ => {
                // 5-bet+ depth — recursion cap (ADR-024 §4.1.3): never raise
                // again. The cap used to fold EVERY hand here, including AA/KK
                // that had just 4-bet and now faced a 5-bet jam (dual-AI
                // requirements pass, 2026-10-14). Premium pairs call / call
                // all-in instead; everything else still folds.
                return decide_vs_5bet_plus(ctx);
            }
        }
    }

    // Raise-first-in or BB option (raise_depth == 0, or we were the last raiser).
    decide_rfi(ctx, bb)
}

/// Raise-first-in: no aggressor yet (only blinds/folds before us).
///
/// GTO-anchored pass L1 (2026-10-14): the opening range is the top
/// `rfi_pct[position]` percent of the 1,326 starting combinations taken in the
/// position-specific priority order of [`rfi_priority_order`]; only the
/// boundary hand class is mixed, at its exact combo fraction. Every persona
/// shares the orderings and differs only in width, so the caricature ordering
/// (Maniac > LAG > Reg > TAG > Nit > Station) is preserved while the
/// ClubRegular widths are the standard 6-max / 100bb / rake-free reference.
pub(super) fn decide_rfi(ctx: &mut DecisionContext<'_>, bb: u32) -> PlayerAction {
    // BB option: everyone limped, BB can raise or check.
    if ctx.seat == ctx.bb_seat && ctx.to_call.0 == 0 {
        return decide_bb_option(ctx, bb);
    }

    let open_pct = ctx.profile.rfi_pct[rfi_knob_index(ctx.position)] as f64;
    let bucket = OpenBucket::for_position(ctx.position);
    let mix = open_range_mix(ctx.hole, bucket, open_pct);
    let should_open = mix >= 1.0 || (mix > 0.0 && ctx.rng.gen_bool(mix));

    if !should_open {
        // A calling station is loose-PASSIVE, not a tighter Nit. Keep its low
        // persona RFI/PFR range, but limp the rest of the top STATION_CONTINUE_PCT
        // VPIP range (caricature: the loosest seat at the table). This is the only
        // persona that limps by design; the others preserve the raise-or-fold
        // contract from ADR-024/047. The limp band keeps the legacy class ranking.
        if ctx.persona == BotPersona::Station && ctx.to_call.0 > 0 {
            let station_vpip_threshold =
                ((169.0_f64 * STATION_CONTINUE_PCT / 100.0).round() as u32).min(169);
            if hand_rank_169(ctx.hole) < station_vpip_threshold {
                return PlayerAction::Call;
            }
        }

        // Not in opening range — fold if there's a call required, else check.
        if ctx.to_call.0 > 0 {
            PlayerAction::Fold
        } else {
            PlayerAction::Check
        }
    } else {
        let raise_chips = rfi_open_chips(ctx, bb);
        emit_raise(ctx, raise_chips)
    }
}

/// Raise-to of a first-in open. Open sizes (standard practice): the late
/// seats open smaller, the early seats and the SB open larger. The SB uses
/// the plain OOP size (the former +0.5bb bump is gone: 3bb for the reference
/// persona).
pub(super) fn rfi_open_chips(ctx: &DecisionContext<'_>, bb: u32) -> u32 {
    let open_bb = if ctx.seat != ctx.sb_seat && opens_from_late_seat(ctx.position) {
        ctx.profile.open_size_bb_ip as f64
    } else {
        ctx.profile.open_size_bb_oop as f64
    };
    (bb as f64 * open_bb).round() as u32
}

/// Index into `PersonaProfile::rfi_pct` (UTG=0, LJ=1, HJ=2, CO=3, BTN=4, SB=5).
/// LJ (lojack) replaces MP in nine-handed games. BB maps to the UTG
/// width and is handled by `decide_bb_option`.
pub(super) fn rfi_knob_index(position: Position) -> usize {
    match position {
        Position::Utg | Position::UtgPlus1 | Position::UtgPlus2 => 0,
        Position::Lojack => 1,
        Position::Hijack => 2,
        Position::Cutoff => 3,
        Position::Dealer => 4,
        Position::SmallBlind => 5,
        Position::BigBlind => 0,
    }
}

/// BB option after all limpers: check or raise with boosted range.
pub(super) fn decide_bb_option(ctx: &mut DecisionContext<'_>, bb: u32) -> PlayerAction {
    // Use the BB's UTG RFI% as a base (position index 0), boosted by 5
    // because limpers signal weakness.
    let base_pct = ctx.profile.rfi_pct[0] as f64;
    let open_pct = (base_pct + 5.0).min(100.0);
    let mix = open_range_mix(ctx.hole, OpenBucket::Early, open_pct);
    let should_raise = mix >= 1.0 || (mix > 0.0 && ctx.rng.gen_bool(mix)) || ctx.rng.gen_bool(0.05);

    if !should_raise {
        PlayerAction::Check
    } else {
        // Size: 4 BB (1 BB per limper + base). Simplified to 4 BB flat.
        let raise_chips = bb.saturating_mul(4);
        emit_raise(ctx, raise_chips)
    }
}

/// Facing an open raise — call, 3-bet, or fold.
pub(super) fn decide_facing_open(ctx: &mut DecisionContext<'_>) -> PlayerAction {
    let strength = preflop_hand_strength(ctx.hole, ctx.position);

    // An all-in is a showdown decision, not a draw from the ordinary
    // polarized 3-bet chart. Keep short-stack value even where that chart
    // mixes the hand into calls/folds, and price speculative hands against
    // the shover's stack rather than a deeper player waiting behind.
    if preflop_aggressor_is_all_in(ctx) && strength != PreflopStrength::Premium {
        if is_short_stack_three_bet_value(ctx) || is_cheap_short_shove_blind_call(ctx) {
            return call_or_all_in(ctx);
        }
        if speculative_preflop_call(ctx) == PlayerAction::Fold {
            return PlayerAction::Fold;
        }
    }

    // Persona-defining exceptions must run before the shared chart. Without
    // these, Station/Nit used the exact same continue range and a 200 BB LAG
    // made the exact same decision as a 100 BB LAG.
    if ctx.persona == BotPersona::Station {
        return decide_station_facing_open(ctx);
    }
    if ctx.persona == BotPersona::Nit {
        return decide_nit_facing_open(ctx, strength);
    }
    if ctx.persona == BotPersona::Lag
        && ctx.stack_bb >= 150
        && ctx.num_callers_so_far > 0
        && is_lag_deep_squeeze_candidate(ctx.hole)
    {
        // Deep-stack LAGs attack capped caller ranges with suited hands that
        // retain equity when called. Derive frequency from the existing 3-bet
        // knob; no new hidden persona table is introduced.
        let squeeze_prob = (ctx.profile.three_bet_pct as f64 / 15.0).min(0.85);
        if ctx.rng.gen_bool(squeeze_prob) {
            let base_multiple = if is_in_position(ctx) { 3.5 } else { 4.5 };
            let caller_extra = ctx.num_callers_so_far as f64 * 0.75;
            let depth_extra = ctx.stack_bb.saturating_sub(150).min(150) as f64 / 300.0;
            let squeeze_to = (ctx.current_bet.0 as f64
                * (base_multiple + caller_extra + depth_extra))
                .round() as u32;
            return emit_selected_three_bet(ctx, squeeze_to);
        }
    }

    // GTO-anchored pass L2: hero-seat × opener-seat widths on two authored
    // priority orders (`THREE_BET_ORDER`, `CALL_ORDER`). The ClubRegular widths
    // are the standard-practice reference; `three_bet_pct` scales the 3-bet
    // width (Reg 6 = 1.0) and the BTN opening width scales the flat width
    // (Reg 45% = 1.0), so the caricatures stay ordered without new knobs.
    let spot = facing_open_spot(ctx);
    let FacingOpenWidths {
        mut three_bet_pct,
        mut call_pct,
    } = reference_facing_open_widths(spot);
    let callers = ctx.num_callers_so_far;
    if callers > 0 {
        // Squeeze: value-heavier re-raise, and in position the flat gets a
        // better price with more money already in the middle.
        three_bet_pct *= 0.6;
        if spot.hero == HeroBucket::InPosition {
            call_pct *= 1.2;
        }
    }
    three_bet_pct = (three_bet_pct * three_bet_scale(ctx.profile)).min(45.0);
    // Even the loosest caricature keeps a fold range facing a raise.
    call_pct = (call_pct * preflop_looseness(ctx.profile))
        .min(60.0)
        .min(85.0 - three_bet_pct);

    let three_bet_mix = range_mix_in_order(three_bet_order(), ctx.hole, three_bet_pct);
    if three_bet_mix >= 1.0 || (three_bet_mix > 0.0 && ctx.rng.gen_bool(three_bet_mix)) {
        return emit_standard_three_bet(ctx);
    }

    if ctx.to_call.0 == 0 {
        return PlayerAction::Check;
    }
    // The flat budget counts only the probability mass that can still reach
    // a call: a class that 3-bets with probability p contributes (1 − p) of
    // its combos, so the aggregate call frequency equals `call_pct` exactly.
    let call_mix = range_mix_in_order_residual(call_order(), ctx.hole, call_pct, |class| {
        1.0 - class_mix_in_order(three_bet_order(), class, three_bet_pct)
    });
    if call_mix >= 1.0 || (call_mix > 0.0 && ctx.rng.gen_bool(call_mix)) {
        // Price guard: a flat is a standard-sized spot. Against an oversized
        // open (worse than ~40% pot odds) the flat range collapses to a fold;
        // the 3-bet branch above already kept every premium.
        if pot_odds_pct(ctx.pot, ctx.to_call) <= 40 {
            return PlayerAction::Call;
        }
    }
    PlayerAction::Fold
}

/// Which seat family the hero defends from when facing one open.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HeroBucket {
    /// A non-blind seat (always acts after the opener postflop).
    InPosition,
    SmallBlind,
    BigBlind,
}

/// Which seat family opened.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OpenerBucket {
    /// UTG, UTG+1, UTG+2, LJ.
    Early,
    /// HJ (also the fallback when the opener's seat is unknown).
    Middle,
    Cutoff,
    Button,
    SmallBlind,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct FacingOpenSpot {
    pub hero: HeroBucket,
    pub opener: OpenerBucket,
}

/// Reference (ClubRegular) widths, percent of the 1,326 combinations.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct FacingOpenWidths {
    pub three_bet_pct: f64,
    pub call_pct: f64,
}

/// Seat of the original open: the first full raise on the street.
pub(super) fn preflop_opener_seat(ctx: &DecisionContext<'_>) -> Option<u8> {
    ctx.street_actions
        .iter()
        .find(|(_, a)| matches!(a, PlayerAction::Raise { .. } | PlayerAction::AllIn))
        .map(|(seat, _)| *seat)
}

pub(super) fn facing_open_spot(ctx: &DecisionContext<'_>) -> FacingOpenSpot {
    let hero = if ctx.seat == ctx.bb_seat {
        HeroBucket::BigBlind
    } else if ctx.seat == ctx.sb_seat {
        HeroBucket::SmallBlind
    } else {
        HeroBucket::InPosition
    };
    let opener_seat = preflop_opener_seat(ctx).or(ctx.last_aggressor_seat);
    let opener = match opener_seat {
        Some(seat) if seat == ctx.sb_seat => OpenerBucket::SmallBlind,
        Some(seat) => match ctx.table.position_of(seat) {
            Some(Position::Utg | Position::UtgPlus1 | Position::UtgPlus2 | Position::Lojack) => {
                OpenerBucket::Early
            }
            Some(Position::Hijack) => OpenerBucket::Middle,
            Some(Position::Cutoff) => OpenerBucket::Cutoff,
            Some(Position::Dealer) => OpenerBucket::Button,
            Some(Position::SmallBlind) => OpenerBucket::SmallBlind,
            Some(Position::BigBlind) | None => OpenerBucket::Middle,
        },
        None => OpenerBucket::Middle,
    };
    FacingOpenSpot { hero, opener }
}

/// Standard-practice defend widths for the reference persona (6-max, 100bb,
/// rake-free, 2.5bb/3bb opens). A non-blind hero can only face an earlier
/// seat; an impossible pairing falls back to the nearest defined one.
pub fn reference_facing_open_widths(spot: FacingOpenSpot) -> FacingOpenWidths {
    use HeroBucket as H;
    use OpenerBucket as O;
    let (three_bet_pct, call_pct) = match (spot.hero, spot.opener) {
        (H::InPosition, O::Early) => (6.0, 8.0),
        (H::InPosition, O::Middle) => (7.0, 9.0),
        (H::InPosition, O::Cutoff | O::Button | O::SmallBlind) => (8.0, 11.0),
        (H::SmallBlind, O::Early) => (7.0, 1.5),
        (H::SmallBlind, O::Middle) => (9.0, 2.0),
        (H::SmallBlind, O::Cutoff) => (11.0, 3.0),
        (H::SmallBlind, O::Button | O::SmallBlind) => (13.0, 4.0),
        (H::BigBlind, O::Early) => (7.0, 18.0),
        (H::BigBlind, O::Middle) => (8.0, 22.0),
        (H::BigBlind, O::Cutoff) => (9.0, 26.0),
        (H::BigBlind, O::Button) => (12.0, 30.0),
        (H::BigBlind, O::SmallBlind) => (14.0, 36.0),
    };
    FacingOpenWidths {
        three_bet_pct,
        call_pct,
    }
}

/// Re-raise width multiplier: the ClubRegular knob (6) is the reference.
pub(super) fn three_bet_scale(profile: &PersonaProfile) -> f64 {
    profile.three_bet_pct as f64 / 6.0
}

/// Flat-call width multiplier derived from the persona's BTN opening width
/// (ClubRegular 45% is the reference): looser openers also defend wider.
pub(super) fn preflop_looseness(profile: &PersonaProfile) -> f64 {
    profile.rfi_pct[4] as f64 / 45.0
}

/// 4-bet bluff width (percent of combos) for the reference persona; scaled by
/// `four_bet_pct` (Reg 3 = 1.0). Personas without `allows_4bet_bluffs` keep a
/// token A5s-only band; a knob below 3 (Nit, Station) never bluff-4-bets.
pub(super) fn four_bet_bluff_pct(profile: &PersonaProfile) -> f64 {
    if profile.four_bet_pct < 3 {
        return 0.0;
    }
    let scale = profile.four_bet_pct as f64 / 3.0;
    let base = if profile.allows_4bet_bluffs {
        0.6
    } else {
        0.15
    };
    base * scale
}

/// Calling-station preflop contract (the ONE Station-facing-open path): continue
/// the top `STATION_CONTINUE_PCT` of hands as a flat, but only 3-bet the top
/// `three_bet_pct` slice. The large gap between VPIP and PFR is what makes this
/// persona loose-passive rather than merely tight. Caricature: the continue band
/// is cranked wide (single lever shared with the limp band in `decide_rfi`), so
/// the Station flats the widest of any persona — VPIP top of the table. The
/// looser price tolerance (45% vs the tight personas' ~35%) matches the "won't
/// fold a decent price" fish archetype.
pub(super) fn decide_station_facing_open(ctx: &mut DecisionContext<'_>) -> PlayerAction {
    if ctx.to_call.0 == 0 {
        return PlayerAction::Check;
    }

    let rank = hand_rank_169(ctx.hole);
    let three_bet_threshold =
        ((169.0 * ctx.profile.three_bet_pct as f64 / 100.0).round() as u32).max(1);
    // Premium value hands are never folded merely because a shove creates a
    // steep raw pot-odds percentage. If raising is forbidden, `emit_raise`
    // converts this value branch to a call; otherwise Station keeps its narrow
    // value 3-bet range.
    if rank < three_bet_threshold {
        return emit_standard_three_bet(ctx);
    }

    let continue_threshold = ((169.0_f64 * STATION_CONTINUE_PCT / 100.0).round() as u32).min(169);
    let pot_odds_pct = pot_odds_pct(ctx.pot, ctx.to_call);
    if rank >= continue_threshold || pot_odds_pct > 45 {
        return PlayerAction::Fold;
    }

    PlayerAction::Call
}

/// Nit/rock preflop contract (the ONE Nit-facing-open path): premiums 3-bet,
/// only the top `NIT_DEFEND_PCT` may call, and polarized bluff candidates such as
/// A5s are folded rather than randomized. Caricature: the defend band is
/// tightened so the Nit defends the narrowest of any persona — VPIP bottom of
/// the table, the "folds everything rock".
pub(super) fn decide_nit_facing_open(
    ctx: &mut DecisionContext<'_>,
    strength: PreflopStrength,
) -> PlayerAction {
    if strength == PreflopStrength::Premium {
        return emit_standard_three_bet(ctx);
    }
    if ctx.to_call.0 == 0 {
        return PlayerAction::Check;
    }

    let defend_threshold = ((169.0_f64 * NIT_DEFEND_PCT / 100.0).round() as u32).min(169);
    let pot_odds_pct = pot_odds_pct(ctx.pot, ctx.to_call);
    if hand_rank_169(ctx.hole) < defend_threshold && pot_odds_pct <= 35 {
        PlayerAction::Call
    } else {
        PlayerAction::Fold
    }
}

/// Standard 3-bet size: 3x the open in position, 4x out of position, plus
/// one open per caller already in the pot (squeeze).
pub(super) fn emit_standard_three_bet(ctx: &mut DecisionContext<'_>) -> PlayerAction {
    let open_size = ctx.current_bet.0 as f64;
    let multiple = if is_in_position(ctx) { 3.0 } else { 4.0 } + ctx.num_callers_so_far as f64;
    emit_selected_three_bet(ctx, (open_size * multiple).round() as u32)
}

/// A light re-raise needs a player who can still fold, and must not become a
/// stack-off merely because the sizing helper rounds it up to an all-in.
/// The value range widens at short effective stacks; it is not limited to the
/// same premium hands that can continue for a full 100bb.
fn emit_selected_three_bet(ctx: &mut DecisionContext<'_>, raise_to: u32) -> PlayerAction {
    if preflop_hand_strength(ctx.hole, ctx.position) != PreflopStrength::Premium {
        let short_stack_value = is_short_stack_three_bet_value(ctx);
        if !preflop_bluff_can_raise(ctx) {
            return if short_stack_value {
                // A 20bb all-in may have a deeper player behind it. Having
                // value against that shove permits calling its price, not
                // committing our full stack into an unrelated side pot.
                call_or_all_in(ctx)
            } else {
                speculative_preflop_call(ctx)
            };
        }
        if preflop_raise_would_jam(ctx, raise_to) && !short_stack_value {
            return speculative_preflop_call(ctx);
        }
    }
    emit_preflop_raise(ctx, raise_to)
}

/// Depth against the player whose wager we face. Another deep seat must not
/// make a short all-in look like a cheap speculative call.
fn preflop_aggressor_effective_stack(ctx: &DecisionContext<'_>) -> u32 {
    let own = ctx.stack_chips.saturating_add(ctx.committed_this_street.0);
    ctx.last_aggressor_seat
        .and_then(|seat| ctx.table.effective_stack_vs(ctx.seat, seat))
        .map_or_else(
            || {
                if preflop_aggressor_is_all_in(ctx) {
                    ctx.current_bet.0.min(own)
                } else {
                    effective_stack_chips(ctx)
                }
            },
            |stack| stack.min(own),
        )
}

/// These hands are value continuations at shallow depth, even though they
/// are below the deep-stack JJ+/AK premium band. A shove uses this value
/// distinction directly; normal opens still use the position/persona chart.
fn is_short_stack_three_bet_value(ctx: &DecisionContext<'_>) -> bool {
    let effective = preflop_aggressor_effective_stack(ctx);
    let bb = ctx.big_blind.max(1);
    if effective > bb.saturating_mul(20) {
        return false;
    }
    let (high, low) = sorted_ranks(ctx.hole);
    let pair = high == low;
    let suited = ctx.hole.card1.suit == ctx.hole.card2.suit;
    // At up to 20bb, 88+, AJs+, AQo+ and KQs belong to the value band,
    // rather than sharing the restrictions for wheel-ace/connector bluffs.
    if (pair && high >= Rank::Eight)
        || (high == Rank::Ace && (low >= Rank::Queen || (suited && low >= Rank::Jack)))
        || (suited && high == Rank::King && low == Rank::Queen)
    {
        return true;
    }
    // With no more than 10bb at risk, add 55+, ATs, AJo, KQo and KJs.
    effective <= bb.saturating_mul(10)
        && ((pair && high >= Rank::Five)
            || (high == Rank::Ace && (low == Rank::Jack || (suited && low == Rank::Ten)))
            || (high == Rank::King && (low == Rank::Queen || (suited && low == Rank::Jack))))
}

/// A blind facing a tiny open shove has already paid much of the price.
/// The general 20%-of-stack flat limit would incorrectly fold a playable
/// holding for 1bb into 3.5bb merely because the opponent started with 2bb.
/// Keep this exception to one extra blind at a favorable price, with no
/// extension to expensive 10bb+ shoves or disconnected trash.
fn is_cheap_short_shove_blind_call(ctx: &DecisionContext<'_>) -> bool {
    let bb = ctx.big_blind.max(1);
    (ctx.seat == ctx.bb_seat || ctx.seat == ctx.sb_seat)
        && preflop_aggressor_effective_stack(ctx) <= bb.saturating_mul(2)
        && ctx.to_call.0 <= bb
        && pot_odds_pct(ctx.pot, ctx.to_call) <= 25
        && preflop_hand_strength(ctx.hole, ctx.position) != PreflopStrength::Weak
}

/// Raise legality alone does not establish fold equity: a multiway pot may
/// permit raising after the aggressor is all-in. Do not use that aggressor's
/// ordinary bluff range to pay for a guaranteed showdown.
fn preflop_bluff_can_raise(ctx: &DecisionContext<'_>) -> bool {
    if !ctx.can_raise || ctx.to_call.0 >= ctx.stack_chips || preflop_aggressor_is_all_in(ctx) {
        return false;
    }
    !ctx.table.is_known()
        || ctx
            .table
            .seats
            .iter()
            .any(|s| s.seat != ctx.seat && !s.folded && !s.all_in && s.stack > 0)
}

fn preflop_aggressor_is_all_in(ctx: &DecisionContext<'_>) -> bool {
    ctx.last_aggressor_seat.is_some_and(|aggressor| {
        ctx.table.seat_entry(aggressor).is_some_and(|s| s.all_in)
            || ctx
                .street_actions
                .iter()
                .rev()
                .find(|(seat, _)| *seat == aggressor)
                .is_some_and(|(_, action)| matches!(action, PlayerAction::AllIn))
    })
}

/// A selected bluff can still complete a cheap short raise, but it must earn
/// that call from its price instead of falling through `emit_raise` to Call.
fn speculative_preflop_call(ctx: &DecisionContext<'_>) -> PlayerAction {
    if ctx.to_call.0 == 0 {
        return PlayerAction::Check;
    }
    let affordable = ctx.to_call.0 as f64
        <= preflop_aggressor_effective_stack(ctx) as f64 * PREFLOP_FLAT_STACK_FRACTION;
    if affordable && pot_odds_pct(ctx.pot, ctx.to_call) <= 40 {
        call_or_all_in(ctx)
    } else {
        PlayerAction::Fold
    }
}

/// Fraction of the effective stack beyond which a preflop raise is made as a
/// jam instead (standard practice: do not leave a sliver behind).
pub(super) const PREFLOP_JAM_STACK_FRACTION: f64 = 0.40;

/// Effective stack for the hero this street: against the deepest live
/// opponent when the table is known, else the hero's own stack.
pub(super) fn effective_stack_chips(ctx: &DecisionContext<'_>) -> u32 {
    let own = ctx.stack_chips.saturating_add(ctx.committed_this_street.0);
    ctx.table
        .effective_stack(ctx.seat)
        .map_or(own, |eff| eff.min(own))
}

/// The raise-to `emit_raise` would actually send for `requested`: lifted to
/// the legal minimum and the half-pot floor, capped at the chips available.
pub(super) fn normalized_raise_to(ctx: &DecisionContext<'_>, requested: u32) -> u32 {
    let available = ctx.committed_this_street.0.saturating_add(ctx.stack_chips);
    let min_rt = ctx
        .min_raise_to
        .map(|c| c.0)
        .unwrap_or(ctx.current_bet.0.saturating_add(ctx.big_blind));
    let half_pot_raise_to = ctx
        .committed_this_street
        .0
        .saturating_add(ctx.to_call.0)
        .saturating_add((ctx.pot.0 as f64 * 0.50).round() as u32);
    requested.max(min_rt).max(half_pot_raise_to).min(available)
}

/// Whether a preflop raise to `requested` (after normalisation) would commit
/// more than `PREFLOP_JAM_STACK_FRACTION` of the effective stack.
pub(super) fn preflop_raise_would_jam(ctx: &DecisionContext<'_>, requested: u32) -> bool {
    let effective = effective_stack_chips(ctx);
    (normalized_raise_to(ctx, requested) as f64) > effective as f64 * PREFLOP_JAM_STACK_FRACTION
}

/// Preflop raise that becomes an all-in once the (normalised) raise-to would
/// commit more than `PREFLOP_JAM_STACK_FRACTION` of the effective stack.
/// Raise permission still comes from the engine (`emit_raise`).
pub(super) fn emit_preflop_raise(ctx: &mut DecisionContext<'_>, raise_to: u32) -> PlayerAction {
    if !ctx.can_raise {
        return emit_raise(ctx, raise_to);
    }
    if preflop_raise_would_jam(ctx, raise_to) {
        return PlayerAction::AllIn;
    }
    emit_raise(ctx, raise_to)
}

/// Fraction of the effective stack a flat call may cost before the flatting
/// range collapses (only hands that re-raise continue against a jam-sized
/// 3-bet).
pub(super) const PREFLOP_FLAT_STACK_FRACTION: f64 = 0.20;

/// Continue against a wager without raising: all-in when the call covers the
/// stack, otherwise a plain call (check when nothing is owed).
pub(super) fn call_or_all_in(ctx: &DecisionContext<'_>) -> PlayerAction {
    if ctx.to_call.0 == 0 {
        PlayerAction::Check
    } else if ctx.to_call.0 >= ctx.stack_chips {
        PlayerAction::AllIn
    } else {
        PlayerAction::Call
    }
}

/// Suited wheel aces plus low/medium suited connectors and one-gappers are the
/// extra deep-stack squeeze candidates: they have playability when called but
/// stay out of the ordinary 100 BB LAG branch.
pub(super) fn is_lag_deep_squeeze_candidate(hole: HoleCards) -> bool {
    let (high, low) = sorted_ranks(hole);
    if hole.card1.suit != hole.card2.suit {
        return false;
    }

    let suited_wheel_ace = high == Rank::Ace && low <= Rank::Five;
    let high_idx = rank_index(high);
    let low_idx = rank_index(low);
    let gap = high_idx - low_idx;
    let suited_connector_or_gapper = (1..=2).contains(&gap)
        && low_idx >= rank_index(Rank::Four)
        && high_idx <= rank_index(Rank::Ten);
    suited_wheel_ace || suited_connector_or_gapper
}

/// Facing a 3-bet (GTO-anchored pass L2). Value 4-bets QQ+/AK for every
/// persona; polarised 4-bet bluffs from `FOUR_BET_BLUFF_ORDER` at the
/// persona's bluff width; flats from `THREE_BET_CALL_ORDER` (wider in
/// position); a hand the chart folds still calls when it lies inside the
/// bot's own entry range's minimum defense ([`defends_reraise_by_mdf`]);
/// everything else folds (a remaining fold may still become a call in
/// `defend_cheap_preflop_price` when the 3-bet is priced cheaply). 4-bet =
/// 2.3x the 3-bet, or a jam once that would commit more than 40% of the
/// effective stack (bluffs never jam).
pub(super) fn decide_vs_3bet(ctx: &mut DecisionContext<'_>) -> PlayerAction {
    let (h, l) = sorted_ranks(ctx.hole);
    let is_value_4bet = (h == l && h >= Rank::Queen) || (h == Rank::Ace && l == Rank::King);
    let four_bet_to = (ctx.current_bet.0 as f64 * 2.3).round() as u32;

    if is_value_4bet {
        return emit_preflop_raise(ctx, four_bet_to);
    }

    let bluff_mix = range_mix_in_order(
        four_bet_bluff_order(),
        ctx.hole,
        four_bet_bluff_pct(ctx.profile),
    );
    // Bluffs never jam: once the (normalised) 4-bet would commit more than
    // the jam fraction, the bluff branch is skipped entirely.
    let jam_required = preflop_raise_would_jam(ctx, four_bet_to);
    if !jam_required && (bluff_mix >= 1.0 || (bluff_mix > 0.0 && ctx.rng.gen_bool(bluff_mix))) {
        return if preflop_bluff_can_raise(ctx) {
            emit_preflop_raise(ctx, four_bet_to)
        } else {
            speculative_preflop_call(ctx)
        };
    }

    // The flat range exists only at a flat's price: against a jam-sized
    // 3-bet (the call costs more than PREFLOP_FLAT_STACK_FRACTION of the
    // effective stack) nothing outside the re-raise range continues, so a
    // skipped bluff can never turn into a stack-off call.
    let flat_affordable =
        (ctx.to_call.0 as f64) <= effective_stack_chips(ctx) as f64 * PREFLOP_FLAT_STACK_FRACTION;
    let call_pct = if is_in_position(ctx) { 5.0 } else { 3.0 } * preflop_looseness(ctx.profile);
    let call_mix = range_mix_in_order(three_bet_call_order(), ctx.hole, call_pct.min(20.0));
    if flat_affordable && (call_mix >= 1.0 || (call_mix > 0.0 && ctx.rng.gen_bool(call_mix))) {
        return call_or_all_in(ctx);
    }
    if defends_reraise_by_mdf(ctx) {
        return call_or_all_in(ctx);
    }
    if ctx.to_call.0 == 0 {
        PlayerAction::Check
    } else {
        PlayerAction::Fold
    }
}

/// Facing a 5-bet or deeper (recursion cap): no further raise is ever emitted.
/// AA/KK continue (a plain call, or an all-in call when the jam covers us);
/// every other hand folds.
pub(super) fn decide_vs_5bet_plus(ctx: &DecisionContext<'_>) -> PlayerAction {
    let (h, l) = sorted_ranks(ctx.hole);
    if h == l && h >= Rank::King {
        if ctx.to_call.0 == 0 {
            PlayerAction::Check
        } else if ctx.to_call.0 >= ctx.stack_chips {
            PlayerAction::AllIn
        } else {
            PlayerAction::Call
        }
    } else if ctx.to_call.0 == 0 {
        PlayerAction::Check
    } else {
        PlayerAction::Fold
    }
}

/// 5-bet size as a multiple of the 4-bet when stacks are deep enough that a
/// sized 5-bet still leaves real play behind.
const FIVE_BET_MULTIPLIER: f64 = 2.2;

/// Share of deep-stack AA/KK that flat the 4-bet instead of 5-betting, so the
/// deep flatting range is not capped at QQ/AK.
const DEEP_PREMIUM_FLAT_PCT: f64 = 0.15;

/// Facing a 4-bet after our 3-bet (GTO-anchored pass L2).
///
/// Shallow (a 2.2x 5-bet would commit more than `PREFLOP_JAM_STACK_FRACTION`
/// of the effective stack, i.e. ordinary ~100bb play): AA/KK jam; QQ and AKs
/// continue every time, half as a jam and half as a call; AKo calls half the
/// time; JJ calls 30%; personas with `allows_4bet_bluffs` 5-bet-jam A5s at a
/// small frequency; any other hand (including the AKo/JJ fold share) calls
/// inside the 3-bet range's minimum defense ([`defends_reraise_by_mdf`]),
/// else folds.
///
/// Deep (owner 2026-11-11: a 10x-pot jam into a 4-bet pot only gets called by
/// AA): no hand jams. AA/KK make a sized, non-all-in 5-bet (and sometimes
/// flat); QQ/AKs flat; the A5s bluff uses the same sized 5-bet. A sized 5-bet
/// is still the last raise: facing a 6-bet, `decide_vs_5bet_plus` only calls
/// with AA/KK, so the recursion cap of ADR-024 §4.1.3 still holds.
///
/// A remaining fold may still become a call in `defend_cheap_preflop_price`
/// when the 4-bet is priced cheaply.
pub(super) fn decide_vs_4bet(ctx: &mut DecisionContext<'_>) -> PlayerAction {
    let (h, l) = sorted_ranks(ctx.hole);
    let suited = ctx.hole.card1.suit == ctx.hole.card2.suit;
    let pair = h == l;
    let five_bet_to = (f64::from(ctx.current_bet.0) * FIVE_BET_MULTIPLIER).round() as u32;
    // Depth is measured against the 4-bettor (a deep player waiting behind a
    // short 4-bettor does not make a sized 5-bet sensible), and against the
    // table-wide effective stack (`preflop_raise_would_jam`).
    let deep = ctx.can_raise
        && !preflop_aggressor_is_all_in(ctx)
        && !preflop_raise_would_jam(ctx, five_bet_to)
        && f64::from(normalized_raise_to(ctx, five_bet_to))
            <= f64::from(preflop_aggressor_effective_stack(ctx)) * PREFLOP_JAM_STACK_FRACTION;
    let jam = |ctx: &DecisionContext<'_>| {
        if ctx.can_raise {
            PlayerAction::AllIn
        } else {
            call_or_all_in(ctx)
        }
    };

    if pair && h >= Rank::King {
        if deep {
            return if ctx.rng.gen_bool(DEEP_PREMIUM_FLAT_PCT) {
                call_or_all_in(ctx)
            } else {
                emit_preflop_raise(ctx, five_bet_to)
            };
        }
        return jam(ctx);
    }
    if (pair && h == Rank::Queen) || (h == Rank::Ace && l == Rank::King && suited) {
        if deep {
            return call_or_all_in(ctx);
        }
        return if ctx.rng.gen_bool(0.5) {
            jam(ctx)
        } else {
            call_or_all_in(ctx)
        };
    }
    // AKo and JJ still call at the chart frequency; the chart's fold share
    // falls through to the minimum-defense check below like every other
    // holding, so their total continue rate can exceed the chart's.
    if h == Rank::Ace && l == Rank::King && ctx.rng.gen_bool(0.5) {
        return call_or_all_in(ctx);
    }
    if pair && h == Rank::Jack && ctx.rng.gen_bool(0.3) {
        return call_or_all_in(ctx);
    }
    if ctx.profile.allows_4bet_bluffs && suited && h == Rank::Ace && l == Rank::Five {
        if !preflop_bluff_can_raise(ctx) {
            return speculative_preflop_call(ctx);
        }
        if ctx
            .rng
            .gen_bool((ctx.profile.four_bet_pct as f64 / 24.0).min(0.5))
        {
            return if deep {
                emit_preflop_raise(ctx, five_bet_to)
            } else {
                jam(ctx)
            };
        }
    }
    if defends_reraise_by_mdf(ctx) {
        return call_or_all_in(ctx);
    }
    if ctx.to_call.0 == 0 {
        PlayerAction::Check
    } else {
        PlayerAction::Fold
    }
}

pub(super) type ClassOrder = &'static PriorityOrder;

/// A seat's most recent voluntary preflop action before the decision being
/// reconstructed: whether it raised, and how many raises / calls preceded it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) struct EntryAction {
    pub raised: bool,
    pub raises_before: usize,
    pub callers_before: usize,
}

/// The range a seat entered the pot with at `entry`: the priority order that
/// range is a prefix of, and its width (percent of the 1,326 combinations),
/// from the same authored widths the preflop decision used. `spot` is the
/// seat's facing-open spot, `bb_option_raise` a big blind raising over limpers
/// without a straddle, `flat_3bet_base` the reference width of its flat of a
/// 3-bet. `None` for a limp or a line the charts do not describe.
pub(super) fn entry_range(
    profile: &PersonaProfile,
    position: Position,
    bb_option_raise: bool,
    spot: FacingOpenSpot,
    flat_3bet_base: f64,
    entry: EntryAction,
) -> Option<(ClassOrder, f64)> {
    let facing_open = || {
        let widths = reference_facing_open_widths(spot);
        let squeeze = entry.callers_before > 0;
        let three_bet =
            (widths.three_bet_pct * if squeeze { 0.6 } else { 1.0 } * three_bet_scale(profile))
                .min(45.0);
        let in_position = spot.hero == HeroBucket::InPosition;
        let call = (widths.call_pct
            * if squeeze && in_position { 1.2 } else { 1.0 }
            * preflop_looseness(profile))
        .min(60.0)
        .min(85.0 - three_bet);
        (three_bet, call)
    };
    match (entry.raised, entry.raises_before) {
        // Raise first in. Without a straddle the big blind can only raise
        // first in from its check option over limpers (`decide_bb_option`,
        // +5 width); with a straddle it faces a call and uses `decide_rfi`.
        (true, 0) => Some(if bb_option_raise {
            (
                rfi_priority_order(OpenBucket::Early),
                (profile.rfi_pct[0] as f64 + 5.0).min(100.0),
            )
        } else {
            (
                rfi_priority_order(OpenBucket::for_position(position)),
                profile.rfi_pct[rfi_knob_index(position)] as f64,
            )
        }),
        // A 3-bet over one open.
        (true, 1) => Some((three_bet_order(), facing_open().0)),
        // A flat of one open.
        (false, 1) => Some((call_order(), facing_open().1)),
        // A flat of a 3-bet.
        (false, 2) => Some((
            three_bet_call_order(),
            (flat_3bet_base * preflop_looseness(profile)).min(20.0),
        )),
        _ => None,
    }
}

/// The bot's own range at its earlier voluntary action this street
/// ([`entry_range`]). Only the bot's own seat, the public action order and its
/// persona profile are read. `None` when the bot has not voluntarily entered
/// (blinds, straddle, cold players) or entered by a limp. Exact for the
/// shared-chart personas; Station/Nit entry bands and the deep LAG squeeze
/// differ, and a hand outside the reconstructed order keeps the old fold.
fn own_preflop_range(ctx: &DecisionContext<'_>) -> Option<(ClassOrder, f64)> {
    let mut raises = 0usize;
    let mut calls = 0usize;
    let mut own = None;
    for (seat, action) in ctx.street_actions {
        let raise = matches!(action, PlayerAction::Raise { .. } | PlayerAction::AllIn);
        let call = matches!(action, PlayerAction::Call);
        if *seat == ctx.seat && (raise || call) {
            own = Some(EntryAction {
                raised: raise,
                raises_before: raises,
                callers_before: calls,
            });
        }
        raises += usize::from(raise);
        calls += usize::from(call);
    }
    entry_range(
        ctx.profile,
        ctx.position,
        ctx.seat == ctx.bb_seat && !ctx.has_straddle,
        facing_open_spot(ctx),
        if is_in_position(ctx) { 5.0 } else { 3.0 },
        own?,
    )
}

/// Share of the called pot an ordinary preflop re-raise is assumed to realize
/// when it is called (a wide re-raising range against the bot's continuing
/// range, before position). Authored constant, not a read.
const RERAISER_CALLED_SHARE: f64 = 0.25;

/// Share of its own entry range each of `defenders` voluntarily entered
/// players keeps against a re-raise to `wager` with `pot` already out there
/// (the wager included) and `to_call` owed; see [`defends_reraise_by_mdf`].
pub(super) fn reraise_defence_share(pot: u32, wager: u32, to_call: u32, defenders: usize) -> f64 {
    let (pot, wager) = (f64::from(pot), f64::from(wager));
    if pot <= wager || defenders == 0 {
        return 0.0;
    }
    // Break-even fold rate for the aggressor: folds win `pot - wager`; a call
    // costs the wager back less the share of the called pot its hand still
    // realizes. Plain MDF (share 0) ignores that equity and leaves any-two
    // re-raises profitable when the pot already holds antes and a straddle.
    let called_pot = pot + f64::from(to_call);
    let realized = RERAISER_CALLED_SHARE * called_pot;
    let breakeven_fold = ((wager - realized).max(0.0) / (pot - realized).max(1.0)).clamp(0.0, 1.0);
    1.0 - breakeven_fold.powf(1.0 / defenders as f64)
}

/// Minimum defense against an ordinary preflop re-raise.
///
/// The authored vs-3-bet / vs-4-bet charts keep fixed continue widths of ALL
/// hands, independent of how wide the bot entered. A wide opener therefore
/// folded ~85% to any re-raise, so re-raising any two cards won the pot
/// outright far above its break-even rate (prod 2026-11-10/11). The
/// aggressor's whole street wager `W` risks winning the pot `P - W` already
/// out there, so the players who voluntarily entered keep `1 - F` of their
/// combined range, where `F` is the aggressor's break-even fold rate once a
/// called re-raise still realizes [`RERAISER_CALLED_SHARE`] of the called pot
/// (plain MDF `(P - W) / P` when that share is 0). Each of `k` such players
/// keeps `1 - F^(1/k)` of its own range, strongest first in the order that
/// range was drawn from. Counting `W` from zero (ignoring a blind or limp the
/// aggressor already had in) overstates the aggressor's new risk, hence `F`,
/// so it slightly understates the defense (the conservative side). Only own cards,
/// the public action/pot state and the persona's own widths are used.
fn defends_reraise_by_mdf(ctx: &mut DecisionContext<'_>) -> bool {
    if ctx.to_call.0 == 0
        || ctx.to_call.0 >= ctx.stack_chips
        || preflop_aggressor_is_all_in(ctx)
        || !ctx.table.is_known()
    {
        return false;
    }
    let Some(aggressor) = ctx.last_aggressor_seat.filter(|&s| s != ctx.seat) else {
        return false;
    };
    // Same flat price guard as the charts: a call that commits a large share
    // of the effective stack is a stack-off decision, not a flat.
    if f64::from(ctx.to_call.0)
        > f64::from(preflop_aggressor_effective_stack(ctx)) * PREFLOP_FLAT_STACK_FRACTION
    {
        return false;
    }
    let Some((order, own_pct)) = own_preflop_range(ctx) else {
        return false;
    };
    let mut defenders: Vec<u8> = ctx
        .street_actions
        .iter()
        .filter(|(seat, action)| {
            *seat != aggressor
                && matches!(
                    action,
                    PlayerAction::Call | PlayerAction::Raise { .. } | PlayerAction::AllIn
                )
                && ctx.table.seat_entry(*seat).is_some_and(|s| !s.folded)
        })
        .map(|(seat, _)| *seat)
        .collect();
    defenders.push(ctx.seat);
    defenders.sort_unstable();
    defenders.dedup();
    let share = reraise_defence_share(ctx.pot.0, ctx.current_bet.0, ctx.to_call.0, defenders.len());
    let mix = range_mix_in_order(order, ctx.hole, own_pct * share);
    mix >= 1.0 || (mix > 0.0 && ctx.rng.gen_bool(mix))
}
