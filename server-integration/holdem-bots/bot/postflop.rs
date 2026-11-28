//! Postflop for bot.

use super::*;

// ---------------------------------------------------------------------------
// Postflop decision
// ---------------------------------------------------------------------------

// ===========================================================================
// Postflop — GTO-anchored pass L3 (2026-10-14)
// ===========================================================================
//
// Every postflop decision is a SHARE of the hero's reach range on the current
// board: the 1,326 starting combinations weighted by the probability that the
// hero's preflop line (open / 3-bet / flat, by seat, from the L1/L2 tables)
// brought them here, ordered by [`PostflopScore`]. Open bets fire with the top
// `freq` share of that range (value hands keep [`VALUE_BET_FLOOR`]); facing a
// wager the hero continues with the top `defend` share, anchored on the
// minimum defence frequency 1 − α (α = call ÷ pot after the wager) and
// deliberately shaded below it out of position before the river and multiway;
// river bets are polarised with the size-implied bluff share b ÷ (p + b).
//
// Inputs are public table facts plus the hero's own cards. The preflop raise
// depth is not carried postflop, so the pot type (single-raised / 3-bet /
// committed) is inferred from the stack-to-pot ratio with street-specific
// thresholds. The reference frequencies are hand-authored standard practice
// for 6-max / 100bb / rake-free play, not solver output (ADR-103, L3 row).
// Station keeps every caricature branch it had (never bets below trips, calls
// down, BUG-209 near-dead guard); the Shark inherits this base under its
// exploit layer exactly as it inherited the previous one.

/// Strength ladder on the current board. Tiers compare first; the exact
/// seven-card rank and the draw outs order holdings inside a tier.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub struct PostflopScore {
    pub tier: u8,
    pub(super) rank: HandRank,
    pub(super) outs: u8,
}

/// Ladder tiers (public for tests). Draw tiers collapse on the river because
/// `draw_strength` reports no outs there.
pub const TIER_AIR: u8 = 0;
pub const TIER_GUTSHOT: u8 = 1;
pub const TIER_WEAK_SHOWDOWN: u8 = 2;
pub const TIER_SECOND_PAIR: u8 = 3;
/// A flush draw or open-ender without a pair ranks above a marginal made
/// pair: it bets (semi-bluffs) ahead of it, and it continues on price anyway.
pub const TIER_STRONG_DRAW: u8 = 4;
pub const TIER_PAIR_PLUS_DRAW: u8 = 5;
pub const TIER_TOP_PAIR: u8 = 6;
pub const TIER_TWO_PAIR: u8 = 7;
pub const TIER_TRIPS_PLUS: u8 = 8;
pub const TIER_MONSTER: u8 = 9;

pub(super) fn postflop_tier(made: MadeHand, outs: u8, hole: HoleCards, board: &BoardCards) -> u8 {
    match made {
        MadeHand::StraightFlushPlus => TIER_MONSTER,
        MadeHand::TripsPlus => TIER_TRIPS_PLUS,
        MadeHand::TwoPair => TIER_TWO_PAIR,
        MadeHand::OnePair => {
            if has_top_pair_or_overpair(hole, board) {
                TIER_TOP_PAIR
            } else if outs >= 8 {
                TIER_PAIR_PLUS_DRAW
            } else {
                TIER_SECOND_PAIR
            }
        }
        MadeHand::WeakMade | MadeHand::BoardMade => {
            if outs >= 12 {
                TIER_PAIR_PLUS_DRAW
            } else if outs >= 8 {
                TIER_STRONG_DRAW
            } else {
                TIER_WEAK_SHOWDOWN
            }
        }
        MadeHand::Air => {
            if outs >= 12 {
                TIER_PAIR_PLUS_DRAW
            } else if outs >= 8 {
                TIER_STRONG_DRAW
            } else if outs >= 4 {
                TIER_GUTSHOT
            } else {
                TIER_AIR
            }
        }
    }
}

/// Score of `hole` on `board` using a pre-converted board evaluator.
pub fn postflop_score(
    hole: HoleCards,
    board: &BoardCards,
    be: &engine::eval::BoardEval,
) -> PostflopScore {
    let rank = engine::eval::rank_with_board(be, &hole);
    let made = made_hand_bucket_with_rank(rank, hole, board);
    let outs = draw_strength(hole, board);
    PostflopScore {
        tier: postflop_tier(made, outs, hole, board),
        rank,
        outs,
    }
}

/// How the hero reached this street, in terms of the preflop tables.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum ReachRole {
    /// Raised first in from `bucket` at the persona's width (percent of 1,326).
    Opener { bucket: OpenBucket, pct: f64 },
    /// Three-bet preflop (the pot type says the open was re-raised).
    ThreeBettor { pct: f64 },
    /// Flat-called an open from `spot` at the persona-scaled widths.
    Caller { three_bet_pct: f64, call_pct: f64 },
    /// Called a 3-bet (the L2 flat width: 5% in position / 3% out, scaled).
    ThreeBetCaller { pct: f64 },
    /// Called a re-raise after entering voluntarily (Shark, public history):
    /// the chart flat range (`chart_pct` of the 3-bet-call order) or the top
    /// `kept_pct` of the `entry` order the minimum defense
    /// (`defends_reraise_by_mdf`) kept. `raises_premiums` removes QQ+/AK,
    /// which always 4-bet instead of flatting a 3-bet.
    ReraiseCaller {
        chart_pct: f64,
        entry: &'static PriorityOrder,
        kept_pct: f64,
        raises_premiums: bool,
    },
    /// The Nit's own defence branch: the top `classes` of the Chen order.
    ChenTop { classes: u32 },
    /// Limped / unknown preflop line: every combination equally likely.
    Uniform,
}

/// Pot type inferred from the stack-to-pot ratio (public facts only). The
/// thresholds follow a 100bb hand down the streets: a single-raised pot has
/// SPR ≈ 14 on the flop, ≈ 7 after a c-bet and ≈ 3 after a turn barrel; a
/// 3-bet pot ≈ 4 / 2 / 0.7.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PotType {
    SingleRaised,
    ThreeBet,
    /// Less than a pot and a half behind on the flop: stack-off territory.
    Committed,
}

/// Pot at the start of the current street: the engine pot minus every chip
/// wagered on this street (public seat facts; without them, the hero's own
/// commitment and the call it faces are the only known street wagers).
pub(super) fn street_start_pot(ctx: &DecisionContext<'_>) -> u32 {
    let street_wagers = if ctx.table.is_known() {
        ctx.table
            .seats
            .iter()
            .map(|s| s.committed_this_street)
            .fold(0u32, u32::saturating_add)
    } else {
        // Hero's own commitment plus the aggressor's WHOLE wager this street
        // (a raise-to includes the part the hero's bet already matched).
        ctx.committed_this_street
            .0
            .saturating_add(ctx.current_bet.0.max(ctx.to_call.0))
    };
    ctx.pot.0.saturating_sub(street_wagers).max(1)
}

/// Stack-to-pot ratio at the start of the street, so a wager on the current
/// street cannot rewrite the inferred preflop history.
pub(super) fn stack_to_pot(ctx: &DecisionContext<'_>) -> f64 {
    let eff = ctx
        .table
        .effective_stack(ctx.seat)
        .unwrap_or_else(|| ctx.stack_chips.saturating_add(ctx.committed_this_street.0));
    eff as f64 / street_start_pot(ctx) as f64
}

pub fn pot_type(ctx: &DecisionContext<'_>) -> PotType {
    if ctx.persona == BotPersona::Shark {
        if let Some(history) = ctx.public_history {
            // Pot ancestry is immutable; a short stack is not evidence of a
            // preflop re-raise. SPR remains available for commitment decisions.
            return if preflop_line(history, ctx.seat).raises >= 2 {
                PotType::ThreeBet
            } else {
                PotType::SingleRaised
            };
        }
    }
    let spr = stack_to_pot(ctx);
    let three_bet_below = match ctx.street {
        Street::Flop => 6.0,
        Street::Turn => 3.5,
        _ => 1.5,
    };
    if ctx.street == Street::Flop && spr < 1.5 {
        PotType::Committed
    } else if spr < three_bet_below {
        PotType::ThreeBet
    } else {
        PotType::SingleRaised
    }
}

/// Width (percent of 1,326) a 3-bettor's range is assumed to hold once the
/// pot type says the open was re-raised; scaled by the persona 3-bet knob.
pub(super) const THREE_BET_REACH_PCT: f64 = 9.0;
/// The BB's raise over limpers uses the early opening order widened by this
/// many points (`decide_bb_option`).
pub(super) const BB_ISOLATION_BOOST_PCT: f64 = 5.0;

pub fn reach_role(ctx: &DecisionContext<'_>) -> ReachRole {
    if ctx.persona == BotPersona::Shark {
        if let Some(role) = historical_reach_role(ctx, ctx.seat, ctx.profile) {
            return role;
        }
    }
    let Some(aggressor) = ctx.preflop_aggressor_seat else {
        return ReachRole::Uniform;
    };
    let three_bet_pot = pot_type(ctx) != PotType::SingleRaised;
    let multiway = live_opponents(ctx) >= 2;
    if aggressor == ctx.seat {
        if three_bet_pot {
            return ReachRole::ThreeBettor {
                pct: (THREE_BET_REACH_PCT * three_bet_scale(ctx.profile)).min(45.0),
            };
        }
        if ctx.position == Position::BigBlind {
            return ReachRole::Opener {
                bucket: OpenBucket::Early,
                pct: (ctx.profile.rfi_pct[0] as f64 + BB_ISOLATION_BOOST_PCT).min(100.0),
            };
        }
        return ReachRole::Opener {
            bucket: OpenBucket::for_position(ctx.position),
            pct: ctx.profile.rfi_pct[rfi_knob_index(ctx.position)] as f64,
        };
    }
    if three_bet_pot {
        // `decide_vs_3bet` flats 5% in position / 3% out, × looseness.
        let base = if is_in_position(ctx) { 5.0 } else { 3.0 };
        return ReachRole::ThreeBetCaller {
            pct: (base * preflop_looseness(ctx.profile)).min(30.0),
        };
    }
    if ctx.persona == BotPersona::Nit {
        // `decide_nit_facing_open`: the top NIT_DEFEND_PCT of the Chen order.
        return ReachRole::ChenTop {
            classes: ((169.0_f64 * NIT_DEFEND_PCT / 100.0).round() as u32).min(169),
        };
    }
    let spot = facing_open_spot_vs(ctx, aggressor);
    let FacingOpenWidths {
        mut three_bet_pct,
        mut call_pct,
    } = reference_facing_open_widths(spot);
    if multiway {
        // Squeeze shape of `decide_facing_open`: with another caller in the
        // pot the 3-bet tightens and an in-position flat widens. Postflop
        // the caller order is unknown, so every multiway pot gets it.
        three_bet_pct *= 0.6;
        if spot.hero == HeroBucket::InPosition {
            call_pct *= 1.2;
        }
    }
    let three_bet_pct = (three_bet_pct * three_bet_scale(ctx.profile)).min(45.0);
    let call_pct = (call_pct * preflop_looseness(ctx.profile))
        .min(60.0)
        .min(85.0 - three_bet_pct);
    ReachRole::Caller {
        three_bet_pct,
        call_pct,
    }
}

/// Probability that a hand class is in the hero's range under `role`.
pub fn reach_weight(role: ReachRole, class: (Rank, Rank, bool)) -> f64 {
    match role {
        ReachRole::Opener { bucket, pct } => {
            class_mix_in_order(rfi_priority_order(bucket), class, pct)
        }
        ReachRole::ThreeBettor { pct } => class_mix_in_order(three_bet_order(), class, pct),
        ReachRole::ThreeBetCaller { pct } => class_mix_in_order(three_bet_call_order(), class, pct),
        ReachRole::ReraiseCaller {
            chart_pct,
            entry,
            kept_pct,
            raises_premiums,
        } => {
            let (high, low, _) = class;
            if raises_premiums
                && ((high == low && high >= Rank::Queen)
                    || (high.max(low) == Rank::Ace && high.min(low) == Rank::King))
            {
                return 0.0;
            }
            // The chart flat is tried first; a hand it folds still calls
            // inside the kept share of the entry range.
            let chart = class_mix_in_order(three_bet_call_order(), class, chart_pct);
            let kept = class_mix_in_order(entry, class, kept_pct);
            chart + (1.0 - chart) * kept
        }
        ReachRole::ChenTop { classes } => {
            // `decide_nit_facing_open` 3-bets JJ+/AK deterministically (the
            // Premium bucket is position-independent), so those classes can
            // never reach the Nit's calling line.
            let sample = class_sample_hole(class);
            let premium =
                preflop_hand_strength(sample, Position::BigBlind) == PreflopStrength::Premium;
            (!premium && hand_rank_169(sample) < classes) as u32 as f64
        }
        ReachRole::Caller {
            three_bet_pct,
            call_pct,
        } => {
            let three_bet = class_mix_in_order(three_bet_order(), class, three_bet_pct);
            let call = range_mix_in_order_residual(
                call_order(),
                class_sample_hole(class),
                call_pct,
                |cl| 1.0 - class_mix_in_order(three_bet_order(), cl, three_bet_pct),
            );
            (1.0 - three_bet) * call
        }
        ReachRole::Uniform => 1.0,
    }
}

/// The hero's holding placed inside its reach range on this board.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct ReachProfile {
    pub role: ReachRole,
    pub score: PostflopScore,
    /// Share of the reach range (by weight) ranked strictly below the hero's
    /// holding plus half of the ties; 0 = bottom, 1 = top.
    pub percentile: f64,
    /// Weight share of the range at or above each tier (index = tier).
    pub mass_at_or_above: [f64; 10],
    /// Weight share of the range with a strong draw (8+ outs) below two pair.
    pub draw_mass: f64,
    /// River air weighted by blockers; used to select bluffs without increasing
    /// the range's total bluff budget.
    pub river_bluff_mass: f64,
    /// Two-pair+ holdings that still qualify as value on this runout.
    pub river_value_mass: f64,
}

/// Public-card removal heuristic, not a solver equity estimate. Prefer the
/// highest missing flush card on three-flush rivers; missed flush draws on a
/// blank river block the opponent's folds and are worse bluff candidates.
/// All weights stay positive and bounded; reach_profile normalizes them.
fn river_bluff_priority(hole: HoleCards, board: &BoardCards) -> f64 {
    let mut counts = [0u8; 4];
    let mut present = [[false; 13]; 4];
    for c in board.all_cards() {
        counts[c.suit as usize] += 1;
        present[c.suit as usize][rank_index(c.rank) as usize] = true;
    }
    let mut weight = if hole_has_ace(hole) { 0.65 } else { 1.0 };
    for suit in 0..4 {
        if counts[suit] == 3 {
            let highest_missing = (0..13).rev().find(|&r| !present[suit][r]);
            if [hole.card1, hole.card2].iter().any(|c| {
                c.suit as usize == suit && Some(rank_index(c.rank) as usize) == highest_missing
            }) {
                weight *= 3.0;
            }
        } else if counts[suit] == 2
            && hole.card1.suit as usize == suit
            && hole.card2.suit as usize == suit
        {
            weight *= 0.4;
        }
    }
    weight
}

/// Enumerate the hero's reach range on the current board. Each combination
/// that does not clash with a board card carries its class reach weight; the
/// hero's own cards are not removed (the range is what an opponent can put the
/// hero on). Cost: ≤ 1,326 bitset ranks per call.
pub fn reach_profile(ctx: &DecisionContext<'_>) -> ReachProfile {
    let role = reach_role(ctx);
    // Use the frequencies of the actions that actually led this seat to the
    // flop. The percentile describes the seat's own range on this board.
    let reach = (ctx.persona == BotPersona::Shark)
        .then(|| preflop_blueprint::flop_reach(ctx))
        .flatten()
        .filter(|r| r.weight(ctx.seat, ctx.hole).is_some_and(|w| w > 0.0));
    let line = (ctx.persona == BotPersona::Shark && ctx.public_history.is_some())
        .then(|| LineEvidence::with_reach(ctx, ctx.seat, reach.as_ref()));
    let be = engine::eval::board_eval(ctx.board);
    let score = postflop_score(ctx.hole, ctx.board, &be);
    let board_cards = ctx.board.all_cards();
    // One weight per hand class (169), looked up per combination.
    let mut class_weights: std::collections::HashMap<(Rank, Rank, bool), f64> =
        std::collections::HashMap::with_capacity(169);
    let mut total = 0.0;
    let mut below = 0.0;
    let mut ties = 0.0;
    let mut mass_at_or_above = [0.0f64; 10];
    let mut draw_mass = 0.0;
    let mut river_bluff_mass = 0.0;
    let mut river_value_mass = 0.0;
    for i in 0..52u8 {
        let a = engine::Card {
            rank: Rank::ALL[(i % 13) as usize],
            suit: engine::Suit::ALL[(i / 13) as usize],
        };
        if board_cards.contains(&a) {
            continue;
        }
        for j in (i + 1)..52u8 {
            let b = engine::Card {
                rank: Rank::ALL[(j % 13) as usize],
                suit: engine::Suit::ALL[(j / 13) as usize],
            };
            if board_cards.contains(&b) {
                continue;
            }
            let hole = HoleCards { card1: a, card2: b };
            let class = hand_class(hole);
            let w = *class_weights.entry(class).or_insert_with(|| {
                reach
                    .as_ref()
                    .and_then(|r| r.weight(ctx.seat, hole))
                    .unwrap_or_else(|| reach_weight(role, class))
            });
            let w = w * line.as_ref().map_or(1.0, |line| line.weight(hole));
            if w <= 0.0 {
                continue;
            }
            let s = postflop_score(hole, ctx.board, &be);
            if ctx.street == Street::River {
                if s.tier <= TIER_GUTSHOT {
                    river_bluff_mass += w * river_bluff_priority(hole, ctx.board);
                }
                if s.tier >= TIER_TWO_PAIR
                    && holding_has_value(
                        hole,
                        ctx.board,
                        made_hand_bucket_with_rank(s.rank, hole, ctx.board),
                    )
                {
                    river_value_mass += w;
                }
            }
            total += w;
            match s.cmp(&score) {
                std::cmp::Ordering::Less => below += w,
                std::cmp::Ordering::Equal => ties += w,
                std::cmp::Ordering::Greater => {}
            }
            for (tier, mass) in mass_at_or_above.iter_mut().enumerate() {
                if s.tier as usize >= tier {
                    *mass += w;
                }
            }
            if s.outs >= 8 && s.tier < TIER_TWO_PAIR {
                draw_mass += w;
            }
        }
    }
    if total <= 0.0 {
        return ReachProfile {
            role,
            score,
            percentile: 0.5,
            mass_at_or_above: [0.0; 10],
            draw_mass: 0.0,
            river_bluff_mass: 0.0,
            river_value_mass: 0.0,
        };
    }
    for m in mass_at_or_above.iter_mut() {
        *m /= total;
    }
    ReachProfile {
        role,
        score,
        percentile: ((below + 0.5 * ties) / total).clamp(0.0, 1.0),
        mass_at_or_above,
        draw_mass: draw_mass / total,
        river_bluff_mass: river_bluff_mass / total,
        river_value_mass: river_value_mass / total,
    }
}

/// Facing-open spot against a known opener seat (postflop reconstruction).
pub(super) fn facing_open_spot_vs(ctx: &DecisionContext<'_>, opener: u8) -> FacingOpenSpot {
    let hero = if ctx.seat == ctx.bb_seat {
        HeroBucket::BigBlind
    } else if ctx.seat == ctx.sb_seat {
        HeroBucket::SmallBlind
    } else {
        HeroBucket::InPosition
    };
    let opener = if opener == ctx.sb_seat {
        OpenerBucket::SmallBlind
    } else {
        match ctx.table.position_of(opener) {
            Some(Position::Utg | Position::UtgPlus1 | Position::UtgPlus2 | Position::Lojack) => {
                OpenerBucket::Early
            }
            Some(Position::Hijack) => OpenerBucket::Middle,
            Some(Position::Cutoff) => OpenerBucket::Cutoff,
            Some(Position::Dealer) => OpenerBucket::Button,
            Some(Position::SmallBlind) => OpenerBucket::SmallBlind,
            Some(Position::BigBlind) | None => OpenerBucket::Middle,
        }
    };
    FacingOpenSpot { hero, opener }
}

/// Opponents still in the hand (all-in seats included: they reach showdown).
pub(super) fn live_opponents(ctx: &DecisionContext<'_>) -> usize {
    ctx.table
        .live_opponents(ctx.seat)
        .unwrap_or_else(|| usize::from(ctx.num_active.saturating_sub(1)))
        .max(1)
}

/// Width of the mixing band around a range cut-off: a holding within ±5
/// percentile points of the cut-off takes the action with a linearly
/// interpolated probability, so the aggregate frequency over the range is
/// exactly the requested share while boundary holdings stay mixed.
pub(super) const RANGE_MIX_BAND: f64 = 0.10;

/// Probability of taking an action reserved for the top `share` of the range
/// when the holding sits at `percentile`.
pub fn top_share_probability(percentile: f64, share: f64) -> f64 {
    let share = share.clamp(0.0, 1.0);
    if share <= 0.0 {
        return 0.0;
    }
    if share >= 1.0 {
        return 1.0;
    }
    let cutoff = 1.0 - share;
    ((percentile - (cutoff - RANGE_MIX_BAND / 2.0)) / RANGE_MIX_BAND).clamp(0.0, 1.0)
}

/// Persona scale around the ClubRegular reference knob (Reg = 1.0).
pub(super) fn knob_scale(knob: u8, reference: u8) -> f64 {
    knob as f64 / reference.max(1) as f64
}

/// Bounds for a persona-scaled open-bet share: every persona keeps a check
/// range and a bet range.
pub(super) const OPEN_BET_SHARE_MIN: f64 = 0.05;
pub(super) const OPEN_BET_SHARE_MAX: f64 = 0.90;
/// Each live opponent beyond the first removes this much of the bet share.
pub(super) const MULTIWAY_SHARE_STEP: f64 = 0.15;
pub(super) const MULTIWAY_SHARE_FLOOR: f64 = 0.10;

pub(super) fn open_bet_share(ctx: &DecisionContext<'_>, reference: f64, scale: f64) -> f64 {
    let extra = live_opponents(ctx).saturating_sub(1) as f64;
    ((reference * scale).clamp(OPEN_BET_SHARE_MIN, OPEN_BET_SHARE_MAX)
        - extra * MULTIWAY_SHARE_STEP)
        .max(MULTIWAY_SHARE_FLOOR)
}

/// Reference c-bet share and size (fraction of pot) for the flop aggressor,
/// heads-up, ClubRegular.
pub fn cbet_reference(ip: bool, texture: BoardTexture, pot: PotType) -> (f64, f64) {
    use BoardTexture as T;
    use PotType as P;
    match (pot, ip, texture) {
        (P::SingleRaised, true, T::Dry) => (0.75, 0.33),
        (P::SingleRaised, true, T::Wet) => (0.55, 0.50),
        (P::SingleRaised, false, T::Dry) => (0.55, 0.33),
        (P::SingleRaised, false, T::Wet) => (0.40, 0.66),
        (P::ThreeBet, true, T::Dry) => (0.80, 0.33),
        (P::ThreeBet, true, T::Wet) => (0.65, 0.40),
        (P::ThreeBet, false, T::Dry) => (0.65, 0.33),
        (P::ThreeBet, false, T::Wet) => (0.50, 0.50),
        (P::Committed, _, _) => (0.85, 0.50),
    }
}

/// Reference second-barrel share and size for the flop aggressor on the turn.
pub fn barrel_reference(ip: bool, texture: BoardTexture, pot: PotType) -> (f64, f64) {
    use BoardTexture as T;
    use PotType as P;
    match (pot, ip, texture) {
        (P::SingleRaised, true, T::Dry) => (0.55, 0.66),
        (P::SingleRaised, true, T::Wet) => (0.45, 0.66),
        (P::SingleRaised, false, T::Dry) => (0.45, 0.66),
        (P::SingleRaised, false, T::Wet) => (0.35, 0.66),
        (P::ThreeBet, true, _) => (0.60, 0.60),
        (P::ThreeBet, false, _) => (0.50, 0.60),
        (P::Committed, _, _) => (0.80, 0.50),
    }
}

/// Reference delayed c-bet (turn, preflop aggressor checked the flop).
pub(super) fn delayed_cbet_reference(ip: bool) -> (f64, f64) {
    if ip {
        (0.40, 0.50)
    } else {
        (0.30, 0.50)
    }
}
/// Reference turn probe (in position, not the aggressor, flop checked through).
pub(super) const PROBE_REFERENCE: (f64, f64) = (0.45, 0.50);
/// Reference flop stab (in position, not the aggressor, checked to).
pub(super) const FLOP_STAB_REFERENCE: (f64, f64) = (0.45, 0.33);
/// Polarised river size (fraction of pot) and the thin-value size.
pub(super) const RIVER_POLAR_SIZE: f64 = 0.75;
pub(super) const RIVER_THIN_SIZE: f64 = 0.50;
/// Top-pair hands bet thin for value on the river only this high in the range
/// and only heads-up.
pub(super) const RIVER_THIN_VALUE_PERCENTILE: f64 = 0.80;

/// The thin-value range gate when a lone opponent has checked the river to
/// us in position. The check caps that range, so top pair needs no rank
/// inside our own range; a river opening line the humans answer and win
/// (`s < 0`, [`adapt::opening_pressure`]) restores the gate in proportion.
pub(super) fn checked_to_thin_percentile(s: f64) -> f64 {
    if s.is_finite() {
        RIVER_THIN_VALUE_PERCENTILE * (-s).clamp(0.0, 1.0)
    } else {
        RIVER_THIN_VALUE_PERCENTILE
    }
}
/// River bluff share is halved against two or more live opponents.
pub(super) const MULTIWAY_BLUFF_SCALE: f64 = 0.5;

/// Bet `size_frac` of the pot through the persona size multiplier and the
/// mean-preserving jitter.
pub(super) fn bet_pot_fraction(ctx: &mut DecisionContext<'_>, size_frac: f64) -> PlayerAction {
    let anchor = scaled_bet(ctx.profile, (ctx.pot.0 as f64 * size_frac).round() as u32);
    // Shark uses recognizable pot fractions; other personas retain their
    // caricature multiplier and random chip jitter.
    let bet = if ctx.persona == BotPersona::Shark {
        let sizes = [0.25, 1.0 / 3.0, 0.50, 2.0 / 3.0, 0.75, 1.0];
        let upper = sizes
            .iter()
            .position(|&size| size >= size_frac)
            .unwrap_or(sizes.len() - 1);
        let lower = upper.saturating_sub(1);
        let lo = sizes[lower];
        let hi = sizes[upper];
        let choose_hi = if hi > lo {
            ((size_frac - lo) / (hi - lo)).clamp(0.0, 1.0)
        } else {
            1.0
        };
        let size = if ctx.rng.gen_bool(choose_hi) { hi } else { lo };
        river_bet_chips(ctx, size)
    } else {
        jittered_bet(ctx, anchor)
    };
    emit_bet(ctx, bet)
}

/// Open-bet decision for the top `share` of the reach range: value hands bet
/// at least [`VALUE_BET_FLOOR`] (same exclusions as before: Station, Shark,
/// four-flush runouts). Shark also protects its heads-up checking range.
pub(super) fn range_open_bet(
    ctx: &mut DecisionContext<'_>,
    reach: &ReachProfile,
    made: MadeHand,
    share: f64,
    size_frac: f64,
    knob_pct: u8,
) -> PlayerAction {
    let value = holds_value_hand(ctx, made);
    let mut p = if ctx.profile.suppresses_postflop_raise {
        // The calling station bets its monsters at the caricature frequency
        // (ADR-031 §3.3), never by range share.
        scaled_freq(ctx.profile, knob_pct)
    } else {
        top_share_probability(reach.percentile, share)
    };
    if value {
        p = p.max(open_bet_freq(ctx, knob_pct, true));
    }
    p = shark_open_mix(ctx, value, reach.score.outs, p);
    if p >= 1.0 || (p > 0.0 && ctx.rng.gen_bool(p)) {
        return bet_pot_fraction(ctx, size_frac);
    }
    PlayerAction::Check
}

/// The range cutoff alone bets its strongest holdings every time, leaving
/// checks without those hands. Keep strong checks and mix live draws rather
/// than exposing a deterministic strength threshold. These are bounded,
/// authored frequencies, not an equilibrium solve or an unchanged bet budget.
/// Keep short-stack, multiway, unknown-topology and river policies intact.
fn shark_open_mix(ctx: &DecisionContext<'_>, value: bool, outs: u8, base: f64) -> f64 {
    if ctx.persona != BotPersona::Shark
        || !matches!(ctx.street, Street::Flop | Street::Turn)
        || ctx.current_bet.0 != 0
        || ctx.to_call.0 != 0
        || !ctx.can_raise
        || ctx.table.live_opponents(ctx.seat) != Some(1)
    {
        return base;
    }
    let Some(villain) = ctx
        .table
        .seats
        .iter()
        .find(|s| s.seat != ctx.seat && !s.folded)
    else {
        return base;
    };
    if villain.all_in
        || u64::from(ctx.stack_chips.min(villain.stack)) < 2 * u64::from(ctx.pot.0.max(1))
    {
        return base;
    }
    // Wet boards need more immediate protection. Both textures retain real
    // value in the check range, including the out-of-position lead below.
    let cap = match board_texture(ctx.board) {
        BoardTexture::Dry => 0.80,
        BoardTexture::Wet => 0.90,
    };
    if value || outs >= 8 {
        // Only retain checks from bets the existing policy already makes.
        // Raw outs can include a board-only draw, so they must never promote
        // a previously checked holding into an additional semi-bluff.
        base.min(cap)
    } else {
        base
    }
}

/// Polarised bet when the hero holds the betting lead on the river: value
/// hands (two pair+, or heads-up top pair high in the range) bet at least
/// the value floor; no-showdown hands bluff at the size-implied share
/// b ÷ (p + b) of the value mass at the actual value-betting frequency and
/// halved multiway; blockers allocate that budget and middle-strength hands check.
pub(super) fn river_polar_bet(
    ctx: &mut DecisionContext<'_>,
    reach: &ReachProfile,
    made: MadeHand,
) -> PlayerAction {
    river_polar_bet_with_frequency(ctx, reach, made, [false; 2], RIVER_THIN_VALUE_PERCENTILE)
}

/// `thin_percentile`: lowest rank inside our own reach range at which a
/// heads-up top pair bets thin ([`RIVER_THIN_VALUE_PERCENTILE`] unless the
/// opponent's check capped its range); the small-size bluff budget follows
/// the same slice, so the bluff-to-value ratio per size is unchanged.
pub(super) fn river_polar_bet_with_frequency(
    ctx: &mut DecisionContext<'_>,
    reach: &ReachProfile,
    made: MadeHand,
    [boost_thin, boost_polar]: [bool; 2],
    thin_percentile: f64,
) -> PlayerAction {
    // The bluff budget at each size follows the frequency of its own value bets.
    let base = open_bet_freq(ctx, ctx.profile.third_barrel_pct, true);
    let frequency = |boost| if boost { (base * 1.10).min(1.0) } else { base };
    let thin_frequency = frequency(boost_thin);
    let polar_frequency = frequency(boost_polar);
    let thin_percentile = thin_percentile.clamp(0.0, 1.0);
    let heads_up = live_opponents(ctx) == 1;
    let thin_value =
        reach.score.tier == TIER_TOP_PAIR && heads_up && reach.percentile >= thin_percentile;
    let value = reach.score.tier >= TIER_TWO_PAIR || thin_value;
    if value {
        if !holds_value_hand(ctx, made) {
            return PlayerAction::Check;
        }
        if ctx.rng.gen_bool(if thin_value {
            thin_frequency
        } else {
            polar_frequency
        }) {
            let size = if thin_value {
                RIVER_THIN_SIZE
            } else {
                RIVER_POLAR_SIZE
            };
            let chips = river_bet_chips(ctx, size);
            return emit_bet(ctx, chips);
        }
        return PlayerAction::Check;
    }
    if reach.score.tier <= TIER_GUTSHOT {
        // Bluffs are balanced per SIZE: the ¾-pot bets carry the two-pair+
        // value (bluff share b/(p+b) of that mass), the ½-pot thin-value
        // bets carry the heads-up top-pair slice (those with at most
        // 1 − thin_percentile of the range above them) with their
        // own b/(p+b). Value mass is discounted by the floor it actually bets
        // at. An air hand picks the size in proportion to the two budgets.
        let two_pair_plus = reach.river_value_mass;
        let thin_mass = if heads_up && !board_has_four_flush(ctx.board) {
            (reach.mass_at_or_above[TIER_TOP_PAIR as usize].min(1.0 - thin_percentile)
                - two_pair_plus)
                .max(0.0)
        } else {
            0.0
        };
        if reach.river_bluff_mass <= 0.0 {
            return PlayerAction::Check;
        }
        let bluff_budget = |value_mass: f64, size: f64, value_frequency: f64| {
            // Use the callable wager, including persona sizing and both
            // stacks. A short all-in must not carry the bluff allowance of
            // a deep 3/4-pot bet; unmatched chips return to the bettor.
            let chips = f64::from(river_bet_chips(ctx, size));
            value_mass * value_frequency * chips
                / (f64::from(ctx.pot.0) + chips).max(1.0)
                / reach.river_bluff_mass
        };
        let big = bluff_budget(two_pair_plus, RIVER_POLAR_SIZE, polar_frequency);
        let small = bluff_budget(thin_mass, RIVER_THIN_SIZE, thin_frequency);
        // Deliberate strengthening across personas: third_barrel_pct governs
        // the value frequency above, not a second multiplier on bluffs.
        // Styles retain their ranges and sizes (Nit smaller, Maniac larger),
        // while every persona's river air is backed by its actual value bets.
        let mut p = (big + small) * river_bluff_priority(ctx.hole, ctx.board);
        if !heads_up {
            p *= MULTIWAY_BLUFF_SCALE;
        }
        let p = p.clamp(0.0, 1.0);
        if p > 0.0 && ctx.rng.gen_bool(p) {
            let big_share = if big + small > 0.0 {
                big / (big + small)
            } else {
                1.0
            };
            let size = if ctx.rng.gen_bool(big_share.clamp(0.0, 1.0)) {
                RIVER_POLAR_SIZE
            } else {
                RIVER_THIN_SIZE
            };
            let chips = river_bet_chips(ctx, size);
            return emit_bet(ctx, chips);
        }
    }
    PlayerAction::Check
}

/// A shared discrete sizing for value and bluffs, capped by callable chips.
/// emit_bet still enforces the legal minimum when a villain has less than a
/// blind left; the unmatched portion does not increase our bluff budget.
pub(super) fn river_bet_chips(ctx: &DecisionContext<'_>, fraction: f64) -> u32 {
    let effective = ctx
        .table
        .effective_stack(ctx.seat)
        .map(|chips| chips.saturating_sub(ctx.committed_this_street.0))
        .unwrap_or(ctx.stack_chips);
    let chips = (f64::from(ctx.pot.0) * fraction).round() as u32;
    let chips = if ctx.persona == BotPersona::Shark {
        chips
    } else {
        scaled_bet(ctx.profile, chips)
    };
    chips.max(ctx.big_blind).min(ctx.stack_chips).min(effective)
}

/// The calling station's open-betting branches before L3, verbatim: it only
/// reaches here with trips or better (ADR-031 §3.3), bets them at its
/// caricature frequency when it holds the lead and always when it does not.
pub(super) fn decide_station_open(ctx: &mut DecisionContext<'_>) -> PlayerAction {
    let texture = board_texture(ctx.board);
    let is_pf_aggr = ctx.preflop_aggressor_seat == Some(ctx.seat);
    let bet_at = |ctx: &mut DecisionContext<'_>, knob: u8, size: f64| {
        if ctx.rng.gen_bool(scaled_freq(ctx.profile, knob)) {
            bet_pot_fraction(ctx, size)
        } else {
            PlayerAction::Check
        }
    };
    match ctx.street {
        Street::Flop if is_pf_aggr => match texture {
            BoardTexture::Dry => bet_at(ctx, ctx.profile.cbet_dry_pct, 0.33),
            BoardTexture::Wet => bet_at(ctx, ctx.profile.cbet_wet_pct, 0.50),
        },
        Street::Flop => bet_pot_fraction(ctx, 0.50),
        Street::Turn => {
            let flop_checked_through = ctx.flop_aggressor_seat.is_none();
            if ctx.flop_aggressor_seat == Some(ctx.seat) {
                let knob = match texture {
                    BoardTexture::Dry => ctx.profile.barrel_dry_pct,
                    BoardTexture::Wet => ctx.profile.barrel_wet_pct,
                };
                bet_at(ctx, knob, 0.66)
            } else if is_pf_aggr && flop_checked_through {
                bet_at(ctx, ctx.profile.delayed_cbet_pct, 0.50)
            } else if !is_pf_aggr && flop_checked_through && is_in_position(ctx) {
                bet_at(ctx, ctx.profile.probe_turn_pct, 0.50)
            } else {
                bet_pot_fraction(ctx, 0.66)
            }
        }
        _ => {
            if ctx.turn_aggressor_seat == Some(ctx.seat) {
                bet_at(ctx, ctx.profile.third_barrel_pct, 0.75)
            } else {
                bet_pot_fraction(ctx, 0.50)
            }
        }
    }
}

/// Flop decision (L3): the preflop aggressor c-bets a share of its reach
/// range by position × texture × pot type × players; a non-aggressor in
/// position stabs when checked to; out of position only trips+ lead. Facing a
/// wager → [`decide_postflop_vs_bet`].
pub(super) fn decide_flop(ctx: &mut DecisionContext<'_>) -> PlayerAction {
    let made = made_hand_bucket(ctx.hole, ctx.board);
    let draw = draw_strength(ctx.hole, ctx.board);
    if ctx.to_call.0 > 0 {
        return decide_postflop_vs_bet(ctx, made, draw);
    }
    if ctx.profile.suppresses_postflop_raise {
        if made < MadeHand::TripsPlus {
            return PlayerAction::Check;
        }
        return decide_station_open(ctx);
    }
    let is_aggressor = ctx.preflop_aggressor_seat == Some(ctx.seat);
    let texture = board_texture(ctx.board);
    let ip = is_in_position(ctx);

    if is_aggressor {
        let reach = reach_profile(ctx);
        let (reference, size) = cbet_reference(ip, texture, pot_type(ctx));
        let knob_pct = match texture {
            BoardTexture::Dry => ctx.profile.cbet_dry_pct,
            BoardTexture::Wet => ctx.profile.cbet_wet_pct,
        };
        let knob = match texture {
            BoardTexture::Dry => knob_scale(knob_pct, 70),
            BoardTexture::Wet => knob_scale(knob_pct, 60),
        };
        let share = open_bet_share(ctx, reference, knob);
        return range_open_bet(ctx, &reach, made, share, size, knob_pct);
    }

    if ip {
        let reach = reach_profile(ctx);
        // Checked to in position: stab with the top of the range.
        let (reference, size) = FLOP_STAB_REFERENCE;
        let share = open_bet_share(ctx, reference, knob_scale(ctx.profile.probe_turn_pct, 12));
        return range_open_bet(ctx, &reach, made, share, size, ctx.profile.probe_turn_pct);
    }

    // Out of position without the lead: lead only with trips or better.
    if made >= MadeHand::TripsPlus {
        let p = shark_open_mix(ctx, holds_value_hand(ctx, made), draw, 1.0);
        if p >= 1.0 || ctx.rng.gen_bool(p) {
            return bet_pot_fraction(ctx, 0.50);
        }
    }
    PlayerAction::Check
}

/// Turn decision (L3): second barrel / delayed c-bet / probe as shares of the
/// reach range; value hands keep the floor; two pair+ bets when nobody does.
pub(super) fn decide_turn(ctx: &mut DecisionContext<'_>) -> PlayerAction {
    let made = made_hand_bucket(ctx.hole, ctx.board);
    let draw = draw_strength(ctx.hole, ctx.board);
    if ctx.to_call.0 > 0 {
        return decide_postflop_vs_bet(ctx, made, draw);
    }
    if ctx.profile.suppresses_postflop_raise {
        if made < MadeHand::TripsPlus {
            return PlayerAction::Check;
        }
        return decide_station_open(ctx);
    }
    let texture = board_texture(ctx.board);
    let is_pf_aggr = ctx.preflop_aggressor_seat == Some(ctx.seat);
    let was_flop_aggr = ctx.flop_aggressor_seat == Some(ctx.seat);
    let flop_checked_through = ctx.flop_aggressor_seat.is_none();
    let ip = is_in_position(ctx);

    // (a) We bet the flop → second barrel.
    if was_flop_aggr {
        let reach = reach_profile(ctx);
        let (reference, size) = barrel_reference(ip, texture, pot_type(ctx));
        let knob_pct = match texture {
            BoardTexture::Dry => ctx.profile.barrel_dry_pct,
            BoardTexture::Wet => ctx.profile.barrel_wet_pct,
        };
        let knob = match texture {
            BoardTexture::Dry => knob_scale(knob_pct, 55),
            BoardTexture::Wet => knob_scale(knob_pct, 40),
        };
        let share = open_bet_share(ctx, reference, knob);
        return range_open_bet(ctx, &reach, made, share, size, knob_pct);
    }

    // (b) Preflop aggressor that checked back the flop → delayed c-bet.
    if is_pf_aggr && flop_checked_through {
        let reach = reach_profile(ctx);
        let (reference, size) = delayed_cbet_reference(ip);
        let share = open_bet_share(ctx, reference, knob_scale(ctx.profile.delayed_cbet_pct, 25));
        return range_open_bet(ctx, &reach, made, share, size, ctx.profile.delayed_cbet_pct);
    }

    // (c) Not the aggressor, in position, flop checked through → probe.
    if !is_pf_aggr && flop_checked_through && ip {
        let reach = reach_profile(ctx);
        let (reference, size) = PROBE_REFERENCE;
        let share = open_bet_share(ctx, reference, knob_scale(ctx.profile.probe_turn_pct, 12));
        return range_open_bet(ctx, &reach, made, share, size, ctx.profile.probe_turn_pct);
    }

    // (d) Nobody has the lead on us: bet two pair and better for value.
    if made >= MadeHand::TwoPair && holds_value_hand(ctx, made) {
        let p = open_bet_freq(ctx, ctx.profile.barrel_dry_pct, true);
        if ctx.rng.gen_bool(p) {
            return bet_pot_fraction(ctx, 0.66);
        }
    }
    PlayerAction::Check
}

/// River decision (L3): polarised third barrel with the size-implied bluff
/// share when we hold the lead or are checked to heads-up in position;
/// two pair+ value otherwise.
pub(super) fn decide_river(ctx: &mut DecisionContext<'_>) -> PlayerAction {
    let made = made_hand_bucket(ctx.hole, ctx.board);
    if ctx.to_call.0 > 0 {
        return decide_postflop_vs_bet(ctx, made, 0);
    }
    if ctx.profile.suppresses_postflop_raise {
        if made < MadeHand::TripsPlus {
            return PlayerAction::Check;
        }
        return decide_station_open(ctx);
    }
    let was_turn_aggr = ctx.turn_aggressor_seat == Some(ctx.seat);
    let reach = reach_profile(ctx);

    if was_turn_aggr {
        return river_polar_bet(ctx, &reach, made);
    }

    // Heads-up in position the opponent has checked to us: the polarised bet
    // of the river lead, with the capped-range thin-value gate.
    let heads_up = live_opponents(ctx) == 1;
    if heads_up && is_in_position(ctx) {
        let thin_percentile = checked_to_thin_percentile(ctx.open_pressure);
        return river_polar_bet_with_frequency(ctx, &reach, made, [false; 2], thin_percentile);
    }

    // First to act without the lead, or multiway: two pair+ for value.
    if made >= MadeHand::TwoPair && holds_value_hand(ctx, made) {
        let p = open_bet_freq(ctx, ctx.profile.third_barrel_pct, true);
        if ctx.rng.gen_bool(p) {
            return bet_pot_fraction(ctx, RIVER_THIN_SIZE);
        }
    }
    PlayerAction::Check
}

/// Reference defence share = 1 − α (α = call ÷ pot after the wager), then
/// shaded: the persona fold-to-c-bet knob (Reg 55 = 1.0) scales it, out of
/// position before the river takes 0.9×, and with two or more live opponents
/// the responsibility is shared (0.75×).
pub(super) const OOP_DEFENCE_SCALE: f64 = 0.90;
pub(super) const MULTIWAY_DEFENCE_SCALE: f64 = 0.75;
pub(super) const DEFENCE_SHARE_MIN: f64 = 0.10;
pub(super) const DEFENCE_SHARE_MAX: f64 = 0.95;
/// Share of the minimum defence frequency (1 − α) the Shark keeps facing a
/// heads-up river bet, whatever its range model; the online read of the
/// bettor's river line scales it ([`adapt::river_floor`]).
pub(super) const RIVER_MIN_DEFENCE: f64 = 0.85;
/// Raise share of the reach range facing a wager (Reg), by street / position.
pub(super) fn raise_share_reference(street: Street, ip: bool) -> f64 {
    match street {
        Street::Flop if ip => 0.08,
        Street::Flop => 0.12,
        Street::Turn => 0.07,
        _ => 0.03,
    }
}
/// The value part of the raise share takes the top of the range; the rest is
/// spread over strong draws as semi-bluffs (flop / turn only).
pub(super) const RAISE_VALUE_FRACTION: f64 = 0.6;
/// Trips and better that fall outside the raise share still fast-play this
/// often before the river, and on the river against a small bet.
pub(super) const TRIPS_PLUS_FAST_PLAY: f64 = 0.70;
/// A wager costing at most a quarter of the pot after it (≤ one third of the
/// pot before it) is "small".
pub(super) const SMALL_BET_ALPHA: f64 = 0.25;
/// Weak showdown hands and worse never pay more than this pot-odds price.
pub(super) const WEAK_SHOWDOWN_MAX_POT_ODDS_PCT: u32 = 40;
/// A call that costs at least half the stack behind needs top pair+ or a
/// twelve-out draw unless the price is still cheap.
pub(super) const STACK_COMMIT_MIN_POT_ODDS_PCT: u32 = 25;

/// α = the aggressor's wager this street ÷ pot after it (the engine pot
/// includes every current wager). Against a raise after the hero already bet,
/// the aggressor risks its whole raise-to amount to win the pot in front of it,
/// which is more than the hero's incremental call. A wager at least as large
/// as the pot cannot be part of it, so a caller that supplied the pot before
/// the wager is handled too.
pub fn facing_alpha(ctx: &DecisionContext<'_>) -> f64 {
    let wager = ctx.current_bet.0.max(ctx.to_call.0) as f64;
    let pot = ctx.pot.0.max(1) as f64;
    if wager >= pot {
        wager / (pot + wager)
    } else {
        wager / pot
    }
}

/// Price a draw continues at: roughly two percent per out, with implied odds
/// on the flop.
pub(super) fn draw_continues_at_price(outs: u8, street: Street, pot_odds_pct: u32) -> bool {
    if outs < 4 {
        return false;
    }
    let implied = if street == Street::Flop { 6 } else { 2 };
    pot_odds_pct <= u32::from(outs) * 2 + implied
}

/// A marginal holding can rank high in a wide bomb-pot reach range without
/// being a sound multiway bluff-catcher. MDF is a heads-up defence reference,
/// not an obligation for every seat to pay substantial turn/river wagers.
/// This is a conservative authored policy, not a multiway equity/EV solve:
/// retain cheap calls, live draws and top pair+, and leave incomplete table
/// facts and side-pot/short-call situations to their existing paths.
pub(super) fn shark_folds_marginal_multiway(
    ctx: &DecisionContext<'_>,
    made: MadeHand,
    draw: u8,
) -> bool {
    ctx.persona == BotPersona::Shark
        && matches!(ctx.street, Street::Turn | Street::River)
        && ctx.table.live_opponents(ctx.seat).is_some_and(|n| n >= 2)
        && ctx.to_call.0 > 0
        && ctx.to_call.0 <= ctx.stack_chips
        && !ctx.table.seats.iter().any(|s| !s.folded && s.all_in)
        && draw < 8
        && postflop_tier(made, draw, ctx.hole, ctx.board) <= TIER_SECOND_PAIR
        && facing_alpha(ctx) >= 1.0 / 3.0
        // At least 20% of the final pot. Widen before adding/multiplying.
        && u64::from(ctx.to_call.0) * 5 >= u64::from(ctx.pot.0) + u64::from(ctx.to_call.0)
}

/// River floor under the Shark's range models, heads-up. Those models are
/// authored guesses about the bettor's line; several narrowing steps compound
/// into near-certainty of a strong hand, and a player who bets any two cards
/// on every street then wins every pot the model gives up (sim 2026-11-11:
/// 71% river folds). Whatever the model says, the top of the bot's own range
/// (never air, never a hand that only plays the board) still calls at
/// [`RIVER_MIN_DEFENCE`] of the minimum defence frequency 1 − α, scaled by
/// the online read of the bettor's river line ([`adapt::river_floor`]): a
/// line the bots kept paying off loses the floor, an uncalled one widens it.
pub(super) fn river_min_defence_calls(ctx: &mut DecisionContext<'_>) -> bool {
    if ctx.persona != BotPersona::Shark
        || ctx.street != Street::River
        || ctx.board.count() != 5
        || ctx.to_call.0 == 0
        || ctx.table.live_opponents(ctx.seat) != Some(1)
        || made_hand_bucket(ctx.hole, ctx.board) == MadeHand::BoardMade
    {
        return false;
    }
    let reach = reach_profile(ctx);
    if reach.score.tier <= TIER_GUTSHOT {
        return false;
    }
    let floor = adapt::river_floor(
        RIVER_MIN_DEFENCE * (1.0 - facing_alpha(ctx)),
        ctx.line_pressure,
    );
    if floor <= 0.0 {
        return false;
    }
    let p = top_share_probability(reach.percentile, floor);
    p >= 1.0 || (p > 0.0 && ctx.rng.gen_bool(p))
}

/// Facing a wager (L3). Station keeps its caricature path verbatim.
pub(super) fn decide_postflop_vs_bet(
    ctx: &mut DecisionContext<'_>,
    made: MadeHand,
    draw: u8,
) -> PlayerAction {
    if ctx.profile.suppresses_postflop_raise {
        return decide_station_vs_bet(ctx, made, draw);
    }
    if ctx.persona == BotPersona::Shark
        && made == MadeHand::BoardMade
        && board_made_hand_is_unbeatable(engine::rank_hand(&ctx.hole, ctx.board), ctx.board)
    {
        return call_or_all_in(ctx);
    }
    if shark_folds_marginal_multiway(ctx, made, draw) {
        return PlayerAction::Fold;
    }
    if let Some(equity) = shark_river_equity(ctx) {
        let price =
            f64::from(ctx.to_call.0) / (f64::from(ctx.pot.0) + f64::from(ctx.to_call.0)).max(1.0);
        // Fold EV is zero; call EV is equity*(pot+call)-call. A narrow
        // indifference band mixes only near-equal decisions under our model.
        let adapted = adapt::equity(equity, ctx.line_pressure);
        let continue_probability = ((adapted - price) / 0.03 + 0.5).clamp(0.0, 1.0);
        if !ctx.rng.gen_bool(continue_probability) {
            if river_min_defence_calls(ctx) {
                return call_or_all_in(ctx);
            }
            return PlayerAction::Fold;
        }
        if equity < 0.85 || !ctx.can_raise || made < MadeHand::TripsPlus {
            return call_or_all_in(ctx);
        }
        // Only high-equity holdings may enter the existing value-raise path.
    }
    let pot_odds_pct = pot_odds_pct(ctx.pot, ctx.to_call);
    let bet_size = ctx.to_call.0;

    // Shared boards keep their dedicated rules: the hero's cards add at most a
    // kicker, so the hand is a chop at best and never a range-share
    // bluff-catcher. An unbeatable board is a forced call; a kicker on a
    // shared pair/trips folds to a multiway river bet; otherwise only a cheap
    // price (or a real draw) continues, at a reduced weak-showdown frequency.
    if made == MadeHand::BoardMade {
        let shared_rank = engine::rank_hand(&ctx.hole, ctx.board);
        if board_made_hand_is_unbeatable(shared_rank, ctx.board) {
            return PlayerAction::Call;
        }
        let kicker_only = matches!(
            shared_rank,
            HandRank::OnePair(_) | HandRank::TwoPair(_) | HandRank::ThreeOfAKind(_)
        );
        if kicker_only && ctx.street == Street::River && ctx.num_active >= 3 {
            return PlayerAction::Fold;
        }
        if draw >= 8 && pot_odds_pct < 30 {
            return PlayerAction::Call;
        }
        if pot_odds_pct > 30 {
            return PlayerAction::Fold;
        }
        return if ctx
            .rng
            .gen_bool(ctx.profile.call_weakmade_pct as f64 / 200.0)
        {
            PlayerAction::Call
        } else {
            PlayerAction::Fold
        };
    }

    let alpha = facing_alpha(ctx);
    let small_bet = alpha <= SMALL_BET_ALPHA;

    let reach = reach_profile(ctx);
    let tier = reach.score.tier;
    let ip = is_in_position(ctx);
    let multiway = live_opponents(ctx) >= 2;

    // Raise region: the top of the range raises for value (trips+ fast-play
    // most of the time before the river), strong draws fill the semi-bluff
    // part heads-up; the river raises only trips and better.
    let raise_share =
        raise_share_reference(ctx.street, ip) * ctx.profile.aggression_multiplier as f64;
    let value_raise_tier = if ctx.street == Street::River {
        TIER_TRIPS_PLUS
    } else {
        TIER_TWO_PAIR
    };
    let raise_multiple = match ctx.street {
        Street::Turn => 2.2,
        _ => 2.5,
    };
    let raise_to = |ctx: &DecisionContext<'_>| {
        scaled_bet(
            ctx.profile,
            (bet_size as f64 * raise_multiple).round() as u32,
        )
        .saturating_add(ctx.committed_this_street.0)
    };
    if tier >= value_raise_tier {
        let mut p = top_share_probability(reach.percentile, raise_share * RAISE_VALUE_FRACTION);
        if tier >= TIER_TRIPS_PLUS && (ctx.street != Street::River || small_bet) {
            p = p.max(TRIPS_PLUS_FAST_PLAY);
        }
        if p >= 1.0 || (p > 0.0 && ctx.rng.gen_bool(p)) {
            let to = raise_to(ctx);
            return emit_raise(ctx, to);
        }
        return PlayerAction::Call;
    }
    if ctx.can_raise
        && !multiway
        && ctx.street != Street::River
        && draw >= 8
        && reach.draw_mass > 0.0
    {
        // Semi-bluffs take whatever the value region leaves of the raise
        // share (a dry board with few two-pair+ combos raises more draws).
        let value_raised = reach.mass_at_or_above[value_raise_tier as usize]
            .min(raise_share * RAISE_VALUE_FRACTION);
        let p = ((raise_share - value_raised) / reach.draw_mass).clamp(0.0, 1.0);
        if p > 0.0 && ctx.rng.gen_bool(p) {
            let to = raise_to(ctx);
            return emit_raise(ctx, to);
        }
    }

    // Draws continue on price (implied odds on the flop) before the range
    // share is consulted.
    if draw >= 8 && draw_continues_at_price(draw, ctx.street, pot_odds_pct) {
        return PlayerAction::Call;
    }

    // Stack-commitment guard: a call for half the stack behind with less than
    // top pair and no twelve-out draw is a fold unless the price is cheap.
    let stack_committing = u64::from(ctx.to_call.0) * 2 >= u64::from(ctx.stack_chips);
    if stack_committing
        && tier < TIER_TOP_PAIR
        && !(ctx.persona == BotPersona::Shark && !multiway)
        && draw < 12
        && pot_odds_pct >= STACK_COMMIT_MIN_POT_ODDS_PCT
    {
        return PlayerAction::Fold;
    }

    // Weak showdown hands and worse never pay a steep price.
    if tier <= TIER_WEAK_SHOWDOWN && pot_odds_pct > WEAK_SHOWDOWN_MAX_POT_ODDS_PCT {
        return PlayerAction::Fold;
    }
    // No air bluff-catching on the river.
    if ctx.street == Street::River && tier <= TIER_GUTSHOT {
        return PlayerAction::Fold;
    }

    // Defence share anchored on the minimum defence frequency.
    let mut defend =
        (1.0 - alpha) * ((100 - ctx.profile.fold_to_cbet_pct) as f64 / 45.0).clamp(0.5, 1.4);
    if !ip && ctx.street != Street::River {
        defend *= OOP_DEFENCE_SCALE;
    }
    if multiway {
        defend *= MULTIWAY_DEFENCE_SCALE;
    }
    if ip && ctx.street == Street::Flop {
        // The float knob keeps its caricature meaning as an in-position
        // flop defence offset (Reg 18 = neutral).
        defend += (ctx.profile.float_flop_pct as f64 - 18.0) / 200.0;
    }
    let defend =
        adapt::defence_share(defend, ctx.line_pressure).clamp(DEFENCE_SHARE_MIN, DEFENCE_SHARE_MAX);
    let mut p = top_share_probability(reach.percentile, defend);
    if tier == TIER_WEAK_SHOWDOWN {
        p *= knob_scale(ctx.profile.call_weakmade_pct, 30).min(1.0);
    }
    if draw >= 4 && draw_continues_at_price(draw, ctx.street, pot_odds_pct) {
        // A gutshot at a cheap price continues regardless of its range place.
        p = p.max(1.0);
    }
    if p >= 1.0 || (p > 0.0 && ctx.rng.gen_bool(p)) {
        PlayerAction::Call
    } else {
        PlayerAction::Fold
    }
}

/// The calling-station caricature facing a wager (ADR-031 §3.3, BUG-209 /
/// ADR-047 §13.4 near-dead guard) — unchanged by L3.
pub(super) fn decide_station_vs_bet(
    ctx: &mut DecisionContext<'_>,
    made: MadeHand,
    draw: u8,
) -> PlayerAction {
    let pot_odds_pct = pot_odds_pct(ctx.pot, ctx.to_call);
    let bet_size = ctx.to_call.0;
    let fold_threshold = adapt::fold_pct(ctx.profile.fold_to_cbet_pct, ctx.line_pressure);

    if station_call_is_stack_committing(ctx)
        && pot_odds_pct >= STATION_NEAR_DEAD_MIN_POT_ODDS_PCT
        && station_hand_is_near_dead(ctx, made, draw)
        && ctx
            .rng
            .gen_bool(STATION_NEAR_DEAD_STACK_OFF_FOLD_PCT as f64 / 100.0)
    {
        return PlayerAction::Fold;
    }

    match made {
        MadeHand::StraightFlushPlus | MadeHand::TripsPlus => {
            // Raise is always allowed for TripsPlus (ADR-031 §3.3: suppress
            // raise only BELOW TripsPlus).
            if ctx.rng.gen_bool(0.80) {
                let base_raise = (bet_size as f64 * 2.5).round() as u32;
                let raise_to =
                    scaled_bet(ctx.profile, base_raise).saturating_add(ctx.committed_this_street.0);
                emit_raise(ctx, raise_to)
            } else {
                PlayerAction::Call
            }
        }
        MadeHand::TwoPair | MadeHand::WeakMade => PlayerAction::Call,
        MadeHand::OnePair => {
            if pot_odds_pct < 25 || draw >= 4 {
                return PlayerAction::Call;
            }
            if (pot_odds_pct as u8) < (100 - fold_threshold) {
                PlayerAction::Call
            } else {
                PlayerAction::Fold
            }
        }
        MadeHand::BoardMade => {
            let shared_rank = engine::rank_hand(&ctx.hole, ctx.board);
            if board_made_hand_is_unbeatable(shared_rank, ctx.board) {
                return PlayerAction::Call;
            }
            if pot_odds_pct <= 40 {
                PlayerAction::Call
            } else {
                PlayerAction::Fold
            }
        }
        MadeHand::Air => {
            let should_fold = ctx.rng.gen_bool(fold_threshold as f64 / 100.0);
            if draw >= 8 && pot_odds_pct < 30 {
                PlayerAction::Call
            } else if should_fold {
                PlayerAction::Fold
            } else {
                PlayerAction::Call
            }
        }
    }
}

// ---------------------------------------------------------------------------
// BUG-209 — Station near-dead stack-off guard (ADR-047 §13.4)
// ---------------------------------------------------------------------------

/// How often the Station folds a near-dead hand to a stack-committing call.
/// Deliberately < 100 so the seat still occasionally stacks off like a fish —
/// a deterministic fold would just be a different exploitable pattern.
pub(super) const STATION_NEAR_DEAD_STACK_OFF_FOLD_PCT: u8 = 85;

/// The guard is a PRICE guard as much as a commitment guard: a near-dead hand
/// (≤7 outs ≈ ≤15% to improve on one street) is only a clear -EV stack-off when
/// the call costs at least this share of the final pot. Below it — a short
/// Station facing a small bet into a big multiway pot (pot 300, bet 30, 90
/// behind = 9% pot odds) — calling is correct even drawing thin, and the
/// ADR-031 §3.3 "calls down" caricature keeps that call. Both cited BUG-209
/// spots sit well above the band (30% and 36%).
pub(super) const STATION_NEAR_DEAD_MIN_POT_ODDS_PCT: u32 = 20;

/// A call "commits the stack" when it costs at least a third of the chips
/// behind (which includes every all-in call). Ordinary c-bets / small barrels
/// at normal depth never trip this, so the ADR-031 §3.3 "calls down with
/// weak-mades and busted draws" caricature is untouched there. Commitment alone
/// is not enough — `decide_postflop_vs_bet` also requires the price to be bad
/// (`STATION_NEAR_DEAD_MIN_POT_ODDS_PCT`).
pub(super) fn station_call_is_stack_committing(ctx: &DecisionContext<'_>) -> bool {
    let to_call = u64::from(ctx.to_call.0);
    to_call > 0 && to_call.saturating_mul(3) >= u64::from(ctx.stack_chips)
}

/// "Near-dead": no real draw (fewer than 8 outs) and no better than a
/// non-top one pair. Real draws, top pair / overpair, genuine two pair and
/// better keep the Station's ordinary (sticky) branches. A shared unbeatable
/// board is a forced chop and is never folded.
pub(super) fn station_hand_is_near_dead(
    ctx: &DecisionContext<'_>,
    made: MadeHand,
    draw: u8,
) -> bool {
    if draw >= 8 {
        return false;
    }
    match made {
        MadeHand::Air | MadeHand::WeakMade => true,
        MadeHand::BoardMade => {
            let shared_rank = engine::rank_hand(&ctx.hole, ctx.board);
            !board_made_hand_is_unbeatable(shared_rank, ctx.board)
        }
        MadeHand::OnePair => !has_top_pair_or_overpair(ctx.hole, ctx.board),
        MadeHand::TwoPair | MadeHand::TripsPlus | MadeHand::StraightFlushPlus => false,
    }
}

#[cfg(test)]
mod blueprint_tests {
    use super::*;
    use crate::bot::preflop_blueprint::snapshot_with;
    use engine::{Card, GameHand, PlayerId, PokerRng, Suit};

    #[test]
    fn ordinary_flop_frequencies_are_ranked_in_the_range_that_played_preflop() {
        for n in 2..=9 {
            let seats = (0..n)
                .map(|i| (PlayerId(i as u64 + 1), Chips(1000), i as u8))
                .collect();
            let mut game = GameHand::new_with_rng(
                seats,
                0,
                Chips(10),
                Chips(5),
                PokerRng::from_seed_bytes([13; 32]),
            );
            game.start().unwrap();
            let act = |g: &mut GameHand, a| {
                g.apply_action(g.snapshot().current_actor.unwrap(), a)
                    .unwrap();
            };
            let live = n.min(3);
            for _ in live..n {
                act(&mut game, PlayerAction::Fold);
            }
            act(&mut game, PlayerAction::Raise { amount: Chips(25) });
            for _ in 1..live {
                act(&mut game, PlayerAction::Call);
            }
            snapshot_with(
                &game,
                |_| {},
                |ctx| {
                    let reach = preflop_blueprint::flop_reach(ctx).unwrap();
                    let board = ctx.board.all_cards();
                    let cards: Vec<_> = Rank::ALL
                        .into_iter()
                        .flat_map(|r| Suit::ALL.map(|s| Card::new(r, s)))
                        .filter(|c| !board.contains(c))
                        .collect();
                    let evaluator = engine::eval::board_eval(ctx.board);
                    let mut weighted = Vec::new();
                    for (i, &a) in cards.iter().enumerate() {
                        for &b in &cards[i + 1..] {
                            let h = HoleCards::new(a, b);
                            let weight = reach.weight(ctx.seat, h).unwrap();
                            if weight > 0.0 {
                                weighted.push((
                                    h,
                                    postflop_score(h, ctx.board, &evaluator),
                                    weight,
                                ));
                            }
                        }
                    }
                    let (h, score, _) = weighted[weighted.len() / 2];
                    ctx.hole = h;
                    let denominator: f64 = weighted.iter().map(|r| r.2).sum();
                    let expected: f64 = weighted
                        .iter()
                        .map(|r| {
                            r.2 * match r.1.cmp(&score) {
                                std::cmp::Ordering::Less => 1.,
                                std::cmp::Ordering::Equal => 0.5,
                                std::cmp::Ordering::Greater => 0.,
                            }
                        })
                        .sum::<f64>()
                        / denominator;
                    assert!(
                        (reach_profile(ctx).percentile - expected).abs() < 1e-8,
                        "{n} dealt"
                    );
                },
            );
        }
    }
}
