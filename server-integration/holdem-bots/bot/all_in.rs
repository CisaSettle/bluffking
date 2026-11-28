//! Bounded joint public-range pricing of call versus fold, including side pots.
//! No opponent cards, deck seed, wallet access or settlement mutation.
use super::*;
use engine::{Card, Suit};
use rand::{seq::SliceRandom, SeedableRng};

#[derive(Debug, Clone, Copy)]
pub struct AllInEvidence {
    pub trials: u32,
    pub blueprint_reach: bool,
    pub exact_river: bool,
    pub call_minus_fold: f64,
    /// The squid component of call_minus_fold, when the side game was priced.
    pub squid_call_minus_fold: Option<f64>,
    /// Width used to weight the preflop aggressor's shove range.
    pub preflop_shove_width: Option<f64>,
    pub margin: f64,
}

#[derive(Debug, Clone)]
pub struct AllInPublic {
    /// Total hand contributions, including dead money and earlier streets.
    pub contributions: Vec<(u8, u32)>,
    /// Posted big-blind ante is shared dead money, not a personal pot cap.
    pub shared_ante: Option<(u8, u32)>,
    pub squid: Option<SquidPublic>,
}

#[derive(Debug, Clone)]
pub struct SquidPublic {
    pub blood: bool,
    pub unit: u64,
    pub seats: Vec<u8>,
    pub marks: Vec<u32>,
    pub weights: Vec<u32>,
    pub remaining: u32,
    pub next_award: u32,
    /// Includes eligible participants sitting out this poker hand.
    pub stacks: Vec<u32>,
    /// Departed holders collect at settlement but never win another token.
    pub banked: Vec<(u32, u32)>,
}

/// Street, depth, straddle, previous own action, and prior aggression remain
/// distinct. Short stacks have their own bucket instead of missing learning.
pub fn all_in_context(
    street: Street,
    total: u32,
    bb: u32,
    straddle: bool,
    seat: u8,
    actions: impl IntoIterator<Item = (u8, PlayerAction)>,
) -> Option<usize> {
    if bb == 0 {
        return None;
    }
    let depth = if total <= bb.saturating_mul(20) {
        0
    } else if total <= bb.saturating_mul(100) {
        1
    } else if total <= bb.saturating_mul(200) {
        2
    } else {
        3
    };
    let mut returning = false;
    let mut raises = 0usize;
    for (s, a) in actions {
        returning |= s == seat;
        raises += usize::from(matches!(
            a,
            PlayerAction::Raise { .. } | PlayerAction::AllIn
        ));
    }
    let street = match street {
        Street::Preflop => 0,
        Street::Flop => 1,
        Street::Turn => 2,
        Street::River => 3,
    };
    Some(
        street * 64
            + raises.min(3) * 16
            + usize::from(straddle) * 8
            + usize::from(returning) * 4
            + depth,
    )
}

pub(super) fn card_bit(c: Card) -> u64 {
    1u64 << (c.rank as u8 * 4 + c.suit as u8)
}
pub(super) fn mask(h: HoleCards) -> u64 {
    card_bit(h.card1) | card_bit(h.card2)
}

fn range_width(ctx: &DecisionContext<'_>, model: &OpponentModel<'_>, seat: &SeatPublic) -> f64 {
    let history = ctx.public_history.unwrap_or_default();
    let attack = history
        .iter()
        .rposition(|a| a.seat == seat.seat)
        .filter(|&i| history[i].increased_bet);
    let mut width: f64 = if ctx.street == Street::Preflop {
        0.12
    } else {
        0.25
    };
    if seat.all_in {
        if let Some(index) = attack {
            let a = &history[index];
            // An earlier-street shove stays an earlier-street read.
            let actions = history[..index]
                .iter()
                .filter(|x| x.street == a.street)
                .map(|x| {
                    (
                        x.seat,
                        if x.full_raise {
                            x.action.clone()
                        } else if x.is_call_like() || x.increased_bet {
                            PlayerAction::Call
                        } else {
                            x.action.clone()
                        },
                    )
                });
            if let Some(bucket) = all_in_context(
                a.street,
                a.committed_to,
                ctx.big_blind,
                ctx.has_straddle,
                seat.seat,
                actions,
            ) {
                if let Some(stats) = model.by_seat.get(&seat.seat) {
                    // Four observations can start cautious adaptation; the Wilson
                    // lower bound still shrinks sparse frequencies substantially.
                    let read = stats.all_in_reads[bucket / 32][bucket % 32];
                    if let Some(frequency) = read.lower_frequency(4) {
                        width = width.max(frequency);
                    }
                }
            }
        }
    }
    if ctx.last_aggressor_seat == Some(seat.seat) {
        width = adapt::range_width(width, ctx.line_pressure);
    }
    width
}

/// Share of an opponent's modelled range left as any two cards, whatever its
/// line: the model's own uncertainty, not a read.
const MODEL_DOUBT: f64 = 0.10;

/// Range floor for an opponent whose last action was a river all-in, as shares
/// of the whole range by hand strength on the final board: below the median
/// combo, and between the median and the 85th percentile. Measured on 301 prod
/// human river shoves against bots (2026-11-08..12): 27% below the median,
/// 37% in between, 36% above. Without it the compounded street-by-street
/// narrowing read a river shove as near-nuts, and bots folded 72% against
/// below-median shoves where 60% breaks even, paying every river bluff.
const RIVER_SHOVE_AIR: f64 = 0.27;
const RIVER_SHOVE_MID: f64 = 0.37;

/// Population river-shove bands are priors, not a permanent bluff quota for
/// every player. A value-heavy line can reduce the weak/middle mass to the
/// model's uncertainty floor; repeated uncontested wins can raise it to 85%.
/// These are bounded strategy adjustments, not calibrated card frequencies.
fn river_shove_bands(pressure: f64, pot_fraction: f64) -> (f64, f64) {
    let pressure = if pressure.is_finite() {
        pressure.clamp(-1.0, 1.0)
    } else {
        0.0
    };
    // The pooled sample anchors small shoves. As the wager grows, the
    // betting line must carry more weight than an unconditional floor.
    let size_discount = (2.0 * pot_fraction).max(1.0).powf(-1.5);
    let prior = (RIVER_SHOVE_AIR + RIVER_SHOVE_MID) * size_discount;
    let weak = if pressure < 0.0 {
        prior + pressure * (prior - MODEL_DOUBT.min(prior * 0.5))
    } else {
        prior + pressure * 0.5 * (0.85 - prior)
    };
    let scale = weak / (RIVER_SHOVE_AIR + RIVER_SHOVE_MID);
    (RIVER_SHOVE_AIR * scale, RIVER_SHOVE_MID * scale)
}

/// Reconstruct the folder's likelihood through its last action before applying
/// today's known blockers. An unseen turn, river or Shark pocket may remove
/// combos from today's posterior, but cannot set yesterday's fold cutoff.
fn postflop_folder(
    ctx: &DecisionContext<'_>,
    seat: u8,
    reach: &preflop_blueprint::FlopReach,
    available: &[Card],
) -> Vec<(HoleCards, f64)> {
    let Some(flop) = ctx.board.flop else {
        return Vec::new();
    };
    let deck = terminal::unseen_cards(flop.into_iter().fold(0, |m, c| m | card_bit(c)));
    let mut range: Vec<_> = deck
        .iter()
        .enumerate()
        .flat_map(|(i, &a)| deck[i + 1..].iter().map(move |&b| HoleCards::new(a, b)))
        .filter_map(|h| reach.weight(seat, h).filter(|w| *w > 0.).map(|w| (h, w)))
        .collect();
    for a in ctx
        .public_history
        .unwrap_or_default()
        .iter()
        .filter(|a| a.seat == seat && a.street != Street::Preflop)
    {
        let folded = a.action == PlayerAction::Fold;
        let fraction = if folded {
            (1. - f64::from(a.to_call_before) / f64::from(a.pot_before.max(1))).clamp(0.25, 0.92)
        } else if a.increased_bet {
            0.28
        } else if a.is_call_like() {
            0.55
        } else {
            continue;
        };
        let street_board = BoardCards {
            flop: Some(flop),
            turn: (a.street != Street::Flop)
                .then_some(ctx.board.turn)
                .flatten(),
            river: (a.street == Street::River)
                .then_some(ctx.board.river)
                .flatten(),
        };
        let public = street_board
            .all_cards()
            .into_iter()
            .fold(0, |m, c| m | card_bit(c));
        let (holes, mut weights): (Vec<_>, Vec<_>) = range
            .into_iter()
            .filter(|(h, _)| mask(*h) & public == 0)
            .unzip();
        apply_strength_evidence(&holes, &mut weights, &street_board, fraction, folded);
        range = holes.into_iter().zip(weights).collect();
        if folded {
            let initial: f64 = range
                .iter()
                .filter_map(|(h, _)| reach.weight(seat, *h))
                .sum();
            if initial <= 0. {
                return Vec::new();
            }
            let doubt = MODEL_DOUBT * range.iter().map(|(_, w)| w).sum::<f64>() / initial;
            let allowed = available.iter().fold(0, |m, &c| m | card_bit(c));
            return range
                .into_iter()
                .filter(|(h, _)| mask(*h) & !allowed == 0)
                .filter_map(|(h, w)| {
                    Some((h, w * (1. - MODEL_DOUBT) + doubt * reach.weight(seat, h)?))
                })
                .collect();
        }
    }
    Vec::new()
}

/// Prior actions keep their selection effect. A river shove's narrowed range
/// fills the share left after the population bands, adjusted by the current
/// aggressor's public line read. Earlier all-ins keep their own prior.
fn pools(
    ctx: &DecisionContext<'_>,
    model: &OpponentModel<'_>,
    live: &[SeatPublic],
    deck: &[Card],
    reach: Option<&preflop_blueprint::FlopReach>,
) -> (Vec<Vec<(HoleCards, f64)>>, Option<f64>) {
    let order = rfi_priority_order(OpenBucket::Button);
    let combos: Vec<_> = deck
        .iter()
        .enumerate()
        .flat_map(|(i, &a)| deck[i + 1..].iter().map(move |&b| HoleCards::new(a, b)))
        .collect();
    let history = ctx.public_history.unwrap_or_default();
    let mut preflop_shove_width = None;
    let pools = live
        .iter()
        .map(|seat| {
            if let Some(native) = reach.filter(|r| {
                seat.folded && r.players.iter().any(|p| p.seat == seat.seat && !p.folded)
            }) {
                return postflop_folder(ctx, seat.seat, native, deck);
            }
            let width = range_width(ctx, model, seat);
            let mut weights: Vec<f64> = combos
                .iter()
                .map(|&h| reach.and_then(|r| r.weight(seat.seat, h)).unwrap_or(1.0))
                .collect();
            // Every dealt hand removes cards. A preflop folder has no later
            // selection, and its private cards never contend for a pot.
            if seat.folded
                && reach.is_some_and(|r| r.players.iter().any(|p| p.seat == seat.seat && p.folded))
            {
                return combos
                    .iter()
                    .copied()
                    .zip(weights)
                    .filter(|(_, w)| *w > 0.0)
                    .collect();
            }
            // Uncertainty about the postflop action can restore the initial
            // range. It cannot introduce a private hand with zero prior mass.
            let prior = reach.map(|_| weights.clone());
            let prior_at = |i: usize| prior.as_ref().map_or(1.0, |w| w[i]);
            let last = history.iter().rposition(|a| a.seat == seat.seat);
            let facing_wager = history.iter().rposition(|a| {
                a.street == ctx.street && Some(a.seat) == ctx.last_aggressor_seat && a.increased_bet
            });
            let river_shove = seat.all_in
                && ctx.street == Street::River
                && last.is_some_and(|i| {
                    history[i].increased_bet && history[i].street == Street::River
                });
            for (index, a) in history
                .iter()
                .enumerate()
                .filter(|(_, a)| a.seat == seat.seat && (a.increased_bet || a.is_call_like()))
            {
                if a.street == Street::Preflop && reach.is_some() {
                    continue;
                }
                let terminal_shove = seat.all_in && a.increased_bet && Some(index) == last;
                let fraction = if terminal_shove {
                    width
                } else if a.increased_bet {
                    // A short all-in elsewhere in the pot also sends ordinary
                    // side-pot bets through this estimator. Their current
                    // aggressor's online read must not disappear just because
                    // this bettor still has chips. Apply it once to the wager
                    // faced, not to earlier streets or another player's line.
                    if Some(index) == facing_wager {
                        adapt::range_width(0.28, ctx.line_pressure)
                    } else {
                        0.28
                    }
                } else {
                    0.55
                };
                if a.street == Street::Preflop {
                    if ctx.street == Street::Preflop
                        && terminal_shove
                        && Some(index) == facing_wager
                    {
                        preflop_shove_width = Some(fraction);
                    }
                    for (w, h) in weights.iter_mut().zip(&combos) {
                        let hi = h.card1.rank.max(h.card2.rank);
                        let lo = h.card1.rank.min(h.card2.rank);
                        let inside = class_mix_in_order(
                            order,
                            (hi, lo, h.card1.suit == h.card2.suit),
                            fraction * 100.0,
                        );
                        *w *= 0.04 + 0.96 * inside;
                    }
                } else {
                    let mut board = ctx.board.clone();
                    if a.street == Street::Flop {
                        board.turn = None;
                        board.river = None;
                    }
                    if a.street == Street::Turn {
                        board.river = None;
                    }
                    keep_strongest(&combos, &mut weights, &board, fraction);
                }
            }
            // Each narrowing step is an authored guess, and several of them
            // compound into near-certainty of a strong hand, which folded two
            // pair to a 2-chip river shove into a 4,000 pot (sim 2026-11-11).
            // Postflop, keep a fixed share of any-two holdings so a cheap price is
            // always judged against some bluffs; it cannot justify a call at
            // a real price by itself.
            let total: f64 = weights.iter().sum();
            if river_shove && total > 0.0 {
                let be = engine::eval::board_eval(ctx.board);
                let ranks: Vec<_> = combos
                    .iter()
                    .map(|h| engine::eval::rank_with_board(&be, h))
                    .collect();
                let mut sorted = ranks.clone();
                sorted.sort_unstable();
                // Share of combos strictly weaker, so equal ranks share a band.
                let band = |r: &HandRank| {
                    let below = sorted.partition_point(|x| x < r) as f64 / sorted.len() as f64;
                    usize::from(below >= 0.5) + usize::from(below >= 0.85)
                };
                let bands: Vec<_> = ranks.iter().map(band).collect();
                let count = |b| {
                    bands
                        .iter()
                        .enumerate()
                        .filter(|(_, &x)| x == b)
                        .map(|(i, _)| prior_at(i))
                        .sum::<f64>()
                };
                let (air, mid) = (count(0), count(1));
                // Only the current aggressor owns this line pressure. Other
                // all-in seats keep their own prior, including earlier shoves.
                let pressure = if ctx.last_aggressor_seat == Some(seat.seat) {
                    ctx.line_pressure
                } else {
                    0.0
                };
                let shove = &history[last.expect("river shove has a last wager")];
                let raise = shove.amount.saturating_sub(shove.to_call_before);
                let effective = if Some(seat.seat) == ctx.last_aggressor_seat
                    && ctx.table.live_opponents(ctx.seat) == Some(1)
                {
                    raise.min(ctx.to_call.0.min(ctx.stack_chips))
                } else {
                    raise
                };
                let pot = u64::from(shove.pot_before) + u64::from(shove.to_call_before);
                let (air_share, mid_share) =
                    river_shove_bands(pressure, f64::from(effective) / pot.max(1) as f64);
                let strong = 1.0
                    - if air > 0.0 { air_share } else { 0.0 }
                    - if mid > 0.0 { mid_share } else { 0.0 };
                for (i, (w, b)) in weights.iter_mut().zip(&bands).enumerate() {
                    let floor = match b {
                        0 if air > 0.0 => air_share / air,
                        1 if mid > 0.0 => mid_share / mid,
                        _ => 0.0,
                    };
                    *w = strong * *w / total + floor * prior_at(i);
                }
            } else if total > 0.0 && ctx.street != Street::Preflop {
                let initial = prior
                    .as_ref()
                    .map_or(weights.len() as f64, |p| p.iter().sum());
                let doubt = MODEL_DOUBT * total / initial;
                for (i, w) in weights.iter_mut().enumerate() {
                    *w = *w * (1.0 - MODEL_DOUBT) + doubt * prior_at(i);
                }
            }
            // No action yet: a waiting player's range remains broad. Their current
            // contribution is contested, but no speculative future call is credited.
            combos
                .iter()
                .copied()
                .zip(weights)
                .filter(|(_, w)| *w > 0.0)
                .collect()
        })
        .collect();
    (pools, preflop_shove_width)
}

/// Narrow `weights` to the strongest `fraction` of their mass by made hand on
/// `board`. Equal made ranks receive the same range weight, strong draws keep
/// at least 55%, and every combo keeps a 4% floor.
pub(super) fn keep_strongest(
    combos: &[HoleCards],
    weights: &mut Vec<f64>,
    board: &BoardCards,
    fraction: f64,
) {
    apply_strength_evidence(combos, weights, board, fraction, false);
}

fn apply_strength_evidence(
    combos: &[HoleCards],
    weights: &mut Vec<f64>,
    board: &BoardCards,
    fraction: f64,
    folded: bool,
) {
    let be = engine::eval::board_eval(board);
    let mut ranked: Vec<_> = combos
        .iter()
        .enumerate()
        .map(|(i, h)| (i, engine::eval::rank_with_board(&be, h)))
        .collect();
    ranked.sort_unstable_by_key(|a| std::cmp::Reverse(a.1));
    let total: f64 = weights.iter().sum();
    let mut cumulative = 0.0;
    let mut next = weights.clone();
    let mut begin = 0;
    while begin < ranked.len() {
        let mut end = begin + 1;
        while end < ranked.len() && ranked[end].1 == ranked[begin].1 {
            end += 1;
        }
        let group: f64 = ranked[begin..end].iter().map(|(i, _)| weights[*i]).sum();
        let share = ((fraction * total - cumulative) / group.max(1e-12)).clamp(0.0, 1.0);
        for &(i, _) in &ranked[begin..end] {
            let draw = if draw_strength(combos[i], board) >= 8 {
                0.55
            } else {
                0.0
            };
            let continuing = share.max(draw);
            next[i] *= 0.04 + 0.96 * if folded { 1.0 - continuing } else { continuing };
        }
        cumulative += group;
        begin = end;
    }
    *weights = next;
}

/// Opponent ranges prepared once for repeated [`sample_holes`] draws: each
/// pool's total and, when every weight is finite and non-negative, its running
/// sums for a binary search.
pub(super) struct HolePools<'a> {
    pools: &'a [Vec<(HoleCards, f64)>],
    totals: Vec<f64>,
    prefix: Vec<Option<Vec<f64>>>,
}

impl<'a> HolePools<'a> {
    pub(super) fn new(pools: &'a [Vec<(HoleCards, f64)>]) -> Self {
        let totals = pools
            .iter()
            .map(|p| p.iter().map(|(_, w)| w).sum())
            .collect();
        let prefix = pools
            .iter()
            .map(|p| {
                let mut sum = 0.0f64;
                let mut prefix = Vec::with_capacity(p.len());
                for &(_, w) in p {
                    if !(w.is_finite() && w >= 0.0) {
                        return None;
                    }
                    sum += w;
                    prefix.push(sum);
                }
                (sum.is_finite() && sum > 0.0).then_some(prefix)
            })
            .collect();
        Self {
            pools,
            totals,
            prefix,
        }
    }
}

/// The holding the linear walk `draw -= w` stops at (the first entry that
/// brings `draw` to or below zero), found by a binary search over the running
/// sums. The search answer is used only when it sits clear of a rigorous bound
/// on the rounding difference between the two summation orders; otherwise,
/// and for any pool without running sums, the walk itself decides. The result
/// is therefore always exactly the walk's.
fn draw_hole(pool: &[(HoleCards, f64)], prefix: Option<&[f64]>, draw: f64) -> Option<HoleCards> {
    if let Some(prefix) = prefix {
        let i = prefix.partition_point(|&sum| sum < draw);
        if let Some(&sum) = prefix.get(i) {
            // Sequential subtraction and the running sums each err by at most
            // (n + 1) unit roundoffs of (draw + total); 8x slack on top.
            let total = prefix[prefix.len() - 1];
            let bound =
                4.0 * (prefix.len() as f64 + 2.0) * f64::EPSILON * (draw.abs() + 2.0 * total);
            let reached = draw - sum < -bound;
            let not_before = i == 0 || draw - prefix[i - 1] > bound;
            if reached && not_before {
                return Some(pool[i].0);
            }
        }
    }
    let mut draw = draw;
    pool.iter().find_map(|(h, w)| {
        if *w <= 0.0 {
            return None;
        }
        draw -= w;
        (draw <= 0.0).then_some(*h)
    })
}

/// Sample joint holdings by rejection of the ENTIRE independently drawn tuple.
/// Sequentially renormalizing later seats would bias conflicting narrow ranges.
pub(super) fn sample_holes(pools: &HolePools<'_>, rng: &mut StdRng) -> Option<Vec<HoleCards>> {
    for _ in 0..64 {
        let mut used = 0;
        let mut result = Vec::with_capacity(pools.pools.len());
        for ((pool, total), prefix) in pools.pools.iter().zip(&pools.totals).zip(&pools.prefix) {
            let draw = rng.gen::<f64>() * total;
            let h = draw_hole(pool, prefix.as_deref(), draw)?;
            if used & mask(h) != 0 {
                break;
            }
            used |= mask(h);
            result.push(h);
        }
        if result.len() == pools.pools.len() {
            return Some(result);
        }
    }
    None
}

/// Actual contributed layers, including uncalled refunds. Main-pot ties do not
/// award a token. Folded money funds pots but folded seats never win them.
fn payouts(paid: &[u32], ranks: &[Option<HandRank>], ante: u32) -> (Vec<f64>, Option<usize>) {
    let mut levels = paid.to_vec();
    levels.sort_unstable();
    levels.dedup();
    let mut previous = 0;
    let mut result = vec![0.0; paid.len()];
    let mut main = None;
    let mut first = true;
    if ante > 0 {
        let best = ranks.iter().flatten().copied().max();
        let winners: Vec<_> = ranks
            .iter()
            .enumerate()
            .filter(|(_, r)| r.is_some() && **r == best)
            .map(|(i, _)| i)
            .collect();
        if !winners.is_empty() {
            for &i in &winners {
                result[i] += f64::from(ante) / winners.len() as f64;
            }
            main = (winners.len() == 1).then_some(winners[0]);
            first = false;
        }
    }
    for level in levels {
        if level == 0 {
            continue;
        }
        let contributors: Vec<_> = paid
            .iter()
            .enumerate()
            .filter(|(_, p)| **p >= level)
            .map(|(i, _)| i)
            .collect();
        let amount = f64::from(level - previous) * contributors.len() as f64;
        previous = level;
        if contributors.len() == 1 {
            result[contributors[0]] += amount;
            continue;
        }
        let best = contributors.iter().filter_map(|&i| ranks[i]).max();
        let winners: Vec<_> = contributors
            .iter()
            .copied()
            .filter(|&i| ranks[i].is_some() && ranks[i] == best)
            .collect();
        if winners.is_empty() {
            // Matches engine hand_side_pots: dead upper layers refund their
            // contributors, not a survivor who never funded that layer.
            for &i in &contributors {
                result[i] += amount / contributors.len() as f64;
            }
            continue;
        }
        for &i in &winners {
            result[i] += amount / winners.len() as f64;
        }
        if first {
            main = (winners.len() == 1).then_some(winners[0]);
            first = false;
        }
    }
    (result, main)
}

struct Forecast<'a> {
    public: &'a SquidPublic,
    outcomes: Vec<Vec<Vec<u32>>>,
}
impl<'a> Forecast<'a> {
    fn new(p: &'a SquidPublic) -> Option<Self> {
        let n = p.seats.len();
        if !(2..=9).contains(&n)
            || n + p.banked.len() < 3
            || p.marks.len() != n
            || p.weights.len() != n
            || p.stacks.len() != n
            || p.remaining > 32
            || p.weights.iter().sum::<u32>() == 0
        {
            return None;
        }
        let mut outcomes = vec![];
        // Index n = chop; n+1 = sole winner outside the squid roster.
        for winner in 0..=n + 1 {
            let mut samples = vec![];
            for sample in 0..32 {
                let mut marks = p.marks.clone();
                let mut left = p.remaining;
                let mut award = p.next_award;
                if winner < n {
                    if p.blood {
                        let a = award.min(left);
                        marks[winner] += a;
                        left -= a;
                        award = 1;
                    } else {
                        marks[winner] = 1;
                    }
                }
                let mut rng = StdRng::seed_from_u64(0x5351_464f_5245 ^ sample);
                for _ in 0..128 {
                    if marks.iter().filter(|&&m| m == 0).count() <= 1 || (p.blood && left == 0) {
                        break;
                    }
                    let mut draw = rng.gen_range(0..p.weights.iter().sum::<u32>());
                    let i = p
                        .weights
                        .iter()
                        .position(|w| {
                            if draw < *w {
                                true
                            } else {
                                draw -= w;
                                false
                            }
                        })
                        .unwrap_or(0);
                    if p.blood {
                        let a = award.min(left);
                        marks[i] += a;
                        left -= a;
                        award = 1;
                    } else {
                        marks[i] = 1;
                    }
                }
                // No terminal result within the bounded horizon gives no credit.
                if marks.iter().filter(|&&m| m == 0).count() > 1 && (!p.blood || left > 0) {
                    marks.fill(1);
                }
                samples.push(marks);
            }
            outcomes.push(samples);
        }
        Some(Self {
            public: p,
            outcomes,
        })
    }
    fn value(&self, hero: u8, winner: Option<u8>, stacks: &[(u8, f64)]) -> f64 {
        let p = self.public;
        let Some(hero) = p.seats.iter().position(|s| *s == hero) else {
            return 0.0;
        };
        let winner = match winner {
            None => p.seats.len(),
            Some(seat) => p
                .seats
                .iter()
                .position(|s| *s == seat)
                .unwrap_or(p.seats.len() + 1),
        };
        let mut value = 0.0;
        for marks in &self.outcomes[winner] {
            let weights: Vec<u64> = marks
                .iter()
                .map(|&m| {
                    if p.blood {
                        u64::from(m) * if m >= 3 { 2 } else { 1 }
                    } else {
                        u64::from(m > 0)
                    }
                })
                .chain(p.banked.iter().map(|(marks, _)| {
                    if p.blood {
                        u64::from(*marks) * if *marks >= 3 { 2 } else { 1 }
                    } else {
                        1
                    }
                }))
                .collect();
            let sum = weights.iter().sum::<u64>();
            if sum == 0 {
                continue;
            }
            let mut balances: Vec<u64> = p
                .seats
                .iter()
                .enumerate()
                .map(|(i, seat)| {
                    stacks
                        .iter()
                        .find(|(s, _)| s == seat)
                        .map_or(u64::from(p.stacks[i]), |(_, v)| v.max(0.0) as u64)
                })
                .chain(p.banked.iter().map(|(_, stack)| u64::from(*stack)))
                .collect();
            for (loser, &mark) in marks.iter().enumerate() {
                if mark != 0 {
                    continue;
                }
                let room = weights
                    .iter()
                    .enumerate()
                    .filter(|(_, w)| **w > 0)
                    .map(|(i, w)| u64::from(u32::MAX).saturating_sub(balances[i]) / w)
                    .min()
                    .unwrap_or(0);
                // Every loser pays in full (gold-bean losers top up a shortfall).
                let funded = p.unit.min(room);
                for (i, w) in weights.iter().enumerate() {
                    balances[i] += funded * w;
                }
                if hero == loser {
                    value -= (funded * sum) as f64;
                } else {
                    value += (funded * weights[hero]) as f64;
                }
            }
        }

        // Future poker stacks are unknown: freeze the sampled post-hand balances
        // and discount continuation, but price a terminal award without haircut.
        let terminal = winner < p.seats.len()
            && (if p.blood {
                p.remaining <= p.next_award
                    || (p.marks[winner] == 0 && p.marks.iter().filter(|&&m| m == 0).count() == 2)
            } else {
                p.marks[winner] == 0 && p.marks.iter().filter(|&&m| m == 0).count() == 2
            });
        value / 32.0 * if terminal { 1.0 } else { 0.75 }
    }
}

/// Authored response for a waiting opponent, using the sampled holding and
/// only the board already visible. Never uses the sampled future runout.
fn continues(
    ctx: &DecisionContext<'_>,
    hole: HoleCards,
    cost: u32,
    pot: u32,
    rng: &mut StdRng,
) -> bool {
    if cost == 0 {
        return true;
    }
    let price = f64::from(cost) / (f64::from(pot) + f64::from(cost)).max(1.0);
    let probability = if ctx.street == Street::Preflop {
        class_mix_in_order(
            rfi_priority_order(OpenBucket::Button),
            (
                hole.card1.rank.max(hole.card2.rank),
                hole.card1.rank.min(hole.card2.rank),
                hole.card1.suit == hole.card2.suit,
            ),
            (0.34 - price * 0.55).clamp(0.04, 0.30) * 100.0,
        )
    } else {
        match made_hand_bucket(hole, ctx.board) {
            MadeHand::StraightFlushPlus | MadeHand::TripsPlus => 1.0,
            MadeHand::TwoPair => 0.90,
            MadeHand::OnePair => (1.0 - price * 1.8).clamp(0.10, 0.90),
            _ if draw_strength(hole, ctx.board) >= 8 && ctx.street != Street::River => {
                (1.0 - price * 2.0).clamp(0.0, 0.80)
            }
            _ => 0.02,
        }
    };
    rng.gen_bool(probability.clamp(0.0, 1.0))
}

pub(super) fn defend(
    ctx: &mut DecisionContext<'_>,
    model: &OpponentModel<'_>,
) -> Option<PlayerAction> {
    let public = ctx.all_in_public.as_ref()?;
    if ctx.to_call.0 == 0
        || !ctx.table.is_known()
        || ctx.table.seats.len() > 9
        || !ctx
            .table
            .seats
            .iter()
            .any(|s| s.seat != ctx.seat && !s.folded && s.all_in)
    {
        return None;
    }
    let seats = ctx.table.seats;
    let hero = seats.iter().position(|s| s.seat == ctx.seat)?;
    let paid: Option<Vec<u32>> = seats
        .iter()
        .map(|s| {
            public
                .contributions
                .iter()
                .find(|(seat, _)| *seat == s.seat)
                .map(|(_, n)| *n)
        })
        .collect();
    let mut paid = paid?;
    if paid.iter().map(|p| u64::from(*p)).sum::<u64>() != u64::from(ctx.pot.0) {
        return None;
    }
    let ante = if let Some((payer, amount)) = public.shared_ante {
        let i = seats.iter().position(|s| s.seat == payer)?;
        paid[i] = paid[i].checked_sub(amount)?;
        amount
    } else {
        0
    };
    let call = ctx.to_call.0.min(ctx.stack_chips);
    let mut called = paid.clone();
    called[hero] = called[hero].checked_add(call)?;
    let live: Vec<_> = seats
        .iter()
        .copied()
        .filter(|s| s.seat != ctx.seat && !s.folded)
        .collect();
    let known = ctx
        .board
        .all_cards()
        .into_iter()
        .fold(mask(ctx.hole), |m, c| m | card_bit(c));
    let deck: Vec<_> = Rank::ALL
        .iter()
        .flat_map(|&r| Suit::ALL.iter().map(move |&s| Card::new(r, s)))
        .filter(|c| known & card_bit(*c) == 0)
        .collect();
    let reach = preflop_blueprint::flop_reach(ctx);
    // Keep live seats first for settlement. Appended folded hands affect card
    // removal and runouts only. Reject the entire independently sampled deal
    // on a collision, including a collision with a folded player's cards.
    let mut dealt = live.clone();
    if reach.is_some() {
        dealt.extend(seats.iter().filter(|s| s.folded && s.seat != ctx.seat));
    }
    let (pools, preflop_shove_width) = pools(ctx, model, &dealt, &deck, reach.as_ref());
    if pools.iter().any(Vec::is_empty) {
        return None;
    }
    let forecast = public.squid.as_ref().and_then(Forecast::new);
    let mut rng = StdRng::seed_from_u64(0x414c_4c49_4e45_5632);
    let exact_river =
        ctx.street == Street::River && dealt.len() == 1 && live.len() == 1 && live[0].all_in;
    let mut sum = 0.0;
    let mut squid_sum = 0.0;
    let mut squares = 0.0;
    let mut total_weight = 0.0;
    let mut trials = 0;
    let mut evaluate =
        |holes: &[HoleCards], board: &BoardCards, weight: f64, rng: &mut StdRng| -> Option<()> {
            let be = engine::eval::board_eval(board);
            let mut ranks = vec![None; seats.len()];
            ranks[hero] = Some(engine::eval::rank_with_board(&be, &ctx.hole));
            let mut call_paid = called.clone();
            let mut fold_paid = paid.clone();
            let mut added = vec![0; seats.len()];
            for (s, h) in live.iter().zip(holes) {
                let i = seats.iter().position(|x| x.seat == s.seat)?;
                let cost = ctx
                    .current_bet
                    .0
                    .saturating_sub(s.committed_this_street)
                    .min(s.stack);
                if s.all_in || continues(ctx, *h, cost, ctx.pot.0, rng) {
                    ranks[i] = Some(engine::eval::rank_with_board(&be, h));
                    added[i] = cost;
                    call_paid[i] = call_paid[i].checked_add(cost)?;
                    fold_paid[i] = fold_paid[i].checked_add(cost)?;
                }
            }
            let (won, call_winner) = payouts(&call_paid, &ranks, ante);
            ranks[hero] = None;
            let (fold_won, fold_winner) = payouts(&fold_paid, &ranks, ante);
            let mut delta = won[hero] - f64::from(call) - fold_won[hero];
            if let Some(f) = &forecast {
                let call_stacks: Vec<_> = seats
                    .iter()
                    .enumerate()
                    .map(|(i, s)| {
                        (
                            s.seat,
                            f64::from(s.stack)
                                - f64::from(added[i])
                                - if i == hero { f64::from(call) } else { 0.0 }
                                + won[i],
                        )
                    })
                    .collect();
                let fold_stacks: Vec<_> = seats
                    .iter()
                    .enumerate()
                    .map(|(i, s)| {
                        (
                            s.seat,
                            f64::from(s.stack) - f64::from(added[i]) + fold_won[i],
                        )
                    })
                    .collect();
                let squid_delta =
                    f.value(ctx.seat, call_winner.map(|i| seats[i].seat), &call_stacks)
                        - f.value(ctx.seat, fold_winner.map(|i| seats[i].seat), &fold_stacks);
                delta += squid_delta;
                squid_sum += weight * squid_delta;
            }
            total_weight += weight;
            sum += weight * delta;
            squares += weight * delta * delta;
            trials += 1;
            Some(())
        };
    if exact_river {
        // Settlement and squid depend on who wins or ties, so evaluate
        // each outcome once after summing every legal combo's weight.
        let board_eval = engine::eval::board_eval(ctx.board);
        let own_rank = engine::eval::rank_with_board(&board_eval, &ctx.hole);
        let mut outcomes = [None; 3];
        for &(hole, weight) in &pools[0] {
            let rank = engine::eval::rank_with_board(&board_eval, &hole);
            let index = usize::from(rank >= own_rank) + usize::from(rank > own_rank);
            let (_, mass) = outcomes[index].get_or_insert((hole, 0.0));
            *mass += weight;
        }
        for (hole, weight) in outcomes.into_iter().flatten() {
            evaluate(&[hole], ctx.board, weight, &mut rng)?;
        }
    } else {
        let sampling = HolePools::new(&pools);
        for _ in 0..512 {
            let Some(holes) = sample_holes(&sampling, &mut rng) else {
                continue;
            };
            let mut cards = ctx.board.all_cards();
            if cards.len() < 5 {
                let used = holes.iter().fold(known, |m, h| m | mask(*h));
                let mut rest: Vec<_> = deck
                    .iter()
                    .copied()
                    .filter(|c| used & card_bit(*c) == 0)
                    .collect();
                rest.shuffle(&mut rng);
                cards.extend(rest.iter().take(5 - cards.len()));
            }
            let board = BoardCards {
                flop: Some([cards[0], cards[1], cards[2]]),
                turn: Some(cards[3]),
                river: Some(cards[4]),
            };
            evaluate(&holes, &board, 1.0, &mut rng)?;
        }
    }
    if total_weight <= 0.0 || (!exact_river && trials < 384) {
        return None;
    }
    let mean = sum / total_weight;
    let variance = (squares / total_weight - mean * mean).max(0.0);
    let waiting = live.iter().filter(|s| !s.all_in && s.stack > 0).count();
    // Sampling uncertainty plus future-action risk; never counts money in an
    // unreachable side pot. River/closed all-in decisions need no future margin.
    let margin = if exact_river {
        0.0
    } else {
        1.5 * (variance / f64::from(trials)).sqrt() + f64::from(call) * 0.02 * waiting as f64
    };
    ctx.all_in_evidence = Some(AllInEvidence {
        trials: if exact_river {
            pools[0].len() as u32
        } else {
            trials
        },
        blueprint_reach: reach.is_some(),
        exact_river,
        call_minus_fold: mean,
        squid_call_minus_fold: forecast.as_ref().map(|_| squid_sum / total_weight),
        preflop_shove_width,
        margin,
    });
    Some(if mean >= margin {
        call_or_all_in(ctx)
    } else {
        PlayerAction::Fold
    })
}

#[cfg(test)]
mod reach_tests {
    use super::*;
    use crate::bot::preflop_blueprint::snapshot_with;
    use engine::{GameHand, PlayerId, PokerRng};

    fn act(game: &mut GameHand, a: PlayerAction) {
        game.apply_action(game.snapshot().current_actor.unwrap(), a)
            .unwrap();
    }

    #[test]
    fn deep_preflop_shove_reports_the_learned_range_and_squid_value() {
        let stacks = [7_734_000, 7_669_000, 4_069_000];
        let mut game = GameHand::new_with_rng(
            stacks
                .into_iter()
                .enumerate()
                .map(|(seat, stack)| (PlayerId(seat as u64), Chips(stack), seat as u8))
                .collect(),
            0,
            Chips(10_000),
            Chips(5_000),
            PokerRng::from_seed_bytes([53; 32]),
        );
        game.start().unwrap();
        assert_eq!(game.snapshot().current_actor, Some(PlayerId(0)));
        act(&mut game, PlayerAction::AllIn);
        snapshot_with(
            &game,
            |_| {},
            |ctx| {
                ctx.big_blind = 10_000;
                ctx.last_aggressor_seat = Some(0);
                ctx.all_in_public = Some(AllInPublic {
                    contributions: ctx
                        .table
                        .seats
                        .iter()
                        .map(|s| (s.seat, stacks[usize::from(s.seat)] - s.stack))
                        .collect(),
                    shared_ante: None,
                    squid: Some(SquidPublic {
                        blood: true,
                        unit: 100_000,
                        seats: vec![0, 1, 2],
                        marks: vec![5, 0, 0],
                        weights: vec![28, 4, 2],
                        remaining: 1,
                        next_award: 1,
                        stacks: ctx.table.seats.iter().map(|s| s.stack).collect(),
                        banked: vec![],
                    }),
                });
                let mut human = OpponentStats::default();
                let bucket =
                    all_in_context(Street::Preflop, stacks[0], ctx.big_blind, false, 0, [])
                        .unwrap();
                let read = &mut human.all_in_reads[bucket / 32][bucket % 32];
                for was_shove in (0..32).map(|i| i < 29) {
                    read.observe(was_shove);
                }
                let expected = read.lower_frequency(4);
                let stats = std::collections::HashMap::from([(0, human)]);
                assert!(defend(ctx, &OpponentModel::new(&stats)).is_some());
                let evidence = ctx.all_in_evidence.unwrap();
                assert_eq!(evidence.preflop_shove_width, expected);
                assert!(evidence.squid_call_minus_fold.unwrap().is_finite());
                ctx.public_history = None;
                ctx.all_in_evidence = None;
                assert!(defend(ctx, &OpponentModel::new(&stats)).is_some());
                assert!(ctx.all_in_evidence.unwrap().preflop_shove_width.is_none());
            },
        );
    }

    #[test]
    fn deep_river_overbet_replay_folds_two_pair_and_prices_the_nuts() {
        use engine::PlayerAction::*;

        // Nine dealt seats, two remaining after a flop raise and turn bet.
        let stacks = [
            3399198, 2626198, 5759102, 4429702, 3275195, 938175, 4889936, 782972, 3388657,
        ];
        let mut game = GameHand::new_with_rng(
            stacks
                .into_iter()
                .enumerate()
                .map(|(seat, stack)| (PlayerId(seat as u64), Chips(stack), seat as u8))
                .collect(),
            8,
            Chips(4000),
            Chips(2000),
            PokerRng::from_seed_bytes([53; 32]),
        );
        game.start().unwrap();
        for (seat, action) in [
            (2, Call),
            (3, Call),
            (4, Fold),
            (
                5,
                Raise {
                    amount: Chips(11000),
                },
            ),
            (6, Fold),
            (7, Call),
            (8, Call),
            (0, Call),
            (1, Call),
            (2, Fold),
            (3, Call),
            (
                0,
                Raise {
                    amount: Chips(46000),
                },
            ),
            (1, Fold),
            (
                3,
                Raise {
                    amount: Chips(149500),
                },
            ),
            (5, Fold),
            (7, Fold),
            (8, Call),
            (0, Call),
            (0, Check),
            (
                3,
                Raise {
                    amount: Chips(345667),
                },
            ),
            (8, Fold),
            (0, Call),
            (0, AllIn),
        ] {
            assert_eq!(game.snapshot().current_actor, Some(PlayerId(seat)));
            act(&mut game, action);
        }
        assert_eq!(game.snapshot().current_actor, Some(PlayerId(3)));
        static BOARD: BoardCards = BoardCards {
            flop: Some([
                Card::new(Rank::King, Suit::Clubs),
                Card::new(Rank::Nine, Suit::Diamonds),
                Card::new(Rank::Four, Suit::Clubs),
            ]),
            turn: Some(Card::new(Rank::Two, Suit::Hearts)),
            river: Some(Card::new(Rank::Jack, Suit::Clubs)),
        };
        snapshot_with(
            &game,
            |_| {},
            |ctx| {
                let public = AllInPublic {
                    contributions: ctx
                        .table
                        .seats
                        .iter()
                        .map(|s| (s.seat, stacks[usize::from(s.seat)] - s.stack))
                        .collect(),
                    shared_ante: None,
                    squid: Some(SquidPublic {
                        blood: true,
                        unit: 40_000,
                        seats: (0..9).collect(),
                        marks: vec![2, 0, 2, 0, 2, 0, 1, 0, 2],
                        weights: vec![6, 6, 4, 5, 7, 5, 5, 6, 6],
                        remaining: 4,
                        next_award: 1,
                        stacks: ctx.table.seats.iter().map(|s| s.stack).collect(),
                        banked: vec![],
                    }),
                };
                ctx.board = &BOARD;
                ctx.plain_cash = false;
                ctx.big_blind = 4000;
                ctx.last_aggressor_seat = Some(0);
                ctx.all_in_public = Some(public);
                ctx.hole = HoleCards::new(
                    Card::new(Rank::King, Suit::Spades),
                    Card::new(Rank::Four, Suit::Spades),
                );
                assert_eq!(ctx.to_call.0, 2_893_031);
                let stats = std::collections::HashMap::new();
                let model = OpponentModel::new(&stats);
                let decision = defend(ctx, &model);
                let evidence = ctx.all_in_evidence.unwrap();
                assert!(evidence.exact_river && evidence.trials == 990);
                assert_eq!(decision, Some(Fold), "{evidence:?}");
                assert!(evidence.call_minus_fold < 0.0);
                assert!(evidence.squid_call_minus_fold.unwrap().is_finite());
                assert!(evidence.preflop_shove_width.is_none());
                ctx.hole = HoleCards::new(
                    Card::new(Rank::Ace, Suit::Clubs),
                    Card::new(Rank::Queen, Suit::Clubs),
                );
                assert_eq!(defend(ctx, &model), Some(Call));
                let squid = ctx.all_in_public.as_mut().unwrap().squid.as_mut().unwrap();
                squid.marks.fill(0);
                squid.marks[0] = 5;
                squid.remaining = 1;
                assert_eq!(defend(ctx, &model), Some(Call));
                // With the nuts, calling earns one unit from each of seven
                // losers. Folding pays twelve units to the six-mark winner.
                let delta = ctx.all_in_evidence.unwrap().squid_call_minus_fold.unwrap();
                assert!((delta - 19.0 * 40_000.0).abs() < 1e-6, "{delta}");
            },
        );
    }

    #[test]
    fn a_postflop_fold_changes_the_dead_card_distribution_at_its_actual_price() {
        let mut game = GameHand::new_with_rng(
            (0..3)
                .map(|seat| (PlayerId(u64::from(seat)), Chips(1000), seat))
                .collect(),
            0,
            Chips(10),
            Chips(5),
            PokerRng::from_seed_bytes([27; 32]),
        );
        game.start().unwrap();
        act(&mut game, PlayerAction::Raise { amount: Chips(25) });
        act(&mut game, PlayerAction::Call);
        act(&mut game, PlayerAction::Call);
        for _ in 0..3 {
            act(&mut game, PlayerAction::Check);
        }
        act(&mut game, PlayerAction::AllIn);
        act(&mut game, PlayerAction::Fold);
        snapshot_with(
            &game,
            |_| {},
            |ctx| {
                let reach = preflop_blueprint::flop_reach(ctx).unwrap();
                let folder = ctx.table.seats.iter().find(|p| p.folded).copied().unwrap();
                let known = ctx
                    .board
                    .all_cards()
                    .into_iter()
                    .fold(mask(ctx.hole), |m, c| m | card_bit(c));
                let deck = terminal::unseen_cards(known);
                let stats = std::collections::HashMap::new();
                let (actual, _) = pools(
                    ctx,
                    &OpponentModel::new(&stats),
                    &[folder],
                    &deck,
                    Some(&reach),
                );
                let eval = engine::eval::board_eval(ctx.board);
                let mut ranks: Vec<_> = actual[0]
                    .iter()
                    .map(|(h, w)| {
                        (
                            engine::eval::rank_with_board(&eval, h),
                            w / reach.weight(folder.seat, *h).unwrap(),
                        )
                    })
                    .collect();
                ranks.sort_unstable_by_key(|&(r, _)| r);
                assert!(ranks.first().unwrap().1 > ranks.last().unwrap().1);
            },
        );
    }

    #[test]
    fn flop_folder_likelihood_survives_new_public_cards_and_private_blockers() {
        let mut game = GameHand::new_with_rng(
            (0..3)
                .map(|s| (PlayerId(u64::from(s)), Chips(1000), s))
                .collect(),
            0,
            Chips(10),
            Chips(5),
            PokerRng::from_seed_bytes([19; 32]),
        );
        game.start().unwrap();
        act(&mut game, PlayerAction::Raise { amount: Chips(25) });
        act(&mut game, PlayerAction::Call);
        act(&mut game, PlayerAction::Call);
        act(&mut game, PlayerAction::Check);
        act(&mut game, PlayerAction::Raise { amount: Chips(40) });
        act(&mut game, PlayerAction::Fold);
        let weights = |game: &GameHand, alternate: bool| {
            snapshot_with(
                game,
                |_| {},
                |ctx| {
                    let reach = preflop_blueprint::flop_reach(ctx).unwrap();
                    let folder = *ctx.table.seats.iter().find(|p| p.folded).unwrap();
                    let public = ctx
                        .board
                        .all_cards()
                        .into_iter()
                        .fold(0, |m, c| m | card_bit(c));
                    if alternate {
                        let other = terminal::unseen_cards(public | mask(ctx.hole));
                        ctx.hole = HoleCards::new(other[0], other[1]);
                    }
                    let deck = terminal::unseen_cards(public | mask(ctx.hole));
                    let (pools, _) = pools(
                        ctx,
                        &OpponentModel::new(&Default::default()),
                        &[folder],
                        &deck,
                        Some(&reach),
                    );
                    pools[0]
                        .iter()
                        .map(|(h, w)| (mask(*h), *w))
                        .collect::<std::collections::BTreeMap<_, _>>()
                },
            )
        };
        let at_fold = weights(&game, false);
        act(&mut game, PlayerAction::Call);
        act(&mut game, PlayerAction::Check);
        act(&mut game, PlayerAction::Check);
        assert_eq!(game.snapshot().street, Street::River);
        for alternate in [false, true] {
            let later = weights(&game, alternate);
            let common: Vec<_> = later
                .iter()
                .filter_map(|(h, w)| Some((at_fold.get(h)?, w)))
                .collect();
            assert!(common.len() > 500 && later.len() < at_fold.len());
            assert!(common.iter().all(|(a, b)| a == b));
        }
    }

    #[test]
    fn blueprint_all_in_uses_dealt_pockets_and_only_live_hands_contest() {
        for n in 2..=9 {
            let seats = (0..n)
                .map(|i| (PlayerId(i as u64 + 1), Chips(1000), i as u8))
                .collect();
            let mut game = GameHand::new_with_rng(
                seats,
                0,
                Chips(10),
                Chips(5),
                PokerRng::from_seed_bytes([27; 32]),
            );
            game.start().unwrap();
            let live = n.min(3);
            for _ in live..n {
                act(&mut game, PlayerAction::Fold);
            }
            act(&mut game, PlayerAction::Raise { amount: Chips(25) });
            for _ in 1..live {
                act(&mut game, PlayerAction::Call);
            }
            act(&mut game, PlayerAction::AllIn);
            let snap = game.snapshot();
            snapshot_with(
                &game,
                |_| {},
                |ctx| {
                    let stats = std::collections::HashMap::new();
                    let model = OpponentModel::new(&stats);
                    let reach = preflop_blueprint::flop_reach(ctx).unwrap();
                    let known = ctx
                        .board
                        .all_cards()
                        .into_iter()
                        .fold(mask(ctx.hole), |m, c| m | card_bit(c));
                    let deck = terminal::unseen_cards(known);
                    let other: Vec<_> = ctx
                        .table
                        .seats
                        .iter()
                        .copied()
                        .filter(|p| p.seat != ctx.seat)
                        .collect();
                    let (data, _) = pools(ctx, &model, &other, &deck, Some(&reach));
                    assert_eq!(data.len(), n - 1);
                    for (p, pool) in other.iter().zip(&data) {
                        assert!(!pool.is_empty());
                        assert!(pool
                            .iter()
                            .all(|(h, w)| *w > 0.0 && reach.weight(p.seat, *h).unwrap() > 0.0));
                        if p.folded {
                            assert!(pool
                                .iter()
                                .all(|(h, w)| Some(*w) == reach.weight(p.seat, *h)));
                        }
                    }
                    let mut rng = StdRng::seed_from_u64(891);
                    let hands = sample_holes(&HolePools::new(&data), &mut rng).unwrap();
                    let blocked = hands.iter().fold(known, |m, &h| m | mask(h));
                    assert_eq!(blocked.count_ones(), 3 + 2 * n as u32);
                    ctx.all_in_public = Some(AllInPublic {
                        contributions: snap
                            .players
                            .iter()
                            .map(|p| (p.seat, 1000 - p.stack.0))
                            .collect(),
                        shared_ante: None,
                        squid: None,
                    });
                    let action = defend(ctx, &model).unwrap();
                    assert!(game
                        .validate_action(snap.current_actor.unwrap(), &action)
                        .is_ok());
                    let evidence = ctx.all_in_evidence.unwrap();
                    assert!(evidence.blueprint_reach);
                    assert!(evidence.squid_call_minus_fold.is_none());
                },
            );
        }
    }
}
