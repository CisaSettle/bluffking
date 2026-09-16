//! Full Texas Hold'em hand orchestration.
//!
//! [`GameHand`] drives a complete hand from blind posting through showdown.
//! It wraps [`BettingRound`] for each street and delegates evaluation to
//! [`crate::eval::rank_players`].

use crate::action::{ActionError, ActionRecord, BlindKind, PlayerAction};
use crate::card::Card;
use crate::deck::Deck;
use crate::eval::{rank_players, HandRank};
use crate::event::EngineEvent;
use crate::hand::{BoardCards, HoleCards, Street};
use crate::player::{Chips, PlayerId};
use crate::rng::{DeckSeed, PokerRng};
use crate::round::{BettingRound, SidePot};
use serde::{Deserialize, Serialize};
use std::collections::{HashMap, HashSet};

// ---------------------------------------------------------------------------
// Public types
// ---------------------------------------------------------------------------

/// A seat's hole-card holding.
///
/// This enum is the **type-level guarantee** that backs the engine-blind table
/// class (ADR-066 §2 step 3, §6 P1): in blind mode the engine holds **no**
/// plaintext hole value for a seat until an externally-verified showdown reveal
/// is injected. The only constructors that produce a plaintext-bearing variant
/// (`Plain` / `Revealed`) are:
///
/// - plaintext mode: [`GameHand::start`] deals from the deck and writes `Plain`;
/// - blind mode: [`GameHand::inject_showdown_reveal`] writes `Revealed` after the
///   server has verified the reveal against the pre-committed ciphertext.
///
/// In blind mode [`GameHand::new_blind`] seeds every seat with [`HoleSlot::Opaque`]
/// and [`GameHand::start`] does **not** write `Plain` (it shuffles nothing and
/// deals nothing). Therefore "no plaintext hole value before a showdown reveal"
/// is structural: there is no code path in blind mode that can place a `Plain`
/// (or a `Revealed` without an injected, verified reveal) onto a seat. Plaintext
/// access flows through [`HoleSlot::plaintext`], which returns `None` for
/// `Opaque`/`Empty`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HoleSlot {
    /// No cards held yet. Plaintext-mode pre-`start()` state (was `None`).
    Empty,
    /// Plaintext cards dealt from a deck (plaintext mode only).
    Plain(HoleCards),
    /// Blind mode: the engine knows the seat has two cards but **not** their
    /// value. No plaintext exists. This is the only state a blind-mode seat can
    /// hold before showdown.
    Opaque,
    /// Blind mode: an externally-verified showdown reveal has been injected for
    /// a non-folded contender. The only blind-mode path to a plaintext value.
    Revealed(HoleCards),
}

impl HoleSlot {
    /// The plaintext hole cards, if any are legitimately known to the engine.
    ///
    /// Returns `Some` only for `Plain` (plaintext mode) and `Revealed` (a
    /// verified blind-mode showdown reveal). `Opaque` and `Empty` yield `None`.
    /// This is the single read path used by `snapshot()`, `finish()`, and the
    /// distribution logic — so an `Opaque` seat can never feed a card into
    /// `rank_hand` / `distribute_pots`.
    pub fn plaintext(self) -> Option<HoleCards> {
        match self {
            HoleSlot::Plain(h) | HoleSlot::Revealed(h) => Some(h),
            HoleSlot::Empty | HoleSlot::Opaque => None,
        }
    }

    /// True when this seat holds an opaque (value-unknown) blind handle.
    pub fn is_opaque(self) -> bool {
        matches!(self, HoleSlot::Opaque)
    }
}

/// A player seat at the start of the hand.
#[derive(Debug, Clone)]
pub struct HandSeat {
    /// Player identity.
    pub player_id: PlayerId,
    /// Starting stack (immutable reference).
    pub starting_stack: Chips,
    /// Current stack (updated street by street, not mid-round).
    pub stack: Chips,
    /// Hole-card holding. `Plain`/`Empty` in plaintext mode; `Opaque`/`Revealed`
    /// in blind mode. See [`HoleSlot`] for the type-level blind invariant.
    pub hole: HoleSlot,
    /// Folded this hand.
    pub folded: bool,
    /// All-in this hand.
    pub all_in: bool,
    /// Seat index.
    pub seat: u8,
}

impl HandSeat {
    /// The plaintext hole cards for this seat, if legitimately known. Thin
    /// accessor over [`HoleSlot::plaintext`] kept so callers read intent.
    pub fn hole_cards(&self) -> Option<HoleCards> {
        self.hole.plaintext()
    }
}

/// A snapshot of game state after any action.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GameSnapshot {
    /// Current street.
    pub street: Street,
    /// Community cards.
    pub board: BoardCards,
    /// Total pot (all contributions across all streets, including current).
    pub pot: Chips,
    /// Side pots (if any all-ins).
    pub side_pots: Vec<SidePot>,
    /// Current actor (`None` if round is over or hand is done).
    pub current_actor: Option<PlayerId>,
    /// Minimum raise-to amount.
    pub min_raise_to: Option<Chips>,
    /// Current bet this street.
    pub current_bet: Chips,
    /// Per-player state.
    pub players: Vec<SnapshotPlayer>,
    /// Seat index of the dealer / button (ADR-024 §5.1, ADR-025 §3).
    /// Populated from `self.players[self.dealer_idx].seat` in `snapshot()`.
    pub dealer_seat: u8,
}

/// Per-player state in a snapshot.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SnapshotPlayer {
    /// Player identity.
    pub player_id: PlayerId,
    /// Current stack.
    pub stack: Chips,
    /// Hole cards (server layer redacts other players').
    pub hole_cards: Option<HoleCards>,
    /// Folded.
    pub folded: bool,
    /// All-in.
    pub all_in: bool,
    /// Chips committed this street.
    pub committed_this_street: Chips,
    /// Last action taken.
    pub last_action: Option<PlayerAction>,
    /// Seat number.
    pub seat: u8,
}

/// Winner of a single pot, or refund recipient when `PotResult::is_refund`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PotWinner {
    /// Player who won or receives returned chips.
    pub player_id: PlayerId,
    /// Amount won from this pot.
    pub amount_won: Chips,
}

/// Result of a single pot.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PotResult {
    /// Ordering index.
    pub index: usize,
    /// Total chips in this pot.
    pub amount: Chips,
    /// Players eligible for this pot.
    pub eligible_player_ids: Vec<PlayerId>,
    /// Winner(s), or refund recipients when `is_refund`.
    pub winners: Vec<PotWinner>,
    /// True when this result represents returned uncalled chips, not a pot win.
    #[serde(default)]
    pub is_refund: bool,
}

/// Showdown entry for one player.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ShowdownEntry {
    /// Player identity.
    pub player_id: PlayerId,
    /// Hole cards revealed at showdown.
    pub hole_cards: HoleCards,
    /// Best hand rank achieved.
    pub rank: HandRank,
    /// Category name string.
    pub rank_name: String,
}

/// Final result of a completed hand.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct HandResult {
    /// Deck seed (for DB storage and reproducibility). 256-bit since ADR-062 §2.
    pub deck_seed: DeckSeed,
    /// Community cards.
    pub board: BoardCards,
    /// Per-pot results.
    pub pots: Vec<PotResult>,
    /// Total chips awarded per player.
    pub chips_awarded: HashMap<u64, u32>,
    /// Final stacks per player.
    pub final_stacks: HashMap<u64, u32>,
    /// All recorded actions.
    pub actions: Vec<ActionRecord>,
    /// Showdown results.
    pub showdown: Vec<ShowdownEntry>,
    /// Canonical WSOP / TDA show-order for the non-folded players at showdown.
    ///
    /// Rules (2026-05-28 BUG-D):
    /// - If any bet/raise occurred on the river → the **last river aggressor**
    ///   shows first, then turn-order from the next non-folded seat clockwise.
    /// - If the river checked through (or there was no river because all
    ///   remaining players were all-in pre-river) → the first non-folded
    ///   player clockwise from the button shows first, then turn-order.
    /// - Empty `Vec` when there was no showdown (fold-around).
    ///
    /// Entries are `PlayerId`; the server translates them to wire-visible
    /// seat indexes via its seat map (no `rs_poker` types in public sigs —
    /// ADR-012; `PlayerId` is engine-owned).
    pub show_order: Vec<PlayerId>,
    /// Dealt hole cards of players who FOLDED this hand, keyed by `PlayerId`
    /// inner value (like `chips_awarded` / `final_stacks`).
    ///
    /// Populated **only for plaintext hands**; engine-blind (mental-poker)
    /// hands leave this EMPTY. This is structural, not a runtime check: a
    /// folded seat can never become `HoleSlot::Revealed`
    /// ([`GameHand::inject_showdown_reveal`] rejects folded players), so a
    /// folded blind seat is always `Opaque` and its value is never known to the
    /// engine. The assembler additionally gates population on
    /// `HandMode::Plaintext`, so no blind result ever carries a folded value.
    ///
    /// This backs the server's post-hand *voluntary* "show my folded cards"
    /// affordance (issue #6 extension): at hand end the server can resolve a
    /// folded human's OWN dealt holes for a plaintext hand, exactly as it
    /// resolves a showdown loser's from `showdown`. Folded players are still
    /// NEVER auto-exposed — this only enables their own opt-in reveal.
    ///
    /// It is never broadcast or persisted wholesale (the wire translation reads
    /// only `showdown`; persist serializes only actions/pots/board), so it adds
    /// no leak surface: a folded seat's cards leave the server solely through
    /// the same identity-checked, opt-in `CardsShown` path a loser's cards do.
    #[serde(default)]
    pub folded_hole_cards: HashMap<u64, HoleCards>,
}

// ---------------------------------------------------------------------------
// Internal types
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, PartialEq, Eq)]
enum Phase {
    NotStarted,
    Betting(Street),
    Done,
}

/// Card-source mode for the hand.
///
/// `Plaintext` is the existing, byte-for-byte-unchanged path: the engine owns a
/// `Deck`, deals hole + board cards from it, and knows every value. `Blind` is
/// the engine-blind path (ADR-066 P1): the engine owns no usable deck, deals
/// nothing, and learns hole values only via [`GameHand::inject_showdown_reveal`]
/// and board values only via [`GameHand::inject_board_for_street`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum HandMode {
    Plaintext,
    Blind,
}

/// Pure, authoritative betting transition preview.
///
/// Engine-blind clients sign these public chip/round facts before the server
/// mutates the hand. They remove every ambiguous inference from an action label:
/// a `Call` or `Raise` can exhaust a stack, and a short all-in may raise the bet
/// without reopening action. The preview is produced by applying the action to a
/// cloned [`BettingRound`]; the live hand, history, pot, sequence and events are
/// untouched.
///
/// The `*_before` fields describe the acting player's current street. The
/// `*_after`, `next_actor`, and `street_after` fields describe the stabilized
/// [`GameHand`] after the action has also performed any automatic street
/// transition. Consequently, a street-closing preflop call reports the flop's
/// zeroed commitments/current bet and its first actor. `round_closed_after` is
/// the one deliberate exception: it records whether this action closed the
/// *prior* betting round. This lets a verifier distinguish a street transition
/// from an ordinary same-street action while still chaining directly into the
/// next prompt's stabilized pre-state.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ActionTransitionPreview {
    pub stack_before: Chips,
    pub committed_before: Chips,
    pub current_bet_before: Chips,
    pub min_raise_to_before: Option<Chips>,
    pub stack_after: Chips,
    pub committed_after: Chips,
    pub current_bet_after: Chips,
    pub min_raise_to_after: Option<Chips>,
    pub all_in_after: bool,
    pub round_closed_after: bool,
    pub next_actor: Option<PlayerId>,
    pub next_actor_can_raise_after: bool,
    pub street_after: Street,
    /// No further betting action can occur; any remaining community cards are
    /// a forced runout before showdown. In blind mode the physical engine may
    /// still be paused at a pending board street, so this is stronger than
    /// `hand_done_after` and is the protocol-facing terminal signal.
    pub betting_terminal_after: bool,
    pub hand_done_after: bool,
}

/// Engine-derived betting semantics for one recorded action on the current
/// street.
///
/// `PlayerAction::AllIn` describes how chips were committed, not whether the
/// wager was a raise.  An all-in may be an under-call, an exact call, a short
/// raise that increases the wager without reopening action, or a full raise.
/// Strategy/coach code must use these flags instead of classifying every
/// `AllIn` token as a 3-bet/4-bet.  The facts are reconstructed from the same
/// action records and minimum-raise rules that the engine applied.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StreetActionFact {
    pub player_id: PlayerId,
    pub action: PlayerAction,
    /// The action raised the street's wager above the previous `current_bet`.
    /// A short all-in raise is `true`; an all-in call/under-call is `false`.
    pub increased_bet: bool,
    /// The wager increase was at least the last full bet/raise increment.
    /// Only these actions advance preflop open/3-bet/4-bet chart depth.
    pub full_raise: bool,
}

impl StreetActionFact {
    /// Whether this action contributed a caller rather than a new wager.
    ///
    /// Some protocol-shaped `Raise` / `AllIn` actions are really exact or
    /// under-calls.  Conversely, a short all-in that increases the wager is
    /// still a raise for caller-count purposes even though it does not reopen
    /// action.  Keep this projection separate from [`Self::strategy_action`],
    /// whose job is only to preserve full-raise chart depth.
    pub fn is_call_like(&self) -> bool {
        matches!(self.action, PlayerAction::Call)
            || (matches!(
                self.action,
                PlayerAction::Raise { .. } | PlayerAction::AllIn
            ) && !self.increased_bet)
    }

    /// Action shape used by full-raise-depth strategy consumers.
    ///
    /// A short raise retains its real wager increase via
    /// [`Self::increased_bet`]; an all-in call has that flag clear. Both are
    /// represented as `Call` here so code that counts `Raise | AllIn` cannot
    /// mistake either for a full re-raise.
    pub fn strategy_action(&self) -> PlayerAction {
        if matches!(
            self.action,
            PlayerAction::Raise { .. } | PlayerAction::AllIn
        ) && !self.full_raise
        {
            PlayerAction::Call
        } else {
            self.action.clone()
        }
    }
}

// ---------------------------------------------------------------------------
// GameHand
// ---------------------------------------------------------------------------

/// How many community cards a street reveals.
///
/// Single source of truth for the street → board-card-count table: the
/// plaintext deal, the blind-mode `inject_board_for_street` arity check, and
/// the board write itself all read it.
pub const fn cards_for_street(street: Street) -> usize {
    match street {
        Street::Preflop => 0,
        Street::Flop => 3,
        Street::Turn | Street::River => 1,
    }
}

/// Orchestrates a complete Texas Hold'em hand.
#[derive(Clone)]
pub struct GameHand {
    seats: Vec<HandSeat>,
    dealer_idx: usize,
    big_blind: Chips,
    small_blind: Chips,
    /// Optional live UTG straddle amount. When present on a 3+ handed table,
    /// the seat immediately left of the big blind posts before cards are dealt,
    /// action opens to that seat's left, and a fully-funded straddle becomes
    /// the preflop bring-in / minimum raise increment.
    forced_straddle: Option<Chips>,
    /// The preflop opening wager / minimum raise increment ACTUALLY handed to
    /// [`BettingRound::new_preflop`] when the hand started: the big blind
    /// normally, the straddle when a live straddle was fully funded (a SHORT
    /// all-in straddle does not inflate it — see `start()`).
    ///
    /// Stored because it is the authoritative preflop baseline and
    /// [`Self::current_street_action_facts`] must project raise depth from the
    /// same number the betting round enforces. Seeded to `big_blind` so a hand
    /// that has not started yet reads the no-straddle baseline
    /// (discovery-r1 BUG-175).
    preflop_bring_in: Chips,
    /// Card-source mode. `Plaintext` deals from `deck`; `Blind` ignores it.
    mode: HandMode,
    deck: Deck,
    deck_seed: DeckSeed,
    board: BoardCards,
    phase: Phase,
    /// Active betting round.
    betting_round: Option<BettingRound>,
    /// Pot accumulated from all completed streets (already absorbed).
    completed_pot: Chips,
    /// Side pots accumulated from completed streets.
    cumulative_side_pots: Vec<SidePot>,
    /// All recorded actions.
    actions: Vec<ActionRecord>,
    action_seq: u16,
    last_action: HashMap<u64, PlayerAction>,
    /// Internal event buffer, drained by the server via [`drain_events`].
    events: Vec<EngineEvent>,
    /// Blind mode only: the street whose board the engine has advanced to but
    /// is still awaiting an injected reveal for (`inject_board_for_street`).
    /// `None` when no board is pending (plaintext mode always keeps this `None`,
    /// since it deals the board itself in `close_street_and_advance`).
    pending_board_street: Option<Street>,
    /// The street the hand ended on, captured when the phase transitions to
    /// `Done`. Lets `current_street()` report the real final street instead of a
    /// `Preflop` sentinel for completed hands (U34, dual-AI OSS review).
    finished_street: Option<Street>,
}

/// Why a blind-mode `finish_blind` / `inject_*` call could not complete.
///
/// Blind mode never panics and never invents cards on a malformed/incomplete
/// reveal — it returns one of these typed errors so the server can void the
/// hand (ADR-066 T-FALLBACK / T-VERIFY-REVEAL) rather than crash the room.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum BlindFinishError {
    /// A non-folded contender reached a contested showdown with no injected,
    /// verified reveal (still `Opaque`). The server must void the hand.
    #[error("blind showdown: non-folded contender (player {player_id:?}, seat {seat}) has no injected reveal")]
    MissingReveal {
        /// The contender missing a reveal.
        player_id: PlayerId,
        /// Their seat index.
        seat: u8,
    },
    /// `finish_blind` was called before the hand reached `Phase::Done`. Returned
    /// instead of panicking so the server can VOID the hand rather than crash the
    /// room (ADR-066 — blind mode never panics).
    #[error("finish_blind rejected: hand is not finished (phase != Done)")]
    NotDone,
    /// `finish_blind` was called on a plaintext-mode hand (use [`GameHand::finish`]).
    /// Returned instead of panicking, for the same reason as [`Self::NotDone`].
    #[error("finish_blind rejected: hand is not in blind mode (use finish())")]
    NotBlindMode,
    /// [`GameHand::finish_blind_forfeit_board`] was asked to forfeit a board
    /// withholder but, after force-folding every named withholder, NO non-folded
    /// contender remained to award the pot to (e.g. every contender was named a
    /// withholder). This is a coordinator misuse — the server must VOID with a
    /// blameless stake-return rather than award chips to no one (never invents a
    /// winner).
    #[error(
        "finish_blind_forfeit_board rejected: no honest survivor after force-folding withholders"
    )]
    NoSurvivor,
}

/// Why a blind-mode injection (`inject_board_for_street` / `inject_showdown_reveal`)
/// was rejected. Like [`BlindFinishError`], the engine never panics — it returns
/// a typed error so the server can decide whether to void the hand or retry.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum BlindInjectError {
    /// `inject_*` called on a plaintext-mode hand.
    #[error("injection rejected: hand is not in blind mode")]
    NotBlindMode,
    /// No board is currently awaiting injection (e.g. injecting twice, or
    /// injecting before the street advanced).
    #[error("no board is pending injection for the current street")]
    NoBoardPending,
    /// The injected board card count is wrong for the pending street
    /// (3 for the flop, 1 for turn/river).
    #[error("wrong board card count for {street}: expected {expected}, got {got}")]
    WrongBoardCount {
        /// The street whose board was being injected.
        street: Street,
        /// The required count (3 flop / 1 turn / 1 river).
        expected: usize,
        /// The count actually supplied.
        got: usize,
    },
    /// A showdown reveal was injected for an unknown player id.
    #[error("showdown reveal rejected: no seat with player id {0:?}")]
    UnknownPlayer(PlayerId),
    /// A showdown reveal was injected for a folded seat (folded seats stay
    /// sealed forever — ADR-066 §2 step 6).
    #[error("showdown reveal rejected: player {0:?} has folded (folded cards stay sealed)")]
    PlayerFolded(PlayerId),
    /// A showdown reveal was injected at the wrong time — the hand is not yet
    /// finished (reveals are only consumed at showdown).
    #[error("showdown reveal rejected: hand is not at showdown (not done)")]
    NotAtShowdown,
    /// An injected card collides with a card the engine already holds — another
    /// board card, an already-`Revealed` hole card, or another card in the same
    /// injected set. The engine is the authoritative ranker and must not trust
    /// injected cards (ADR-066 §2): a duplicate would silently mis-rank in
    /// release builds (`rank_hand` only `debug_assert!`s uniqueness). The server
    /// must re-derive / void rather than rank a deck with a repeated card.
    #[error("injection rejected: duplicate card {card}")]
    DuplicateCard {
        /// The card that collided with one already known to the engine.
        card: crate::card::Card,
    },
    /// A showdown reveal was injected for a seat that is already `Revealed` with
    /// DIFFERENT cards. A verified reveal is immutable showdown evidence
    /// (ADR-066); silently overwriting it would let a second injection mutate
    /// accepted evidence. Idempotent re-injection of the identical cards is
    /// accepted (returns `Ok`).
    #[error("showdown reveal rejected: player {0:?} is already revealed with different cards")]
    AlreadyRevealed(PlayerId),
}

mod actions;
mod pots;
mod start;
mod streets;

#[cfg(test)]
mod blind_tests;
#[cfg(test)]
mod tests;

impl GameHand {
    /// Whether the hand is done.
    pub fn is_done(&self) -> bool {
        self.phase == Phase::Done
    }

    /// Whether the hand has been started (i.e. `phase != NotStarted`).
    ///
    /// Returns `true` from the moment `start()` is called, including after
    /// `finish()` transitions to `Phase::Done`. This lets the server
    /// distinguish a "waiting for players" table from an active preflop hand
    /// without exposing the private `Phase` enum.
    pub fn is_started(&self) -> bool {
        self.phase != Phase::NotStarted
    }

    /// Live straddle seat and requested amount for this hand, when configured
    /// and the hand has at least three players. This is public table topology;
    /// reconnect snapshots may expose it without revealing card state.
    pub fn straddle_position(&self) -> Option<(u8, Chips)> {
        let amount = self.forced_straddle?;
        let n = self.seats.len();
        if n < 3 {
            return None;
        }
        let (_, bb_idx) = blind_positions(self.dealer_idx, n);
        let idx = (bb_idx + 1) % n;
        Some((self.seats[idx].seat, amount))
    }

    /// Drain all events accumulated since the last call.
    ///
    /// Returns events in FIFO order and clears the internal buffer. Calling
    /// this twice in a row returns events on the first call and an empty `Vec`
    /// on the second.
    ///
    /// The server calls this after every `start()`, `apply_action()`, and
    /// `finish()`. Events already carry `seat: u8` — no further translation
    /// is needed by the caller.
    pub fn drain_events(&mut self) -> Vec<EngineEvent> {
        std::mem::take(&mut self.events)
    }

    /// Read-only view of the hand's seats (ADR-066 §2 — the server-blind
    /// coordinator inspects `hole` slots to prove no plaintext hole value exists
    /// before showdown, and reads fold/stack state). Returns `Opaque`/`Revealed`
    /// holdings in blind mode; no plaintext value is exposed for an `Opaque` seat.
    pub fn seats(&self) -> &[HandSeat] {
        &self.seats
    }

    /// The `PlayerId`s of every seat that has not folded, in seat order. Used by
    /// the engine-blind coordinator to decide contested vs uncontested and to
    /// identify the contenders that owe a showdown reveal.
    pub fn non_folded_player_ids(&self) -> Vec<PlayerId> {
        self.seats
            .iter()
            .filter(|s| !s.folded)
            .map(|s| s.player_id)
            .collect()
    }

    /// Build a (unredacted) game snapshot.
    pub fn snapshot(&self) -> GameSnapshot {
        let current_actor = self.betting_round.as_ref().and_then(|r| r.current_player());
        // Only expose min_raise_to when the current actor may actually raise;
        // after a non-reopening all-in the next actor can only call/fold (TDA
        // Rule 6) and a non-None min_raise_to would imply an illegal raise
        // (audit 2026-06-03).
        let min_raise_to = self.betting_round.as_ref().and_then(|r| {
            if r.current_player_can_raise() {
                Some(r.min_raise_to())
            } else {
                None
            }
        });
        let current_bet = self
            .betting_round
            .as_ref()
            .map(|r| r.current_bet())
            .unwrap_or(Chips::ZERO);
        let round_pot = self
            .betting_round
            .as_ref()
            .map(|r| r.pot_total())
            .unwrap_or(Chips::ZERO);
        let pot = Chips(self.completed_pot.0 + round_pot.0);

        let side_pots = self.merged_side_pots();

        let players = self
            .seats
            .iter()
            .map(|seat| {
                let committed = self
                    .betting_round
                    .as_ref()
                    .map(|r| r.player_contributed(seat.player_id))
                    .unwrap_or(Chips::ZERO);
                SnapshotPlayer {
                    player_id: seat.player_id,
                    // In blind mode this is `None` (Opaque) before showdown — the
                    // server process never materializes plaintext. In plaintext
                    // mode it carries the dealt cards exactly as before.
                    hole_cards: seat.hole_cards(),
                    stack: seat.stack,
                    folded: seat.folded,
                    all_in: seat.all_in,
                    committed_this_street: committed,
                    last_action: self.last_action.get(&seat.player_id.inner()).cloned(),
                    seat: seat.seat,
                }
            })
            .collect();

        // Wrap the dealer index (`% n`) to match `blind_positions`/`start()`,
        // so an out-of-range `dealer_idx` reports the wrapped seat rather than
        // the seat-0 fallback (defensive, audit 2026-06-03).
        let dealer_seat = if self.seats.is_empty() {
            0
        } else {
            self.seats[self.dealer_idx % self.seats.len()].seat
        };

        GameSnapshot {
            street: self.current_street(),
            board: self.board.clone(),
            pot,
            side_pots,
            current_actor,
            min_raise_to,
            current_bet,
            players,
            dealer_seat,
        }
    }

    /// The deck seed (stored in DB). 256-bit since ADR-062 §2.
    pub fn deck_seed(&self) -> DeckSeed {
        self.deck_seed
    }

    /// Read-only reference to the active betting round, if any.
    ///
    /// Used by the session layer to query the current actor's raise permission.
    pub fn betting_round_ref(&self) -> Option<&BettingRound> {
        self.betting_round.as_ref()
    }
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

/// Returns `(sb_idx, bb_idx)` for the given dealer index and number of seats.
///
/// In heads-up (n=2), the dealer is the SB and acts first pre-flop (WSOP/TDA
/// rule). In 3+ player, the seat after the dealer is the SB.
///
/// A hand needs at least two seats to have blinds; `n < 2` has no meaningful
/// blind assignment and returns `(0, 0)` rather than dividing by zero (U35,
/// dual-AI OSS review — the public API must not panic on a degenerate seat
/// count).
pub fn blind_positions(dealer_idx: usize, n: usize) -> (usize, usize) {
    if n < 2 {
        return (0, 0);
    }
    let dealer_idx = dealer_idx % n;
    if n == 2 {
        // Heads-up: dealer is SB, other player is BB. Apply `% n` to the dealer
        // here too — `start()` indexes `self.seats[sb_idx]` BEFORE the `% n`
        // guard on the HandStarted event, so an out-of-range `dealer_idx`
        // (>= 2) would panic on index-out-of-bounds. The 3+ branch already
        // normalizes; this makes the stated defense-in-depth hold for n == 2.
        let sb = dealer_idx;
        let bb = (dealer_idx + 1) % 2;
        (sb, bb)
    } else {
        // 3+ players: standard rotation.
        let sb = (dealer_idx + 1) % n;
        let bb = (dealer_idx + 2) % n;
        (sb, bb)
    }
}
