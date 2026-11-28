//! Shark range reconstruction. Authored likelihoods, not an equilibrium solve.
//! Only public actions and the board visible on each historical street enter
//! the model. River equity is exact against that model, not against known cards.

use super::*;

pub(super) fn historical_reach_role(
    ctx: &DecisionContext<'_>,
    seat: u8,
    profile: &PersonaProfile,
) -> Option<ReachRole> {
    let line = preflop_line(ctx.public_history?, seat);
    let position = ctx.table.position_of(seat)?;
    if let Some(depth) = line
        .called_at
        .filter(|&depth| line.raised_at == 0 || depth > line.raised_at)
    {
        return Some(match depth {
            0 => ReachRole::Uniform,
            1 => {
                let opener = line.opener?;
                let mut spot = facing_open_spot_vs(ctx, opener);
                spot.hero = if seat == ctx.bb_seat {
                    HeroBucket::BigBlind
                } else if seat == ctx.sb_seat {
                    HeroBucket::SmallBlind
                } else {
                    HeroBucket::InPosition
                };
                let widths = reference_facing_open_widths(spot);
                ReachRole::Caller {
                    three_bet_pct: (widths.three_bet_pct * three_bet_scale(profile)).min(45.0),
                    call_pct: (widths.call_pct * preflop_looseness(profile)).min(60.0),
                }
            }
            2 => reraise_caller_role(ctx, seat, profile, position, depth).unwrap_or(
                ReachRole::ThreeBetCaller {
                    pct: 4.0 * preflop_looseness(profile),
                },
            ),
            _ => reraise_caller_role(ctx, seat, profile, position, depth).unwrap_or(
                ReachRole::ThreeBetCaller {
                    pct: 1.5 * preflop_looseness(profile),
                },
            ),
        });
    }
    Some(match line.raised_at {
        0 => ReachRole::Uniform,
        1 => ReachRole::Opener {
            bucket: OpenBucket::for_position(position),
            pct: profile.rfi_pct[rfi_knob_index(position)] as f64,
        },
        2 => ReachRole::ThreeBettor {
            pct: THREE_BET_REACH_PCT * three_bet_scale(profile),
        },
        // Keep a narrower authored value/blocker range for 4-bets. This is
        // still an approximation, not a stack-specific solved chart.
        _ => ReachRole::ThreeBettor {
            pct: profile.four_bet_pct.max(1) as f64,
        },
    })
}

/// The bot's own range after it called a 3-bet or 4-bet. The vs-re-raise
/// charts flat a fixed narrow width, but `defends_reraise_by_mdf` also keeps a
/// price-determined share of the range the bot entered with; without those
/// hands in the reach range every one of them sits at the bottom of an
/// imagined tight range postflop and folds to any bet. Rebuilt from the public
/// action history with the same widths the preflop decision used. Own seat
/// only: an opponent's persona and defence rule are not known. `None` (the
/// chart-only range) when the seat had not voluntarily entered, the entry is
/// not a chart line, or the minimum defense did not apply (an all-in
/// re-raise, or a call above the flat price).
fn reraise_caller_role(
    ctx: &DecisionContext<'_>,
    seat: u8,
    profile: &PersonaProfile,
    position: Position,
    depth: u8,
) -> Option<ReachRole> {
    // A call of a 5-bet or deeper is AA/KK only (`decide_vs_5bet_plus`).
    if seat != ctx.seat || ctx.persona != BotPersona::Shark || depth >= 4 {
        return None;
    }
    let history = ctx.public_history?;
    let preflop = || history.iter().filter(|a| a.street == Street::Preflop);
    // The call being explained: this seat's last preflop call.
    let call_index = preflop()
        .enumerate()
        .filter(|(_, a)| a.seat == seat && a.is_call_like())
        .last()?
        .0;
    let call = preflop().nth(call_index)?;
    let mut raises = 0usize;
    let mut callers = 0usize;
    let mut entry = None;
    let mut aggressor = None;
    let mut defenders: Vec<u8> = vec![seat];
    for action in preflop().take(call_index) {
        let voluntary = action.increased_bet || action.is_call_like();
        if action.seat == seat && voluntary {
            entry = Some(EntryAction {
                raised: action.increased_bet,
                raises_before: raises,
                callers_before: callers,
            });
        }
        if action.increased_bet {
            aggressor = Some(action);
            raises += 1;
        } else if action.is_call_like() {
            callers += 1;
        }
        if matches!(action.action, PlayerAction::Fold) {
            defenders.retain(|&s| s != action.seat);
        } else if voluntary && !defenders.contains(&action.seat) {
            defenders.push(action.seat);
        }
    }
    let aggressor = aggressor.filter(|a| a.seat != seat)?;
    defenders.retain(|&s| s != aggressor.seat);
    // Effective stack against the aggressor when the call was made, as the
    // preflop price guard measured it: each seat's stack at the start of the
    // hand (what it holds now plus everything it has put in). Without the
    // hand contributions, later-street wagers are not added back and the
    // guard is stricter than the preflop one.
    let stack_at_start = |seat: u8, preflop_commit: u32| {
        let entry = ctx.table.seat_entry(seat)?;
        let paid = ctx
            .all_in_public
            .as_ref()
            .and_then(|p| p.contributions.iter().find(|(s, _)| *s == seat))
            .map_or(
                entry.committed_this_street.saturating_add(preflop_commit),
                |(_, paid)| *paid,
            );
        Some(entry.stack.saturating_add(paid))
    };
    let stack_at_call = stack_at_start(seat, call.committed_to)?
        .min(stack_at_start(aggressor.seat, aggressor.committed_to)?);
    if matches!(aggressor.action, PlayerAction::AllIn)
        || call.to_call_before == 0
        || call.amount < call.to_call_before
        || f64::from(call.to_call_before) > f64::from(stack_at_call) * PREFLOP_FLAT_STACK_FRACTION
    {
        return None;
    }
    let opener = preflop().find(|a| a.increased_bet)?.seat;
    let mut spot = facing_open_spot_vs(ctx, opener);
    spot.hero = if seat == ctx.bb_seat {
        HeroBucket::BigBlind
    } else if seat == ctx.sb_seat {
        HeroBucket::SmallBlind
    } else {
        HeroBucket::InPosition
    };
    // Whether the earlier flat of a 3-bet was in position is not rebuilt;
    // the 4.0 reference sits between the chart's 5 (in) and 3 (out).
    let (order, entry_pct) = entry_range(
        profile,
        position,
        seat == ctx.bb_seat && !ctx.has_straddle,
        spot,
        4.0,
        entry?,
    )?;
    let share = reraise_defence_share(
        call.pot_before,
        aggressor.committed_to,
        call.to_call_before,
        defenders.len(),
    );
    Some(ReachRole::ReraiseCaller {
        chart_pct: if depth == 2 { 4.0 } else { 1.5 } * preflop_looseness(profile),
        entry: order,
        kept_pct: entry_pct * share,
        raises_premiums: depth == 2,
    })
}

#[derive(Clone, Copy)]
enum LineAction {
    Check,
    Call,
    Bet,
}

struct StreetEvidence {
    board: BoardCards,
    evaluator: engine::eval::BoardEval,
    action: LineAction,
    pressure: f64,
    raised: bool,
    /// Price of the last wager this seat called: wager ÷ pot after it.
    call_alpha: f64,
    /// Own seat only: the weakest holding the defence share kept when this
    /// seat called (`own_call_cutoffs`).
    call_cutoff: Option<PostflopScore>,
    /// Opponent only: share of this street's opening-bet spot the player bets
    /// ([`LineEvidence::with_bet_rates`]); `None` keeps the authored likelihoods.
    bet_rate: Option<f64>,
}

/// At most one summary per street, built once outside combination enumeration.
/// A bet followed by a call retains its aggression and records the extra
/// pressure. No future turn/river card can explain an earlier action.
pub(super) struct LineEvidence(Vec<StreetEvidence>);

/// The opening-bet share the authored bet likelihoods stand for.
const AUTHORED_BET_RATE: f64 = 0.50;

impl LineEvidence {
    pub(super) fn new(ctx: &DecisionContext<'_>, seat: u8) -> Self {
        Self::with_reach(ctx, seat, None)
    }

    pub(super) fn with_reach(
        ctx: &DecisionContext<'_>,
        seat: u8,
        reach: Option<&preflop_blueprint::FlopReach>,
    ) -> Self {
        let mut result =
            Self::from_streets(ctx, seat, &[Street::Flop, Street::Turn, Street::River]);
        if seat == ctx.seat && ctx.persona == BotPersona::Shark {
            own_call_cutoffs(ctx, &mut result.0, reach);
        }
        result
    }

    pub(super) fn before_turn(ctx: &DecisionContext<'_>, seat: u8) -> Self {
        Self::from_streets(ctx, seat, &[Street::Flop])
    }

    /// Public river-root prior: both players get the same reference likelihood
    /// model for their own observed flop and turn actions. The river tree
    /// accounts for its own actions. Neither hero identity nor cards enter.
    pub(super) fn before_river(ctx: &DecisionContext<'_>, seat: u8) -> Self {
        Self::from_streets(ctx, seat, &[Street::Flop, Street::Turn])
    }

    /// The first two seats have already checked when the three-way river
    /// subgame begins. A later reply to the button belongs to that subgame.
    pub(super) fn after_river_check(ctx: &DecisionContext<'_>, seat: u8) -> Self {
        let mut prior = Self::before_river(ctx, seat);
        prior.0.push(StreetEvidence {
            board: ctx.board.clone(),
            evaluator: engine::eval::board_eval(ctx.board),
            action: LineAction::Check,
            pressure: 0.0,
            raised: false,
            call_alpha: 0.0,
            call_cutoff: None,
            bet_rate: None,
        });
        prior
    }

    fn from_streets(ctx: &DecisionContext<'_>, seat: u8, streets: &[Street]) -> Self {
        let mut evidence = Vec::with_capacity(streets.len());
        for &street in streets {
            let board = BoardCards {
                flop: ctx.board.flop,
                turn: if street != Street::Flop {
                    ctx.board.turn
                } else {
                    None
                },
                river: if street == Street::River {
                    ctx.board.river
                } else {
                    None
                },
            };
            let expected = match street {
                Street::Flop => 3,
                Street::Turn => 4,
                _ => 5,
            };
            if board.count() != expected {
                continue;
            }
            let mut last = None;
            let mut pressure: f64 = 0.0;
            let mut bets = 0u8;
            let mut raised = false;
            let mut call_alpha = 0.0;
            for a in ctx
                .public_history
                .unwrap_or_default()
                .iter()
                .filter(|a| a.seat == seat && a.street == street)
            {
                if a.increased_bet {
                    bets = bets.saturating_add(1);
                    raised |= a.to_call_before > 0;
                    last = Some(LineAction::Bet);
                    pressure = pressure.max(a.amount as f64 / a.pot_before.max(1) as f64);
                } else if a.is_call_like() {
                    if bets == 0 {
                        last = Some(LineAction::Call);
                    }
                    pressure = pressure.max(a.to_call_before as f64 / a.pot_before.max(1) as f64);
                    let (wager, pot) =
                        (f64::from(a.to_call_before), f64::from(a.pot_before.max(1)));
                    call_alpha = if wager >= pot {
                        wager / (pot + wager)
                    } else {
                        wager / pot
                    };
                } else if matches!(a.action, PlayerAction::Check) && bets == 0 {
                    last = Some(LineAction::Check);
                }
            }
            if let Some(action) = last {
                evidence.push(StreetEvidence {
                    evaluator: engine::eval::board_eval(&board),
                    board,
                    action,
                    pressure: pressure.min(3.0),
                    raised,
                    call_alpha,
                    call_cutoff: None,
                    bet_rate: None,
                });
            }
        }

        Self(evidence)
    }

    /// Widen each opening bet of `seat` to how often the player bets that
    /// spot ([`opening_read`]): the authored likelihoods describe a player who
    /// bets about [`AUTHORED_BET_RATE`] of the time there, so a player who
    /// bets almost every time keeps almost every holding in the range. An
    /// unread or rarer bettor, or one whose line the bots have been calling
    /// and losing to ([`opening_read::keep`]), keeps the authored model.
    pub(super) fn with_bet_rates(mut self, ctx: &DecisionContext<'_>, seat: u8) -> Self {
        let Some(history) = ctx.public_history else {
            return self;
        };
        let Some(read) = ctx.squid_defense.reads.iter().find(|r| r.seat == seat) else {
            return self;
        };
        let chances = opening_read::chances(history, ctx.table.seats.len());
        for evidence in &mut self.0 {
            if !matches!(evidence.action, LineAction::Bet) || evidence.raised {
                continue;
            }
            let street = evidence.board.count().saturating_sub(3);
            evidence.bet_rate = chances
                .iter()
                .find(|c| c.seat == seat && c.spot.street == street && c.bet.is_some())
                .map(|c| {
                    let size = c.bet.unwrap_or(0);
                    // The bettor's line read: a negative one returns the
                    // authored likelihoods.
                    opening_read::keep(
                        &read.opening,
                        c.spot,
                        size,
                        AUTHORED_BET_RATE,
                        ctx.line_pressure,
                    )
                });
        }
        self
    }

    pub(super) fn weight(&self, hole: HoleCards) -> f64 {
        self.0.iter().fold(1.0, |weight, evidence| {
            let score = postflop_score(hole, &evidence.board, &evidence.evaluator);
            weight * evidence.likelihood(score)
        })
    }
}

impl StreetEvidence {
    fn likelihood(&self, score: PostflopScore) -> f64 {
        let evidence = self;
        if let (LineAction::Call, Some(cutoff)) = (evidence.action, evidence.call_cutoff) {
            // The bot's own call is its defence share, not a guess: every
            // holding at or above the cut-off continued, draws continue
            // on price, and the rest only through the mixing band.
            return if score >= cutoff {
                1.0
            } else if score.outs >= 8 {
                0.70
            } else if score.outs >= 4 {
                0.30
            } else {
                0.05
            };
        }
        let strong = score.tier >= TIER_TWO_PAIR;
        let pair = score.tier >= TIER_SECOND_PAIR;
        let draw = score.outs >= 8;
        // Positive likelihoods retain slowplays and bluffs. These values
        // express a conservative prior, never certainty about a holding.
        let mut likelihood: f64 = match evidence.action {
            LineAction::Check => {
                if strong {
                    0.35
                } else if draw || pair {
                    0.60
                } else {
                    0.85
                }
            }
            LineAction::Call => {
                if strong {
                    0.65
                } else if draw {
                    0.70
                } else if pair {
                    0.60
                } else {
                    0.10
                }
            }
            LineAction::Bet => {
                if strong {
                    0.85
                } else if draw {
                    0.65
                } else if pair {
                    0.50
                } else {
                    0.18
                }
            }
        };
        if !strong && !draw && (evidence.pressure > 0.75 || evidence.raised) {
            likelihood *= if pair { 0.65 } else { 0.75 };
        }
        match (evidence.action, evidence.bet_rate) {
            (LineAction::Bet, Some(rate)) if rate.is_finite() => {
                // `keep` never reads below the authored rate.
                let rate = rate.clamp(AUTHORED_BET_RATE, 1.0);
                1.0 - (1.0 - likelihood) * (1.0 - rate) / (1.0 - AUTHORED_BET_RATE)
            }
            _ => likelihood,
        }
    }
}

/// The Shark's own earlier calls, replayed against its own range. The generic
/// call likelihood treats a call as mostly pairs and draws, but the bot calls
/// with the top defence share of its range (`decide_postflop_vs_bet`), which
/// on most boards reaches well below a pair. Modelled the generic way, the
/// hands it really called with sat at the bottom of its imagined range on the
/// next street and folded to any second bet. For each street the bot called
/// on, in order, this finds the weakest holding inside the share it kept at
/// that price, given the range that reached that street. Public history, the
/// board visible on that street and the bot's own profile only.
fn own_call_cutoffs(
    ctx: &DecisionContext<'_>,
    evidence: &mut [StreetEvidence],
    reach: Option<&preflop_blueprint::FlopReach>,
) {
    if !evidence
        .iter()
        .any(|e| matches!(e.action, LineAction::Call))
    {
        return;
    }
    let role = historical_reach_role(ctx, ctx.seat, ctx.profile);
    if role.is_none() && reach.is_none() {
        return;
    }
    let board = ctx.board.all_cards();
    let cards: Vec<_> = (0..52u8)
        .map(|i| engine::Card {
            rank: Rank::ALL[(i % 13) as usize],
            suit: engine::Suit::ALL[(i / 13) as usize],
        })
        .filter(|c| !board.contains(c))
        .collect();
    let mut classes = std::collections::HashMap::with_capacity(169);
    let mut range: Vec<(HoleCards, f64)> = Vec::with_capacity(1_326);
    for (i, &a) in cards.iter().enumerate() {
        for &b in &cards[i + 1..] {
            let hole = HoleCards::new(a, b);
            let class = hand_class(hole);
            let weight = *classes.entry(class).or_insert_with(|| {
                reach
                    .and_then(|r| r.weight(ctx.seat, hole))
                    .unwrap_or_else(|| role.map_or(1.0, |role| reach_weight(role, class)))
            });
            if weight > 0.0 {
                range.push((hole, weight));
            }
        }
    }
    let in_position = is_in_position(ctx);
    for street in evidence.iter_mut() {
        let scores: Vec<PostflopScore> = range
            .iter()
            .map(|(hole, _)| postflop_score(*hole, &street.board, &street.evaluator))
            .collect();
        if matches!(street.action, LineAction::Call) {
            // Same share as `decide_postflop_vs_bet` heads-up; the flop float
            // offset and the multiway shading are not rebuilt.
            let mut defend = (1.0 - street.call_alpha)
                * ((100 - ctx.profile.fold_to_cbet_pct) as f64 / 45.0).clamp(0.5, 1.4);
            if !in_position && street.board.count() < 5 {
                defend *= OOP_DEFENCE_SCALE;
            }
            let defend = defend.clamp(DEFENCE_SHARE_MIN, DEFENCE_SHARE_MAX);
            let total: f64 = range.iter().map(|(_, w)| w).sum();
            let mut order: Vec<usize> = (0..range.len()).collect();
            order.sort_unstable_by(|&a, &b| scores[b].cmp(&scores[a]));
            let mut kept = 0.0;
            for index in order {
                kept += range[index].1;
                if kept >= defend * total {
                    street.call_cutoff = Some(scores[index]);
                    break;
                }
            }
        }
        for ((_, weight), score) in range.iter_mut().zip(&scores) {
            *weight *= street.likelihood(*score);
        }
    }
}

/// Enumerate <=990 legal river holdings, including card removal and ties.
/// Heads-up/full calls only: no multiway independence or side-pot shortcuts.
pub(super) fn shark_river_equity(ctx: &DecisionContext<'_>) -> Option<f64> {
    if ctx.persona != BotPersona::Shark
        || ctx.street != Street::River
        || ctx.board.count() != 5
        || ctx.to_call.0 == 0
        || ctx.to_call.0 > ctx.stack_chips
        || ctx.table.live_opponents(ctx.seat) != Some(1)
    {
        return None;
    }
    let villain = ctx
        .table
        .seats
        .iter()
        .find(|s| s.seat != ctx.seat && !s.folded)?;
    // Never infer the opponent's hidden persona; use a neutral prior and only
    // public actions. A small full-support mixture tolerates off-chart play.
    let reach = (ctx.table.seats.len() == 2)
        .then(|| preflop_blueprint::flop_reach(ctx))
        .flatten();
    let role = historical_reach_role(ctx, villain.seat, lookup_profile(BotPersona::ClubRegular));
    if reach.is_none() && role.is_none() {
        return None;
    }
    let line = LineEvidence::new(ctx, villain.seat).with_bet_rates(ctx, villain.seat);
    let evaluator = engine::eval::board_eval(ctx.board);
    let hero = engine::eval::rank_with_board(&evaluator, &ctx.hole);
    let mut dead = ctx.board.all_cards();
    dead.extend(ctx.hole.as_array());
    let cards: Vec<_> = (0..52u8)
        .map(|i| engine::Card {
            rank: Rank::ALL[(i % 13) as usize],
            suit: engine::Suit::ALL[(i / 13) as usize],
        })
        .filter(|c| !dead.contains(c))
        .collect();
    let mut classes = std::collections::HashMap::with_capacity(169);
    let mut total = 0.0;
    let mut wins = 0.0;
    let mut prior_sum = 0.0;
    let mut combos = 0u32;
    let mut broad_total = 0.0;
    let mut broad_wins = 0.0;
    for (i, &a) in cards.iter().enumerate() {
        for &b in &cards[i + 1..] {
            let hole = HoleCards::new(a, b);
            let class = hand_class(hole);
            let prior = *classes.entry(class).or_insert_with(|| {
                reach
                    .as_ref()
                    .and_then(|r| r.weight(villain.seat, hole))
                    .unwrap_or_else(|| role.map_or(1.0, |role| reach_weight(role, class)))
            });
            let likelihood = line.weight(hole);
            let weight = prior * likelihood;
            prior_sum += prior;
            combos += 1;
            let rank = engine::eval::rank_with_board(&evaluator, &hole);
            total += weight;
            let share = match hero.cmp(&rank) {
                std::cmp::Ordering::Greater => 1.0,
                std::cmp::Ordering::Equal => 0.5,
                std::cmp::Ordering::Less => 0.0,
            };
            wins += weight * share;
            broad_total += likelihood;
            broad_wins += likelihood * share;
        }
    }
    // Preserve the previous off-chart allowance for a chart prior. Action
    // likelihoods can have arbitrarily small total mass on a rare line, so
    // normalize their prior before mixing the 0.1% any-two component.
    let floor = if reach.is_some() {
        0.001 * prior_sum / f64::from(combos)
    } else {
        0.001
    };
    let total = 0.999 * total + floor * broad_total;
    let wins = 0.999 * wins + floor * broad_wins;
    (total > 0.0).then(|| (wins / total).clamp(0.0, 1.0))
}
