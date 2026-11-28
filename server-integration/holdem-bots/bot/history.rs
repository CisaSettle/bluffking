//! Public, chronological action context for the Shark strategy.
//!
//! Engine facts supply full/short-raise semantics. This projection accepts no
//! private cards, deck state, showdown result, or future action.

use engine::{PlayerAction, PlayerId, Street, StreetActionFact};

/// One voluntary action, expressed using seats and public chip amounts.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PublicAction {
    pub street: Street,
    pub seat: u8,
    /// Raw action. Use the flags below rather than treating every all-in as a
    /// raise: an all-in may be a call or a short raise without reopening.
    pub action: PlayerAction,
    pub increased_bet: bool,
    pub full_raise: bool,
    pub amount: u32,
    pub committed_to: u32,
    pub to_call_before: u32,
    pub pot_before: u32,
}

impl PublicAction {
    pub fn is_call_like(&self) -> bool {
        matches!(self.action, PlayerAction::Call)
            || (matches!(
                self.action,
                PlayerAction::Raise { .. } | PlayerAction::AllIn
            ) && !self.increased_bet)
    }
}

/// Project already-public engine facts in street order. Any unmapped player
/// invalidates the projection, so an incomplete history cannot masquerade as
/// a limped pot. The caller supplies only already-recorded streets.
pub fn public_action_history(
    streets: impl IntoIterator<Item = (Street, Vec<StreetActionFact>)>,
    seat_of: impl Fn(PlayerId) -> Option<u8>,
) -> Option<Vec<PublicAction>> {
    streets
        .into_iter()
        .flat_map(|(street, facts)| facts.into_iter().map(move |fact| (street, fact)))
        .map(|(street, fact)| {
            Some(PublicAction {
                street,
                seat: seat_of(fact.player_id)?,
                action: fact.action,
                increased_bet: fact.increased_bet,
                full_raise: fact.full_raise,
                amount: fact.amount.0,
                committed_to: fact.committed_to.0,
                to_call_before: fact.to_call_before.0,
                pot_before: fact.pot_before.0,
            })
        })
        .collect()
}

/// Full-raise depth and the seat's actual path into the pot. Depth 0 means
/// limped/unopened, 1 an open, 2 a 3-bet, and 3+ a 4-bet or later raise.
/// These facts remain unchanged when a short stack or a large later-street
/// bet changes SPR. A short all-in may change `last_aggressor`, but does not
/// increment the full-raise depth or become a fictitious call.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct PreflopLine {
    pub raises: u8,
    /// Depth of this seat's most recent full raise; zero if none.
    pub raised_at: u8,
    /// Depth of this seat's most recent call; `Some(0)` is a limp.
    pub called_at: Option<u8>,
    /// First voluntary wager increase, excluding blind/straddle posts.
    pub opener: Option<u8>,
    pub last_aggressor: Option<u8>,
}

pub fn preflop_line(history: &[PublicAction], seat: u8) -> PreflopLine {
    let mut line = PreflopLine::default();
    for action in history
        .iter()
        .filter(|action| action.street == Street::Preflop)
    {
        if action.increased_bet {
            line.opener.get_or_insert(action.seat);
            line.last_aggressor = Some(action.seat);
        }
        if action.full_raise {
            line.raises = line.raises.saturating_add(1);
            if action.seat == seat {
                line.raised_at = line.raises;
            }
        } else if action.seat == seat && action.is_call_like() {
            line.called_at = Some(line.raises);
        }
    }
    line
}
