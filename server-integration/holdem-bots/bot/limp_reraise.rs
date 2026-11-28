//! Bounded public reads for ordinary (non-all-in) limp-reraises.
use super::*;

/// Last 32 dealt hands at comparable own depth/straddle, NOT just the hands
/// where the opponent chose to limp. Rare premium limps must not look wide.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct LimpReraiseRead {
    pub hands: u8,
    /// First voluntary limps within the observed hands.
    pub limps: u32,
    pub opportunities: [u32; 4],
    pub attacks: [u32; 4],
}

impl LimpReraiseRead {
    pub fn observe(&mut self, limped: bool, observation: Option<(usize, bool)>) {
        let observation = observation.filter(|_| limped);
        self.limps = (self.limps << 1) | u32::from(limped);
        self.hands = self.hands.saturating_add(1).min(32);
        for i in 0..4 {
            self.opportunities[i] = (self.opportunities[i] << 1)
                | u32::from(observation.is_some_and(|(bucket, _)| bucket == i));
            self.attacks[i] = (self.attacks[i] << 1) | u32::from(observation == Some((i, true)));
        }
    }

    /// History where every hand in `self` came before every hand in `later`.
    pub fn then(self, later: Self) -> Self {
        let n = u32::from(later.hands);
        let shift = |older: u32, newer: u32| older.checked_shl(n).unwrap_or(0) | newer;
        Self {
            hands: (u32::from(self.hands) + n).min(32) as u8,
            limps: shift(self.limps, later.limps),
            opportunities: std::array::from_fn(|i| {
                shift(self.opportunities[i], later.opportunities[i])
            }),
            attacks: std::array::from_fn(|i| shift(self.attacks[i], later.attacks[i])),
        }
    }

    pub fn range_width(self, context: usize) -> Option<f64> {
        let opportunities = *self.opportunities.get(context)? & self.limps;
        let attacks = *self.attacks.get(context)? & opportunities;
        let n = opportunities.count_ones();
        let k = attacks.count_ones();
        if n < 4 || self.hands < 12 {
            return None;
        }
        // Participation and the conditional response are distinct. Dividing
        // attacks in one caller/size context by ALL dealt hands penalizes an
        // opponent just because that external situation arose less often.
        // Both factors are shrunk; their product is an authored range estimate,
        // NOT a calibrated joint confidence bound or proof of anyone's cards.
        let conditional = wilson(k, n);
        if conditional < 0.35 {
            return None;
        }
        let participation = wilson(self.limps.count_ones(), u32::from(self.hands));
        let width = participation * conditional;
        (width >= 0.10).then_some(width.min(0.35))
    }
}

fn wilson(k: u32, n: u32) -> f64 {
    if n == 0 {
        return 0.0;
    }
    let n = f64::from(n);
    let p = f64::from(k) / n;
    let z2 = 1.96 * 1.96;
    (p + z2 / (2.0 * n) - 1.96 * (p * (1.0 - p) / n + z2 / (4.0 * n * n)).sqrt()) / (1.0 + z2 / n)
}

pub fn limp_read_bucket(total: u32, live_blind: u32, straddle: bool) -> Option<usize> {
    if live_blind == 0 {
        return None;
    }
    let depth = if total <= live_blind.saturating_mul(20) {
        0
    } else if total <= live_blind.saturating_mul(100) {
        1
    } else if total <= live_blind.saturating_mul(200) {
        2
    } else {
        3
    };
    Some(depth + usize::from(straddle) * 4)
}

#[derive(Debug, Clone, Copy)]
pub struct LimpReturn {
    pub opener: u8,
    pub open_to: u32,
    pub live_blind: u32,
    pub context: usize,
}

/// Before a limper's first return: exactly one full open, no short increases,
/// and no intervening action by that limper. Caller count and open price remain
/// separate contexts. Raise rights/available stack are checked by the caller.
pub fn limp_return(history: &[PublicAction], actor: u8) -> Option<LimpReturn> {
    let actions: Vec<_> = history
        .iter()
        .filter(|a| a.street == Street::Preflop)
        .collect();
    let increases: Vec<_> = actions
        .iter()
        .enumerate()
        .filter(|(_, a)| a.increased_bet)
        .collect();
    if increases.len() != 1 {
        return None;
    }
    let (open_index, open) = increases[0];
    if !open.full_raise || open.seat == actor {
        return None;
    }
    let own: Vec<_> = actions
        .iter()
        .enumerate()
        .filter(|(_, a)| a.seat == actor)
        .collect();
    if own.len() != 1 {
        return None;
    }
    let (limp_index, limp) = own[0];
    if limp_index >= open_index
        || !matches!(limp.action, PlayerAction::Call)
        || limp.increased_bet
        || limp.committed_to == 0
    {
        return None;
    }
    // Do not pool unusually large opens with normal isolation raises.
    if open.committed_to > limp.committed_to.saturating_mul(10) {
        return None;
    }
    let callers = actions[open_index + 1..].iter().any(|a| a.is_call_like());
    Some(LimpReturn {
        opener: open.seat,
        open_to: open.committed_to,
        live_blind: limp.committed_to,
        context: usize::from(open.committed_to > limp.committed_to.saturating_mul(6)) * 2
            + usize::from(callers),
    })
}

pub fn ordinary_limp_reraise(spot: LimpReturn, raise_to: u32, total: u32) -> bool {
    raise_to < total
        && raise_to >= spot.open_to.saturating_mul(2)
        && raise_to <= spot.open_to.saturating_mul(6)
}

#[derive(Debug, Clone, Copy)]
pub struct LimpReraiseFacing {
    pub aggressor: u8,
    pub opener: u8,
    pub bucket: usize,
    pub context: usize,
}

/// Structural attribution also works with unknown reads. Only folds may have
/// followed the reraise; an overcall, short raise or fourth bet changes the spot.
pub fn facing_limp_reraise(ctx: &DecisionContext<'_>) -> Option<LimpReraiseFacing> {
    if ctx.street != Street::Preflop || ctx.to_call.0 == 0 {
        return None;
    }
    let history = ctx.public_history?;
    let index = history
        .iter()
        .rposition(|a| a.street == Street::Preflop && a.increased_bet)?;
    let attack = &history[index];
    if !attack.full_raise
        || attack.seat == ctx.seat
        || history[index + 1..]
            .iter()
            .any(|a| !matches!(a.action, PlayerAction::Fold))
    {
        return None;
    }
    let villain = ctx.table.seat_entry(attack.seat)?;
    if villain.all_in || villain.folded {
        return None;
    }
    let total = villain.stack.saturating_add(villain.committed_this_street);
    let spot = limp_return(&history[..index], attack.seat)?;
    if !ordinary_limp_reraise(spot, attack.committed_to, total) {
        return None;
    }
    let line = preflop_line(&history[..index], ctx.seat);
    if line.raised_at != 1 && line.called_at != Some(1) {
        return None;
    }
    Some(LimpReraiseFacing {
        aggressor: attack.seat,
        opener: spot.opener,
        bucket: limp_read_bucket(total, spot.live_blind, ctx.has_straddle)?,
        context: spot.context,
    })
}

/// Private, bounded attribution produced by the evaluated decision, without
/// another strategy call or RNG draw. No cards or player identity are stored.
#[derive(Debug, Clone, Copy)]
pub struct LimpDecisionEvidence {
    pub reason: &'static str,
    pub effective_stack: u32,
    pub waiting_players: usize,
    pub realization: Option<f64>,
    pub modeled_value: Option<f64>,
    pub required_value: Option<f64>,
}

pub(super) fn defend_limp_reraise(
    ctx: &mut DecisionContext<'_>,
    model: &OpponentModel<'_>,
    evidence: &mut Option<LimpDecisionEvidence>,
) -> Option<PlayerAction> {
    let spot = facing_limp_reraise(ctx)?;
    let villain = ctx.table.seat_entry(spot.aggressor)?;
    let effective = ctx
        .stack_chips
        .saturating_add(ctx.committed_this_street.0)
        .min(villain.stack.saturating_add(villain.committed_this_street));
    let waiting: Vec<_> = ctx
        .table
        .seats
        .iter()
        .filter(|s| s.seat != ctx.seat && s.seat != spot.aggressor && !s.folded)
        .collect();
    *evidence = Some(LimpDecisionEvidence {
        reason: "insufficient_matched_history",
        effective_stack: effective,
        waiting_players: waiting.len(),
        realization: None,
        modeled_value: None,
        required_value: None,
    });
    let read = model.by_seat.get(&spot.aggressor)?.limp_reraise_reads[spot.bucket];
    let width = read.range_width(spot.context)?;
    // Keep existing premium and bluff raises. Widen only a base fold, with
    // bounded realization; there is no forced collective defense quota.
    let base = chart_action(ctx);
    let trace = evidence.as_mut()?;
    trace.reason = "base_continuation";
    if base != PlayerAction::Fold {
        return Some(base);
    }
    if waiting.iter().any(|s| s.all_in) {
        trace.reason = "waiting_all_in";
        return Some(base);
    }
    if ctx.to_call.0 >= ctx.stack_chips || f64::from(ctx.to_call.0) > f64::from(effective) * 0.10 {
        trace.reason = "effective_depth_cap";
        return Some(base);
    }
    let (high, low) = sorted_ranks(ctx.hole);
    let playable = high == low || ctx.hole.card1.suit == ctx.hole.card2.suit || low >= Rank::Ten;
    if !playable {
        trace.reason = "outside_playable_range";
        return Some(base);
    }
    let call = f64::from(ctx.to_call.0);
    let pot = f64::from(ctx.pot.0) + call;
    let equity = super::shark::shove_range_equity(ctx.hole, width);
    // Future betting makes showdown equity optimistic. Charge more OOP and
    // against players still to act. No future implied-win credit is added.
    let realization = if is_in_position(ctx) { 0.85 } else { 0.75 };
    let margin = 0.055 + waiting.len() as f64 * 0.045;
    let bonus = ctx.squid_call_value.win_bonus;
    let loss = ctx.squid_call_value.loss_risk;
    if !bonus.is_finite() || !loss.is_finite() {
        trace.reason = "invalid_squid_value";
        return Some(base);
    }
    let bonus = bonus.clamp(0.0, pot * 0.25);
    let value = realization
        * (f64::from(equity.combined_pct) / 100.0 * pot
            + f64::from(equity.win_pct) / 100.0 * bonus)
        - loss.max(0.0);
    let required = call + margin * (pot + bonus);
    trace.realization = Some(realization);
    trace.modeled_value = Some(value);
    trace.required_value = Some(required);
    trace.reason = if value >= required {
        "priced_call"
    } else {
        "price_insufficient"
    };
    Some(if value >= required {
        call_or_all_in(ctx)
    } else {
        base
    })
}
