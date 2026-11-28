//! Range-independent chip-EV decisions when a call closes betting for the hero.
//! The caller admits ordinary single-runout cash rules. With only a call or a
//! fold left, the sign of call EV for *every* opposing holding is sufficient:
//! no distribution over those holdings can reverse a unanimous decision.

use super::all_in::{card_bit, mask};
use super::{
    BoardCards, DecisionContext, HandRank, HoleCards, PlayerAction, Rank, SeatPublic, Street,
};
use engine::eval::{board_eval, rank_with_board, BoardEval};
use engine::{Card, Suit};
use std::cmp::Ordering;

pub(super) fn decision(ctx: &DecisionContext<'_>) -> Option<PlayerAction> {
    if !ctx.table.is_known()
        || ctx.table.seats.len() > 9
        || ctx.to_call.0 == 0
        || !matches!(ctx.street, Street::Flop | Street::Turn | Street::River)
    {
        return None;
    }
    let hero = ctx.table.seat_entry(ctx.seat)?;
    let mut opponents = ctx
        .table
        .seats
        .iter()
        .filter(|s| s.seat != ctx.seat && !s.folded);
    if hero.folded
        || hero.all_in
        || hero.stack == 0
        || hero.stack != ctx.stack_chips
        || hero.committed_this_street != ctx.committed_this_street.0
        || ctx.current_bet.0.checked_sub(hero.committed_this_street)? != ctx.to_call.0
        || !opponents
            .clone()
            .any(|s| s.committed_this_street == ctx.current_bet.0)
        // With chips behind, a matched active opponent has no further action
        // when our river call closes the round. A pending live raise would
        // change the price; only certify a decision final for this player.
        || (hero.stack > ctx.to_call.0
            && opponents.clone().any(|s| !s.all_in)
            && (ctx.street != Street::River || ctx.can_raise
                || opponents.clone().any(|s| !s.all_in && s.committed_this_street < ctx.current_bet.0)))
    {
        return None;
    }
    let count = match (ctx.street, ctx.board.flop, ctx.board.turn, ctx.board.river) {
        (Street::Flop, Some(_), None, None) => 3,
        (Street::Turn, Some(_), Some(_), None) => 4,
        (Street::River, Some(_), Some(_), Some(_)) => 5,
        _ => return None,
    };
    let known = ctx
        .board
        .all_cards()
        .iter()
        .fold(mask(ctx.hole), |m, &c| m | card_bit(c));
    if known.count_ones() != count + 2 {
        return None;
    }
    // A made royal can never lose on any runout, in any side pot.
    let made = engine::rank_hand(&ctx.hole, ctx.board);
    if made == HandRank::StraightFlush(9) {
        return Some(PlayerAction::Call);
    }
    let deck = unseen_cards(known);
    if ctx.street == Street::River {
        // A tied winner receives at least their contribution in each matched
        // layer: there can be no more tied winners than contributors. Refunds
        // of unmatched chips are independent of hand rank. This protects the
        // call even with short stacks, dead money, odd chips and more callers.
        let board = board_eval(ctx.board);
        let hero_rank = rank_with_board(&board, &ctx.hole);
        // Each counted opponent will reach showdown and covers every layer
        // hero can enter. If there are too few distinct cards to give them
        // all a tying or worse hand, one necessarily beats us in every pot.
        let cap = hero
            .committed_this_street
            .checked_add(ctx.to_call.0.min(hero.stack))?;
        let closes = opponents
            .clone()
            .all(|s| s.all_in || s.committed_this_street == ctx.current_bet.0);
        let covering = opponents
            .clone()
            .filter(|s| (s.all_in || closes) && s.committed_this_street >= cap)
            .count() as u32;
        let mut seen_beater = false;
        let mut possible_cards = 0u64;
        let mut disjoint = 0u64;
        for (i, &first) in deck.iter().enumerate() {
            for &second in &deck[i + 1..] {
                let cards = HoleCards::new(first, second);
                if rank_with_board(&board, &cards) > hero_rank {
                    seen_beater = true;
                } else {
                    let pair = mask(cards);
                    possible_cards |= pair;
                    if disjoint & pair == 0 {
                        disjoint |= pair;
                    }
                }
                // A witness to both a beater and enough different hands
                // that can lose or tie rules out either certificate here.
                if seen_beater && disjoint.count_ones() / 2 >= covering {
                    return None;
                }
            }
        }
        return if !seen_beater {
            Some(PlayerAction::Call)
        } else if possible_cards.count_ones() / 2 < covering {
            Some(PlayerAction::Fold)
        } else {
            None
        };
    }
    let call = u64::from(ctx.to_call.0.min(hero.stack));
    // Every seat that could put chips in on this street entered it at the
    // same matched wager as these two live players. All earlier chips remain
    // contestable. Cap each current-street contribution at hero's call-to
    // amount; a folded player's excess can belong to a separate side pot.
    let cap = u64::from(hero.committed_this_street) + call;
    let final_pot = called_pot(ctx.table.seats, ctx.pot.0, cap, call)?;
    if count == 3 && ctx.table.seats.len() > 2 {
        return multiway_flop_call(
            ctx,
            &deck,
            made,
            call,
            final_pot,
            opponents.clone().count() as u64 + 1,
        );
    }
    let live = opponents.clone().count() as u64 + 1;
    if live > 2 {
        return multiway_turn_call(ctx, &deck, call, final_pot, live);
    }
    let odd_chip =
        ctx.table.ring_offset(ctx.seat)? < ctx.table.ring_offset(opponents.next()?.seat)?;
    let mucked = 2 * (ctx.table.seats.len() as u64 - 2);
    unanimous(
        ctx.board, ctx.hole, &deck, call, final_pot, odd_chip, mucked,
    )
}

/// A flopped full house or better cannot fall below its current rank. Bound an
/// opponent's best possible straight flush, quads and full house from the five
/// public cards and hero blockers. Price the worst subset of turn/river pairs
/// compatible with the number of cards held by the other dealt seats.
fn multiway_flop_call(
    ctx: &DecisionContext<'_>,
    deck: &[Card],
    rank: HandRank,
    call: u64,
    pot: u64,
    live: u64,
) -> Option<PlayerAction> {
    if !matches!(
        rank,
        HandRank::FullHouse(_) | HandRank::FourOfAKind(_) | HandRank::StraightFlush(_)
    ) {
        return None;
    }
    let flop = ctx.board.flop?;
    let mut held = [0; 13];
    let mut held_suits = [0u16; 4];
    for c in ctx.hole.as_array() {
        held[c.rank as usize] += 1;
        held_suits[c.suit as usize] |= 1 << c.rank as u8;
    }
    let mut made = held;
    for c in flop {
        made[c.rank as usize] += 1;
    }
    let trips = made.iter().rposition(|&n| n >= 3);
    let pair = (0..13).rev().find(|&r| Some(r) != trips && made[r] >= 2);
    let available = deck.len() as u64 - 2 * (ctx.table.seats.len() as u64 - 1);
    let outcomes = available * (available - 1) / 2;
    let tie = multiway_tie_floor(ctx.table.seats.len(), pot, call, live);
    let price = call * outcomes;
    let mut possible_losses: u64 = 0;
    let mut possible_ties: u64 = 0;
    for (i, &turn) in deck.iter().enumerate() {
        for &river in &deck[i + 1..] {
            let mut board = [0u8; 13];
            let mut suits = [0u16; 4];
            for card in [flop[0], flop[1], flop[2], turn, river] {
                board[card.rank as usize] += 1;
                suits[card.suit as usize] |= 1 << card.rank as u8;
            }
            let best_sf = suits
                .iter()
                .enumerate()
                .find(|(_, mask)| mask.count_ones() >= 3)
                .and_then(|(s, mask)| {
                    (0..=9).rev().find(|&high| {
                        let straight = if high == 0 {
                            0x100f
                        } else {
                            0x1f << (high - 1)
                        };
                        let missing = straight & !mask;
                        missing.count_ones() <= 2 && missing & held_suits[s] == 0
                    })
                });
            let worst = if let Some(high) = best_sf {
                HandRank::StraightFlush(high).cmp(&rank)
            } else if matches!(rank, HandRank::StraightFlush(_)) {
                Ordering::Less
            } else if (0..13).any(|r| {
                board[r] >= 2
                    && held[r] == 0
                    && (!matches!(rank, HandRank::FourOfAKind(_)) || Some(r) >= trips)
            }) {
                // A matching public quad with a better kicker could still win.
                Ordering::Greater
            } else if matches!(rank, HandRank::FullHouse(_)) {
                (0..13)
                    .rev()
                    .filter(|&t| board[t] > 0 && held[t] < 2 && Some(t) >= trips)
                    .find_map(|t| {
                        let need = 3u8.saturating_sub(board[t]);
                        (0..13)
                            .rev()
                            .find(|&p| p != t && need + 2u8.saturating_sub(board[p]) <= 2)
                            .map(|p| (Some(t), Some(p)).cmp(&(trips, pair)))
                    })
                    .unwrap_or(Ordering::Less)
            } else {
                Ordering::Less
            };
            possible_losses += u64::from(worst == Ordering::Greater);
            possible_ties += u64::from(worst == Ordering::Equal);
            let can_return = outcomes.saturating_sub(possible_losses);
            let floor = can_return.saturating_sub(possible_ties) * pot
                + can_return.min(possible_ties) * tie;
            // Once too many bad pairs exist our certificate cannot succeed,
            // even crediting every as-yet unseen pair as a sole win.
            if floor < price {
                return None;
            }
        }
    }
    Some(PlayerAction::Call)
}

fn multiway_tie_floor(seats: usize, pot: u64, call: u64, live: u64) -> u64 {
    // One layer per seat and a possible shared ante. Integer shares across
    // multiple layers can lose at most one chip per additional layer.
    (pot / live).saturating_sub(seats as u64).max(call)
}

/// A guarantee over *joint* opponent and folded hands. For each candidate
/// river ignore all wins except board ranks no other pocket can beat. Remove
/// the most valuable rivers that could already be in any other player's hand.
fn multiway_turn_call(
    ctx: &DecisionContext<'_>,
    deck: &[Card],
    call: u64,
    pot: u64,
    live: u64,
) -> Option<PlayerAction> {
    let mut public = ctx.board.clone();
    let hidden = 2 * (ctx.table.seats.len() as u64 - 1);
    let possible = deck.len() as u64 - hidden;
    let price = call * possible;
    let mut wins: u64 = 0;
    let mut ties: u64 = 0;
    let tie = multiway_tie_floor(ctx.table.seats.len(), pot, call, live);
    for (i, &river) in deck.iter().enumerate() {
        public.river = Some(river);
        let board = board_eval(&public);
        let hero = rank_with_board(&board, &ctx.hole);
        let mut award = pot;
        'pockets: for (j, &first) in deck.iter().enumerate() {
            if first == river {
                continue;
            }
            for &second in &deck[j + 1..] {
                if second == river {
                    continue;
                }
                match rank_with_board(&board, &HoleCards::new(first, second)).cmp(&hero) {
                    Ordering::Greater => {
                        award = 0;
                        break 'pockets;
                    }
                    Ordering::Equal => award = tie,
                    Ordering::Less => (),
                }
            }
        }
        wins += u64::from(award == pot);
        ties += u64::from(award != pot && award > 0);
        let floor = |wins: u64| {
            wins.saturating_sub(hidden) * pot
                + ties.saturating_sub(hidden.saturating_sub(wins)) * tie
        };
        if floor(wins) >= price {
            return Some(PlayerAction::Call);
        }
        let unchecked = (deck.len() - i - 1) as u64;
        if floor(wins + unchecked) < price {
            return None;
        }
    }
    None
}

pub(super) fn unseen_cards(known: u64) -> Vec<Card> {
    Rank::ALL
        .into_iter()
        .flat_map(|r| Suit::ALL.into_iter().map(move |s| Card::new(r, s)))
        .filter(|&c| known & card_bit(c) == 0)
        .collect()
}

fn called_pot(seats: &[SeatPublic], pot: u32, cap: u64, call: u64) -> Option<u64> {
    let street_paid: u64 = seats
        .iter()
        .map(|s| u64::from(s.committed_this_street))
        .sum();
    let prior = u64::from(pot).checked_sub(street_paid)?;
    let contested: u64 = seats
        .iter()
        .map(|s| u64::from(s.committed_this_street).min(cap))
        .sum();
    Some(prior + contested + call)
}

struct Runout {
    dealt: u64,
    board: BoardEval,
    hero: HandRank,
}

fn unanimous(
    board: &BoardCards,
    hole: HoleCards,
    deck: &[Card],
    call: u64,
    pot: u64,
    odd_chip: bool,
    mucked: u64,
) -> Option<PlayerAction> {
    const BUDGET: u64 = 46 * 45 / 2 * 44;
    if board.turn.is_none() {
        let holdings = 47 * 46 / 2;
        let runouts = 45 * 44 / 2;
        // Even granting a full-pot win on each examined board, proving a
        // nonnegative call needs at least this many boards per opposing hand.
        // Proving a fold costs at least as many since the call is <= half pot.
        if (call * runouts).div_ceil(pot) * holdings > BUDGET {
            return None;
        }
    }
    let runout = |board: &BoardCards, dealt| {
        let board = board_eval(board);
        Runout {
            dealt,
            hero: rank_with_board(&board, &hole),
            board,
        }
    };
    let mut completed = board.clone();
    let mut runouts = Vec::new();
    let outcomes = if board.turn.is_some() {
        for &river in deck {
            completed.river = Some(river);
            runouts.push(runout(&completed, card_bit(river)));
        }
        44
    } else {
        // With no further bets, the order of turn and river does not affect
        // chip awards. Each compatible unordered pair has equal probability.
        for (i, &turn) in deck.iter().enumerate() {
            completed.turn = Some(turn);
            for &river in &deck[i + 1..] {
                completed.river = Some(river);
                runouts.push(runout(&completed, card_bit(turn) | card_bit(river)));
            }
        }
        45 * 44 / 2
    };
    // One full heads-up turn fits. A flop that cannot be certified inside the
    // same CPU budget yields the decision back to the existing strategy.
    let mut budget = BUDGET;
    let tie = pot / 2 + if odd_chip { pot % 2 } else { 0 };
    // Conditional on the actual folded hands, only these runouts remain
    // equiprobable. Their identities may be correlated with the public line.
    let possible = outcomes - mucked;
    let price = call * possible;
    let mut prefers_call = false;
    let mut prefers_fold = false;
    for (i, &first) in deck.iter().enumerate() {
        for &second in &deck[i + 1..] {
            let villain = HoleCards::new(first, second);
            let blocked = mask(villain);
            let mut wins: u64 = 0;
            let mut ties: u64 = 0;
            let mut remaining = outcomes;
            for r in &runouts {
                if r.dealt & blocked != 0 {
                    continue;
                }
                if budget == 0 {
                    return None;
                }
                budget -= 1;
                match r.hero.cmp(&rank_with_board(&r.board, &villain)) {
                    Ordering::Greater => wins += 1,
                    Ordering::Equal => ties += 1,
                    Ordering::Less => (),
                }
                remaining -= 1;
                // For the floor, unseen runouts all lose and the muck removes
                // winning rivers first. For the ceiling, unseen runouts win
                // and the muck removes losing rivers first. On the flop there
                // are no mucked hands; outcomes there count unordered pairs.
                let lower = wins.saturating_sub(mucked) * pot
                    + ties.saturating_sub(mucked.saturating_sub(wins)) * tie;
                let best_wins = (wins + remaining).min(possible);
                let upper = best_wins * pot + ties.min(possible - best_wins) * tie;
                if lower > price {
                    prefers_call = true;
                    break;
                }
                if upper < price {
                    prefers_fold = true;
                    break;
                }
                if remaining == 0 {
                    prefers_call |= upper > price;
                    prefers_fold |= lower < price;
                }
            }
            if prefers_fold && prefers_call {
                return None;
            }
        }
    }
    // An exactly indifferent holding supports either action. A pure call or
    // fold here is optimal against every possible opposing range.
    Some(if prefers_fold {
        PlayerAction::Fold
    } else {
        PlayerAction::Call
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use engine::betting::{pot_layers, Contribution, PotPolicy};
    use engine::card::parse_card;

    #[test]
    fn folded_cards_and_uncallable_chips_change_the_turn_certificate() {
        let card = |s: &str| parse_card(s).unwrap();
        let hero = HoleCards::new(card("As"), card("Ks"));
        let board = BoardCards {
            flop: Some([card("2s"), card("5s"), card("Jc")]),
            turn: Some(card("Qd")),
            river: None,
        };
        let deck = unseen_cards(
            board
                .all_cards()
                .into_iter()
                .fold(mask(hero), |m, c| m | card_bit(c)),
        );
        // A folded player can block two outs. Seven folded players can
        // block all winning rivers against a set.
        let decide = |cost, pot, mucked| unanimous(&board, hero, &deck, cost, pot, true, mucked);
        assert_eq!(decide(1, 150, 2), Some(PlayerAction::Call));
        assert_eq!(decide(1, 150, 14), None);

        // All three players put in 10 on earlier streets. Hero can call 20,
        // a folded player put in 80 this street, the bettor put in 100.
        let seats = [0, 80, 100].map(|paid| SeatPublic {
            committed_this_street: paid,
            ..Default::default()
        });
        let pot = called_pot(&seats, 210, 20, 20).unwrap();
        let layers = pot_layers(
            &[(30, true), (90, false), (110, true)].map(|(amount, live)| Contribution {
                id: amount,
                amount,
                live,
            }),
            PotPolicy::RefundUnmatched,
        );
        assert_eq!(
            pot,
            layers
                .iter()
                .filter(|p| p.eligible.contains(&30))
                .map(|p| p.amount)
                .sum::<u64>()
        );
        assert_eq!(pot, 90);
        assert_eq!(decide(20, pot, 2), None);
        assert_eq!(decide(20, 230, 2), Some(PlayerAction::Call));
    }
}
