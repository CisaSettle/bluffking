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
            let continue_action = advisor::preflop_continue_action(req.hero_position, bucket, &key);
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
fn villain_chart_bucket(line: PreflopLine, _street: Street) -> Option<ActionBucket> {
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
    let rfi: BTreeMap<HandKey, f32> = preflop_charts::range_entries(vpos, ActionBucket::Rfi)
        .into_iter()
        .collect();
    if rfi.is_empty() {
        return Vec::new();
    }
    // Hands the villain would 3-bet (and therefore NOT flat) — subtract these.
    let threebet: BTreeMap<HandKey, f32> =
        preflop_charts::range_entries(vpos, ActionBucket::FacingOpen)
            .into_iter()
            .collect();
    let mut out: Vec<(HandKey, f32)> = rfi
        .into_iter()
        .filter_map(|(key, rfi_f)| {
            let tb = threebet.get(&key).copied().unwrap_or(0.0);
            let flat = (rfi_f - tb).clamp(0.0, 1.0);
            if flat > 0.0 {
                Some((key, flat))
            } else {
                None
            }
        })
        .collect();
    out.sort_by(|a, b| a.0.cmp(&b.0));
    out
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
            format!("{}_flat_defend", lower(vpos.key())),
        ),
        _ => {
            let Some(vbucket) = villain_chart_bucket(req.preflop_line, req.street) else {
                return fallback();
            };
            (
                preflop_charts::range_entries(vpos, vbucket),
                format!("{}_{}", lower(vpos.key()), lower(vbucket.key())),
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
    for (key, freq) in &entries {
        let f = freq.clamp(0.0, 1.0);
        weighted_combos += preflop_charts::combos_for_key(key) as f32 * f;
        buckets.push(RangeBucket {
            hand_key: key.clone(),
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
            let action = grid_action(req.preflop_line, freq);
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
fn grid_action(line: PreflopLine, freq: f32) -> GridAction {
    if freq <= 0.0 {
        return GridAction::Fold;
    }
    if freq < 0.99 {
        return GridAction::Mix;
    }
    // freq ~1.0 → the bucket's full-frequency continue action.
    match line {
        // F2 (2026-06-25): vs_4bet continue is a 5-bet JAM (all-in raise), NOT a
        // flat-call — see `advisor::recommend_preflop` + `preflop_cfr::pot_model`
        // (`hero_allin = true`). The 169-grid's 4-action legend has no separate
        // all-in swatch, so a jam is painted as the aggressive `Raise` action —
        // consistent with the headline (which says AllIn) and never the
        // contradictory `Call` the user saw before. An all-in IS a raise (to
        // stack); the previous `Call` told premiums to flat a 4-bet.
        PreflopLine::Vs4bet => GridAction::Raise,
        _ => GridAction::Raise,
    }
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

fn lower(s: &str) -> String {
    s.to_ascii_lowercase()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::card::{Card, Rank, Suit};

    fn hc(r1: Rank, s1: Suit, r2: Rank, s2: Suit) -> HoleCards {
        HoleCards::new(Card::new(r1, s1), Card::new(r2, s2))
    }

    fn preflop_req(hero: HoleCards, pos: PositionBucket, line: PreflopLine) -> SpotRequest {
        SpotRequest {
            table_size: TableSize::SixMax,
            hero_position: pos,
            villain_position: None,
            stack_bb: 100,
            street: Street::Preflop,
            hero,
            board: BoardCards::empty(),
            pot_bb: 1.5,
            to_call_bb: 0.0,
            preflop_line: line,
            hero_facing: HeroFacing::FirstToAct,
            hero_action: None,
            villain_hand: None,
            advice_seed: 42,
            reveal_seed: 42,
        }
    }

    fn board3(c1: Card, c2: Card, c3: Card) -> BoardCards {
        BoardCards {
            flop: Some([c1, c2, c3]),
            turn: None,
            river: None,
        }
    }

    #[test]
    fn aa_utg_rfi_recommends_raise_with_grid() {
        let hero = hc(Rank::Ace, Suit::Spades, Rank::Ace, Suit::Hearts);
        let req = preflop_req(hero, PositionBucket::UTG, PreflopLine::Rfi);
        let a = analyze_spot(&req).unwrap();
        assert_eq!(a.method, SolveMethod::CfrEquityRealization);
        assert_eq!(a.recommended, SolverAction::Raise);
        let grid = a.preflop_grid.expect("preflop must emit a grid");
        assert_eq!(grid.len(), 169, "grid must be exactly 169 cells");
        let aa = grid.iter().find(|c| c.hand_key == "AA").unwrap();
        assert!(aa.freq >= 0.5, "AA freq must be ≥0.5 in UTG RFI");
        assert!(aa.is_hero, "AA must be marked is_hero");
        assert_eq!(aa.action, GridAction::Raise);
        // Exactly one hero cell.
        assert_eq!(grid.iter().filter(|c| c.is_hero).count(), 1);
        assert_eq!(a.reason_code, ReasonCode::PreflopInRange);
    }

    #[test]
    fn seven_two_off_utg_rfi_folds_not_in_range() {
        let hero = hc(Rank::Seven, Suit::Spades, Rank::Two, Suit::Hearts);
        let req = preflop_req(hero, PositionBucket::UTG, PreflopLine::Rfi);
        let a = analyze_spot(&req).unwrap();
        assert_eq!(a.recommended, SolverAction::Fold);
        assert_eq!(a.reason_code, ReasonCode::PreflopNotInRange);
        let grid = a.preflop_grid.unwrap();
        let h72 = grid.iter().find(|c| c.hand_key == "72o").unwrap();
        assert_eq!(h72.action, GridAction::Fold);
        assert!(h72.is_hero);
    }

    /// F6 (consistency): the chart now emits genuine MIXED frequencies (e.g. CO
    /// vs_4bet AQs = 0.26). The grid paints such a cell `Mix`, but the headline
    /// recommendation collapses freq < 0.5 → Fold. A user who took the OTHER
    /// legitimate mixed branch (F2: JAM the 4-bet — the vs_4bet continue is a 5-bet
    /// all-in, NOT a flat-call) must NOT be graded a Mistake — that would
    /// contradict the grid. This asserts the mixed-cell verdict semantics: grid
    /// shows `Mix`, and BOTH branches (fold / jam) grade Ok/Good (never Mistake).
    #[test]
    fn preflop_mixed_frequency_preserves_mixed_verdict_semantics() {
        // CO vs_4bet AQs is a mixed cell (~0.26): headline Fold, continue=AllIn (jam).
        let hero = hc(Rank::Ace, Suit::Spades, Rank::Queen, Suit::Spades);
        let mut req = preflop_req(hero, PositionBucket::CO, PreflopLine::Vs4bet);

        // Sanity: the underlying cell really is mixed (guards against a future
        // chart regen turning it pure, which would silently void this test).
        let cell = preflop_charts::lookup(PositionBucket::CO, ActionBucket::Vs4bet, "AQs")
            .expect("AQs must be present in CO vs_4bet");
        assert!(
            advisor::preflop_cell_is_mixed(cell.frequency),
            "AQs CO vs_4bet must be a MIXED cell for this test, got freq {}",
            cell.frequency
        );

        // Grid paints it Mix.
        req.hero_action = None;
        let a = analyze_spot(&req).unwrap();
        let grid = a.preflop_grid.expect("preflop grid");
        let aqs = grid.iter().find(|c| c.hand_key == "AQs").unwrap();
        assert_eq!(aqs.action, GridAction::Mix, "mixed cell must paint Mix");
        assert_eq!(a.reason_code, ReasonCode::PreflopMixed);
        // Headline collapses freq<0.5 → Fold (the higher-frequency branch).
        assert_eq!(a.recommended, SolverAction::Fold);

        // Branch 1: hero JAMS the 4-bet (the lower-frequency mixed branch — F2: the
        // vs_4bet continue is a 5-bet all-in, not a flat-call). Under the OLD binary
        // grading this was Mistake (AllIn vs gto=Fold). Now Ok.
        req.hero_action = Some(SolverAction::AllIn);
        let jammed = analyze_spot(&req).unwrap();
        assert_eq!(
            jammed.verdict,
            Some(SolverVerdict::Ok),
            "jamming a mixed 4-bet cell must be Ok, not a Mistake"
        );

        // Branch 2: hero FOLDS (the headline branch) → Good.
        req.hero_action = Some(SolverAction::Fold);
        let folded = analyze_spot(&req).unwrap();
        assert_eq!(
            folded.verdict,
            Some(SolverVerdict::Good),
            "folding the headline branch of a mixed cell is Good"
        );

        // A genuinely off-strategy action in this fold/jam-mix cell (flat-CALLING a
        // 4-bet — never part of the model's strategy, F2) is still graded by the
        // normal logic — mixing does not bless every action.
        req.hero_action = Some(SolverAction::Call);
        let called = analyze_spot(&req).unwrap();
        assert!(
            called.verdict.is_some(),
            "off-strategy action still produces a verdict (not blessed by the mix)"
        );
    }

    #[test]
    fn postflop_uses_equity_heuristic_and_emits_no_grid() {
        // Nut flush draw: hero AhKh on Qh-7h-2c (4 hearts → strong draw).
        let hero = hc(Rank::Ace, Suit::Hearts, Rank::King, Suit::Hearts);
        let board = BoardCards {
            flop: Some([
                Card::new(Rank::Queen, Suit::Hearts),
                Card::new(Rank::Seven, Suit::Hearts),
                Card::new(Rank::Two, Suit::Clubs),
            ]),
            turn: None,
            river: None,
        };
        let req = SpotRequest {
            table_size: TableSize::SixMax,
            hero_position: PositionBucket::BTN,
            villain_position: Some(PositionBucket::CO),
            stack_bb: 100,
            street: Street::Flop,
            hero,
            board,
            pot_bb: 6.0,
            to_call_bb: 2.0,
            preflop_line: PreflopLine::FacingOpen,
            hero_facing: HeroFacing::Bet,
            hero_action: None,
            villain_hand: None,
            advice_seed: 7,
            reveal_seed: 7,
        };
        let a = analyze_spot(&req).unwrap();
        assert_eq!(a.method, SolveMethod::EquityHeuristic);
        assert!(a.preflop_grid.is_none(), "postflop emits no grid");
        // Nut flush draw vs a value range still has real equity.
        assert!(
            a.equity_pct > 20 && a.equity_pct <= 100,
            "got {}",
            a.equity_pct
        );
        assert!(a.pot_odds_pct > 0);
    }

    #[test]
    fn villain_range_from_charts_lowers_equity_vs_random() {
        // AKo facing a CO open: villain range = CO RFI (tight-ish). Equity vs
        // that range should differ from (and be a real, derived) villain range
        // code, not "random".
        let hero = hc(Rank::Ace, Suit::Spades, Rank::King, Suit::Diamonds);
        let board = BoardCards {
            flop: Some([
                Card::new(Rank::Two, Suit::Clubs),
                Card::new(Rank::Seven, Suit::Diamonds),
                Card::new(Rank::Nine, Suit::Hearts),
            ]),
            turn: None,
            river: None,
        };
        let req = SpotRequest {
            table_size: TableSize::SixMax,
            hero_position: PositionBucket::BTN,
            villain_position: Some(PositionBucket::CO),
            stack_bb: 100,
            street: Street::Flop,
            hero,
            board,
            pot_bb: 5.0,
            to_call_bb: 0.0,
            preflop_line: PreflopLine::FacingOpen,
            hero_facing: HeroFacing::FirstToAct,
            hero_action: None,
            villain_hand: None,
            advice_seed: 3,
            reveal_seed: 3,
        };
        let a = analyze_spot(&req).unwrap();
        assert_eq!(a.villain_range.code, "co_rfi");
        assert!(a.villain_range.combos > 0 && a.villain_range.combos < 1326);
        assert!(a.villain_range.pct > 0.0 && a.villain_range.pct < 100.0);
    }

    #[test]
    fn postflop_rfi_aggressor_faces_flat_defend_range_not_threebet_or_random() {
        // Single most common postflop spot: hero opened (RFI) preflop from CO,
        // villain (BTN, which HAS a chart RFI range) flat-CALLED, now flop. F3:
        // the villain range MUST be the derived FLAT-DEFEND range (RFI minus
        // 3-bet) — NOT the tight `facing_open` 3-bet range (those hands would have
        // 3-bet, not flatted), and NOT "random".
        let hero = hc(Rank::Ace, Suit::Spades, Rank::King, Suit::Diamonds);
        let board = BoardCards {
            flop: Some([
                Card::new(Rank::Two, Suit::Clubs),
                Card::new(Rank::Seven, Suit::Diamonds),
                Card::new(Rank::Nine, Suit::Hearts),
            ]),
            turn: None,
            river: None,
        };
        let req = SpotRequest {
            table_size: TableSize::SixMax,
            hero_position: PositionBucket::CO,
            villain_position: Some(PositionBucket::BTN),
            stack_bb: 100,
            street: Street::Flop,
            hero,
            board,
            pot_bb: 6.0,
            to_call_bb: 0.0,
            preflop_line: PreflopLine::Rfi,
            hero_facing: HeroFacing::FirstToAct,
            hero_action: None,
            villain_hand: None,
            advice_seed: 9,
            reveal_seed: 9,
        };
        let a = analyze_spot(&req).unwrap();
        assert_eq!(
            a.villain_range.code, "btn_flat_defend",
            "hero-as-aggressor postflop must face villain's derived flat-defend range (F3)"
        );
        assert!(
            a.villain_range.combos > 0 && a.villain_range.combos < 1326,
            "expected a real defend range, got {} combos",
            a.villain_range.combos
        );
        assert!(a.villain_range.pct > 0.0 && a.villain_range.pct < 100.0);

        // The flat-defend range is the "calls but does NOT 3-bet" range — it must
        // EXCLUDE the hands the villain 3-bets (their strongest holdings), which is
        // exactly what makes it a genuine flat-CALL range and NOT the aggressive
        // `facing_open` (3-bet/continue) range the F3 regression analyzed against.
        let flat: std::collections::BTreeMap<HandKey, f32> =
            flat_defend_range(PositionBucket::BTN).into_iter().collect();
        let threebet: std::collections::BTreeMap<HandKey, f32> =
            preflop_charts::range_entries(PositionBucket::BTN, ActionBucket::FacingOpen)
                .into_iter()
                .collect();
        // AA is a full-frequency 3-bet (in facing_open) ⇒ it must NOT appear in the
        // flat-call range (you jam/3-bet AA, you never flat it). This proves the
        // top of the range is capped out — the defining property of a flat-call
        // range vs the 3-bet range.
        assert!(
            threebet.get("AA").copied().unwrap_or(0.0) >= 0.99,
            "precondition: AA must be a (near-)pure 3-bet in BTN facing_open"
        );
        assert!(
            flat.get("AA").copied().unwrap_or(0.0) <= 0.0,
            "AA must be EXCLUDED from the flat-call range (it 3-bets, never flats) — \
             proves the flat-defend range caps the top, unlike the 3-bet range (F3)"
        );
        // And it must still be a REAL, non-trivial range (not collapsed to empty):
        let flat_combos: f32 = flat
            .iter()
            .map(|(k, f)| preflop_charts::combos_for_key(k) as f32 * f)
            .sum();
        assert!(
            flat_combos > 50.0,
            "flat-defend range must be a real range, got {flat_combos:.1} combos"
        );

        // Equity is computed vs that derived Range (not vs random), and sane.
        assert!(
            a.equity_pct > 0 && a.equity_pct <= 100,
            "got {}",
            a.equity_pct
        );
        assert_eq!(a.method, SolveMethod::EquityHeuristic);
    }

    #[test]
    fn postflop_rfi_aggressor_vs_bb_defender_falls_back_to_wide() {
        // F3 edge: the BB has NO chart RFI range (it never opens unopened), so the
        // v3 chart has no flat-defend proxy for a BB caller. Rather than analyze vs
        // a WRONG tight 3-bet range, this honestly falls back to the widest proxy
        // ("random") — a BB call-defend range really is very wide.
        let hero = hc(Rank::Ace, Suit::Spades, Rank::King, Suit::Diamonds);
        let board = BoardCards {
            flop: Some([
                Card::new(Rank::Two, Suit::Clubs),
                Card::new(Rank::Seven, Suit::Diamonds),
                Card::new(Rank::Nine, Suit::Hearts),
            ]),
            turn: None,
            river: None,
        };
        let req = SpotRequest {
            table_size: TableSize::SixMax,
            hero_position: PositionBucket::BTN,
            villain_position: Some(PositionBucket::BB),
            stack_bb: 100,
            street: Street::Flop,
            hero,
            board,
            pot_bb: 6.0,
            to_call_bb: 0.0,
            preflop_line: PreflopLine::Rfi,
            hero_facing: HeroFacing::FirstToAct,
            hero_action: None,
            villain_hand: None,
            advice_seed: 9,
            reveal_seed: 9,
        };
        let a = analyze_spot(&req).unwrap();
        assert_eq!(
            a.villain_range.code, "random",
            "no chart flat-defend proxy for a BB caller ⇒ honest wide fallback, NOT a tight 3-bet range"
        );
    }

    #[test]
    fn preflop_rfi_still_random_no_villain_acted_yet() {
        // Preflop RFI: no villain has acted ⇒ range stays "random" even when a
        // villain_position is supplied.
        let hero = hc(Rank::Ace, Suit::Spades, Rank::King, Suit::Diamonds);
        let mut req = preflop_req(hero, PositionBucket::BTN, PreflopLine::Rfi);
        req.villain_position = Some(PositionBucket::BB);
        let a = analyze_spot(&req).unwrap();
        assert_eq!(a.villain_range.code, "random");
        assert_eq!(a.villain_range.combos, 1326);
        assert_eq!(a.villain_range.pct, 100.0);
    }

    #[test]
    fn preflop_recommendation_consistent_with_own_grid_cell() {
        // Forward-compat invariant lock: with the current binary charts (all
        // freq=1.0, no mixed strategies), the hero's recommended action MUST agree
        // with hero's own grid cell action — in-range hand ⇒ cell Raise +
        // recommended Raise; out-of-range hand ⇒ cell Fold + recommended Fold.
        // Locks the invariant BEFORE mixed-strategy charts ever land.

        // In-range: AA UTG RFI.
        let in_range = hc(Rank::Ace, Suit::Spades, Rank::Ace, Suit::Hearts);
        let a = analyze_spot(&preflop_req(
            in_range,
            PositionBucket::UTG,
            PreflopLine::Rfi,
        ))
        .unwrap();
        let aa = a
            .preflop_grid
            .as_ref()
            .unwrap()
            .iter()
            .find(|c| c.is_hero)
            .unwrap();
        assert_eq!(aa.action, GridAction::Raise);
        assert_eq!(a.recommended, SolverAction::Raise);

        // Out-of-range: 72o UTG RFI.
        let out_range = hc(Rank::Seven, Suit::Spades, Rank::Two, Suit::Hearts);
        let b = analyze_spot(&preflop_req(
            out_range,
            PositionBucket::UTG,
            PreflopLine::Rfi,
        ))
        .unwrap();
        let h72 = b
            .preflop_grid
            .as_ref()
            .unwrap()
            .iter()
            .find(|c| c.is_hero)
            .unwrap();
        assert_eq!(h72.action, GridAction::Fold);
        assert_eq!(b.recommended, SolverAction::Fold);
    }

    /// F2 (2026-06-25): a pure vs_4bet continue cell (AA CO vs_4bet, freq 1.0) must
    /// surface the model's actual action — a 5-bet JAM. The HEADLINE must be
    /// `AllIn` (NOT the old `Call`, which told premiums to flat a 4-bet at 100bb),
    /// and the GRID must paint it `Raise` (a jam is an aggressive raise; the 4-color
    /// grid has no all-in swatch). Both are aggressive + mutually consistent —
    /// never the contradictory "headline Call / grid something-else" the user saw.
    #[test]
    fn vs4bet_pure_premium_headline_is_jam_grid_is_aggressive() {
        let aa = hc(Rank::Ace, Suit::Spades, Rank::Ace, Suit::Hearts);
        let a = analyze_spot(&preflop_req(aa, PositionBucket::CO, PreflopLine::Vs4bet)).unwrap();
        // Headline: a 5-bet jam, NOT a flat-call.
        assert_eq!(
            a.recommended,
            SolverAction::AllIn,
            "vs_4bet premium continue must be a 5-bet JAM (AllIn), never a flat Call"
        );
        // Grid: the hero's own cell is the aggressive `Raise` (jam), not `Call`.
        let cell = a
            .preflop_grid
            .as_ref()
            .unwrap()
            .iter()
            .find(|c| c.is_hero)
            .unwrap();
        assert_eq!(
            cell.action,
            GridAction::Raise,
            "pure vs_4bet continue cell must paint aggressive Raise (jam), never Call"
        );
        assert_ne!(
            cell.action,
            GridAction::Call,
            "the grid must NOT tell the user to flat-call a 4-bet (F2 regression)"
        );

        // Grading: jamming AA vs a 4-bet (the recommendation) is Good; flat-CALLING
        // it — never part of the model's strategy — must NOT be graded Good.
        let mut req = preflop_req(aa, PositionBucket::CO, PreflopLine::Vs4bet);
        req.hero_action = Some(SolverAction::AllIn);
        assert_eq!(
            analyze_spot(&req).unwrap().verdict,
            Some(SolverVerdict::Good),
            "jamming AA vs a 4-bet matches the recommendation ⇒ Good"
        );
    }

    #[test]
    fn verdict_only_present_when_hero_action_given() {
        let hero = hc(Rank::Ace, Suit::Spades, Rank::Ace, Suit::Hearts);
        let mut req = preflop_req(hero, PositionBucket::UTG, PreflopLine::Rfi);
        // No hero action → no verdict.
        assert!(analyze_spot(&req).unwrap().verdict.is_none());
        // Hero raises AA (= recommendation) → Good.
        req.hero_action = Some(SolverAction::Raise);
        assert_eq!(
            analyze_spot(&req).unwrap().verdict,
            Some(SolverVerdict::Good)
        );
    }

    #[test]
    fn determinism_same_seed_same_analysis() {
        let hero = hc(Rank::Ace, Suit::Hearts, Rank::Queen, Suit::Hearts);
        let board = BoardCards {
            flop: Some([
                Card::new(Rank::Jack, Suit::Hearts),
                Card::new(Rank::Five, Suit::Hearts),
                Card::new(Rank::Two, Suit::Clubs),
            ]),
            turn: None,
            river: None,
        };
        let req = SpotRequest {
            table_size: TableSize::SixMax,
            hero_position: PositionBucket::BTN,
            villain_position: Some(PositionBucket::CO),
            stack_bb: 100,
            street: Street::Flop,
            hero,
            board,
            pot_bb: 6.0,
            to_call_bb: 2.5,
            preflop_line: PreflopLine::FacingOpen,
            hero_facing: HeroFacing::Bet,
            hero_action: Some(SolverAction::Call),
            villain_hand: None,
            advice_seed: 12345,
            reveal_seed: 12345,
        };
        let a1 = analyze_spot(&req).unwrap();
        let a2 = analyze_spot(&req).unwrap();
        assert_eq!(a1.equity_pct, a2.equity_pct);
        assert_eq!(a1.recommended, a2.recommended);
        assert_eq!(a1.verdict, a2.verdict);
        assert_eq!(a1.ev_margin_bb, a2.ev_margin_bb);
    }

    #[test]
    fn invalid_cards_rejected() {
        // Duplicate hero cards.
        let hero = hc(Rank::Ace, Suit::Spades, Rank::Ace, Suit::Spades);
        let req = preflop_req(hero, PositionBucket::UTG, PreflopLine::Rfi);
        assert_eq!(analyze_spot(&req).unwrap_err(), SolverError::InvalidCards);
    }

    #[test]
    fn board_count_must_match_street() {
        // Street is Flop but board is empty → invalid.
        let hero = hc(Rank::Ace, Suit::Spades, Rank::King, Suit::Spades);
        let mut req = preflop_req(hero, PositionBucket::UTG, PreflopLine::Rfi);
        req.street = Street::Flop; // board still empty
        assert_eq!(analyze_spot(&req).unwrap_err(), SolverError::InvalidCards);
    }

    #[test]
    fn pot_odds_pct_rounds_correctly() {
        // to_call 2 into pot 4 → 2/6 = 33%.
        assert_eq!(pot_odds_pct(4.0, 2.0), 33);
        // No to_call → 0.
        assert_eq!(pot_odds_pct(10.0, 0.0), 0);
    }

    #[test]
    fn wire_shape_serializes_snake_case_per_spec() {
        // Locks the §2 wire contract: snake_case enum values + the documented
        // field names. (server/client mirror this exact shape.)
        let hero = hc(Rank::Ace, Suit::Spades, Rank::King, Suit::Spades);
        let mut req = preflop_req(hero, PositionBucket::BTN, PreflopLine::Rfi);
        req.hero_action = Some(SolverAction::Raise);
        let a = analyze_spot(&req).unwrap();
        let v: serde_json::Value = serde_json::to_value(&a).unwrap();
        assert_eq!(v["method"], "cfr_equity_realization");
        assert_eq!(v["recommended"], "raise");
        assert_eq!(v["verdict"], "good");
        assert!(v["preflop_grid"].is_array());
        assert_eq!(v["preflop_grid"].as_array().unwrap().len(), 169);
        // hand_class is a snake_case HandStrength string.
        assert!(v["hand_class"].is_string());
        // villain_range summary object present with code/combos/pct.
        assert!(v["villain_range"]["code"].is_string());
        assert!(v["villain_range"]["combos"].is_number());
        assert!(v["villain_range"]["pct"].is_number());
        // reason_code is snake_case.
        assert!(v["reason_code"].is_string());
        // a grid cell mirrors {hand_key, action, freq, is_hero}.
        let cell = &v["preflop_grid"][0];
        assert!(cell["hand_key"].is_string());
        assert!(cell["action"].is_string());
        assert!(cell["freq"].is_number());
        assert!(cell["is_hero"].is_boolean());
        // exact_equity_pct is present and serializes to null when no villain
        // hand was supplied (the field must exist so the client mirror sees it).
        assert!(
            v.as_object().unwrap().contains_key("exact_equity_pct"),
            "exact_equity_pct field must always be present in the wire shape"
        );
        assert!(v["exact_equity_pct"].is_null());
    }

    #[test]
    fn grid_maps_freq_to_actions() {
        assert_eq!(grid_action(PreflopLine::Rfi, 1.0), GridAction::Raise);
        assert_eq!(grid_action(PreflopLine::Rfi, 0.0), GridAction::Fold);
        assert_eq!(grid_action(PreflopLine::Rfi, 0.5), GridAction::Mix);
        // F2: a pure vs_4bet continue is a 5-bet JAM (all-in raise), painted as the
        // aggressive `Raise` action — NOT the contradictory `Call` it was before
        // (the headline says AllIn; the grid's 4-action legend has no all-in swatch).
        assert_eq!(grid_action(PreflopLine::Vs4bet, 1.0), GridAction::Raise);
    }

    // -----------------------------------------------------------------------
    // villain_hand exact-equity reveal + analyze_replay (对局复盘)
    // -----------------------------------------------------------------------

    #[test]
    fn reveal_seed_does_not_leak_into_advice() {
        // F1 regression (CONTRACT): the reveal channel (`reveal_seed` /
        // `villain_hand`) must NOT perturb the advice channel. Hold the spot fixed
        // and the advice_seed fixed; vary ONLY the reveal_seed AND the villain
        // hand across many values (what the server would do for different revealed
        // villain hands). The range equity_pct / recommendation / verdict / reason
        // MUST be byte-identical every time — only exact_equity_pct may move.
        let hero = hc(Rank::Ace, Suit::Hearts, Rank::Queen, Suit::Hearts);
        let board = BoardCards {
            flop: Some([
                Card::new(Rank::Jack, Suit::Hearts),
                Card::new(Rank::Five, Suit::Hearts),
                Card::new(Rank::Two, Suit::Clubs),
            ]),
            turn: None,
            river: None,
        };
        let base = SpotRequest {
            table_size: TableSize::SixMax,
            hero_position: PositionBucket::BTN,
            villain_position: Some(PositionBucket::CO),
            stack_bb: 100,
            street: Street::Flop,
            hero,
            board,
            pot_bb: 6.0,
            to_call_bb: 2.5,
            preflop_line: PreflopLine::FacingOpen,
            hero_facing: HeroFacing::Bet,
            hero_action: Some(SolverAction::Call),
            villain_hand: None,
            advice_seed: 555,
            reveal_seed: 555,
        };
        // Reference advice computed with NO villain hand at all.
        let reference = analyze_spot(&base).unwrap();

        // A handful of distinct villain holdings that don't conflict with hero/board.
        let villains = [
            hc(Rank::King, Suit::Spades, Rank::King, Suit::Diamonds),
            hc(Rank::Nine, Suit::Spades, Rank::Eight, Suit::Diamonds),
            hc(Rank::Ace, Suit::Spades, Rank::Ace, Suit::Diamonds),
            hc(Rank::Seven, Suit::Diamonds, Rank::Three, Suit::Spades),
        ];
        for (i, v) in villains.iter().enumerate() {
            // Each revealed hand lands on a DIFFERENT reveal_seed (as the server's
            // villain-hashing seed would), but the advice_seed is unchanged.
            let req = SpotRequest {
                villain_hand: Some(*v),
                reveal_seed: 9000 + i as u64,
                ..base.clone()
            };
            let a = analyze_spot(&req).unwrap();
            assert_eq!(
                a.equity_pct, reference.equity_pct,
                "range equity_pct must be independent of the revealed villain hand / reveal_seed"
            );
            assert_eq!(a.recommended, reference.recommended);
            assert_eq!(a.verdict, reference.verdict);
            assert_eq!(a.reason_code, reference.reason_code);
            assert_eq!(a.ev_margin_bb, reference.ev_margin_bb);
            // The reveal number IS present and (for these different hands) can vary.
            assert!(a.exact_equity_pct.is_some());
        }
    }

    #[test]
    fn exact_equity_present_with_villain_hand_22_vs_aq() {
        // Hero 22 vs villain AQ preflop. The EXACT reveal must be Some and land
        // in a sane band (22 vs AQo/s is a ~50% coin-flip; allow a wide [35,65]
        // window so we don't over-assert on MC noise), while the range-based
        // equity_pct stays populated and untouched by the villain hand.
        let hero = hc(Rank::Two, Suit::Spades, Rank::Two, Suit::Hearts);
        let villain = hc(Rank::Ace, Suit::Clubs, Rank::Queen, Suit::Diamonds);
        let mut req = preflop_req(hero, PositionBucket::BTN, PreflopLine::Rfi);
        req.villain_hand = Some(villain);
        let a = analyze_spot(&req).unwrap();
        let exact = a
            .exact_equity_pct
            .expect("villain hand ⇒ exact equity Some");
        assert!(
            (35..=65).contains(&exact),
            "22 vs AQ exact equity should be a coin-flip-ish band, got {exact}"
        );
        // Range equity still computed (and independent of the reveal).
        assert!(a.equity_pct <= 100);
    }

    #[test]
    fn exact_equity_none_without_villain_hand() {
        // Existing analyze_spot behavior preserved: no villain hand ⇒ None.
        let hero = hc(Rank::Two, Suit::Spades, Rank::Two, Suit::Hearts);
        let req = preflop_req(hero, PositionBucket::BTN, PreflopLine::Rfi);
        let a = analyze_spot(&req).unwrap();
        assert!(a.exact_equity_pct.is_none());
    }

    #[test]
    fn villain_hand_conflicting_with_hero_is_invalid() {
        // Villain shares a card with hero → InvalidCards.
        let hero = hc(Rank::Ace, Suit::Spades, Rank::King, Suit::Spades);
        let villain = hc(Rank::Ace, Suit::Spades, Rank::Queen, Suit::Diamonds);
        let mut req = preflop_req(hero, PositionBucket::BTN, PreflopLine::Rfi);
        req.villain_hand = Some(villain);
        assert_eq!(analyze_spot(&req).unwrap_err(), SolverError::InvalidCards);
    }

    #[test]
    fn villain_hand_conflicting_with_board_is_invalid() {
        // Villain shares a card with the board → InvalidCards (flop spot).
        let hero = hc(Rank::Ace, Suit::Spades, Rank::King, Suit::Spades);
        let board = board3(
            Card::new(Rank::Two, Suit::Clubs),
            Card::new(Rank::Seven, Suit::Diamonds),
            Card::new(Rank::Nine, Suit::Hearts),
        );
        let req = SpotRequest {
            table_size: TableSize::SixMax,
            hero_position: PositionBucket::BTN,
            villain_position: Some(PositionBucket::BB),
            stack_bb: 100,
            street: Street::Flop,
            hero,
            board,
            pot_bb: 6.0,
            to_call_bb: 0.0,
            preflop_line: PreflopLine::Rfi,
            hero_facing: HeroFacing::FirstToAct,
            hero_action: None,
            // Villain holds the 2c that is already on the flop.
            villain_hand: Some(hc(Rank::Two, Suit::Clubs, Rank::Three, Suit::Diamonds)),
            advice_seed: 1,
            reveal_seed: 1,
        };
        assert_eq!(analyze_spot(&req).unwrap_err(), SolverError::InvalidCards);
    }

    /// Build a 4-street replay over a full 5-card board: hero AhKh, villain QdQs.
    fn full_replay_req(seed: u64) -> ReplayRequest {
        let hero = hc(Rank::Ace, Suit::Hearts, Rank::King, Suit::Hearts);
        let villain = hc(Rank::Queen, Suit::Diamonds, Rank::Queen, Suit::Spades);
        let board = BoardCards {
            flop: Some([
                Card::new(Rank::King, Suit::Clubs),
                Card::new(Rank::Seven, Suit::Hearts),
                Card::new(Rank::Two, Suit::Hearts),
            ]),
            turn: Some(Card::new(Rank::Ten, Suit::Hearts)), // hero makes the nut flush
            river: Some(Card::new(Rank::Three, Suit::Spades)),
        };
        let street = || ReplayStreetInput {
            pot_bb: 6.0,
            to_call_bb: 0.0,
            hero_facing: HeroFacing::FirstToAct,
            hero_action: None,
        };
        ReplayRequest {
            table_size: TableSize::SixMax,
            hero_position: PositionBucket::BTN,
            villain_position: Some(PositionBucket::BB),
            stack_bb: 100,
            hero,
            villain,
            board,
            preflop_line: PreflopLine::Rfi,
            streets: vec![street(), street(), street(), street()],
            advice_seed: seed,
            // Distinct reveal seed (exercises the advice/reveal split).
            reveal_seed: seed.wrapping_add(0xA11CE),
        }
    }

    #[test]
    fn analyze_replay_returns_one_entry_per_street_in_order() {
        let req = full_replay_req(100);
        let out = analyze_replay(&req).unwrap();
        assert_eq!(out.len(), 4);
        assert_eq!(out[0].street, Street::Preflop);
        assert_eq!(out[1].street, Street::Flop);
        assert_eq!(out[2].street, Street::Turn);
        assert_eq!(out[3].street, Street::River);
        // Every street carries the exact-equity reveal.
        for s in &out {
            assert!(
                s.analysis.exact_equity_pct.is_some(),
                "{:?} must carry exact_equity_pct",
                s.street
            );
        }
        // Hero AhKh turns the nut flush; by the river the exact equity vs QQ is
        // 100% (made flush beats a set). Don't over-assert earlier streets.
        assert_eq!(out[3].analysis.exact_equity_pct, Some(100));
        // Preflop entry emits the grid; postflop entries do not.
        assert!(out[0].analysis.preflop_grid.is_some());
        assert!(out[1].analysis.preflop_grid.is_none());
        assert!(out[3].analysis.preflop_grid.is_none());
        // Honesty: preflop is CfrEquityRealization, postflop EquityHeuristic.
        assert_eq!(out[0].analysis.method, SolveMethod::CfrEquityRealization);
        assert_eq!(out[1].analysis.method, SolveMethod::EquityHeuristic);
    }

    #[test]
    fn analyze_replay_is_deterministic_byte_identical() {
        let a = analyze_replay(&full_replay_req(2024)).unwrap();
        let b = analyze_replay(&full_replay_req(2024)).unwrap();
        let ja = serde_json::to_vec(&a).unwrap();
        let jb = serde_json::to_vec(&b).unwrap();
        assert_eq!(
            ja, jb,
            "same ReplayRequest must produce byte-identical output"
        );
    }

    /// A representative spot for the §5(a) golden-byte guard: a postflop spot with
    /// a Range advice channel + a Known reveal channel (both `early_stop: None`).
    fn golden_spot_req() -> SpotRequest {
        SpotRequest {
            table_size: TableSize::SixMax,
            hero_position: PositionBucket::BTN,
            villain_position: Some(PositionBucket::BB),
            stack_bb: 100,
            street: Street::Flop,
            hero: hc(Rank::Ace, Suit::Spades, Rank::King, Suit::Spades),
            board: board3(
                Card::new(Rank::Ace, Suit::Hearts),
                Card::new(Rank::King, Suit::Diamonds),
                Card::new(Rank::Two, Suit::Clubs),
            ),
            pot_bb: 6.0,
            to_call_bb: 4.0,
            preflop_line: PreflopLine::Rfi,
            hero_facing: HeroFacing::Bet,
            hero_action: Some(SolverAction::Call),
            villain_hand: Some(hc(Rank::Queen, Suit::Diamonds, Rank::Jack, Suit::Diamonds)),
            advice_seed: 31337,
            reveal_seed: 0xBEEF,
        }
    }

    /// A preflop spot for the §5(a) golden-byte guard (grid + Range advice).
    fn golden_preflop_req() -> SpotRequest {
        SpotRequest {
            hero_action: Some(SolverAction::Raise),
            ..preflop_req(
                hc(Rank::Ace, Suit::Spades, Rank::Queen, Suit::Spades),
                PositionBucket::CO,
                PreflopLine::Rfi,
            )
        }
    }

    /// §5(a): ADR-082 must NOT perturb the public-spot/replay output. We assert
    /// `serde_json::to_vec(&analyze_replay(..))` equals a FROZEN golden byte
    /// vector committed below — so the guard survives even if both runs share the
    /// new code (a self-consistency check could pass while still having shifted).
    /// The non-coach (`None`) path is byte-for-byte the pre-ADR-082 behaviour.
    #[test]
    fn analyze_replay_byte_identical_across_adr082() {
        let out = analyze_replay(&full_replay_req(2024)).unwrap();
        let bytes = serde_json::to_vec(&out).unwrap();
        // FROZEN GOLDEN — captured from the pre-ADR-082 `None`-path behaviour.
        let golden = include_bytes!("testdata/adr082_replay_golden.json");
        assert_eq!(
            bytes.as_slice(),
            golden.as_slice(),
            "analyze_replay output drifted from the frozen pre-ADR-082 golden (F4 byte-identical guard)"
        );
    }

    /// §5(a): same frozen-golden guard for a representative `SpotRequest` — both
    /// a postflop spot (Range advice + Known reveal) and a preflop spot (grid).
    #[test]
    fn analyze_spot_byte_identical_across_adr082() {
        let post = serde_json::to_vec(&analyze_spot(&golden_spot_req()).unwrap()).unwrap();
        let golden_post = include_bytes!("testdata/adr082_spot_postflop_golden.json");
        assert_eq!(
            post.as_slice(),
            golden_post.as_slice(),
            "postflop analyze_spot drifted from frozen pre-ADR-082 golden"
        );

        let pre = serde_json::to_vec(&analyze_spot(&golden_preflop_req()).unwrap()).unwrap();
        let golden_pre = include_bytes!("testdata/adr082_spot_preflop_golden.json");
        assert_eq!(
            pre.as_slice(),
            golden_pre.as_slice(),
            "preflop analyze_spot drifted from frozen pre-ADR-082 golden"
        );
    }

    #[test]
    fn analyze_replay_rejects_villain_conflicting_with_board() {
        let mut req = full_replay_req(1);
        // Villain holds the Kc that is on the flop → InvalidCards from analyze_spot.
        req.villain = hc(Rank::King, Suit::Clubs, Rank::Five, Suit::Diamonds);
        assert_eq!(analyze_replay(&req).unwrap_err(), SolverError::InvalidCards);
    }

    #[test]
    fn analyze_replay_board_count_must_match_last_street() {
        // 4 streets (through river) but only a 3-card board → InvalidCards.
        let mut req = full_replay_req(1);
        req.board = BoardCards {
            flop: Some([
                Card::new(Rank::King, Suit::Clubs),
                Card::new(Rank::Seven, Suit::Hearts),
                Card::new(Rank::Two, Suit::Hearts),
            ]),
            turn: None,
            river: None,
        };
        assert_eq!(analyze_replay(&req).unwrap_err(), SolverError::InvalidCards);
    }

    #[test]
    fn analyze_replay_two_streets_needs_flop_only_board() {
        // 2 streets (preflop + flop) require exactly a 3-card board.
        let hero = hc(Rank::Ace, Suit::Hearts, Rank::King, Suit::Hearts);
        let villain = hc(Rank::Queen, Suit::Diamonds, Rank::Queen, Suit::Spades);
        let board = board3(
            Card::new(Rank::King, Suit::Clubs),
            Card::new(Rank::Seven, Suit::Hearts),
            Card::new(Rank::Two, Suit::Spades),
        );
        let street = || ReplayStreetInput {
            pot_bb: 6.0,
            to_call_bb: 0.0,
            hero_facing: HeroFacing::FirstToAct,
            hero_action: None,
        };
        let req = ReplayRequest {
            table_size: TableSize::SixMax,
            hero_position: PositionBucket::BTN,
            villain_position: Some(PositionBucket::BB),
            stack_bb: 100,
            hero,
            villain,
            board,
            preflop_line: PreflopLine::Rfi,
            streets: vec![street(), street()],
            advice_seed: 7,
            reveal_seed: 7,
        };
        let out = analyze_replay(&req).unwrap();
        assert_eq!(out.len(), 2);
        assert_eq!(out[0].street, Street::Preflop);
        assert_eq!(out[1].street, Street::Flop);
        // Flop entry sees only the 3-card board prefix.
        assert!(out[1].analysis.exact_equity_pct.is_some());
    }

    #[test]
    fn analyze_replay_rejects_zero_and_too_many_streets() {
        let mut req = full_replay_req(1);
        req.streets = vec![];
        assert_eq!(analyze_replay(&req).unwrap_err(), SolverError::InvalidCards);

        let mut req = full_replay_req(1);
        req.streets = vec![
            ReplayStreetInput {
                pot_bb: 1.5,
                to_call_bb: 0.0,
                hero_facing: HeroFacing::FirstToAct,
                hero_action: None,
            };
            5
        ];
        assert_eq!(analyze_replay(&req).unwrap_err(), SolverError::InvalidCards);
    }

    #[test]
    fn replay_wire_shape_has_street_and_analysis() {
        let req = full_replay_req(3);
        let out = analyze_replay(&req).unwrap();
        let v: serde_json::Value = serde_json::to_value(&out).unwrap();
        assert!(v.is_array());
        let first = &v[0];
        // Street serializes snake_case lowercase.
        assert_eq!(first["street"], "preflop");
        // analysis nests the full SpotAnalysis incl. exact_equity_pct.
        assert!(first["analysis"]["recommended"].is_string());
        assert!(first["analysis"]["exact_equity_pct"].is_number());
    }
}
