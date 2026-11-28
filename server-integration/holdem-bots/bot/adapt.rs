//! Hold'em lines for the online counter-exploitation layer
//! ([`crate::bot_adapt`]) and the bounded ways every persona consumes them.
//!
//! A line is classified from one public wager increase by a human:
//!
//! | id | line |
//! |----|------|
//! | 0 | preflop first-in raise to ≤ 3 bb |
//! | 1 | preflop first-in raise to ≤ 6 bb |
//! | 2 | preflop first-in raise to ≤ 15 bb |
//! | 3 | preflop first-in raise above 15 bb, not all-in |
//! | 4 | preflop re-raise (3-bet or later), not all-in |
//! | 5 | preflop all-in |
//! | 6–8 | flop opening bet: ≤ half pot, ≤ pot, overbet or all-in |
//! | 9–11 | turn, same sizes |
//! | 12–14 | river, same sizes |
//! | 15–23 | flop/turn/river raise over a wager, same sizes |
//! | 24–26 | a bot's own flop/turn/river opening bet into the human |
//! | 30 | the human's share of the token pots of squid rounds with bots |
//! | 31–33 | flop/turn/river big opening bet, in big blinds |
//! | 34–36 | flop/turn/river big raise over a wager, in big blinds |
//! | 37 | preflop 4-bet or later, not all-in |
//! | 40 + postflop line | same postflop wager in a 4-bet or larger pot |
//!
//! A big wager is read twice: under its pot-fraction line and under the big
//! line of its street. Pot fractions pool a half-pot bet of 5 bb with one of
//! 150 bb: a player who takes small pots with wide bets but puts big chips
//! in only with a hand had one mixed read while the bots paid the big pots
//! off. The big read leads where it has evidence; the
//! pot-fraction read fills in the rest. A large river opening bet short of
//! all-in can round up by one big blind and still share the pot-sized read.
//! Preflop sizes already have their own lines.
//! Big opening bets and raises also stay separate: pooling them let frequent
//! uncontested bets widen the read of a rare value raise. Retired cells 27–29
//! mixed both actions and cannot supply either new read.
//!
//! A raise is its own line: a player who bets wide and raises only made
//! hands otherwise gets one mixed read that can lead to too many calls.
//!
//! Preflop sizes are the raise-to total in big blinds (straddles and antes
//! are dead money, not part of the size). Postflop size is the wager increase
//! over the pot after calling. With pressure 0 every adjustment below is an
//! exact identity, so the base strategy is unchanged.

use super::DecisionContext;
use crate::bot_adapt::{Game, Observation, Outcome};
use engine::{PlayerAction, PlayerId, Street, StreetActionFact};
use rand::{rngs::StdRng, Rng, RngCore, SeedableRng};
use std::collections::HashSet;
use uuid::Uuid;

const PREFLOP_OPEN_3BB: u8 = 0;
const PREFLOP_OPEN_6BB: u8 = 1;
const PREFLOP_OPEN_15BB: u8 = 2;
const PREFLOP_OPEN_LARGE: u8 = 3;
const PREFLOP_RERAISE: u8 = 4;
const PREFLOP_ALL_IN: u8 = 5;
const POSTFLOP_BASE: u8 = 6;
const POSTFLOP_RAISE_BASE: u8 = 15;
const OPEN_BASE: u8 = 24;
const POT_SHARE: u8 = 30;
const BIG_BET_BASE: u8 = 31;
const BIG_RAISE_BASE: u8 = 34;
const PREFLOP_DEEP_RERAISE: u8 = 37;
const FOUR_BET_POT_OFFSET: u8 = 40;

// 80–95: strictly river all-ins. Separate opening bets, raises and four-bet pots.
fn river_shove_line(fact: &StreetActionFact, own_call: u32, four_bet_pot: bool) -> u8 {
    let price = fact.to_call_before.0;
    let raise = fact.amount.0.saturating_sub(price).min(own_call);
    let pot = u64::from(fact.pot_before.0) + u64::from(price);
    80 + 8 * u8::from(four_bet_pot)
        + 4 * u8::from(price > 0)
        + crate::bot_adapt::river_shove_size(u64::from(raise), pot)
}

/// Count only full voluntary raises. A straddle or short all-in does not
/// move the hand into a different postflop reading.
pub fn four_bet_pot(facts: &[StreetActionFact]) -> bool {
    facts.iter().filter(|fact| fact.full_raise).nth(2).is_some()
}

fn postflop_line(line: u8, four_bet_pot: bool) -> u8 {
    if four_bet_pot {
        line + FOUR_BET_POT_OFFSET
    } else {
        line
    }
}

/// The big line of `fact` on a postflop `street`: a wager increase of at
/// least [`super::commitment::START_BB`] big blinds.
fn big_line(street: Street, fact: &StreetActionFact, big_blind: u32) -> Option<u8> {
    let street = match street {
        Street::Preflop => return None,
        Street::Flop => 0,
        Street::Turn => 1,
        Street::River => 2,
    };
    let raise = f64::from(fact.amount.0.saturating_sub(fact.to_call_before.0));
    let base = if fact.to_call_before.0 > 0 {
        BIG_RAISE_BASE
    } else {
        BIG_BET_BASE
    };
    (fact.increased_bet && raise >= super::commitment::START_BB * f64::from(big_blind.max(1)))
        .then_some(base + street)
}

/// The line of `facts[index]` when it is a wager increase on `street`.
/// `all_in` = the actor had no chips behind after it.
fn line(
    street: Street,
    facts: &[StreetActionFact],
    index: usize,
    big_blind: u32,
    all_in: bool,
    four_bet_pot: bool,
) -> Option<u8> {
    let fact = facts.get(index).filter(|f| f.increased_bet)?;
    let street_index = match street {
        Street::Preflop => {
            let bb = u64::from(big_blind.max(1));
            let to = u64::from(fact.committed_to.0);
            return Some(if all_in {
                PREFLOP_ALL_IN
            } else if fact.full_raise
                && facts[..index]
                    .iter()
                    .filter(|f| f.full_raise)
                    .nth(1)
                    .is_some()
            {
                PREFLOP_DEEP_RERAISE
            } else if facts[..index].iter().any(|f| f.increased_bet) {
                PREFLOP_RERAISE
            } else if to <= 3 * bb {
                PREFLOP_OPEN_3BB
            } else if to <= 6 * bb {
                PREFLOP_OPEN_6BB
            } else if to <= 15 * bb {
                PREFLOP_OPEN_15BB
            } else {
                PREFLOP_OPEN_LARGE
            });
        }
        Street::Flop => 0,
        Street::Turn => 1,
        Street::River => 2,
    };
    let raise = u64::from(fact.amount.0.saturating_sub(fact.to_call_before.0));
    let pot = u64::from(fact.pot_before.0) + u64::from(fact.to_call_before.0);
    // A pot-sized river bet rounded up by a chip step represents the same
    // pressure as an exact pot bet. With a 40-chip big blind, 4,980 into 4,970
    // went into a different read from 5,540 into 5,540 two minutes earlier.
    let over_pot = raise > pot
        && !(street == Street::River
            && fact.to_call_before.0 == 0
            && raise <= pot + u64::from(big_blind)
            && big_line(street, fact, big_blind).is_some());
    let size = if all_in || over_pot {
        2
    } else if raise <= pot.div_ceil(2) {
        0
    } else {
        1
    };
    let base = if fact.to_call_before.0 > 0 {
        POSTFLOP_RAISE_BASE
    } else {
        POSTFLOP_BASE
    };
    Some(postflop_line(base + 3 * street_index + size, four_bet_pot))
}

fn open_line(street: Street) -> Option<u8> {
    Some(match street {
        Street::Preflop => return None,
        Street::Flop => OPEN_BASE,
        Street::Turn => OPEN_BASE + 1,
        Street::River => OPEN_BASE + 2,
    })
}

/// Source of online line and token reads at this table. Shark deviates for
/// this particular human; the other personas also use population experience.
pub fn player_pressure(
    persona: super::BotPersona,
    beans: bool,
    line: u8,
    user: Option<Uuid>,
) -> f64 {
    if persona == super::BotPersona::Shark {
        let own = user.map_or(0.0, |id| {
            crate::bot_adapt::personal_pressure(Game::Holdem, beans, line, id)
        });
        if line == POT_SHARE {
            // Token share has its own update scale and noise floor.
            own
        } else {
            // Keep only the directional pressure in excess of one maximal
            // result. An isolated pot cannot change Shark's wager strategy,
            // even when it updates both the size and big-wager lines.
            own.signum() * (own.abs() - crate::bot_adapt::SINGLE_LINE_UPDATE).max(0.0)
        }
    } else {
        crate::bot_adapt::pressure(Game::Holdem, beans, line, user)
    }
}

/// Pressure on a bot's opening bet into this human on the current street.
pub fn opening_pressure(
    read: impl Fn(u8, Option<Uuid>) -> f64,
    street: Street,
    user: Option<Uuid>,
) -> f64 {
    let s = open_line(street).map_or(0.0, |line| read(line, user));
    if !s.is_finite() || s.abs() < DEAD_BAND {
        0.0
    } else {
        s
    }
}

/// Record one token pot of a squid round of `seats` players for a roster
/// human: `won` = the human was its sole main-pot winner.
pub fn observe_pot(beans: bool, seats: usize, user: Uuid, won: bool) {
    crate::bot_adapt::observe_share(Game::Holdem, beans, POT_SHARE, user, seats, won);
}

/// A share read below this is a fair share within its noise (about one
/// spread of the read at a fair share): no contest.
const CONTEST_FROM: f64 = 0.20;
/// The share read at which the contest is full: half a fair share above one.
const CONTEST_FULL: f64 = 0.50;

/// How far the humans still in the pot have been collecting more than one
/// seat's share of the token pots:
/// the highest read among them, mapped to `[0, 1]` between [`CONTEST_FROM`]
/// and [`CONTEST_FULL`]. `0.0` with no human in the pot, so pots among bots
/// keep the base pricing, and for a human at or below a fair share.
pub fn pot_contest(
    read: impl Fn(u8, Option<Uuid>) -> f64,
    humans: impl IntoIterator<Item = Option<Uuid>>,
) -> f64 {
    let s = humans
        .into_iter()
        .map(|user| read(POT_SHARE, user))
        .fold(0.0, f64::max);
    ((s - CONTEST_FROM) / (CONTEST_FULL - CONTEST_FROM)).clamp(0.0, 1.0)
}

/// Pressure of the line a bot faces on this street: the street's last wager
/// increase, when a human made it. `human` returns `(user, all_in)` for a
/// human player and `None` for a bot. `own_call` is what the bot itself can put in to call
/// (its stack caps it). River shove reads use that effective price. Other
/// wagers get the big-line read when the call reaches the big size.
pub fn facing_pressure(
    pressure: impl Fn(u8, Option<Uuid>) -> f64,
    street: Street,
    facts: &[StreetActionFact],
    big_blind: u32,
    own_call: u32,
    four_bet_pot: bool,
    human: impl Fn(PlayerId) -> Option<(Option<Uuid>, bool)>,
) -> f64 {
    let Some(index) = facts.iter().rposition(|f| f.increased_bet) else {
        return 0.0;
    };
    let Some((user, all_in)) = human(facts[index].player_id) else {
        return 0.0;
    };
    let read = |line| pressure(line, user);
    let s = if street == Street::River && all_in {
        read(river_shove_line(&facts[index], own_call, four_bet_pot))
    } else {
        let s = line(street, facts, index, big_blind, all_in, four_bet_pot).map_or(0.0, read);
        // A big wager's own read leads; the pot-fraction read fills in the rest.
        let own_big =
            f64::from(own_call) >= super::commitment::START_BB * f64::from(big_blind.max(1));
        big_line(street, &facts[index], big_blind)
            .filter(|_| own_big)
            .map_or(s, |line| {
                let big = read(postflop_line(line, four_bet_pot));
                big + s * (1.0 - big.abs())
            })
    };
    // Decayed or barely-sampled reads play the base strategy exactly.
    if s.abs() < DEAD_BAND {
        0.0
    } else {
        s
    }
}

/// Pressure below this magnitude is treated as none.
const DEAD_BAND: f64 = 0.05;

/// One dealt human's settled chips.
pub struct HumanResult {
    pub player: PlayerId,
    pub user: Uuid,
    pub start: u32,
    pub end: i64,
    pub awarded: u32,
}

/// One dealt bot's settled chips.
pub struct BotResult {
    pub player: PlayerId,
    pub start: u32,
    pub end: i64,
    pub awarded: u32,
}

/// Record each human's lines of a settled hand: per street, the human's last
/// wager increase that at least one bot then answered; and each bot's opening
/// bets that a human answered. Public actions and chip results only.
pub fn observe_hand(
    beans: bool,
    big_blind: u32,
    streets: &[(Street, Vec<StreetActionFact>)],
    dealt: &[PlayerId],
    is_bot: impl Fn(PlayerId) -> bool,
    humans: impl IntoIterator<Item = HumanResult>,
    bots: impl IntoIterator<Item = BotResult>,
) {
    let humans: Vec<HumanResult> = humans.into_iter().collect();
    observe_openings(beans, big_blind, streets, dealt, &humans, bots);
    let bb = f64::from(big_blind.max(1));
    let four_bet_pot = streets
        .iter()
        .find(|(street, _)| *street == Street::Preflop)
        .is_some_and(|(_, facts)| self::four_bet_pot(facts));
    let folded: HashSet<PlayerId> = streets
        .iter()
        .flat_map(|(_, facts)| facts.iter())
        .filter(|f| f.action == PlayerAction::Fold)
        .map(|f| f.player_id)
        .collect();
    for h in humans {
        let risked = (i64::from(h.start) + i64::from(h.awarded) - h.end).max(0) as u64;
        if risked == 0 {
            continue;
        }
        let net = h.end as f64 - f64::from(h.start);
        let took_pot = h.awarded > 0 && dealt.iter().all(|p| *p == h.player || folded.contains(p));
        // An all-in is the human's last action of the hand.
        let last = streets
            .iter()
            .enumerate()
            .rev()
            .find_map(|(s, (_, facts))| {
                facts
                    .iter()
                    .rposition(|f| f.player_id == h.player)
                    .map(|i| (s, i))
            });
        for (s, (street, facts)) in streets.iter().enumerate() {
            let Some(index) = facts
                .iter()
                .rposition(|f| f.player_id == h.player && f.increased_bet)
            else {
                continue;
            };
            // Use the action record on the river: an uncalled shove may
            // have been refunded by the time the net result is observed.
            let all_in = last == Some((s, index))
                && if *street == Street::River {
                    facts[index].stack_after == engine::Chips::ZERO
                } else {
                    risked >= u64::from(h.start)
                };
            let Some(line) = line(*street, facts, index, big_blind, all_in, four_bet_pot) else {
                continue;
            };
            let responses = facts[index + 1..]
                .iter()
                .filter(|f| f.player_id != h.player);
            let (mut bot_faced, mut bot_continued, mut human_continued) = (false, false, false);
            for f in responses {
                let continued = f.action != PlayerAction::Fold;
                if is_bot(f.player_id) {
                    bot_faced = true;
                    bot_continued |= continued;
                } else {
                    human_continued |= continued;
                }
            }
            let outcome = if !bot_faced {
                continue;
            } else if bot_continued {
                if net <= 0.0 {
                    Outcome::ContestedLost
                } else if took_pot || dealt.iter().all(|p| !is_bot(*p) || folded.contains(p)) {
                    // The bots continued here and folded to a later line:
                    // that later line carries the evidence, not this one
                    // (also when only another human paid the human off).
                    continue;
                } else {
                    Outcome::ContestedWon
                }
            } else if !human_continued && took_pot {
                Outcome::Uncontested
            } else {
                continue;
            };
            let mut lines = [false; 4];
            let first = river_shove_line(&facts[index], 0, four_bet_pot);
            if *street == Street::River && all_in && !human_continued {
                for response in facts[index + 1..]
                    .iter()
                    .take_while(|f| !f.increased_bet)
                    .filter(|f| is_bot(f.player_id) && f.to_call_before.0 > 0)
                {
                    let call = response.to_call_before.0.min(response.stack_before.0);
                    let matched = river_shove_line(&facts[index], call, four_bet_pot);
                    lines[usize::from(matched - first)] = true;
                }
            }
            let matched_lines = lines
                .into_iter()
                .enumerate()
                .filter_map(|(size, matched)| matched.then_some(first + size as u8));
            for line in [
                Some(line),
                big_line(*street, &facts[index], big_blind)
                    .map(|line| postflop_line(line, four_bet_pot)),
            ]
            .into_iter()
            .flatten()
            .chain(matched_lines)
            {
                crate::bot_adapt::observe(Observation {
                    game: Game::Holdem,
                    beans,
                    line,
                    user: h.user,
                    risked: risked as f64 / bb,
                    net: net / bb,
                    outcome,
                });
            }
        }
    }
}

/// Each bot's postflop opening bets (the street's first wager, nothing to
/// call) that at least one human answered. Every responder folding and the
/// bot taking the pot counts for each human who folded; a human continuing
/// and winning the hand while the bot lost it counts against that human.
fn observe_openings(
    beans: bool,
    big_blind: u32,
    streets: &[(Street, Vec<StreetActionFact>)],
    dealt: &[PlayerId],
    humans: &[HumanResult],
    bots: impl IntoIterator<Item = BotResult>,
) {
    let bb = f64::from(big_blind.max(1));
    let human = |p: PlayerId| humans.iter().find(|h| h.player == p);
    let folded: HashSet<PlayerId> = streets
        .iter()
        .flat_map(|(_, facts)| facts.iter())
        .filter(|f| f.action == PlayerAction::Fold)
        .map(|f| f.player_id)
        .collect();
    for b in bots {
        let risked = (i64::from(b.start) + i64::from(b.awarded) - b.end).max(0) as u64;
        if risked == 0 {
            continue;
        }
        let net = b.end as f64 - f64::from(b.start);
        let took_pot = b.awarded > 0 && dealt.iter().all(|p| *p == b.player || folded.contains(p));
        for (street, facts) in streets {
            let Some(line) = open_line(*street) else {
                continue;
            };
            let Some(index) = facts.iter().position(|f| f.increased_bet) else {
                continue;
            };
            if facts[index].player_id != b.player || facts[index].to_call_before.0 != 0 {
                continue;
            }
            let responses: Vec<&StreetActionFact> = facts[index + 1..]
                .iter()
                .filter(|f| f.player_id != b.player)
                .collect();
            let all_folded = responses.iter().all(|f| f.action == PlayerAction::Fold);
            let outcome = if all_folded && took_pot {
                Outcome::Uncontested
            } else if net < 0.0 {
                Outcome::OpenLost
            } else {
                continue;
            };
            let mut seen = HashSet::new();
            for f in &responses {
                let continued = f.action != PlayerAction::Fold;
                if continued != matches!(outcome, Outcome::OpenLost) || !seen.insert(f.player_id) {
                    continue;
                }
                let Some(h) = human(f.player_id) else {
                    continue;
                };
                // Only a human who won the hand beat the bet: one who folded
                // later or lost it too (to another seat) did not.
                if continued && (folded.contains(&h.player) || h.end <= i64::from(h.start)) {
                    continue;
                }
                let user = h.user;
                crate::bot_adapt::observe(Observation {
                    game: Game::Holdem,
                    beans,
                    line,
                    user,
                    risked: risked as f64 / bb,
                    net: net / bb,
                    outcome,
                });
            }
        }
    }
}

/// Widest range (share of combos) a fully positive pressure assumes for the
/// line's aggressor; never narrows a range already wider than this.
const WIDE_RANGE: f64 = 0.60;
/// Share of the assumed range a fully negative pressure removes.
const NARROW_SHARE: f64 = 0.50;
/// Relative swing of a postflop defence share (or fold frequency) at |s| = 1.
const DEFENCE_SWING: f64 = 0.35;
/// Most the preflop price floor's required-equity cap rises at s = 1.
const FLOOR_CAP_SWING: f64 = 0.10;
/// Shift of a modelled river equity at |s| = 1.
const EQUITY_SWING: f64 = 0.06;

/// Aggressor range width under pressure `s`.
pub(super) fn range_width(width: f64, s: f64) -> f64 {
    if s > 0.0 {
        width + s * (WIDE_RANGE - width).max(0.0)
    } else {
        width * (1.0 + NARROW_SHARE * s)
    }
}

/// Defence share under pressure `s` (the caller clamps it as before).
pub(super) fn defence_share(defend: f64, s: f64) -> f64 {
    defend * (1.0 + DEFENCE_SWING * s)
}

/// A fold frequency in percent under pressure `s`.
pub(super) fn fold_pct(pct: u8, s: f64) -> u8 {
    (f64::from(pct) * (1.0 - DEFENCE_SWING * s))
        .round()
        .clamp(0.0, 100.0) as u8
}

/// The preflop price floor's required-equity cap under pressure `s`.
pub(super) fn floor_cap(cap: f64, s: f64) -> f64 {
    cap + FLOOR_CAP_SWING * s.max(0.0)
}

/// The Shark's river minimum-defence floor under pressure `s`. The floor only
/// insures against bluffs the range model cannot see, so a line the bots kept
/// calling and losing to (s < 0) removes it in proportion, to nothing at
/// s = −1. A line that keeps
/// taking pots uncalled widens it like any defence share.
pub(super) fn river_floor(share: f64, s: f64) -> f64 {
    let floor = if s < 0.0 {
        share * (1.0 + s)
    } else {
        defence_share(share, s)
    };
    floor.clamp(0.0, 1.0)
}

/// Domain separation for the restraint fork of the bot RNG.
const RESTRAIN_FORK: u64 = 0x4f50_454e_5245_5354;

/// Check instead of opening a postflop bet without top pair or better, with
/// probability `-s` under a negative opening pressure: the humans left in the
/// pot have been answering such bets and winning. Bets with a made hand, and
/// every decision at `s >= 0`, are unchanged. The coin comes from a fork of
/// the bot RNG, so the action RNG is never advanced.
pub(super) fn restrain_open(ctx: &DecisionContext<'_>, action: PlayerAction) -> PlayerAction {
    let s = ctx.open_pressure;
    if !s.is_finite()
        || s >= 0.0
        || ctx.street == Street::Preflop
        || ctx.to_call.0 != 0
        || ctx.current_bet.0 != 0
        || !matches!(action, PlayerAction::Raise { .. } | PlayerAction::AllIn)
        || engine::solver::classify(ctx.hole, ctx.board)
            >= engine::solver::HandStrength::PairTopWeakKicker
    {
        return action;
    }
    let mut fork = StdRng::seed_from_u64(
        ctx.rng.clone().next_u64() ^ RESTRAIN_FORK ^ (ctx.street as u64) << 56,
    );
    if fork.gen::<f64>() < -s {
        PlayerAction::Check
    } else {
        action
    }
}

/// An opponent's modelled fold share to a bot opening bet under the opening
/// pressure `s`: toward none as answered bets keep losing, toward
/// [`OPEN_FOLD_CAP`] as they keep taking pots.
pub(crate) fn open_fold(fold: f64, s: f64) -> f64 {
    if s < 0.0 {
        fold * (1.0 + s)
    } else {
        fold + s * (OPEN_FOLD_CAP - fold).max(0.0)
    }
}

/// Highest fold share a fully positive opening pressure assumes.
const OPEN_FOLD_CAP: f64 = 0.75;

/// A modelled equity under pressure `s`.
pub(super) fn equity(equity: f64, s: f64) -> f64 {
    equity + EQUITY_SWING * s
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::bot::BotPersona;

    #[test]
    fn repeated_river_shoves_adapt_only_the_players_matched_size() {
        let user = Uuid::new_v4();
        let stranger = Uuid::new_v4();
        let make = |amount| StreetActionFact {
            player_id: PlayerId(1),
            action: PlayerAction::AllIn,
            amount: engine::Chips(amount),
            committed_to: engine::Chips(amount),
            stack_before: engine::Chips(amount),
            stack_after: engine::Chips::ZERO,
            to_call_before: engine::Chips::ZERO,
            pot_before: engine::Chips(100),
            increased_bet: true,
            full_raise: true,
        };
        let large = make(250);
        let small = make(80);
        let pressure = |fact, call, person, four_bet| {
            facing_pressure(
                |line, user| player_pressure(BotPersona::Shark, true, line, user),
                Street::River,
                &[fact],
                10,
                call,
                four_bet,
                |_| Some((Some(person), true)),
            )
        };
        let settle = |person, call| {
            let call_fact = StreetActionFact {
                player_id: PlayerId(2),
                action: PlayerAction::Call,
                amount: engine::Chips(call),
                committed_to: engine::Chips(call),
                stack_before: engine::Chips(call),
                stack_after: engine::Chips::ZERO,
                to_call_before: large.amount,
                pot_before: engine::Chips(350),
                increased_bet: false,
                full_raise: false,
            };
            observe_hand(
                true,
                10,
                &[(Street::River, vec![large.clone(), call_fact])],
                &[PlayerId(1), PlayerId(2)],
                |id| id == PlayerId(2),
                [HumanResult {
                    player: PlayerId(1),
                    user: person,
                    start: 300,
                    end: i64::from(350 + call),
                    awarded: 100 + 2 * call,
                }],
                [BotResult {
                    player: PlayerId(2),
                    start: 50 + call,
                    end: 0,
                    awarded: 0,
                }],
            );
        };
        settle(user, 250);
        assert_eq!(pressure(large.clone(), 250, user, false), 0.0);
        for _ in 0..12 {
            settle(user, 250);
        }
        assert!(pressure(large.clone(), 250, user, false) < 0.0);
        assert_eq!(pressure(large.clone(), 80, user, false), 0.0);
        assert_eq!(pressure(large.clone(), 250, user, true), 0.0);
        assert_eq!(pressure(large.clone(), 250, stranger, false), 0.0);
        let short_stack_opponent = Uuid::new_v4();
        for _ in 0..13 {
            settle(short_stack_opponent, 80);
        }
        assert_eq!(
            pressure(large.clone(), 250, short_stack_opponent, false),
            0.0
        );
        let capped = pressure(large, 80, short_stack_opponent, false);
        assert!(capped < 0.0);
        let near_pot = pressure(small, 80, short_stack_opponent, false);
        assert!((capped - near_pot).abs() < 1e-6);
    }

    #[test]
    fn shark_pressure_follows_only_repeated_results_for_this_player_and_line() {
        let known = Uuid::new_v4();
        let stranger = Uuid::new_v4();
        let beans = true;
        let street = Street::River;
        let line = open_line(street).unwrap();
        let won_open = Observation {
            game: Game::Holdem,
            beans,
            line,
            user: known,
            risked: 1.0,
            net: 2.0,
            outcome: Outcome::Uncontested,
        };
        let personal = |line, user| player_pressure(BotPersona::Shark, beans, line, user);
        let open = |player| opening_pressure(personal, street, player);

        crate::bot_adapt::observe(won_open);
        assert_eq!(open(Some(known)), 0.0);
        for _ in 0..8 {
            crate::bot_adapt::observe(won_open);
        }
        assert!(open(Some(known)) > 0.5);
        assert_eq!(open(Some(stranger)), 0.0);
        assert_eq!(open(None), 0.0);
        assert_eq!(opening_pressure(personal, Street::Turn, Some(known)), 0.0);
        assert_eq!(
            player_pressure(BotPersona::Shark, false, line, Some(known)),
            0.0
        );
        assert!(player_pressure(BotPersona::Tag, beans, line, Some(stranger)) > 0.05);

        for _ in 0..10 {
            crate::bot_adapt::observe(Observation {
                net: -2.0,
                outcome: Outcome::OpenLost,
                ..won_open
            });
        }
        let known_pressure = open(Some(known));
        assert!(known_pressure < -0.5);
        let mut responder = crate::bot::SquidRead::prior(0);
        responder.open_pressure = known_pressure;
        let unknown = crate::bot::SquidRead::prior(1);
        assert!(responder.fold_to_open(2, BotPersona::Shark, known_pressure) < 0.2);
        assert_eq!(
            unknown.fold_to_open(2, BotPersona::Shark, known_pressure),
            unknown.folds[2]
        );
        assert!(unknown.fold_to_open(2, BotPersona::Tag, known_pressure) < 0.2);
        assert_eq!(open(Some(stranger)), 0.0);
    }
}
