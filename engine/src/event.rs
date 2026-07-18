//! Engine event sink.
//!
//! [`EngineEvent`] is emitted by [`crate::game::GameHand`] at every state-changing
//! point during a hand. The server consumes these via [`crate::game::GameHand::drain_events`]
//! and forwards them to connected clients.
//!
//! **Redaction is the consumer's responsibility.** Two fields are secret and
//! MUST be stripped/filtered before broadcasting: [`EngineEvent::HoleCardsDealt`]
//! `cards` (redact per recipient seat) and [`EngineEvent::HandStarted`]
//! `deck_seed` (the full shuffle key — never broadcast it to any client). The
//! stream is server-internal; "forward to clients" above assumes that redaction.
//!
//! ## Design notes
//! - Events carry `seat: u8` (not `PlayerId`) — the translation from the engine's
//!   internal `PlayerId` to the wire-visible seat index happens inside `drain_events`,
//!   not on the consumer side.
//! - `PotUpdated.side_pots` is always `Vec<SidePot>` (non-optional). An empty vec
//!   means no side pots exist, which is the normal case. Wire-side optionality is
//!   the server's concern (B-a6), not the engine's.
//! - Cards in `HoleCardsDealt` are unredacted. Per-client redaction (showing cards
//!   only to their owner) is the server's concern (B-a6).

use crate::action::PlayerAction;
use crate::card::Card;
use crate::game::HandResult;
use crate::hand::Street;
use crate::rng::DeckSeed;
use crate::round::SidePot;
use serde::{Deserialize, Serialize};

/// An event emitted by the engine at every observable state change.
///
/// The server drains these after each `start()`, `apply_action()`, or `finish()`
/// call and forwards them to clients in FIFO order.
///
/// All player-identity fields use `seat: u8` (the position number from
/// [`crate::game::HandSeat::seat`]), never `PlayerId`. The translation is
/// performed inside [`crate::game::GameHand::drain_events`].
// `HandFinished` carries a `HandResult`, a large value type (board + several
// pots/actions/showdown vectors + three maps, including `folded_hole_cards`). It
// is emitted at most ONCE per hand and drained/consumed immediately, so the size
// skew versus the small variants is not a hot-path concern — boxing it would only
// add a per-hand heap allocation. Mirrors the documented trade-off on
// `MpDealOutcome::Completed` (server/src/mp_engine_blind.rs).
#[allow(clippy::large_enum_variant)]
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum EngineEvent {
    /// A new hand has started. Emitted as the **first** event in `GameHand::start()`,
    /// before any `ActionApplied { Blind, .. }` events.
    ///
    /// Carries the static topology of the hand so the server can forward the
    /// dealer-button position to clients before blind animations begin (ADR-025).
    /// Also used by the bot module to build `DecisionContext` (ADR-024).
    HandStarted {
        /// Seat index of the dealer / button.
        dealer_seat: u8,
        /// Seat index of the small blind.
        sb_seat: u8,
        /// Seat index of the big blind.
        bb_seat: u8,
        /// Big blind amount in chips.
        big_blind: u64,
        /// Small blind amount in chips.
        small_blind: u64,
        /// Deck seed (for per-bot RNG derivation — ADR-024 §6, ADR-062 §2).
        /// 256-bit; the server projects it to a `u64` for the bot RNG.
        ///
        /// **SECURITY — MUST NOT be forwarded to clients.** In the default
        /// (plaintext) deal this is the full ChaCha20 shuffle key: it
        /// reconstructs the entire 52-card order (every hole card and the whole
        /// runout) via [`crate::deck::Deck::new`] before any betting. Unlike the
        /// per-seat filtering that redacts [`Self::HoleCardsDealt`], leaking this
        /// one field exposes *all* seats at once. The event stream is
        /// server-internal; consume `deck_seed` only on trusted paths (bot RNG,
        /// hand-history persistence) and strip it before broadcasting. See also
        /// [`crate::game::GameHand::deck_seed`] for a getter that does not travel
        /// on the broadcastable stream.
        deck_seed: DeckSeed,
    },

    /// Hole cards dealt to one player.
    ///
    /// Emitted once per player during `GameHand::start()` in **plaintext** mode,
    /// in seat order. `cards` is the raw (unredacted) pair — server must filter
    /// for other seats.
    ///
    /// **Blind mode (ADR-066 P1) emits NO `HoleCardsDealt` event at all** — the
    /// engine never originates or broadcasts a plaintext hole value. The
    /// contract (ADR-066 §2 step 3) permits "a redacted/opaque event variant, or
    /// none"; we choose **none** so the wire/event stream introduces no new
    /// variant the (not-yet-blind-aware) server must handle. The owning client
    /// learns its own two cards out-of-band via the threshold-decrypt protocol,
    /// never from an engine event.
    HoleCardsDealt {
        /// Seat index of the recipient.
        seat: u8,
        /// The two hole cards dealt.
        cards: [Card; 2],
    },

    /// A new street has been revealed (flop, turn, or river).
    ///
    /// Emitted whenever `close_street_and_advance` deals community cards.
    /// For all-in runout this may be emitted multiple times in a single
    /// `apply_action` call: `Flop → Turn → River` in that order.
    StreetRevealed {
        /// Which street is now active.
        street: Street,
        /// The newly revealed community cards (3 for flop, 1 for turn/river).
        new_cards: Vec<Card>,
        /// Current bet at the start of the new street (always 0 for a fresh street).
        current_bet: u64,
        /// Minimum raise-to amount for the new street.
        /// `None` when all remaining players are all-in (runout — no one can bet).
        min_raise_to: Option<u64>,
        /// Seat of the first actor on the new street.
        /// `None` when all remaining players are all-in (runout — no betting).
        next_actor_seat: Option<u8>,
    },

    /// A player action has been applied (including blind posts).
    ///
    /// Blind posting uses `PlayerAction::Blind { kind, amount }`. Two of these
    /// are emitted during `start()` before `HoleCardsDealt` events.
    ///
    /// The three post-action betting-state fields let the server forward live
    /// `current_bet` / `min_raise_to` / `next_actor_seat` to clients without
    /// waiting for a full Snapshot (which is only sent at hand start).
    ActionApplied {
        /// Seat of the acting player.
        seat: u8,
        /// The action taken.
        action: PlayerAction,
        /// Chips contributed to the pot by this action (0 for fold/check).
        contributed: u64,
        /// Current bet level for this street **after** the action was applied.
        current_bet: u64,
        /// Minimum raise-to amount **after** the action (None when the street is
        /// already closed or the hand is done).
        min_raise_to: Option<u64>,
        /// Seat of the next actor **after** this action (None when the street is
        /// closed or the hand is done).
        next_actor_seat: Option<u8>,
        /// ADR-079 (F2) — the engine-authoritative per-hand action sequence number
        /// for THIS action. Mirrors the recorded [`crate::game::ActionRecord::seq`].
        /// Blinds consume seq 0 (SB) and 1 (BB); the **first voluntary action is
        /// seq 2** (verified by `first_voluntary_action_seq_is_2_after_blinds`).
        ///
        /// The engine-blind signed-action chain (ADR-079 §3.3) binds this value
        /// into each signed `actionClaim`; the server forwards it to the web client
        /// via the `ServerMsg::ActionApplied.action_seq` echo (§4.4). It is purely
        /// informational for every other consumer.
        action_seq: u16,
    },

    /// The pot total (and side pots) have changed.
    ///
    /// Emitted after every action that moves chips. `side_pots` is always
    /// present; an empty `Vec` means no side pots (normal case).
    ///
    /// `has_all_in` is `true` when at least one active player has gone all-in at
    /// the point the event is emitted. The translate layer uses this flag (in
    /// addition to `has_real_split`) to gate phantom side-pot emission: a
    /// non-empty `side_pots` vec without any all-in is always contribution-
    /// accounting slices, never a true side pot.
    PotUpdated {
        /// Current total pot in chips.
        pot: u64,
        /// Active side pots (possibly empty).
        side_pots: Vec<SidePot>,
        /// True when at least one player is all-in at the time of emission.
        has_all_in: bool,
    },

    /// The hand has finished. Carries the full result.
    ///
    /// Emitted by `GameHand::finish()`.
    HandFinished {
        /// The complete hand result.
        result: HandResult,
    },
}
