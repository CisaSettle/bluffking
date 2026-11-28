//! Squid-token defense for every persona.
//!
//! A squid round pays the winner of each sole main pot a token that is worth
//! far more than the blinds it was won with (up to `2 × penalty_bb` from every
//! unmarked payer per token in blood mode). The persona strategies price only
//! the poker pot, so a player who opened every hand collected every token
//! uncontested and left the bots paying for the whole squid round.
//!
//! When a persona would fold facing a wager, this compares the public squid
//! value of winning the pot instead of letting an opponent win it with the
//! bot's own showdown chance. Inputs are only the bot's own cards, the board,
//! public stacks/commitments, the hand's public actions, public per-seat reads
//! and the public squid state; the estimate never sees another player's cards
//! and never consumes the action RNG.
//!
//! Every opponent in the pot is priced against the range its public line
//! keeps: its preflop width from public VPIP, then the strongest share of it on
//! each street it bet, raised or called, narrower as the wager grows and wider
//! for players who bet often. A range read as any hand the player opens paid
//! off too many value lines: a frequent opener who bet flop, turn and river
//! was still credited with a wide river range, so weak pairs called too often.
//!
//! The room's 7-2 bounty is priced here as the same kind of side value of
//! winning the pot ([`seven_deuce::win_bonus`]).

use super::all_in::{card_bit, keep_strongest, mask, sample_holes, HolePools};
use super::commitment::street_index;
use super::opening_read::{self, OpeningReads};
use super::*;
use engine::{Card, Suit};
use rand::{seq::SliceRandom, Rng, RngCore, SeedableRng};

/// Public squid pricing for the actor, supplied by the session.
#[derive(Clone, Debug, Default)]
pub struct SquidDefense {
    /// Smallest public side-game gain of winning this pot instead of any live
    /// opponent winning it (chips). Zero outside an active squid round.
    pub bonus: f64,
    /// Public reads of the opponents; a missing seat uses the priors.
    pub reads: Vec<SquidRead>,
    /// Public actions of this hand; `None` prices preflop widths alone.
    pub history: Option<Vec<PublicAction>>,
    /// How far a human still in this pot has been collecting more than one
    /// seat's share of the token pots (0 = a fair share within noise or less,
    /// 1 = half a share above it or more): the online read
    /// [`adapt::pot_contest`] of public pot winners, per player, in blood
    /// rounds only. Eases part of [`defend`]'s caution.
    pub contest: f64,
}

/// Smoothed public frequencies of one opponent.
#[derive(Clone, Copy, Debug)]
pub struct SquidRead {
    pub seat: u8,
    /// Share of hands the player enters voluntarily (0..=1).
    pub width: f64,
    /// Share of the player's postflop actions that bet or raise (0..=1).
    pub aggression: f64,
    /// Smoothed share of heads-up opening bets of at most pot the player
    /// folded to, per flop/turn/river, from the player's own public history
    /// (the cross-table profile included): what a squid stab expects.
    pub folds: [f64; 3],
    /// This responder's online result against opening bets on this street.
    pub open_pressure: f64,
    /// How often the player opens the betting in each postflop spot.
    pub opening: OpeningReads,
}

/// Prior preflop width before enough public VPIP evidence exists.
const DEFAULT_WIDTH: f64 = 0.35;
/// Prior postflop bet/raise share, also the neutral aggression reference.
const DEFAULT_AGGRESSION: f64 = 0.40;
/// Prior fold share to an opening postflop bet and its virtual sample size
/// (the same sixteen opportunities at 40% the bet-response reads use).
const FOLD_PRIOR: f64 = 0.40;
const FOLD_PRIOR_WEIGHT: f64 = 16.0;

impl SquidRead {
    pub fn prior(seat: u8) -> Self {
        Self {
            seat,
            width: DEFAULT_WIDTH,
            aggression: DEFAULT_AGGRESSION,
            folds: [FOLD_PRIOR; 3],
            open_pressure: 0.0,
            opening: OpeningReads::default(),
        }
    }

    pub(super) fn fold_to_open(
        self,
        street: usize,
        persona: BotPersona,
        field_pressure: f64,
    ) -> f64 {
        let fold = self.folds[street];
        let fold = if fold.is_finite() {
            fold.clamp(0.0, 1.0)
        } else {
            FOLD_PRIOR
        };
        let pressure = if persona == BotPersona::Shark {
            self.open_pressure
        } else {
            field_pressure
        };
        adapt::open_fold(fold, pressure).clamp(0.0, 1.0)
    }

    /// Only a sole responder with chips behind faces the same river price as
    /// the heads-up observations. The live model verifies the profile owner.
    pub(super) fn opening_bet_fold(
        self,
        ctx: &DecisionContext<'_>,
        responder: &SeatPublic,
        field: usize,
        bet: u32,
        opponents: Option<&OpponentModel<'_>>,
    ) -> f64 {
        let street = (ctx.street as usize).saturating_sub(1);
        let ordinary_river = ctx.persona == BotPersona::Shark
            && ctx.street == Street::River
            && ctx.squid_defense.bonus > 0.0
            && ctx.current_bet.0 == 0
            && field == 1
            && !responder.all_in
            && bet < responder.stack
            && bet < ctx.stack_chips
            && bet <= ctx.pot.0;
        if ordinary_river {
            if let Some(read) = opponents
                .and_then(|m| m.river_response_stats(responder.seat))
                .and_then(|stats| {
                    bet_response_bucket(ctx.street, bet, ctx.pot.0)
                        .map(|(street, size)| stats.bet_responses[street][size])
                })
            {
                let fold = (f64::from(read.folded.min(read.faced))
                    + FOLD_PRIOR * FOLD_PRIOR_WEIGHT)
                    / (f64::from(read.faced) + FOLD_PRIOR_WEIGHT);
                return adapt::open_fold(fold, self.open_pressure).clamp(0.0, 1.0);
            }
        }
        self.fold_to_open(street, ctx.persona, ctx.open_pressure)
    }

    /// Ten virtual observations at each prior.
    pub fn observed(seat: u8, stats: &OpponentStats) -> Self {
        let smooth = |hits: u32, total: u32, prior: f64, floor: f64| {
            let total = f64::from(total);
            let hits = f64::from(hits).min(total);
            ((hits + prior * 10.0) / (total + 10.0)).clamp(floor, 1.0)
        };
        Self {
            seat,
            open_pressure: 0.0,
            width: smooth(stats.vpip_hands, stats.hands, DEFAULT_WIDTH, 0.05),
            opening: stats.opening_bets,
            aggression: smooth(
                stats.aggressive_actions,
                stats.postflop_actions,
                DEFAULT_AGGRESSION,
                0.0,
            ),
            folds: std::array::from_fn(|street| {
                // Overbets are a different price; pool the two sizes up to pot.
                let reads = &stats.bet_responses[street][..2];
                let faced: u32 = reads.iter().map(|r| r.faced).sum();
                let folded: u32 = reads.iter().map(|r| r.folded.min(r.faced)).sum();
                (f64::from(folded) + FOLD_PRIOR * FOLD_PRIOR_WEIGHT)
                    / (f64::from(faced) + FOLD_PRIOR_WEIGHT)
            }),
        }
    }
}

/// Fixed simulation seed: independent of the deck, dealer seed and action RNG.
const SEED: u64 = 0x5351_4446_4c49_4e45;
const TRIALS: u32 = 512;
/// Fewer valid joint samples than this leave the persona's fold unchanged.
const MIN_TRIALS: u32 = 256;
/// Showdown-chance factor per opponent still to act on this wager.
pub(super) const WAITING_FACTOR: f64 = 0.85;
/// Share of a call's showdown chance still realised against later betting,
/// preflop/flop/turn/river. Only a river call reaches showdown at this price.
const REALISED: [f64; 4] = [0.75, 0.80, 0.90, 1.0];
/// Estimation margin on everything the call can win.
const MARGIN: f64 = 0.05;
/// Share of the margin a full contest read removes ([`SquidDefense::contest`]).
const MARGIN_EASED: f64 = 0.5;
/// At most this many in-pot opponents are simulated.
pub(super) const MAX_FIELD: usize = 5;
/// Share of the preceding range kept by each full preflop re-raise.
const RERAISE_SHARE: f64 = 0.40;
/// Share of the range kept by a postflop bet of at most half pot, at most
/// pot, and above pot (or all-in), for a player of neutral aggression.
const BET_KEEP: [f64; 3] = [0.60, 0.50, 0.40];
/// Same for a raise over an earlier wager on the street.
const RAISE_KEEP: [f64; 3] = [0.40, 0.32, 0.25];
/// Share of the range kept by a postflop call.
const CALL_KEEP: f64 = 0.70;
/// Bounds of the aggression scaling of a wager's kept share, and of the share.
const AGGRESSION_SCALE: (f64, f64) = (0.70, 1.60);
const KEEP_BOUNDS: (f64, f64) = (0.15, 0.90);
/// Postflop share of each range left as any two cards: the model's own doubt.
/// A negative online read of the aggressor's line ("paid off when called")
/// shrinks it by up to half, like the line's range.
const MODEL_DOUBT: f64 = 0.12;
/// On the turn and river the token counts for at most this many pots. A
/// late-street call against a strong line wins mostly through the model's
/// doubt share, and a token several times the pot otherwise turned that guess
/// into the whole decision.
const LATE_BONUS_POTS: f64 = 2.0;

/// Turn a persona fold into a call when the squid value of winning the pot
/// pays for the call. Any other action is returned unchanged.
pub(super) fn defend(ctx: &DecisionContext<'_>, action: PlayerAction) -> PlayerAction {
    // An all-in already priced with its funded squid outcomes keeps its answer.
    if !matches!(action, PlayerAction::Fold) || ctx.to_call.0 == 0 || ctx.all_in_evidence.is_some()
    {
        return action;
    }
    if call_pays(ctx, ctx.to_call.0.min(ctx.stack_chips)) {
        call_or_all_in(ctx)
    } else {
        action
    }
}

/// The field a call of `call` chips meets, from the `live` opponents.
pub(super) struct CallField<'a> {
    /// What the call plays for: the pot plus the call, less the chips above
    /// what the hero can match (refunded, not won).
    pub pot: f64,
    /// The street's last aggressor, while it is still live.
    pub aggressor: Option<u8>,
    /// Opponents already in the pot: the aggressor first, then the rest in
    /// table order, at most [`MAX_FIELD`].
    pub in_pot: Vec<&'a SeatPublic>,
    /// Opponents the showdown estimate does not simulate: those still to act
    /// on the wager (they may fold) and those beyond the simulated field.
    pub unmodelled: usize,
}

/// [`CallField`] of `call`; `None` while no opponent is in the pot yet.
pub(super) fn call_field<'a>(
    ctx: &DecisionContext<'_>,
    live: &[&'a SeatPublic],
    call: u32,
) -> Option<CallField<'a>> {
    let hero_total = ctx.committed_this_street.0.saturating_add(ctx.stack_chips);
    let refund = live
        .iter()
        .map(|s| s.committed_this_street.saturating_sub(hero_total))
        .max()
        .unwrap_or(0);
    let pot = f64::from(ctx.pot.0.saturating_sub(refund)) + f64::from(call);
    let aggressor = ctx
        .last_aggressor_seat
        .filter(|seat| live.iter().any(|s| s.seat == *seat));
    // Opponents who already matched the wager are in the pot; the rest still
    // act and may fold.
    let (mut in_pot, waiting): (Vec<&SeatPublic>, Vec<&SeatPublic>) =
        live.iter().copied().partition(|s| {
            Some(s.seat) == aggressor || s.all_in || s.committed_this_street >= ctx.current_bet.0
        });
    if in_pot.is_empty() {
        return None;
    }
    in_pot.sort_by_key(|s| Some(s.seat) != aggressor);
    let unmodelled = waiting.len() + in_pot.len().saturating_sub(MAX_FIELD);
    in_pot.truncate(MAX_FIELD);
    Some(CallField {
        pot,
        aggressor,
        in_pot,
        unmodelled,
    })
}

/// Whether putting `call` more chips into this pot against the field facing
/// the hero now is paid for by the showdown share and the side value of
/// winning it ([`seven_deuce::win_bonus`]), with the estimation margin.
pub(super) fn call_pays(ctx: &DecisionContext<'_>, call: u32) -> bool {
    let bonus = seven_deuce::win_bonus(ctx);
    if !bonus.is_finite() || bonus <= 0.0 || !ctx.table.is_known() {
        return false;
    }
    if call == 0 {
        return false;
    }
    let live: Vec<&SeatPublic> = ctx
        .table
        .seats
        .iter()
        .filter(|s| s.seat != ctx.seat && !s.folded)
        .collect();
    if live.is_empty() {
        return false;
    }
    let Some(CallField {
        pot,
        aggressor,
        in_pot,
        unmodelled,
    }) = call_field(ctx, &live, call)
    else {
        return false;
    };
    let Some((win, share)) = showdown(ctx, &in_pot, aggressor, None) else {
        return false;
    };
    let street = street_index(ctx.street);
    let bonus = if street >= 2 {
        bonus.min(LATE_BONUS_POTS * pot)
    } else {
        bonus
    };
    // No later betting once the call is all-in or every opponent left is.
    let closed = call >= ctx.stack_chips || live.iter().all(|s| s.all_in);
    // A human in this pot who collects more than a fair share of the token
    // pots is getting them from folds like this one, so the caution eases
    // with that read — but only the part the estimate itself covers. The
    // discount for opponents it never simulates (still to act, or beyond the
    // field) stays whole: they are real. What eases is the realisation
    // haircut against the modelled field, and at most half the margin.
    let contest = if ctx.squid_defense.contest.is_finite() {
        ctx.squid_defense.contest.clamp(0.0, 1.0)
    } else {
        0.0
    };
    let realised = if closed {
        1.0
    } else {
        REALISED[street] + (1.0 - REALISED[street]) * contest
    };
    let factor = WAITING_FACTOR.powi(unmodelled as i32) * realised;
    let margin = MARGIN * (1.0 - MARGIN_EASED * contest);
    // Chops earn a pot share but no token.
    let value = factor * (share * pot + win * bonus);
    value >= f64::from(call) + margin * (pot + bonus)
}

/// A squid stab bets one of the Shark's own recognisable pot fractions, so an
/// exact size never marks a converted check (a fixed half pot was a tell).
const STAB_SIZES: [f64; 4] = [1.0 / 3.0, 0.50, 2.0 / 3.0, 0.75];
/// Domain separation for the stab-size fork of the bot RNG.
const STAB_FORK: u64 = 0x5351_5354_4142_5349;
/// Share of the answers to a stab that raise it instead of calling; a raised
/// stab is given up and loses the bet.
const STAB_RAISED: f64 = 0.20;
/// Stabs only into at most this many live opponents (all must fold).
const STAB_MAX_FIELD: usize = 2;

/// Turn a postflop check into a bet of a Shark pot fraction when the token makes taking the
/// pot now worth more than checking. Loose players collected most tokens by
/// betting the pots every bot checked to them.
/// Each opponent's fold chance is its own public fold share to an opening bet
/// on this street ([`SquidRead::folds`], its cross-table profile included),
/// moved by the online result of the bots' opening bets into the humans in
/// the pot ([`adapt::open_fold`]), so the stab is priced per player. A called
/// stab is priced against the part of each opponent's range that continues,
/// and a share of the answers raises it off the pot. The bot never leads into
/// the previous street's aggressor while that player still acts behind: the
/// fold share is measured on pots checked to the bettor. A flat 70% of the
/// showdown chance when called, no raises and leads into the bettor made the
/// stab bet too many hands. Inputs are the same public data as
/// [`defend`]; the action RNG is never consumed.
pub(super) fn attack(
    ctx: &mut DecisionContext<'_>,
    action: PlayerAction,
    opponents: Option<&OpponentModel<'_>>,
) -> PlayerAction {
    if action != PlayerAction::Check
        || ctx.to_call.0 != 0
        || ctx.street == Street::Preflop
        || !ctx.can_raise
        || ctx.stack_chips == 0
        || ctx.all_in_evidence.is_some()
    {
        return action;
    }
    let bonus = seven_deuce::win_bonus(ctx);
    if !bonus.is_finite() || bonus <= 0.0 || !ctx.table.is_known() {
        return action;
    }
    let live: Vec<&SeatPublic> = ctx
        .table
        .seats
        .iter()
        .filter(|s| s.seat != ctx.seat && !s.folded)
        .collect();
    // Nobody left to fold, or an all-in opponent who cannot.
    if live.is_empty() || live.len() > STAB_MAX_FIELD || live.iter().any(|s| s.all_in) {
        return action;
    }
    // The previous street's last bettor keeps the initiative until it checks.
    let previous = match ctx.street {
        Street::Preflop => return action,
        Street::Flop => Street::Preflop,
        Street::Turn => Street::Flop,
        Street::River => Street::Turn,
    };
    let history: &[PublicAction] = ctx.squid_defense.history.as_deref().unwrap_or_default();
    let initiative = history
        .iter()
        .rev()
        .find(|a| a.street == previous && a.increased_bet)
        .map(|a| a.seat)
        .filter(|seat| *seat != ctx.seat && live.iter().any(|s| s.seat == *seat));
    if initiative.is_some_and(|seat| {
        !history
            .iter()
            .any(|a| a.street == ctx.street && a.seat == seat)
    }) {
        return action;
    }
    let pot = f64::from(ctx.pot.0);
    // Size from a fork of the bot's server-side RNG: the seed never reaches
    // clients, so the size reveals nothing about the hole cards (a hash of
    // public state and the holding could be inverted from the public source),
    // and the action RNG itself is never advanced. Shark sizes snap like its
    // other bets (effective-stack cap included); other personas get their
    // usual size multiplier and jitter.
    let mut fork =
        StdRng::seed_from_u64(ctx.rng.clone().next_u64() ^ STAB_FORK ^ (ctx.street as u64) << 56);
    let size = STAB_SIZES[fork.gen_range(0..STAB_SIZES.len())];
    let bet = if ctx.persona == BotPersona::Shark {
        postflop::river_bet_chips(ctx, size)
    } else {
        let anchor = scaled_bet(ctx.profile, (pot * size).round() as u32);
        let factor =
            fork.gen_range((1.0 - POSTFLOP_BET_SIZE_JITTER)..=(1.0 + POSTFLOP_BET_SIZE_JITTER));
        (f64::from(anchor) * factor).round() as u32
    }
    .max(ctx.big_blind)
    .min(ctx.stack_chips);
    if bet == 0 || pot <= 0.0 {
        return action;
    }
    let folds: Vec<f64> = live
        .iter()
        .map(|s| {
            ctx.squid_defense
                .reads
                .iter()
                .find(|r| r.seat == s.seat)
                .copied()
                .unwrap_or_else(|| SquidRead::prior(s.seat))
                .opening_bet_fold(ctx, s, live.len(), bet, opponents)
        })
        .collect();
    let fold = folds.iter().product::<f64>();
    let Some((win, share)) = showdown(ctx, &live, None, None) else {
        return action;
    };
    // Called: every opponent holds the part of its range that does not fold.
    let continues: Vec<f64> = folds.iter().map(|f| 1.0 - f).collect();
    let Some((called_win, called_share)) = showdown(ctx, &live, None, Some(&continues)) else {
        return action;
    };
    let realised = REALISED[ctx.street as usize];
    let b = f64::from(bet);
    let check = realised * (share * pot + win * bonus);
    let called = realised * (called_share * (pot + 2.0 * b) + called_win * bonus) - b;
    let answered = (1.0 - STAB_RAISED) * called - STAB_RAISED * b;
    let stab = fold * (pot + bonus) + (1.0 - fold) * answered;
    if stab > check + MARGIN * (pot + bonus) {
        emit_bet(ctx, bet)
    } else {
        action
    }
}

/// Hero's outright-win chance and expected pot share against the modelled
/// ranges of `opponents`, by a bounded joint simulation on a fixed seed.
/// `continues` narrows each opponent further to the strongest share of its
/// range that would answer a bet now.
pub(super) fn showdown(
    ctx: &DecisionContext<'_>,
    opponents: &[&SeatPublic],
    aggressor: Option<u8>,
    continues: Option<&[f64]>,
) -> Option<(f64, f64)> {
    let known = ctx
        .board
        .all_cards()
        .into_iter()
        .fold(mask(ctx.hole), |m, c| m | card_bit(c));
    let deck: Vec<Card> = Rank::ALL
        .iter()
        .flat_map(|&r| Suit::ALL.iter().map(move |&s| Card::new(r, s)))
        .filter(|c| known & card_bit(*c) == 0)
        .collect();
    let combos: Vec<HoleCards> = deck
        .iter()
        .enumerate()
        .flat_map(|(i, &a)| deck[i + 1..].iter().map(move |&b| HoleCards::new(a, b)))
        .collect();
    let pools: Vec<Vec<(HoleCards, f64)>> = opponents
        .iter()
        .enumerate()
        .map(|(i, s)| {
            let mut weights = line_weights(ctx, &combos, s.seat, Some(s.seat) == aggressor);
            if let Some(keep) = continues.and_then(|c| c.get(i)) {
                keep_strongest(
                    &combos,
                    &mut weights,
                    ctx.board,
                    keep.clamp(KEEP_BOUNDS.0, KEEP_BOUNDS.1),
                );
            }
            combos.iter().copied().zip(weights).collect()
        })
        .collect();
    let mut rng = StdRng::seed_from_u64(SEED);
    let (mut wins, mut shares, mut trials) = (0.0, 0.0, 0u32);
    let sampling = HolePools::new(&pools);
    for _ in 0..TRIALS {
        let Some(holes) = sample_holes(&sampling, &mut rng) else {
            continue;
        };
        let used = holes.iter().fold(known, |m, h| m | mask(*h));
        let mut cards = ctx.board.all_cards();
        if cards.len() < 5 {
            let mut rest: Vec<Card> = deck
                .iter()
                .copied()
                .filter(|c| used & card_bit(*c) == 0)
                .collect();
            rest.shuffle(&mut rng);
            let missing = 5 - cards.len();
            cards.extend(rest.into_iter().take(missing));
        }
        let board = BoardCards {
            flop: Some([cards[0], cards[1], cards[2]]),
            turn: Some(cards[3]),
            river: Some(cards[4]),
        };
        let be = engine::eval::board_eval(&board);
        let hero = engine::eval::rank_with_board(&be, &ctx.hole);
        let mut tied = 0u32;
        let mut beaten = false;
        for h in &holes {
            let rank = engine::eval::rank_with_board(&be, h);
            if rank > hero {
                beaten = true;
                break;
            }
            tied += u32::from(rank == hero);
        }
        trials += 1;
        if !beaten {
            if tied == 0 {
                wins += 1.0;
                shares += 1.0;
            } else {
                shares += 1.0 / f64::from(tied + 1);
            }
        }
    }
    (trials >= MIN_TRIALS).then(|| (wins / f64::from(trials), shares / f64::from(trials)))
}

/// Weight of each combo in `seat`'s range given its public line this hand.
fn line_weights(
    ctx: &DecisionContext<'_>,
    combos: &[HoleCards],
    seat: u8,
    is_aggressor: bool,
) -> Vec<f64> {
    let read = ctx
        .squid_defense
        .reads
        .iter()
        .find(|r| r.seat == seat)
        .copied()
        .unwrap_or_else(|| SquidRead::prior(seat));
    let width = if read.width.is_finite() {
        read.width.clamp(0.05, 1.0)
    } else {
        DEFAULT_WIDTH
    };
    let aggression = if read.aggression.is_finite() {
        read.aggression.clamp(0.0, 1.0)
    } else {
        DEFAULT_AGGRESSION
    };
    let history: &[PublicAction] = ctx.squid_defense.history.as_deref().unwrap_or_default();
    let own: Vec<(usize, &PublicAction)> = history
        .iter()
        .enumerate()
        .filter(|(_, a)| a.seat == seat && (a.increased_bet || a.is_call_like()))
        .collect();
    // The online layer's read of the human line faced now (0 for bots).
    let last_wager = own
        .iter()
        .rev()
        .find(|(_, a)| a.street == ctx.street && a.increased_bet)
        .map(|(i, _)| *i);
    let pressure = if is_aggressor { ctx.line_pressure } else { 0.0 };

    // Preflop: the public VPIP width, narrower after each re-raise. A player with
    // no voluntary preflop action (a blind who checked) holds any two cards;
    // without a history every seat gets its width.
    let preflop: Vec<_> = own
        .iter()
        .filter(|(_, a)| a.street == Street::Preflop)
        .collect();
    let mut weights = vec![1.0; combos.len()];
    if !preflop.is_empty() || ctx.squid_defense.history.is_none() {
        let reraised = preflop.iter().any(|(i, a)| {
            a.increased_bet
                && history[..*i]
                    .iter()
                    .any(|b| b.street == Street::Preflop && b.increased_bet)
        });
        // Keep the depth of this seat's own raise, not simply the final pot
        // depth. A 3-bettor who calls a 4-bet did not make a 4-bet.
        let raise_depth = preflop_line(history, seat).raised_at;
        let reraises = raise_depth.saturating_sub(1).max(u8::from(reraised));
        let mut pct = width * RERAISE_SHARE.powi(i32::from(reraises));
        if ctx.street == Street::Preflop && last_wager.is_some() {
            pct = adapt::range_width(pct, pressure);
        }
        let order = rfi_priority_order(OpenBucket::Button);
        for (w, h) in weights.iter_mut().zip(combos) {
            let hi = h.card1.rank.max(h.card2.rank);
            let lo = h.card1.rank.min(h.card2.rank);
            *w = class_mix_in_order(order, (hi, lo, h.card1.suit == h.card2.suit), pct * 100.0);
        }
        if weights.iter().all(|w| *w <= 0.0) {
            weights.fill(1.0);
        }
    }

    // Postflop: each wager or call keeps the strongest share of the range on
    // that street's board; larger wagers keep less, frequent bettors more.
    let scale = (aggression / DEFAULT_AGGRESSION).clamp(AGGRESSION_SCALE.0, AGGRESSION_SCALE.1);
    let chances = if own
        .iter()
        .any(|(_, a)| a.street != Street::Preflop && a.increased_bet && a.to_call_before == 0)
    {
        opening_read::chances(history, ctx.table.seats.len())
    } else {
        Vec::new()
    };
    for &(index, a) in own.iter().filter(|(_, a)| a.street != Street::Preflop) {
        let mut keep = if a.increased_bet {
            let size = opening_read::size_class(
                &a.action,
                a.amount.saturating_sub(a.to_call_before),
                a.pot_before.saturating_add(a.to_call_before),
            );
            let base = if a.to_call_before > 0 {
                RAISE_KEEP[size]
            } else {
                BET_KEEP[size]
            };
            let authored = (base * scale).clamp(KEEP_BOUNDS.0, KEEP_BOUNDS.1);
            // An opening bet keeps at least the share of its spot the player
            // bets at this size, less as this line's online read turns negative.
            chances
                .iter()
                .find(|c| c.index == index && c.seat == seat)
                .map_or(authored, |c| {
                    opening_read::keep(&read.opening, c.spot, size, authored, pressure)
                })
                .clamp(KEEP_BOUNDS.0, KEEP_BOUNDS.1)
        } else {
            CALL_KEEP
        };
        if Some(index) == last_wager && ctx.street != Street::Preflop {
            keep = adapt::range_width(keep, pressure).clamp(KEEP_BOUNDS.0, KEEP_BOUNDS.1);
        }
        let mut board = ctx.board.clone();
        if a.street == Street::Flop {
            board.turn = None;
            board.river = None;
        }
        if a.street == Street::Turn {
            board.river = None;
        }
        keep_strongest(combos, &mut weights, &board, keep);
    }
    let total: f64 = weights.iter().sum();
    if ctx.street != Street::Preflop && total > 0.0 {
        let doubt = adapt::range_width(MODEL_DOUBT, pressure.min(0.0));
        let uniform = doubt * total / weights.len() as f64;
        for w in &mut weights {
            *w = *w * (1.0 - doubt) + uniform;
        }
    }
    weights
}
