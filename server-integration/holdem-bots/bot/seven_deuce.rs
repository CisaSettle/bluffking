//! The room's 7-2 bounty as side value of winning the pot.
//!
//! At a bounty table the sole winner of the main pot holding 7-2
//! offsuit after the flop collects the bounty from every other dealt player,
//! paid in full with automatic shortfall funding; a post-flop fold-around pays too.
//! The persona strategies price only the poker pot, so a bot folded 7-2
//! offsuit preflop and checked it down as air, never collecting a bounty that
//! is worth several pots at a full table.
//!
//! The bounty is the same kind of value as a squid token: chips gained only
//! by winning this pot. The session supplies its size for the bot's own hole
//! cards ([`bounty_value`]), and the layers that already price the token —
//! the fold defence and the postflop stab ([`squid_defense`]) and big-pot
//! discipline ([`commitment`]) — price the sum ([`win_bonus`]). So a 7-2
//! offsuit bot:
//!
//! * preflop calls an open or defends a blind when the bounty it expects to
//!   win pays for the price with the estimation margin, against the ranges
//!   the callers' public lines keep and discounted for every player still to
//!   act (a raise behind folds the hand), and folds to a price the bounty
//!   does not cover. First in it keeps its persona's raise-or-fold contract
//!   ([`open`]): only the Station limps by design, so a limp would show the
//!   7-2 to anyone watching;
//! * postflop bets into one or two opponents when their folds — a fold-around
//!   collects the bounty — make betting worth more than checking, and calls a
//!   wager with any hand whose showdown chance makes the bounty pay for it
//!   (on the turn and river the side value counts for at most two pots);
//! * in a pot of at least twenty big blinds, big-pot discipline counts the
//!   side value for at most one pot; the fold defence still has the last
//!   word on a fold, pricing it with the whole bounty preflop and on the flop
//!   (at most two pots later), so a large call is made only when the bounty
//!   it can win pays for it.
//!
//! Preflop wins and split main pots never pay, and no layer bets preflop for
//! the bounty. The shared pricing credits side value to outright wins only. Opponents' bounty threat is
//! not modelled: their cards are unknown. With the rule off, or any other
//! holding, the value is zero and every decision is unchanged.

use super::*;

/// Chips the 7-2 bounty would pay the hero for winning this pot: `bounty`
/// from every other dealt seat, including short/all-in payers. A small
/// overestimate: players the
/// settlement exempts (left the table during the hand) are still counted.
/// Zero with the rule off or any holding other than 7-2 offsuit.
pub fn bounty_value(bounty: Chips, hole: HoleCards, hero: u8, seats: &[SeatPublic]) -> f64 {
    if bounty.0 == 0 || !is_seven_deuce_offsuit(hole) {
        return 0.0;
    }
    seats
        .iter()
        .filter(|s| s.seat != hero)
        .map(|_| f64::from(bounty.0))
        .sum()
}

/// The engine's bounty hand: a seven and a deuce of different suits.
fn is_seven_deuce_offsuit(hole: HoleCards) -> bool {
    let ranks = (hole.card1.rank, hole.card2.rank);
    let seven_deuce = matches!(ranks, (Rank::Seven, Rank::Two) | (Rank::Two, Rank::Seven));
    seven_deuce && hole.card1.suit != hole.card2.suit
}

/// Side value of winning this pot instead of any live opponent: the squid
/// token plus the 7-2 bounty. Without a bounty this is the squid bonus itself,
/// bit for bit.
pub(super) fn win_bonus(ctx: &DecisionContext<'_>) -> f64 {
    let bounty = ctx.seven_deuce_bounty;
    if bounty.is_finite() && bounty > 0.0 {
        let squid = ctx.squid_defense.bonus;
        if squid.is_finite() && squid > 0.0 {
            squid + bounty
        } else {
            bounty
        }
    } else {
        ctx.squid_defense.bonus
    }
}

/// First in preflop, a non-Station persona never limps: the charts raise or
/// fold, so a limp by anyone else would mark the 7-2 offsuit for the whole
/// table. When the side value turned the persona's fold (`before`) into a
/// limp (`after`), the bot opens to its persona's usual first-in size instead
/// if that larger price pays against the same field, and folds otherwise.
/// The open is priced as a call of the chips it commits, crediting none of
/// the blinds it takes uncontested (and, in turn, not modelling that the
/// blinds fold or continue stronger once it raises). Any other spot returns
/// `after` unchanged.
pub(super) fn open(
    ctx: &mut DecisionContext<'_>,
    before: PlayerAction,
    after: PlayerAction,
) -> PlayerAction {
    let bounty = ctx.seven_deuce_bounty;
    if !bounty.is_finite()
        || bounty <= 0.0
        || ctx.street != Street::Preflop
        || ctx.persona == BotPersona::Station
        || before != PlayerAction::Fold
        || after != PlayerAction::Call
        || ctx.last_aggressor_seat.is_some()
        || preflop::preflop_raise_depth(ctx) != 0
    {
        return after;
    }
    if !ctx.can_raise {
        return before;
    }
    // Price the chips the legal open actually commits: the minimum raise, the
    // half-pot floor (straddles, antes, limpers) or an all-in can lift it
    // above the chart size.
    let raise = emit_raise(ctx, preflop::rfi_open_chips(ctx, ctx.big_blind));
    let chips = match raise {
        PlayerAction::Raise { amount } => amount.0.saturating_sub(ctx.committed_this_street.0),
        PlayerAction::AllIn => ctx.stack_chips,
        _ => return before,
    }
    .min(ctx.stack_chips);
    if squid_defense::call_pays(ctx, chips) {
        raise
    } else {
        before
    }
}

#[cfg(test)]
mod full_bounty_tests {
    use super::*;
    #[test]
    fn full_bounty_value_includes_short_and_all_in_payers() {
        use engine::card::{Card, Rank, Suit};
        let hole = HoleCards::new(
            Card::new(Rank::Seven, Suit::Spades),
            Card::new(Rank::Two, Suit::Hearts),
        );
        let seats = [
            SeatPublic {
                seat: 0,
                stack: 100,
                ..Default::default()
            },
            SeatPublic {
                seat: 1,
                stack: 7,
                folded: true,
                ..Default::default()
            },
            SeatPublic {
                seat: 2,
                stack: 0,
                all_in: true,
                ..Default::default()
            },
        ];
        assert_eq!(bounty_value(Chips(20), hole, 0, &seats), 40.0);
    }
}
