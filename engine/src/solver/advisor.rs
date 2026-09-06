//! Top-level solver entry point — `analyze(SolverInput) -> CoachHintOut`.
//!
//! Implements the rule table from ADR-043 §3.4 (preflop §3.4.1, postflop
//! §3.4.2). All thresholds in this file are LOCKED — see ADR-043 §3.4.2;
//! changing them requires an ADR revision + `PROMPT_V` bump.

use thiserror::Error;

use crate::hand::{BoardCards, HoleCards, Street};
use crate::player::Position;

use super::equity::{equity_vs_random, EquityResult};
use super::hand_strength::{classify, HandStrength};
use super::preflop_charts::{hand_key, lookup, position_to_bucket, ActionBucket};
use super::templates_zh::{render, render_chat_reply, RenderContext};
use super::MAX_ACTIONS_SO_FAR;

/// 6-max vs HU table sizes. v1 only supports 6-max preflop charts; HU
/// uses the same charts with `position_to_bucket` mapping SB→SB, BB→BB.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TableSize {
    /// Heads-up (2 players).
    Hu,
    /// 6-max.
    SixMax,
}

/// Mirror of ADR-029 §4.3 `gto_action` enum.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize)]
#[serde(rename_all = "snake_case")]
pub enum SolverAction {
    Fold,
    Check,
    Call,
    Raise,
    AllIn,
}

impl SolverAction {
    /// Wire string (snake_case, matches `CoachHintData.gto_action`).
    pub fn as_str(self) -> &'static str {
        match self {
            SolverAction::Fold => "fold",
            SolverAction::Check => "check",
            SolverAction::Call => "call",
            SolverAction::Raise => "raise",
            SolverAction::AllIn => "all_in",
        }
    }
}

/// Verdict (mirrors ADR-029 §4.3 `verdict`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize)]
#[serde(rename_all = "snake_case")]
pub enum SolverVerdict {
    Good,
    Ok,
    Mistake,
}

impl SolverVerdict {
    pub fn as_str(self) -> &'static str {
        match self {
            SolverVerdict::Good => "good",
            SolverVerdict::Ok => "ok",
            SolverVerdict::Mistake => "mistake",
        }
    }
}

/// Preflop action context bucket — what kind of preflop spot is the hero in.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PreflopAction {
    /// Folded around to hero, no opens yet.
    Rfi,
    /// Exactly one open ahead of hero (single raise, no 3bet yet) — hero
    /// chooses between fold / call / 3bet. Distinct from `Rfi` (no opens
    /// yet) per ADR-043 §3.2 / preflop_v2.
    FacingOpen,
    /// Hero open-raised, opponent 3-bet, hero facing the 3-bet.
    Vs3bet,
    /// Hero 3-bet, opponent 4-bet, hero facing the 4-bet.
    Vs4bet,
    /// Hero faces one or more limps with no raise yet.
    FacingLimp,
}

impl PreflopAction {
    pub fn to_bucket(self) -> ActionBucket {
        match self {
            PreflopAction::Rfi => ActionBucket::Rfi,
            PreflopAction::FacingOpen => ActionBucket::FacingOpen,
            PreflopAction::Vs3bet => ActionBucket::Vs3bet,
            PreflopAction::Vs4bet => ActionBucket::Vs4bet,
            PreflopAction::FacingLimp => ActionBucket::FacingLimp,
        }
    }
}

/// 169-grid hand key (re-export for callers).
pub use super::preflop_charts::HandKey;

/// Solver input — the projection of `server::coach::PromptContext` that the
/// engine needs. Server is the projection layer (OQ-1 (a)).
#[derive(Debug, Clone)]
pub struct SolverInput {
    pub street: Street,
    pub position: Position,
    pub table_size: TableSize,
    pub hero: HoleCards,
    pub board: BoardCards,
    pub pot_before: u32,
    pub to_call: u32,
    pub stack_before: u32,
    pub num_players_in_hand: u8,
    /// Seat of the hand's current **aggressor** — the player who made the most
    /// recent wager increase (bet / raise) so far on ANY street: the current
    /// street if it has seen aggression, otherwise the latest earlier street
    /// that did (blind posts never count). This is the poker aggressor that
    /// survives street boundaries — the preflop raiser c-betting the flop, the
    /// flop bettor barrelling the turn — and is what ADR-043 §3.4.2's
    /// `is_aggressor` (R3 c-bet / R6 semi-bluff barrel) reads.
    ///
    /// NOT the current-street-only `PersistedDecisionContext.last_aggressor_seat`
    /// (persist.rs). Feeding that here made R3/R6 unreachable — whenever hero
    /// can check nobody has raised THIS street, so `can_check && is_aggressor`
    /// was always false and every checked-to / first-to-act spot came back
    /// `Check` regardless of strength (discovery-r1 BUG-236). Producers:
    /// `GameHand::last_aggressor_player_id` (live session.rs),
    /// `coach::hand_last_aggressor_seat` (persisted re-analysis) and the
    /// self-play acceptance harness `server/examples/gto_selfplay.rs`
    /// (`build_solver_input`) — keep all three in step.
    pub last_aggressor_seat: Option<u8>,
    /// Hero's own seat (for `is_aggressor` comparison).
    pub hero_seat: u8,
    /// What the user did at this seq. `None` means "evaluate the spot
    /// pre-action" (chat-mode pre-eval).
    pub hero_action_taken: Option<SolverAction>,
    /// Preflop action context bucket (only used when `street == Preflop`).
    pub preflop_action: Option<PreflopAction>,
    /// Number of actions so far in this hand (DoS guard, T3 §7).
    pub actions_so_far_count: usize,
    /// Solver RNG seed (server-supplied — `seed_from_hand_seq`).
    pub seed: u64,
}

/// Pure data result (the server maps this to wire-shape `CoachHintData`).
#[derive(Debug, Clone)]
pub struct CoachHintOut {
    pub verdict: SolverVerdict,
    pub gto_action: SolverAction,
    pub equity_estimate_pct: u8,
    pub reasoning_zh: String,
}

/// Solver-side errors (T3, T4 §7). Server maps these to `hint_error`.
#[derive(Debug, Error, PartialEq, Eq)]
pub enum SolverError {
    #[error("invalid cards (duplicates or overlap)")]
    InvalidCards,
    #[error("context too large (actions_so_far > MAX_ACTIONS_SO_FAR)")]
    ContextTooLarge,
    #[error("preflop action context missing")]
    MissingPreflopAction,
}

/// How the reasoning sentence should be rendered.
#[derive(Debug, Clone, Copy)]
enum RenderMode<'a> {
    /// Hint-mode rendering — straight `templates_zh::render`.
    Hint,
    /// Chat-mode rendering — prepend `关于手牌 [hand:UUID]：` via
    /// `templates_zh::render_chat_reply`. ADR-043 §4.3.
    Chat { hand_id: &'a str },
}

/// Equity estimator used by the rule pipeline: `(hero, board, opp_count, seed)`
/// → `EquityResult`. The shipped path is [`equity_vs_random`] (which opts into
/// the ADR-082 coach early-stop); the `#[cfg(test)]` verdict-stability corpus
/// (§5(d)) injects the full-10k `equity_vs_random_full` to prove no verdict /
/// gto_action flips between the two.
type EquityFn = fn(HoleCards, BoardCards, u8, u64) -> EquityResult;

/// Top-level entry — runs the locked rule pipeline.
///
/// Pure + deterministic given `input.seed` (ADR-043 §6). Uses the live-coach
/// equity path ([`equity_vs_random`], ADR-082 early-stop on).
pub fn analyze(input: &SolverInput) -> Result<CoachHintOut, SolverError> {
    analyze_inner(input, RenderMode::Hint, equity_vs_random)
}

/// Chat-mode analyze — identical rule pipeline as [`analyze`], but the
/// returned `reasoning_zh` is wrapped via `templates_zh::render_chat_reply`
/// to include the canonical `关于手牌 [hand:UUID]：` preamble (ADR-043 §4.3).
///
/// Server's chat handler MUST use this entry point and MUST NOT inline the
/// preamble itself — that path was an audit finding (C-3, 2026-05-23).
pub fn analyze_chat(input: &SolverInput, hand_id: &str) -> Result<CoachHintOut, SolverError> {
    analyze_inner(input, RenderMode::Chat { hand_id }, equity_vs_random)
}

/// `#[cfg(test)]`-only: the FULL-10k counterpart of [`analyze`] — identical rule
/// pipeline but the equity is computed with `early_stop: None`. The ADR-082
/// §5(d) corpus asserts this produces the SAME `verdict` + `gto_action` as the
/// shipped (early-stop) [`analyze`] for every spot.
#[cfg(test)]
pub(crate) fn analyze_full(input: &SolverInput) -> Result<CoachHintOut, SolverError> {
    analyze_inner(
        input,
        RenderMode::Hint,
        super::equity::equity_vs_random_full,
    )
}

/// `#[cfg(test)]`-only: full-10k counterpart of [`analyze_chat`] (see [`analyze_full`]).
#[cfg(test)]
pub(crate) fn analyze_chat_full(
    input: &SolverInput,
    hand_id: &str,
) -> Result<CoachHintOut, SolverError> {
    analyze_inner(
        input,
        RenderMode::Chat { hand_id },
        super::equity::equity_vs_random_full,
    )
}

fn analyze_inner(
    input: &SolverInput,
    mode: RenderMode<'_>,
    equity_fn: EquityFn,
) -> Result<CoachHintOut, SolverError> {
    // ---- §7 T3: DoS guard ----
    if input.actions_so_far_count > MAX_ACTIONS_SO_FAR {
        return Err(SolverError::ContextTooLarge);
    }

    // ---- §7 T4: card-validity guard ----
    if !cards_unique(&input.hero, &input.board) {
        return Err(SolverError::InvalidCards);
    }

    if matches!(input.street, Street::Preflop) {
        analyze_preflop(input, mode, equity_fn)
    } else {
        Ok(analyze_postflop(input, mode, equity_fn))
    }
}

fn render_with_mode(ctx: &RenderContext<'_>, seed: u64, mode: RenderMode<'_>) -> String {
    match mode {
        RenderMode::Hint => render(ctx, seed),
        RenderMode::Chat { hand_id } => render_chat_reply(ctx, hand_id, seed),
    }
}

// ---------------------------------------------------------------------------
// Preflop
// ---------------------------------------------------------------------------

fn analyze_preflop(
    input: &SolverInput,
    mode: RenderMode<'_>,
    equity_fn: EquityFn,
) -> Result<CoachHintOut, SolverError> {
    // ADR-043 §3.4.1.
    let pos = position_to_bucket(input.position);
    let bucket = input
        .preflop_action
        .ok_or(SolverError::MissingPreflopAction)?
        .to_bucket();
    let key = hand_key(input.hero);

    let opp_count = input.num_players_in_hand.saturating_sub(1).max(1);
    let eq = equity_fn(input.hero, input.board.clone(), opp_count, input.seed);
    let equity_pct = eq.equity_pct();
    let pot_odds_pct = pot_odds(input.pot_before, input.to_call);

    let hs = classify(input.hero, &input.board);
    let gto_action = recommend_preflop_spot(
        pos,
        bucket,
        &key,
        &PostflopRuleCtx {
            equity_pct,
            pot_odds_pct,
            spr: stack_to_pot_ratio(input.stack_before, input.pot_before),
            hand_strength: hs,
            to_call: input.to_call,
            stack_before: input.stack_before,
            can_check: input.to_call == 0,
            is_aggressor: input.last_aggressor_seat == Some(input.hero_seat),
            street: input.street,
        },
    );

    // F6: respect MIXED-strategy cells. The chart emits genuine mixed frequencies;
    // the headline `gto_action` collapses a cell to one action, but in a mixed cell
    // BOTH fold and the bucket's continue action are equilibrium plays. Grade with
    // `preflop_verdict` so a user who took the other mixed branch is NOT a Mistake.
    let cell_freq = lookup(pos, bucket, &key)
        .map(|c| c.frequency)
        .unwrap_or(0.0);
    let continue_action = preflop_continue_action(bucket);
    let verdict = preflop_verdict(
        input.hero_action_taken,
        gto_action,
        continue_action,
        cell_freq,
        equity_pct,
        pot_odds_pct,
    );

    let reasoning = render_with_mode(
        &RenderContext {
            verdict,
            hs,
            equity_pct,
            pot_odds_pct,
            recommended: gto_action,
            hero_action: input.hero_action_taken,
            hand_label: &key,
            position: input.position.short_label(),
            preflop: true,
        },
        input.seed,
        mode,
    );

    Ok(CoachHintOut {
        verdict,
        gto_action,
        equity_estimate_pct: equity_pct,
        reasoning_zh: reasoning,
    })
}

/// Preflop recommendation from the chart cell (ADR-043 §3.4.1). Extracted so
/// the spot analyzer (M1 §1b) reuses the identical chart-freq rule logic.
pub(crate) fn recommend_preflop(
    pos: super::preflop_charts::PositionBucket,
    bucket: ActionBucket,
    key: &str,
) -> SolverAction {
    match lookup(pos, bucket, key) {
        Some(c) if c.frequency >= 0.5 => preflop_continue_action(bucket),
        // Chart says fold OR chart missing — fold safe.
        Some(_) | None => SolverAction::Fold,
    }
}

/// Preflop recommendation for a concrete SPOT — [`recommend_preflop`] plus the
/// ADR-043 §3.4.1 step-3 safety net.
///
/// The chart's Fold (a missing / low-frequency cell) is a response to a WAGER.
/// When `to_call == 0` there is nothing to fold — the big blind checking its
/// free option in a limped pot (`FacingLimp`), where e.g. 72o/83o are absent
/// from the BB `facing_limp` table. Recommending Fold there told players to
/// surrender a free look and stamped the Phase 11 GTO pill with `fold`
/// (discovery-r1 BUG-235). Per §3.4.1 step 3 such a miss falls through to the
/// postflop rule table with `street = Preflop`, which resolves the free option
/// to Check (R7/R8) — or an iso-raise via R3 when hero is the aggressor.
/// A charted continue (freq ≥ 0.5) is returned unchanged, and a Fold that
/// answers a real wager (`to_call > 0`) stays a Fold: the v3 charts are sparse
/// (absent = fold), so the fall-through is confined to the free-check case.
///
/// The free option is a property of the SPOT SHAPE, not of `to_call` alone:
/// preflop it exists only in the `FacingLimp` bucket (the BB behind limpers
/// has already matched the highest bet). In every other bucket the blinds or
/// an open are an unmatched wager for hero — an RFI seat always owes the BB
/// (`session.rs` / `coach.rs` feed `to_call = current_bet − committed`), so a
/// `to_call == 0` there is a caller-side inconsistency, not a free look, and
/// the chart's Fold vs. trash (72o BTN RFI, rubric P5) must stand. Gate on the
/// bucket so a malformed input can never turn "fold the worst hand" into
/// "check".
pub(crate) fn recommend_preflop_spot(
    pos: super::preflop_charts::PositionBucket,
    bucket: ActionBucket,
    key: &str,
    postflop_ctx: &PostflopRuleCtx,
) -> SolverAction {
    let chart_action = recommend_preflop(pos, bucket, key);
    let free_option = bucket == ActionBucket::FacingLimp && postflop_ctx.to_call == 0;
    if chart_action == SolverAction::Fold && free_option {
        // Never Fold a free option — §3.4.1 step 3 fall-through.
        let fallback = recommend_postflop(postflop_ctx);
        // The postflop table cannot answer a free option with Fold either
        // (R8 catches `to_call == 0`), but pin it so a future rule edit can
        // never re-introduce a Fold here.
        if fallback == SolverAction::Fold {
            SolverAction::Check
        } else {
            fallback
        }
    } else {
        chart_action
    }
}

/// The chart's continue branch: a 5-bet jam against a 4-bet, otherwise a
/// raise. Both the recommendation and mixed-cell verdict use this mapping.
pub(crate) fn preflop_continue_action(bucket: ActionBucket) -> SolverAction {
    match bucket {
        ActionBucket::Vs4bet => SolverAction::AllIn,
        _ => SolverAction::Raise,
    }
}

// ---------------------------------------------------------------------------
// Postflop
// ---------------------------------------------------------------------------

/// Inputs the postflop rule table needs, decoupled from `SolverInput` so the
/// spot analyzer (M1 §1b) can feed equity computed vs a real villain range.
pub(crate) struct PostflopRuleCtx {
    pub equity_pct: u8,
    pub pot_odds_pct: u8,
    pub spr: f32,
    pub hand_strength: HandStrength,
    pub to_call: u32,
    /// Hero's stack before this action. Used to reject a physically impossible
    /// Raise when calling already commits the whole stack (U20).
    pub stack_before: u32,
    pub can_check: bool,
    pub is_aggressor: bool,
    pub street: Street,
}

/// Postflop recommendation (ADR-043 §3.4.2 rule table — first match wins,
/// top-down). Extracted so the spot analyzer reuses the identical thresholds.
pub(crate) fn recommend_postflop(ctx: &PostflopRuleCtx) -> SolverAction {
    let equity_pct = ctx.equity_pct;
    let pot_odds_pct = ctx.pot_odds_pct;
    let spr = ctx.spr;
    let hs = ctx.hand_strength;
    let can_check = ctx.can_check;
    let is_aggressor = ctx.is_aggressor;
    // When calling already commits hero's whole stack, a Raise is physically
    // impossible — the only legal continuing action is the call-all-in. Map a
    // would-be value raise to Call in that case (U20, dual-AI OSS review): there
    // is nothing to raise, hero simply calls off.
    let cannot_raise = ctx.stack_before > 0 && ctx.to_call >= ctx.stack_before;
    let value_aggression = || {
        if cannot_raise {
            SolverAction::Call
        } else {
            SolverAction::Raise
        }
    };

    if equity_pct >= 85 && !can_check {
        // R0: value jam range
        value_aggression()
    } else if equity_pct >= 65 && hs >= HandStrength::TwoPair && spr <= 3.0 && !can_check {
        // R1: commit threshold
        SolverAction::AllIn
    } else if equity_pct >= 65 && !can_check {
        // R2: standard value
        value_aggression()
    } else if equity_pct >= 50 && can_check && is_aggressor {
        // R3: c-bet / barrel
        SolverAction::Raise
    } else if ctx.to_call > 0 && (equity_pct as i32) >= (pot_odds_pct as i32 + 5) {
        // R4: +5 fudge for implied odds — call with positive EV margin
        SolverAction::Call
    } else if hs == HandStrength::DrawStrong
        && ctx.to_call > 0
        && (equity_pct as i32) >= (pot_odds_pct as i32 - 3)
    {
        // R5: semi-bluff defense
        SolverAction::Call
    } else if hs == HandStrength::DrawStrong
        && can_check
        && is_aggressor
        && !matches!(ctx.street, Street::River)
    {
        // R6: semi-bluff barrel
        SolverAction::Raise
    } else if can_check && equity_pct < 35 {
        // R7: pot control / give up
        SolverAction::Check
    } else if ctx.to_call == 0 {
        // R8: default passive
        SolverAction::Check
    } else {
        // R9: fallthrough — negative-EV call
        SolverAction::Fold
    }
}

fn analyze_postflop(
    input: &SolverInput,
    mode: RenderMode<'_>,
    equity_fn: EquityFn,
) -> CoachHintOut {
    let opp_count = input.num_players_in_hand.saturating_sub(1).max(1);
    let eq = equity_fn(input.hero, input.board.clone(), opp_count, input.seed);
    let equity_pct = eq.equity_pct();

    let pot_odds_pct = pot_odds(input.pot_before, input.to_call);
    let spr = stack_to_pot_ratio(input.stack_before, input.pot_before);
    let hs = classify(input.hero, &input.board);
    let can_check = input.to_call == 0;
    let is_aggressor = input.last_aggressor_seat == Some(input.hero_seat);

    // ADR-043 §3.4.2 rule table — first match wins, top-down. The R0–R9 thresholds
    // live in EXACTLY ONE place: `recommend_postflop`. (F9, 2026-06-25: the live
    // coach path and the spot analyzer used to carry byte-identical copies of the
    // table with no parity guard, so they could silently drift. Now both call
    // through this single source of truth.)
    let gto_action = recommend_postflop(&PostflopRuleCtx {
        equity_pct,
        pot_odds_pct,
        spr,
        hand_strength: hs,
        to_call: input.to_call,
        stack_before: input.stack_before,
        can_check,
        is_aggressor,
        street: input.street,
    });

    let verdict = verdict_from_actions(
        input.hero_action_taken,
        gto_action,
        equity_pct,
        pot_odds_pct,
    );

    let reasoning = render_with_mode(
        &RenderContext {
            verdict,
            hs,
            equity_pct,
            pot_odds_pct,
            recommended: gto_action,
            hero_action: input.hero_action_taken,
            hand_label: &hand_key(input.hero),
            position: input.position.short_label(),
            preflop: false,
        },
        input.seed,
        mode,
    );

    CoachHintOut {
        verdict,
        gto_action,
        equity_estimate_pct: equity_pct,
        reasoning_zh: reasoning,
    }
}

// ---------------------------------------------------------------------------
// Verdict mapping (shared preflop + postflop)
// ---------------------------------------------------------------------------

pub(crate) fn verdict_from_actions(
    hero: Option<SolverAction>,
    gto: SolverAction,
    equity_pct: u8,
    pot_odds_pct: u8,
) -> SolverVerdict {
    let hero = match hero {
        Some(a) => a,
        // No hero action (pre-eval / chat mode) — verdict is "Good" by default.
        None => return SolverVerdict::Good,
    };
    if hero == gto {
        return SolverVerdict::Good;
    }
    // Strong-mistake gates (per ADR-043 §3.4.2 verdict table).
    if hero == SolverAction::Fold
        && matches!(
            gto,
            SolverAction::Call | SolverAction::Raise | SolverAction::AllIn
        )
        && (equity_pct as i32) >= (pot_odds_pct as i32)
    {
        return SolverVerdict::Mistake;
    }
    if matches!(
        hero,
        SolverAction::Call | SolverAction::Raise | SolverAction::AllIn
    ) && gto == SolverAction::Fold
        && (equity_pct as i32) < (pot_odds_pct as i32 - 5)
    {
        return SolverVerdict::Mistake;
    }
    // Both aggressive ↔ both aggressive (Raise vs AllIn): Ok.
    SolverVerdict::Ok
}

/// A preflop chart cell is "pure" (single-action) only when its continue
/// frequency is essentially 0 (pure fold — usually the key is simply absent) or
/// essentially 1 (pure continue). Anything strictly in between is a MIXED cell:
/// the equilibrium plays BOTH the continue action AND fold with positive
/// frequency. `PREFLOP_PURE_EPS` matches the grid renderer's 0.99 pure-continue
/// cutoff (`spot::grid_action`) so the headline verdict and the grid agree on
/// what "mixed" means.
pub(crate) const PREFLOP_PURE_EPS: f32 = 0.01;

/// True iff `freq` is a MIXED preflop frequency (strictly between pure-fold and
/// pure-continue). In a mixed cell BOTH fold and the bucket's continue action are
/// part of the equilibrium, so neither may be graded a Mistake.
pub(crate) fn preflop_cell_is_mixed(freq: f32) -> bool {
    freq > PREFLOP_PURE_EPS && freq < 1.0 - PREFLOP_PURE_EPS
}

/// Preflop verdict that respects MIXED-strategy cells (F6).
///
/// The published chart now emits genuine mixed frequencies (e.g. CO vs_4bet
/// AQs ≈ 0.26). The headline recommendation still collapses a cell to a single
/// action (continue iff freq ≥ 0.5), but in a MIXED cell the OTHER branch is ALSO
/// a legitimate equilibrium action — so a user who took it must NOT be graded a
/// Mistake (it would contradict the grid, which paints the same cell `Mix`).
///
/// Rule: in a mixed cell, if hero's action is one of the two mixed branches —
/// FOLD, or the bucket's continue action (`continue_action`, i.e. Raise for
/// RFI/3bet/iso and AllIn (5-bet jam) for vs_4bet, mirroring `recommend_preflop`)
/// — return at
/// worst `Ok` (never `Mistake`); `Good` only when it matches the higher-frequency
/// branch. Any genuinely off-strategy action (e.g. open-jamming a flat-or-fold
/// cell) still falls through to the normal `verdict_from_actions` grading. Pure /
/// absent cells delegate unchanged.
#[allow(clippy::too_many_arguments)]
pub(crate) fn preflop_verdict(
    hero: Option<SolverAction>,
    recommended: SolverAction,
    continue_action: SolverAction,
    freq: f32,
    equity_pct: u8,
    pot_odds_pct: u8,
) -> SolverVerdict {
    let Some(hero) = hero else {
        return SolverVerdict::Good;
    };

    if preflop_cell_is_mixed(freq) {
        // The two equilibrium branches of a mixed preflop cell.
        let hero_is_mix_branch = hero == SolverAction::Fold || hero == continue_action;
        if hero_is_mix_branch {
            // Matching the higher-frequency (headline) branch is Good; the other
            // legitimate branch is Ok — never a Mistake.
            return if hero == recommended {
                SolverVerdict::Good
            } else {
                SolverVerdict::Ok
            };
        }
        // Not one of the mixed branches → grade normally (could be a real error,
        // e.g. jamming when the cell mixes fold/flat).
    }

    verdict_from_actions(Some(hero), recommended, equity_pct, pot_odds_pct)
}

// ---------------------------------------------------------------------------
// Pure helpers
// ---------------------------------------------------------------------------

pub(crate) fn pot_odds(pot_before: u32, to_call: u32) -> u8 {
    if to_call == 0 {
        return 0;
    }
    let denom = (pot_before as u64) + (to_call as u64);
    if denom == 0 {
        return 0;
    }
    (((to_call as u64) * 100 + denom / 2) / denom).min(100) as u8
}

pub(crate) fn stack_to_pot_ratio(stack: u32, pot: u32) -> f32 {
    // T6 §7: never divide by 0.
    let denom = pot.max(1) as f32;
    stack as f32 / denom
}

fn cards_unique(hero: &HoleCards, board: &BoardCards) -> bool {
    let mut all = vec![hero.card1, hero.card2];
    all.extend(board.all_cards());
    for (i, a) in all.iter().enumerate() {
        for b in &all[i + 1..] {
            if a == b {
                return false;
            }
        }
    }
    true
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::card::{Card, Rank, Suit};

    fn c(r: Rank, s: Suit) -> Card {
        Card::new(r, s)
    }

    fn empty_input() -> SolverInput {
        SolverInput {
            street: Street::Flop,
            position: Position::Dealer,
            table_size: TableSize::SixMax,
            hero: HoleCards::new(c(Rank::Ace, Suit::Spades), c(Rank::King, Suit::Spades)),
            board: BoardCards::empty(),
            pot_before: 100,
            to_call: 50,
            stack_before: 1000,
            num_players_in_hand: 2,
            last_aggressor_seat: None,
            hero_seat: 0,
            hero_action_taken: Some(SolverAction::Call),
            preflop_action: None,
            actions_so_far_count: 0,
            seed: 42,
        }
    }

    #[test]
    fn dos_guard_returns_context_too_large() {
        let mut input = empty_input();
        input.actions_so_far_count = MAX_ACTIONS_SO_FAR + 1;
        let result = analyze(&input);
        assert_eq!(result.unwrap_err(), SolverError::ContextTooLarge);
    }

    #[test]
    fn duplicate_hole_cards_returns_invalid() {
        let mut input = empty_input();
        input.hero = HoleCards::new(c(Rank::Ace, Suit::Spades), c(Rank::Ace, Suit::Spades));
        let result = analyze(&input);
        assert_eq!(result.unwrap_err(), SolverError::InvalidCards);
    }

    #[test]
    fn pot_zero_does_not_panic() {
        let mut input = empty_input();
        input.pot_before = 0;
        input.to_call = 0;
        // Must not panic on div-by-zero in SPR calc.
        let _ = analyze(&input).unwrap();
    }

    /// discovery-r1 BUG-235 — the big blind checking its FREE option in a
    /// limped pot (bucket = FacingLimp, 72o absent from the BB `facing_limp`
    /// table). The chart's Fold answers a wager; with `to_call == 0` there is
    /// nothing to fold, so ADR-043 §3.4.1 step 3 falls through to the postflop
    /// rule table → Check. Recommending Fold told players to surrender a free
    /// look and stamped the Phase 11 GTO pill `fold`.
    #[test]
    fn preflop_bb_free_option_never_recommends_fold() {
        let mut input = empty_input();
        input.street = Street::Preflop;
        input.position = Position::BigBlind;
        input.hero = HoleCards::new(c(Rank::Seven, Suit::Spades), c(Rank::Two, Suit::Hearts));
        input.board = BoardCards::empty();
        input.preflop_action = Some(PreflopAction::FacingLimp);
        input.pot_before = 30; // sb 5 + bb 10 + one limp 10 (+ sb complete)
        input.to_call = 0;
        input.last_aggressor_seat = None;
        input.hero_seat = 2;
        input.hero_action_taken = Some(SolverAction::Check);
        assert!(
            lookup(
                super::super::preflop_charts::PositionBucket::BB,
                ActionBucket::FacingLimp,
                "72o"
            )
            .is_none(),
            "precondition: 72o is a chart miss in BB facing_limp"
        );

        let out = analyze(&input).unwrap();
        assert_eq!(
            out.gto_action,
            SolverAction::Check,
            "a free option is never a Fold — §3.4.1 step 3 fall-through"
        );
        assert_eq!(
            out.verdict,
            SolverVerdict::Good,
            "checking the option is the line"
        );
        assert!(
            !out.reasoning_zh.contains("弃牌"),
            "copy must not advise folding a free check: {}",
            out.reasoning_zh
        );

        // Same hand, same bucket, but the chart's Fold answers a REAL wager
        // (a limp-raise / any to_call > 0): stays Fold — the fall-through is
        // confined to the free-check case (v3 charts are sparse: absent = fold).
        input.to_call = 20;
        input.hero_action_taken = Some(SolverAction::Fold);
        let facing_wager = analyze(&input).unwrap();
        assert_eq!(facing_wager.gto_action, SolverAction::Fold);
    }

    /// BUG-235 guard (rubric P5 regression, L1 r1-post-codex-a1) — the
    /// free-option safety net is confined to the `FacingLimp` bucket. An RFI
    /// seat always owes the big blind, so `Rfi` + `to_call == 0` is a malformed
    /// input, not a free look: the chart's Fold vs. trash (72o BTN) must stand
    /// and never resolve to Check.
    #[test]
    fn preflop_rfi_trash_stays_fold_even_with_zero_to_call() {
        let mut input = empty_input();
        input.street = Street::Preflop;
        input.position = Position::Dealer;
        input.table_size = TableSize::SixMax;
        input.hero = HoleCards::new(c(Rank::Seven, Suit::Hearts), c(Rank::Two, Suit::Spades));
        input.board = BoardCards::empty();
        input.preflop_action = Some(PreflopAction::Rfi);
        input.pot_before = 150;
        input.to_call = 0;
        input.stack_before = 10_000;
        input.num_players_in_hand = 6;
        input.last_aggressor_seat = None;
        input.hero_seat = 0;
        input.hero_action_taken = Some(SolverAction::Fold);
        let out = analyze(&input).unwrap();
        assert_eq!(out.gto_action, SolverAction::Fold, "72o BTN RFI is a fold");
        assert_eq!(out.verdict, SolverVerdict::Good);

        // The genuine live shape (BTN owes the BB) is unchanged too.
        input.to_call = 100;
        assert_eq!(analyze(&input).unwrap().gto_action, SolverAction::Fold);
    }

    /// BUG-235 companion — a charted iso-raise (freq ≥ 0.5) on the BB option
    /// is untouched by the free-option safety net.
    #[test]
    fn preflop_bb_free_option_keeps_a_charted_iso_raise() {
        let key = "AA";
        let cell = lookup(
            super::super::preflop_charts::PositionBucket::BB,
            ActionBucket::FacingLimp,
            key,
        )
        .expect("AA is charted in BB facing_limp");
        assert!(cell.frequency >= 0.5);
        let mut input = empty_input();
        input.street = Street::Preflop;
        input.position = Position::BigBlind;
        input.hero = HoleCards::new(c(Rank::Ace, Suit::Spades), c(Rank::Ace, Suit::Hearts));
        input.board = BoardCards::empty();
        input.preflop_action = Some(PreflopAction::FacingLimp);
        input.pot_before = 30;
        input.to_call = 0;
        let out = analyze(&input).unwrap();
        assert_eq!(out.gto_action, SolverAction::Raise);
    }

    /// discovery-r1 BUG-236 — with `last_aggressor_seat` carrying the hand-level
    /// aggressor (the preflop raiser), a checked-to c-bet spot with a strong
    /// hand reaches R3 (`eq >= 50 && can_check && is_aggressor` → Raise) instead
    /// of collapsing to R8's default Check; the same spot for the NON-aggressor
    /// (checked to, no initiative) stays Check.
    #[test]
    fn postflop_checked_to_aggressor_with_strong_hand_is_a_cbet() {
        let mut input = empty_input();
        input.street = Street::Flop;
        input.position = Position::Dealer;
        input.hero = HoleCards::new(c(Rank::Ace, Suit::Spades), c(Rank::Ace, Suit::Hearts));
        input.board = BoardCards {
            flop: Some([
                c(Rank::King, Suit::Diamonds),
                c(Rank::Seven, Suit::Clubs),
                c(Rank::Two, Suit::Spades),
            ]),
            turn: None,
            river: None,
        };
        input.pot_before = 130;
        input.to_call = 0; // BB checked to hero
        input.stack_before = 940;
        input.num_players_in_hand = 2;
        input.hero_seat = 0;
        input.hero_action_taken = Some(SolverAction::Raise);

        // Hero was the preflop raiser: the hand-level aggressor.
        input.last_aggressor_seat = Some(0);
        let aggressor = analyze(&input).unwrap();
        assert!(aggressor.equity_estimate_pct >= 50);
        assert_eq!(
            aggressor.gto_action,
            SolverAction::Raise,
            "R3 c-bet must be reachable on the live path"
        );
        assert_eq!(aggressor.verdict, SolverVerdict::Good);

        // Hero was NOT the aggressor (villain 3-bet, hero called): checked to,
        // the table keeps Check (R8) — no initiative to c-bet with.
        input.last_aggressor_seat = Some(1);
        let caller = analyze(&input).unwrap();
        assert_eq!(caller.gto_action, SolverAction::Check);
        // A limped pot with no aggressor anywhere: also Check.
        input.last_aggressor_seat = None;
        assert_eq!(analyze(&input).unwrap().gto_action, SolverAction::Check);
    }

    #[test]
    fn preflop_aa_in_utg_is_raise() {
        let mut input = empty_input();
        input.street = Street::Preflop;
        input.position = Position::Utg;
        input.hero = HoleCards::new(c(Rank::Ace, Suit::Spades), c(Rank::Ace, Suit::Hearts));
        input.board = BoardCards::empty();
        input.preflop_action = Some(PreflopAction::Rfi);
        input.to_call = 0;
        let result = analyze(&input).unwrap();
        assert_eq!(result.gto_action, SolverAction::Raise);
    }

    /// F2 (2026-06-25, supersedes the pre-v3 `preflop_facing_open_call_only_*`):
    /// in the v3 CFR model `facing_open` IS a 3-BET bucket (`pot_model` continue =
    /// 3-bet, `hero_aggressor = true`), so a `facing_open` cell with freq≥0.5 is a
    /// RAISE — exactly what the grid paints — NOT a flat-call, even for hands that
    /// are ABSENT from `vs_3bet`. BB's T9s has facing_open=1.0 but vs_3bet=None in
    /// `preflop_v2.json`; the old union model wrongly returned Call for it, so the
    /// user saw Raise on the grid but Call in the headline. It must now be Raise.
    #[test]
    fn preflop_facing_open_continue_means_raise_in_v3() {
        let mut input = empty_input();
        input.street = Street::Preflop;
        input.position = Position::BigBlind;
        input.hero = HoleCards::new(c(Rank::Ten, Suit::Spades), c(Rank::Nine, Suit::Spades));
        input.board = BoardCards::empty();
        input.preflop_action = Some(PreflopAction::FacingOpen);
        input.to_call = 30;
        let result = analyze(&input).unwrap();
        assert_eq!(
            result.gto_action,
            SolverAction::Raise,
            "v3: T9s facing_open=1.0 (vs_3bet absent) must be Raise to match the grid"
        );
    }

    /// Regression companion: a `facing_open` hand that IS in the value-3bet
    /// range (`vs_3bet`) must still be advised Raise. AA is a 3-bet everywhere.
    #[test]
    fn preflop_facing_open_value_hand_is_raise() {
        let mut input = empty_input();
        input.street = Street::Preflop;
        input.position = Position::BigBlind;
        input.hero = HoleCards::new(c(Rank::Ace, Suit::Spades), c(Rank::Ace, Suit::Hearts));
        input.board = BoardCards::empty();
        input.preflop_action = Some(PreflopAction::FacingOpen);
        input.to_call = 30;
        let result = analyze(&input).unwrap();
        assert_eq!(
            result.gto_action,
            SolverAction::Raise,
            "AA facing a single open is a value 3-bet"
        );
    }

    #[test]
    fn preflop_72o_in_utg_is_fold() {
        let mut input = empty_input();
        input.street = Street::Preflop;
        input.position = Position::Utg;
        input.hero = HoleCards::new(c(Rank::Seven, Suit::Spades), c(Rank::Two, Suit::Hearts));
        input.board = BoardCards::empty();
        input.preflop_action = Some(PreflopAction::Rfi);
        let result = analyze(&input).unwrap();
        assert_eq!(result.gto_action, SolverAction::Fold);
    }

    // -----------------------------------------------------------------------
    // ADR-082 §5(d) — VERDICT-STABILITY CORPUS
    //
    // The advisor thresholds (R0–R9, the R4 +5 / R5 -3 fudge, the verdict gates
    // at `verdict_from_actions`, and the preflop mixed-cell `preflop_verdict`)
    // flip on integer-pp boundaries, so a 1pp equity wobble could flip a verdict.
    // The hard safety gate is therefore VERDICT IDENTITY (not equity closeness):
    // no corpus spot may flip `verdict` or `gto_action` between the shipped
    // early-stop path (`analyze` / `analyze_chat`) and a full-10k variant
    // (`analyze_full` / `analyze_chat_full`). Equity may differ by <= 1pp
    // (secondary check). No reusable SolverInput corpus exists (seed_drills seeds
    // DB drills, not SolverInputs), so this is a fixed INLINE corpus.
    // -----------------------------------------------------------------------

    fn board3(a: Card, b: Card, d: Card) -> BoardCards {
        BoardCards {
            flop: Some([a, b, d]),
            turn: None,
            river: None,
        }
    }

    fn hole(r1: Rank, s1: Suit, r2: Rank, s2: Suit) -> HoleCards {
        HoleCards::new(c(r1, s1), c(r2, s2))
    }

    /// Fixed corpus of `(label, SolverInput)` spots spanning the preflop buckets
    /// and the postflop R0–R9 boundaries called out in §5(d). Each base spot is
    /// duplicated across a couple of seeds + player counts by the test harness.
    fn corpus() -> Vec<(&'static str, SolverInput)> {
        use PreflopAction::*;
        use Street::*;

        // Builders keep the noise down; every field the rule table reads is set.
        let pre = |label: &'static str,
                   pos: Position,
                   action: PreflopAction,
                   hero: HoleCards,
                   players: u8,
                   to_call: u32,
                   pot: u32,
                   taken: SolverAction|
         -> (&'static str, SolverInput) {
            (
                label,
                SolverInput {
                    street: Preflop,
                    position: pos,
                    table_size: TableSize::SixMax,
                    hero,
                    board: BoardCards::empty(),
                    pot_before: pot,
                    to_call,
                    stack_before: 1000,
                    num_players_in_hand: players,
                    last_aggressor_seat: None,
                    hero_seat: 0,
                    hero_action_taken: Some(taken),
                    preflop_action: Some(action),
                    actions_so_far_count: 4,
                    seed: 1,
                },
            )
        };
        let post = |label: &'static str,
                    pos: Position,
                    hero: HoleCards,
                    board: BoardCards,
                    street: Street,
                    players: u8,
                    to_call: u32,
                    pot: u32,
                    stack: u32,
                    aggressor: Option<u8>,
                    taken: SolverAction|
         -> (&'static str, SolverInput) {
            (
                label,
                SolverInput {
                    street,
                    position: pos,
                    table_size: TableSize::SixMax,
                    hero,
                    board,
                    pot_before: pot,
                    to_call,
                    stack_before: stack,
                    num_players_in_hand: players,
                    last_aggressor_seat: aggressor,
                    hero_seat: 0,
                    hero_action_taken: Some(taken),
                    preflop_action: None,
                    actions_so_far_count: 8,
                    seed: 1,
                },
            )
        };

        vec![
            // ---- Preflop ----
            // RFI premium (pure-continue).
            pre(
                "pre RFI AA premium",
                Position::Utg,
                Rfi,
                hole(Rank::Ace, Suit::Spades, Rank::Ace, Suit::Hearts),
                6,
                0,
                3,
                SolverAction::Raise,
            ),
            pre(
                "pre RFI KK premium",
                Position::Cutoff,
                Rfi,
                hole(Rank::King, Suit::Spades, Rank::King, Suit::Hearts),
                6,
                0,
                3,
                SolverAction::Raise,
            ),
            // RFI trash (pure-fold).
            pre(
                "pre RFI 72o trash",
                Position::Utg,
                Rfi,
                hole(Rank::Seven, Suit::Spades, Rank::Two, Suit::Hearts),
                6,
                0,
                3,
                SolverAction::Fold,
            ),
            // Mixed cell — CO vs_4bet AQs (≈0.26): exercises preflop_verdict both
            // branches (hero takes the continue branch = AllIn 5-bet jam).
            pre(
                "pre CO vs4bet AQs mixed (jam)",
                Position::Cutoff,
                Vs4bet,
                hole(Rank::Ace, Suit::Spades, Rank::Queen, Suit::Spades),
                2,
                90,
                120,
                SolverAction::AllIn,
            ),
            // Same mixed cell, hero takes the OTHER branch (fold).
            pre(
                "pre CO vs4bet AQs mixed (fold)",
                Position::Cutoff,
                Vs4bet,
                hole(Rank::Ace, Suit::Spades, Rank::Queen, Suit::Spades),
                2,
                90,
                120,
                SolverAction::Fold,
            ),
            // FacingOpen 3-bet candidate.
            pre(
                "pre BB facing_open AQs",
                Position::BigBlind,
                FacingOpen,
                hole(Rank::Ace, Suit::Hearts, Rank::Queen, Suit::Hearts),
                2,
                30,
                45,
                SolverAction::Raise,
            ),
            // Vs3bet spot.
            pre(
                "pre BTN vs3bet AKs",
                Position::Dealer,
                Vs3bet,
                hole(Rank::Ace, Suit::Spades, Rank::King, Suit::Spades),
                2,
                60,
                90,
                SolverAction::Raise,
            ),
            // Vs4bet premium (pure jam).
            pre(
                "pre BTN vs4bet AA jam",
                Position::Dealer,
                Vs4bet,
                hole(Rank::Ace, Suit::Spades, Rank::Ace, Suit::Hearts),
                2,
                90,
                120,
                SolverAction::AllIn,
            ),
            // ---- Postflop ----
            // R0/R1 dominant made hand (equity >= 85) facing a bet.
            post(
                "post dominant set facing bet",
                Position::Dealer,
                hole(Rank::King, Suit::Spades, Rank::King, Suit::Hearts),
                board3(
                    c(Rank::King, Suit::Diamonds),
                    c(Rank::Seven, Suit::Clubs),
                    c(Rank::Two, Suit::Spades),
                ),
                Flop,
                2,
                40,
                100,
                160,
                Some(1),
                SolverAction::Raise,
            ),
            // R2 clear value (65–84) facing a bet.
            post(
                "post top-two value facing bet",
                Position::Dealer,
                hole(Rank::Ace, Suit::Spades, Rank::King, Suit::Spades),
                board3(
                    c(Rank::Ace, Suit::Hearts),
                    c(Rank::King, Suit::Diamonds),
                    c(Rank::Two, Suit::Clubs),
                ),
                Flop,
                2,
                40,
                100,
                500,
                Some(1),
                SolverAction::Raise,
            ),
            // R4 marginal call exercising the +5 implied-odds fudge. Verified
            // in-worktree (full-10k): KJo on Q-7-2 rainbow vs Random(1) measures
            // eq≈47-49% (hs=PureBluffNoEquity, so R5 cannot shadow), to_call=50 /
            // pot=100 ⇒ pot_odds=33, so R4's `eq >= pot_odds+5` band [38,65) is the
            // FIRST matching branch ⇒ Call. (The previous row here — AhJd top pair
            // on As8c3d — measured eq≈87% and actually entered R0 as a value Raise,
            // never R4; F1, 2026-06-25.) `num_players_in_hand=2` is FIXED for this
            // row: the branch precondition is opponent-count-sensitive, so the
            // dedicated near-threshold test below pins it; the broad corpus harness
            // re-spreads {2,3,6} only for verdict identity, not branch entry.
            post(
                "post marginal call near pot-odds (R4 +5 fudge)",
                Position::BigBlind,
                hole(Rank::King, Suit::Clubs, Rank::Jack, Suit::Diamonds),
                board3(
                    c(Rank::Queen, Suit::Spades),
                    c(Rank::Seven, Suit::Hearts),
                    c(Rank::Two, Suit::Clubs),
                ),
                Flop,
                2,
                50,
                100,
                400,
                Some(1),
                SolverAction::Call,
            ),
            // R5 strong-draw semi-bluff defense at the -3 boundary. Verified
            // in-worktree (full-10k): AhKh nut-flush draw on Qh-7h-2c vs Random(3)
            // measures eq≈52-53% (hs=DrawStrong), to_call=100 / pot=100 ⇒
            // pot_odds=50, so R4's `eq >= pot_odds+5` (≥55) is SKIPPED and R5's
            // `DrawStrong && eq >= pot_odds-3` (≥47) is the first match ⇒ Call.
            // (The previous row here — AhKh vs Random(1) — measured eq≈72% and
            // actually entered R2 as a value Raise, never R5; F1, 2026-06-25.)
            // `num_players_in_hand=4` (opp_count=3) is FIXED for the same reason as
            // the R4 row above.
            post(
                "post strong flush draw -3 boundary (R5)",
                Position::BigBlind,
                hole(Rank::Ace, Suit::Hearts, Rank::King, Suit::Hearts),
                board3(
                    c(Rank::Queen, Suit::Hearts),
                    c(Rank::Seven, Suit::Hearts),
                    c(Rank::Two, Suit::Clubs),
                ),
                Flop,
                4,
                100,
                100,
                400,
                Some(1),
                SolverAction::Call,
            ),
            // R7 pot-control / give-up (equity < 35, can check).
            post(
                "post pot-control give-up",
                Position::BigBlind,
                hole(Rank::Seven, Suit::Diamonds, Rank::Six, Suit::Clubs),
                board3(
                    c(Rank::Ace, Suit::Spades),
                    c(Rank::King, Suit::Hearts),
                    c(Rank::Nine, Suit::Diamonds),
                ),
                Flop,
                2,
                0,
                60,
                400,
                None,
                SolverAction::Check,
            ),
            // R9 clear fold (equity < pot_odds-5) — exercise the Mistake gate.
            post(
                "post clear fold negative-EV",
                Position::BigBlind,
                hole(Rank::Seven, Suit::Diamonds, Rank::Two, Suit::Clubs),
                BoardCards {
                    flop: Some([
                        c(Rank::Ace, Suit::Spades),
                        c(Rank::King, Suit::Hearts),
                        c(Rank::Nine, Suit::Diamonds),
                    ]),
                    turn: Some(c(Rank::Queen, Suit::Clubs)),
                    river: Some(c(Rank::Five, Suit::Hearts)),
                },
                River,
                3,
                80,
                100,
                400,
                Some(1),
                SolverAction::Fold,
            ),
        ]
    }

    /// §5(d): for EVERY corpus entry, across a spread of seeds + player counts,
    /// `analyze` (shipped early-stop) and `analyze_full` (full-10k) MUST return
    /// IDENTICAL `verdict` and `gto_action`. Equity may differ by <= 1pp.
    #[test]
    fn coach_verdict_identical_early_stop_vs_full() {
        let seeds: [u64; 2] = [1, 9_999];
        for (label, base) in corpus() {
            // Spread num_players ∈ {2, 3, 6} where it doesn't change the spot's
            // meaning (preflop premiums/trash are position-bucketed; postflop
            // opp_count drives equity). Keep the base players value but also test
            // the two other counts.
            let player_counts = [base.num_players_in_hand, 3, 6];
            for &players in player_counts.iter() {
                for &seed in seeds.iter() {
                    let mut input = base.clone();
                    input.num_players_in_hand = players.max(2);
                    input.seed = seed;

                    let es = analyze(&input).unwrap();
                    let full = analyze_full(&input).unwrap();

                    assert_eq!(
                        es.gto_action, full.gto_action,
                        "[{label}] players={players} seed={seed}: gto_action FLIPPED early-stop={:?} full={:?}",
                        es.gto_action, full.gto_action
                    );
                    assert_eq!(
                        es.verdict, full.verdict,
                        "[{label}] players={players} seed={seed}: verdict FLIPPED early-stop={:?} full={:?} (eq es={} full={})",
                        es.verdict, full.verdict, es.equity_estimate_pct, full.equity_estimate_pct
                    );
                    let dpp =
                        (es.equity_estimate_pct as i32 - full.equity_estimate_pct as i32).abs();
                    assert!(
                        dpp <= 1,
                        "[{label}] players={players} seed={seed}: equity drifted > 1pp (es={} full={})",
                        es.equity_estimate_pct,
                        full.equity_estimate_pct
                    );
                }
            }
        }
    }

    /// §5(d): chat mode shares the pipeline — verify verdict/gto_action identity
    /// for at least one preflop + one postflop entry in chat mode too.
    #[test]
    fn coach_chat_verdict_identical_early_stop_vs_full() {
        let hand_id = "00000000-0000-0000-0000-000000000001";
        let picks: Vec<(&str, SolverInput)> = corpus()
            .into_iter()
            .filter(|(l, _)| {
                *l == "pre BB facing_open AQs" || *l == "post top-two value facing bet"
            })
            .collect();
        assert_eq!(picks.len(), 2, "expected one preflop + one postflop pick");
        for (label, mut input) in picks {
            for &seed in &[1u64, 4242] {
                input.seed = seed;
                let es = analyze_chat(&input, hand_id).unwrap();
                let full = analyze_chat_full(&input, hand_id).unwrap();
                assert_eq!(
                    es.gto_action, full.gto_action,
                    "[chat {label}] seed={seed}: gto_action flipped"
                );
                assert_eq!(
                    es.verdict, full.verdict,
                    "[chat {label}] seed={seed}: verdict flipped"
                );
            }
        }
    }

    /// F1 (2026-06-25): the verdict-stability corpus's R4 / R5 rows were
    /// MIS-LABELLED — the old "marginal call near pot-odds" (AhJd top pair on
    /// As8c3d, eq≈87%) entered R0 as a value Raise, and the old "strong flush
    /// draw -3 boundary" (AhKh vs Random(1), eq≈72%) entered R2 as a value
    /// Raise. Neither ever exercised the R4 `+5` implied-odds fudge or the R5
    /// `-3` semi-bluff band, so the boundaries §5(d) claims to cover had ZERO
    /// coverage: a future estimator change could flip a verdict at one of those
    /// gates with no corpus row noticing.
    ///
    /// This dedicated test pins both branches at a FIXED opponent count
    /// (precondition entry IS opp-count-sensitive, so it must not be re-spread):
    ///   1. asserts the branch PRECONDITION against the full-10k equity, i.e.
    ///      `recommend_postflop` reaches R4 / R5 specifically (not a higher rule
    ///      that shadows it) and yields `Call`; and
    ///   2. asserts early-stop-vs-full `gto_action` + `verdict` identity across a
    ///      seed spread, so the no-flip guarantee actually holds AT those gates.
    #[test]
    fn near_threshold_rows_enter_r4_r5_and_do_not_flip() {
        use crate::solver::equity::equity_vs_random_full;

        // (label, base spot) — opponent count is FIXED inside the base spot.
        let r4 = corpus()
            .into_iter()
            .find(|(l, _)| *l == "post marginal call near pot-odds (R4 +5 fudge)")
            .expect("R4 corpus row present")
            .1;
        let r5 = corpus()
            .into_iter()
            .find(|(l, _)| *l == "post strong flush draw -3 boundary (R5)")
            .expect("R5 corpus row present")
            .1;

        // --- Part 1: branch PRECONDITION against full-10k equity. ---
        // Re-derive the rule-table inputs exactly as `analyze_postflop` does, then
        // assert which branch `recommend_postflop` enters, so the label is proven.
        let branch_ctx = |input: &SolverInput| -> PostflopRuleCtx {
            let opp_count = input.num_players_in_hand.saturating_sub(1).max(1);
            let eq = equity_vs_random_full(input.hero, input.board.clone(), opp_count, input.seed);
            PostflopRuleCtx {
                equity_pct: eq.equity_pct(),
                pot_odds_pct: pot_odds(input.pot_before, input.to_call),
                spr: stack_to_pot_ratio(input.stack_before, input.pot_before),
                hand_strength: classify(input.hero, &input.board),
                to_call: input.to_call,
                stack_before: input.stack_before,
                can_check: input.to_call == 0,
                is_aggressor: input.last_aggressor_seat == Some(input.hero_seat),
                street: input.street,
            }
        };

        // R4: the +5 implied-odds band. PureBluffNoEquity so R5 cannot shadow,
        // and eq must sit in [pot_odds+5, 65) with `!can_check` so R0..R3 are all
        // skipped — the FIRST matching rule is R4.
        let r4_ctx = branch_ctx(&r4);
        assert_eq!(
            r4_ctx.hand_strength,
            HandStrength::PureBluffNoEquity,
            "R4 row must NOT be DrawStrong (else R5 could shadow R4): hs={:?}",
            r4_ctx.hand_strength
        );
        assert!(
            !r4_ctx.can_check && r4_ctx.to_call > 0,
            "R4 row must be facing a bet"
        );
        assert!(
            r4_ctx.equity_pct < 65,
            "R4 row eq must be < 65 (else R1/R2 shadow it): eq={}",
            r4_ctx.equity_pct
        );
        assert!(
            (r4_ctx.equity_pct as i32) >= (r4_ctx.pot_odds_pct as i32 + 5),
            "R4 precondition `eq >= pot_odds+5` must hold: eq={} pot_odds={}",
            r4_ctx.equity_pct,
            r4_ctx.pot_odds_pct
        );
        assert_eq!(
            recommend_postflop(&r4_ctx),
            SolverAction::Call,
            "R4 row must resolve to Call via the +5 fudge"
        );

        // R5: the -3 semi-bluff band. DrawStrong, eq below pot_odds+5 (so R4 is
        // SKIPPED) but >= pot_odds-3 (so R5 is the first match), `!can_check`.
        let r5_ctx = branch_ctx(&r5);
        assert_eq!(
            r5_ctx.hand_strength,
            HandStrength::DrawStrong,
            "R5 row must be DrawStrong: hs={:?}",
            r5_ctx.hand_strength
        );
        assert!(
            !r5_ctx.can_check && r5_ctx.to_call > 0,
            "R5 row must be facing a bet"
        );
        assert!(
            r5_ctx.equity_pct < 65,
            "R5 row eq must be < 65 (else R1/R2 shadow it): eq={}",
            r5_ctx.equity_pct
        );
        assert!(
            (r5_ctx.equity_pct as i32) < (r5_ctx.pot_odds_pct as i32 + 5),
            "R5 row must SKIP R4 (`eq < pot_odds+5`): eq={} pot_odds={}",
            r5_ctx.equity_pct,
            r5_ctx.pot_odds_pct
        );
        assert!(
            (r5_ctx.equity_pct as i32) >= (r5_ctx.pot_odds_pct as i32 - 3),
            "R5 precondition `eq >= pot_odds-3` must hold: eq={} pot_odds={}",
            r5_ctx.equity_pct,
            r5_ctx.pot_odds_pct
        );
        assert_eq!(
            recommend_postflop(&r5_ctx),
            SolverAction::Call,
            "R5 row must resolve to Call via the semi-bluff defense band"
        );

        // --- Part 2: early-stop vs full-10k must NOT flip verdict/gto_action ---
        // at these gates, across a seed spread, at the FIXED opponent count.
        for (label, base) in [("R4", &r4), ("R5", &r5)] {
            for seed in [1u64, 7, 42, 4242, 9_999] {
                let mut input = base.clone();
                input.seed = seed;
                let es = analyze(&input).unwrap();
                let full = analyze_full(&input).unwrap();
                assert_eq!(
                    es.gto_action, full.gto_action,
                    "[{label}] seed={seed}: gto_action FLIPPED early-stop={:?} full={:?} (eq es={} full={})",
                    es.gto_action, full.gto_action, es.equity_estimate_pct, full.equity_estimate_pct
                );
                assert_eq!(
                    es.verdict, full.verdict,
                    "[{label}] seed={seed}: verdict FLIPPED early-stop={:?} full={:?} (eq es={} full={})",
                    es.verdict, full.verdict, es.equity_estimate_pct, full.equity_estimate_pct
                );
            }
        }
    }

    /// BUG-230 (discovery r1): `classify` is documented (ADR-043 §3.3) to return a
    /// COARSE pocket-pair hint preflop — PairWeak for every unpaired hand,
    /// PairMiddle for 77–TT, Overpair for JJ+ — "solely as a hint to advisor for
    /// templating". The zh templates rendered that hint through the POSTFLOP band
    /// label table, so the delivered preflop coach text called AKs / AQo a 弱对
    /// (weak pair), 88 a 中对 (middle pair) and JJ+ a 超对 (overpair), and spoke of
    /// 这块面 (this board) with zero board cards. Preflop copy must describe the
    /// STARTING HAND, never a postflop made-hand band.
    #[test]
    fn preflop_reasoning_never_uses_postflop_made_hand_bands() {
        const POSTFLOP_ONLY: [&str; 11] = [
            "弱对",
            "中对",
            "超对",
            "顶对",
            "两对",
            "暗三条",
            "顺子",
            "葫芦",
            "听牌",
            "这块面",
            "无对无听",
        ];
        let hands: [(Card, Card, &str); 5] = [
            (
                c(Rank::Ace, Suit::Spades),
                c(Rank::King, Suit::Spades),
                "AKs",
            ),
            (
                c(Rank::Ace, Suit::Spades),
                c(Rank::Queen, Suit::Hearts),
                "AQo",
            ),
            (
                c(Rank::Jack, Suit::Spades),
                c(Rank::Jack, Suit::Hearts),
                "JJ",
            ),
            (
                c(Rank::Eight, Suit::Spades),
                c(Rank::Eight, Suit::Hearts),
                "88",
            ),
            (
                c(Rank::Five, Suit::Clubs),
                c(Rank::Five, Suit::Diamonds),
                "55",
            ),
        ];
        let heros = [
            Some(SolverAction::Raise),
            Some(SolverAction::Call),
            Some(SolverAction::Fold),
            Some(SolverAction::AllIn),
            None,
        ];
        for (c1, c2, label) in hands {
            for hero_action in heros {
                for seed in 0..6u64 {
                    let mut input = empty_input();
                    input.street = Street::Preflop;
                    input.board = BoardCards::empty();
                    input.position = Position::Dealer;
                    input.hero = HoleCards::new(c1, c2);
                    input.preflop_action = Some(PreflopAction::Rfi);
                    input.pot_before = 3;
                    input.to_call = 2;
                    input.stack_before = 200;
                    input.hero_action_taken = hero_action;
                    input.seed = seed;
                    let out = analyze(&input).unwrap();
                    for bad in POSTFLOP_ONLY {
                        assert!(
                            !out.reasoning_zh.contains(bad),
                            "preflop {label} hero={hero_action:?} seed={seed} verdict={:?}: copy uses postflop band word {bad:?}: {}",
                            out.verdict,
                            out.reasoning_zh
                        );
                    }
                    assert!(
                        out.reasoning_zh.contains("胜率"),
                        "preflop {label} seed={seed}: copy must still state equity: {}",
                        out.reasoning_zh
                    );
                    // Chat mode shares the template set.
                    let chat =
                        analyze_chat(&input, "00000000-0000-0000-0000-000000000000").unwrap();
                    for bad in POSTFLOP_ONLY {
                        assert!(
                            !chat.reasoning_zh.contains(bad),
                            "preflop chat {label} seed={seed}: {bad:?} in {}",
                            chat.reasoning_zh
                        );
                    }
                }
            }
        }
    }
}
