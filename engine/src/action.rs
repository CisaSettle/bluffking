//! Player actions and action records.

use crate::hand::Street;
use crate::player::{Chips, PlayerId};
use serde::{Deserialize, Serialize};
use thiserror::Error;

/// The kind of blind posted.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum BlindKind {
    /// Small blind.
    Small,
    /// Big blind.
    Big,
    /// Live forced UTG straddle.
    Straddle,
}

/// A player's betting action.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "action", rename_all = "snake_case")]
pub enum PlayerAction {
    /// Fold — discard hole cards and forfeit any claim to the pot.
    Fold,
    /// Check — pass the action without betting (only valid when no bet is pending).
    Check,
    /// Call — match the current bet.
    Call,
    /// Raise — increase the current bet to the given total amount (not a delta).
    ///
    /// Uses a named field so that serde's internally-tagged format can serialize
    /// this variant without error. `Raise(Chips)` (a newtype variant) would fail
    /// serde_json serialization with "cannot serialize tagged newtype variant
    /// containing an integer" — the internal tag format requires a map inner type.
    Raise { amount: Chips },
    /// All-in — commit the player's entire remaining stack.
    AllIn,
    /// Blind post (small or big). Server-synthesized; clients never send this.
    Blind {
        /// Which type of blind.
        kind: BlindKind,
        /// The posted amount.
        amount: Chips,
    },
}

/// One betting action taken during a hand, with chip context.
///
/// The `stack_before` field is populated by the engine at action time —
/// an AI pipeline never needs to join another table to reconstruct chip history.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ActionRecord {
    /// 0-based index of this action within the hand.
    ///
    /// Contract: `seq` fits in `u16` (the wire protocol range-locks it and the DB
    /// stores it as a smallint). The counter that produces it saturates rather
    /// than wraps (U33, dual-AI OSS review) so it can never alias a late action
    /// onto an early sequence number (e.g. the SB blind at `seq == 0`). Reaching
    /// 65 535 actions in one hand is not physically possible in real play.
    pub seq: u16,
    /// Street on which the action occurred.
    pub street: Street,
    /// Player who took the action.
    pub player_id: PlayerId,
    /// The action taken.
    pub action: PlayerAction,
    /// Total chips committed by this action (0 for fold/check).
    pub amount: Chips,
    /// Player's stack immediately **before** this action.
    pub stack_before: Chips,
    /// Player's stack immediately **after** this action.
    pub stack_after: Chips,
    /// Total pot size immediately **before** this action.
    pub pot_before: Chips,
    /// Total pot size immediately **after** this action.
    pub pot_after: Chips,
}

/// Errors that can occur when applying an action to a betting round or hand.
///
/// `#[non_exhaustive]` (U70, dual-AI OSS review): new failure modes may be added
/// in a minor release, so downstream matches must include a wildcard arm.
#[derive(Debug, Clone, PartialEq, Eq, Error)]
#[non_exhaustive]
pub enum ActionError {
    /// The acting player is not the one whose turn it is.
    #[error("not your turn")]
    NotYourTurn,
    /// The action is illegal in the current state (e.g. `check` with a bet pending).
    #[error("invalid action")]
    InvalidAction,
    /// Raise amount is below the minimum raise.
    #[error("raise amount below minimum")]
    BelowMinRaise,
    /// The player is not active in the current hand.
    #[error("player not in hand")]
    NotInHand,
    /// The hand is already finished.
    #[error("hand already finished")]
    HandFinished,
    /// The hand has not started yet.
    #[error("hand not started")]
    HandNotStarted,
    /// Cannot act: the player has already folded.
    #[error("player has folded")]
    AlreadyFolded,
}
