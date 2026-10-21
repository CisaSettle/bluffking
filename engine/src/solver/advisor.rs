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
/// the ADR-082 coach early-stop).
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
