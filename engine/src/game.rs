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

/// Orchestrates a complete Texas Hold'em hand.
#[derive(Clone)]
pub struct GameHand {
    seats: Vec<HandSeat>,
    dealer_idx: usize,
    big_blind: Chips,
    small_blind: Chips,
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

impl GameHand {
    /// Production constructor using OS-random seed.
    pub fn new(
        players: Vec<(PlayerId, Chips, u8)>,
        dealer_idx: usize,
        big_blind: Chips,
        small_blind: Chips,
    ) -> Self {
        let rng = PokerRng::from_os();
        Self::new_with_rng(players, dealer_idx, big_blind, small_blind, rng)
    }

    /// Test constructor with injected deterministic RNG.
    pub fn new_with_rng(
        players: Vec<(PlayerId, Chips, u8)>,
        dealer_idx: usize,
        big_blind: Chips,
        small_blind: Chips,
        mut rng: PokerRng,
    ) -> Self {
        let deck = Deck::new(&mut rng);
        let seed = rng.seed();
        Self::new_with_deck(players, dealer_idx, big_blind, small_blind, deck, seed)
    }

    /// Construct a hand from a deck produced by the dealing abstraction.
    ///
    /// This is the constructor the `DealingProvider` path uses: the deck order
    /// is decided outside the engine (legacy server shuffle, or a verified
    /// Mental Poker protocol run) and handed in via [`Deck::from_cards`]. The
    /// engine no longer depends on a server-side RNG to deal a hand.
    ///
    /// `deck_seed` is recorded on the `HandResult` for hand-history
    /// reproducibility; for Mental Poker it is a transcript-derived value
    /// rather than an RNG seed.
    pub fn new_with_deck(
        players: Vec<(PlayerId, Chips, u8)>,
        dealer_idx: usize,
        big_blind: Chips,
        small_blind: Chips,
        deck: Deck,
        deck_seed: DeckSeed,
    ) -> Self {
        let seed = deck_seed;
        let seats = players
            .into_iter()
            .map(|(player_id, stack, seat)| HandSeat {
                player_id,
                starting_stack: stack,
                stack,
                hole: HoleSlot::Empty,
                folded: false,
                all_in: false,
                seat,
            })
            .collect();
        Self {
            seats,
            dealer_idx,
            big_blind,
            small_blind,
            mode: HandMode::Plaintext,
            deck,
            deck_seed: seed,
            board: BoardCards::empty(),
            phase: Phase::NotStarted,
            betting_round: None,
            completed_pot: Chips::ZERO,
            cumulative_side_pots: vec![],
            actions: vec![],
            action_seq: 0,
            last_action: HashMap::new(),
            events: vec![],
            pending_board_street: None,
            finished_street: None,
        }
    }

    /// Blind-mode constructor (ADR-066 P1).
    ///
    /// Takes **no** `Deck`, shuffles nothing, and deals **no** hole cards. Seats
    /// begin with [`HoleSlot::Opaque`] — the engine knows each seat has two
    /// cards but not their value. Blinds, positions, and stacks are set up
    /// exactly as the normal constructor; only the card source differs.
    ///
    /// `deck_seed` is a transcript-derived identifier recorded on the
    /// `HandResult` for hand-history reproducibility (it is **not** an RNG seed
    /// and reveals nothing about hole values). [`DeckSeed`] is a `[u8; 32]`, so
    /// it is fine to pass a default all-zero seed when no transcript identifier
    /// is available.
    ///
    /// After construction:
    /// - [`GameHand::start`] posts blinds and begins preflop betting but deals
    ///   no hole cards and emits no `HoleCardsDealt` event at all (no plaintext
    ///   value, and no redacted variant either — blind start emits nothing for
    ///   the deal);
    /// - board cards are fed per street via [`GameHand::inject_board_for_street`];
    /// - showdown reveals are fed via [`GameHand::inject_showdown_reveal`].
    pub fn new_blind(
        players: Vec<(PlayerId, Chips, u8)>,
        dealer_idx: usize,
        big_blind: Chips,
        small_blind: Chips,
        deck_seed: DeckSeed,
    ) -> Self {
        let seats = players
            .into_iter()
            .map(|(player_id, stack, seat)| HandSeat {
                player_id,
                starting_stack: stack,
                stack,
                // Opaque, not Empty: a blind seat is dealt two cards whose value
                // the engine does not know — the type-level blind invariant.
                hole: HoleSlot::Opaque,
                folded: false,
                all_in: false,
                seat,
            })
            .collect();
        Self {
            seats,
            dealer_idx,
            big_blind,
            small_blind,
            mode: HandMode::Blind,
            // Empty deck: blind mode never deals from it. Any accidental
            // `self.deck.deal()` would error (HandFinished) rather than leak a
            // plaintext card — but the blind paths never call it.
            deck: Deck::from_cards(Vec::new()),
            deck_seed,
            board: BoardCards::empty(),
            phase: Phase::NotStarted,
            betting_round: None,
            completed_pot: Chips::ZERO,
            cumulative_side_pots: vec![],
            actions: vec![],
            action_seq: 0,
            last_action: HashMap::new(),
            events: vec![],
            pending_board_street: None,
            finished_street: None,
        }
    }

    /// Start the hand: post blinds, deal hole cards, begin preflop betting.
    pub fn start(&mut self) -> Result<GameSnapshot, ActionError> {
        if self.phase != Phase::NotStarted {
            return Err(ActionError::HandFinished);
        }
        // Every public chip/pot amount is u32. Reject an unrepresentable table
        // before blinds or pot math can overflow in debug or wrap in release.
        if self
            .seats
            .iter()
            .map(|seat| u64::from(seat.starting_stack.0))
            .sum::<u64>()
            > u64::from(u32::MAX)
        {
            return Err(ActionError::InvalidAction);
        }
        let n = self.seats.len();
        if n < 2 {
            return Err(ActionError::NotInHand);
        }

        // Determine blind positions.
        let (sb_idx, bb_idx) = blind_positions(self.dealer_idx, n);
        let sb_id = self.seats[sb_idx].player_id;
        let bb_id = self.seats[bb_idx].player_id;

        // Emit HandStarted as the FIRST event (ADR-024 §5.2, ADR-025 §4).
        // Must be emitted before any ActionApplied { Blind, .. } events.
        {
            // Bound the dealer index the same way `blind_positions` already
            // normalizes (`% n`) so an out-of-range `dealer_idx` wraps instead
            // of panicking with index-out-of-bounds (defensive hardening,
            // audit 2026-06-03).
            let dealer_seat = self.seats[self.dealer_idx % n].seat;
            let sb_seat = self.seats[sb_idx].seat;
            let bb_seat = self.seats[bb_idx].seat;
            self.events.push(EngineEvent::HandStarted {
                dealer_seat,
                sb_seat,
                bb_seat,
                big_blind: self.big_blind.0 as u64,
                small_blind: self.small_blind.0 as u64,
                deck_seed: self.deck_seed,
            });
        }

        // Record blind action metadata (using starting stacks).
        let sb_stack_start = self.seats[sb_idx].stack;
        let bb_stack_start = self.seats[bb_idx].stack;
        let sb_blind_amount = self.small_blind.0.min(sb_stack_start.0);
        let bb_blind_amount = self.big_blind.0.min(bb_stack_start.0);

        // Record SB action.
        // ADR-079 (F2): capture the blind seqs so the emitted ActionApplied events
        // carry the same engine-authoritative seq as the recorded ActionRecord
        // (SB = 0, BB = 1; first voluntary action = 2).
        let sb_action_seq = self.action_seq;
        self.actions.push(ActionRecord {
            seq: self.action_seq,
            street: Street::Preflop,
            player_id: sb_id,
            action: PlayerAction::Blind {
                kind: BlindKind::Small,
                amount: Chips(sb_blind_amount),
            },
            amount: Chips(sb_blind_amount),
            stack_before: sb_stack_start,
            stack_after: Chips(sb_stack_start.0 - sb_blind_amount),
            pot_before: Chips::ZERO,
            pot_after: Chips(sb_blind_amount),
        });
        self.last_action.insert(
            sb_id.inner(),
            PlayerAction::Blind {
                kind: BlindKind::Small,
                amount: Chips(sb_blind_amount),
            },
        );
        self.action_seq = self.action_seq.saturating_add(1); // SB seq=0 before BB seq=1; saturating (U33)

        // Record BB action.
        let bb_action_seq = self.action_seq;
        self.actions.push(ActionRecord {
            seq: self.action_seq,
            street: Street::Preflop,
            player_id: bb_id,
            action: PlayerAction::Blind {
                kind: BlindKind::Big,
                amount: Chips(bb_blind_amount),
            },
            amount: Chips(bb_blind_amount),
            stack_before: bb_stack_start,
            stack_after: Chips(bb_stack_start.0 - bb_blind_amount),
            pot_before: Chips(sb_blind_amount),
            pot_after: Chips(sb_blind_amount + bb_blind_amount),
        });
        self.last_action.insert(
            bb_id.inner(),
            PlayerAction::Blind {
                kind: BlindKind::Big,
                amount: Chips(bb_blind_amount),
            },
        );
        self.action_seq = self.action_seq.saturating_add(1); // saturating (U33)

        // Deal hole cards (two passes over all active seats).
        //
        // Blind mode (ADR-066 P1): deal NOTHING from the deck. Seats keep their
        // `HoleSlot::Opaque` holding from `new_blind` — the engine never holds a
        // plaintext hole value before a verified showdown reveal. (Skipping this
        // block is what makes "no plaintext before showdown" structural: there
        // is no blind code path that writes `Plain`.)
        if self.mode == HandMode::Plaintext {
            let n_seats = self.seats.len();
            let mut first_cards: Vec<crate::card::Card> = Vec::with_capacity(n_seats);
            let mut second_cards: Vec<crate::card::Card> = Vec::with_capacity(n_seats);
            for _ in 0..n_seats {
                first_cards.push(self.deck.deal().map_err(|_| ActionError::HandFinished)?);
            }
            for _ in 0..n_seats {
                second_cards.push(self.deck.deal().map_err(|_| ActionError::HandFinished)?);
            }
            for (i, seat) in self.seats.iter_mut().enumerate() {
                seat.hole = HoleSlot::Plain(HoleCards::new(first_cards[i], second_cards[i]));
            }
        }

        // Build the preflop betting round BEFORE emitting blind events so that
        // the post-blind current_bet / min_raise_to / next_actor are accurate.
        // Pass FULL stacks to the round; new_preflop handles blind deductions internally.
        let preflop_players: Vec<(PlayerId, Chips)> =
            self.seats.iter().map(|s| (s.player_id, s.stack)).collect();

        // UTG acts first preflop; seats and round players are in the same order,
        // so the seat index is also the round player index.
        let first_actor_in_round = (bb_idx + 1) % n;

        let blinds = vec![
            (sb_id, Chips(sb_blind_amount)),
            (bb_id, Chips(bb_blind_amount)),
        ];

        let round = BettingRound::new_preflop(
            preflop_players,
            first_actor_in_round,
            &blinds,
            self.big_blind,
        );

        self.phase = Phase::Betting(Street::Preflop);
        self.betting_round = Some(round);

        // Sync seat stacks from round (blinds have been applied inside round).
        self.sync_stacks_from_round();

        // Now that the round is constructed, read post-blind betting state for
        // the blind ActionApplied events.  Both blind events carry the SAME
        // post-round state (the final state after both blinds are posted).
        let post_blind_current_bet = self
            .betting_round
            .as_ref()
            .map(|r| r.current_bet().0 as u64)
            .unwrap_or(0);
        let post_blind_min_raise_to = self
            .betting_round
            .as_ref()
            .map(|r| r.min_raise_to().0 as u64);
        let post_blind_next_actor_seat = self.betting_round.as_ref().and_then(|r| {
            r.current_player().and_then(|pid| {
                self.seats
                    .iter()
                    .find(|s| s.player_id == pid)
                    .map(|s| s.seat)
            })
        });

        // Emit SB blind event.
        let sb_seat = self.seats[sb_idx].seat;
        self.events.push(EngineEvent::ActionApplied {
            seat: sb_seat,
            action: PlayerAction::Blind {
                kind: BlindKind::Small,
                amount: Chips(sb_blind_amount),
            },
            contributed: sb_blind_amount as u64,
            current_bet: post_blind_current_bet,
            min_raise_to: post_blind_min_raise_to,
            next_actor_seat: post_blind_next_actor_seat,
            action_seq: sb_action_seq,
        });

        // Emit BB blind event.
        let bb_seat = self.seats[bb_idx].seat;
        self.events.push(EngineEvent::ActionApplied {
            seat: bb_seat,
            action: PlayerAction::Blind {
                kind: BlindKind::Big,
                amount: Chips(bb_blind_amount),
            },
            contributed: bb_blind_amount as u64,
            current_bet: post_blind_current_bet,
            min_raise_to: post_blind_min_raise_to,
            next_actor_seat: post_blind_next_actor_seat,
            action_seq: bb_action_seq,
        });

        // Emit PotUpdated after both blinds are posted.
        // Blinds are never all-in for normal stacks; compute accurately anyway.
        let total_blind_pot = sb_blind_amount as u64 + bb_blind_amount as u64;
        let has_all_in_after_blinds = self.seats.iter().any(|s| s.all_in);
        self.events.push(EngineEvent::PotUpdated {
            pot: total_blind_pot,
            side_pots: vec![],
            has_all_in: has_all_in_after_blinds,
        });

        // Emit HoleCardsDealt events for each seat in seat order.
        //
        // Plaintext mode: cards are unredacted here — server is responsible for
        // per-client filtering.
        //
        // Blind mode (ADR-066 §2 step 3): emit NOTHING. The engine never
        // originates or broadcasts a plaintext hole value, and we deliberately
        // do NOT introduce a redacted event variant (the contract allows "or
        // none") so the not-yet-blind-aware server needs no changes. A client
        // learns its own cards out-of-band via the threshold-decrypt protocol.
        if self.mode == HandMode::Plaintext {
            for seat in &self.seats {
                if let Some(hole) = seat.hole_cards() {
                    self.events.push(EngineEvent::HoleCardsDealt {
                        seat: seat.seat,
                        cards: hole.as_array(),
                    });
                }
            }
        }

        if self
            .betting_round
            .as_ref()
            .map(|r| r.is_done())
            .unwrap_or(false)
        {
            return self.close_street_and_advance();
        }

        Ok(self.snapshot())
    }

    /// Apply an action from the current player.
    ///
    /// This is the pure preflight companion to [`Self::apply_action`]. It runs
    /// the complete betting-round legality check on a clone and therefore does
    /// not move chips, allocate an action sequence, append history, or emit an
    /// event. A single-owner coordinator can use it to finish every fallible
    /// protocol/chain check before the real mutation.
    pub fn validate_action(
        &self,
        player_id: PlayerId,
        action: &PlayerAction,
    ) -> Result<(), ActionError> {
        self.preview_action_transition(player_id, action)
            .map(|_| ())
    }

    /// Preview the exact public pre/post betting state without mutating the hand.
    pub fn preview_action_transition(
        &self,
        player_id: PlayerId,
        action: &PlayerAction,
    ) -> Result<ActionTransitionPreview, ActionError> {
        match &self.phase {
            Phase::NotStarted => return Err(ActionError::HandNotStarted),
            Phase::Done => return Err(ActionError::HandFinished),
            Phase::Betting(_) => {}
        }
        if !self.seats.iter().any(|seat| seat.player_id == player_id) {
            return Err(ActionError::NotInHand);
        }
        let mut round_after_action = self
            .betting_round
            .clone()
            .ok_or(ActionError::HandFinished)?;
        let before = round_after_action
            .player_states()
            .iter()
            .find(|player| player.player_id == player_id)
            .ok_or(ActionError::NotInHand)?;
        let stack_before = before.stack;
        let committed_before = before.contributed;
        let current_bet_before = round_after_action.current_bet();
        let min_raise_to_before = round_after_action
            .current_player_can_raise()
            .then(|| round_after_action.min_raise_to());

        round_after_action.apply_action(player_id, action)?;
        // This flag intentionally describes the round in which the action was
        // taken. All other post fields below come from the fully stabilized
        // hand (which may already contain the next street's betting round).
        let round_closed_after = round_after_action.is_done();

        let mut hand_after = self.clone();
        hand_after.apply_action(player_id, action.clone())?;
        let after_snapshot = hand_after.snapshot();
        let after = after_snapshot
            .players
            .iter()
            .find(|player| player.player_id == player_id)
            .expect("previewed player remains in hand snapshot");
        let next_actor = after_snapshot.current_actor;
        let next_actor_can_raise_after = next_actor.is_some()
            && hand_after
                .betting_round_ref()
                .is_some_and(BettingRound::current_player_can_raise);
        let betting_terminal_after = hand_after.is_done()
            || hand_after
                .betting_round_ref()
                .is_none_or(BettingRound::is_done);
        Ok(ActionTransitionPreview {
            stack_before,
            committed_before,
            current_bet_before,
            min_raise_to_before,
            stack_after: after.stack,
            committed_after: after.committed_this_street,
            current_bet_after: after_snapshot.current_bet,
            min_raise_to_after: after_snapshot.min_raise_to,
            all_in_after: after.all_in,
            round_closed_after,
            next_actor,
            next_actor_can_raise_after,
            street_after: after_snapshot.street,
            betting_terminal_after,
            hand_done_after: hand_after.is_done(),
        })
    }

    /// Apply an action from the current player.
    pub fn apply_action(
        &mut self,
        player_id: PlayerId,
        action: PlayerAction,
    ) -> Result<GameSnapshot, ActionError> {
        match &self.phase {
            Phase::NotStarted => return Err(ActionError::HandNotStarted),
            Phase::Done => return Err(ActionError::HandFinished),
            Phase::Betting(_) => {}
        }

        let current_street = self.current_street();
        let action_seq = self.action_seq;

        // Capture pre-action values.
        let stack_before = self
            .seats
            .iter()
            .find(|s| s.player_id == player_id)
            .map(|s| s.stack)
            .ok_or(ActionError::NotInHand)?;

        let pot_before = {
            let round_pot = self
                .betting_round
                .as_ref()
                .map(|r| r.pot_total())
                .unwrap_or(Chips::ZERO);
            Chips(self.completed_pot.0 + round_pot.0)
        };

        // Normalize a zero-chip `Call` (nothing to call) to `Check` for the
        // recorded/emitted label: it is the functionally-equivalent legal action
        // (0 chips move either way, same state transition), and recording it as a
        // `call` would pollute action history / coach inputs with an illegal
        // zero-chip call (C6 review). The applied state transition is unchanged.
        let recorded_action = {
            let to_call = self
                .betting_round
                .as_ref()
                .map(|r| r.to_call(player_id))
                .unwrap_or(Chips::ZERO);
            if matches!(action, PlayerAction::Call) && to_call.0 == 0 {
                PlayerAction::Check
            } else {
                action.clone()
            }
        };

        // Apply action.
        let round = self
            .betting_round
            .as_mut()
            .ok_or(ActionError::HandFinished)?;
        let chips_moved = round.apply_action(player_id, &action)?;

        let stack_after = Chips(stack_before.0.saturating_sub(chips_moved.0));
        let pot_after = Chips(pot_before.0 + chips_moved.0);
        let round_done = round.is_done();
        let player_folded = round.player_folded(player_id);
        let player_all_in = round.player_all_in(player_id);

        // Sync seat state.
        for seat in &mut self.seats {
            if seat.player_id == player_id {
                seat.stack = stack_after;
                seat.folded = player_folded;
                seat.all_in = player_all_in;
            }
        }

        // Record the action (zero-chip Call already relabelled to Check above).
        self.actions.push(ActionRecord {
            seq: action_seq,
            street: current_street,
            player_id,
            action: recorded_action.clone(),
            amount: chips_moved,
            stack_before,
            stack_after,
            pot_before,
            pot_after,
        });
        self.action_seq = self.action_seq.saturating_add(1); // saturating (U33)
        self.last_action
            .insert(player_id.inner(), recorded_action.clone());

        // Emit ActionApplied event.
        // SAFETY: player_id was validated above by the stack_before lookup (ok_or NotInHand?),
        // so a seat must exist here. Using expect instead of unwrap_or to surface any
        // future refactor that breaks this invariant loudly.
        let acting_seat = self
            .seats
            .iter()
            .find(|s| s.player_id == player_id)
            .map(|s| s.seat)
            .expect("player_id was validated above; seat must exist");
        // Compute post-action betting state for the ActionApplied event.
        // These fields let the server stream live betting state to clients
        // without waiting for a full Snapshot round-trip.
        // NOTE: if round_done is true the street is about to close; fields are
        // None / 0 for that final event, which is correct — no one can act.
        let action_current_bet = self
            .betting_round
            .as_ref()
            .map(|r| r.current_bet().0 as u64)
            .unwrap_or(0);
        // Suppress min_raise_to when the next actor cannot legally raise —
        // i.e. after a non-reopening (sub-minimum) all-in they may only call or
        // fold (TDA Rule 6). Advertising a min_raise_to there implied a raise
        // the engine would then reject (audit 2026-06-03).
        let action_min_raise_to = if round_done {
            None
        } else {
            self.betting_round.as_ref().and_then(|r| {
                if r.current_player_can_raise() {
                    Some(r.min_raise_to().0 as u64)
                } else {
                    None
                }
            })
        };
        let action_next_actor_seat = if round_done {
            None
        } else {
            self.betting_round.as_ref().and_then(|r| {
                r.current_player().and_then(|pid| {
                    self.seats
                        .iter()
                        .find(|s| s.player_id == pid)
                        .map(|s| s.seat)
                })
            })
        };
        self.events.push(EngineEvent::ActionApplied {
            seat: acting_seat,
            action: recorded_action,
            contributed: chips_moved.0 as u64,
            current_bet: action_current_bet,
            min_raise_to: action_min_raise_to,
            next_actor_seat: action_next_actor_seat,
            // ADR-079 (F2): the engine seq captured at function entry (line ~750),
            // identical to the recorded `ActionRecord.seq` for this action.
            action_seq,
        });

        // Emit PotUpdated if chips moved (fold/check contribute 0).
        if chips_moved.0 > 0 {
            let current_round_pot = self
                .betting_round
                .as_ref()
                .map(|r| r.pot_total())
                .unwrap_or(Chips::ZERO);
            let total_pot = self.completed_pot.0 as u64 + current_round_pot.0 as u64;
            let side_pots = self.merged_side_pots();
            // Snapshot all-in state at emit time so the translate layer can apply
            // the has_all_in guard without needing access to full engine state.
            let has_all_in = self.seats.iter().any(|s| s.all_in);
            self.events.push(EngineEvent::PotUpdated {
                pot: total_pot,
                side_pots,
                has_all_in,
            });
        }

        if round_done {
            return self.close_street_and_advance();
        }

        Ok(self.snapshot())
    }

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

    /// Produce the final result and emit a [`EngineEvent::HandFinished`] event.
    ///
    /// Plaintext mode only. For blind-mode hands call [`GameHand::finish_blind`]
    /// instead — `finish` would panic on the `assert` below if any non-folded
    /// contender's value were missing.
    ///
    /// # Panics
    ///
    /// Panics if [`is_done()`] is not true. Callers must check [`is_done()`]
    /// before calling. Also panics if called on a blind-mode hand (use
    /// [`GameHand::finish_blind`]).
    pub fn finish(&mut self) -> HandResult {
        assert!(self.phase == Phase::Done, "hand not finished");
        assert!(
            self.mode == HandMode::Plaintext,
            "finish() is plaintext-only; blind-mode hands must use finish_blind()"
        );

        // Players still in (non-folded) with plaintext hole cards. In plaintext
        // mode every non-folded seat holds `Plain` after `start()`.
        let eligible: Vec<(PlayerId, HoleCards)> = self
            .seats
            .iter()
            .filter(|s| !s.folded)
            .filter_map(|s| s.hole_cards().map(|h| (s.player_id, h)))
            .collect();

        // Build a normal contested/lone showdown (plaintext path is unchanged).
        let showdown = self.build_plaintext_showdown(&eligible);
        self.assemble_result(eligible, showdown)
    }

    /// Produce the final result for a **blind-mode** hand (ADR-066 P1).
    ///
    /// Distribution and showdown use **only** injected, verified reveals:
    ///
    /// - **Uncontested (`non-folded contenders <= 1`)** — there is no showdown
    ///   and **no reveal is required**. The lone survivor (if any) takes the pot
    ///   via the card-free single-contender path; the showdown is **empty**.
    ///   `rank_hand` is never called on the lone survivor, so "uncontested =
    ///   zero leak" is structural, not a runtime check.
    /// - **Contested (`>= 2`)** — every non-folded contender MUST have an
    ///   injected [`HoleSlot::Revealed`] (fed by [`GameHand::inject_showdown_reveal`]).
    ///   Ranking uses only those reveals. If any non-folded contender is still
    ///   `Opaque` (or holds any non-`Revealed` slot), this returns
    ///   [`BlindFinishError::MissingReveal`] — it never panics and never invents
    ///   cards.
    ///
    /// # Errors
    ///
    /// Never panics. Returns [`BlindFinishError::NotDone`] if the hand has not
    /// reached `Phase::Done`, [`BlindFinishError::NotBlindMode`] if called on a
    /// plaintext hand (use [`GameHand::finish`]), or
    /// [`BlindFinishError::MissingReveal`] if a contested contender lacks a
    /// verified reveal — in every case the server can VOID the hand rather than
    /// crash the room.
    pub fn finish_blind(&mut self) -> Result<HandResult, BlindFinishError> {
        // Preconditions return typed errors (never panic) so the server can VOID
        // the hand rather than crash the room (ADR-066 — blind mode never panics).
        if self.phase != Phase::Done {
            return Err(BlindFinishError::NotDone);
        }
        if self.mode != HandMode::Blind {
            return Err(BlindFinishError::NotBlindMode);
        }

        let non_folded: Vec<&HandSeat> = self.seats.iter().filter(|s| !s.folded).collect();

        if non_folded.len() <= 1 {
            // Uncontested: structural zero-leak. No reveal needed, no rank_hand.
            // Award the lone survivor (if any) via the card-free path and emit
            // an EMPTY showdown.
            let lone: Option<PlayerId> = non_folded.first().map(|s| s.player_id);
            let result = self.assemble_blind_uncontested_result(lone);
            self.events.push(EngineEvent::HandFinished {
                result: result.clone(),
            });
            return Ok(result);
        }

        // Contested: every non-folded contender must have a verified reveal.
        // The blind contested rank path consumes ONLY injected `Revealed` slots
        // — a stray `Plain` (which blind mode never writes; unreachable in prod)
        // is treated as MISSING, not as a reveal, so ranking can never feed on
        // anything but an externally-verified showdown reveal (ADR-066 §2). This
        // hardens the "ranking uses only injected `Revealed`" invariant against
        // an out-of-protocol `Plain`/`Opaque`/`Empty` seat.
        let mut eligible: Vec<(PlayerId, HoleCards)> = Vec::with_capacity(non_folded.len());
        for seat in &non_folded {
            match seat.hole {
                HoleSlot::Revealed(h) => eligible.push((seat.player_id, h)),
                HoleSlot::Plain(_) | HoleSlot::Opaque | HoleSlot::Empty => {
                    // No verified reveal for a non-folded contender at a contested
                    // showdown. Fail loudly with a typed error; never invent
                    // cards, never panic.
                    return Err(BlindFinishError::MissingReveal {
                        player_id: seat.player_id,
                        seat: seat.seat,
                    });
                }
            }
        }

        let showdown = self.build_plaintext_showdown(&eligible);
        let result = self.assemble_result(eligible, showdown);
        Ok(result)
    }

    /// Produce the final result for a **blind-mode** hand, treating any non-folded
    /// contender that lacks a verified reveal as a **FORFEIT** (ADR-066 §5 T-LIVE
    /// forfeit-not-refund) rather than a hard error.
    ///
    /// This is the contested-showdown sibling of [`GameHand::finish_blind`] for the
    /// server-blind choreography: a contender that withholds, disconnects, or
    /// supplies a value that failed `verify_and_open` (so the server never injected
    /// a `Revealed` slot for it) is **excluded from the eligible set**. Its already-
    /// committed chips stay in the pots (the side pots were absorbed at street
    /// close and are unchanged), so they are awarded to the contenders who DID
    /// reveal — the cardroom "won't table a called hand → muck and lose" rule.
    /// This makes stalling strictly −EV (no stake-return for a non-revealer).
    ///
    /// Difference from [`GameHand::finish_blind`]:
    /// - **Uncontested** (`non-folded <= 1`): identical — structural zero-leak,
    ///   no reveal needed, no `rank_hand`.
    /// - **Contested** (`>= 2`): the eligible set is **only** the non-folded
    ///   contenders with an injected [`HoleSlot::Revealed`]. Non-revealed
    ///   contenders are dropped (forfeit). If **no** contender revealed (the
    ///   blameless quorum-wide failure — there is no honest revealer to award the
    ///   pot to), this returns [`BlindFinishError::MissingReveal`] for the first
    ///   such contender so the server VOIDs the hand with a stake-return rather
    ///   than awarding chips to no one.
    ///
    /// # Errors
    ///
    /// Same typed errors as [`GameHand::finish_blind`]; never panics, never
    /// invents cards.
    pub fn finish_blind_with_forfeits(&mut self) -> Result<HandResult, BlindFinishError> {
        if self.phase != Phase::Done {
            return Err(BlindFinishError::NotDone);
        }
        if self.mode != HandMode::Blind {
            return Err(BlindFinishError::NotBlindMode);
        }

        let non_folded: Vec<&HandSeat> = self.seats.iter().filter(|s| !s.folded).collect();

        if non_folded.len() <= 1 {
            // Uncontested: structural zero-leak, identical to `finish_blind`.
            let lone: Option<PlayerId> = non_folded.first().map(|s| s.player_id);
            let result = self.assemble_blind_uncontested_result(lone);
            self.events.push(EngineEvent::HandFinished {
                result: result.clone(),
            });
            return Ok(result);
        }

        // Contested: eligible = non-folded contenders WITH a verified reveal.
        // Non-revealed contenders forfeit (excluded). Only `Revealed` slots are
        // ranked — a stray `Plain`/`Opaque`/`Empty` is a forfeit, never invented.
        let mut eligible: Vec<(PlayerId, HoleCards)> = Vec::with_capacity(non_folded.len());
        let mut first_unrevealed: Option<(PlayerId, u8)> = None;
        // The non-folded contenders who did NOT reveal — forfeiters. A side pot
        // whose eligible set is entirely forfeiters must NOT be refunded to them
        // (ADR-066 §5 forfeit-not-refund / audit F3); its chips go to the honest
        // revealers. Tracked here so `assemble_result` can distinguish a forfeiter
        // (non-folded, no reveal) from a genuinely folded "no contest" refund.
        let mut forfeiters: HashSet<u64> = HashSet::new();
        for seat in &non_folded {
            match seat.hole {
                HoleSlot::Revealed(h) => eligible.push((seat.player_id, h)),
                HoleSlot::Plain(_) | HoleSlot::Opaque | HoleSlot::Empty => {
                    if first_unrevealed.is_none() {
                        first_unrevealed = Some((seat.player_id, seat.seat));
                    }
                    forfeiters.insert(seat.player_id.inner());
                }
            }
        }

        if eligible.is_empty() {
            // Blameless quorum-wide failure: not a single contender revealed, so
            // there is no honest revealer to award the pot to. Surface
            // `MissingReveal` so the server VOIDs (stake-return), never awards.
            let (player_id, seat) = first_unrevealed.expect("non_folded.len() >= 2 ⇒ some seat");
            return Err(BlindFinishError::MissingReveal { player_id, seat });
        }

        let showdown = self.build_plaintext_showdown(&eligible);
        let result = self.assemble_result_with_forfeits(eligible, showdown, &forfeiters);
        Ok(result)
    }

    /// Resolve a blind-mode hand whose **board cannot be revealed** because a
    /// non-folded contender withheld (or aborted) a required board decryption
    /// share (audit LIVE-F1). The named `withholders` — each an **attributable**
    /// non-folded contender that failed to contribute its board share — are
    /// **force-folded** (so they FORFEIT their already-committed chips), and the
    /// pot is awarded to the remaining honest contenders. This makes withholding a
    /// required board share strictly −EV for the withholder (it can NEVER recover
    /// its all-in stake by stalling the board), closing the abort/withhold→refund
    /// hole the void path left open.
    ///
    /// Forfeit-not-refund: the board is interleaved AFTER betting, so by the
    /// turn/river a contender may be all-in for real chips. A blameless
    /// VOID + stake-return would hand that all-in stake BACK to the withholder
    /// (`+EV` to stall). Instead the withholder's committed chips stay in the pots
    /// and flow to the honest survivors.
    ///
    /// Resolution among survivors (the honest n-of-n residual, ADR-066 §5):
    /// - **One honest survivor**: it takes the whole pot (card-free, uncontested) —
    ///   the realistic 2-contender all-in case.
    /// - **Multiple honest survivors**: the board never came, so the engine cannot
    ///   rank them on merit — it **splits the contested pots equally** among the
    ///   honest survivors (TDA-25 odd-chip placement). They each get their own
    ///   committed chips back plus an equal share of the forfeiter's stake. Full
    ///   *fair* ranking under a withholder needs a (t,n) threshold scheme and is
    ///   deferred (P5/future); an equal chop is the chip-conserving, withholder-
    ///   forfeiting interim that never returns a withholder's matched stake.
    ///
    /// Uncalled-overage layers (`refund_to`) are ALWAYS returned to their owner —
    /// even a withholder's — because an uncalled bet was never matched (no one
    /// could win it), so returning it is not a +EV exploit of the withhold.
    ///
    /// # Errors
    /// - [`BlindFinishError::NotBlindMode`] in plaintext mode.
    /// - [`BlindFinishError::NoSurvivor`] if force-folding every named withholder
    ///   leaves no non-folded contender (coordinator misuse) — the server must
    ///   VOID with a blameless stake-return rather than award chips to no one.
    ///
    /// Never panics, never invents cards (the board is never recovered here — the
    /// award is card-free: a lone survivor or an equal chop).
    pub fn finish_blind_forfeit_board(
        &mut self,
        withholders: &[PlayerId],
    ) -> Result<HandResult, BlindFinishError> {
        if self.mode != HandMode::Blind {
            return Err(BlindFinishError::NotBlindMode);
        }

        // Absorb any still-open betting round into the cumulative pots before we
        // resolve, so no committed chip is stranded (mirrors
        // `close_street_and_advance`). At a `pending_board_street` the next
        // street's round exists with zero new contributions in an all-in run-out,
        // so this typically absorbs 0 — but doing it unconditionally is the
        // chip-safe move regardless of where the withhold landed.
        if let Some(round) = self.betting_round.take() {
            let new_pots = round.side_pots();
            let round_total = new_pots.iter().map(|p| p.amount.0).sum::<u32>();
            self.completed_pot.0 += round_total;
            self.accumulate_pots(new_pots);
        }
        // Capture entitlement before force-folding withholders. Their matched
        // stake is forfeited to honest survivors, not reclassified as a normal
        // folded-player refund.
        let settlement_pots = self.settlement_side_pots();

        // Force-fold each attributable withholder (only a currently non-folded
        // seat — a folded/unknown/duplicate id is ignored, never an error: the
        // coordinator may pass a superset and we forfeit only the live contenders).
        for pid in withholders {
            if let Some(seat) = self
                .seats
                .iter_mut()
                .find(|s| s.player_id == *pid && !s.folded)
            {
                seat.folded = true;
                seat.all_in = false; // a forfeiter is out of the hand
            }
        }

        // The board is unrecoverable → the hand ends now (no more streets).
        self.pending_board_street = None;
        self.set_done();

        let survivors: Vec<PlayerId> = self
            .seats
            .iter()
            .filter(|s| !s.folded)
            .map(|s| s.player_id)
            .collect();
        if survivors.is_empty() {
            // Every contender was force-folded — there is no honest survivor to
            // award the pot to. Surface a typed error so the server VOIDs with a
            // blameless stake-return rather than inventing a winner.
            return Err(BlindFinishError::NoSurvivor);
        }

        let button_order = self.button_relative_order();
        let pot_results =
            distribute_pots_among_survivors(&settlement_pots, &survivors, &button_order);
        // The board was not recovered, so this is a chip settlement without a
        // showdown. Keep the result card-free: `show_order` is the protocol's
        // explicit signal that no cards/rank should be exposed.
        let result = self.finalize_pots_with_show_order(pot_results, Vec::new(), Vec::new());
        self.events.push(EngineEvent::HandFinished {
            result: result.clone(),
        });
        Ok(result)
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

    /// Build the showdown vector from a non-empty eligible set (shared by the
    /// plaintext `finish` and the contested blind branch). Mirrors the original
    /// `finish` showdown logic exactly so plaintext output is byte-for-byte
    /// unchanged.
    fn build_plaintext_showdown(&self, eligible: &[(PlayerId, HoleCards)]) -> Vec<ShowdownEntry> {
        if eligible.len() > 1 {
            let ranked = rank_players(eligible, &self.board);
            ranked
                .into_iter()
                .map(|(pid, rank)| {
                    let hole = eligible
                        .iter()
                        .find(|(p, _)| *p == pid)
                        .map(|(_, h)| *h)
                        .unwrap(); // safe: pid is from eligible
                    ShowdownEntry {
                        player_id: pid,
                        hole_cards: hole,
                        rank_name: rank.name().to_string(),
                        rank,
                    }
                })
                .collect()
        } else {
            // Single-eligible (fold-around) entry: fill `rank_name` from the
            // computed rank so the wire shape matches the contested branch
            // instead of carrying a real `rank` next to an empty `rank_name`
            // (audit 2026-06-03).
            eligible
                .iter()
                .map(|(pid, hole)| {
                    let rank = crate::eval::rank_hand(hole, &self.board);
                    ShowdownEntry {
                        player_id: *pid,
                        hole_cards: *hole,
                        rank,
                        rank_name: rank.name().to_string(),
                    }
                })
                .collect()
        }
    }

    /// Distribute pots from a plaintext `eligible` set and assemble the final
    /// `HandResult` + chip-conservation check. Shared by `finish` and the
    /// contested blind branch.
    ///
    /// The plaintext (non-blind) path has no forfeiters — every non-folded seat
    /// either reaches showdown with cards or folded — so it delegates with an
    /// empty forfeiter set, preserving its output byte-for-byte.
    fn assemble_result(
        &mut self,
        eligible: Vec<(PlayerId, HoleCards)>,
        showdown: Vec<ShowdownEntry>,
    ) -> HandResult {
        self.assemble_result_with_forfeits(eligible, showdown, &HashSet::new())
    }

    /// Forfeit-aware result assembly (ADR-066 §5 / audit F3). Identical to
    /// [`Self::assemble_result`] except it carries the set of **forfeiters** —
    /// non-folded contenders that did NOT reveal — so a side pot whose eligible
    /// set is entirely forfeiters is awarded to the honest revealers rather than
    /// refunded to the forfeiters. An empty `forfeiters` set reproduces the
    /// plaintext path exactly.
    fn assemble_result_with_forfeits(
        &mut self,
        eligible: Vec<(PlayerId, HoleCards)>,
        showdown: Vec<ShowdownEntry>,
        forfeiters: &HashSet<u64>,
    ) -> HandResult {
        // TDA Rule 25 odd-chip placement: the remainder of a split pot goes to
        // the tied winner in the earliest position — the first seat left of the
        // button. Build a `PlayerId -> button-relative order` map (0 = first seat
        // clockwise after the button) so `distribute_pots` can place the odd
        // chip deterministically by table position rather than by pot.eligible
        // (action) order (audit 2026-06-03).
        let button_order = self.button_relative_order();

        let settlement_pots = self.settlement_side_pots();
        let pot_results = distribute_pots(
            &settlement_pots,
            &eligible,
            &self.board,
            &button_order,
            forfeiters,
        );

        let result = self.finalize_pots(pot_results, showdown);

        // Emit HandFinished event.
        self.events.push(EngineEvent::HandFinished {
            result: result.clone(),
        });

        result
    }

    /// Assemble a blind-mode **uncontested** result: award the lone survivor (if
    /// any) the entire pot via a card-free single-contender path, with an EMPTY
    /// showdown. No `HoleCards` are ever constructed — uncontested = structural
    /// zero-leak. `rank_hand` is not called.
    fn assemble_blind_uncontested_result(&self, lone: Option<PlayerId>) -> HandResult {
        let button_order = self.button_relative_order();
        let settlement_pots = self.settlement_side_pots();
        let pot_results = distribute_pots_uncontested(&settlement_pots, lone, &button_order);
        self.finalize_pots(pot_results, Vec::new())
    }

    /// Shared tail of result assembly: chip-conservation check, `chips_awarded`,
    /// `final_stacks`, `show_order`, and the `HandResult` struct. Does NOT emit
    /// the `HandFinished` event (callers decide whether to emit).
    fn finalize_pots(
        &self,
        pot_results: Vec<PotResult>,
        showdown: Vec<ShowdownEntry>,
    ) -> HandResult {
        self.finalize_pots_with_show_order(pot_results, showdown, self.compute_show_order())
    }

    fn finalize_pots_with_show_order(
        &self,
        pot_results: Vec<PotResult>,
        showdown: Vec<ShowdownEntry>,
        show_order: Vec<PlayerId>,
    ) -> HandResult {
        // chips_awarded: sum from all pots.
        let mut chips_awarded: HashMap<u64, u32> = self
            .seats
            .iter()
            .map(|s| (s.player_id.inner(), 0u32))
            .collect();
        for pr in &pot_results {
            for w in &pr.winners {
                *chips_awarded.entry(w.player_id.inner()).or_default() += w.amount_won.0;
            }
        }

        // Invariant: every chip in every pot is awarded or refunded — never
        // destroyed. A future regression that drops a pot (e.g. the orphaned
        // side-pot bug fixed 2026-05-29 #5) panics here instead of silently
        // leaking chips in production.
        //
        // backend review F-ENG-7: this is a deliberate always-on failsafe, so
        // it uses `assert_eq!` (active in release) rather than `debug_assert_eq!`
        // (compiled out of `--release`). The invariant is already proven by the
        // engine property tests (`prop_chip_conservation`,
        // `prop_side_pot_multistreet`, `orphaned_side_pot_does_not_destroy_chips`)
        // so it never fires in practice; and per the architecture a panic that
        // kills a single room is strictly preferable to silently corrupting the
        // persisted stacks of a distribution regression. (A graceful Result return
        // would change this fn's signature across many call sites — out of scope.)
        for pot in &pot_results {
            assert_eq!(
                pot.winners
                    .iter()
                    .map(|winner| u64::from(winner.amount_won.0))
                    .sum::<u64>(),
                u64::from(pot.amount.0),
                "every pot must award or refund its full amount"
            );
        }
        let total_awarded = chips_awarded
            .values()
            .map(|amount| u64::from(*amount))
            .sum::<u64>();
        let result_pot_total = pot_results
            .iter()
            .map(|pot| u64::from(pot.amount.0))
            .sum::<u64>();
        let legacy_pot_total = self
            .cumulative_side_pots
            .iter()
            .map(|pot| u64::from(pot.amount.0))
            .sum::<u64>();
        let total_contributed =
            self.seats
                .iter()
                .map(|seat| {
                    u64::from(
                        seat.starting_stack.0.checked_sub(seat.stack.0).expect(
                            "in-hand stack cannot exceed its starting stack before settlement",
                        ),
                    )
                })
                .sum::<u64>();
        assert_eq!(
            legacy_pot_total, total_contributed,
            "street bookkeeping must contain every committed chip"
        );
        assert_eq!(
            result_pot_total, total_contributed,
            "canonical pots must equal the whole hand's committed chips"
        );
        assert_eq!(
            total_awarded, result_pot_total,
            "chip conservation: awards must equal canonical pots"
        );

        // final_stacks = current stack + chips awarded.
        let final_stacks: HashMap<u64, u32> = self
            .seats
            .iter()
            .map(|s| {
                let awarded = *chips_awarded.get(&s.player_id.inner()).unwrap_or(&0);
                (
                    s.player_id.inner(),
                    s.stack
                        .0
                        .checked_add(awarded)
                        .expect("settled stack must fit the public u32 chip range"),
                )
            })
            .collect();
        assert_eq!(
            final_stacks
                .values()
                .map(|stack| u64::from(*stack))
                .sum::<u64>(),
            self.seats
                .iter()
                .map(|seat| u64::from(seat.starting_stack.0))
                .sum::<u64>(),
            "final stacks must equal starting stacks"
        );

        HandResult {
            deck_seed: self.deck_seed,
            board: self.board.clone(),
            pots: pot_results,
            chips_awarded,
            final_stacks,
            actions: self.actions.clone(),
            showdown,
            show_order,
        }
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

    /// Return the real community cards that were still undealt when this hand
    /// finished.
    ///
    /// Plaintext hands deal directly from `deck`, with no burn cards, so the
    /// next `5 - board.count()` cards are the only valid rabbit-hunt runout.
    /// This accessor is deliberately read-only and does not advance the deck.
    /// Blind hands return an empty vector because their coordinator owns the
    /// unopened deck and the server must never invent a continuation.
    pub fn undealt_board(&self) -> Vec<Card> {
        if self.mode != HandMode::Plaintext || !self.is_done() {
            return Vec::new();
        }
        let needed = 5usize.saturating_sub(self.board.count());
        if needed == 0 {
            return Vec::new();
        }
        let remaining = self.deck.remaining();
        let start = self.deck.cards().len().saturating_sub(remaining);
        self.deck
            .cards()
            .iter()
            .skip(start)
            .take(needed.min(remaining))
            .copied()
            .collect()
    }

    /// Read-only reference to the active betting round, if any.
    ///
    /// Used by the session layer to query `players_yet_to_act()` without
    /// duplicating the round-access pattern already inside `snapshot()`.
    pub fn betting_round_ref(&self) -> Option<&BettingRound> {
        self.betting_round.as_ref()
    }

    /// Returns all actions recorded on the **current street** as `(player_id, action)` pairs.
    ///
    /// Blind posts (`PlayerAction::Blind { .. }`) are excluded — they are not
    /// voluntary betting actions and must not be treated as aggression when
    /// building bot `DecisionContext.street_actions`.
    ///
    /// This is a raw compatibility view. `AllIn` and call-shaped `Raise` tokens
    /// are semantically ambiguous, so strategy, coach, and aggressor consumers
    /// must use [`Self::current_street_action_facts`] (or its strategy
    /// projection) instead of inferring aggression from this method.
    pub fn current_street_actions(&self) -> Vec<(PlayerId, PlayerAction)> {
        let current = self.current_street();
        self.actions
            .iter()
            .filter(|rec| {
                rec.street == current && !matches!(rec.action, PlayerAction::Blind { .. })
            })
            .map(|rec| (rec.player_id, rec.action.clone()))
            .collect()
    }

    /// Current-street actions with authoritative wager semantics.
    ///
    /// An `AllIn` token alone is ambiguous.  This projection distinguishes an
    /// all-in call/under-call, a short all-in raise, and a full raise by
    /// replaying only public commitment totals.  Preflop starts from the
    /// nominal big-blind bring-in even when the posted BB was short; later
    /// streets start from zero.  `last_full_raise` changes only after a full
    /// wager, exactly like [`BettingRound`].
    pub fn current_street_action_facts(&self) -> Vec<StreetActionFact> {
        let current_street = self.current_street();
        let mut current_bet = if current_street == Street::Preflop {
            self.big_blind.0
        } else {
            0
        };
        let mut last_full_raise = self.big_blind.0;
        let mut committed_by_player: HashMap<u64, u32> = HashMap::new();
        let mut facts = Vec::new();

        for record in self
            .actions
            .iter()
            .filter(|record| record.street == current_street)
        {
            let committed = committed_by_player
                .entry(record.player_id.inner())
                .or_default();
            *committed = committed
                .checked_add(record.amount.0)
                .expect("one player's street commitment cannot exceed u32");

            if matches!(record.action, PlayerAction::Blind { .. }) {
                continue;
            }

            let aggressive_shape = matches!(
                record.action,
                PlayerAction::Raise { .. } | PlayerAction::AllIn
            );
            let increased_bet = aggressive_shape && *committed > current_bet;
            let raise_delta = (*committed).saturating_sub(current_bet);
            let full_raise = increased_bet && raise_delta >= last_full_raise;

            if increased_bet {
                current_bet = *committed;
            }
            if full_raise {
                last_full_raise = raise_delta;
            }

            facts.push(StreetActionFact {
                player_id: record.player_id,
                action: record.action.clone(),
                increased_bet,
                full_raise,
            });
        }

        facts
    }

    /// Strategy-facing current-street history.
    ///
    /// Full raises retain their `Raise` / `AllIn` shape.  A short raise or an
    /// all-in call becomes `Call`, while its real aggressor identity remains
    /// available from [`Self::current_street_action_facts`].
    pub fn current_street_strategy_actions(&self) -> Vec<(PlayerId, PlayerAction)> {
        self.current_street_action_facts()
            .into_iter()
            .map(|fact| (fact.player_id, fact.strategy_action()))
            .collect()
    }

    /// The sequence number that the next successfully-applied action will
    /// receive. Blinds consume sequence numbers 0 and 1, so the first
    /// voluntary action of a normal hand is 2.
    ///
    /// This is intentionally read-only: the engine remains the sole allocator
    /// of action sequence numbers. The engine-blind coordinator uses the value
    /// to validate a client's pre-signed action claim *before* mutating betting
    /// state, then `apply_action` records the same value atomically.
    pub fn next_action_seq(&self) -> u16 {
        self.action_seq
    }

    // ---------------------------------------------------------------------------
    // Blind-mode injection API (ADR-066 P1)
    // ---------------------------------------------------------------------------

    /// Feed externally-supplied, already-verified board cards for the street the
    /// engine has advanced to (ADR-066 §2 step 4). Replaces the `self.deck.deal()`
    /// board path that plaintext mode uses.
    ///
    /// The caller (server) supplies the board cards that all parties threshold-
    /// decrypted and the server verified against the committed ciphertext. This
    /// fills `self.board` for the pending street and emits the same
    /// [`EngineEvent::StreetRevealed`] event a plaintext deal would, carrying the
    /// current betting state.
    ///
    /// `cards.len()` must match the pending street: **3** for the flop, **1** for
    /// the turn, **1** for the river — otherwise [`BlindInjectError::WrongBoardCount`].
    ///
    /// During an all-in run-out the betting round for the just-advanced street may
    /// already be done; after filling the board this method advances to the next
    /// street (via `close_street_and_advance`) so the caller can then inject that
    /// street's board too. When the hand reaches showdown / completion this
    /// returns with `is_done()` true and the snapshot reflects the final state.
    ///
    /// Errors (never panics, never deals a server-visible card):
    /// - [`BlindInjectError::NotBlindMode`] if the hand is plaintext;
    /// - [`BlindInjectError::NoBoardPending`] if no street is awaiting a board;
    /// - [`BlindInjectError::WrongBoardCount`] on a bad card count.
    pub fn inject_board_for_street(
        &mut self,
        cards: &[crate::card::Card],
    ) -> Result<GameSnapshot, BlindInjectError> {
        if self.mode != HandMode::Blind {
            return Err(BlindInjectError::NotBlindMode);
        }
        let street = self
            .pending_board_street
            .ok_or(BlindInjectError::NoBoardPending)?;

        let expected = match street {
            Street::Flop => 3,
            Street::Turn | Street::River => 1,
            // Preflop never has a board to inject; we never record it as pending.
            Street::Preflop => return Err(BlindInjectError::NoBoardPending),
        };
        if cards.len() != expected {
            return Err(BlindInjectError::WrongBoardCount {
                street,
                expected,
                got: cards.len(),
            });
        }

        // The engine is the authoritative ranker; an injected board card that
        // collides with another card it already holds (an existing board card,
        // an already-`Revealed` hole card, or another card in this same injected
        // set) would silently mis-rank in release builds (`rank_hand` only
        // `debug_assert!`s uniqueness). Reject before storing anything.
        self.check_injected_cards_unique(cards)?;

        // Fill the board for this street and emit StreetRevealed with the cards
        // plus the live betting state (mirrors the plaintext deal event).
        let new_cards: Vec<crate::card::Card> = match street {
            Street::Flop => {
                self.board.flop = Some([cards[0], cards[1], cards[2]]);
                vec![cards[0], cards[1], cards[2]]
            }
            Street::Turn => {
                self.board.turn = Some(cards[0]);
                vec![cards[0]]
            }
            Street::River => {
                self.board.river = Some(cards[0]);
                vec![cards[0]]
            }
            Street::Preflop => unreachable!("guarded above"),
        };

        let (current_bet, min_raise_to, next_actor_seat) = self.new_street_betting_state();
        self.events.push(EngineEvent::StreetRevealed {
            street,
            new_cards,
            current_bet,
            min_raise_to,
            next_actor_seat,
        });

        // Board for this street is now known.
        self.pending_board_street = None;

        // All-in run-out: if the betting round for this street was immediately
        // done (everyone all-in), advance to the next street now that the board
        // is recorded — re-arming `pending_board_street` for the next inject.
        if self
            .betting_round
            .as_ref()
            .map(|r| r.is_done())
            .unwrap_or(false)
        {
            return self
                .close_street_and_advance()
                .map_err(|_| BlindInjectError::NoBoardPending);
        }

        Ok(self.snapshot())
    }

    /// Feed an externally-verified showdown reveal for a **non-folded** player
    /// (ADR-066 §2 step 5). Only injected reveals are used by `finish_blind` /
    /// `rank_hand` in blind mode.
    ///
    /// The caller (server) supplies the hole-card plaintext only after verifying
    /// it against the player's pre-committed ciphertext (`verify_and_open`). This
    /// transitions the seat from [`HoleSlot::Opaque`] to [`HoleSlot::Revealed`].
    ///
    /// Errors (never panics, never invents cards):
    /// - [`BlindInjectError::NotBlindMode`] if the hand is plaintext;
    /// - [`BlindInjectError::NotAtShowdown`] if the hand is not done (reveals are
    ///   only consumed at showdown);
    /// - [`BlindInjectError::UnknownPlayer`] for an unknown player id;
    /// - [`BlindInjectError::PlayerFolded`] for a folded seat — folded cards stay
    ///   sealed forever (ADR-066 §2 step 6).
    pub fn inject_showdown_reveal(
        &mut self,
        player_id: PlayerId,
        hole_cards: HoleCards,
    ) -> Result<(), BlindInjectError> {
        if self.mode != HandMode::Blind {
            return Err(BlindInjectError::NotBlindMode);
        }
        // Reveals are a showdown concept — only meaningful once betting is over.
        if self.phase != Phase::Done {
            return Err(BlindInjectError::NotAtShowdown);
        }
        // Validate target seat WITHOUT taking a mutable borrow yet — the
        // uniqueness check below needs an immutable borrow of all seats/board.
        match self.seats.iter().find(|s| s.player_id == player_id) {
            None => return Err(BlindInjectError::UnknownPlayer(player_id)),
            Some(seat) if seat.folded => {
                // A folded player never reveals — their cards stay sealed forever.
                return Err(BlindInjectError::PlayerFolded(player_id));
            }
            // A verified reveal is immutable showdown evidence: reject a second
            // reveal with DIFFERENT cards, but accept an identical re-injection
            // idempotently (U32, dual-AI OSS review).
            Some(seat) => {
                if let HoleSlot::Revealed(existing) = seat.hole {
                    if existing == hole_cards {
                        return Ok(());
                    }
                    return Err(BlindInjectError::AlreadyRevealed(player_id));
                }
            }
        }

        // Uniqueness: a revealed hole card must not collide with any board card,
        // any already-`Revealed` hole card, or its own pair-mate. The engine
        // ranks from these reveals and must never store a deck with a repeated
        // card (ADR-066 §2 — the authoritative ranker does not trust injected
        // cards; `rank_hand` only `debug_assert!`s uniqueness in debug builds).
        // The revealing seat is still `Opaque` here, so its own (about-to-be)
        // cards are not yet in the known set — no false self-collision.
        self.check_injected_cards_unique(&hole_cards.as_array())?;

        let seat = self
            .seats
            .iter_mut()
            .find(|s| s.player_id == player_id)
            .ok_or(BlindInjectError::UnknownPlayer(player_id))?;
        seat.hole = HoleSlot::Revealed(hole_cards);
        Ok(())
    }

    /// Validate that none of `injected` collide with a card the engine already
    /// holds (any board card, any already-`Revealed` hole card) and that the
    /// injected set has no internal duplicate. Returns the first colliding card
    /// as [`BlindInjectError::DuplicateCard`].
    ///
    /// The engine is the authoritative ranker (ADR-066 §2). `rank_hand` only
    /// `debug_assert!`s card uniqueness, which is compiled out in release — so an
    /// injected duplicate would silently mis-rank in production. This is the
    /// release-safe guard that keeps that from happening.
    fn check_injected_cards_unique(
        &self,
        injected: &[crate::card::Card],
    ) -> Result<(), BlindInjectError> {
        // Known set: every board card + every already-Revealed hole card.
        let mut known: std::collections::HashSet<crate::card::Card> =
            self.board.all_cards().into_iter().collect();
        for seat in &self.seats {
            if let HoleSlot::Revealed(h) = seat.hole {
                known.insert(h.card1);
                known.insert(h.card2);
            }
        }
        // Reject collisions against the known set AND within the injected set
        // itself (a flop with two equal cards, or a reveal pair of identical
        // cards). Insert each injected card into the same set as we go so an
        // intra-set duplicate is caught too.
        for &card in injected {
            if !known.insert(card) {
                return Err(BlindInjectError::DuplicateCard { card });
            }
        }
        Ok(())
    }

    /// The street currently awaiting a board injection in blind mode, if any.
    /// `None` in plaintext mode or when no board is pending.
    pub fn pending_board_street(&self) -> Option<Street> {
        self.pending_board_street
    }

    /// Compute the new-street betting-state triple
    /// `(current_bet, min_raise_to, next_actor_seat)` from the active round.
    /// Shared by the plaintext StreetRevealed patch and the blind
    /// `inject_board_for_street` event. `min_raise_to`/`next_actor_seat` are
    /// `None` when the round is already done (all-in runout).
    fn new_street_betting_state(&self) -> (u64, Option<u64>, Option<u8>) {
        let current_bet = self
            .betting_round
            .as_ref()
            .map(|r| r.current_bet().0 as u64)
            .unwrap_or(0);
        let min_raise_to = self.betting_round.as_ref().and_then(|r| {
            if r.is_done() {
                None
            } else {
                Some(r.min_raise_to().0 as u64)
            }
        });
        let next_actor_seat = self.betting_round.as_ref().and_then(|r| {
            if r.is_done() {
                None
            } else {
                r.current_player().and_then(|pid| {
                    self.seats
                        .iter()
                        .find(|s| s.player_id == pid)
                        .map(|s| s.seat)
                })
            }
        });
        (current_bet, min_raise_to, next_actor_seat)
    }

    // ---------------------------------------------------------------------------
    // Test-only helpers
    // ---------------------------------------------------------------------------

    /// Override the hole cards for a specific player (by `PlayerId`).
    ///
    /// Intended exclusively for integration tests that need to control which
    /// starting hand a player holds after `start()` has already dealt random cards.
    /// Gated behind the `test-helpers` cargo feature so production builds cannot
    /// call this method.
    #[cfg(any(test, feature = "test-helpers"))]
    pub fn override_hole_cards_for_test(&mut self, player_id: PlayerId, cards: HoleCards) {
        let mut found = false;
        for seat in &mut self.seats {
            if seat.player_id == player_id {
                seat.hole = HoleSlot::Plain(cards);
                found = true;
                break;
            }
        }
        if !found {
            return;
        }
        // Reserve the pinned cards out of the remaining deck so a later board
        // deal cannot collide with them. Without this, the board (still dealt
        // from the unshuffled remainder) can repeat a pinned hole card, and
        // `rank_hand` then receives overlapping cards — a real-deck impossibility
        // that its debug_assert (see eval.rs) correctly rejects. Test-only
        // helper, so rebuilding the deck from its remaining cards is fine.
        let remaining: Vec<crate::card::Card> = self
            .deck
            .cards()
            .iter()
            .copied()
            .filter(|c| *c != cards.card1 && *c != cards.card2)
            .collect();
        self.deck = Deck::from_cards(remaining);
    }

    // ---------------------------------------------------------------------------
    // Private helpers
    // ---------------------------------------------------------------------------

    fn current_street(&self) -> Street {
        match &self.phase {
            Phase::Betting(s) => *s,
            // A finished hand reports the street it ended on (U34), not a Preflop
            // sentinel. `finished_street` is set at every Done transition; it is
            // only `None` for a hand that never started.
            Phase::Done => self.finished_street.unwrap_or(Street::Preflop),
            Phase::NotStarted => Street::Preflop,
        }
    }

    /// Record the final street and transition the hand to `Done` (U34). Capturing
    /// here (while `self.phase` is still `Betting(s)`) is what lets a completed
    /// hand report its real final street.
    fn set_done(&mut self) {
        self.finished_street = Some(self.current_street());
        self.phase = Phase::Done;
    }

    /// Sync seat stacks from the betting round's current state.
    fn sync_stacks_from_round(&mut self) {
        if let Some(round) = &self.betting_round {
            for seat in &mut self.seats {
                // The round tracks stacks after contributions.
                // We need the round's player stacks (which account for blinds).
                let round_stack = round
                    .player_states()
                    .iter()
                    .find(|ps| ps.player_id == seat.player_id)
                    .map(|ps| ps.stack);
                if let Some(stack) = round_stack {
                    seat.stack = stack;
                }
                seat.all_in = round.player_all_in(seat.player_id);
                seat.folded = round.player_folded(seat.player_id);
            }
        }
    }

    /// Close the current betting round and advance to the next street.
    fn close_street_and_advance(&mut self) -> Result<GameSnapshot, ActionError> {
        // Absorb the round's pots into cumulative state.
        if let Some(round) = self.betting_round.take() {
            let new_pots = round.side_pots();
            let round_total = new_pots.iter().map(|p| p.amount.0).sum::<u32>();
            self.completed_pot.0 += round_total;
            self.accumulate_pots(new_pots);
        }

        // Check if hand is over (only one non-folded player, or we've exhausted streets).
        let active_count = self.seats.iter().filter(|s| !s.folded).count();
        if active_count <= 1 {
            self.set_done();
            return Ok(self.snapshot());
        }

        // Advance to next street.
        let next_street = match self.current_street() {
            Street::Preflop => Some(Street::Flop),
            Street::Flop => Some(Street::Turn),
            Street::Turn => Some(Street::River),
            Street::River => None,
        };

        match next_street {
            None => {
                self.set_done();
            }
            Some(street) => {
                // Deal community cards and emit StreetRevealed.
                //
                // Blind mode (ADR-066 P1): the engine does NOT deal the board
                // from a deck. It advances to the next street and builds the
                // betting round (board-independent), but the board cards arrive
                // later via `inject_board_for_street`, which emits StreetRevealed
                // once they are verified. We record the pending street and skip
                // the deck deal + StreetRevealed emission here.
                match self.mode {
                    HandMode::Plaintext => match street {
                        Street::Flop => {
                            let c1 = self.deck.deal().map_err(|_| ActionError::HandFinished)?;
                            let c2 = self.deck.deal().map_err(|_| ActionError::HandFinished)?;
                            let c3 = self.deck.deal().map_err(|_| ActionError::HandFinished)?;
                            self.board.flop = Some([c1, c2, c3]);
                            // Betting-state fields (current_bet, min_raise_to, next_actor_seat)
                            // are placeholder zeros/None here; they are patched to the real
                            // values immediately after BettingRound construction below.
                            self.events.push(EngineEvent::StreetRevealed {
                                street: Street::Flop,
                                new_cards: vec![c1, c2, c3],
                                current_bet: 0,
                                min_raise_to: None,
                                next_actor_seat: None,
                            });
                        }
                        Street::Turn => {
                            let c = self.deck.deal().map_err(|_| ActionError::HandFinished)?;
                            self.board.turn = Some(c);
                            self.events.push(EngineEvent::StreetRevealed {
                                street: Street::Turn,
                                new_cards: vec![c],
                                current_bet: 0,
                                min_raise_to: None,
                                next_actor_seat: None,
                            });
                        }
                        Street::River => {
                            let c = self.deck.deal().map_err(|_| ActionError::HandFinished)?;
                            self.board.river = Some(c);
                            self.events.push(EngineEvent::StreetRevealed {
                                street: Street::River,
                                new_cards: vec![c],
                                current_bet: 0,
                                min_raise_to: None,
                                next_actor_seat: None,
                            });
                        }
                        Street::Preflop => {}
                    },
                    HandMode::Blind => {
                        // Defer the board: it will be filled by
                        // `inject_board_for_street(street, ...)`. Record which
                        // street is awaiting its verified cards.
                        if street != Street::Preflop {
                            self.pending_board_street = Some(street);
                        }
                    }
                }

                self.phase = Phase::Betting(street);

                // Determine first actor post-flop: first non-folded/all-in seat
                // strictly LEFT OF the button (WSOP/TDA standard).
                //
                // For 3+ players this is the SB seat (dealer+1).  In heads-up
                // (n=2) the dealer IS the SB, so we must walk to dealer+1 (the
                // BB) — the non-button player acts first post-flop in HU.
                // Starting the walk at `dealer + 1` unifies both cases.
                let n = self.seats.len();
                let dealer_idx = self.dealer_idx % n;
                let first_seat_idx = self.first_active_after((dealer_idx + 1) % n);
                let first_actor_pid = self.seats[first_seat_idx].player_id;

                // Build the round with current stacks (post-previous-round).
                // Filter out folded seats so they never re-enter the actor sequence.
                let players_for_round: Vec<(PlayerId, Chips)> = self
                    .seats
                    .iter()
                    .filter(|s| !s.folded)
                    .map(|s| (s.player_id, s.stack))
                    .collect();

                // Resolve first_actor as an index into the filtered vec.
                let first_actor = players_for_round
                    .iter()
                    .position(|(pid, _)| *pid == first_actor_pid)
                    .unwrap_or(0);

                let round =
                    BettingRound::new(players_for_round, first_actor, Chips::ZERO, self.big_blind);
                self.betting_round = Some(round);

                // Read post-construction betting state for the StreetRevealed event.
                // If all players are all-in, the round is immediately done and there
                // is no next actor — `next_actor_seat` and `min_raise_to` will be None.
                let new_street_current_bet = self
                    .betting_round
                    .as_ref()
                    .map(|r| r.current_bet().0 as u64)
                    .unwrap_or(0);
                let new_street_min_raise_to = self.betting_round.as_ref().and_then(|r| {
                    if r.is_done() {
                        None
                    } else {
                        Some(r.min_raise_to().0 as u64)
                    }
                });
                let new_street_next_actor_seat = self.betting_round.as_ref().and_then(|r| {
                    if r.is_done() {
                        None
                    } else {
                        r.current_player().and_then(|pid| {
                            self.seats
                                .iter()
                                .find(|s| s.player_id == pid)
                                .map(|s| s.seat)
                        })
                    }
                });

                // Patch the StreetRevealed event that was pushed above with the
                // now-known betting state fields.  We pushed it before constructing
                // the round so the card data is already there; replace the last
                // event (which must be the StreetRevealed we just pushed).
                if let Some(EngineEvent::StreetRevealed {
                    current_bet,
                    min_raise_to,
                    next_actor_seat,
                    ..
                }) = self.events.last_mut()
                {
                    *current_bet = new_street_current_bet;
                    *min_raise_to = new_street_min_raise_to;
                    *next_actor_seat = new_street_next_actor_seat;
                }

                // If all remaining players are already all-in, skip forward.
                //
                // Blind mode (ADR-066 §2 step 7 — run-out reveal phase): do NOT
                // auto-recurse through the runout. Each street's board must be
                // injected separately, so we pause here with the round done and a
                // pending board. `inject_board_for_street` re-drives the advance
                // after it fills this street's board (it calls
                // `close_street_and_advance` if the just-revealed round is done).
                if self.mode == HandMode::Plaintext
                    && self
                        .betting_round
                        .as_ref()
                        .map(|r| r.is_done())
                        .unwrap_or(false)
                {
                    return self.close_street_and_advance();
                }
            }
        }

        Ok(self.snapshot())
    }

    /// Compute the WSOP / TDA canonical show-order for the current showdown.
    ///
    /// Called from `finish()` after the showdown is built. The returned
    /// `Vec<PlayerId>` lists non-folded players in the order they should
    /// reveal hole cards at showdown. See `HandResult.show_order` for the
    /// per-rule details.
    fn compute_show_order(&self) -> Vec<PlayerId> {
        // Non-folded seats are the only candidates.
        let alive_count = self.seats.iter().filter(|s| !s.folded).count();
        if alive_count <= 1 {
            return Vec::new();
        }

        // Did anyone bet/raise on the river?
        // Walk river actions in order; track each player's contribution before
        // their action and flag aggression when the action moved current_bet up
        // (Raise / AllIn that exceeded the running bet) — `actions` carries
        // pot_before/pot_after but not current_bet, so use action-shape +
        // amount > 0 to identify aggression.
        let mut river_current_bet: u32 = 0;
        let mut river_committed: std::collections::HashMap<u64, u32> =
            std::collections::HashMap::new();
        let mut last_river_aggressor: Option<PlayerId> = None;
        for a in &self.actions {
            if a.street != Street::River {
                continue;
            }
            let pid_key = a.player_id.inner();
            let prior = *river_committed.get(&pid_key).unwrap_or(&0);
            let new_total = prior + a.amount.0;
            river_committed.insert(pid_key, new_total);
            match a.action {
                PlayerAction::Raise { .. } | PlayerAction::AllIn
                    if new_total > river_current_bet =>
                {
                    river_current_bet = new_total;
                    last_river_aggressor = Some(a.player_id);
                }
                _ => {}
            }
        }

        // Pick the start seat:
        //   * river aggressor exists → their seat
        //   * else → first non-folded seat clockwise from button (dealer + 1)
        let n = self.seats.len();
        let first_left_of_button = (self.dealer_idx % n + 1) % n;
        let start_idx = if let Some(pid) = last_river_aggressor {
            self.seats
                .iter()
                .position(|s| s.player_id == pid)
                .unwrap_or(first_left_of_button)
        } else {
            // Clockwise from the button — start the search at dealer+1 and walk
            // through non-folded seats.
            let mut idx = first_left_of_button;
            for _ in 0..n {
                if !self.seats[idx].folded {
                    break;
                }
                idx = (idx + 1) % n;
            }
            idx
        };

        // Build the show order by walking clockwise from start_idx and emitting
        // non-folded seats. start_idx may itself be folded only when alive is
        // empty (already returned above), so it is always included.
        let mut order: Vec<PlayerId> = Vec::with_capacity(alive_count);
        let mut idx = start_idx;
        for _ in 0..n {
            if !self.seats[idx].folded {
                order.push(self.seats[idx].player_id);
            }
            idx = (idx + 1) % n;
            if order.len() == alive_count {
                break;
            }
        }
        order
    }

    /// Map each player to its button-relative seat order: 0 for the first seat
    /// clockwise after the button (small blind in 3+ handed), 1 for the next,
    /// and so on. Used for TDA Rule 25 odd-chip placement (the odd chip goes to
    /// the tied winner in the earliest position, i.e. the lowest order).
    fn button_relative_order(&self) -> HashMap<u64, usize> {
        let n = self.seats.len();
        let mut order = HashMap::with_capacity(n);
        if n == 0 {
            return order;
        }
        let first_left_of_button = (self.dealer_idx % n + 1) % n;
        for offset in 0..n {
            let idx = (first_left_of_button + offset) % n;
            order.insert(self.seats[idx].player_id.inner(), offset);
        }
        order
    }

    /// Find the first active (non-folded, non-all-in) seat starting from `start`.
    fn first_active_after(&self, start: usize) -> usize {
        let n = self.seats.len();
        for i in 0..n {
            let idx = (start + i) % n;
            if !self.seats[idx].folded && !self.seats[idx].all_in {
                return idx;
            }
        }
        start % n
    }

    /// The hand-global pot decomposition visible right now.
    fn merged_side_pots(&self) -> Vec<SidePot> {
        self.hand_side_pots()
    }

    /// Rebuild canonical pots from each player's total contribution for the
    /// whole hand. Street-local pot fragments are bookkeeping only: real side
    /// pot boundaries arise when total contributions (normally all-in caps)
    /// change the set of players entitled to a layer.
    fn hand_side_pots(&self) -> Vec<SidePot> {
        let all: Vec<(PlayerId, u32)> = self
            .seats
            .iter()
            .map(|seat| {
                (
                    seat.player_id,
                    seat.starting_stack
                        .0
                        .checked_sub(seat.stack.0)
                        .expect("in-hand stack cannot exceed its starting stack"),
                )
            })
            .collect();
        let eligible: Vec<(PlayerId, u32)> = self
            .seats
            .iter()
            .filter(|seat| !seat.folded)
            .map(|seat| {
                (
                    seat.player_id,
                    seat.starting_stack
                        .0
                        .checked_sub(seat.stack.0)
                        .expect("in-hand stack cannot exceed its starting stack"),
                )
            })
            .collect();
        if eligible.is_empty() {
            return Vec::new();
        }

        let mut levels: Vec<u32> = all.iter().map(|(_, amount)| *amount).collect();
        levels.sort_unstable();
        levels.dedup();
        let mut previous = 0u32;
        let mut pots: Vec<SidePot> = Vec::new();

        for level in levels.into_iter().filter(|level| *level > 0) {
            let slice = level - previous;
            let amount = all
                .iter()
                .map(|(_, contribution)| contribution.saturating_sub(previous).min(slice))
                .sum::<u32>();
            let pot_eligible: Vec<PlayerId> = eligible
                .iter()
                .filter(|(_, contribution)| *contribution >= level)
                .map(|(pid, _)| *pid)
                .collect();
            let contributors: Vec<PlayerId> = all
                .iter()
                .filter(|(_, contribution)| *contribution > previous)
                .map(|(pid, _)| *pid)
                .collect();

            let (eligible_ids, refund_to) = if pot_eligible.is_empty() {
                (Vec::new(), contributors)
            } else {
                let refund = if pot_eligible.len() == 1 && contributors == pot_eligible {
                    pot_eligible.clone()
                } else {
                    Vec::new()
                };
                (pot_eligible, refund)
            };
            if amount > 0 {
                pots.push(SidePot {
                    cap: Chips(level),
                    amount: Chips(amount),
                    eligible: eligible_ids,
                    refund_to,
                });
            }
            previous = level;
        }

        let mut canonical: Vec<SidePot> = Vec::new();
        for pot in pots {
            if let Some(existing) = canonical.last_mut().filter(|existing| {
                existing.eligible == pot.eligible && existing.refund_to == pot.refund_to
            }) {
                existing.amount.0 += pot.amount.0;
                existing.cap.0 = existing.cap.0.max(pot.cap.0);
            } else {
                canonical.push(pot);
            }
        }
        canonical
    }

    /// Canonical, hand-global pots used for every terminal settlement path.
    fn settlement_side_pots(&self) -> Vec<SidePot> {
        self.hand_side_pots()
    }

    /// Accumulate new side pots into the running cumulative list.
    fn accumulate_pots(&mut self, new_pots: Vec<SidePot>) {
        if new_pots.is_empty() {
            return;
        }
        self.cumulative_side_pots.extend(new_pots);
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

/// Distribute each pot to winner(s).
///
/// `button_order` maps each player to its button-relative seat order (0 = first
/// seat left of the button). A split pot's odd chips are spread one per seat to
/// the tied winners with the lowest order (TDA Rule 25). When `button_order` is
/// missing a player (defensive), that player sorts last.
///
/// `forfeiters` (ADR-066 §5 / audit F3) is the set of non-folded contenders that
/// did NOT reveal at a blind-mode showdown. It is **empty** for the plaintext
/// path (which has no forfeit concept). When a pot has no contender reaching
/// showdown (`contenders.is_empty()`) the refund branch must distinguish two
/// cases by inspecting `pot.eligible`:
/// - every eligible contributor genuinely **folded** (none is a forfeiter): a
///   real "no contest" pot → refund to the eligible contributors (unchanged);
/// - at least one eligible contributor is a **forfeiter**: a forfeiter must
///   never get its stake back, so the layer is awarded to the honest revealers
///   (`eligible_with_cards`), ranked among themselves — forfeit-not-refund.
fn distribute_pots(
    pots: &[SidePot],
    eligible_with_cards: &[(PlayerId, HoleCards)],
    board: &BoardCards,
    button_order: &HashMap<u64, usize>,
    forfeiters: &HashSet<u64>,
) -> Vec<PotResult> {
    if pots.is_empty() {
        return vec![];
    }

    // TDA Rule 25: a split pot's `rem` odd chips are spread ONE PER SEAT to the
    // `rem` tied winners in the earliest positions (lowest button-relative
    // order), breaking ties by player id for determinism — never piled on one
    // seat. Returns those recipients (the first `rem` players in button order).
    let odd_chip_recipients = |ids: &[PlayerId], rem: u32| -> Vec<PlayerId> {
        let mut sorted: Vec<PlayerId> = ids.to_vec();
        sorted.sort_by_key(|pid| {
            (
                button_order
                    .get(&pid.inner())
                    .copied()
                    .unwrap_or(usize::MAX),
                pid.inner(),
            )
        });
        sorted.truncate(rem as usize);
        sorted
    };

    pots.iter()
        .enumerate()
        .filter_map(|(idx, pot)| {
            // Filter to non-folded eligible players who have cards.
            let contenders: Vec<(PlayerId, HoleCards)> = pot
                .eligible
                .iter()
                .filter_map(|pid| {
                    eligible_with_cards
                        .iter()
                        .find(|(p, _)| p == pid)
                        .map(|(p, h)| (*p, *h))
                })
                .collect();

            if pot.amount.0 == 0 {
                return None;
            }

            if !pot.refund_to.is_empty() {
                let refund_ids = &pot.refund_to;
                let n = refund_ids.len() as u32;
                let base = pot.amount.0 / n;
                let rem = pot.amount.0 % n;
                let odd_recipients = odd_chip_recipients(refund_ids, rem);
                let winners = refund_ids
                    .iter()
                    .map(|pid| PotWinner {
                        player_id: *pid,
                        amount_won: Chips(base + if odd_recipients.contains(pid) { 1 } else { 0 }),
                    })
                    .collect();
                return Some(PotResult {
                    index: idx,
                    amount: pot.amount,
                    eligible_player_ids: if pot.eligible.is_empty() {
                        refund_ids.to_vec()
                    } else {
                        pot.eligible.clone()
                    },
                    winners,
                    is_refund: true,
                });
            }

            // No eligible player reached showdown for THIS layer. Two distinct
            // cases (the second is ADR-066 §5 / audit F3, blind mode only):
            //
            // (1) Every player eligible for this pot genuinely **folded** ("no
            //     contest" side pot). Real-poker resolution is to return the
            //     stakes, not destroy them: refund the pot equally to its eligible
            //     contributors (each contributed an equal share of this layer by
            //     side-pot construction). The previous `return None` here silently
            //     destroyed the chips, violating chip conservation (bug-sweep
            //     2026-05-29 finding #5: reachable when a side pot's whole eligible
            //     set folds on a later street, e.g. both deep stacks fold an
            //     uncontested pot to an already-all-in short stack).
            //
            // (2) At least one eligible contributor is a **forfeiter** (non-folded
            //     but withheld/failed its blind showdown reveal). A forfeiter must
            //     NEVER get its stake back (forfeit-not-refund) — that would reward
            //     stalling. The layer is instead awarded to the honest revealers
            //     (`eligible_with_cards`), ranked among themselves, exactly as if
            //     this layer were contested by them. `eligible_with_cards` is
            //     guaranteed non-empty here: a contested blind finish that reaches
            //     distribution has ≥1 honest revealer (zero-revealer contests VOID
            //     before distribution), and the plaintext path has no forfeiters.
            if contenders.is_empty() {
                let has_forfeiter = pot.eligible.iter().any(|p| forfeiters.contains(&p.inner()));
                if has_forfeiter && !eligible_with_cards.is_empty() {
                    // Forfeited layer → award to the honest revealers (ranked),
                    // NOT refunded to the forfeiters.
                    let winners = if eligible_with_cards.len() == 1 {
                        vec![PotWinner {
                            player_id: eligible_with_cards[0].0,
                            amount_won: pot.amount,
                        }]
                    } else {
                        let ranked = rank_players(eligible_with_cards, board);
                        let best = ranked[0].1;
                        let shared: Vec<PlayerId> = ranked
                            .iter()
                            .take_while(|(_, r)| *r == best)
                            .map(|(pid, _)| *pid)
                            .collect();
                        let count = shared.len() as u32;
                        let base = pot.amount.0 / count;
                        let rem = pot.amount.0 % count;
                        let odd_recipients = odd_chip_recipients(&shared, rem);
                        shared
                            .iter()
                            .map(|pid| PotWinner {
                                player_id: *pid,
                                amount_won: Chips(
                                    base + if odd_recipients.contains(pid) { 1 } else { 0 },
                                ),
                            })
                            .collect()
                    };
                    return Some(PotResult {
                        index: idx,
                        amount: pot.amount,
                        // The award is on merit (a forfeit loss), not a refund —
                        // the eligible-id list stays the layer's original set.
                        eligible_player_ids: pot.eligible.clone(),
                        winners,
                        is_refund: false,
                    });
                }

                // Refund target: the layer's eligible contributors. Explicit
                // uncalled-overage refunds are handled by `refund_to` above.
                let refund_ids: &[PlayerId] = &pot.eligible;
                let n = refund_ids.len() as u32;
                if n == 0 {
                    return None;
                }
                let base = pot.amount.0 / n;
                let rem = pot.amount.0 % n;
                let odd_recipients = odd_chip_recipients(refund_ids, rem);
                let winners = refund_ids
                    .iter()
                    .map(|pid| PotWinner {
                        player_id: *pid,
                        amount_won: Chips(base + if odd_recipients.contains(pid) { 1 } else { 0 }),
                    })
                    .collect();
                return Some(PotResult {
                    index: idx,
                    amount: pot.amount,
                    eligible_player_ids: refund_ids.to_vec(),
                    winners,
                    is_refund: true,
                });
            }

            let pot_eligible_ids = pot.eligible.clone();

            let winners = if contenders.len() == 1 {
                vec![PotWinner {
                    player_id: contenders[0].0,
                    amount_won: pot.amount,
                }]
            } else {
                let ranked = rank_players(&contenders, board);
                let best = ranked[0].1;
                let shared: Vec<PlayerId> = ranked
                    .iter()
                    .take_while(|(_, r)| *r == best)
                    .map(|(pid, _)| *pid)
                    .collect();
                let count = shared.len() as u32;
                let base = pot.amount.0 / count;
                let rem = pot.amount.0 % count;
                // TDA Rule 25: the odd chip goes to the tied winner in the
                // earliest position (first seat left of the button), NOT to
                // whoever is first in action order (audit 2026-06-03).
                let odd_recipients = odd_chip_recipients(&shared, rem);
                shared
                    .iter()
                    .map(|pid| PotWinner {
                        player_id: *pid,
                        amount_won: Chips(base + if odd_recipients.contains(pid) { 1 } else { 0 }),
                    })
                    .collect()
            };

            Some(PotResult {
                index: idx,
                amount: pot.amount,
                eligible_player_ids: pot_eligible_ids,
                winners,
                is_refund: false,
            })
        })
        .collect()
}

/// Card-free pot distribution for a blind-mode **uncontested** hand
/// (ADR-066 §2 step 6). No `HoleCards` exist; the winner is decided purely by
/// "who is the lone non-folded survivor" — there is no ranking and no card is
/// ever read. This is the structural zero-leak path: it is impossible for a
/// hole value to influence (or even reach) the outcome.
///
/// - `lone == Some(pid)`: that player is the single non-folded survivor and wins
///   each pot it is eligible for; pots it is not eligible for (a higher all-in
///   side-pot layer it never matched) are refunded to their eligible
///   contributors, exactly like the `contenders.is_empty()` branch of
///   [`distribute_pots`].
/// - `lone == None`: every player folded (degenerate); every pot is refunded to
///   its eligible contributors.
///
/// Chip conservation holds: each pot is fully awarded or fully refunded.
fn distribute_pots_uncontested(
    pots: &[SidePot],
    lone: Option<PlayerId>,
    button_order: &HashMap<u64, usize>,
) -> Vec<PotResult> {
    if pots.is_empty() {
        return vec![];
    }

    // Split a pot's `rem` odd chips one-per-seat to the earliest-position
    // recipients (TDA Rule 25), identical to `distribute_pots`.
    let odd_chip_recipients = |ids: &[PlayerId], rem: u32| -> Vec<PlayerId> {
        let mut sorted: Vec<PlayerId> = ids.to_vec();
        sorted.sort_by_key(|pid| {
            (
                button_order
                    .get(&pid.inner())
                    .copied()
                    .unwrap_or(usize::MAX),
                pid.inner(),
            )
        });
        sorted.truncate(rem as usize);
        sorted
    };

    let refund = |idx: usize, pot: &SidePot| -> Option<PotResult> {
        // Prefer an explicit `refund_to` (uncalled overage); else the eligible
        // contributors of the layer.
        let refund_ids: &[PlayerId] = if !pot.refund_to.is_empty() {
            &pot.refund_to
        } else {
            &pot.eligible
        };
        let n = refund_ids.len() as u32;
        if n == 0 {
            return None;
        }
        let base = pot.amount.0 / n;
        let rem = pot.amount.0 % n;
        let odd_recipients = odd_chip_recipients(refund_ids, rem);
        let winners = refund_ids
            .iter()
            .map(|pid| PotWinner {
                player_id: *pid,
                amount_won: Chips(base + if odd_recipients.contains(pid) { 1 } else { 0 }),
            })
            .collect();
        Some(PotResult {
            index: idx,
            amount: pot.amount,
            eligible_player_ids: if pot.eligible.is_empty() {
                refund_ids.to_vec()
            } else {
                pot.eligible.clone()
            },
            winners,
            is_refund: true,
        })
    };

    pots.iter()
        .enumerate()
        .filter_map(|(idx, pot)| {
            if pot.amount.0 == 0 {
                return None;
            }
            // Explicit uncalled-overage layers always refund.
            if !pot.refund_to.is_empty() {
                return refund(idx, pot);
            }
            match lone {
                // The lone survivor wins every pot it is eligible for; otherwise
                // (a side-pot layer it never matched) the layer is refunded.
                Some(pid) if pot.eligible.contains(&pid) => Some(PotResult {
                    index: idx,
                    amount: pot.amount,
                    eligible_player_ids: pot.eligible.clone(),
                    winners: vec![PotWinner {
                        player_id: pid,
                        amount_won: pot.amount,
                    }],
                    is_refund: false,
                }),
                _ => refund(idx, pot),
            }
        })
        .collect()
}

/// Distribute pots when the board is **unrevealable** because a contender
/// withheld a required board share (audit LIVE-F1, `finish_blind_forfeit_board`).
/// The `withholders` have already been force-folded; `survivors` is the set of
/// remaining non-folded contenders. Each contested pot is split **equally** among
/// the survivors that are eligible for it (TDA-25 odd-chip placement to the
/// earliest button-relative seats). The withholder's committed chips therefore
/// flow to the honest survivors — never refunded (forfeit-not-refund).
///
/// Uncalled-overage layers (`refund_to`) ALWAYS return to their owner (an
/// uncalled bet was never matched, so no one could win it — returning it is not a
/// +EV exploit of the withhold). A contested pot whose eligible set contains NO
/// survivor (every eligible contributor for THAT layer was force-folded) is
/// awarded to the honest overall `survivors` — NOT refunded to its eligible set
/// (audit F5). Refunding it would hand the withholders their matched side-pot
/// stake back, making a withheld side-pot board share +EV; the only refund path
/// is `refund_to` (uncalled overage). The all-withholders whole-hand case (no
/// overall survivor) is rejected upstream by the caller's `NoSurvivor` guard.
fn distribute_pots_among_survivors(
    pots: &[SidePot],
    survivors: &[PlayerId],
    button_order: &HashMap<u64, usize>,
) -> Vec<PotResult> {
    if pots.is_empty() {
        return vec![];
    }

    // Spread `rem` odd chips one-per-seat to the earliest-position recipients
    // (TDA Rule 25), identical to `distribute_pots_uncontested`.
    let odd_chip_recipients = |ids: &[PlayerId], rem: u32| -> Vec<PlayerId> {
        let mut sorted: Vec<PlayerId> = ids.to_vec();
        sorted.sort_by_key(|pid| {
            (
                button_order
                    .get(&pid.inner())
                    .copied()
                    .unwrap_or(usize::MAX),
                pid.inner(),
            )
        });
        sorted.truncate(rem as usize);
        sorted
    };

    // Split a pot's `amount` equally among `recipients` (>=1), with `is_refund`
    // controlling the flag on the resulting `PotResult`.
    let split = |idx: usize,
                 pot: &SidePot,
                 recipients: &[PlayerId],
                 is_refund: bool|
     -> Option<PotResult> {
        let n = recipients.len() as u32;
        if n == 0 {
            return None;
        }
        let base = pot.amount.0 / n;
        let rem = pot.amount.0 % n;
        let odd = odd_chip_recipients(recipients, rem);
        let winners = recipients
            .iter()
            .map(|pid| PotWinner {
                player_id: *pid,
                amount_won: Chips(base + if odd.contains(pid) { 1 } else { 0 }),
            })
            .collect();
        Some(PotResult {
            index: idx,
            amount: pot.amount,
            eligible_player_ids: if pot.eligible.is_empty() {
                recipients.to_vec()
            } else {
                pot.eligible.clone()
            },
            winners,
            is_refund,
        })
    };

    pots.iter()
        .enumerate()
        .filter_map(|(idx, pot)| {
            if pot.amount.0 == 0 {
                return None;
            }
            // Explicit uncalled-overage layers always refund to their owner.
            if !pot.refund_to.is_empty() {
                return split(idx, pot, &pot.refund_to, true);
            }
            // Contested layer: split equally among the survivors eligible for it.
            let eligible_survivors: Vec<PlayerId> = if pot.eligible.is_empty() {
                survivors.to_vec()
            } else {
                pot.eligible
                    .iter()
                    .copied()
                    .filter(|pid| survivors.contains(pid))
                    .collect()
            };
            if eligible_survivors.is_empty() {
                // No survivor is eligible for THIS layer — every eligible
                // contributor was force-folded (audit LIVE-F1 / F5). Refunding
                // the layer to `pot.eligible` here would hand the withholders
                // their matched side-pot stake back, making a withheld board
                // share +EV (the F5 exploit: an honest main-pot survivor keeps
                // the hand out of `NoSurvivor`, but a withholder-only side pot
                // leaked the refund). Forfeit-not-refund: award the layer to the
                // honest overall survivors instead. (`refund_to` uncalled-overage
                // is the ONLY refund path; the all-withholders whole-hand case is
                // still caught upstream by the `NoSurvivor` guard.)
                if !survivors.is_empty() {
                    return split(idx, pot, survivors, false);
                }
                // Defensive: no overall survivor reached this distributor (the
                // caller's `NoSurvivor` guard should have rejected first). Refund
                // to the eligible set so no chip is destroyed rather than panic.
                return split(idx, pot, &pot.eligible, true);
            }
            split(idx, pot, &eligible_survivors, false)
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::card::{Card, Rank, Suit};

    fn pid(n: u64) -> PlayerId {
        PlayerId::new(n)
    }
    fn c(n: u32) -> Chips {
        Chips(n)
    }
    fn card(r: Rank, s: Suit) -> Card {
        Card::new(r, s)
    }

    #[test]
    fn start_rejects_table_chip_total_above_public_u32_pot_range() {
        let mut hand = GameHand::new_with_rng(
            vec![(pid(1), c(u32::MAX), 0), (pid(2), c(1), 1)],
            0,
            c(2),
            c(1),
            PokerRng::from_seed(1),
        );
        assert!(matches!(hand.start(), Err(ActionError::InvalidAction)));
        assert!(
            hand.actions.is_empty(),
            "oversized table must fail before blinds"
        );
    }

    // ---- P0 regression: blind seq uniqueness (M6-6 bug) ----

    /// SB blind must get seq=0, BB blind must get seq=1.
    ///
    /// Before the fix, both were seq=0, which caused the DB unique index
    /// `hand_actions_hand_seq` to reject the second INSERT, leaving
    /// `hand_actions` empty for every hand.
    #[test]
    fn blinds_have_distinct_seq_after_start() {
        let mut hand = GameHand::new_with_rng(
            vec![
                (pid(1), c(1000), 0),
                (pid(2), c(1000), 1),
                (pid(3), c(1000), 2),
            ],
            0,
            c(20),
            c(10),
            PokerRng::from_seed(42),
        );
        hand.start().expect("start ok");
        let result_actions = &hand.actions;
        assert_eq!(
            result_actions.len(),
            2,
            "start() must record exactly 2 blind actions"
        );
        assert_eq!(result_actions[0].seq, 0, "SB blind must have seq=0");
        assert_eq!(result_actions[1].seq, 1, "BB blind must have seq=1");
        // Verify uniqueness — the core invariant the DB index enforces.
        assert_ne!(
            result_actions[0].seq, result_actions[1].seq,
            "SB and BB blind seq must be distinct (was the P0 bug)"
        );
    }

    #[test]
    fn validate_action_is_pure_for_valid_and_invalid_inputs() {
        let mut hand = GameHand::new_with_rng(
            vec![(pid(1), c(1000), 0), (pid(2), c(1000), 1)],
            0,
            c(20),
            c(10),
            PokerRng::from_seed(43),
        );
        let before = hand.start().expect("start ok");
        let actor = before.current_actor.expect("preflop actor");
        let wrong_actor = if actor == pid(1) { pid(2) } else { pid(1) };
        let seq = hand.next_action_seq();
        let actions = hand.actions.len();
        let events = hand.events.len();
        let snapshot_before = hand.snapshot();

        hand.validate_action(actor, &PlayerAction::Call)
            .expect("current actor call validates");
        assert!(matches!(
            hand.validate_action(wrong_actor, &PlayerAction::Call),
            Err(ActionError::NotYourTurn)
        ));

        let snapshot_after = hand.snapshot();
        assert_eq!(hand.next_action_seq(), seq);
        assert_eq!(hand.actions.len(), actions);
        assert_eq!(hand.events.len(), events);
        assert_eq!(snapshot_after.current_actor, snapshot_before.current_actor);
        assert_eq!(snapshot_after.pot, snapshot_before.pot);
        assert_eq!(snapshot_after.current_bet, snapshot_before.current_bet);
        assert_eq!(
            snapshot_after
                .players
                .iter()
                .map(|player| (player.player_id, player.stack, player.committed_this_street))
                .collect::<Vec<_>>(),
            snapshot_before
                .players
                .iter()
                .map(|player| (player.player_id, player.stack, player.committed_this_street))
                .collect::<Vec<_>>()
        );
    }

    #[test]
    fn hu_all_in_opponent_exposes_call_only_and_stabilized_runout_preview() {
        let mut hand = GameHand::new_blind(
            vec![(pid(1), c(100), 0), (pid(2), c(1000), 1)],
            0,
            c(20),
            c(10),
            DeckSeed::default(),
        );
        hand.start().expect("start blind hand");
        assert_eq!(hand.snapshot().current_actor, Some(pid(1)));
        hand.apply_action(pid(1), PlayerAction::AllIn)
            .expect("button/SB shove");

        let snap = hand.snapshot();
        assert_eq!(snap.current_actor, Some(pid(2)));
        assert_eq!(snap.current_bet, c(100));
        assert_eq!(
            snap.min_raise_to, None,
            "the sole live stack may fold/call but cannot build a dry side pot"
        );
        assert_eq!(
            hand.preview_action_transition(pid(2), &PlayerAction::Raise { amount: c(200) })
                .unwrap_err(),
            ActionError::InvalidAction
        );
        assert_eq!(
            hand.preview_action_transition(pid(2), &PlayerAction::AllIn)
                .unwrap_err(),
            ActionError::InvalidAction,
            "an over-call all-in is still an aggressive dry-side-pot wager"
        );

        let call = hand
            .preview_action_transition(pid(2), &PlayerAction::Call)
            .expect("call remains legal");
        assert_eq!(call.min_raise_to_before, None);
        assert!(call.round_closed_after, "the call closes preflop");
        assert_eq!(call.street_after, Street::Flop);
        assert_eq!(call.committed_after, c(0));
        assert_eq!(call.current_bet_after, c(0));
        assert_eq!(call.min_raise_to_after, None);
        assert_eq!(call.next_actor, None, "all-in runout has no flop actor");
        assert!(!call.next_actor_can_raise_after);
        assert!(call.betting_terminal_after);
    }

    #[test]
    fn transition_preview_stabilizes_street_closing_action_into_flop_state() {
        let mut hand = GameHand::new_blind(
            vec![(pid(1), c(1000), 0), (pid(2), c(1000), 1)],
            0,
            c(20),
            c(10),
            DeckSeed::default(),
        );
        hand.start().expect("start blind hand");
        hand.apply_action(pid(1), PlayerAction::Call)
            .expect("SB completes");

        let transition = hand
            .preview_action_transition(pid(2), &PlayerAction::Check)
            .expect("BB option checks");
        assert!(transition.round_closed_after, "check closes preflop");
        assert_eq!(transition.street_after, Street::Flop);
        assert_eq!(transition.stack_after, c(980));
        assert_eq!(transition.committed_after, c(0));
        assert_eq!(transition.current_bet_after, c(0));
        assert_eq!(transition.min_raise_to_after, Some(c(20)));
        assert_eq!(
            transition.next_actor,
            Some(pid(2)),
            "the non-button/BB acts first postflop heads-up"
        );
        assert!(transition.next_actor_can_raise_after);
        assert!(!transition.betting_terminal_after);
    }

    #[test]
    fn short_big_blind_transition_uses_actual_live_contribution() {
        // 3-way 10/20: button folds, SB's posted 10 already covers the BB's
        // short all-in 8. No SB action exists; betting is terminal and only the
        // board runout remains.
        let mut covered = GameHand::new_blind(
            vec![(pid(1), c(100), 0), (pid(2), c(100), 1), (pid(3), c(8), 2)],
            0,
            c(20),
            c(10),
            DeckSeed::default(),
        );
        covered.start().unwrap();
        assert_eq!(covered.snapshot().current_actor, Some(pid(1)));
        let closes = covered
            .preview_action_transition(pid(1), &PlayerAction::Fold)
            .unwrap();
        assert!(closes.round_closed_after);
        assert!(closes.betting_terminal_after);
        assert_eq!(closes.next_actor, None);
        assert_eq!(closes.min_raise_to_after, None);

        // Negative companion: with a 5-chip SB, the short BB's actual 8 leaves
        // a three-chip decision. The fold transition stays preflop, exposes the
        // SB as actor, and advertises no dry-side-pot raise.
        let mut owing = GameHand::new_blind(
            vec![(pid(1), c(100), 0), (pid(2), c(100), 1), (pid(3), c(8), 2)],
            0,
            c(20),
            c(5),
            DeckSeed::default(),
        );
        owing.start().unwrap();
        let remains = owing
            .preview_action_transition(pid(1), &PlayerAction::Fold)
            .unwrap();
        assert!(!remains.round_closed_after);
        assert!(!remains.betting_terminal_after);
        assert_eq!(remains.street_after, Street::Preflop);
        assert_eq!(remains.current_bet_after, c(8));
        assert_eq!(remains.next_actor, Some(pid(2)));
        assert_eq!(remains.min_raise_to_after, None);
        assert!(!remains.next_actor_can_raise_after);
    }

    /// First voluntary action must get seq=2 (not seq=1 as before the fix).
    #[test]
    fn first_voluntary_action_seq_is_2_after_blinds() {
        let mut hand = GameHand::new_with_rng(
            vec![
                (pid(1), c(1000), 0),
                (pid(2), c(1000), 1),
                (pid(3), c(1000), 2),
            ],
            0,
            c(20),
            c(10),
            PokerRng::from_seed(43),
        );
        hand.start().unwrap();
        // UTG (pid(1)) is first to act preflop in 3-player.
        let actor = hand.snapshot().current_actor.unwrap();
        hand.apply_action(actor, PlayerAction::Fold).unwrap();
        let result_actions = &hand.actions;
        assert_eq!(
            result_actions.len(),
            3,
            "after one voluntary action, 3 total"
        );
        assert_eq!(
            result_actions[2].seq, 2,
            "first voluntary action must have seq=2"
        );
        // All three seqs must be distinct.
        let seqs: Vec<u16> = result_actions.iter().map(|r| r.seq).collect();
        let unique: std::collections::HashSet<u16> = seqs.iter().copied().collect();
        assert_eq!(seqs.len(), unique.len(), "all action seqs must be unique");
    }

    /// Defensive hardening (audit 2026-06-03): an out-of-range `dealer_idx`
    /// must wrap (`% n`) like `blind_positions` instead of panicking with
    /// index-out-of-bounds in `start()` / `snapshot()`.
    #[test]
    fn out_of_range_dealer_idx_wraps_without_panic() {
        // 3 seats but dealer_idx = 5 (>= n). Should wrap to 5 % 3 == 2.
        let mut hand = GameHand::new_with_rng(
            vec![
                (pid(1), c(1000), 0),
                (pid(2), c(1000), 1),
                (pid(3), c(1000), 2),
            ],
            5, // out-of-range dealer index
            c(20),
            c(10),
            PokerRng::from_seed(11),
        );
        // Must not panic.
        hand.start().expect("start with wrapped dealer index");
        let snap = hand.snapshot();
        // dealer_idx 5 % 3 == 2 → seat 2 is the button.
        assert_eq!(
            snap.dealer_seat, 2,
            "out-of-range dealer index must wrap to seat 2"
        );
    }

    /// C6 review: the heads-up (`n == 2`) branch of `blind_positions` must also
    /// apply `% n` to the dealer/SB. `start()` indexes `self.seats[sb_idx]`
    /// BEFORE the `% n` guard on the HandStarted event, so an out-of-range
    /// `dealer_idx` (>= 2) would panic without this. Mirrors the 3+ test above.
    #[test]
    fn heads_up_out_of_range_dealer_idx_wraps_without_panic() {
        // 2 seats but dealer_idx = 4 (>= n). Should wrap to 4 % 2 == 0.
        let mut hand = GameHand::new_with_rng(
            vec![(pid(1), c(1000), 0), (pid(2), c(1000), 1)],
            4, // out-of-range dealer index
            c(20),
            c(10),
            PokerRng::from_seed(11),
        );
        // Must not panic.
        hand.start().expect("HU start with wrapped dealer index");
        let snap = hand.snapshot();
        // dealer_idx 4 % 2 == 0 → seat 0 is the button (HU: dealer is SB).
        assert_eq!(
            snap.dealer_seat, 0,
            "out-of-range HU dealer index must wrap to seat 0"
        );
        // An odd out-of-range index must wrap to seat 1.
        let mut hand2 = GameHand::new_with_rng(
            vec![(pid(1), c(1000), 0), (pid(2), c(1000), 1)],
            5, // 5 % 2 == 1
            c(20),
            c(10),
            PokerRng::from_seed(12),
        );
        hand2
            .start()
            .expect("HU start with wrapped odd dealer index");
        assert_eq!(
            hand2.snapshot().dealer_seat,
            1,
            "out-of-range odd HU dealer index must wrap to seat 1"
        );
    }

    #[test]
    fn usize_max_dealer_idx_wraps_through_a_complete_hand() {
        assert_eq!(blind_positions(usize::MAX, 2), (usize::MAX % 2, 0));
        let dealer = usize::MAX % 3;
        assert_eq!(
            blind_positions(usize::MAX, 3),
            ((dealer + 1) % 3, (dealer + 2) % 3)
        );

        let mut hand = GameHand::new_with_rng(
            vec![
                (pid(1), c(1_000), 0),
                (pid(2), c(1_000), 1),
                (pid(3), c(1_000), 2),
            ],
            usize::MAX,
            c(20),
            c(10),
            PokerRng::from_seed(13),
        );
        hand.start().expect("usize::MAX dealer starts safely");

        while !hand.is_done() {
            let snapshot = hand.snapshot();
            let actor = snapshot.current_actor.expect("live hand has an actor");
            let player = snapshot
                .players
                .iter()
                .find(|player| player.player_id == actor)
                .expect("actor exists");
            let action = if snapshot.current_bet.0 > player.committed_this_street.0 {
                PlayerAction::Call
            } else {
                PlayerAction::Check
            };
            hand.apply_action(actor, action)
                .expect("call/check runout remains legal");
        }

        let result = hand.finish();
        assert_eq!(result.show_order.len(), 3);
    }

    #[test]
    fn street_action_facts_distinguish_all_in_call_short_raise_and_full_raise() {
        struct Case {
            all_in_total: u32,
            increased_bet: bool,
            full_raise: bool,
            call_like: bool,
            expected_last_aggressor: PlayerId,
        }

        let opener = pid(4);
        let all_in_player = pid(1);
        let cases = [
            Case {
                all_in_total: 50,
                increased_bet: false,
                full_raise: false,
                call_like: true,
                expected_last_aggressor: opener,
            },
            Case {
                all_in_total: 100,
                increased_bet: false,
                full_raise: false,
                call_like: true,
                expected_last_aggressor: opener,
            },
            Case {
                all_in_total: 150,
                increased_bet: true,
                full_raise: false,
                call_like: false,
                expected_last_aggressor: all_in_player,
            },
            Case {
                all_in_total: 180,
                increased_bet: true,
                full_raise: true,
                call_like: false,
                expected_last_aggressor: all_in_player,
            },
        ];

        for case in cases {
            // Four handed, dealer pid1: pid2 posts SB, pid3 posts BB, pid4
            // opens first to 100, then pid1 commits the case's exact stack.
            let mut hand = GameHand::new_with_rng(
                vec![
                    (all_in_player, c(case.all_in_total), 0),
                    (pid(2), c(1_000), 1),
                    (pid(3), c(1_000), 2),
                    (opener, c(1_000), 3),
                ],
                0,
                c(20),
                c(10),
                PokerRng::from_seed(u64::from(case.all_in_total)),
            );
            hand.start().expect("start hand");
            assert_eq!(hand.snapshot().current_actor, Some(opener));
            hand.apply_action(opener, PlayerAction::Raise { amount: c(100) })
                .expect("opening raise");
            assert_eq!(hand.snapshot().current_actor, Some(all_in_player));
            hand.apply_action(all_in_player, PlayerAction::AllIn)
                .expect("all-in case is legal");

            let facts = hand.current_street_action_facts();
            assert_eq!(facts.len(), 2);
            assert!(facts[0].increased_bet);
            assert!(facts[0].full_raise);
            assert_eq!(facts[1].increased_bet, case.increased_bet);
            assert_eq!(facts[1].full_raise, case.full_raise);
            assert_eq!(facts[1].is_call_like(), case.call_like);

            let projected = facts[1].strategy_action();
            if case.full_raise {
                assert_eq!(projected, PlayerAction::AllIn);
            } else {
                assert_eq!(projected, PlayerAction::Call);
            }
            assert_eq!(
                facts
                    .iter()
                    .rev()
                    .find(|fact| fact.increased_bet)
                    .map(|fact| fact.player_id),
                Some(case.expected_last_aggressor)
            );
        }
    }

    #[test]
    fn call_like_fact_counts_calls_but_not_short_raises() {
        let fact = |action, increased_bet| StreetActionFact {
            player_id: pid(1),
            action,
            increased_bet,
            full_raise: false,
        };

        assert!(fact(PlayerAction::Call, false).is_call_like());
        assert!(fact(PlayerAction::AllIn, false).is_call_like());
        assert!(fact(PlayerAction::Raise { amount: c(100) }, false).is_call_like());
        assert!(!fact(PlayerAction::AllIn, true).is_call_like());
        assert!(!fact(PlayerAction::Raise { amount: c(150) }, true).is_call_like());
    }

    #[test]
    fn heads_up_fold_preflop() {
        let mut hand = GameHand::new_with_rng(
            vec![(pid(1), c(1000), 0), (pid(2), c(1000), 1)],
            0,
            c(20),
            c(10),
            PokerRng::from_seed(2),
        );
        hand.start().unwrap();
        let actor = hand.snapshot().current_actor.unwrap();
        hand.apply_action(actor, PlayerAction::Fold).unwrap();
        assert!(hand.is_done());
        let result = hand.finish();
        // SB=10 + BB=20 = 30 total. Winner gets 30.
        let total: u32 = result.chips_awarded.values().sum();
        assert_eq!(total, 30);
    }

    #[test]
    fn undealt_board_returns_the_real_preflop_runout_without_advancing_deck() {
        let mut hand = GameHand::new_with_rng(
            vec![(pid(1), c(1000), 0), (pid(2), c(1000), 1)],
            0,
            c(20),
            c(10),
            PokerRng::from_seed(2),
        );
        hand.start().unwrap();
        let actor = hand.snapshot().current_actor.unwrap();
        hand.apply_action(actor, PlayerAction::Fold).unwrap();
        assert!(hand.is_done());

        let before = hand.deck.remaining();
        let runout = hand.undealt_board();

        assert_eq!(runout.len(), 5, "preflop fold leaves five real board cards");
        assert_eq!(
            hand.deck.remaining(),
            before,
            "rabbit hunt must not consume cards"
        );
        assert_eq!(
            runout[0],
            hand.deck.cards()[hand.deck.cards().len() - before]
        );
    }

    #[test]
    fn undealt_board_is_empty_after_a_complete_river() {
        let mut hand = GameHand::new_with_rng(
            vec![(pid(1), c(1000), 0), (pid(2), c(1000), 1)],
            0,
            c(20),
            c(10),
            PokerRng::from_seed(42),
        );
        hand.start().unwrap();

        // Both players check/call through every street. The engine deals the
        // flop, turn, and river before reaching Done.
        loop {
            let actor = hand.snapshot().current_actor;
            let Some(actor) = actor else { break };
            let snap = hand.snapshot();
            let action = if snap.current_bet
                > snap
                    .players
                    .iter()
                    .find(|p| p.player_id == actor)
                    .map(|p| p.committed_this_street)
                    .unwrap_or(Chips::ZERO)
            {
                PlayerAction::Call
            } else {
                PlayerAction::Check
            };
            hand.apply_action(actor, action).unwrap();
        }
        assert!(hand.is_done());
        assert_eq!(hand.snapshot().board.count(), 5);
        assert!(hand.undealt_board().is_empty());
    }

    /// C6 review: a `Call` issued in a checkable spot (`to_call == 0`) must be
    /// recorded/emitted as a `Check`, not as an illegal zero-chip `call` — but
    /// it must NOT error (a client may legitimately send Call there). 0 chips
    /// move either way and the pot/state transition is unchanged; only the
    /// recorded label differs.
    #[test]
    fn zero_chip_call_recorded_as_check() {
        // HU, dealer=0: pid(1)=SB acts first preflop, pid(2)=BB.
        let mut hand = GameHand::new_with_rng(
            vec![(pid(1), c(1000), 0), (pid(2), c(1000), 1)],
            0,
            c(20), // big blind
            c(10), // small blind
            PokerRng::from_seed(9),
        );
        hand.start().unwrap();

        // SB completes (calls 10 → matches the 20 big blind).
        let sb = hand.snapshot().current_actor.unwrap();
        assert_eq!(sb, pid(1), "SB acts first preflop in HU");
        hand.apply_action(sb, PlayerAction::Call).unwrap();

        // BB now faces to_call == 0 (already in for the full current bet).
        let snap_before = hand.snapshot();
        let bb = snap_before.current_actor.unwrap();
        assert_eq!(bb, pid(2), "BB has the option to check or close the street");
        let pot_before = snap_before.pot;
        let bb_stack_before = snap_before
            .players
            .iter()
            .find(|p| p.player_id == bb)
            .unwrap()
            .stack;

        // BB sends Call in a checkable spot — must succeed (not rejected).
        hand.apply_action(bb, PlayerAction::Call)
            .expect("zero-chip Call must be accepted");

        // It must be RECORDED as a Check, never as a zero-chip call.
        let last = hand.actions.last().unwrap();
        assert_eq!(last.player_id, bb);
        assert_eq!(
            last.action,
            PlayerAction::Check,
            "zero-chip Call must be relabelled to Check in action history"
        );
        assert_eq!(last.amount, Chips::ZERO, "zero chips move either way");

        // Pot and BB stack are unchanged by the zero-chip action.
        assert_eq!(
            last.pot_before, last.pot_after,
            "pot unchanged by zero-chip action"
        );
        let snap_after = hand.snapshot();
        let bb_stack_after = snap_after
            .players
            .iter()
            .find(|p| p.player_id == bb)
            .unwrap()
            .stack;
        assert_eq!(bb_stack_before, bb_stack_after, "BB stack unchanged");
        // Total pot is unchanged from the moment before BB acted.
        assert!(
            snap_after.pot >= pot_before,
            "pot must not shrink: {} -> {}",
            pot_before.0,
            snap_after.pot.0
        );
        assert_eq!(
            snap_after.pot, pot_before,
            "zero-chip action must not change the pot"
        );

        // The relabelled Check must also appear as the last_action for BB.
        assert_eq!(
            hand.last_action.get(&bb.inner()),
            Some(&PlayerAction::Check),
            "last_action must reflect the relabelled Check"
        );
    }

    /// A-002: In heads-up (n=2) the dealer is the SB and the non-dealer is the BB.
    /// After start(), dealer.stack == initial - small_blind,
    /// non_dealer.stack == initial - big_blind.
    #[test]
    fn heads_up_dealer_is_small_blind() {
        let initial = 1000u32;
        let sb_amt = 10u32;
        let bb_amt = 20u32;
        // dealer_idx = 0, so seat 0 is dealer/SB, seat 1 is BB.
        let mut hand = GameHand::new_with_rng(
            vec![(pid(1), c(initial), 0), (pid(2), c(initial), 1)],
            0, // dealer_idx = 0
            c(bb_amt),
            c(sb_amt),
            PokerRng::from_seed(3),
        );
        hand.start().unwrap();
        let snap = hand.snapshot();
        let dealer_player = snap.players.iter().find(|p| p.player_id == pid(1)).unwrap();
        let non_dealer_player = snap.players.iter().find(|p| p.player_id == pid(2)).unwrap();
        // Dealer (seat 0, dealer_idx=0) must have posted the small blind.
        assert_eq!(
            dealer_player.stack.0,
            initial - sb_amt,
            "dealer should have posted small blind"
        );
        // Non-dealer (seat 1) must have posted the big blind.
        assert_eq!(
            non_dealer_player.stack.0,
            initial - bb_amt,
            "non-dealer should have posted big blind"
        );
    }

    /// A-002: 3-way: dealer stack unchanged, seat after dealer is SB, next is BB.
    #[test]
    fn three_way_standard_blind_positions() {
        let initial = 1000u32;
        let sb_amt = 10u32;
        let bb_amt = 20u32;
        // dealer_idx = 0: dealer=pid(1), SB=pid(2), BB=pid(3)
        let mut hand = GameHand::new_with_rng(
            vec![
                (pid(1), c(initial), 0),
                (pid(2), c(initial), 1),
                (pid(3), c(initial), 2),
            ],
            0, // dealer_idx = 0
            c(bb_amt),
            c(sb_amt),
            PokerRng::from_seed(4),
        );
        hand.start().unwrap();
        let snap = hand.snapshot();
        let dealer_p = snap.players.iter().find(|p| p.player_id == pid(1)).unwrap();
        let sb_p = snap.players.iter().find(|p| p.player_id == pid(2)).unwrap();
        let bb_p = snap.players.iter().find(|p| p.player_id == pid(3)).unwrap();
        assert_eq!(
            dealer_p.stack.0, initial,
            "dealer should not post any blind"
        );
        assert_eq!(sb_p.stack.0, initial - sb_amt, "seat after dealer is SB");
        assert_eq!(
            bb_p.stack.0,
            initial - bb_amt,
            "seat two after dealer is BB"
        );
    }

    /// Regression (audit 2026-06-03): a fold-around HandResult's lone showdown
    /// entry must carry a non-empty `rank_name` (consistent with the contested
    /// branch), not a real `rank` next to an empty string.
    #[test]
    fn fold_around_showdown_entry_has_rank_name() {
        let mut hand = GameHand::new_with_rng(
            vec![(pid(1), c(1000), 0), (pid(2), c(1000), 1)],
            0,
            c(20),
            c(10),
            PokerRng::from_seed(5),
        );
        hand.start().unwrap();
        let actor = hand.snapshot().current_actor.unwrap();
        hand.apply_action(actor, PlayerAction::Fold).unwrap();
        assert!(hand.is_done());
        let result = hand.finish();
        assert_eq!(
            result.showdown.len(),
            1,
            "fold-around has one showdown entry"
        );
        let entry = &result.showdown[0];
        assert!(
            !entry.rank_name.is_empty(),
            "lone showdown entry must carry a rank_name"
        );
        assert_eq!(
            entry.rank_name,
            entry.rank.name(),
            "rank_name must match the computed rank"
        );
    }

    #[test]
    fn heads_up_all_in_preflop_completes_without_panic() {
        let mut hand = GameHand::new_with_rng(
            vec![(pid(1), c(1000), 0), (pid(2), c(1000), 1)],
            0,
            c(20),
            c(10),
            PokerRng::from_seed(1),
        );
        hand.start().unwrap();
        loop {
            if hand.is_done() {
                break;
            }
            let actor = match hand.snapshot().current_actor {
                Some(a) => a,
                None => break,
            };
            hand.apply_action(actor, PlayerAction::AllIn).unwrap();
        }
        let result = hand.finish();
        let total: u32 = result.chips_awarded.values().sum();
        assert_eq!(total, 2000);
    }

    /// REGRESSION (bug-sweep 2026-05-29, finding #5): a side pot whose entire
    /// eligible set folded (no contender reaches showdown) must NOT silently
    /// destroy chips — `distribute_pots` returns the uncontested layer equally
    /// to its eligible contributors. Chip conservation (chips in == chips out)
    /// is the canonical engine invariant and must hold in this degenerate "no
    /// contest" case.
    ///
    /// This exercises the `contenders.is_empty()` branch of `distribute_pots`
    /// DIRECTLY. It used to be driven through a full HU+side-pot hand where both
    /// deep stacks folded the side pot away on the turn — but the 2026-05-29
    /// all-in-runout fix in `BettingRound::check_done` now closes betting the
    /// instant only one live stack remains owing nothing, so that black-box path
    /// can no longer reach a fully-folded side pot (see
    /// `all_but_one_all_in_runs_out_to_showdown`). Testing the function directly
    /// keeps the refund branch covered regardless of black-box reachability.
    #[test]
    fn orphaned_side_pot_does_not_destroy_chips() {
        // A 200-chip side pot whose entire eligible set {A=pid(2), B=pid(3)}
        // folded: neither appears in the showdown card set, so the pot has zero
        // contenders. (The only other player — an all-in short stack — is not
        // eligible for this higher side-pot layer.)
        let side_pots = vec![SidePot {
            cap: c(100),
            amount: c(200),
            eligible: vec![pid(2), pid(3)],
            refund_to: Vec::new(),
        }];
        let no_contenders: Vec<(PlayerId, HoleCards)> = Vec::new();
        let button_order = HashMap::new();
        let results = distribute_pots(
            &side_pots,
            &no_contenders,
            &BoardCards::empty(),
            &button_order,
            &HashSet::new(),
        );

        // The pot must be RETURNED, not dropped: one PotResult covering the full
        // 200, split equally between the two eligible contributors.
        assert_eq!(
            results.len(),
            1,
            "uncontested side pot must be refunded, not dropped"
        );
        let refunded: u32 = results[0].winners.iter().map(|w| w.amount_won.0).sum();
        assert_eq!(refunded, 200, "chip conservation: full pot amount refunded");
        let mut winners: Vec<u64> = results[0]
            .winners
            .iter()
            .map(|w| w.player_id.inner())
            .collect();
        winners.sort_unstable();
        assert_eq!(
            winners,
            vec![pid(2).inner(), pid(3).inner()],
            "refund goes to the eligible contributors"
        );
    }

    /// REGRESSION (2026-05-29): all-but-one all-in must run the board out
    /// automatically instead of prompting the lone chip-holder street by street.
    ///
    /// Reproduces the reported production hand: heads-up, the short stack shoves
    /// all-in pre-flop and the deep stack CALLS. There is no betting decision
    /// left (only one player can act and owes nothing), so the engine must NOT
    /// expose a `current_actor` on the flop — it must deal flop/turn/river to
    /// showdown on its own. Before the `check_done` fix the deep stack was asked
    /// to Fold/Raise/Check on every street. Distinct from
    /// `heads_up_all_in_preflop_completes_without_panic`, where BOTH players are
    /// all-in (n_can_act == 0); here the deep stack still has chips behind
    /// (n_can_act == 1).
    #[test]
    fn all_but_one_all_in_runs_out_to_showdown() {
        // dealer=0 => pid(1) is the button/SB and acts first pre-flop heads-up.
        let villain_start = 100u32; // short — will shove
        let hero_start = 1000u32; // deep — calls and keeps chips behind
        let mut hand = GameHand::new_with_rng(
            vec![
                (pid(1), c(villain_start), 0), // villain — SB, short
                (pid(2), c(hero_start), 1),    // hero — BB, deep
            ],
            0,
            c(20), // big blind
            c(10), // small blind
            PokerRng::from_seed(7),
        );
        hand.start().unwrap();
        let total_start = villain_start + hero_start;

        // Pre-flop: villain (SB) shoves all-in 100, hero (BB) calls to 100.
        assert_eq!(
            hand.snapshot().current_actor,
            Some(pid(1)),
            "button acts first heads-up pre-flop"
        );
        hand.apply_action(pid(1), PlayerAction::AllIn).unwrap();
        let hero = hand.snapshot().current_actor.unwrap();
        assert_eq!(hero, pid(2), "hero (BB) acts after the shove");
        hand.apply_action(hero, PlayerAction::Call).unwrap();

        // The instant the call matches the shove, betting is closed for the rest
        // of the hand: the board runs out and the hand completes with NO actor.
        assert!(
            hand.is_done(),
            "all-but-one all-in must run out to showdown, not prompt the deep stack"
        );
        assert!(
            hand.snapshot().current_actor.is_none(),
            "no current_actor once only one live stack remains with nothing owed"
        );

        // Chip conservation across the auto-runout.
        let result = hand.finish();
        let total_final: u32 = result.final_stacks.values().sum();
        assert_eq!(
            total_final, total_start,
            "chip conservation across all-in runout: {total_final} vs {total_start}"
        );
    }

    /// B-M4-001: After one player folds preflop in a 3-player game, that player
    /// must NOT appear as current_actor on the flop, turn, or river.
    #[test]
    fn three_player_fold_preflop_actor_never_revisited() {
        // dealer=0, SB=pid(2) (seat 1), BB=pid(3) (seat 2), UTG=pid(1) (seat 0) acts first preflop.
        let mut hand = GameHand::new_with_rng(
            vec![
                (pid(1), c(500), 0),
                (pid(2), c(500), 1),
                (pid(3), c(500), 2),
            ],
            0, // dealer_idx = 0
            c(20),
            c(10),
            PokerRng::from_seed(7),
        );
        hand.start().unwrap();

        // pid(1) is UTG — fold preflop.
        let snap = hand.snapshot();
        assert_eq!(
            snap.current_actor,
            Some(pid(1)),
            "UTG should act first preflop"
        );
        hand.apply_action(pid(1), PlayerAction::Fold).unwrap();

        // SB (pid(2)) and BB (pid(3)) complete preflop.
        let snap = hand.snapshot();
        let actor = snap.current_actor.unwrap();
        hand.apply_action(actor, PlayerAction::Call).unwrap(); // SB calls
        let snap = hand.snapshot();
        if let Some(actor) = snap.current_actor {
            hand.apply_action(actor, PlayerAction::Check).unwrap(); // BB checks
        }

        // Now on flop (or hand could be done in edge cases — either way, pid(1) must never be actor).
        let mut safety = 0;
        loop {
            if hand.is_done() {
                break;
            }
            safety += 1;
            if safety > 30 {
                panic!("too many iterations");
            }
            let snap = hand.snapshot();
            let actor = match snap.current_actor {
                Some(a) => a,
                None => break,
            };
            assert_ne!(
                actor,
                pid(1),
                "folded player pid(1) must never be current_actor; street={:?}",
                snap.street
            );
            // Check/call to advance.
            let committed = snap
                .players
                .iter()
                .find(|p| p.player_id == actor)
                .map(|p| p.committed_this_street.0)
                .unwrap_or(0);
            let to_call = snap.current_bet.0.saturating_sub(committed);
            if to_call > 0 {
                hand.apply_action(actor, PlayerAction::Call).unwrap();
            } else {
                hand.apply_action(actor, PlayerAction::Check).unwrap();
            }
        }
        let result = hand.finish();
        // Total pot = 10 (SB) + 20 (BB) + 20 (UTG call... wait UTG folded, so only SB+BB = 30).
        // Actually SB called (20) + BB checked: pot = 20 + 20 = 40. pid(1) folded for 0.
        let total: u32 = result.chips_awarded.values().sum();
        assert!(total > 0, "winner must receive chips");
        // Verify pid(1) is not in showdown winners.
        for pot in &result.pots {
            for winner in &pot.winners {
                assert_ne!(winner.player_id, pid(1), "folded player cannot win");
            }
        }
    }

    /// P0-1 regression: when all players fold preflop except one,
    /// the engine must immediately end the hand with NO community cards.
    /// Board length == 0 (no flop/turn/river).
    #[test]
    fn fold_around_preflop_skips_board() {
        // dealer=pid(1), SB=pid(2), BB=pid(3), UTG=pid(1) acts first preflop.
        let mut hand = GameHand::new_with_rng(
            vec![
                (pid(1), c(500), 0),
                (pid(2), c(500), 1),
                (pid(3), c(500), 2),
            ],
            0, // dealer_idx = 0
            c(20),
            c(10),
            PokerRng::from_seed(42),
        );
        hand.start().unwrap();

        // UTG (pid1) folds first.
        let snap = hand.snapshot();
        let actor = snap.current_actor.unwrap();
        assert_eq!(actor, pid(1), "UTG should act first preflop");
        hand.apply_action(actor, PlayerAction::Fold).unwrap();

        // SB (pid2) folds.
        let snap = hand.snapshot();
        let actor = snap.current_actor.unwrap();
        assert_eq!(actor, pid(2), "SB should act next");
        hand.apply_action(actor, PlayerAction::Fold).unwrap();

        // Only BB (pid3) remains — hand must terminate immediately.
        assert!(
            hand.is_done(),
            "fold-around: hand must end immediately when only 1 player remains"
        );

        let snap = hand.snapshot();
        // Board must be empty — no community cards when fold-around.
        assert!(
            snap.board.flop.is_none(),
            "fold-around preflop: flop must NOT be dealt"
        );
        assert!(
            snap.board.turn.is_none(),
            "fold-around preflop: turn must NOT be dealt"
        );
        assert!(
            snap.board.river.is_none(),
            "fold-around preflop: river must NOT be dealt"
        );
    }

    /// Regression (audit 2026-06-03, TDA Rule 25): the odd chip of a split pot
    /// goes to the tied winner in the EARLIEST position (lowest button-relative
    /// order / first seat left of the button), not to whoever happens to be
    /// first in `pot.eligible` (action) order.
    #[test]
    fn odd_chip_goes_to_earliest_position_winner() {
        // Board is a Broadway straight both players play → exact tie.
        let board = BoardCards {
            flop: Some([
                card(Rank::Ace, Suit::Clubs),
                card(Rank::King, Suit::Hearts),
                card(Rank::Queen, Suit::Diamonds),
            ]),
            turn: Some(card(Rank::Jack, Suit::Clubs)),
            river: Some(card(Rank::Ten, Suit::Spades)),
        };
        // Both hole sets are dominated by the board (play-the-board straight).
        let p1_hole = HoleCards::new(
            card(Rank::Two, Suit::Hearts),
            card(Rank::Three, Suit::Hearts),
        );
        let p3_hole = HoleCards::new(
            card(Rank::Four, Suit::Diamonds),
            card(Rank::Five, Suit::Diamonds),
        );
        let eligible = vec![(pid(1), p1_hole), (pid(3), p3_hole)];
        // 101-chip pot, eligible in action order [pid(1), pid(3)].
        let pots = vec![SidePot {
            cap: c(101),
            amount: c(101),
            eligible: vec![pid(1), pid(3)],
            refund_to: Vec::new(),
        }];

        // Case A: pid(3) is earliest position (order 0) → it gets the odd chip
        // even though pid(1) is first in pot.eligible.
        let mut order_a = HashMap::new();
        order_a.insert(pid(3).inner(), 0usize);
        order_a.insert(pid(1).inner(), 1usize);
        let res_a = distribute_pots(&pots, &eligible, &board, &order_a, &HashSet::new());
        assert_eq!(res_a.len(), 1);
        let p3_won_a = res_a[0]
            .winners
            .iter()
            .find(|w| w.player_id == pid(3))
            .map(|w| w.amount_won.0)
            .unwrap();
        let p1_won_a = res_a[0]
            .winners
            .iter()
            .find(|w| w.player_id == pid(1))
            .map(|w| w.amount_won.0)
            .unwrap();
        assert_eq!(p3_won_a, 51, "earliest-position pid(3) gets the odd chip");
        assert_eq!(p1_won_a, 50);
        assert_eq!(p3_won_a + p1_won_a, 101, "chip conservation");

        // Case B: pid(1) is earliest position (order 0) → it gets the odd chip.
        let mut order_b = HashMap::new();
        order_b.insert(pid(1).inner(), 0usize);
        order_b.insert(pid(3).inner(), 1usize);
        let res_b = distribute_pots(&pots, &eligible, &board, &order_b, &HashSet::new());
        let p1_won_b = res_b[0]
            .winners
            .iter()
            .find(|w| w.player_id == pid(1))
            .map(|w| w.amount_won.0)
            .unwrap();
        assert_eq!(p1_won_b, 51, "earliest-position pid(1) gets the odd chip");
    }

    #[test]
    fn three_player_full_hand_no_panic() {
        let mut hand = GameHand::new_with_rng(
            vec![
                (pid(1), c(500), 0),
                (pid(2), c(500), 1),
                (pid(3), c(500), 2),
            ],
            0,
            c(20),
            c(10),
            PokerRng::from_seed(99),
        );
        hand.start().unwrap();
        // Everyone checks/calls to showdown.
        let mut safety = 0;
        loop {
            if hand.is_done() {
                break;
            }
            safety += 1;
            if safety > 50 {
                panic!("too many iterations");
            }
            let snap = hand.snapshot();
            let actor = match snap.current_actor {
                Some(a) => a,
                None => break,
            };
            let to_call = snap.current_bet.0.saturating_sub(
                snap.players
                    .iter()
                    .find(|p| p.player_id == actor)
                    .map(|p| p.committed_this_street.0)
                    .unwrap_or(0),
            );
            if to_call > 0 {
                hand.apply_action(actor, PlayerAction::Call).unwrap();
            } else {
                hand.apply_action(actor, PlayerAction::Check).unwrap();
            }
        }
        let result = hand.finish();
        let total: u32 = result.chips_awarded.values().sum();
        assert_eq!(total, 60); // 3 * 20
    }
}

// ---------------------------------------------------------------------------
// Blind mode (ADR-066 P1) tests
// ---------------------------------------------------------------------------
#[cfg(test)]
mod blind_tests {
    use super::*;
    use crate::card::{Card, Rank, Suit};

    fn pid(n: u64) -> PlayerId {
        PlayerId::new(n)
    }
    fn c(n: u32) -> Chips {
        Chips(n)
    }
    fn card(r: Rank, s: Suit) -> Card {
        Card::new(r, s)
    }
    fn hole(a: Card, b: Card) -> HoleCards {
        HoleCards::new(a, b)
    }

    /// Drive blind-mode betting to completion, injecting boards as the engine
    /// requests them (`pending_board_street`). Returns once `is_done()`.
    /// `act` decides the action for the current actor.
    fn run_blind_betting(
        hand: &mut GameHand,
        board_cards: &[Card],
        mut act: impl FnMut(&GameSnapshot, PlayerId) -> PlayerAction,
    ) {
        // board_cards is the full 5-card board in flop(3)/turn(1)/river(1) order;
        // we feed slices as each street's board is requested.
        let mut board_offset = 0usize;
        let mut safety = 0;
        loop {
            if hand.is_done() {
                break;
            }
            safety += 1;
            assert!(safety < 200, "blind betting loop runaway");

            // Inject any pending board before acting on the new street.
            if let Some(street) = hand.pending_board_street() {
                let n = match street {
                    Street::Flop => 3,
                    Street::Turn | Street::River => 1,
                    Street::Preflop => 0,
                };
                let slice = &board_cards[board_offset..board_offset + n];
                board_offset += n;
                hand.inject_board_for_street(slice).expect("inject board");
                continue;
            }

            let snap = hand.snapshot();
            let actor = match snap.current_actor {
                Some(a) => a,
                None => break,
            };
            let action = act(&snap, actor);
            hand.apply_action(actor, action).expect("apply action");
        }
        // Drain any trailing pending board (all-in runout can leave the last
        // street's board pending with the round already done).
        while let Some(street) = hand.pending_board_street() {
            let n = match street {
                Street::Flop => 3,
                Street::Turn | Street::River => 1,
                Street::Preflop => 0,
            };
            let slice = &board_cards[board_offset..board_offset + n];
            board_offset += n;
            hand.inject_board_for_street(slice)
                .expect("inject trailing board");
        }
    }

    /// A standard distinct 5-card board (no overlap with the hole cards used in
    /// these tests). Order: flop[0..3], turn[3], river[4].
    fn standard_board() -> [Card; 5] {
        [
            card(Rank::Two, Suit::Clubs),
            card(Rank::Seven, Suit::Diamonds),
            card(Rank::Nine, Suit::Spades),
            card(Rank::Jack, Suit::Hearts),
            card(Rank::Four, Suit::Clubs),
        ]
    }

    // ---- 1. blind-mode betting runs a full hand with no hole values ----

    /// A full blind-mode hand runs its entire betting + board reveals with the
    /// engine holding NO plaintext hole values: every seat stays `Opaque` and
    /// every snapshot redacts `hole_cards` to `None` until a reveal is injected.
    #[test]
    fn blind_full_hand_no_hole_values() {
        let mut hand = GameHand::new_blind(
            vec![(pid(1), c(1000), 0), (pid(2), c(1000), 1)],
            0,
            c(20),
            c(10),
            DeckSeed::default(),
        );
        hand.start().unwrap();

        // Blind start emits NO HoleCardsDealt event at all (no plaintext value
        // ever leaves the engine; no redacted variant is introduced either).
        let evs = hand.drain_events();
        assert!(
            evs.iter()
                .all(|e| !matches!(e, EngineEvent::HoleCardsDealt { .. })),
            "blind start must NOT emit any HoleCardsDealt event"
        );

        // Every snapshot during betting: hole_cards is None for all seats.
        let snap = hand.snapshot();
        assert!(
            snap.players.iter().all(|p| p.hole_cards.is_none()),
            "no plaintext hole values in a blind snapshot before showdown"
        );

        let board = standard_board();
        run_blind_betting(&mut hand, &board, |snap, actor| {
            // Everyone checks/calls to showdown.
            let committed = snap
                .players
                .iter()
                .find(|p| p.player_id == actor)
                .map(|p| p.committed_this_street.0)
                .unwrap_or(0);
            let to_call = snap.current_bet.0.saturating_sub(committed);
            if to_call > 0 {
                PlayerAction::Call
            } else {
                PlayerAction::Check
            }
        });

        assert!(hand.is_done());
        // Full board got revealed via injection.
        let snap = hand.snapshot();
        assert_eq!(snap.board.count(), 5, "full board revealed via injection");
        // Still no plaintext holes (no reveals injected yet).
        assert!(snap.players.iter().all(|p| p.hole_cards.is_none()));

        // STRUCTURAL: right up to showdown, EVERY seat slot is still `Opaque` —
        // no `Plain`/`Revealed` was ever written in blind mode. There is no code
        // path that produces a plaintext hole value before an injected reveal.
        assert!(
            hand.seats.iter().all(|s| s.hole == HoleSlot::Opaque),
            "every blind seat remains Opaque before any showdown reveal"
        );

        // Two contenders → finish_blind requires reveals; without them it errors.
        let err = hand.finish_blind().unwrap_err();
        assert!(matches!(err, BlindFinishError::MissingReveal { .. }));
    }

    // ---- 2. fold-around / last-man-standing: NO reveal, NO rank_hand ----

    /// Heads-up fold pre-flop: the lone survivor takes the pot with NO injected
    /// reveal and an EMPTY showdown — structural zero-leak (rank_hand never runs
    /// because finish_blind takes the uncontested path).
    #[test]
    fn blind_fold_around_awards_without_reveal() {
        let mut hand = GameHand::new_blind(
            vec![(pid(1), c(1000), 0), (pid(2), c(1000), 1)],
            0,
            c(20),
            c(10),
            DeckSeed::default(),
        );
        hand.start().unwrap();
        // SB (button, pid1) acts first heads-up — fold.
        let actor = hand.snapshot().current_actor.unwrap();
        assert_eq!(actor, pid(1));
        hand.apply_action(actor, PlayerAction::Fold).unwrap();
        assert!(hand.is_done());

        // No reveal injected. finish_blind awards the lone survivor and produces
        // an empty showdown.
        let result = hand.finish_blind().expect("uncontested finish");
        assert!(
            result.showdown.is_empty(),
            "uncontested blind hand has an EMPTY showdown (zero leak)"
        );
        assert!(
            result.show_order.is_empty(),
            "no show order for an uncontested hand"
        );
        // Winner is pid(2) (BB), takes SB(10)+BB(20)=30.
        let total: u32 = result.chips_awarded.values().sum();
        assert_eq!(total, 30, "pot fully awarded");
        assert_eq!(
            *result.chips_awarded.get(&pid(2).inner()).unwrap(),
            30,
            "lone survivor (BB) wins the whole pot"
        );
        // Board must be empty — fold-around reveals no community cards.
        assert_eq!(result.board.count(), 0);
    }

    /// 3-handed: two fold, the lone survivor wins with no reveal and no board.
    #[test]
    fn blind_last_man_standing_three_handed() {
        let mut hand = GameHand::new_blind(
            vec![
                (pid(1), c(500), 0),
                (pid(2), c(500), 1),
                (pid(3), c(500), 2),
            ],
            0,
            c(20),
            c(10),
            DeckSeed::default(),
        );
        hand.start().unwrap();
        // UTG (pid1) folds, SB (pid2) folds → BB (pid3) wins.
        let a = hand.snapshot().current_actor.unwrap();
        hand.apply_action(a, PlayerAction::Fold).unwrap();
        let a = hand.snapshot().current_actor.unwrap();
        hand.apply_action(a, PlayerAction::Fold).unwrap();
        assert!(hand.is_done());

        let result = hand.finish_blind().expect("uncontested finish");
        assert!(result.showdown.is_empty());
        assert_eq!(
            *result.chips_awarded.get(&pid(3).inner()).unwrap(),
            30,
            "lone survivor wins SB+BB"
        );
    }

    // ---- 3. 2-player showdown with injected reveals ranks the correct winner ----

    /// Heads-up showdown: with both reveals injected, finish_blind ranks the
    /// correct winner from ONLY the injected reveals.
    #[test]
    fn blind_two_player_showdown_ranks_correct_winner() {
        let mut hand = GameHand::new_blind(
            vec![(pid(1), c(1000), 0), (pid(2), c(1000), 1)],
            0,
            c(20),
            c(10),
            DeckSeed::default(),
        );
        hand.start().unwrap();

        let board = standard_board(); // 2c 7d 9s | Jh | 4c
        run_blind_betting(&mut hand, &board, |snap, actor| {
            let committed = snap
                .players
                .iter()
                .find(|p| p.player_id == actor)
                .map(|p| p.committed_this_street.0)
                .unwrap_or(0);
            let to_call = snap.current_bet.0.saturating_sub(committed);
            if to_call > 0 {
                PlayerAction::Call
            } else {
                PlayerAction::Check
            }
        });
        assert!(hand.is_done());

        // pid(1) holds a pair of nines (9h+9? — use 9d+Ah → pair of nines, A kicker)
        // pid(2) holds A-K offsuit → ace-high only. pid(1) must win with the pair.
        let p1 = hole(
            card(Rank::Nine, Suit::Diamonds),
            card(Rank::Ace, Suit::Hearts),
        );
        let p2 = hole(card(Rank::Ace, Suit::Spades), card(Rank::King, Suit::Clubs));

        hand.inject_showdown_reveal(pid(1), p1).expect("reveal p1");
        hand.inject_showdown_reveal(pid(2), p2).expect("reveal p2");

        let result = hand.finish_blind().expect("contested finish");
        assert_eq!(
            result.showdown.len(),
            2,
            "two reveals → two showdown entries"
        );
        // The pot (BB*2 = 40) goes entirely to pid(1) (pair of 9s > ace high).
        assert_eq!(
            *result.chips_awarded.get(&pid(1).inner()).unwrap(),
            40,
            "pair of nines beats ace-high"
        );
        assert_eq!(*result.chips_awarded.get(&pid(2).inner()).unwrap(), 0);
        // show_order is populated for a contested showdown.
        assert_eq!(result.show_order.len(), 2);
    }

    // ---- 4. 3-way all-in with side pots + injected reveals distributes correctly ----

    /// Three players with unequal stacks all-in pre-flop → a main pot + side pot.
    /// With all three reveals injected, finish_blind distributes both layers to
    /// the correct winners.
    #[test]
    fn blind_three_way_all_in_side_pots() {
        // Stacks 100 / 200 / 300 → all-in creates layered pots.
        let mut hand = GameHand::new_blind(
            vec![
                (pid(1), c(100), 0), // short
                (pid(2), c(200), 1), // mid
                (pid(3), c(300), 2), // deep
            ],
            0,
            c(20),
            c(10),
            DeckSeed::default(),
        );
        hand.start().unwrap();

        // The short and mid stacks shove; the deep stack is then the sole live
        // stack and calls the 200 wager, retaining its unmatched 100 instead of
        // manufacturing a dry side pot. Betting closes and the board runs out.
        let board = standard_board();
        run_blind_betting(&mut hand, &board, |_snap, actor| {
            if actor == pid(3) {
                PlayerAction::Call
            } else {
                PlayerAction::AllIn
            }
        });
        assert!(hand.is_done());
        let snap = hand.snapshot();
        assert_eq!(snap.board.count(), 5, "all-in runout reveals full board");

        // Inject reveals. Give the SHORT stack (pid1) the best hand so it scoops
        // the main pot it is eligible for; pid3 (deep) beats pid2 for the side
        // pot. Board: 2c 7d 9s Jh 4c.
        //   pid1: Js Jd → trips jacks (Jh on board) → strongest.
        //   pid3: 9h 9c → trips nines (9s on board) → second.
        //   pid2: A-K   → ace high → weakest.
        let p1 = hole(
            card(Rank::Jack, Suit::Spades),
            card(Rank::Jack, Suit::Diamonds),
        );
        let p2 = hole(
            card(Rank::Ace, Suit::Diamonds),
            card(Rank::King, Suit::Hearts),
        );
        let p3 = hole(
            card(Rank::Nine, Suit::Hearts),
            card(Rank::Nine, Suit::Clubs),
        );
        hand.inject_showdown_reveal(pid(1), p1).unwrap();
        hand.inject_showdown_reveal(pid(2), p2).unwrap();
        hand.inject_showdown_reveal(pid(3), p3).unwrap();

        let result = hand.finish_blind().expect("contested side-pot finish");

        // Chip conservation: 100 + 200 + 300 = 600 total in play.
        let total_final: u32 = result.final_stacks.values().sum();
        assert_eq!(total_final, 600, "chip conservation across blind side pots");

        // pid1 (trips jacks) wins the main pot (all three contribute 100 → 300).
        // pid3 (trips nines) beats pid2 for the 200-chip second layer (pid2,pid3
        // contribute 100 each above the 100 cap). pid3's unmatched top 100 never
        // enters the pot.
        let p1_award = *result.chips_awarded.get(&pid(1).inner()).unwrap();
        let p3_award = *result.chips_awarded.get(&pid(3).inner()).unwrap();
        let p2_award = *result.chips_awarded.get(&pid(2).inner()).unwrap();
        assert_eq!(p1_award, 300, "short stack scoops the 300 main pot");
        assert_eq!(p2_award, 0, "mid stack (ace high) wins nothing");
        assert_eq!(p3_award, 200, "deep stack wins only the contested side pot");
        assert_eq!(
            result.final_stacks.get(&pid(3).inner()),
            Some(&300),
            "deep stack retains 100 and wins the 200 side pot"
        );
    }

    // ---- 4b. forfeit economics: a side pot eligible ONLY to forfeiters must
    //          NOT be refunded to the forfeiters (ADR-066 §5 T-LIVE / audit F3) ----

    /// Three players with unequal stacks go all-in pre-flop → a main pot
    /// (eligible to all three) + a side pot (eligible only to the two deep
    /// stacks). At showdown ONLY the short stack reveals; both deep stacks
    /// FORFEIT (withhold their reveal).
    ///
    /// The bug (audit F3): the side pot's eligible set is exactly the two
    /// forfeiters, so `distribute_pots` finds no contender for it and refunds
    /// the layer to its eligible contributors — i.e. it hands the forfeiters
    /// their side-pot chips back. ADR-066 §5 forbids that: a forfeiter's
    /// committed chips "stay in the pot and are awarded to the contenders who DID
    /// reveal." The honest revealer (the short stack) must receive the forfeited
    /// side pot, NOT the forfeiters. Chips are conserved either way, so the
    /// conservation assert cannot catch this — only an explicit per-player award
    /// check does.
    #[test]
    fn blind_forfeited_side_pot_goes_to_revealer_not_refunded() {
        // Stacks 100 / 1000 / 1000. Short stack (pid1) all-in for 100; the two
        // deep stacks (pid2, pid3) call all-in for 1000 each. Layers:
        //   main pot  = 100 * 3 = 300, eligible {pid1, pid2, pid3}
        //   side pot  = 900 * 2 = 1800, eligible {pid2, pid3} only
        let mut hand = GameHand::new_blind(
            vec![
                (pid(1), c(100), 0),  // short
                (pid(2), c(1000), 1), // deep
                (pid(3), c(1000), 2), // deep
            ],
            0,
            c(20),
            c(10),
            DeckSeed::default(),
        );
        hand.start().unwrap();

        let board = standard_board();
        run_blind_betting(&mut hand, &board, |_snap, _actor| PlayerAction::AllIn);
        assert!(hand.is_done());
        assert_eq!(hand.snapshot().board.count(), 5);

        // ONLY the short stack reveals. Give it any hand (it is the sole honest
        // revealer, so it wins the main pot it is eligible for, and per ADR-066
        // §5 must also receive the forfeited side pot). pid2 / pid3 WITHHOLD.
        let p1 = hole(
            card(Rank::Jack, Suit::Spades),
            card(Rank::Jack, Suit::Diamonds),
        );
        hand.inject_showdown_reveal(pid(1), p1).unwrap();
        // pid2 and pid3 do NOT reveal → forfeit at finish_blind_with_forfeits.

        let result = hand
            .finish_blind_with_forfeits()
            .expect("a single honest revealer wins by forfeit, not void");

        // Chip conservation (cannot catch the bug, but must always hold).
        let total_final: u32 = result.final_stacks.values().sum();
        assert_eq!(total_final, 2100, "chip conservation: 100 + 1000 + 1000");

        let p1_award = *result.chips_awarded.get(&pid(1).inner()).unwrap_or(&0);
        let p2_award = *result.chips_awarded.get(&pid(2).inner()).unwrap_or(&0);
        let p3_award = *result.chips_awarded.get(&pid(3).inner()).unwrap_or(&0);

        // The forfeiters get NOTHING — not even their side-pot stake back.
        assert_eq!(
            p2_award, 0,
            "forfeiter pid2 must be awarded ZERO (no side-pot refund)"
        );
        assert_eq!(
            p3_award, 0,
            "forfeiter pid3 must be awarded ZERO (no side-pot refund)"
        );
        // The lone honest revealer receives the ENTIRE pot: main (300) + the
        // forfeited side pot (1800) = 2100.
        assert_eq!(
            p1_award, 2100,
            "the honest revealer wins the main pot AND the forfeited side pot \
             (forfeit-not-refund)"
        );
        // And its final stack reflects winning everyone's chips.
        let p1_final = *result.final_stacks.get(&pid(1).inner()).unwrap_or(&0);
        assert_eq!(p1_final, 2100, "the sole revealer scoops the whole table");
    }

    // ---- 4c. LIVE-F1: a board-share withholder FORFEITS its committed (all-in)
    //          stake to the honest survivor — never a blameless refund ----

    /// Two players go all-in pre-flop; the FLOP is injected, but the TURN board
    /// cannot be revealed because pid2 withholds its board share. The coordinator
    /// attributes the failure to pid2 and calls `finish_blind_forfeit_board`.
    /// pid2 must FORFEIT its committed all-in stake to pid1 (the honest survivor),
    /// NOT recover it via a void+refund. Chips conserved.
    #[test]
    fn blind_board_withholder_forfeits_committed_stake() {
        let mut hand = GameHand::new_blind(
            vec![(pid(1), c(1000), 0), (pid(2), c(1000), 1)],
            0,
            c(20),
            c(10),
            DeckSeed::default(),
        );
        hand.start().unwrap();

        // Both shove all-in pre-flop (heads-up: SB=pid1 acts first).
        let a0 = hand.snapshot().current_actor.unwrap();
        hand.apply_action(a0, PlayerAction::AllIn).unwrap();
        let a1 = hand.snapshot().current_actor.unwrap();
        hand.apply_action(a1, PlayerAction::AllIn).unwrap();

        // Inject ONLY the flop (the board reveal is interleaved after betting).
        let board = standard_board();
        assert_eq!(hand.pending_board_street(), Some(Street::Flop));
        hand.inject_board_for_street(&board[0..3])
            .expect("inject flop");
        // The TURN board is now pending and pid2 withholds its share.
        assert_eq!(hand.pending_board_street(), Some(Street::Turn));
        assert!(!hand.is_done(), "hand stuck awaiting the turn board");

        // pid2 is the attributable board withholder → forfeit-board resolution.
        let result = hand
            .finish_blind_forfeit_board(&[pid(2)])
            .expect("≥1 honest survivor → Completed, not void");

        // Chip conservation: 1000 + 1000 = 2000 total, no chip created/destroyed.
        let total_final: u32 = result.final_stacks.values().sum();
        assert_eq!(
            total_final, 2000,
            "chip conservation across deal → all-in → board-withhold forfeit"
        );
        // pid1 (honest survivor) wins the WHOLE pot; pid2 (withholder) gets 0.
        let p1_award = *result.chips_awarded.get(&pid(1).inner()).unwrap_or(&0);
        let p2_award = *result.chips_awarded.get(&pid(2).inner()).unwrap_or(&0);
        assert_eq!(p1_award, 2000, "honest survivor scoops the pot");
        assert_eq!(
            p2_award, 0,
            "the board withholder is awarded ZERO (forfeit-not-refund)"
        );
        // The withholder LOSES its committed all-in stake (final 0), it is NOT
        // refunded to ~1000 as the old void+refund path would have done.
        let p2_final = *result
            .final_stacks
            .get(&pid(2).inner())
            .unwrap_or(&u32::MAX);
        assert_eq!(
            p2_final, 0,
            "the all-in withholder must lose its whole committed stake (not be refunded)"
        );
        assert_eq!(
            *result.final_stacks.get(&pid(1).inner()).unwrap_or(&0),
            2000,
            "the honest survivor ends with the whole table"
        );
        assert!(
            result.showdown.is_empty(),
            "board-forfeit settlement must not fabricate a showdown"
        );
        assert!(
            result.show_order.is_empty(),
            "board-forfeit settlement must not expose a showdown order"
        );
    }

    /// LIVE-F1 residual: when ≥2 HONEST contenders remain after the withholder is
    /// force-folded, the board still cannot rank them — they SPLIT the contested
    /// pot equally (the withholder is excluded, never refunded). Chips conserved.
    #[test]
    fn blind_board_withholder_residual_splits_among_honest() {
        // 3 players, equal stacks all-in pre-flop → a single 3000 main pot.
        let mut hand = GameHand::new_blind(
            vec![
                (pid(1), c(1000), 0),
                (pid(2), c(1000), 1),
                (pid(3), c(1000), 2),
            ],
            0,
            c(20),
            c(10),
            DeckSeed::default(),
        );
        hand.start().unwrap();
        let board = standard_board();
        // Everyone shoves; drive only the PRE-FLOP betting (stop before injecting
        // the flop) so the flop board is pending and pid3 can withhold it.
        let mut safety = 0;
        while let Some(actor) = hand.snapshot().current_actor {
            hand.apply_action(actor, PlayerAction::AllIn).unwrap();
            safety += 1;
            assert!(safety < 50, "preflop runaway");
        }
        assert_eq!(hand.pending_board_street(), Some(Street::Flop));
        // Inject the flop, then pid3 withholds the TURN board → 2 honest remain.
        hand.inject_board_for_street(&board[0..3]).unwrap();
        assert_eq!(hand.pending_board_street(), Some(Street::Turn));

        let result = hand
            .finish_blind_forfeit_board(&[pid(3)])
            .expect("2 honest survivors → Completed (equal chop), not void");

        // Chip conservation: 3000 total.
        let total_final: u32 = result.final_stacks.values().sum();
        assert_eq!(total_final, 3000, "chip conservation in the residual split");
        // The withholder (pid3) gets nothing; pid1 + pid2 split the 3000 pot.
        let p1 = *result.chips_awarded.get(&pid(1).inner()).unwrap_or(&0);
        let p2 = *result.chips_awarded.get(&pid(2).inner()).unwrap_or(&0);
        let p3 = *result.chips_awarded.get(&pid(3).inner()).unwrap_or(&0);
        assert_eq!(p3, 0, "the withholder is excluded (forfeit-not-refund)");
        assert_eq!(
            p1 + p2,
            3000,
            "the two honest survivors split the whole pot"
        );
        assert_eq!(p1, 1500, "equal chop: pid1 half");
        assert_eq!(p2, 1500, "equal chop: pid2 half");
        // The withholder loses its committed stake (final 0), never refunded.
        assert_eq!(
            *result
                .final_stacks
                .get(&pid(3).inner())
                .unwrap_or(&u32::MAX),
            0,
            "the withholder loses its full committed stake"
        );
    }

    /// LIVE-F1 audit F5 (HIGH): the board-withhold→refund exploit SURVIVING in
    /// the side-pot layer. An honest short all-in survivor (pid1) keeps the hand
    /// out of `NoSurvivor`, but a side pot whose eligible set is ENTIRELY the two
    /// deep withholders (pid2, pid3) was previously refunded back to {pid2, pid3}
    /// at `distribute_pots_among_survivors` — recovering their matched side-pot
    /// stake by stalling the turn board. That makes withholding a side-pot board
    /// share +EV. The fix awards that withholder-only side-pot layer to the honest
    /// overall survivor (pid1) instead; the withholders never recover a matched
    /// stake. Only `refund_to` (uncalled overage) ever refunds. Chips conserved.
    #[test]
    fn blind_board_withholder_only_side_pot_goes_to_survivor_not_refunded() {
        // Stacks 100 / 1000 / 1000 — pid1 short all-in; pid2/pid3 deep. All shove
        // pre-flop. Layers:
        //   main pot = 100 * 3 = 300, eligible {pid1, pid2, pid3}
        //   side pot = 900 * 2 = 1800, eligible {pid2, pid3} ONLY
        let mut hand = GameHand::new_blind(
            vec![
                (pid(1), c(100), 0),  // short, honest
                (pid(2), c(1000), 1), // deep, withholder
                (pid(3), c(1000), 2), // deep, withholder
            ],
            0,
            c(20),
            c(10),
            DeckSeed::default(),
        );
        hand.start().unwrap();
        let board = standard_board();
        // Everyone shoves; drive ONLY the pre-flop betting so the flop board is
        // pending and pid2/pid3 can withhold a later street's board.
        let mut safety = 0;
        while let Some(actor) = hand.snapshot().current_actor {
            hand.apply_action(actor, PlayerAction::AllIn).unwrap();
            safety += 1;
            assert!(safety < 50, "preflop runaway");
        }
        assert_eq!(hand.pending_board_street(), Some(Street::Flop));
        // Inject the flop, then pid2 + pid3 withhold the TURN board. pid1 (the
        // short all-in) is the only honest survivor; the side pot is eligible to
        // {pid2, pid3} only — exactly the withholders.
        hand.inject_board_for_street(&board[0..3]).unwrap();
        assert_eq!(hand.pending_board_street(), Some(Street::Turn));

        let result = hand
            .finish_blind_forfeit_board(&[pid(2), pid(3)])
            .expect("pid1 honest survivor → Completed, not NoSurvivor");

        // Chip conservation: 100 + 1000 + 1000 = 2100, nothing created/destroyed.
        let total_final: u32 = result.final_stacks.values().sum();
        assert_eq!(
            total_final, 2100,
            "chip conservation across deal → all-in → side-pot board-withhold forfeit"
        );

        let p1_award = *result.chips_awarded.get(&pid(1).inner()).unwrap_or(&0);
        let p2_award = *result.chips_awarded.get(&pid(2).inner()).unwrap_or(&0);
        let p3_award = *result.chips_awarded.get(&pid(3).inner()).unwrap_or(&0);

        // BEFORE the F5 fix: the withholder-only side pot (1800) was refunded to
        // {pid2, pid3} (900 each) — recovering their matched stake. AFTER: that
        // layer goes to the honest survivor pid1, and the withholders get ZERO.
        assert_eq!(
            p2_award, 0,
            "withholder pid2 must be awarded ZERO (no side-pot refund)"
        );
        assert_eq!(
            p3_award, 0,
            "withholder pid3 must be awarded ZERO (no side-pot refund)"
        );
        // pid1 scoops the main pot (300) AND the forfeited withholder-only side
        // pot (1800) = 2100.
        assert_eq!(
            p1_award, 2100,
            "the honest survivor wins the main pot AND the withholder-only side pot \
             (forfeit-not-refund)"
        );

        // The withholders end BELOW their starting stack (they committed 1000 and
        // recovered nothing).
        let p2_final = *result
            .final_stacks
            .get(&pid(2).inner())
            .unwrap_or(&u32::MAX);
        let p3_final = *result
            .final_stacks
            .get(&pid(3).inner())
            .unwrap_or(&u32::MAX);
        assert!(
            p2_final < 1000,
            "withholder pid2 must end BELOW its 1000 starting stack (final {p2_final})"
        );
        assert!(
            p3_final < 1000,
            "withholder pid3 must end BELOW its 1000 starting stack (final {p3_final})"
        );
        assert_eq!(
            p2_final, 0,
            "withholder pid2 forfeits its whole committed stake"
        );
        assert_eq!(
            p3_final, 0,
            "withholder pid3 forfeits its whole committed stake"
        );
        assert_eq!(
            *result.final_stacks.get(&pid(1).inner()).unwrap_or(&0),
            2100,
            "the honest survivor ends with the whole table"
        );
    }

    /// LIVE-F1 edge: if EVERY contender is named a withholder, there is no honest
    /// survivor — `finish_blind_forfeit_board` returns `NoSurvivor` so the server
    /// VOIDs with a blameless stake-return (never invents a winner).
    #[test]
    fn blind_board_all_withhold_returns_no_survivor() {
        let mut hand = GameHand::new_blind(
            vec![(pid(1), c(1000), 0), (pid(2), c(1000), 1)],
            0,
            c(20),
            c(10),
            DeckSeed::default(),
        );
        hand.start().unwrap();
        let a0 = hand.snapshot().current_actor.unwrap();
        hand.apply_action(a0, PlayerAction::AllIn).unwrap();
        let a1 = hand.snapshot().current_actor.unwrap();
        hand.apply_action(a1, PlayerAction::AllIn).unwrap();
        let board = standard_board();
        hand.inject_board_for_street(&board[0..3]).unwrap();

        let err = hand
            .finish_blind_forfeit_board(&[pid(1), pid(2)])
            .expect_err("no honest survivor → NoSurvivor (caller VOIDs)");
        assert_eq!(err, BlindFinishError::NoSurvivor);
    }

    // ---- 5. rejection: reveal for folded seat, wrong-count board ----

    /// Injecting a reveal for a FOLDED seat is rejected (folded cards stay sealed).
    #[test]
    fn blind_reject_reveal_for_folded_seat() {
        let mut hand = GameHand::new_blind(
            vec![
                (pid(1), c(500), 0),
                (pid(2), c(500), 1),
                (pid(3), c(500), 2),
            ],
            0,
            c(20),
            c(10),
            DeckSeed::default(),
        );
        hand.start().unwrap();
        // UTG (pid1) folds.
        let a = hand.snapshot().current_actor.unwrap();
        assert_eq!(a, pid(1));
        hand.apply_action(a, PlayerAction::Fold).unwrap();
        // SB calls, BB checks → flop. Then both check down to showdown.
        let board = standard_board();
        run_blind_betting(&mut hand, &board, |snap, actor| {
            let committed = snap
                .players
                .iter()
                .find(|p| p.player_id == actor)
                .map(|p| p.committed_this_street.0)
                .unwrap_or(0);
            let to_call = snap.current_bet.0.saturating_sub(committed);
            if to_call > 0 {
                PlayerAction::Call
            } else {
                PlayerAction::Check
            }
        });
        assert!(hand.is_done());

        // Try to inject a reveal for the folded seat (pid1) → rejected.
        let p1 = hole(
            card(Rank::Two, Suit::Hearts),
            card(Rank::Three, Suit::Hearts),
        );
        let err = hand.inject_showdown_reveal(pid(1), p1).unwrap_err();
        assert_eq!(err, BlindInjectError::PlayerFolded(pid(1)));
    }

    /// Injecting the wrong number of board cards for a street is rejected.
    #[test]
    fn blind_reject_wrong_board_count() {
        let mut hand = GameHand::new_blind(
            vec![(pid(1), c(1000), 0), (pid(2), c(1000), 1)],
            0,
            c(20),
            c(10),
            DeckSeed::default(),
        );
        hand.start().unwrap();
        // Heads-up: SB calls, BB checks → flop is pending.
        let a = hand.snapshot().current_actor.unwrap();
        hand.apply_action(a, PlayerAction::Call).unwrap();
        let a = hand.snapshot().current_actor.unwrap();
        hand.apply_action(a, PlayerAction::Check).unwrap();

        assert_eq!(hand.pending_board_street(), Some(Street::Flop));
        // Flop needs 3 cards; inject 1 → WrongBoardCount.
        let one = [card(Rank::Two, Suit::Clubs)];
        let err = hand.inject_board_for_street(&one).unwrap_err();
        assert_eq!(
            err,
            BlindInjectError::WrongBoardCount {
                street: Street::Flop,
                expected: 3,
                got: 1
            }
        );
        // The correct count is accepted.
        let flop = [
            card(Rank::Two, Suit::Clubs),
            card(Rank::Seven, Suit::Diamonds),
            card(Rank::Nine, Suit::Spades),
        ];
        hand.inject_board_for_street(&flop)
            .expect("correct flop count");
        assert_eq!(hand.snapshot().board.count(), 3);
    }

    /// Injecting a reveal for an unknown player id is rejected.
    #[test]
    fn blind_reject_reveal_unknown_player() {
        let mut hand = GameHand::new_blind(
            vec![(pid(1), c(1000), 0), (pid(2), c(1000), 1)],
            0,
            c(20),
            c(10),
            DeckSeed::default(),
        );
        hand.start().unwrap();
        let a = hand.snapshot().current_actor.unwrap();
        hand.apply_action(a, PlayerAction::Fold).unwrap();
        assert!(hand.is_done());
        let h = hole(
            card(Rank::Two, Suit::Hearts),
            card(Rank::Three, Suit::Hearts),
        );
        let err = hand.inject_showdown_reveal(pid(99), h).unwrap_err();
        assert_eq!(err, BlindInjectError::UnknownPlayer(pid(99)));
    }

    /// Cross-mode guards: blind injectors reject a plaintext hand; `finish` and
    /// `finish_blind` are mode-exclusive.
    #[test]
    fn blind_injectors_reject_plaintext_hand() {
        let mut hand = GameHand::new_with_rng(
            vec![(pid(1), c(1000), 0), (pid(2), c(1000), 1)],
            0,
            c(20),
            c(10),
            PokerRng::from_seed(1),
        );
        hand.start().unwrap();
        let flop = [
            card(Rank::Two, Suit::Clubs),
            card(Rank::Seven, Suit::Diamonds),
            card(Rank::Nine, Suit::Spades),
        ];
        assert_eq!(
            hand.inject_board_for_street(&flop).unwrap_err(),
            BlindInjectError::NotBlindMode
        );
        let h = hole(
            card(Rank::Two, Suit::Hearts),
            card(Rank::Three, Suit::Hearts),
        );
        assert_eq!(
            hand.inject_showdown_reveal(pid(1), h).unwrap_err(),
            BlindInjectError::NotBlindMode
        );
    }

    /// `inject_showdown_reveal` before the hand is done is rejected — reveals are
    /// a showdown-only concept.
    #[test]
    fn blind_reject_reveal_before_showdown() {
        let mut hand = GameHand::new_blind(
            vec![(pid(1), c(1000), 0), (pid(2), c(1000), 1)],
            0,
            c(20),
            c(10),
            DeckSeed::default(),
        );
        hand.start().unwrap();
        // Still preflop, not done.
        let h = hole(
            card(Rank::Two, Suit::Hearts),
            card(Rank::Three, Suit::Hearts),
        );
        assert_eq!(
            hand.inject_showdown_reveal(pid(1), h).unwrap_err(),
            BlindInjectError::NotAtShowdown
        );
    }

    /// `new_blind` deals nothing: every seat starts `Opaque` and no plaintext
    /// is reachable through the public seat accessor.
    #[test]
    fn blind_new_blind_seats_start_opaque() {
        let hand = GameHand::new_blind(
            vec![(pid(1), c(1000), 0), (pid(2), c(1000), 1)],
            0,
            c(20),
            c(10),
            DeckSeed::default(),
        );
        for seat in &hand.seats {
            assert!(seat.hole.is_opaque(), "blind seat must start Opaque");
            assert!(
                seat.hole_cards().is_none(),
                "no plaintext reachable for an Opaque seat"
            );
        }
    }

    // ---- F1: injected cards must be unique (the engine is the authoritative
    //          ranker and must NOT trust injected cards). ----

    /// Drive a heads-up blind hand to the point where the flop board is pending,
    /// returning the started+pre-flop-closed hand. Both players check/call to the
    /// flop street so `pending_board_street() == Some(Flop)`.
    fn heads_up_to_pending_flop() -> GameHand {
        let mut hand = GameHand::new_blind(
            vec![(pid(1), c(1000), 0), (pid(2), c(1000), 1)],
            0,
            c(20),
            c(10),
            DeckSeed::default(),
        );
        hand.start().unwrap();
        // SB (button) calls, BB checks → flop pending.
        let a = hand.snapshot().current_actor.unwrap();
        hand.apply_action(a, PlayerAction::Call).unwrap();
        let a = hand.snapshot().current_actor.unwrap();
        hand.apply_action(a, PlayerAction::Check).unwrap();
        assert_eq!(hand.pending_board_street(), Some(Street::Flop));
        hand
    }

    /// A board card equal to another card in the SAME injected flop is rejected
    /// with `DuplicateCard` — the engine never stores a deck with a repeated card.
    #[test]
    fn blind_reject_duplicate_within_injected_flop() {
        let mut hand = heads_up_to_pending_flop();
        let dup = card(Rank::Two, Suit::Clubs);
        let flop = [dup, card(Rank::Seven, Suit::Diamonds), dup];
        let err = hand.inject_board_for_street(&flop).unwrap_err();
        assert_eq!(err, BlindInjectError::DuplicateCard { card: dup });
        // Board untouched — rejection happens before any card is stored.
        assert_eq!(hand.snapshot().board.count(), 0);
    }

    /// A turn card equal to an existing board (flop) card is rejected.
    #[test]
    fn blind_reject_board_card_equals_existing_board_card() {
        let mut hand = heads_up_to_pending_flop();
        let flop = [
            card(Rank::Two, Suit::Clubs),
            card(Rank::Seven, Suit::Diamonds),
            card(Rank::Nine, Suit::Spades),
        ];
        hand.inject_board_for_street(&flop).expect("valid flop");
        // Heads-up flop betting: SB(BB position?) → both check to turn.
        let a = hand.snapshot().current_actor.unwrap();
        hand.apply_action(a, PlayerAction::Check).unwrap();
        let a = hand.snapshot().current_actor.unwrap();
        hand.apply_action(a, PlayerAction::Check).unwrap();
        assert_eq!(hand.pending_board_street(), Some(Street::Turn));
        // Turn duplicates the existing flop card 2c → DuplicateCard.
        let dup = card(Rank::Two, Suit::Clubs);
        let err = hand.inject_board_for_street(&[dup]).unwrap_err();
        assert_eq!(err, BlindInjectError::DuplicateCard { card: dup });
        // Board still just the flop — the duplicate turn was not stored.
        assert_eq!(hand.snapshot().board.count(), 3);
    }

    /// Drive a heads-up blind hand to a contested showdown (both check down a
    /// distinct board), ready for showdown reveals. Returns the done hand.
    fn heads_up_to_contested_showdown() -> GameHand {
        let mut hand = GameHand::new_blind(
            vec![(pid(1), c(1000), 0), (pid(2), c(1000), 1)],
            0,
            c(20),
            c(10),
            DeckSeed::default(),
        );
        hand.start().unwrap();
        let board = standard_board();
        run_blind_betting(&mut hand, &board, |snap, actor| {
            let committed = snap
                .players
                .iter()
                .find(|p| p.player_id == actor)
                .map(|p| p.committed_this_street.0)
                .unwrap_or(0);
            let to_call = snap.current_bet.0.saturating_sub(committed);
            if to_call > 0 {
                PlayerAction::Call
            } else {
                PlayerAction::Check
            }
        });
        assert!(hand.is_done());
        hand
    }

    /// Two seats revealing the SAME card is rejected with `DuplicateCard`. The
    /// first reveal is stored; the second (colliding) reveal is rejected.
    #[test]
    fn blind_reject_two_seats_reveal_same_card() {
        let mut hand = heads_up_to_contested_showdown();
        let shared = card(Rank::Queen, Suit::Spades);
        // pid(1) reveals Qs + Kd (distinct from the board 2c 7d 9s Jh 4c).
        let p1 = hole(shared, card(Rank::King, Suit::Diamonds));
        hand.inject_showdown_reveal(pid(1), p1).expect("reveal p1");
        // pid(2) tries to reveal the SAME Qs → DuplicateCard.
        let p2 = hole(shared, card(Rank::Ten, Suit::Diamonds));
        let err = hand.inject_showdown_reveal(pid(2), p2).unwrap_err();
        assert_eq!(err, BlindInjectError::DuplicateCard { card: shared });
        // pid(2) was NOT mutated — it stays Opaque (no partial reveal stored).
        let seat2 = hand.seats.iter().find(|s| s.player_id == pid(2)).unwrap();
        assert!(seat2.hole.is_opaque(), "rejected reveal leaves seat Opaque");
    }

    /// A reveal card equal to a board card is rejected with `DuplicateCard`.
    #[test]
    fn blind_reject_reveal_card_equals_board_card() {
        let mut hand = heads_up_to_contested_showdown();
        // Board is 2c 7d 9s Jh 4c; reveal a hole containing the board card 9s.
        let board_card = card(Rank::Nine, Suit::Spades);
        let p1 = hole(board_card, card(Rank::King, Suit::Diamonds));
        let err = hand.inject_showdown_reveal(pid(1), p1).unwrap_err();
        assert_eq!(err, BlindInjectError::DuplicateCard { card: board_card });
        let seat1 = hand.seats.iter().find(|s| s.player_id == pid(1)).unwrap();
        assert!(seat1.hole.is_opaque(), "rejected reveal leaves seat Opaque");
    }

    /// A reveal whose two hole cards are equal to each other is rejected.
    #[test]
    fn blind_reject_reveal_with_internal_duplicate() {
        let mut hand = heads_up_to_contested_showdown();
        let dup = card(Rank::Queen, Suit::Spades);
        let p1 = hole(dup, dup);
        let err = hand.inject_showdown_reveal(pid(1), p1).unwrap_err();
        assert_eq!(err, BlindInjectError::DuplicateCard { card: dup });
    }

    // ---- F2: finish_blind returns typed errors instead of panicking on a
    //          wrong phase / wrong mode. ----

    /// `finish_blind` before the hand is done returns `NotDone` (NOT a panic) so
    /// the server can void the hand rather than crash the room.
    #[test]
    fn blind_finish_before_done_returns_not_done() {
        let mut hand = GameHand::new_blind(
            vec![(pid(1), c(1000), 0), (pid(2), c(1000), 1)],
            0,
            c(20),
            c(10),
            DeckSeed::default(),
        );
        hand.start().unwrap();
        // Still preflop — not done.
        assert!(!hand.is_done());
        let err = hand.finish_blind().unwrap_err();
        assert_eq!(err, BlindFinishError::NotDone);
    }

    /// `finish_blind` on a plaintext-mode hand returns `NotBlindMode` (NOT a
    /// panic).
    #[test]
    fn blind_finish_on_plaintext_hand_returns_not_blind_mode() {
        let mut hand = GameHand::new_with_rng(
            vec![(pid(1), c(1000), 0), (pid(2), c(1000), 1)],
            0,
            c(20),
            c(10),
            PokerRng::from_seed(1),
        );
        hand.start().unwrap();
        // Fold to done so phase == Done but mode == Plaintext.
        let a = hand.snapshot().current_actor.unwrap();
        hand.apply_action(a, PlayerAction::Fold).unwrap();
        assert!(hand.is_done());
        let err = hand.finish_blind().unwrap_err();
        assert_eq!(err, BlindFinishError::NotBlindMode);
    }

    // ---- F3: the contested rank path uses ONLY injected `Revealed` reveals;
    //          a `Plain` slot (unreachable in prod) counts as a missing reveal. ----

    /// If a non-folded contender somehow holds a `Plain` slot (which blind mode
    /// never writes), the contested rank path treats it as a MISSING reveal —
    /// only `Revealed` feeds the ranker. This guards the "ranking uses only
    /// injected `Revealed`" invariant against a stray `Plain`.
    #[test]
    fn blind_contested_treats_plain_slot_as_missing_reveal() {
        let mut hand = heads_up_to_contested_showdown();
        // Legitimately reveal pid(2).
        let p2 = hole(card(Rank::Ace, Suit::Spades), card(Rank::King, Suit::Clubs));
        hand.inject_showdown_reveal(pid(2), p2).expect("reveal p2");
        // Force pid(1) into a `Plain` slot (an out-of-protocol state). The
        // contested path must NOT treat this as a reveal.
        let stray = hole(
            card(Rank::Queen, Suit::Hearts),
            card(Rank::Queen, Suit::Diamonds),
        );
        let seat1 = hand
            .seats
            .iter_mut()
            .find(|s| s.player_id == pid(1))
            .unwrap();
        seat1.hole = HoleSlot::Plain(stray);
        let err = hand.finish_blind().unwrap_err();
        assert_eq!(
            err,
            BlindFinishError::MissingReveal {
                player_id: pid(1),
                seat: 0,
            }
        );
    }
}
