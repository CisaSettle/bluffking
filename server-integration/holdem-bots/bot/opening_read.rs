//! How often each opponent opens the betting on a postflop street, from its
//! own public actions, and how much of its range such a bet therefore keeps.
//!
//! The range models narrowed every opening bet to an authored share of the
//! bettor's range (the stronger part of it, scaled by the player's overall
//! aggression). A player who checks and calls most streets but bets again on
//! almost every street where it holds the initiative has a low overall
//! aggression, so each of its bets was read as the top half of its range and
//! three of them as near-certain strength; the bots folded one-pair hands to
//! half-pot barrels that could come from a wide range.
//!
//! A player who bets a spot `f` of the time holds at best the strongest `f` of
//! its range there, so its bet keeps at least `f` of the range ([`keep`]). The
//! spot is the street, whether the player made the last wager increase of the
//! previous street (the initiative) and whether more than one opponent was
//! left; each size class counts on its own. The read is shrunk toward the
//! authored share until it has a sample and never narrows it, so an unknown or
//! rare bettor keeps the authored model (whose half-pot class, like the online
//! lines', now includes a half pot rounded up by one chip). The widening then
//! follows the online read of the bettor's line ([`crate::bot_adapt`]) on every
//! one of its opening bets: a player whose bets keep winning when the bots call is back
//! at the authored model at a fully negative read, before the narrowing of the
//! last wager ([`super::adapt::range_width`]), so a player who keeps the
//! frequency but drops the bluffs loses the extra calls. A forced check (a
//! timed-out turn) counts as a chance not taken, which only lowers the read.

use super::PublicAction;
use engine::{PlayerAction, Street};
use serde::{Deserialize, Serialize};

/// One opening-bet spot of one player: how often it had the chance to open
/// the betting and how often it did, by [`size_class`].
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct OpeningBets {
    pub chances: u32,
    pub bets: [u32; 3],
}

/// `[street - 1][initiative][multiway]`.
pub type OpeningReads = [[[OpeningBets; 2]; 2]; 3];

/// The spot of a postflop opening chance.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Spot {
    /// 0 = flop, 1 = turn, 2 = river.
    pub street: usize,
    pub initiative: bool,
    pub multiway: bool,
}

/// One chance to open the betting, and its size class if the player bet.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Chance {
    pub seat: u8,
    pub spot: Spot,
    pub bet: Option<usize>,
    /// Index of the action in the history.
    pub index: usize,
}

/// Virtual chances at the authored share before a player's own read counts.
const PRIOR_CHANCES: f64 = 8.0;
/// Chances one spot keeps in the stored profile; in play a spot is halved once
/// it reaches twice as many, so a player who changes style moves the read.
pub const SPOT_CHANCES: u32 = 100;

fn street_index(street: Street) -> Option<usize> {
    match street {
        Street::Preflop => None,
        Street::Flop => Some(0),
        Street::Turn => Some(1),
        Street::River => Some(2),
    }
}

/// Size class of a wager increase of `raise` over `pot` (the pot after
/// calling): at most half pot (a half pot rounded up to the next chip
/// included), at most pot, larger or all-in. The range models' own classes.
pub fn size_class(action: &PlayerAction, raise: u32, pot: u32) -> usize {
    if matches!(action, PlayerAction::AllIn) || raise > pot {
        2
    } else if raise <= pot.div_ceil(2) {
        0
    } else {
        1
    }
}

/// Every postflop chance to open the betting in `history`: a player's first
/// action on a street that nobody has bet yet. `dealt` is the number of
/// players dealt into the hand; all-in players still count as opponents.
pub fn chances(history: &[PublicAction], dealt: usize) -> Vec<Chance> {
    let mut out = Vec::new();
    let mut folded: Vec<u8> = Vec::new();
    let mut previous_aggressor: Option<u8> = None;
    let mut street_aggressor: Option<u8> = None;
    let mut street = Street::Preflop;
    let mut acted: Vec<u8> = Vec::new();
    let mut opened = false;
    for (index, a) in history.iter().enumerate() {
        if a.street != street {
            previous_aggressor = street_aggressor;
            street_aggressor = None;
            street = a.street;
            acted.clear();
            opened = false;
        }
        if let Some(street) = street_index(a.street) {
            if !opened && !acted.contains(&a.seat) {
                let live = dealt.saturating_sub(folded.len());
                out.push(Chance {
                    seat: a.seat,
                    spot: Spot {
                        street,
                        initiative: previous_aggressor == Some(a.seat),
                        multiway: live > 2,
                    },
                    bet: a
                        .increased_bet
                        .then(|| size_class(&a.action, a.amount, a.pot_before)),
                    index,
                });
            }
        }
        acted.push(a.seat);
        if a.increased_bet {
            opened = true;
            street_aggressor = Some(a.seat);
        }
        if matches!(a.action, PlayerAction::Fold) && !folded.contains(&a.seat) {
            folded.push(a.seat);
        }
    }
    out
}

/// Record `seat`'s chances of one settled hand.
pub fn observe(reads: &mut OpeningReads, chances: &[Chance], seat: u8) {
    for c in chances.iter().filter(|c| c.seat == seat) {
        let cell =
            &mut reads[c.spot.street][usize::from(c.spot.initiative)][usize::from(c.spot.multiway)];
        cell.chances = cell.chances.saturating_add(1);
        if let Some(size) = c.bet {
            cell.bets[size] = cell.bets[size].saturating_add(1);
        }
        if cell.chances >= 2 * SPOT_CHANCES {
            cap(cell, SPOT_CHANCES);
        }
    }
}

/// Rescale a spot to at most `limit` chances, rounding to the nearest count.
fn cap(cell: &mut OpeningBets, limit: u32) {
    if cell.chances > limit {
        let chances = u64::from(cell.chances);
        for bet in &mut cell.bets {
            *bet = ((u64::from(*bet) * u64::from(limit) + chances / 2) / chances) as u32;
        }
        cell.chances = limit;
    }
}

/// Combined reads, `earlier` first.
pub fn merged(earlier: &OpeningReads, later: &OpeningReads) -> OpeningReads {
    std::array::from_fn(|s| {
        std::array::from_fn(|i| {
            std::array::from_fn(|m| {
                let (a, b) = (earlier[s][i][m], later[s][i][m]);
                OpeningBets {
                    chances: a.chances.saturating_add(b.chances),
                    bets: std::array::from_fn(|k| a.bets[k].saturating_add(b.bets[k])),
                }
            })
        })
    })
}

/// The stored form: each spot rescaled to at most [`SPOT_CHANCES`].
pub fn capped(reads: &OpeningReads) -> OpeningReads {
    let mut out = *reads;
    for cell in out.iter_mut().flatten().flatten() {
        cap(cell, SPOT_CHANCES);
    }
    out
}

/// Share of the range a bet of size class `size` in `spot` keeps for this
/// player: its share of chances it bet that size, shrunk toward the authored
/// share `prior`, and never below `prior`. Keeping the strongest `f` of a range
/// is the strongest range a player who bets `f` of the time can hold, so a
/// frequent bettor cannot be as strong as the authored share says; a rare bet,
/// though, can still be polar (value and air), so a low frequency is no
/// evidence of a stronger range and leaves the authored share alone. Only the
/// exact size counts: a player who bets big with anything and small for value
/// has a narrow small-bet share. The widening then follows the online read
/// `s` of the bettor's line ([`crate::bot_adapt`]): none of it is left at
/// `s = -1`, where the bots kept calling this line and losing.
pub fn keep(reads: &OpeningReads, spot: Spot, size: usize, prior: f64, s: f64) -> f64 {
    let cell = reads[spot.street][usize::from(spot.initiative)][usize::from(spot.multiway)];
    let chances = f64::from(cell.chances);
    let bets = f64::from(cell.bets[size.min(2)]).min(chances);
    let read = ((bets + prior * PRIOR_CHANCES) / (chances + PRIOR_CHANCES)).max(prior);
    let back_off = if s.is_finite() {
        (1.0 + s.min(0.0)).clamp(0.0, 1.0)
    } else {
        1.0
    };
    prior + (read - prior) * back_off
}
