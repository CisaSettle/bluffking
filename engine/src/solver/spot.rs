//! Self-serve spot analyzer (Solver M1 §1b).
//!
//! `analyze_spot(&SpotRequest) -> Result<SpotAnalysis, SolverError>` turns a
//! user-described spot (positions, stack, street, board+hand, pot/to-call,
//! preflop line, facing) into a structured verdict + equity + EV + reason +
//! (preflop) the 169 range grid.
//!
//! Honesty rule (M1-SPEC): preflop answers come from the BluffKing CFR
//! approximate-equilibrium chart → `SolveMethod::CfrEquityRealization`; postflop
//! answers are equity/EV heuristic → `SolveMethod::EquityHeuristic`. The grid is
//! emitted ONLY for preflop spots.
//!
//! NOTE (honest label): the preflop chart (`engine/data/preflop_v2.json`) is a
//! BluffKing Discounted-CFR APPROXIMATE-equilibrium solve over a SIMPLIFIED tree
//! with all-in-equity + per-class equity-realization terminal EV (see
//! `engine/src/solver/preflop_cfr.rs` + `engine/examples/gen_preflop_ranges.rs`).
//! It is genuine equilibrium math but NOT a true postflop GTO solve. The honesty
//! badge is therefore `cfr_equity_realization` ("CFR approx. equilibrium ·
//! equity-realization"), NEVER "GTO".
//!
//! All public types are our own — no `rs_poker` leaks (ADR-012). Pure +
//! deterministic given `req.advice_seed` / `req.reveal_seed`.

use crate::hand::{BoardCards, HoleCards, Street};

use super::advisor::{self, PostflopRuleCtx, SolverAction, SolverError, SolverVerdict, TableSize};
use super::equity::{equity, EquityInput, OpponentSpec, RangeBucket};
use super::hand_strength::{classify, HandStrength};
use super::preflop_charts::{self, hand_key, ActionBucket, ChartCell, HandKey, PositionBucket};
use super::DEFAULT_MC_TRIALS;

/// Preflop line the spot sits on, from hero's perspective.
#[derive(Debug, Clone, Copy, serde::Serialize, serde::Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum PreflopLine {
    /// Folded to hero, no opens yet — hero open-raises (RFI).
    Rfi,
    /// Exactly one opener ahead of hero.
    FacingOpen,
    /// Hero open-raised, villain 3-bet, hero facing the 3-bet.
    Vs3bet,
    /// Hero 3-bet, villain 4-bet, hero facing the 4-bet.
    Vs4bet,
    /// Hero faces one or more limps with no raise yet.
    FacingLimp,
}

/// What hero is facing this street (postflop UX hint).
#[derive(Debug, Clone, Copy, serde::Serialize, serde::Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum HeroFacing {
    /// Villain has bet/raised — hero must call/raise/fold.
    Bet,
    /// Villain checked — hero may check behind or bet.
    Check,
    /// Hero is first to act this street.
    FirstToAct,
}

/// A fully-described spot to analyze.
#[derive(Debug, Clone)]
pub struct SpotRequest {
    pub table_size: TableSize,
    pub hero_position: PositionBucket,
    pub villain_position: Option<PositionBucket>,
    pub stack_bb: u32,
    pub street: Street,
    pub hero: HoleCards,
    pub board: BoardCards,
    pub pot_bb: f32,
    pub to_call_bb: f32,
    pub preflop_line: PreflopLine,
    pub hero_facing: HeroFacing,
    /// Present ⇒ a verdict is returned (graded against the recommendation).
    pub hero_action: Option<SolverAction>,
    /// Present ⇒ ALSO compute `exact_equity_pct` = hero equity vs this exact
    /// villain holding (the "你 vs 对手实际牌" reveal). The recommendation /
    /// verdict / `equity_pct` stay range-based (villain cards are NOT used for
    /// advice — only this reveal number). `None` ⇒ `exact_equity_pct = None`.
    pub villain_hand: Option<HoleCards>,
    /// Seed for the ADVICE channel: the range-equity Monte-Carlo draw that
    /// drives `equity_pct`, the recommendation, the verdict and the reason code.
    /// The caller MUST derive this WITHOUT the villain's exact hand so the
    /// reveal-only `villain_hand` can never perturb the advice (the documented
    /// invariant — see [`SpotRequest::villain_hand`]). Renamed from the former
    /// single `seed`; that field fed both channels, letting `villain_hand` leak
    /// into the range equity / recommendation via the MC draw.
    pub advice_seed: u64,
    /// Seed for the REVEAL channel: the exact-equity Monte-Carlo draw vs the
    /// villain's real hand (`exact_equity_pct`). MAY incorporate `villain_hand`
    /// since it only affects the reveal number, never the advice. Unused when
    /// `villain_hand` is `None`.
    pub reveal_seed: u64,
}

/// How the answer was derived (honesty badge).
#[derive(Debug, Clone, Copy, serde::Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum SolveMethod {
    /// Preflop answer from the BluffKing Discounted-CFR APPROXIMATE-equilibrium
    /// solve over a SIMPLIFIED tree with all-in-equity + per-class
    /// equity-realization terminal EV (`engine/data/preflop_v2.json`, generated
    /// by `engine/src/solver/preflop_cfr.rs`). Genuine equilibrium math, but NOT
    /// a true postflop GTO solve — labelled "CFR approx. equilibrium ·
    /// equity-realization", NEVER "GTO". Serializes to `"cfr_equity_realization"`.
    CfrEquityRealization,
    /// Equity/EV heuristic (NOT true GTO).
    EquityHeuristic,
}

/// Per-cell action class for the 169 grid.
#[derive(Debug, Clone, Copy, serde::Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum GridAction {
    Raise,
    Call,
    Fold,
    /// Mixed strategy (0 < freq < ~0.99).
    Mix,
}

/// One cell of the 169 grid.
#[derive(Debug, Clone, serde::Serialize, PartialEq)]
pub struct GridCell {
    pub hand_key: String,
    pub action: GridAction,
    /// Continue frequency, 0..1.
    pub freq: f32,
    /// True for hero's own hand key.
    pub is_hero: bool,
}

/// Summary of the villain range the equity was computed against.
#[derive(Debug, Clone, serde::Serialize, PartialEq)]
pub struct RangeSummary {
    /// e.g. "co_rfi" / "random".
    pub code: String,
    /// Concrete combos in the range (pre-conflict-filter).
    pub combos: u32,
    /// Percent of all 1326 starting combos.
    pub pct: f32,
}

/// Structured reason → the client renders bilingual copy (no baked sentence
/// here;守 i18n: strings are templates, data is bound at render time).
#[derive(Debug, Clone, Copy, serde::Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum ReasonCode {
    PreflopInRange,
    PreflopNotInRange,
    PreflopMixed,
    ValueBetStrong,
    SemibluffDraw,
    PotOddsCall,
    PotOddsFold,
    PotControlCheck,
    MarginalCall,
    ClearFold,
}

/// The structured spot analysis (mirrors the §2 wire response).
#[derive(Debug, Clone, serde::Serialize)]
pub struct SpotAnalysis {
    pub method: SolveMethod,
    pub recommended: SolverAction,
    pub verdict: Option<SolverVerdict>,
    pub equity_pct: u8,
    pub pot_odds_pct: u8,
    pub ev_margin_bb: f32,
    pub hand_class: HandStrength,
    pub villain_range: RangeSummary,
    pub reason_code: ReasonCode,
    /// `Some(169 cells)` iff `street == Preflop`.
    pub preflop_grid: Option<Vec<GridCell>>,
    /// `Some(pct)` iff a `villain_hand` was supplied ⇒ hero's exact equity vs
    /// that real holding (the "你 vs 对手实际牌" reveal). `None` otherwise.
    /// Serializes to `"exact_equity_pct"` (null when absent).
    pub exact_equity_pct: Option<u8>,
}

/// Total number of distinct 2-card combos: C(52,2) = 1326.
const TOTAL_COMBOS: f32 = 1326.0;

/// Analyze a fully-described spot.
///
/// Pure + deterministic given `req.advice_seed` / `req.reveal_seed`. Validates
/// cards/board-vs-street and returns a [`SolverError`] on bad input.
pub fn analyze_spot(req: &SpotRequest) -> Result<SpotAnalysis, SolverError> {
    validate(req)?;

    let is_preflop = matches!(req.street, Street::Preflop);

    // --- Villain range (from charts) → OpponentSpec ---
    let (villain_spec, villain_summary) = villain_range(req);

    // --- Equity vs that range / fallback ---
    // ADVICE channel: seeded with `advice_seed`, which the caller derives WITHOUT
    // the villain's exact hand. This is what guarantees the reveal-only
    // `villain_hand` cannot perturb the range equity / recommendation / verdict.
    let eq = equity(EquityInput {
        hero: req.hero,
        board: req.board.clone(),
        opponents: villain_spec,
        trials: DEFAULT_MC_TRIALS,
        seed: req.advice_seed,
        // ADR-082: the public spot/replay path is NOT the coach — stays byte-identical.
        early_stop: None,
    });
    let equity_pct = eq.equity_pct();

    // --- Pot odds ---
    let pot_odds_pct = pot_odds_pct(req.pot_bb, req.to_call_bb);

    let hand_class = classify(req.hero, &req.board);

    // --- Recommendation (reuse advisor rule logic) ---
    let key = hand_key(req.hero);
    let recommended = if is_preflop {
        let bucket = hero_action_bucket(req.preflop_line);
        advisor::recommend_preflop(req.hero_position, bucket, &key)
    } else {
        let ctx = PostflopRuleCtx {
            equity_pct,
            pot_odds_pct,
            spr: spr(req.stack_bb, req.pot_bb),
            hand_strength: hand_class,
            to_call: bb_to_units(req.to_call_bb),
            stack_before: bb_to_units(req.stack_bb as f32),
            can_check: req.to_call_bb <= 0.0,
            // Intentional self-serve proxy: we don't track preflop last-aggressor
            // here, so `hero_facing ∈ {Check, FirstToAct}` stands in for it. Only
            // rules R3/R6 read `is_aggressor`, and both require `can_check==true`,
            // so a `HeroFacing::Bet` (to_call>0 ⇒ can_check==false) never reaches
            // them. NOT a bug — see advisor::analyze_postflop for the live model.
            is_aggressor: matches!(req.hero_facing, HeroFacing::Check | HeroFacing::FirstToAct),
            street: req.street,
        };
        advisor::recommend_postflop(&ctx)
    };

    // --- Verdict (only when hero_action provided) ---
    // F6: preflop MIXED-strategy cells must accept BOTH equilibrium branches (fold
    // and the bucket's continue action) — grading the other mixed branch as a
    // Mistake would contradict the grid, which paints the same cell `Mix`. Postflop
    // keeps the plain action-vs-action grading.
    let verdict = req.hero_action.map(|hero| {
        if is_preflop {
            let bucket = hero_action_bucket(req.preflop_line);
            let cell_freq = preflop_charts::lookup(req.hero_position, bucket, &key)
                .map(|c| c.frequency)
                .unwrap_or(0.0);
            let continue_action = advisor::preflop_continue_action(bucket);
            advisor::preflop_verdict(
                Some(hero),
                recommended,
                continue_action,
                cell_freq,
                equity_pct,
                pot_odds_pct,
            )
        } else {
            advisor::verdict_from_actions(Some(hero), recommended, equity_pct, pot_odds_pct)
        }
    });

    // --- EV margin (bb): call EV vs fold (0). Positive ⇒ +EV to continue ---
    let ev_margin_bb = ev_margin(equity_pct, req.pot_bb, req.to_call_bb);

    let method = if is_preflop {
        SolveMethod::CfrEquityRealization
    } else {
        SolveMethod::EquityHeuristic
    };

    let reason_code = reason_code(
        req,
        is_preflop,
        recommended,
        &key,
        equity_pct,
        pot_odds_pct,
        hand_class,
    );

    let preflop_grid = if is_preflop {
        Some(build_grid(req, &key))
    } else {
        None
    };

    // --- Exact equity vs the villain's REAL hand (reveal only) ---
    // This is computed ONLY for the "你 vs 对手实际牌" display number — it never
    // feeds the recommendation / verdict / range equity above (those stay
    // chart-range-based, the only teachable strategy at the table). When no
    // villain_hand is supplied, the reveal is absent.
    //
    // REVEAL channel: seeded with the SEPARATE `reveal_seed` (which may depend on
    // `villain_hand`). Keeping it off `advice_seed` is the second half of the
    // split — the advice MC above never observes a villain-dependent seed, so the
    // reveal-only hand cannot bleed into `equity_pct` / recommendation / verdict.
    let exact_equity_pct = req.villain_hand.map(|v| {
        equity(EquityInput {
            hero: req.hero,
            board: req.board.clone(),
            opponents: OpponentSpec::Known(vec![v]),
            trials: DEFAULT_MC_TRIALS,
            seed: req.reveal_seed,
            // ADR-082: reveal channel is NOT the coach — stays byte-identical.
            early_stop: None,
        })
        .equity_pct()
    });

    Ok(SpotAnalysis {
        method,
        recommended,
        verdict,
        equity_pct,
        pot_odds_pct,
        ev_margin_bb,
        hand_class,
        villain_range: villain_summary,
        reason_code,
        preflop_grid,
        exact_equity_pct,
    })
}

// ---------------------------------------------------------------------------
// Hand replay (per-street 对局复盘)
// ---------------------------------------------------------------------------

/// Per-street betting context for a [`ReplayRequest`]. Mirrors the slice of a
/// [`SpotRequest`] that varies street-to-street; the shared positions / stack /
/// line / table size live on the `ReplayRequest`.
#[derive(Debug, Clone)]
pub struct ReplayStreetInput {
    pub pot_bb: f32,
    pub to_call_bb: f32,
    pub hero_facing: HeroFacing,
    /// Present ⇒ a verdict is graded for that street; `None` ⇒ no verdict.
    pub hero_action: Option<SolverAction>,
}

/// A full hand replay: hero's + villain's EXACT hands plus the board as far as
/// the hand went, replayed street-by-street. Each analyzed street reuses the
/// chart-range advice machinery AND surfaces the exact equity vs villain's real
/// hand (see [`SpotRequest::villain_hand`]).
#[derive(Debug, Clone)]
pub struct ReplayRequest {
    pub table_size: TableSize,
    pub hero_position: PositionBucket,
    pub villain_position: Option<PositionBucket>,
    pub stack_bb: u32,
    pub hero: HoleCards,
    pub villain: HoleCards,
    pub board: BoardCards,
    pub preflop_line: PreflopLine,
    /// One entry per analyzed street, in order preflop→flop→turn→river. Length
    /// 1..=4 selects how many streets are analyzed; the board count must match
    /// the LAST street (1→0, 2→3, 3→4, 4→5).
    pub streets: Vec<ReplayStreetInput>,
    /// Base seed for the ADVICE channel (range equity / recommendation / verdict)
    /// across all streets. The caller MUST derive it WITHOUT the villain's exact
    /// hand; `analyze_replay` does the per-street `wrapping_add`. See
    /// [`SpotRequest::advice_seed`].
    pub advice_seed: u64,
    /// Base seed for the REVEAL channel (exact equity vs the villain's real
    /// hand). MAY incorporate the villain's hand. See [`SpotRequest::reveal_seed`].
    pub reveal_seed: u64,
}

/// One street's analysis in a replay result.
#[derive(Debug, Clone, serde::Serialize)]
pub struct ReplayStreetAnalysis {
    /// `Street` serializes snake_case lowercase (`"preflop"|"flop"|"turn"|
    /// "river"`) — see `crate::hand::Street`.
    pub street: Street,
    pub analysis: SpotAnalysis,
}

/// Replay a hand street-by-street, returning one [`ReplayStreetAnalysis`] per
/// analyzed street (preflop→river, in order).
///
/// Pure + deterministic. Each street's analysis is computed via [`analyze_spot`]
/// with the villain's exact hand supplied, so every entry carries both the
/// chart-range advice (recommended / verdict / `equity_pct` / …) AND the exact
/// equity reveal (`exact_equity_pct`).
///
/// ## Per-street seed derivation
/// Each street `i` (0-based) is analyzed with
/// `advice_seed = req.advice_seed.wrapping_add(i)` and
/// `reveal_seed = req.reveal_seed.wrapping_add(i)`, so streets are independent
/// yet the whole replay is reproducible from two base seeds (the server derives
/// the base seeds for the request; this function does the per-street
/// wrapping_add). The two channels stay separate per [`SpotRequest`] so the
/// reveal-only villain hand never perturbs a street's advice.
///
/// ## Validation
/// - `streets.len()` must be in `1..=4` (else [`SolverError::InvalidCards`]).
/// - `req.board.count()` must match the LAST street: len 1→0, 2→3, 3→4, 4→5.
/// - Each per-street prefix board (preflop empty, flop = flop only, turn =
///   flop+turn, river = full) is fed to [`analyze_spot`], which re-checks card
///   uniqueness (incl. villain) + board-vs-street.
pub fn analyze_replay(req: &ReplayRequest) -> Result<Vec<ReplayStreetAnalysis>, SolverError> {
    let n = req.streets.len();
    if !(1..=4).contains(&n) {
        return Err(SolverError::InvalidCards);
    }

    // The supplied board must match exactly the LAST analyzed street's expected
    // community-card count.
    let expected_board = match n {
        1 => 0, // preflop only
        2 => 3, // through flop
        3 => 4, // through turn
        4 => 5, // through river
        _ => unreachable!("n is checked to be in 1..=4 above"),
    };
    if req.board.count() != expected_board {
        return Err(SolverError::InvalidCards);
    }

    const STREETS: [Street; 4] = [Street::Preflop, Street::Flop, Street::Turn, Street::River];

    let mut out = Vec::with_capacity(n);
    for (i, s) in req.streets.iter().enumerate() {
        let street = STREETS[i];
        let board_i = board_prefix_for_street(&req.board, street);

        let spot = SpotRequest {
            table_size: req.table_size,
            hero_position: req.hero_position,
            villain_position: req.villain_position,
            stack_bb: req.stack_bb,
            street,
            hero: req.hero,
            board: board_i,
            pot_bb: s.pot_bb,
            to_call_bb: s.to_call_bb,
            preflop_line: req.preflop_line,
            hero_facing: s.hero_facing,
            hero_action: s.hero_action,
            villain_hand: Some(req.villain),
            advice_seed: req.advice_seed.wrapping_add(i as u64),
            reveal_seed: req.reveal_seed.wrapping_add(i as u64),
        };
        let analysis = analyze_spot(&spot)?;
        out.push(ReplayStreetAnalysis { street, analysis });
    }

    Ok(out)
}

/// The board prefix visible on `street`: preflop ⇒ empty; flop ⇒ flop only;
/// turn ⇒ flop+turn; river ⇒ full board.
fn board_prefix_for_street(board: &BoardCards, street: Street) -> BoardCards {
    match street {
        Street::Preflop => BoardCards::empty(),
        Street::Flop => BoardCards {
            flop: board.flop,
            turn: None,
            river: None,
        },
        Street::Turn => BoardCards {
            flop: board.flop,
            turn: board.turn,
            river: None,
        },
        Street::River => board.clone(),
    }
}

// ---------------------------------------------------------------------------
// Villain range
// ---------------------------------------------------------------------------

/// Derive the villain's chart `ActionBucket` from the preflop line and the
/// street being analyzed.
///
/// The villain range is the one that produced the action hero is facing:
/// - `FacingOpen` ⇒ villain is the opener ⇒ villain RFI.
/// - `Vs3bet`     ⇒ villain 3-bet hero's open ⇒ villain facing_open (3-bet).
/// - `Vs4bet`     ⇒ villain 4-bet ⇒ villain vs_3bet (continuing/4-bet value).
/// - `FacingLimp` ⇒ villain limped (no chart range) ⇒ widest proxy: villain RFI.
/// - `Rfi`        ⇒ PREFLOP: no villain has acted yet ⇒ no chart range (random).
///   POSTFLOP: hero opened and villain CALLED (flat-defended) ⇒ NOT a chart
///   `ActionBucket` — the v3 model has no flat-call defend bucket (`facing_open`
///   is the 3-BET range, see `recommend_preflop` F2 note). This case is handled
///   separately by `flat_defend_range` (F3), which builds the "calls but does NOT
///   3-bet" range; returning `FacingOpen` here would wrongly analyze the most
///   common postflop spot vs a tight 3-bet range. So this returns `None` and the
///   caller routes `Rfi` postflop to the derived flat-defend range instead.
fn villain_chart_bucket(line: PreflopLine) -> Option<ActionBucket> {
    match line {
        // RFI (preflop OR postflop) maps to NO single chart bucket here. Preflop:
        // villain has not acted ⇒ random. Postflop: hero opened + villain CALLED
        // ⇒ derived flat-defend range (F3), handled in `villain_range`.
        PreflopLine::Rfi => None,
        PreflopLine::FacingOpen => Some(ActionBucket::Rfi),
        PreflopLine::Vs3bet => Some(ActionBucket::FacingOpen),
        PreflopLine::Vs4bet => Some(ActionBucket::Vs3bet),
        PreflopLine::FacingLimp => Some(ActionBucket::Rfi),
    }
}

/// Build the villain's POSTFLOP flat-call DEFEND range (F3): hero opened and
/// villain CALLED (did not 3-bet). The v3 chart has no dedicated flat-call bucket
/// — `facing_open` is the 3-BET range — so a villain who flats holds the hands
/// they'd voluntarily play (their RFI range) MINUS the hands they'd 3-bet
/// (`facing_open`), i.e. the "call, don't 3-bet" portion. This is the realistic
/// defend-by-calling range: WIDER and WEAKER than the 3-bet range (the strongest
/// hands are removed because they 3-bet), and tighter than a random hand.
///
/// Returns `(key, weight)` entries; `weight` is `defend_freq − threebet_freq`
/// clamped to `[0, 1]` (a hand the villain 3-bets at full frequency contributes
/// nothing to the flat-call range). Empty ⇒ caller falls back to `Random`.
///
/// For a villain with no chart RFI range (the BB, which never opens unopened),
/// there is no flat-defend proxy in the chart, so this returns empty and the
/// caller falls back to the widest proxy (`Random`) rather than a wrong 3-bet
/// range — honest "unknown defender" instead of a fabricated tight range.
fn flat_defend_range(vpos: PositionBucket) -> Vec<(HandKey, f32)> {
    use std::collections::BTreeMap;
    // Hands the villain would voluntarily play (their open range) = the widest
    // honest superset of a flat-call defend range available in the v3 chart.
    let rfi = preflop_charts::range_entries(vpos, ActionBucket::Rfi);
    if rfi.is_empty() {
        return Vec::new();
    }
    // Hands the villain would 3-bet (and therefore NOT flat) — subtract these.
    let threebet: BTreeMap<HandKey, f32> =
        preflop_charts::range_entries(vpos, ActionBucket::FacingOpen)
            .into_iter()
            .collect();
    rfi.into_iter()
        .filter_map(|(key, rfi_f)| {
            let tb = threebet.get(&key).copied().unwrap_or(0.0);
            let flat = (rfi_f - tb).clamp(0.0, 1.0);
            if flat > 0.0 {
                Some((key, flat))
            } else {
                None
            }
        })
        .collect()
}

/// The action bucket for HERO's own chart lookup (recommendation/grid).
fn hero_action_bucket(line: PreflopLine) -> ActionBucket {
    match line {
        PreflopLine::Rfi => ActionBucket::Rfi,
        PreflopLine::FacingOpen => ActionBucket::FacingOpen,
        PreflopLine::Vs3bet => ActionBucket::Vs3bet,
        PreflopLine::Vs4bet => ActionBucket::Vs4bet,
        PreflopLine::FacingLimp => ActionBucket::FacingLimp,
    }
}

/// Build the villain `OpponentSpec` + its `RangeSummary`. Falls back to
/// `Random(1)` (summary code `"random"`) when no chart range is available
/// (no villain position, or empty bucket like UTG facing_open / RFI line).
fn villain_range(req: &SpotRequest) -> (OpponentSpec, RangeSummary) {
    let fallback = || {
        (
            OpponentSpec::Random(1),
            RangeSummary {
                code: "random".to_string(),
                combos: TOTAL_COMBOS as u32,
                pct: 100.0,
            },
        )
    };

    let Some(vpos) = req.villain_position else {
        return fallback();
    };

    // F3: hero opened + villain CALLED (Rfi postflop) ⇒ derived flat-call defend
    // range (RFI minus 3-bet), NOT the tight `facing_open` 3-bet range. All other
    // lines use the chart `ActionBucket`. Preflop Rfi has no villain range yet.
    let is_postflop = !matches!(req.street, Street::Preflop);
    let (entries, code): (Vec<(HandKey, f32)>, String) = match req.preflop_line {
        PreflopLine::Rfi if is_postflop => (
            flat_defend_range(vpos),
            format!("{}_flat_defend", vpos.key().to_ascii_lowercase()),
        ),
        _ => {
            let Some(vbucket) = villain_chart_bucket(req.preflop_line) else {
                return fallback();
            };
            (
                preflop_charts::range_entries(vpos, vbucket),
                format!(
                    "{}_{}",
                    vpos.key().to_ascii_lowercase(),
                    vbucket.key().to_ascii_lowercase()
                ),
            )
        }
    };
    if entries.is_empty() {
        return fallback();
    }

    // F4: the DISPLAYED combos/pct must weight each key's combos by its
    // continue FREQUENCY — a mixed cell at freq 0.5 contributes HALF its combos
    // to the effective range size. The old code summed RAW combos (every key at
    // full weight) while `RangeBucket.weight = freq`, so the shown range size was
    // inflated for any v3 mixed bucket (e.g. CO vs_4bet showed 134/10.1% raw vs
    // ~48.7/3.7% freq-weighted). The equity MC already uses `weight`, so only the
    // summary number was wrong; this aligns the shown size with the actual range.
    let mut buckets = Vec::with_capacity(entries.len());
    let mut weighted_combos: f32 = 0.0;
    for (key, freq) in entries {
        let f = freq.clamp(0.0, 1.0);
        weighted_combos += preflop_charts::combos_for_key(&key) as f32 * f;
        buckets.push(RangeBucket {
            hand_key: key,
            weight: f,
        });
    }
    if weighted_combos <= 0.0 {
        return fallback();
    }
    // Round to nearest whole combo for display (combos is an integer count).
    let combos = weighted_combos.round() as u32;

    let pct = (weighted_combos / TOTAL_COMBOS) * 100.0;
    (
        OpponentSpec::Range(buckets),
        RangeSummary { code, combos, pct },
    )
}

// ---------------------------------------------------------------------------
// Preflop grid
// ---------------------------------------------------------------------------

/// Build all 169 grid cells for hero's (position, action) bucket.
///
/// freq → `GridAction`: ≥0.99 → the bucket's single action; 0<freq<0.99 →
/// `Mix`; absent (None) → `Fold` (freq 0). Hero's own key marked `is_hero`.
fn build_grid(req: &SpotRequest, hero_key: &str) -> Vec<GridCell> {
    let bucket = hero_action_bucket(req.preflop_line);
    let pos = req.hero_position;
    preflop_charts::all_hand_keys()
        .into_iter()
        .map(|key| {
            let cell: Option<ChartCell> = preflop_charts::lookup(pos, bucket, &key);
            let freq = cell.map(|c| c.frequency).unwrap_or(0.0);
            let action = grid_action(freq);
            let is_hero = key == hero_key;
            GridCell {
                hand_key: key,
                action,
                freq,
                is_hero,
            }
        })
        .collect()
}

/// Map a chart frequency to a grid action, given the line's "continue" action.
fn grid_action(freq: f32) -> GridAction {
    if freq <= 0.0 {
        return GridAction::Fold;
    }
    if freq < 0.99 {
        return GridAction::Mix;
    }
    // freq ~1.0 → the bucket's full-frequency continue action.
    GridAction::Raise
}

// ---------------------------------------------------------------------------
// Reason code
// ---------------------------------------------------------------------------

#[allow(clippy::too_many_arguments)]
fn reason_code(
    req: &SpotRequest,
    is_preflop: bool,
    recommended: SolverAction,
    hero_key: &str,
    equity_pct: u8,
    pot_odds_pct: u8,
    hand_class: HandStrength,
) -> ReasonCode {
    if is_preflop {
        let bucket = hero_action_bucket(req.preflop_line);
        let freq = preflop_charts::lookup(req.hero_position, bucket, hero_key)
            .map(|c| c.frequency)
            .unwrap_or(0.0);
        return if freq <= 0.0 {
            ReasonCode::PreflopNotInRange
        } else if freq < 0.99 {
            ReasonCode::PreflopMixed
        } else {
            ReasonCode::PreflopInRange
        };
    }

    // Postflop reasons follow the recommendation + the equity/strength signal.
    match recommended {
        SolverAction::Raise | SolverAction::AllIn => {
            if hand_class == HandStrength::DrawStrong {
                ReasonCode::SemibluffDraw
            } else {
                ReasonCode::ValueBetStrong
            }
        }
        SolverAction::Call => {
            if hand_class == HandStrength::DrawStrong {
                ReasonCode::SemibluffDraw
            } else if equity_pct as i32 >= pot_odds_pct as i32 + 5 {
                ReasonCode::PotOddsCall
            } else {
                ReasonCode::MarginalCall
            }
        }
        SolverAction::Check => ReasonCode::PotControlCheck,
        SolverAction::Fold => {
            if req.to_call_bb > 0.0 {
                ReasonCode::PotOddsFold
            } else {
                ReasonCode::ClearFold
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Validation + numeric helpers
// ---------------------------------------------------------------------------

fn validate(req: &SpotRequest) -> Result<(), SolverError> {
    // Card uniqueness (hero + board + villain's exact hand when present). A
    // villain card that collides with hero, the board, or itself is invalid.
    let mut all = vec![req.hero.card1, req.hero.card2];
    all.extend(req.board.all_cards());
    if let Some(v) = req.villain_hand {
        all.push(v.card1);
        all.push(v.card2);
    }
    for (i, a) in all.iter().enumerate() {
        for b in &all[i + 1..] {
            if a == b {
                return Err(SolverError::InvalidCards);
            }
        }
    }
    // Board SHAPE must match the street exactly — a count-only check accepts
    // structurally impossible boards (e.g. a turn card with no flop, or a river
    // with no turn) whenever the total happens to match (U21, dual-AI OSS review).
    let b = &req.board;
    let shape_ok = match req.street {
        Street::Preflop => b.flop.is_none() && b.turn.is_none() && b.river.is_none(),
        Street::Flop => b.flop.is_some() && b.turn.is_none() && b.river.is_none(),
        Street::Turn => b.flop.is_some() && b.turn.is_some() && b.river.is_none(),
        Street::River => b.flop.is_some() && b.turn.is_some() && b.river.is_some(),
    };
    if !shape_ok {
        return Err(SolverError::InvalidCards);
    }
    // Negative pot/to-call or NaN is invalid.
    if !req.pot_bb.is_finite()
        || !req.to_call_bb.is_finite()
        || req.pot_bb < 0.0
        || req.to_call_bb < 0.0
    {
        return Err(SolverError::InvalidCards);
    }
    Ok(())
}

/// `round(to_call / (pot + to_call) * 100)`. Zero to-call ⇒ 0.
fn pot_odds_pct(pot_bb: f32, to_call_bb: f32) -> u8 {
    if to_call_bb <= 0.0 {
        return 0;
    }
    let denom = pot_bb + to_call_bb;
    if denom <= 0.0 {
        return 0;
    }
    ((to_call_bb / denom) * 100.0).round().clamp(0.0, 100.0) as u8
}

/// EV margin in bb of continuing (call) vs folding (EV 0):
/// `EV_call = equity * (pot + to_call) - to_call`. For a check (to_call=0) the
/// margin is `equity * pot` (the share of the current pot hero realizes).
fn ev_margin(equity_pct: u8, pot_bb: f32, to_call_bb: f32) -> f32 {
    let eq = equity_pct as f32 / 100.0;
    let raw = if to_call_bb <= 0.0 {
        eq * pot_bb
    } else {
        eq * (pot_bb + to_call_bb) - to_call_bb
    };
    // F4 (correctness/robustness) defense-in-depth: the handler caps pot/to_call
    // so this normally can't overflow, but guard against a non-finite result so
    // the serialized response is ALWAYS a finite JSON number (+inf/NaN serialize
    // as `null`, breaking the API "number" contract). 0.0 is a safe neutral EV.
    if raw.is_finite() {
        round2(raw)
    } else {
        0.0
    }
}

fn spr(stack_bb: u32, pot_bb: f32) -> f32 {
    let denom = pot_bb.max(1.0);
    stack_bb as f32 / denom
}

/// Convert a bb amount to integer chip "units" for the advisor's u32-based
/// rule context (1 bb = 100 units; the absolute scale is irrelevant — only the
/// >0 / ratio semantics matter for the postflop rules).
fn bb_to_units(bb: f32) -> u32 {
    if bb <= 0.0 {
        0
    } else {
        (bb * 100.0).round() as u32
    }
}

fn round2(v: f32) -> f32 {
    (v * 100.0).round() / 100.0
}

#[cfg(test)]
mod tests;
