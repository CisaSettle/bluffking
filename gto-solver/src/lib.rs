//! `gto-solver` — a thin, SAFE, ADR-012-clean wrapper over the open-source
//! [`postflop-solver`](https://github.com/b-inary/postflop-solver) crate
//! (AGPL-3.0-or-later, Discounted-CFR over the unabstracted 2-player postflop
//! game tree).
//!
//! # What this is (HONESTY)
//! This is a REAL approximate Nash equilibrium solve. The upstream crate runs
//! Discounted-CFR (γ=3.0) over the FULL postflop game tree with **no range or
//! bet abstraction** beyond the (configurable, disclosed) bet-size set you give
//! it (isomorphic turn/river suit merging is exact, not lossy). The output is a
//! genuine equilibrium approximation with a **measured exploitability** we
//! report back to the caller.
//!
//! It is therefore correctly labeled **GTO** — but ALWAYS qualified, never bare:
//! it is GTO *for the inputs given*. The equilibrium is only as meaningful as
//! (a) the assumed OOP/IP ranges, (b) the (deliberately limited) bet-size set,
//! and (c) the iteration / exploitability stopping point. We surface all three
//! in [`SolveOutput`] so the UI can disclose them. The honesty label is
//! [`SolveMethod::CfrEquilibrium`] — categorically distinct from the engine's
//! existing honestly-labeled `EquityHeuristic` / `PreflopChart`. NEVER relabel a
//! CFR solve as a bare "GTO" without the bet-size / iteration / exploitability
//! qualification.
//!
//! # ADR-012 boundary
//! The public API takes and returns ONLY engine types ([`engine::card::Card`],
//! [`engine::hand::HoleCards`]) and plain owned structs. No `postflop_solver` or
//! `rs_poker` type ever appears in a public signature.
//!
//! # Safety / robustness
//! The upstream crate uses `unsafe` + SIMD internally, so this crate cannot live
//! under the `forbid(unsafe)` `engine` crate. ALL inputs are validated before a
//! game is constructed (cards legal + unique, board legal for the street, ranges
//! parse, pot/stack sane), and the heavy solve is meant to be called from a
//! `spawn_blocking` task by the server so any internal panic is isolated.
//!
//! # Cost / DoS guard
//! Memory is dominated by `range-width × bet-size-count` (tree branching), only
//! secondarily by iteration count. [`solve_spot`] calls the solver's
//! `memory_usage()` and REJECTS (without allocating) when the estimate exceeds
//! [`SolveLimits::max_memory_bytes`]. The caller is additionally expected to
//! impose a wall-clock timeout + a global concurrency cap (a flop solve is
//! ~1.5 GB+).

use engine::card::Card;
use engine::hand::HoleCards;
use serde::Serialize;
use thiserror::Error;

use postflop_solver::{
    card_from_str, flop_from_str, holes_to_strings, solve, Action as PfAction, ActionTree,
    BetSizeOptions, BoardState, CardConfig, PostFlopGame, Range, TreeConfig, NOT_DEALT,
};

/// Honesty badge for a solve result — the structural twin of the engine's
/// `solver::SolveMethod`, kept SEPARATE so a CFR equilibrium can never be
/// silently relabeled as the engine's `EquityHeuristic`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum SolveMethod {
    /// Real per-spot CFR equilibrium (Discounted-CFR over the unabstracted
    /// 2-player postflop game tree). GTO *for the specified ranges + bet sizes*,
    /// solved to the reported exploitability. The label MUST be presented with
    /// the bet sizes used, iterations run, and achieved exploitability — never as
    /// a bare "GTO".
    CfrEquilibrium,
}

/// Which player a strategy/equity/EV view is for.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Player {
    /// Out-of-position player (range index 0).
    Oop,
    /// In-position player (range index 1).
    Ip,
}

impl Player {
    fn index(self) -> usize {
        match self {
            Player::Oop => 0,
            Player::Ip => 1,
        }
    }
}

/// A single available action at the solved root node, with the equilibrium
/// frequency the SOLVING player takes it with (range-averaged over all hands
/// that reach this node).
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct ActionFreq {
    /// Action label: `"fold"`, `"check"`, `"call"`, `"bet"`, `"raise"`,
    /// `"all_in"`. Plain strings — no `postflop_solver::Action` leaks.
    pub action: String,
    /// Bet/raise amount in the tree's chip scale (`None` for fold/check/call).
    /// Same scale as `starting_pot` / `effective_stack` in the request.
    pub amount: Option<i32>,
    /// Range-average equilibrium frequency this action is taken with, 0..1.
    pub frequency: f32,
}

/// Per-hand strategy at the solved root for ONE specific holding (e.g. hero's
/// hand). Frequencies are aligned 1:1 with [`SolveOutput::actions`].
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct HandStrategy {
    /// The holding, e.g. `"AsKh"` (descending card order, the solver's form).
    pub hand: String,
    /// Per-action equilibrium frequency for this exact hand, aligned with
    /// [`SolveOutput::actions`] (index `i` ↔ `actions[i]`). Sums ~1.0.
    pub frequencies: Vec<f32>,
    /// This hand's equity at the node, 0..1 (share of pot at showdown).
    pub equity: f32,
    /// This hand's expected value at the node, in the tree's chip scale.
    pub ev: f32,
}

/// Cost estimate echoed back so the caller / UI can disclose it (and so the
/// caller can log real per-solve cost).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
pub struct SolveCost {
    /// Estimated uncompressed memory the solve allocated, in bytes (from the
    /// solver's `memory_usage()` BEFORE allocation — the gate value).
    pub memory_bytes: u64,
    /// The iteration CAP the solve ran under (NOT the count actually run). F5
    /// (codex MED): the upstream `solve()` returns only the achieved
    /// exploitability, never the iteration count, so the actual count is not
    /// available without `solve_step` instrumentation. This is the bound — the
    /// equilibrium is typically reached in far fewer iterations — so the field
    /// is named `iteration_cap` to be honest about what it is. (The wire DTO
    /// surfaces it as `max_iterations`, matching this meaning.)
    pub iteration_cap: u32,
}

/// The full solve result — all numbers derived from the SAME equilibrium so the
/// strategy, equity and EV are internally consistent.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct SolveOutput {
    /// Honesty badge — always [`SolveMethod::CfrEquilibrium`] here.
    pub method: SolveMethod,
    /// Which player the [`actions`](Self::actions) / [`hero`](Self::hero) view is
    /// for (the player to act at the solved root, mirrored from the request).
    pub solving_player: Player,
    /// Achieved exploitability of the solved equilibrium, in the tree's chip
    /// scale (per the upstream `solve` return). Reported as ACHIEVED, not a
    /// fixed guarantee — CFR-with-rayon is not bit-reproducible across thread
    /// counts.
    pub exploitability: f32,
    /// Exploitability as a percentage of the starting pot (the disclosable
    /// "how close to GTO" figure). `exploitability / starting_pot * 100`.
    pub exploitability_pct_of_pot: f32,
    /// Available actions at the solved root + the solving player's range-average
    /// equilibrium frequency for each.
    pub actions: Vec<ActionFreq>,
    /// Range-average equity of the solving player at the root, 0..1.
    pub range_equity: f32,
    /// Range-average EV of the solving player at the root, in the tree's chip
    /// scale.
    pub range_ev: f32,
    /// The solving player's specific HERO hand strategy (present iff the request
    /// supplied a `hero` hand that is in that player's range and not blocked by
    /// the board).
    pub hero: Option<HandStrategy>,
    /// The solving player's FULL per-hand strategy table at the solved root —
    /// every live hand in that player's range with its per-action frequencies,
    /// equity and EV. This is the HERO-INDEPENDENT product of the equilibrium:
    /// the optional [`hero`](Self::hero) above is just one row SELECTED out of
    /// this table. F3 (codex MED): the server caches this table under the
    /// hero-independent spot key so changing ONLY the hero hand re-derives the
    /// hero row cheaply via [`hero_strategy`] instead of forcing a full
    /// (GB-scale) re-solve. Bounded by the public-tier range width.
    pub hands: Vec<HandStrategy>,
    /// Solve-cost disclosure (memory + iterations).
    pub cost: SolveCost,
}

/// Select one hero hand's [`HandStrategy`] out of a previously-solved per-hand
/// table ([`SolveOutput::hands`]) — the CHEAP, hero-independent derivation F3
/// relies on. Returns `None` when the hero hand is not in the solving player's
/// range (or was blocked by the board), exactly mirroring the in-solve hero
/// extraction. The lookup is by the solver's canonical hole-card string, so it is
/// order-insensitive on the hero's two cards. No solve, no game tree — a pure
/// table lookup over the already-computed equilibrium.
pub fn hero_strategy(hands: &[HandStrategy], hero: HoleCards) -> Option<HandStrategy> {
    let hero_str = solver_hole_string(hero);
    hands.iter().find(|h| h.hand == hero_str).cloned()
}

/// Which street the solve starts on (mirrors the upstream `BoardState`, exposed
/// as our own enum so the dep type does not leak).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SolveStreet {
    Flop,
    Turn,
    River,
}

/// Hard safety bounds applied to every solve. The caller (server) owns the
/// wall-clock timeout + concurrency semaphore; THIS struct caps the per-solve
/// memory and iterations (the OOM and runaway-iteration guards).
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct SolveLimits {
    /// Reject (before allocating) when the solver's estimated uncompressed
    /// memory exceeds this. A flop with 1 bet size is ~1.5 GB; 2 bet sizes
    /// blows past 7 GB and MUST be rejected on a shared public box.
    pub max_memory_bytes: u64,
    /// Iteration cap — a safety net. Flop converges to <1% pot in well under
    /// 100 iters, so this rarely bites; it bounds a pathological non-converging
    /// solve.
    pub max_iterations: u32,
    /// Stop early once exploitability reaches this fraction of the starting pot
    /// (e.g. `0.005` = 0.5% of pot). The achieved value is reported regardless.
    pub target_exploitability_pct_of_pot: f32,
}

impl Default for SolveLimits {
    fn default() -> Self {
        Self {
            // 1.5 GB — a single flop solve with ONE bet size fits (~1.58 GB
            // measured); two bet sizes (7 GB+) are rejected. Tunable by the
            // caller.
            max_memory_bytes: 1_500_000_000,
            max_iterations: 1000,
            target_exploitability_pct_of_pot: 0.005,
        }
    }
}

/// A fully-described spot to solve. All cards are engine [`Card`]s; ranges are
/// the upstream crate's range-string grammar (e.g. `"66+,A8s+,AJo+"`) which the
/// caller maps from its own range representation and we validate before solving.
#[derive(Debug, Clone)]
pub struct SolveRequest {
    /// Street the tree starts on.
    pub street: SolveStreet,
    /// Flop cards (exactly 3, always required — every postflop spot has a flop).
    pub flop: [Card; 3],
    /// Turn card — required iff `street` is `Turn` or `River`.
    pub turn: Option<Card>,
    /// River card — required iff `street` is `River`.
    pub river: Option<Card>,
    /// OOP range string (upstream grammar). e.g. `"66+,A8s+,AJo+"`.
    pub oop_range: String,
    /// IP range string (upstream grammar).
    pub ip_range: String,
    /// Starting pot in chips (must be > 0). EV/exploitability use this scale.
    pub starting_pot: i32,
    /// Effective stack in chips (must be > 0).
    pub effective_stack: i32,
    /// Bet sizes (upstream grammar, e.g. `"50%"` or `"33%,75%"`). The PUBLIC
    /// tier caps this to ONE flop size — enforce that in the caller, not here.
    pub bet_sizes: String,
    /// Raise sizes (upstream grammar, e.g. `"2.5x"`).
    pub raise_sizes: String,
    /// Optional hero hand whose specific strategy to extract.
    pub hero: Option<HoleCards>,
    /// Which player is to act at the solved root (the strategy view).
    pub solving_player: Player,
    /// Safety bounds.
    pub limits: SolveLimits,
}

/// Errors from validation or the solve. All are caller-input errors except
/// [`SolveError::Internal`]; map the input ones to a 400.
#[derive(Debug, Error, PartialEq, Eq)]
pub enum SolveError {
    /// A board card is illegal or the flop/turn/river cards collide.
    #[error("invalid board: {0}")]
    InvalidBoard(String),
    /// The street and the supplied turn/river cards are inconsistent.
    #[error("street/board mismatch: {0}")]
    StreetMismatch(String),
    /// A range string failed to parse against the upstream grammar.
    #[error("invalid range ({player}): {detail}")]
    InvalidRange { player: String, detail: String },
    /// A bet/raise size string failed to parse.
    #[error("invalid bet sizing: {0}")]
    InvalidBetSize(String),
    /// pot/stack out of the legal range.
    #[error("invalid pot/stack: {0}")]
    InvalidStakes(String),
    /// The requested `solving_player` is not the player to act at the solved
    /// ROOT node. At the root only ONE player acts first (OOP on most postflop
    /// trees); the solver exposes `available_actions()` / `strategy()` for THAT
    /// player only. Reading them while pairing with the OTHER player's private
    /// cards yields a misaligned/garbage strategy. We therefore reject this up
    /// front (and the UI hides the option) rather than return a wrong "GTO"
    /// action mix — a correctness AND honesty guard. Carries the player that
    /// actually acts at the root (`"oop"`/`"ip"`).
    #[error("solving_player is not the player to act at the root (root actor is {root_actor})")]
    PlayerNotAtRoot { root_actor: String },
    /// The solver's estimated memory exceeds the cap — REJECTED before
    /// allocating (DoS / OOM guard). Carries (estimated, cap) bytes.
    #[error("solve too large: estimated {estimated} bytes exceeds cap {cap} bytes")]
    TooLarge { estimated: u64, cap: u64 },
    /// The upstream crate returned an error while building the game tree.
    #[error("solver build error: {0}")]
    Internal(String),
}

/// CHEAP, solve-free validation of a [`SolveRequest`]: board legality + street
/// consistency, range parse + non-empty, stakes bounds, and bet/raise sizing
/// parse. Does NOT build the game tree or run the solver, so it is fast and
/// allocation-light.
///
/// The server calls this BEFORE acquiring the scarce global solve permit, so a
/// flood of malformed requests can't each consume a permit and starve real
/// solves (a capacity-DoS hardening tied to F3). [`solve_spot`] re-runs the same
/// checks as the single source of truth, so callers that skip this are still
/// safe — it is a cheap fast-fail, not the only guard.
pub fn validate_request(req: &SolveRequest) -> Result<(), SolveError> {
    let (flop, turn, river) = parse_board(req)?;
    let (oop, ip) = parse_ranges(req)?;

    // F7 (codex MED): reject a range with zero LIVE combos after the board
    // removes its cards (the common "fully blocked by board" case) CHEAPLY here —
    // before the solve permit — so the handler returns a clean 400 without
    // consuming a scarce permit. `get_hands_weights(dead_mask)` counts a single
    // player's combos against the board. (The rarer all-OOP-vs-all-IP collision
    // is still caught post-build in `solve_spot` via `map_with_config_error`.)
    let dead_mask = board_dead_mask(&flop, turn, river);
    if oop.get_hands_weights(dead_mask).0.is_empty() {
        return Err(SolveError::InvalidRange {
            player: "oop".into(),
            detail: "range fully blocked by the board (no live combinations)".into(),
        });
    }
    if ip.get_hands_weights(dead_mask).0.is_empty() {
        return Err(SolveError::InvalidRange {
            player: "ip".into(),
            detail: "range fully blocked by the board (no live combinations)".into(),
        });
    }

    validate_stakes(req)?;
    BetSizeOptions::try_from((req.bet_sizes.as_str(), req.raise_sizes.as_str()))
        .map(|_| ())
        .map_err(SolveError::InvalidBetSize)?;
    // F1: on every postflop tree OOP acts first, so the root actor is ALWAYS
    // OOP — only the OOP view has a well-defined root strategy here. Reject an
    // IP view cheaply (before any permit/solve); `solve_spot`'s post-build
    // `current_player()` check in `project_output` is the runtime authority that
    // backs this structural assumption.
    if req.solving_player == Player::Ip {
        return Err(SolveError::PlayerNotAtRoot {
            root_actor: "oop".into(),
        });
    }
    Ok(())
}

/// Parse + validate the board into `(flop, turn, river)` solver cards, enforcing
/// the street↔card-count invariant and board-card uniqueness. Shared by
/// [`validate_request`] and [`solve_spot`].
fn parse_board(req: &SolveRequest) -> Result<([u8; 3], u8, u8), SolveError> {
    let flop_str = format!("{}{}{}", req.flop[0], req.flop[1], req.flop[2]);
    let flop = flop_from_str(&flop_str).map_err(SolveError::InvalidBoard)?;

    let turn = match (req.street, req.turn) {
        (SolveStreet::Flop, None) => NOT_DEALT,
        (SolveStreet::Flop, Some(_)) => {
            return Err(SolveError::StreetMismatch(
                "flop solve must not supply a turn card".into(),
            ))
        }
        (SolveStreet::Turn | SolveStreet::River, Some(c)) => {
            card_from_str(&c.to_string()).map_err(SolveError::InvalidBoard)?
        }
        (SolveStreet::Turn | SolveStreet::River, None) => {
            return Err(SolveError::StreetMismatch(
                "turn/river solve requires a turn card".into(),
            ))
        }
    };

    let river = match (req.street, req.river) {
        (SolveStreet::River, Some(c)) => {
            card_from_str(&c.to_string()).map_err(SolveError::InvalidBoard)?
        }
        (SolveStreet::River, None) => {
            return Err(SolveError::StreetMismatch(
                "river solve requires a river card".into(),
            ))
        }
        (SolveStreet::Flop | SolveStreet::Turn, None) => NOT_DEALT,
        (SolveStreet::Flop | SolveStreet::Turn, Some(_)) => {
            return Err(SolveError::StreetMismatch(
                "river card supplied for a non-river solve".into(),
            ))
        }
    };

    if turn != NOT_DEALT && flop.contains(&turn) {
        return Err(SolveError::InvalidBoard(
            "turn card duplicates a flop card".into(),
        ));
    }
    if river != NOT_DEALT && (flop.contains(&river) || river == turn) {
        return Err(SolveError::InvalidBoard(
            "river card duplicates a board card".into(),
        ));
    }
    Ok((flop, turn, river))
}

/// 52-bit dead-card mask for the board (flop + optional turn/river). Cards are
/// the solver's `u8` ids (0..52); `NOT_DEALT` slots are skipped.
fn board_dead_mask(flop: &[u8; 3], turn: u8, river: u8) -> u64 {
    let mut mask = 0u64;
    for &c in flop {
        mask |= 1u64 << c;
    }
    if turn != NOT_DEALT {
        mask |= 1u64 << turn;
    }
    if river != NOT_DEALT {
        mask |= 1u64 << river;
    }
    mask
}

/// Parse both ranges and reject empty ones. Shared by [`validate_request`] and
/// [`solve_spot`].
fn parse_ranges(req: &SolveRequest) -> Result<(Range, Range), SolveError> {
    let oop: Range = req
        .oop_range
        .parse()
        .map_err(|e: String| SolveError::InvalidRange {
            player: "oop".into(),
            detail: e,
        })?;
    let ip: Range = req
        .ip_range
        .parse()
        .map_err(|e: String| SolveError::InvalidRange {
            player: "ip".into(),
            detail: e,
        })?;
    if oop.is_empty() {
        return Err(SolveError::InvalidRange {
            player: "oop".into(),
            detail: "range is empty".into(),
        });
    }
    if ip.is_empty() {
        return Err(SolveError::InvalidRange {
            player: "ip".into(),
            detail: "range is empty".into(),
        });
    }
    Ok((oop, ip))
}

/// Bound the stakes: both must be in `1..=MAX_STAKE`. Shared.
fn validate_stakes(req: &SolveRequest) -> Result<(), SolveError> {
    // F8: the upstream tree uses `i32` for pot/stack/bet amounts throughout; a
    // very large public input can overflow or build a degenerate tree. Bound
    // both to a sane study range.
    const MAX_STAKE: i32 = 100_000;
    if req.starting_pot <= 0 {
        return Err(SolveError::InvalidStakes("starting_pot must be > 0".into()));
    }
    if req.effective_stack <= 0 {
        return Err(SolveError::InvalidStakes(
            "effective_stack must be > 0".into(),
        ));
    }
    if req.starting_pot > MAX_STAKE {
        return Err(SolveError::InvalidStakes(format!(
            "starting_pot must be <= {MAX_STAKE}"
        )));
    }
    if req.effective_stack > MAX_STAKE {
        return Err(SolveError::InvalidStakes(format!(
            "effective_stack must be <= {MAX_STAKE}"
        )));
    }
    Ok(())
}

/// Solve one fully-described postflop spot to an approximate Nash equilibrium.
///
/// SYNCHRONOUS and CPU/RAM-heavy (a flop solve is ~1.5 GB / single-digit
/// seconds). The server MUST call this inside `spawn_blocking` with a wall-clock
/// timeout + a global concurrency cap. Validates every input and applies the
/// [`SolveLimits`] memory cap BEFORE allocating; never panics on caller input
/// (the upstream `unsafe`/SIMD paths are reached only after full validation).
pub fn solve_spot(req: &SolveRequest) -> Result<SolveOutput, SolveError> {
    // --- Cheap, solve-free validation (single source of truth) --------------
    // N2 (codex MED): run the FULL `validate_request` first so EVERY input is
    // validated before a game is constructed — honoring this crate's documented
    // contract and closing a latent DoS footgun. Previously `solve_spot` ran
    // `parse_board`/`parse_ranges`/`validate_stakes` inline but SKIPPED the
    // IP-at-root rejection and the cheap board-blocked-range pre-check, so a
    // direct crate caller with `solving_player == Ip` (or a fully-blocked range)
    // would allocate + run the FULL CFR solve and only fail LATE in
    // `project_output` with `PlayerNotAtRoot`. The HTTP handler already calls
    // `validate_request` before the solve permit; doing it here too makes the
    // crate safe for any caller (the server's call is then a redundant fast-fail,
    // not the only guard). The board/range/stakes results are re-parsed below
    // (cheap) for the game build.
    validate_request(req)?;

    // Board legality + street consistency, ranges (parse + non-empty), and
    // stakes bounds — re-parsed into the concrete types the game build needs.
    let (flop, turn, river) = parse_board(req)?;
    let (oop, ip) = parse_ranges(req)?;
    validate_stakes(req)?;

    // --- Bet sizing ---------------------------------------------------------
    let bet_sizes = BetSizeOptions::try_from((req.bet_sizes.as_str(), req.raise_sizes.as_str()))
        .map_err(SolveError::InvalidBetSize)?;

    let initial_state = match req.street {
        SolveStreet::Flop => BoardState::Flop,
        SolveStreet::Turn => BoardState::Turn,
        SolveStreet::River => BoardState::River,
    };

    let card_config = CardConfig {
        range: [oop, ip],
        flop,
        turn,
        river,
    };

    let tree_config = TreeConfig {
        initial_state,
        starting_pot: req.starting_pot,
        effective_stack: req.effective_stack,
        rake_rate: 0.0,
        rake_cap: 0.0,
        flop_bet_sizes: [bet_sizes.clone(), bet_sizes.clone()],
        turn_bet_sizes: [bet_sizes.clone(), bet_sizes.clone()],
        river_bet_sizes: [bet_sizes.clone(), bet_sizes.clone()],
        turn_donk_sizes: None,
        river_donk_sizes: None,
        // Add an all-in line when the largest bet is within 1.5× pot, and force
        // all-in at low SPR — the upstream-recommended defaults so the tree is a
        // sensible, complete equilibrium rather than a truncated one.
        add_allin_threshold: 1.5,
        force_allin_threshold: 0.15,
        merging_threshold: 0.1,
    };

    let action_tree = ActionTree::new(tree_config).map_err(SolveError::Internal)?;
    // F7 (codex MED): a range with zero LIVE combos after the board removes its
    // cards (e.g. an OOP range of only `AsAh` on a board containing the As) is a
    // caller-input error, not an internal fault. The upstream `with_config`
    // rejects it inside `check_card_config` with one of a few specific messages
    // ("OOP/IP range is empty", "Valid card assignment does not exist"). Map
    // THOSE to a clean `InvalidRange` (→ 400) so a fully-board-blocked range is
    // not a 500 (and is never silently solved to a zeroed strategy under the
    // cfr_equilibrium label). Any OTHER build error remains an Internal (500).
    let mut game = match PostFlopGame::with_config(card_config, action_tree) {
        Ok(g) => g,
        Err(msg) => return Err(map_with_config_error(msg)),
    };

    // Defensive backstop: even if a future upstream change stops erroring, an
    // empty private-card list for either player means a fully-blocked range.
    if game.private_cards(0).is_empty() {
        return Err(SolveError::InvalidRange {
            player: "oop".into(),
            detail: "range fully blocked by the board (no live combinations)".into(),
        });
    }
    if game.private_cards(1).is_empty() {
        return Err(SolveError::InvalidRange {
            player: "ip".into(),
            detail: "range fully blocked by the board (no live combinations)".into(),
        });
    }

    // --- MEMORY GATE (the single most important DoS guard) ------------------
    // memory_usage() returns (uncompressed, compressed). We solve uncompressed
    // for speed/accuracy, so gate on the uncompressed estimate BEFORE allocating.
    let (uncompressed, _compressed) = game.memory_usage();
    if uncompressed > req.limits.max_memory_bytes {
        return Err(SolveError::TooLarge {
            estimated: uncompressed,
            cap: req.limits.max_memory_bytes,
        });
    }

    // --- Allocate + solve ---------------------------------------------------
    game.allocate_memory(false);

    let target = req.limits.target_exploitability_pct_of_pot * req.starting_pot as f32;
    let exploitability = solve(&mut game, req.limits.max_iterations, target, false);

    // Required before reading equity/EV/strategy at the (root) node.
    game.cache_normalized_weights();

    project_output(&game, req, exploitability)
}

/// Build the [`SolveOutput`] from a solved game at the root node.
fn project_output(
    game: &PostFlopGame,
    req: &SolveRequest,
    exploitability: f32,
) -> Result<SolveOutput, SolveError> {
    let player = req.solving_player;
    let p = player.index();

    // F1 (codex HIGH): the solver exposes `available_actions()` / `strategy()`
    // for the player to act at the CURRENT (here: root) node only. At the root
    // exactly one player acts first — `current_player()` (0 = OOP, 1 = IP).
    // Requesting the OTHER player's view would index that root strategy
    // (length = root_actions × root_hands) with the wrong player's hand count,
    // producing a misaligned/garbage action mix that `.get().unwrap_or(0.0)`
    // would silently mask. Reject it cleanly rather than emit a wrong "GTO"
    // result (a correctness AND honesty guard); the UI hides the non-root view.
    let root_actor = game.current_player();
    if root_actor != p {
        return Err(SolveError::PlayerNotAtRoot {
            root_actor: match root_actor {
                0 => "oop".into(),
                _ => "ip".into(),
            },
        });
    }

    // Available actions at the root + range-average frequency for each.
    let available = game.available_actions();
    let strategy = game.strategy(); // len = num_actions * num_hands
    let private = game.private_cards(p);
    let num_hands = private.len();
    let num_actions = available.len();
    let weights = game.normalized_weights(p);

    // Range-average frequency of action i = Σ_h strategy[i*H+h] * w[h] / Σ w[h].
    let weight_sum: f32 = weights.iter().sum();
    let mut actions = Vec::with_capacity(num_actions);
    for (i, act) in available.iter().enumerate() {
        let base = i * num_hands;
        let mut acc = 0.0f32;
        if weight_sum > 0.0 {
            for (h, w) in weights.iter().enumerate().take(num_hands) {
                acc += strategy.get(base + h).copied().unwrap_or(0.0) * w;
            }
            acc /= weight_sum;
        }
        let (label, amount) = action_label(*act);
        actions.push(ActionFreq {
            action: label,
            amount,
            frequency: acc,
        });
    }

    // Range-average equity / EV of the solving player.
    let equity_vec = game.equity(p);
    let ev_vec = game.expected_values(p);
    let range_equity = weighted_average(&equity_vec, weights);
    let range_ev = weighted_average(&ev_vec, weights);

    // F3 (codex MED): build the FULL per-hand strategy table once — every live
    // hand of the solving player with its per-action frequencies, equity and EV.
    // This is the hero-INDEPENDENT product of the equilibrium; the optional hero
    // row below is just one entry SELECTED from it. The server caches this table
    // under the hero-independent spot key so a hero-only change re-derives the
    // hero row cheaply (via `hero_strategy`) instead of re-solving. The labels are
    // the solver's canonical hole strings, matching `solver_hole_string`.
    let labels = holes_to_strings(private).map_err(SolveError::Internal)?;
    let mut hands = Vec::with_capacity(num_hands);
    for (idx, label) in labels.iter().enumerate().take(num_hands) {
        let mut freqs = Vec::with_capacity(num_actions);
        for i in 0..num_actions {
            freqs.push(strategy.get(i * num_hands + idx).copied().unwrap_or(0.0));
        }
        hands.push(HandStrategy {
            hand: label.clone(),
            frequencies: freqs,
            equity: equity_vec.get(idx).copied().unwrap_or(0.0),
            ev: ev_vec.get(idx).copied().unwrap_or(0.0),
        });
    }

    // Hero hand (optional) — a pure lookup into the table just built (same result
    // as the previous index-based slice). `None` when blocked by the board or not
    // in the player's range.
    let hero = match req.hero {
        None => None,
        Some(h) => hero_strategy(&hands, h),
    };

    let (uncompressed, _) = game.memory_usage();
    let exploitability_pct_of_pot = if req.starting_pot > 0 {
        exploitability / req.starting_pot as f32 * 100.0
    } else {
        0.0
    };

    Ok(SolveOutput {
        method: SolveMethod::CfrEquilibrium,
        solving_player: player,
        exploitability,
        exploitability_pct_of_pot,
        actions,
        range_equity,
        range_ev,
        hero,
        hands,
        cost: SolveCost {
            memory_bytes: uncompressed,
            // F5: the upstream `solve` does not return the iteration count, so we
            // report the CAP, not the actual count — the field name
            // (`iteration_cap`) and its doc say exactly that, so the disclosure
            // is never misleadingly presented as "iterations actually run".
            iteration_cap: req.limits.max_iterations,
        },
    })
}

/// Map an upstream `PostFlopGame::with_config` error STRING to a `SolveError`.
/// The fully-board-blocked / empty-range cases are caller-input errors the
/// upstream surfaces from `check_card_config`; classify those as `InvalidRange`
/// (→ 400) so they are not 500s. Anything else is a genuine `Internal` (500).
fn map_with_config_error(msg: String) -> SolveError {
    // Upstream messages from `check_card_config` (src/game/base.rs):
    //   "OOP range is empty" / "IP range is empty"  — range empty after parse
    //   "OOP range is invalid ..." / "IP range is invalid ..."
    //   "Valid card assignment does not exist"       — zero live combos (blocked)
    let lower = msg.to_ascii_lowercase();
    let blocked = lower.contains("valid card assignment does not exist");
    if lower.contains("oop range") {
        return SolveError::InvalidRange {
            player: "oop".into(),
            detail: msg,
        };
    }
    if lower.contains("ip range") {
        return SolveError::InvalidRange {
            player: "ip".into(),
            detail: msg,
        };
    }
    if blocked {
        // Can't attribute to one side (every pairing collides) — report against
        // OOP with a descriptive detail.
        return SolveError::InvalidRange {
            player: "oop".into(),
            detail: "range fully blocked by the board (no live card assignment exists)".into(),
        };
    }
    SolveError::Internal(msg)
}

/// Weighted average of `values` by `weights` (0 when total weight is 0).
fn weighted_average(values: &[f32], weights: &[f32]) -> f32 {
    let total: f32 = weights.iter().sum();
    if total <= 0.0 {
        return 0.0;
    }
    let acc: f32 = values.iter().zip(weights.iter()).map(|(v, w)| v * w).sum();
    acc / total
}

/// Map an upstream `Action` to a plain `(label, amount)` pair (no dep type leak).
fn action_label(action: PfAction) -> (String, Option<i32>) {
    match action {
        PfAction::Fold => ("fold".into(), None),
        PfAction::Check => ("check".into(), None),
        PfAction::Call => ("call".into(), None),
        PfAction::Bet(a) => ("bet".into(), Some(a)),
        PfAction::Raise(a) => ("raise".into(), Some(a)),
        PfAction::AllIn(a) => ("all_in".into(), Some(a)),
        // None / Chance never appear in `available_actions()` at a player node;
        // map defensively rather than panicking.
        PfAction::None => ("none".into(), None),
        PfAction::Chance(_) => ("chance".into(), None),
    }
}

/// The solver's canonical hole-card string for an engine [`HoleCards`]:
/// higher card-id first (matches `hole_to_string`). We re-derive it from the
/// card tokens so the lookup against `holes_to_strings(private_cards)` matches
/// exactly regardless of input order.
fn solver_hole_string(h: HoleCards) -> String {
    // Reuse the upstream encoding by parsing each engine card token back to the
    // solver id, then sorting high-first (the solver's hole_to_string order).
    let a = card_from_str(&h.card1.to_string());
    let b = card_from_str(&h.card2.to_string());
    match (a, b) {
        (Ok(x), Ok(y)) => {
            let (hi, lo) = if x >= y { (x, y) } else { (y, x) };
            // card_to_string can't fail for a valid id; fall back to the token.
            let hi_s = postflop_solver::card_to_string(hi).unwrap_or_else(|_| h.card1.to_string());
            let lo_s = postflop_solver::card_to_string(lo).unwrap_or_else(|_| h.card2.to_string());
            format!("{hi_s}{lo_s}")
        }
        // Should be unreachable (engine cards are always valid tokens).
        _ => format!("{}{}", h.card1, h.card2),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use engine::card::{Card, Rank, Suit};

    fn c(rank: Rank, suit: Suit) -> Card {
        Card::new(rank, suit)
    }

    /// A small but real flop solve: tight ranges + ONE bet size → fits well
    /// under the memory cap and converges fast. Asserts the result is a genuine,
    /// internally-consistent equilibrium (frequencies sum ~1, equity in [0,1],
    /// exploitability reported).
    fn small_flop_request() -> SolveRequest {
        SolveRequest {
            street: SolveStreet::Flop,
            // Td9d6h
            flop: [
                c(Rank::Ten, Suit::Diamonds),
                c(Rank::Nine, Suit::Diamonds),
                c(Rank::Six, Suit::Hearts),
            ],
            turn: None,
            river: None,
            // Deliberately narrow ranges to keep the test fast/cheap.
            oop_range: "AA,KK,QQ".into(),
            ip_range: "JJ,TT,99".into(),
            starting_pot: 60,
            effective_stack: 100,
            bet_sizes: "50%".into(),
            raise_sizes: "2.5x".into(),
            hero: None,
            solving_player: Player::Oop,
            limits: SolveLimits::default(),
        }
    }

    #[test]
    fn flop_solve_produces_a_consistent_equilibrium() {
        let out = solve_spot(&small_flop_request()).expect("small flop solve");
        assert_eq!(out.method, SolveMethod::CfrEquilibrium);
        assert_eq!(out.solving_player, Player::Oop);
        assert!(!out.actions.is_empty(), "root has available actions");

        // Frequencies are a probability distribution over the available actions.
        let total: f32 = out.actions.iter().map(|a| a.frequency).sum();
        assert!(
            (total - 1.0).abs() < 0.05,
            "range-average action frequencies should sum to ~1.0, got {total}"
        );
        for a in &out.actions {
            assert!(
                (0.0..=1.0).contains(&a.frequency),
                "freq {} out of range for {}",
                a.frequency,
                a.action
            );
        }

        assert!(
            (0.0..=1.0).contains(&out.range_equity),
            "equity {} out of [0,1]",
            out.range_equity
        );
        assert!(
            out.exploitability >= 0.0,
            "exploitability must be non-negative"
        );
        assert!(out.cost.memory_bytes > 0, "memory estimate reported");
    }

    #[test]
    fn hero_hand_strategy_is_extracted_and_aligned() {
        let mut req = small_flop_request();
        // AA is in the OOP range above; pick a concrete AA combo not on the board.
        req.hero = Some(HoleCards::new(
            c(Rank::Ace, Suit::Spades),
            c(Rank::Ace, Suit::Hearts),
        ));
        let out = solve_spot(&req).expect("solve with hero");
        let hero = out
            .hero
            .expect("AA is in the OOP range → hero strategy present");
        assert_eq!(
            hero.frequencies.len(),
            out.actions.len(),
            "hero per-action frequencies align with the action list"
        );
        let total: f32 = hero.frequencies.iter().sum();
        assert!(
            (total - 1.0).abs() < 0.05,
            "hero strategy should sum to ~1.0, got {total}"
        );
        assert!((0.0..=1.0).contains(&hero.equity));
    }

    #[test]
    fn hero_hand_not_in_range_yields_none() {
        let mut req = small_flop_request();
        // 72o is in neither range → no hero strategy (not an error).
        req.hero = Some(HoleCards::new(
            c(Rank::Seven, Suit::Spades),
            c(Rank::Two, Suit::Hearts),
        ));
        let out = solve_spot(&req).expect("solve still succeeds");
        assert!(out.hero.is_none(), "out-of-range hero → None, not an error");
    }

    // F3 (codex MED): the full per-hand `hands` table is the hero-INDEPENDENT
    // product of the equilibrium, and `hero_strategy` selects one hero row out of
    // it CHEAPLY — yielding exactly what the in-solve hero extraction does. This
    // is what lets the server cache the solve under the hero-independent spot key
    // and serve a different hero from the cached table without re-solving.
    #[test]
    fn hero_strategy_lookup_matches_in_solve_extraction() {
        let mut req = small_flop_request();
        let hero = HoleCards::new(c(Rank::Ace, Suit::Spades), c(Rank::Ace, Suit::Hearts));
        req.hero = Some(hero);
        let out = solve_spot(&req).expect("solve with hero");

        // The table is non-empty and aligned with the action list.
        assert!(
            !out.hands.is_empty(),
            "the per-hand table must list the solving player's live hands"
        );
        for h in &out.hands {
            assert_eq!(
                h.frequencies.len(),
                out.actions.len(),
                "every hand's frequencies align 1:1 with the action list"
            );
        }

        // The in-solve `hero` row is IDENTICAL to a cheap lookup into the table —
        // the derivation the server reuses on a cache hit.
        let in_solve = out.hero.clone().expect("AA is in the OOP range");
        let derived = hero_strategy(&out.hands, hero).expect("AA is present in the table");
        assert_eq!(
            in_solve, derived,
            "the cheap table lookup must reproduce the in-solve hero strategy"
        );

        // Card order on the hero hand is irrelevant to the lookup.
        let derived_swapped = hero_strategy(
            &out.hands,
            HoleCards::new(c(Rank::Ace, Suit::Hearts), c(Rank::Ace, Suit::Spades)),
        )
        .expect("the lookup is order-insensitive on the hero's two cards");
        assert_eq!(in_solve, derived_swapped);

        // A hand absent from the table (72o, in neither range) → None.
        assert!(
            hero_strategy(
                &out.hands,
                HoleCards::new(c(Rank::Seven, Suit::Spades), c(Rank::Two, Suit::Hearts)),
            )
            .is_none(),
            "a hand not in the solving player's range yields None"
        );
    }

    #[test]
    fn memory_cap_rejects_before_allocating() {
        let mut req = small_flop_request();
        // An absurdly low cap (1 KB) must reject even this tiny solve.
        req.limits.max_memory_bytes = 1024;
        match solve_spot(&req) {
            Err(SolveError::TooLarge { estimated, cap }) => {
                assert_eq!(cap, 1024);
                assert!(estimated > cap, "estimate {estimated} should exceed cap");
            }
            other => panic!("expected TooLarge, got {other:?}"),
        }
    }

    #[test]
    fn invalid_range_is_a_clean_error() {
        let mut req = small_flop_request();
        req.oop_range = "not-a-range!!".into();
        match solve_spot(&req) {
            Err(SolveError::InvalidRange { player, .. }) => assert_eq!(player, "oop"),
            other => panic!("expected InvalidRange, got {other:?}"),
        }
    }

    #[test]
    fn invalid_bet_size_is_a_clean_error() {
        let mut req = small_flop_request();
        req.bet_sizes = "purple".into();
        assert!(matches!(
            solve_spot(&req),
            Err(SolveError::InvalidBetSize(_))
        ));
    }

    #[test]
    fn street_board_mismatch_is_rejected() {
        let mut req = small_flop_request();
        req.street = SolveStreet::Turn; // but no turn card supplied
        assert!(matches!(
            solve_spot(&req),
            Err(SolveError::StreetMismatch(_))
        ));
    }

    #[test]
    fn duplicate_turn_card_is_rejected() {
        let mut req = small_flop_request();
        req.street = SolveStreet::Turn;
        // Turn == one of the flop cards (Td) → duplicate.
        req.turn = Some(c(Rank::Ten, Suit::Diamonds));
        assert!(matches!(solve_spot(&req), Err(SolveError::InvalidBoard(_))));
    }

    #[test]
    fn zero_pot_is_rejected() {
        let mut req = small_flop_request();
        req.starting_pot = 0;
        assert!(matches!(
            solve_spot(&req),
            Err(SolveError::InvalidStakes(_))
        ));
    }

    #[test]
    fn ip_view_at_root_is_rejected() {
        // F1 (codex HIGH): on every postflop tree OOP acts first, so the root
        // actor is OOP. Requesting the IP view at the root must be REJECTED
        // (PlayerNotAtRoot) rather than return a misaligned/garbage IP action
        // mix that would be mislabeled a real CFR equilibrium.
        let mut req = small_flop_request();
        req.solving_player = Player::Ip;
        match solve_spot(&req) {
            Err(SolveError::PlayerNotAtRoot { root_actor }) => {
                assert_eq!(root_actor, "oop", "the root actor is OOP postflop");
            }
            other => panic!("expected PlayerNotAtRoot, got {other:?}"),
        }
    }

    #[test]
    fn solve_spot_runs_full_validation_up_front() {
        // N2 (codex MED): `solve_spot` must call `validate_request` FIRST, so an
        // IP-at-root request (or any invalid input) is rejected by the same
        // up-front validation rather than failing LATE in `project_output` after
        // a full allocate + solve. We assert `solve_spot` and `validate_request`
        // return the SAME early error for the IP-at-root case (the documented
        // "all inputs validated before a game is constructed" contract).
        let mut req = small_flop_request();
        req.solving_player = Player::Ip;
        let via_validate = validate_request(&req);
        let via_solve = solve_spot(&req);
        assert!(
            matches!(via_validate, Err(SolveError::PlayerNotAtRoot { .. })),
            "validate_request rejects an IP-at-root request"
        );
        assert!(
            matches!(via_solve, Err(SolveError::PlayerNotAtRoot { .. })),
            "solve_spot rejects an IP-at-root request via up-front validate_request \
             (not late in project_output)"
        );
    }

    #[test]
    fn oop_view_at_root_is_correct() {
        // The mirror of the F1 guard: the OOP (root-actor) view solves cleanly
        // and is internally consistent.
        let out = solve_spot(&small_flop_request()).expect("oop root view solves");
        assert_eq!(out.solving_player, Player::Oop);
        let total: f32 = out.actions.iter().map(|a| a.frequency).sum();
        assert!((total - 1.0).abs() < 0.05, "oop freqs sum ~1, got {total}");
    }

    #[test]
    fn empty_range_is_rejected_as_invalid_range() {
        // F7 (codex MED): the upstream grammar accepts "" as a valid (empty)
        // range. We must reject it as InvalidRange (→ 400), NOT let it surface
        // as a 500-mapped Internal.
        let mut req = small_flop_request();
        req.oop_range = "".into();
        match solve_spot(&req) {
            Err(SolveError::InvalidRange { player, .. }) => assert_eq!(player, "oop"),
            other => panic!("expected InvalidRange for empty oop range, got {other:?}"),
        }
        let mut req2 = small_flop_request();
        req2.ip_range = "   ".into(); // whitespace-only is also empty after parse
        match solve_spot(&req2) {
            Err(SolveError::InvalidRange { player, .. }) => assert_eq!(player, "ip"),
            other => panic!("expected InvalidRange for empty ip range, got {other:?}"),
        }
    }

    #[test]
    fn range_fully_blocked_by_board_is_rejected() {
        // F7 (codex MED): an OOP range of only AhAs on a board containing the As
        // has ZERO live combos after the board removes its cards. It must be a
        // clean InvalidRange (400), not a zeroed-strategy solve under the
        // cfr_equilibrium label.
        let mut req = small_flop_request();
        // Board (from small_flop_request) is Td 9d 6h. Put As on the board and
        // make OOP only AsAh → fully blocked.
        req.flop = [
            c(Rank::Ace, Suit::Spades),
            c(Rank::Nine, Suit::Diamonds),
            c(Rank::Six, Suit::Hearts),
        ];
        req.oop_range = "AsAh".into();
        req.ip_range = "KK".into();
        match solve_spot(&req) {
            Err(SolveError::InvalidRange { player, detail }) => {
                assert_eq!(player, "oop");
                assert!(
                    detail.contains("blocked"),
                    "detail names the cause: {detail}"
                );
            }
            other => panic!("expected InvalidRange (fully blocked), got {other:?}"),
        }
    }

    #[test]
    fn absurd_pot_or_stack_is_rejected() {
        // F8 (codex MED): pot/stack are now upper-bounded (the upstream tree uses
        // i32 amounts throughout). An absurd value must be a clean InvalidStakes.
        let mut req = small_flop_request();
        req.starting_pot = 1_000_000_000;
        assert!(matches!(
            solve_spot(&req),
            Err(SolveError::InvalidStakes(_))
        ));
        let mut req2 = small_flop_request();
        req2.effective_stack = 1_000_000_000;
        assert!(matches!(
            solve_spot(&req2),
            Err(SolveError::InvalidStakes(_))
        ));
    }

    #[test]
    fn cost_reports_the_iteration_cap() {
        // F5 (codex MED): the cost field is the iteration CAP (the upstream solve
        // never returns the actual count). It must equal the configured cap.
        let req = small_flop_request();
        let out = solve_spot(&req).expect("solve");
        assert_eq!(out.cost.iteration_cap, req.limits.max_iterations);
    }

    #[test]
    fn turn_solve_is_cheap_and_consistent() {
        // Turn trees are tiny (the design measured ~7 MB / 0.07 s). Good coverage
        // for the non-flop path + a fast test.
        let mut req = small_flop_request();
        req.street = SolveStreet::Turn;
        req.turn = Some(c(Rank::Two, Suit::Clubs));
        let out = solve_spot(&req).expect("turn solve");
        let total: f32 = out.actions.iter().map(|a| a.frequency).sum();
        assert!((total - 1.0).abs() < 0.05, "turn freqs sum ~1, got {total}");
    }
}
