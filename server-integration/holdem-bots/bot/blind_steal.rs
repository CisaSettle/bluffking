//! Small-blind steals from an admitted mixed fold/2.5 BB opening policy.

use super::{
    forced_blind, multiway_exploit, terminal, DecisionContext, OpponentModel, PlayerAction, Street,
};
use engine::{Chips, GameHand, GameSnapshot};
use rand::Rng;
use uuid::Uuid;

pub(crate) const OPEN_SOURCE: &str = "gtopen_sb_steal_read";

/// Exists only for the current cash hand. An ordinary baseline open never
/// activates the special continuation, even when it uses the same size.
/// Reconnects keep the same room actor. The runtime checkpoint abandons an
/// unfinished hand on a process restart and returns its commitments.
#[derive(Default)]
pub(crate) struct Hand {
    /// Shark SB, dealt human BB and that account, after validating forced posts.
    pub seats: Option<(u8, u8, Uuid)>,
    pub extra_opener: Option<u8>,
}

impl Hand {
    pub(crate) fn faced(&self, hand: &GameHand, snap: &GameSnapshot, bb: u32, actor: u8) -> bool {
        let Some((small, big, _)) = self.seats else {
            return false;
        };
        if actor != big || snap.street != Street::Preflop {
            return false;
        }
        let facts = hand.current_street_action_facts();
        let Some((open, earlier)) = facts.split_last() else {
            return false;
        };
        let size = bb / 2 * 5; // the opening snapshot admitted 100 BB stacks
        earlier.len() + 2 == snap.players.len()
            && earlier.iter().all(|f| f.action == PlayerAction::Fold)
            && snap
                .players
                .iter()
                .any(|p| p.seat == small && p.player_id == open.player_id && !p.folded)
            && snap.current_bet == Chips(size)
            && snap.pot == Chips(size + bb)
            && open.action
                == (PlayerAction::Raise {
                    amount: Chips(size),
                })
            && open.increased_bet
            && open.full_raise
            && open.pot_before == Chips(bb + bb / 2)
            && open.to_call_before == Chips(bb / 2)
            && open.amount == Chips(size - bb / 2)
            && open.committed_to == Chips(size)
    }
}

/// The node already authenticated all seats, posted chips, complete history,
/// 100 BB starting stacks and the two supported actions for this hand class.
pub(super) fn extra_open(ctx: &mut DecisionContext<'_>, model: &OpponentModel<'_>) -> bool {
    let n = ctx.table.seats.len();
    if ctx.seat != ctx.sb_seat
        || !(2..=9).contains(&n)
        || ctx.pot.0 != ctx.big_blind + ctx.big_blind / 2
        || ctx.to_call.0 != ctx.big_blind / 2
        || !ctx
            .public_history
            .is_some_and(|h| h.len() == n - 2 && h.iter().all(|f| f.action == PlayerAction::Fold))
    {
        return false;
    }
    let Some((big, owner)) = model.steal_bb else {
        return false;
    };
    let Some(stats) = model.by_seat.get(&big) else {
        return false;
    };
    if big != ctx.bb_seat || stats.profile_owner != Some(owner) {
        return false;
    }
    let Some(f) = stats
        .sb_steal_folds
        .and_then(|r| r[n - 2].lower_frequency(model.min_sample.max(20)))
    else {
        return false;
    };
    // Folding BB yields the whole 1.5 BB pot relative to our sampled fold.
    // If defended, the extra raise costs 2 BB. The tagged continuation can
    // check/fold for free or take conditionally proven nonnegative chip EV.
    // Transfer of this public frequency to our specific cards is a behavioral
    // assumption, including opponents' earlier folds and changes of style.
    // At f = .70 modeled gain is at least .45 BB. Even if every steal fails,
    // .015 * 2 BB <= .02 of the 1.5 BB pot per sampled baseline fold.
    f > 0.70 && ctx.rng.gen_bool(0.015)
}

pub(super) fn continuation(ctx: &mut DecisionContext<'_>) -> PlayerAction {
    let nuts = match ctx.street {
        Street::River => multiway_exploit::river_nuts(ctx.board, ctx.hole),
        // A royal already made before the river uses at least one private
        // card. No future board can give this one opponent that same royal.
        Street::Flop | Street::Turn
            if engine::rank_hand(&ctx.hole, ctx.board) == engine::HandRank::StraightFlush(9) =>
        {
            Some(true)
        }
        _ => None,
    };
    if let Some(sole_winner) = nuts {
        if ctx.to_call.0 > 0 {
            return PlayerAction::Call;
        }
        if sole_winner && ctx.can_raise {
            return super::emit_bet(ctx, super::river_bet_chips(ctx, 0.5));
        }
    }
    if ctx.to_call.0 == 0 {
        return PlayerAction::Check;
    }
    forced_blind::decision(ctx)
        .or_else(|| terminal::decision(ctx))
        .unwrap_or(PlayerAction::Fold)
}
