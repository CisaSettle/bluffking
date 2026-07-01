//! Local solver for the AI Coach (ADR-043).
//!
//! Replaces the Anthropic LLM-backed analyzer with a deterministic, sub-200ms
//! local pipeline:
//!   1. `equity` — Monte Carlo / exact enumeration of hero equity.
//!   2. `preflop_charts` — position-based preflop range lookup at 6max 100bb.
//!      The data is a BluffKing CFR approximate-equilibrium solve over a
//!      SIMPLIFIED tree (`preflop_cfr` + the `gen_preflop_ranges` example) with
//!      all-in-equity + equity-realization terminals — NOT a true postflop GTO
//!      solve, and NEVER labelled "GTO" (see `spot::SolveMethod`).
//!   3. `hand_strength` — categorical hand-strength classifier.
//!   4. `advisor` — applies the locked rule table (§3.4.2) to produce a hint.
//!   5. `templates_zh` — Chinese reasoning sentence library.
//!
//! All public types are our own (no `rs_poker` types leak — ADR-012). All
//! functions are pure and deterministic given the input seed (§6).

pub mod advisor;
pub mod equity;
pub mod hand_strength;
pub mod preflop_cfr;
pub mod preflop_charts;
pub mod range_estimate;
pub mod spot;
pub mod templates_zh;
pub mod tool_math;

pub use advisor::{
    analyze, analyze_chat, CoachHintOut, HandKey, PreflopAction, SolverAction, SolverError,
    SolverInput, SolverVerdict, TableSize,
};
pub use equity::{
    equity, range_legal_combos, EarlyStop, EquityInput, EquityResult, OpponentSpec, RangeBucket,
};
pub use hand_strength::{classify, HandStrength};
pub use preflop_charts::{
    all_hand_keys, combos_for_key, lookup as lookup_preflop, range_entries, ActionBucket,
    ChartCell, PositionBucket,
};
pub use range_estimate::{estimate_range, RangeClass, RangeEstimate};
pub use spot::{
    analyze_replay, analyze_spot, GridAction, GridCell, HeroFacing, PreflopLine, RangeSummary,
    ReasonCode, ReplayRequest, ReplayStreetAnalysis, ReplayStreetInput, SolveMethod, SpotAnalysis,
    SpotRequest,
};
pub use tool_math::{
    detect_draws, outs_to_odds, overcards_to_board_top, pot_odds, scare_card,
    scare_card_from_board, DetectedDraws, DrawKind, OutsOdds, OutsStreet, PotOdds, PotOddsError,
    PotOddsVerdict, ScareCardError, ScareCardOdds, POT_ODDS_MARGIN_PCT,
};

/// Preflop chart data version (ADR-043 §3.2).
///
/// v2 (2026-05-23, audit fix B-2): added `facing_open` action bucket — the
/// previous v1 layout collapsed "RFI / no opens yet" and "facing one open"
/// into the same bucket, which mis-classified hero's spot when an opener
/// was ahead. Bumping this constant invalidates `coach_hints` cache rows
/// per ADR-043 §5.
///
/// v3 (2026-06-25, codex F4): the chart content materially changed — the
/// generator switched to a DCFR approx.-equilibrium solve emitting MIXED
/// frequencies (the JSON `version` field is `3`), and the F2 all-in-symmetry math
/// fix shifted the `vs_4bet` (and force-added `vs_3bet`) ranges. ADR-043 §5
/// REQUIRES bumping `PREFLOP_V` (and the paired `server::coach::PROMPT_V`)
/// whenever the data content changes, so any populated `coach_hints` cache is
/// invalidated and stale prompt_v=2 hints over the OLD chart are never served.
/// `server::main` asserts `PROMPT_V == PREFLOP_V` at startup — keep them in lock
/// step.
///
/// v4 (2026-06-25, ADR-082): the preflop CHART DATA is unchanged — this bump is
/// because the COACH equity ESTIMATOR gained a Wilson-CI Monte-Carlo early-stop
/// on the live-coach path (`equity_vs_random`). On dominant / clear-fold spots
/// the loop now stops once the 95% CI half-width on combined equity drops below
/// 0.5pp, so the coach's `equity_estimate_pct` may differ from the old full-10k
/// value (by ≤ ~1pp, never a verdict flip — see ADR-082 §5(d)). Per ADR-043 §5
/// any estimator change that can move a cached coach number REQUIRES bumping
/// `PREFLOP_V` (and the paired `server::coach::PROMPT_V`) so stale `coach_hints`
/// rows computed under the full-10k estimate miss-and-recompute. The chart JSON
/// `version` field is still `3` — only the coach estimator moved.
pub const PREFLOP_V: u32 = 4;

/// Default Monte Carlo trial count for equity estimation (ADR-043 §3.1).
pub const DEFAULT_MC_TRIALS: u32 = 10_000;

/// Hard cap to defend against malicious input expanding sim cost (T5 §7).
pub const MAX_MC_TRIALS: u32 = 50_000;

/// ADR-082 coach early-stop: minimum trials before the Wilson CI check runs.
pub const EARLY_STOP_MIN_TRIALS: u32 = 1_000;
/// ADR-082 coach early-stop: stop when the Wilson 95% CI half-width < 0.5pp.
pub const EARLY_STOP_HALF_WIDTH: f64 = 0.005;
/// ADR-082 coach early-stop: re-evaluate the CI every K trials after min_trials.
pub const EARLY_STOP_CHECK_EVERY: u32 = 250;

/// Hard cap on `actions_so_far` length (T3 §7).
pub const MAX_ACTIONS_SO_FAR: usize = 200;
