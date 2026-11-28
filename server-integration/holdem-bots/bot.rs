//! Table bot decisions, including the position-aware personalities and Shark.
//!
//! ## Architecture
//! - `decide(ctx)` is a pure function. No IO, no async, no DB.
//! - Shark checks solved preflop assets, eligible postflop results, and named opponent reads.
//! - The chart policies use position-specific RFI ranges (§4.1).
//! - Postflop: board-texture heuristics + made-hand/draw classifier (§4.2).
//! - Per-bot RNG: caller injects `StdRng` seeded from `(deck_seed ^ pid_mix)` (§6).
//! - Persona profiles follow ADR-031/047, with preflop calibration amended by ADR-103.

pub mod adapt;
mod all_in;
pub mod commitment;
pub mod learning;
pub mod opening_read;
pub mod opponent_memory;
pub use all_in::{all_in_context, AllInEvidence, AllInPublic, SquidPublic};
pub(crate) mod blind_steal;
mod forced_blind;
pub(crate) mod hu_jam;
mod limp_reraise;
mod multiway_exploit;
mod multiway_river_model;
pub(crate) mod postflop_work;
mod preflop_blueprint;
pub(crate) mod river_model;
mod shark;
mod terminal;
pub(crate) mod turn_model;
pub use limp_reraise::*;
mod seven_deuce;
pub use seven_deuce::bounty_value as seven_deuce_bounty_value;
mod squid_defense;
pub use squid_defense::{SquidDefense, SquidRead};
mod strategy;
pub use shark::river_half_pot_opportunity;
use shark::*;
use strategy::*;
mod preflop;
pub use preflop::*;
mod postflop;
pub use postflop::*;
mod ranges;
pub use ranges::*;
mod hand_features;
pub use hand_features::*;
mod history;
pub use history::*;

use engine::{BoardCards, Chips, HandRank, HoleCards, PlayerAction, Position, Rank, Street};
use rand::rngs::StdRng;
use rand::Rng;
use serde::{Deserialize, Serialize};

// ---------------------------------------------------------------------------
// Public types
// ---------------------------------------------------------------------------

/// Hold’em bot persona. ADR-031 describes the snake-case wire labels.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum BotPersona {
    /// Tight-ish 6-max regular: VPIP~24, PFR~18, 3bet~6, AF~1.5.
    ClubRegular,
    /// Tight-aggressive: VPIP~22, PFR~18, 3bet~7, AF~2.5.
    Tag,
    /// Loose-aggressive: VPIP~30, PFR~25, 3bet~11, AF~3.0.
    Lag,
    /// Extremely tight: VPIP~14, PFR~11, 3bet~3, AF~1.0.
    Nit,
    /// Wild aggressor: VPIP~45, PFR~35, 3bet~15, AF~4.0. Has 4-bet bluff combos.
    Maniac,
    /// Loose-passive calling station: VPIP~38, PFR~12, 3bet~2, AF~0.6.
    Station,
    /// Range-based Shark with conservative opponent adjustments. Reliable
    /// context-matched shove reads adjust preflop value calls. Voluntary
    /// opening-bet responses are matched by street and size before changing
    /// postflop betting frequencies. Flop folds and aggression alone never
    /// establish river over-folding or over-bluffing. Consumes [`OpponentModel`] through
    /// [`decide_with_opponents`].
    Shark,
}

/// All position-aware parameters for a persona.
///
/// `decide_*` sub-functions read this profile instead of hard-coded constants;
/// ADR-103 maps the preflop re-raise knobs to population-weighted ranges while
/// preserving the shared server-authoritative decision surface.
#[derive(Debug, Clone, Copy)]
pub struct PersonaProfile {
    pub label: BotPersona,
    /// Raise-first-in width per seat (UTG/LJ/HJ/CO/BTN/SB) as a percent of
    /// the 1,326 starting combinations, applied to the position-specific
    /// priority order (`rfi_priority_order`, L1 2026-10-14). ClubRegular holds
    /// the standard 6-max/100bb reference widths; the caricatures are scaled
    /// around it (their combo-weighted bands are unchanged from ADR-103).
    pub rfi_pct: [u8; 6],
    /// In-position open size in big blinds (BTN/CO/HJ). ADR-047 §4.
    pub open_size_bb_ip: f32,
    /// Out-of-position open size in big blinds (UTG/LJ/MP/SB). ADR-047 §4.
    pub open_size_bb_oop: f32,
    pub three_bet_pct: u8,
    pub four_bet_pct: u8,
    pub cbet_dry_pct: u8,
    pub cbet_wet_pct: u8,
    pub fold_to_cbet_pct: u8,
    /// Second-barrel frequency on dry turns as flop aggressor (ADR-047 §4).
    pub barrel_dry_pct: u8,
    /// Second-barrel frequency on wet turns as flop aggressor (ADR-047 §4).
    pub barrel_wet_pct: u8,
    /// IP probe frequency when aggressor checks flop (ADR-047 §4).
    pub probe_turn_pct: u8,
    /// Delayed c-bet frequency on turn after checking back flop (ADR-047 §4).
    pub delayed_cbet_pct: u8,
    /// Third-barrel (river) frequency as turn aggressor (ADR-047 §4).
    pub third_barrel_pct: u8,
    /// Float frequency on flop with weak-made / backdoor (IP only) (ADR-047 §4).
    pub float_flop_pct: u8,
    /// Marginal-call frequency facing a bet with WeakMade hands (ADR-047 §4).
    pub call_weakmade_pct: u8,
    pub aggression_multiplier: f32,
    pub allows_4bet_bluffs: bool,
    pub suppresses_postflop_raise: bool,
}

/// Persona roster — all seven profiles, indexed by `BotPersona as usize`.
///
/// Order: ClubRegular=0, Tag=1, Lag=2, Nit=3, Maniac=4, Station=5, Shark=6.
///
/// fix/bot-persona-caricature-v2: the casual rows (1..5) are deliberately
/// CARICATURED, not GTO-realistic — the product owner wants each persona
/// instantly recognizable within ~1 orbit of casual play ("松凶就打得疯批一些").
/// This widens the spread of every knob and lets `aggression_multiplier` bite on
/// BOTH postflop frequency (via `scaled_freq`) and sizing (via `scaled_bet`).
/// ClubRegular (index 0) is the neutral reference with an aggression multiplier
/// of 1.0. Shark (index 6) has its own base profile.
pub const PERSONA_ROSTER: [PersonaProfile; 7] = [
    // ClubRegular — the balanced 1.0 reference.
    // row 1. aggression_multiplier == 1.0 keeps its bands identical.
    PersonaProfile {
        label: BotPersona::ClubRegular,
        rfi_pct: [16, 18, 21, 27, 45, 40],
        open_size_bb_ip: 2.5,
        open_size_bb_oop: 3.0,
        three_bet_pct: 6,
        four_bet_pct: 3,
        cbet_dry_pct: 70,
        cbet_wet_pct: 60,
        fold_to_cbet_pct: 55,
        barrel_dry_pct: 55,
        barrel_wet_pct: 40,
        probe_turn_pct: 12,
        delayed_cbet_pct: 25,
        third_barrel_pct: 35,
        float_flop_pct: 18,
        call_weakmade_pct: 30,
        aggression_multiplier: 1.00,
        allows_4bet_bluffs: false,
        suppresses_postflop_raise: false,
    },
    // TAG — ADR-103. Opens tighter than the neutral reg, then hits its selected
    // range HARD: a 12% effective 3-bet range plus cranked postflop aggression
    // (aggression_multiplier 1.25, high barrels/cbets, high fold-to-cbet).
    PersonaProfile {
        label: BotPersona::Tag,
        rfi_pct: [10, 12, 15, 20, 34, 26],
        open_size_bb_ip: 2.5,
        open_size_bb_oop: 3.0,
        three_bet_pct: 12,
        four_bet_pct: 4,
        cbet_dry_pct: 82,
        cbet_wet_pct: 72,
        fold_to_cbet_pct: 58,
        barrel_dry_pct: 70,
        barrel_wet_pct: 58,
        probe_turn_pct: 18,
        delayed_cbet_pct: 38,
        third_barrel_pct: 52,
        float_flop_pct: 20,
        call_weakmade_pct: 24,
        aggression_multiplier: 1.25,
        allows_4bet_bluffs: false,
        suppresses_postflop_raise: false,
    },
    // LAG (松凶) — CARICATURE headline "疯批". Raises a LOT preflop, 3-bets/4-bets
    // light (allows_4bet_bluffs), barrels relentlessly, rarely folds to c-bets,
    // and sizes up (aggression_multiplier 1.60 → bigger + more-frequent postflop
    // bets via scaled_bet/scaled_freq). Second only to the Maniac on aggression.
    PersonaProfile {
        label: BotPersona::Lag,
        rfi_pct: [19, 24, 30, 40, 66, 54],
        open_size_bb_ip: 2.5,
        open_size_bb_oop: 3.0,
        three_bet_pct: 14,
        four_bet_pct: 8,
        cbet_dry_pct: 86,
        cbet_wet_pct: 78,
        fold_to_cbet_pct: 28,
        barrel_dry_pct: 80,
        barrel_wet_pct: 70,
        probe_turn_pct: 40,
        delayed_cbet_pct: 55,
        third_barrel_pct: 64,
        float_flop_pct: 48,
        call_weakmade_pct: 42,
        aggression_multiplier: 1.60,
        allows_4bet_bluffs: true,
        suppresses_postflop_raise: false,
    },
    // Nit — CARICATURE rock. Folds almost everything; only premiums. Visibly
    // tight: tiny RFI, tiny 3-bet, folds to c-bets a ton (80), barrels rarely.
    // Lowest VPIP/PFR at the table. Open size kept at 2.5/2.75 (P1-C).
    PersonaProfile {
        label: BotPersona::Nit,
        rfi_pct: [4, 5, 7, 9, 14, 10],
        open_size_bb_ip: 2.5,
        open_size_bb_oop: 2.75,
        three_bet_pct: 2,
        four_bet_pct: 1,
        cbet_dry_pct: 58,
        cbet_wet_pct: 42,
        fold_to_cbet_pct: 80,
        barrel_dry_pct: 38,
        barrel_wet_pct: 20,
        probe_turn_pct: 4,
        delayed_cbet_pct: 12,
        third_barrel_pct: 18,
        float_flop_pct: 4,
        call_weakmade_pct: 12,
        aggression_multiplier: 0.75,
        allows_4bet_bluffs: false,
        suppresses_postflop_raise: false,
    },
    // Maniac — CARICATURE wildest at the table. Near-max RFI, constant
    // re-raising, biggest sizings (aggression_multiplier 1.90), most bluffs,
    // almost never folds. More extreme than the LAG on every aggression axis.
    PersonaProfile {
        label: BotPersona::Maniac,
        rfi_pct: [35, 42, 50, 64, 75, 68],
        open_size_bb_ip: 3.0,
        open_size_bb_oop: 3.5,
        three_bet_pct: 20,
        four_bet_pct: 12,
        cbet_dry_pct: 90,
        cbet_wet_pct: 85,
        fold_to_cbet_pct: 18,
        barrel_dry_pct: 88,
        barrel_wet_pct: 80,
        probe_turn_pct: 52,
        delayed_cbet_pct: 68,
        third_barrel_pct: 78,
        float_flop_pct: 60,
        call_weakmade_pct: 50,
        aggression_multiplier: 1.90,
        allows_4bet_bluffs: true,
        suppresses_postflop_raise: false,
    },
    // Station — CARICATURE calling "fish". Near-zero RFI/raising, but continues
    // a HUGE chunk of hands as a limp (unopened) or flat (facing an open) so its
    // VPIP is the HIGHEST at the table (the wide bands live in decide_rfi /
    // decide_station_facing_open, tuned to STATION_CONTINUE_PCT). Almost never
    // folds to a bet (fold_to_cbet 8), never raises postflop
    // (suppresses_postflop_raise), calls weak-made ~always. Lowest aggression.
    PersonaProfile {
        label: BotPersona::Station,
        rfi_pct: [2, 3, 4, 5, 9, 5],
        open_size_bb_ip: 2.5,
        open_size_bb_oop: 2.75,
        three_bet_pct: 1,
        four_bet_pct: 1,
        cbet_dry_pct: 20,
        cbet_wet_pct: 15,
        fold_to_cbet_pct: 8,
        barrel_dry_pct: 12,
        barrel_wet_pct: 8,
        probe_turn_pct: 2,
        delayed_cbet_pct: 5,
        third_barrel_pct: 6,
        float_flop_pct: 3,
        call_weakmade_pct: 60,
        aggression_multiplier: 0.55,
        allows_4bet_bluffs: false,
        suppresses_postflop_raise: true,
    },
    // Shark (ADR-024 strong-AI tier) — solid LAG-ish aggressive BASE that the
    // exploit layer (`decide_with_opponents`) modulates per-opponent at runtime.
    // Base sits between TAG and LAG: positionally aware, aggressive postflop,
    // never a calling station. The base numbers are deliberately moderate; the
    // edge comes from adapting to reads, not from a static wide range.
    PersonaProfile {
        label: BotPersona::Shark,
        // Loose-aggressive base (the proven +bb/100 archetype in this field):
        // wide late-position steals, frequent c-bets/barrels, then the exploit
        // layer modulates per-opponent. Tuned via gto_selfplay bb/100, not GTO
        // agreement (study: GTO-agreement is NEGATIVELY correlated with winning).
        rfi_pct: [19, 25, 30, 40, 60, 48],
        open_size_bb_ip: 2.2,
        open_size_bb_oop: 2.6,
        three_bet_pct: 11,
        four_bet_pct: 6,
        cbet_dry_pct: 80,
        cbet_wet_pct: 66,
        fold_to_cbet_pct: 48,
        barrel_dry_pct: 66,
        barrel_wet_pct: 52,
        probe_turn_pct: 28,
        delayed_cbet_pct: 42,
        third_barrel_pct: 50,
        float_flop_pct: 34,
        call_weakmade_pct: 32,
        aggression_multiplier: 1.30,
        allows_4bet_bluffs: true,
        suppresses_postflop_raise: false,
    },
];

/// Look up the `PersonaProfile` for a given `BotPersona` label.
///
/// Returns a `&'static PersonaProfile` — zero-cost lookup from the const roster.
pub fn lookup_profile(persona: BotPersona) -> &'static PersonaProfile {
    // `PERSONA_ROSTER` is ordered to match the `BotPersona as usize` discriminant
    // convention (ClubRegular=0 … Station=5), so the enum is a direct index.
    &PERSONA_ROSTER[persona as usize]
}

/// Parse a create-room-wizard persona key into a [`BotPersona`].
///
/// The wizard (`client-game/PokerSetupForm.vue`, `client-mobile`) sends a
/// `bot_personas` map keyed by the UI persona keys
/// (`maniac`/`lag`/`tag`/`reg`/`nit`/`station`). These mostly coincide with the
/// `#[serde(rename_all = "snake_case")]` wire names of `BotPersona`, EXCEPT the
/// "club regular" persona, which the wizard abbreviates to `reg` while the enum
/// serialises as `club_regular`. Accept both spellings (plus `regular`) so a
/// host-configured roster is honoured regardless of which key the client used.
///
/// Returns `None` for unknown keys (forward-compat: the caller falls back to
/// Shark for any seat left unspecified).
pub fn persona_from_wizard_key(key: &str) -> Option<BotPersona> {
    match key.trim().to_ascii_lowercase().as_str() {
        "reg" | "regular" | "club_regular" => Some(BotPersona::ClubRegular),
        "tag" => Some(BotPersona::Tag),
        "lag" => Some(BotPersona::Lag),
        "nit" => Some(BotPersona::Nit),
        "maniac" => Some(BotPersona::Maniac),
        "station" => Some(BotPersona::Station),
        "shark" => Some(BotPersona::Shark),
        _ => None,
    }
}

// ---------------------------------------------------------------------------
// Public table state (GTO-anchored pass L1, 2026-10-14)
// ---------------------------------------------------------------------------

/// Public facts about ONE seat dealt into the current hand. Never carries hole
/// cards: the strategy layer must see only what every player at the table can
/// see (card secrecy, ADR-024 §3).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct SeatPublic {
    pub seat: u8,
    /// Chips still behind.
    pub stack: u32,
    /// Chips already committed on the current street.
    pub committed_this_street: u32,
    pub folded: bool,
    pub all_in: bool,
}

/// Public table state for the current hand: every dealt seat in table order
/// plus the dealer seat. `Default` (no seats) means "unknown"; every consumer
/// falls back to the legacy positional heuristics in that case, so callers
/// that cannot supply it keep their previous behaviour.
#[derive(Debug, Clone, Copy, Default)]
pub struct TablePublic<'a> {
    pub dealer_seat: u8,
    /// Seats dealt into the hand, in table (clockwise) order, including folded
    /// and all-in seats so original positions survive folds.
    pub seats: &'a [SeatPublic],
}

impl<'a> TablePublic<'a> {
    /// True when the table state is usable (two or more dealt seats and the
    /// dealer is one of them).
    pub fn is_known(&self) -> bool {
        self.seats.len() >= 2 && self.seats.iter().any(|s| s.seat == self.dealer_seat)
    }

    fn seat_entry(&self, seat: u8) -> Option<&SeatPublic> {
        self.seats.iter().find(|s| s.seat == seat)
    }

    /// Postflop acting order: 0 = first seat after the dealer, dealer = last.
    fn ring_offset(&self, seat: u8) -> Option<usize> {
        if !self.is_known() {
            return None;
        }
        let n = self.seats.len();
        let dealer = self.seats.iter().position(|s| s.seat == self.dealer_seat)?;
        let idx = self.seats.iter().position(|s| s.seat == seat)?;
        Some((idx + 2 * n - dealer - 1) % n)
    }

    /// Original dealt position of `seat` (survives folds).
    pub fn position_of(&self, seat: u8) -> Option<Position> {
        if !self.is_known() || self.seat_entry(seat).is_none() {
            return None;
        }
        let nums: Vec<u8> = self.seats.iter().map(|s| s.seat).collect();
        Some(Position::for_seat(seat, self.dealer_seat, &nums))
    }

    /// Whether `hero` closes the postflop action: no opponent that can still
    /// act (not folded, not all-in) sits later in the post-dealer order.
    pub fn closes_postflop_action(&self, hero: u8) -> Option<bool> {
        self.closes_action_among(hero, |_| true)
    }

    /// Whether `hero` acts last among the opponents selected by `involved`
    /// that can still act (not folded, not all-in). `closes_postflop_action`
    /// is the `|_| true` case; preflop callers restrict it to the opponents
    /// already in the pot.
    pub fn closes_action_among(
        &self,
        hero: u8,
        involved: impl Fn(&SeatPublic) -> bool,
    ) -> Option<bool> {
        let mine = self.ring_offset(hero)?;
        Some(!self.seats.iter().any(|s| {
            s.seat != hero
                && !s.folded
                && !s.all_in
                && involved(s)
                && self.ring_offset(s.seat).is_some_and(|o| o > mine)
        }))
    }

    /// First live opponent clockwise from an opening bettor.
    pub fn first_river_responder(&self, hero: u8) -> Option<&SeatPublic> {
        if !self.is_known() {
            return None;
        }
        let start = self.seats.iter().position(|s| s.seat == hero)?;
        (1..self.seats.len())
            .map(|step| &self.seats[(start + step) % self.seats.len()])
            .find(|s| !s.folded && !s.all_in)
    }

    /// Opponents still in the hand (not folded; all-in seats count because
    /// they reach showdown).
    pub fn live_opponents(&self, hero: u8) -> Option<usize> {
        if !self.is_known() || self.seat_entry(hero).is_none() {
            return None;
        }
        Some(
            self.seats
                .iter()
                .filter(|s| s.seat != hero && !s.folded)
                .count(),
        )
    }

    /// Effective stack between `hero` and `other` for this street: the smaller
    /// of the two (stack behind + chips already committed this street).
    pub fn effective_stack_vs(&self, hero: u8, other: u8) -> Option<u32> {
        if !self.is_known() {
            return None;
        }
        let h = self.seat_entry(hero)?;
        let o = self.seat_entry(other)?;
        Some(
            h.stack
                .saturating_add(h.committed_this_street)
                .min(o.stack.saturating_add(o.committed_this_street)),
        )
    }

    /// Effective stack against the deepest opponent still in the hand.
    pub fn effective_stack(&self, hero: u8) -> Option<u32> {
        if !self.is_known() {
            return None;
        }
        let h = self.seat_entry(hero)?;
        let deepest = self
            .seats
            .iter()
            .filter(|s| s.seat != hero && !s.folded)
            .map(|s| s.stack.saturating_add(s.committed_this_street))
            .max()?;
        Some(h.stack.saturating_add(h.committed_this_street).min(deepest))
    }
}

/// Discounted public side-game estimates relative to possible other winners.
/// The loss spread prevents crediting a benefit available by folding anyway.
#[derive(Clone, Copy, Debug, Default)]
pub struct SquidCallValue {
    pub win_bonus: f64,
    pub loss_risk: f64,
}

/// Everything a bot needs to decide.
///
/// Derived from the live snapshot, action history, and assigned persona.
pub struct DecisionContext<'a> {
    // --- Identity ---
    /// Bot's seat index (0..n).
    pub seat: u8,
    /// Computed position relative to the dealer button.
    pub position: Position,
    /// Persona label (retained for serde / logging convenience).
    pub persona: BotPersona,
    /// Resolved persona profile (`&'static` — zero-cost lookup from PERSONA_ROSTER).
    /// Callers should set this via `lookup_profile(persona)`.
    pub profile: &'static PersonaProfile,
    /// Per-bot RNG (injected by session; seeded from deck_seed ^ pid_mix).
    pub rng: &'a mut StdRng,

    /// Single-board cash chip utility, without conditional prizes or side payments.
    pub plain_cash: bool,

    // --- Table topology ---
    pub sb_seat: u8,
    pub bb_seat: u8,
    /// How many players have NOT folded or gone all-in (including self).
    pub num_active: u8,
    /// Self's stack in big blinds (rounded down).
    pub stack_bb: u32,
    /// Exact chips remaining behind. Sizing and all-in legality must use this
    /// value rather than reconstructing chips from the rounded-down BB depth.
    pub stack_chips: u32,
    /// Big blind size in chips.
    pub big_blind: u32,
    /// Authoritative forced straddle presence, including a short funded post.
    pub has_straddle: bool,

    // --- Street state ---
    pub street: Street,
    pub board: &'a BoardCards,
    pub hole: HoleCards,
    /// Total pot across all streets.
    pub pot: Chips,
    /// Amount the bot must pay to call (0 if free to check/bet).
    pub to_call: Chips,
    /// Current street bet level.
    pub current_bet: Chips,
    /// Authoritative raise right for the current actor, sourced from the live
    /// engine betting round. This is deliberately independent of
    /// `min_raise_to`: `None` is sizing data, not permission to invent an
    /// all-in fallback when only call/fold (or check) is legal.
    pub can_raise: bool,
    /// Minimum legal raise-to amount (`None` if raise is not available).
    pub min_raise_to: Option<Chips>,
    /// Chips this bot has already committed this street.
    pub committed_this_street: Chips,

    // --- Street history ---
    /// All actions this street in order: `(seat, action)`. Production callers
    /// supply the engine's full-raise-depth projection: an exact/under-call
    /// all-in and a non-full short raise are represented as `Call`; only a full
    /// raise retains `Raise` / `AllIn`. Use `last_aggressor_seat` when any wager
    /// increase (including a short raise) matters instead of recounting tokens.
    pub street_actions: &'a [(u8, PlayerAction)],
    /// Public actions from the beginning of this hand, retaining their street
    /// and wager sizes. Shark-only production input; `None` means unavailable,
    /// while `Some(&[])` is a known hand with no voluntary action yet.
    pub public_history: Option<&'a [PublicAction]>,
    /// Seat of the last player who bet/raised this street. `None` if no aggression.
    pub last_aggressor_seat: Option<u8>,

    // --- Preflop-only signals ---
    /// Seat of the preflop aggressor (for c-bet logic on flop). `None` if no open.
    pub preflop_aggressor_seat: Option<u8>,
    /// Number of limpers (callers) ahead of us preflop (informs squeeze decisions).
    pub num_callers_so_far: u8,

    // --- Street-aggressor signals (ADR-047 §6) ---
    /// Seat of the last bettor/raiser on the flop. Frozen at flop close so
    /// `decide_turn` can identify the flop aggressor (for second-barrel logic).
    pub flop_aggressor_seat: Option<u8>,
    /// Seat of the last bettor/raiser on the turn. Frozen at turn close so
    /// `decide_river` can identify the turn aggressor (for third-barrel logic).
    pub turn_aggressor_seat: Option<u8>,

    /// Discounted public side-game value of hero winning vs the aggressor.
    /// Estimated continuation value, never guaranteed pot/settlement money.
    pub squid_call_value: SquidCallValue,
    /// Public reads and history of the opponents for any persona, with the
    /// squid-token value of this pot (zero outside an active squid round).
    /// Default (no history) where a caller supplies none.
    pub squid_defense: SquidDefense,
    /// Chips the room's 7-2 bounty pays this bot for winning this pot after
    /// the flop ([`seven_deuce_bounty_value`]); zero with the rule off or any
    /// holding but 7-2 offsuit. Priced with the squid token as side value.
    pub seven_deuce_bounty: f64,
    /// Exact public hand contributions and optional funded squid forecast.
    pub all_in_public: Option<AllInPublic>,
    pub all_in_evidence: Option<AllInEvidence>,

    // --- Public table state (L1) ---
    /// Public seat facts for the whole table; `TablePublic::default()` when a
    /// caller cannot supply them (legacy heuristics then apply).
    pub table: TablePublic<'a>,

    /// Online counter-exploitation pressure in `[-1, 1]` of the human line
    /// this bot faces ([`adapt`]); positive widens continuing, negative
    /// tightens it. `0.0` (no human line, tournaments, self-play) leaves every
    /// decision unchanged.
    pub line_pressure: f64,

    /// Online pressure in `[-1, 1]` on this bot's own postflop opening bet
    /// into the humans left in the pot ([`adapt::opening_pressure`]);
    /// negative opens less without a hand, positive expects more folds.
    /// `0.0` leaves every decision unchanged.
    pub open_pressure: f64,
}

// ---------------------------------------------------------------------------
// Opponent model (ADR-024 Shark tier) — simple per-opponent stat accumulation
// ---------------------------------------------------------------------------

/// Voluntary opening-bet responses in a single street/size context.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct BetResponseStats {
    pub faced: u32,
    pub folded: u32,
}

/// Match public observations by street AND wager size. A fold to a river
/// overbet is not evidence about a flop third-pot bet (or vice versa).
pub fn bet_response_bucket(street: Street, bet: u32, pot_before: u32) -> Option<(usize, usize)> {
    let street = match street {
        Street::Preflop => return None,
        Street::Flop => 0,
        Street::Turn => 1,
        Street::River => 2,
    };
    if bet == 0 || pot_before == 0 {
        return None;
    }
    let size = if bet <= pot_before.div_ceil(2) {
        0
    } else if bet <= pot_before {
        1
    } else {
        2
    };
    Some((street, size))
}

/// A heads-up opening bet is comparable only when two players were live at
/// the bet and the observed response is the very next action.
pub fn heads_up_bet_response(
    snap: &engine::GameSnapshot,
    facts: &[engine::StreetActionFact],
) -> Option<(usize, usize)> {
    let (bet, preceding) = facts.split_last()?;
    let mut live = snap.players.iter().filter(|p| !p.folded);
    let first = live.next()?;
    let second = live.next()?;
    if live.next().is_some()
        || first.all_in
        || second.all_in
        || !bet.increased_bet
        || bet.to_call_before.0 != 0
        || preceding.iter().any(|f| f.increased_bet)
    {
        return None;
    }
    let actor = [first, second]
        .into_iter()
        .find(|p| Some(p.player_id) == snap.current_actor)?;
    if actor.player_id == bet.player_id
        || actor.committed_this_street.0 != 0
        || actor.stack <= snap.current_bet
        || bet.amount != snap.current_bet
    {
        return None;
    }
    bet_response_bucket(snap.street, bet.amount.0, bet.pot_before.0)
}

/// Last 32 comparable shove opportunities; old play ages out when style changes.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ShoveRead {
    pub opportunities: u8,
    pub recent: u32,
}

impl ShoveRead {
    pub fn observe(&mut self, shoved: bool) {
        self.recent = (self.recent << 1) | u32::from(shoved);
        self.opportunities = self.opportunities.saturating_add(1).min(32);
    }

    /// History where every opportunity in `self` came before those in `later`.
    pub fn then(self, later: Self) -> Self {
        let n = u32::from(later.opportunities);
        Self {
            opportunities: (u32::from(self.opportunities) + n).min(32) as u8,
            recent: self.recent.checked_shl(n).unwrap_or(0) | later.recent,
        }
    }

    /// Wilson lower bound, with z=1.96. Sparse/uncertain histories stay narrow.
    pub fn lower_frequency(self, min_sample: u32) -> Option<f64> {
        if u32::from(self.opportunities) < min_sample.max(4) {
            return None;
        }
        let n = f64::from(self.opportunities);
        let p = f64::from(self.recent.count_ones()) / n;
        let z2 = 1.96 * 1.96;
        Some(
            (p + z2 / (2.0 * n) - 1.96 * (p * (1.0 - p) / n + z2 / (4.0 * n * n)).sqrt())
                / (1.0 + z2 / n),
        )
    }
}

/// Keep short/ordinary/deep shoves and straddled pots in separate read buckets.
/// Total chips BEFORE the decision, not chips behind after posting a blind.
pub fn open_shove_bucket(total: u32, bb: u32, straddle: bool) -> Option<usize> {
    if bb == 0 || total < bb.saturating_mul(10) {
        return None;
    }
    let depth = if total <= bb.saturating_mul(20) {
        0
    } else if total <= bb.saturating_mul(100) {
        1
    } else if total <= bb.saturating_mul(200) {
        2
    } else {
        3
    };
    Some(depth + if straddle { 4 } else { 0 })
}

/// First-decision contexts split by zero/one/two/three-plus prior raises,
/// each with and without prior calls. A player returning after their own
/// previous action is never pooled into a first-decision read.
pub fn shove_opportunity_bucket(
    total: u32,
    bb: u32,
    straddle: bool,
    actor: u8,
    actions: impl IntoIterator<Item = (u8, PlayerAction)>,
) -> Option<usize> {
    let depth = open_shove_bucket(total, bb, straddle)?;
    let mut raises = 0;
    let mut called = false;
    for (seat, action) in actions {
        if seat == actor {
            return None;
        }
        match action {
            PlayerAction::Fold => {}
            PlayerAction::Raise { .. } | PlayerAction::AllIn => raises += 1,
            PlayerAction::Call => called = true,
            _ => return None,
        }
    }
    Some(depth + (raises.min(3) + usize::from(called) * 4) * 8)
}

/// Session-owned public observations for one opponent seat across hands.
/// Counters saturate. Stored profiles include every non-optional counter.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct OpponentStats {
    /// BB's most recent responses to a Shark SB opening to 2.5 BB at 100 BB,
    /// with all others folded. Each dealt player count has its own window.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) sb_steal_folds: Option<[ShoveRead; 8]>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) hu100_open_jams: Option<hu_jam::OpenJams>,
    /// Context-specific all-street windows.
    pub all_in_reads: [[ShoveRead; 32]; 8],
    /// Recent public limp-return reads, by own depth and straddle.
    pub limp_reraise_reads: [LimpReraiseRead; 8],
    /// Hands this opponent was dealt into while observed.
    pub hands: u32,
    /// Hands where the opponent voluntarily put money in preflop (call/raise).
    pub vpip_hands: u32,
    /// How often the opponent opened the betting on each postflop street
    /// ([`opening_read`]).
    pub opening_bets: opening_read::OpeningReads,
    /// Context-matched recent reads.
    pub shove_reads: [[ShoveRead; 8]; 8],
    /// Times the opponent FACED a flop continuation bet (a spot to fold-to-cbet).
    pub cbet_faced: u32,
    /// Of those, times the opponent FOLDED to the flop c-bet.
    pub cbet_folded: u32,
    /// Postflop actions taken by the opponent that were bet/raise (aggressive).
    pub aggressive_actions: u32,
    /// Postflop actions taken by the opponent total (for aggression frequency).
    pub postflop_actions: u32,
    /// Voluntary heads-up responses to an opening bet, indexed by
    /// flop/turn/river then <=half-pot/<=pot/overbet. Forced actions, facing raises,
    /// multiway pots and all-in wagers are excluded at observation time.
    pub bet_responses: [[BetResponseStats; 3]; 3],
    /// First response to an ordinary half-pot river open, with exactly 2–9
    /// live players. Each bit is one recently faced bet (fold or continue).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    river_half_pot_folds: Option<[ShoveRead; 8]>,
    /// The user whose cross-table profile is already folded into these reads
    /// (`bot::opponent_memory`). `None` until it loads; a different occupant
    /// never inherits it. Internal only: never sent to clients.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub profile_owner: Option<uuid::Uuid>,
}

impl OpponentStats {
    pub fn observe_river_half_pot(&mut self, players: usize, folded: bool) {
        if (2..=9).contains(&players) {
            self.river_half_pot_folds
                .get_or_insert_with(Default::default)[players - 2]
                .observe(folded);
        }
    }

    pub fn shove_read(&self, bucket: usize) -> ShoveRead {
        self.shove_reads
            .get(bucket / 8)
            .and_then(|row| row.get(bucket % 8))
            .copied()
            .unwrap_or_default()
    }
    pub fn observe_shove(&mut self, bucket: usize, shoved: bool) {
        if let Some(read) = self
            .shove_reads
            .get_mut(bucket / 8)
            .and_then(|row| row.get_mut(bucket % 8))
        {
            read.observe(shoved);
        }
    }

    /// Combined reads where everything in `earlier` was observed before
    /// everything in `self`: counters add, recent windows keep the newest.
    pub fn after(&self, earlier: &Self) -> Self {
        let add = |a: u32, b: u32| a.saturating_add(b);
        Self {
            sb_steal_folds: match (earlier.sb_steal_folds, self.sb_steal_folds) {
                (Some(first), Some(last)) => Some(std::array::from_fn(|i| first[i].then(last[i]))),
                (first, last) => last.or(first),
            },
            river_half_pot_folds: match (earlier.river_half_pot_folds, self.river_half_pot_folds) {
                (Some(first), Some(last)) => Some(std::array::from_fn(|i| first[i].then(last[i]))),
                (first, last) => last.or(first),
            },
            hu100_open_jams: hu_jam::OpenJams::merged(
                earlier.hu100_open_jams,
                self.hu100_open_jams,
            ),
            all_in_reads: std::array::from_fn(|i| {
                std::array::from_fn(|j| earlier.all_in_reads[i][j].then(self.all_in_reads[i][j]))
            }),
            limp_reraise_reads: std::array::from_fn(|i| {
                earlier.limp_reraise_reads[i].then(self.limp_reraise_reads[i])
            }),
            hands: add(earlier.hands, self.hands),
            vpip_hands: add(earlier.vpip_hands, self.vpip_hands),
            opening_bets: opening_read::merged(&earlier.opening_bets, &self.opening_bets),
            shove_reads: std::array::from_fn(|i| {
                std::array::from_fn(|j| earlier.shove_reads[i][j].then(self.shove_reads[i][j]))
            }),
            cbet_faced: add(earlier.cbet_faced, self.cbet_faced),
            cbet_folded: add(earlier.cbet_folded, self.cbet_folded),
            aggressive_actions: add(earlier.aggressive_actions, self.aggressive_actions),
            postflop_actions: add(earlier.postflop_actions, self.postflop_actions),
            bet_responses: std::array::from_fn(|i| {
                std::array::from_fn(|j| BetResponseStats {
                    faced: add(
                        earlier.bet_responses[i][j].faced,
                        self.bet_responses[i][j].faced,
                    ),
                    folded: add(
                        earlier.bet_responses[i][j].folded,
                        self.bet_responses[i][j].folded,
                    ),
                })
            }),
            profile_owner: self.profile_owner,
        }
    }

    /// The stored form: no seat owner, and cumulative ratios rescaled so a
    /// long history keeps its shape without letting years outweigh recent
    /// play. Recent windows are already bounded at 32 observations.
    pub fn stored(&self) -> Self {
        fn cap(pair: (&mut u32, &mut u32), limit: u32) {
            let (total, part) = pair;
            if *total > limit {
                *part = (u64::from(*part) * u64::from(limit) / u64::from(*total)) as u32;
                *total = limit;
            }
        }
        let mut out = *self;
        out.profile_owner = None;
        cap((&mut out.hands, &mut out.vpip_hands), 400);
        out.opening_bets = opening_read::capped(&out.opening_bets);
        cap((&mut out.cbet_faced, &mut out.cbet_folded), 200);
        cap(
            (&mut out.postflop_actions, &mut out.aggressive_actions),
            400,
        );
        for row in &mut out.bet_responses {
            for read in row {
                cap((&mut read.faced, &mut read.folded), 100);
            }
        }
        out
    }

    /// VPIP as a fraction (0.0..=1.0). `None` until there is a sample.
    pub fn vpip(&self) -> Option<f64> {
        (self.hands >= 1).then(|| self.vpip_hands as f64 / self.hands as f64)
    }
    /// Fold-to-c-bet as a fraction. `None` until the opponent has faced a c-bet.
    pub fn fold_to_cbet(&self) -> Option<f64> {
        (self.cbet_faced >= 1).then(|| self.cbet_folded as f64 / self.cbet_faced as f64)
    }
    /// Postflop aggression frequency. `None` until there is a postflop action.
    pub fn aggression_freq(&self) -> Option<f64> {
        (self.postflop_actions >= 1)
            .then(|| self.aggressive_actions as f64 / self.postflop_actions as f64)
    }
}

/// The Shark's read on the table: per-opponent stats keyed by SEAT, plus the
/// minimum sample size before a read is trusted. Borrowed read-only by the
/// Shark decision path; the session owns the backing store.
pub struct OpponentModel<'a> {
    pub(crate) steal_bb: Option<(u8, uuid::Uuid)>,
    pub(crate) extra_steal_opener: Option<u8>,
    /// Unambiguous human occupying the SB in this hand.
    pub(crate) opening_sb_user: Option<uuid::Uuid>,
    /// The current occupant must still be the person dealt this hand.
    pub river_responder: Option<(u8, uuid::Uuid)>,
    /// Per-seat opponent stats. Missing seats = unknown (play the base profile).
    pub by_seat: &'a std::collections::HashMap<u8, OpponentStats>,
    /// Minimum matched opportunities for a preflop shove read (default 12).
    pub min_sample: u32,
    /// Configured fold-read floor (default 12); street/size reads additionally
    /// require at least 20 matching voluntary opportunities.
    pub min_cbet_sample: u32,
}

impl<'a> OpponentModel<'a> {
    pub(super) fn river_response_stats(&self, seat: u8) -> Option<&OpponentStats> {
        let (first, user) = self.river_responder?;
        let stats = self.by_seat.get(&seat)?;
        (first == seat && stats.profile_owner == Some(user)).then_some(stats)
    }

    pub fn new(by_seat: &'a std::collections::HashMap<u8, OpponentStats>) -> Self {
        OpponentModel {
            steal_bb: None,
            extra_steal_opener: None,
            opening_sb_user: None,
            river_responder: None,
            by_seat,
            min_sample: 12,
            min_cbet_sample: 12,
        }
    }
}

/// An evidence-limited read for one street and opening-bet size.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum OpponentType {
    Unknown,
    OverFolder,
    Station,
    Balanced,
}

fn classify_bet_response(
    model: &OpponentModel<'_>,
    seat: u8,
    street: Street,
    bet: u32,
    pot_before: u32,
) -> OpponentType {
    let Some((street, size)) = bet_response_bucket(street, bet, pot_before) else {
        return OpponentType::Unknown;
    };
    let Some(stats) = model.by_seat.get(&seat) else {
        return OpponentType::Unknown;
    };
    let read = stats.bet_responses[street][size];
    if read.faced < model.min_cbet_sample.max(20) {
        return OpponentType::Unknown;
    }
    // Sixteen virtual opportunities at 40% shrink noisy reads. There is no
    // inference from VPIP/aggression to bluffing, or from flop to later streets.
    let fold_rate = (f64::from(read.folded.min(read.faced)) + 6.4) / (f64::from(read.faced) + 16.0);
    if fold_rate >= 0.62 {
        OpponentType::OverFolder
    } else if fold_rate <= 0.22 {
        OpponentType::Station
    } else {
        OpponentType::Balanced
    }
}

// ---------------------------------------------------------------------------
// Main decision function
// ---------------------------------------------------------------------------

/// Decide the bot's action given a `DecisionContext`, with an optional live
/// opponent model. Only [`BotPersona::Shark`] consumes the model; every other
/// persona ignores it and falls through to [`decide`] verbatim, so this is a
/// strict superset of `decide` and changes nothing for the casual tier.
///
/// The session calls this for Shark seats (passing the accumulated reads) and
/// `decide` for everyone else.
pub fn decide_with_opponents(
    ctx: &mut DecisionContext<'_>,
    opponents: Option<&OpponentModel<'_>>,
) -> PlayerAction {
    decide_with_opponents_evidence(ctx, opponents).0
}

/// Same single decision with bounded private attribution for the session log.
pub fn decide_with_opponents_evidence(
    ctx: &mut DecisionContext<'_>,
    opponents: Option<&OpponentModel<'_>>,
) -> (PlayerAction, Option<LimpDecisionEvidence>) {
    let (action, limp, _) = table_decision(ctx, opponents);
    (action, limp)
}

/// The last item identifies the strategy that selected the action in the session log.
pub(crate) fn table_decision(
    ctx: &mut DecisionContext<'_>,
    opponents: Option<&OpponentModel<'_>>,
) -> (PlayerAction, Option<LimpDecisionEvidence>, &'static str) {
    table_decision_with_postflop(ctx, opponents, None)
}

pub(crate) fn table_decision_with_postflop(
    ctx: &mut DecisionContext<'_>,
    opponents: Option<&OpponentModel<'_>>,
    postflop: Option<postflop_work::Policy<'_>>,
) -> (PlayerAction, Option<LimpDecisionEvidence>, &'static str) {
    if ctx.persona == BotPersona::Shark
        && ctx.plain_cash
        && opponents.is_some_and(|m| m.extra_steal_opener == Some(ctx.seat))
    {
        return (
            blind_steal::continuation(ctx),
            None,
            "sb_steal_continuation",
        );
    }
    let mut evidence = None;
    let (action, source) = if let Some(choice) = shark_baseline(ctx, opponents) {
        choice
    } else if let Some(choice) = postflop.and_then(|policy| policy.decision(ctx.rng, opponents)) {
        choice
    } else {
        let proposed = match (ctx.persona, opponents) {
            (BotPersona::Shark, Some(model)) => decide_shark(ctx, model, &mut evidence),
            _ => chart_action(ctx),
        };
        (review(ctx, proposed, opponents), "range_policy")
    };
    if action == PlayerAction::Fold
        && ctx.persona == BotPersona::Shark
        && ctx.plain_cash
        && ctx.street == Street::Preflop
        && opponents.is_some_and(|model| hu_jam::adjust_fold(ctx, model))
    {
        let adjusted_source = if source == "gtopen_preflop" {
            "gtopen_hu_open_jam_read"
        } else {
            "range_hu_open_jam_read"
        };
        return (PlayerAction::Call, evidence, adjusted_source);
    }
    let (action, source) =
        multiway_exploit::adjust(ctx, opponents, &action).unwrap_or((action, source));
    (enforce_raise_right(ctx, action), evidence, source)
}

fn shark_baseline(
    ctx: &mut DecisionContext<'_>,
    opponents: Option<&OpponentModel<'_>>,
) -> Option<(PlayerAction, &'static str)> {
    if ctx.persona != BotPersona::Shark || !ctx.plain_cash {
        return None;
    }
    if let Some(decision) = split_board_decision(ctx, opponents) {
        return Some(decision);
    }
    if let Some(action) = multiway_exploit::sole_wager_with_nuts(ctx) {
        return Some((action, "river_sole_wager_nuts"));
    }
    match ctx.street {
        Street::Preflop => forced_blind::decision(ctx)
            .map(|a| (a, "preflop_equity_bound"))
            .or_else(|| preflop_blueprint::decision_with_opponents(ctx, opponents))
            .or_else(|| {
                // An open jam can have too little reach in the solved game.
                // Price it with the existing unread range. Only this precise
                // opponent/line's observation may then widen a sampled fold.
                hu_jam::matches(ctx).then(|| {
                    let no_reads = std::collections::HashMap::new();
                    let prior_pressure = std::mem::take(&mut ctx.line_pressure);
                    let action = all_in::defend(ctx, &OpponentModel::new(&no_reads))
                        .unwrap_or_else(|| chart_action(ctx));
                    ctx.line_pressure = prior_pressure;
                    (action, "range_policy")
                })
            }),
        Street::Flop | Street::Turn | Street::River => {
            terminal::decision(ctx).map(|a| (a, "showdown_bound"))
        }
    }
}

/// The layers every persona's action passes after its own strategy: big-pot
/// discipline, the squid layers, then the online opening restraint.
/// Discipline counts the token of this pot for at most one pot, so run last
/// it folded the calls the squid defence had just priced with the whole
/// token and handed the pot back (nine-seat blood-squid simulation: a loose
/// player's fold-out wins went from 4% of hands to 9%). The squid layers
/// therefore price the action discipline leaves — its folds and checks
/// included — and have the last word on calls. Opening restraint runs last
/// so a squid stab cannot recreate an air bet just restrained against this
/// opponent; it never changes a call, a top-pair-or-better bet or action facing a wager.
/// The 7-2 bounty is priced by the same
/// layers as the token ([`seven_deuce`]). With no token or bounty value both
/// are no-ops, so tables without a squid round or bounty are unchanged.
fn review(
    ctx: &mut DecisionContext<'_>,
    action: PlayerAction,
    opponents: Option<&OpponentModel<'_>>,
) -> PlayerAction {
    let action = commitment::discipline(ctx, action, opponents);
    let defended = squid_defense::defend(ctx, action.clone());
    let action = seven_deuce::open(ctx, action, defended);
    let action = squid_defense::attack(ctx, action, opponents);
    adapt::restrain_open(ctx, action)
}

/// Decide the bot's action given a `DecisionContext`.
///
/// Returns a legal action. Shark first uses the admitted chip-EV policy when available.
pub fn decide(ctx: &mut DecisionContext<'_>) -> PlayerAction {
    if let Some((action, _)) = shark_baseline(ctx, None) {
        return enforce_raise_right(ctx, action);
    }
    chart_action(ctx)
}

fn chart_action(ctx: &mut DecisionContext<'_>) -> PlayerAction {
    // ADR-047 §5.1 — explicit per-street postflop routing.
    let action = match ctx.street {
        Street::Preflop => decide_preflop(ctx),
        Street::Flop => decide_flop(ctx),
        Street::Turn => decide_turn(ctx),
        Street::River => decide_river(ctx),
    };
    enforce_raise_right(ctx, action)
}

/// Apply the engine's authoritative raise right to every strategy exit.
///
/// A bot can reach an aggressive branch with a premium hand even when poker
/// rules allow no aggressive action (for example, it is the sole live stack
/// facing an all-in). Returning that branch's `Raise`/`AllIn` makes the engine
/// reject the action; the session's invalid-action safety fallback then folds
/// the premium hand. Preserve a deliberate fold while facing a wager, but map
/// every other no-raise result to the only passive continuation. With no call
/// debt, checking is the sole sensible action.
fn enforce_raise_right(ctx: &DecisionContext<'_>, action: PlayerAction) -> PlayerAction {
    if ctx.can_raise {
        return action;
    }
    if ctx.to_call.0 == 0 {
        return PlayerAction::Check;
    }
    if matches!(action, PlayerAction::Fold) {
        PlayerAction::Fold
    } else {
        PlayerAction::Call
    }
}

// ---------------------------------------------------------------------------
// Preflop decision
// ---------------------------------------------------------------------------

/// Caricature calling-station continue width, as a percentage of the 169
/// hand-type grid. This ONE constant is the single Station-VPIP lever: it drives
/// both the unopened limp band (`decide_rfi`) and the facing-an-open flat band
/// (`decide_station_facing_open`), so there is exactly one Station-facing-open
/// path and one Station-limp path — no competing `call_vs_open_pct` mechanism.
/// Cranked up from the realistic ~38% to make the Station the LOOSEST seat at
/// the table (VPIP top, ≳ Maniac) — the "calls everything fish" caricature.
const STATION_CONTINUE_PCT: f64 = 68.0;

/// Caricature nit defend width facing an open, as a percentage of the 169
/// hand-type grid. Tightened from the realistic ~10% so the Nit defends the
/// narrowest of any persona (VPIP bottom) — the "folds everything rock"
/// caricature. Premiums still 3-bet (handled before this band).
const NIT_DEFEND_PCT: f64 = 7.0;

// ---------------------------------------------------------------------------
// Aggression helpers (fix/bot-persona-caricature-v2)
// ---------------------------------------------------------------------------

/// Pot odds as a whole percentage, floored for the integer persona thresholds.
/// `u32` chips widen exactly to `f64`, so this matches integer `100·call / (pot + call)`.
fn pot_odds_pct(pot: Chips, to_call: Chips) -> u32 {
    engine::solver::pot_odds_pct(f64::from(pot.0), f64::from(to_call.0)).floor() as u32
}

/// Amplify a base postflop bet/raise FREQUENCY (a persona percentage 0..=100) by
/// the persona's `aggression_multiplier`, returning a probability in `[0.0, 1.0]`
/// safe to feed to `rng.gen_bool`. This is how the multiplier now bites on
/// FREQUENCY (not just sizing): a LAG/Maniac (>1.0) bets more often, a
/// Nit/Station (<1.0) less often, and the ClubRegular baseline (1.0) is a strict
/// identity so its statistical bands are unchanged. Clamped so an amplified
/// frequency can never panic `gen_bool` or exceed certainty.
fn scaled_freq(profile: &PersonaProfile, base_pct: u8) -> f64 {
    ((base_pct as f64 / 100.0) * profile.aggression_multiplier as f64).clamp(0.0, 1.0)
}

/// Minimum open-bet frequency with a genuine value hand (top pair / over-pair
/// or better). The persona c-bet / barrel knobs describe how often a persona
/// fires its WHOLE range; they were applied to value hands too, so a Nit
/// could check a strong hand too often. Value hands bet at least this often for every
/// persona; the knobs keep governing bluffs, draws and marginal hands.
/// Station is unaffected (its `suppresses_postflop_raise` check runs first).
const VALUE_BET_FLOOR: f64 = 0.85;

/// Top pair / over-pair or better: the value region that should keep betting
/// when checked to, regardless of persona aggression. Conservative runout
/// guard: once the board shows four cards of one suit, every non-flush
/// "value" hand is a bluff-catcher, so it falls back to the persona frequency.
fn holds_value_hand(ctx: &DecisionContext<'_>, made: MadeHand) -> bool {
    holding_has_value(ctx.hole, ctx.board, made)
}

fn holding_has_value(hole: HoleCards, board: &BoardCards, made: MadeHand) -> bool {
    let strong = made >= MadeHand::TwoPair
        || (made == MadeHand::OnePair && has_top_pair_or_overpair(hole, board));
    if !strong {
        return false;
    }
    if board_has_four_flush(board) {
        // A flush, or a hand that beats every flush, is still value there.
        return matches!(
            engine::rank_hand(&hole, board),
            HandRank::Flush(_)
                | HandRank::FullHouse(_)
                | HandRank::FourOfAKind(_)
                | HandRank::StraightFlush(_)
        );
    }
    true
}

/// Four or more board cards of one suit (one-card flushes are live).
fn board_has_four_flush(board: &BoardCards) -> bool {
    let mut suit_counts = [0u8; 4];
    for c in board.all_cards() {
        suit_counts[c.suit as usize] += 1;
    }
    suit_counts.iter().any(|&n| n >= 4)
}

/// Persona-scaled open-bet frequency, floored at [`VALUE_BET_FLOOR`] for value.
/// The Station (`suppresses_postflop_raise`) keeps its caricature passivity
/// even with a monster, and the Shark keeps its self-play-tuned fallback
/// frequencies: both are out of scope for the floor.
fn open_bet_freq(ctx: &DecisionContext<'_>, base_pct: u8, value: bool) -> f64 {
    let freq = scaled_freq(ctx.profile, base_pct);
    let floor_applies =
        value && !ctx.profile.suppresses_postflop_raise && ctx.persona != BotPersona::Shark;
    if floor_applies {
        freq.max(VALUE_BET_FLOOR)
    } else {
        freq
    }
}

/// Amplify a base bet/raise SIZE (in chips) by the persona's
/// `aggression_multiplier`. The result is still routed through
/// `emit_bet`/`emit_raise`, which clamp to legal amounts (min-raise, stack), so
/// an aggressive persona's bigger sizing can never produce an illegal action —
/// it just shoves when the amplified size meets/exceeds the stack. ClubRegular
/// (1.0) is an identity.
fn scaled_bet(profile: &PersonaProfile, base_chips: u32) -> u32 {
    (base_chips as f64 * profile.aggression_multiplier as f64).round() as u32
}

/// BUG-209 — mean-preserving sizing jitter for postflop BETS (c-bet, barrel,
/// probe, delayed c-bet, value bet). Without it every bet was a deterministic
/// function of the pot, so identical pot histories replayed an identical
/// barrel tree (Chaos Cal's 7 / 17 / 65 in two consecutive hands) — a sizing
/// tell. Multiplies the persona-scaled size by a uniform factor in
/// `[1 - JITTER, 1 + JITTER]`; the ADR-047 anchors stay the expected value
/// and `emit_bet` still clamps the result legal.
const POSTFLOP_BET_SIZE_JITTER: f64 = 0.20;

fn jittered_bet(ctx: &mut DecisionContext<'_>, scaled_chips: u32) -> u32 {
    let factor = ctx
        .rng
        .gen_range((1.0 - POSTFLOP_BET_SIZE_JITTER)..=(1.0 + POSTFLOP_BET_SIZE_JITTER));
    ((scaled_chips as f64) * factor).round().max(1.0) as u32
}

// ---------------------------------------------------------------------------
// Sizing helpers
// ---------------------------------------------------------------------------

/// Emit a raise action, clamping to `max(requested_amount, min_raise_to)` and
/// capping at the bot's total stack. Never emits a min-raise when we can afford
/// more (kills the "min-raise festival" bug, ADR-024 §4.2).
fn emit_raise(ctx: &mut DecisionContext<'_>, requested_raise_to: u32) -> PlayerAction {
    // Raise permission comes from the engine, never from a sizing fallback.
    // In particular, `min_raise_to == None` while facing an all-in must not be
    // converted into a synthetic shove that the engine will reject.
    if !ctx.can_raise {
        return if ctx.to_call.0 > 0 {
            PlayerAction::Call
        } else {
            PlayerAction::Check
        };
    }

    // Maximum legal raise-to (total chips committed this street if we shove):
    // chips already in front of us this street + everything still behind.
    // `stack_chips` is the exact chips behind (the call is paid out of those
    // chips, so it must NOT be added separately — doing so overcounts the ceiling
    // by `to_call` and can emit a Raise larger than our stack, which the engine
    // rejects and the session then converts to a Fold of a strong hand).
    let available = ctx.committed_this_street.0.saturating_add(ctx.stack_chips);

    // Ensure we meet the minimum raise.
    let min_rt = ctx
        .min_raise_to
        .map(|c| c.0)
        .unwrap_or(ctx.current_bet.0.saturating_add(ctx.big_blind));

    // Ensure raise-to is at least 50% pot (kills min-raise festival).
    let half_pot_raise_to = ctx
        .committed_this_street
        .0
        .saturating_add(ctx.to_call.0)
        .saturating_add((ctx.pot.0 as f64 * 0.50).round() as u32);

    let raise_to = requested_raise_to
        .max(min_rt)
        .max(half_pot_raise_to)
        .min(available);

    if raise_to >= available || raise_to <= ctx.current_bet.0 {
        // Can't raise properly — go all-in if we have a strong hand, else call.
        if ctx.to_call.0 > 0 {
            return PlayerAction::AllIn;
        }
        return PlayerAction::Check;
    }

    if raise_to < min_rt {
        // Can't meet minimum raise — go all-in.
        if ctx.to_call.0 > 0 {
            return PlayerAction::AllIn;
        }
        return PlayerAction::Check;
    }

    PlayerAction::Raise {
        amount: Chips(raise_to),
    }
}

/// Emit a bet action (no prior bet this street). Clamps to available chips.
fn emit_bet(ctx: &mut DecisionContext<'_>, bet_size: u32) -> PlayerAction {
    if !ctx.can_raise {
        return if ctx.to_call.0 > 0 {
            PlayerAction::Call
        } else {
            PlayerAction::Check
        };
    }

    let bb = ctx.big_blind;
    let min_bet = bb; // Minimum bet is 1 BB.
    let available = ctx.stack_chips;

    let actual = bet_size.max(min_bet).min(available);
    if actual == 0 || available == 0 {
        return PlayerAction::Check;
    }
    if actual >= available {
        return PlayerAction::AllIn;
    }

    PlayerAction::Raise {
        amount: Chips(actual),
    }
}

// ---------------------------------------------------------------------------
// Position classification
// ---------------------------------------------------------------------------

fn is_in_position(ctx: &DecisionContext<'_>) -> bool {
    // Exact answer from the public table state when it is known: postflop we
    // are in position when nobody who can still act sits after us; preflop
    // facing a raise we are in position when no opponent already in the pot
    // (the raiser or anyone who called/raised this street) can still act
    // after us in the post-dealer order. So the BB is in position against an
    // SB raise, the CO is NOT in position against a BTN 3-bet, and a BB facing
    // an SB all-in behind a live BTN open stays out of position (the all-in
    // seat never acts again, the BTN does). Unopened preflop pots and callers
    // without table state use the positional convention.
    if ctx.table.is_known() {
        let exact = if ctx.street == Street::Preflop {
            match ctx.last_aggressor_seat.filter(|&s| s != ctx.seat) {
                Some(aggressor) => ctx.table.closes_action_among(ctx.seat, |s| {
                    s.seat == aggressor
                        || ctx.street_actions.iter().any(|(seat, a)| {
                            *seat == s.seat
                                && matches!(
                                    a,
                                    PlayerAction::Call
                                        | PlayerAction::Raise { .. }
                                        | PlayerAction::AllIn
                                )
                        })
                }),
                None => Some(opens_from_late_seat(ctx.position)),
            }
        } else {
            ctx.table.closes_postflop_action(ctx.seat)
        };
        if let Some(ip) = exact {
            return ip;
        }
    }
    opens_from_late_seat(ctx.position)
}

/// Positional convention used for unopened pots and legacy callers: the three
/// latest seats open smaller and are treated as in position.
fn opens_from_late_seat(position: Position) -> bool {
    matches!(
        position,
        Position::Dealer | Position::Cutoff | Position::Hijack
    )
}

/// A memo-table hasher for search states whose key is already a well-mixed
/// word (斗地主 and 掼蛋 bots); SipHash would dominate a decision.
#[derive(Default)]
pub(crate) struct KeyHasher(u64);

impl std::hash::Hasher for KeyHasher {
    fn finish(&self) -> u64 {
        self.0
    }
    fn write(&mut self, bytes: &[u8]) {
        for &byte in bytes {
            self.write_u64(u64::from(byte));
        }
    }
    fn write_u64(&mut self, value: u64) {
        self.0 = (self.0 ^ value)
            .wrapping_mul(0x9E37_79B9_7F4A_7C15)
            .rotate_left(29);
    }
}
