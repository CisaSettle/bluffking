//! Closed-form poker-MATH helpers for the free poker-tools surface (Wave 1).
//!
//! Pure, allocation-light, fully deterministic — no IO, no RNG, no game state.
//! These power the stateless `/api/tools/poker/{outs,pot-odds}` endpoints (the
//! equity tool reuses [`crate::solver::equity`]). Nothing here deals, resolves,
//! or persists a playable hand — it is reference arithmetic only.
//!
//! Three concerns live here:
//!   1. [`outs_to_odds`] — hypergeometric "hit by river / next card" odds for a
//!      known number of outs, plus the 2/4 rule of thumb.
//!   2. [`detect_draws`] — best-effort draw auto-detection (flush draw, open-
//!      ended straight draw, gutshot) returning a conservative outs count. It
//!      NEVER overcounts; ambiguous spots return `[]` / 0 so the caller can
//!      require an explicit outs value.
//!   3. [`pot_odds`] — pot-odds / required-equity / call-or-fold arithmetic.

use crate::card::{Card, Rank, Suit};
use crate::hand::{BoardCards, HoleCards};

// ===========================================================================
// 1. Outs → odds (hypergeometric)
// ===========================================================================

/// Which street the outs calculation is for, derived from board card count.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OutsStreet {
    /// 3 board cards → two cards still to come (turn + river).
    Flop,
    /// 4 board cards → one card still to come (river).
    Turn,
}

impl OutsStreet {
    /// Wire token: `"flop"` / `"turn"`.
    pub fn as_str(self) -> &'static str {
        match self {
            OutsStreet::Flop => "flop",
            OutsStreet::Turn => "turn",
        }
    }

    /// Number of community cards still to be dealt on this street.
    pub fn cards_to_come(self) -> u32 {
        match self {
            OutsStreet::Flop => 2,
            OutsStreet::Turn => 1,
        }
    }

    /// Derive the street from a board card count. Only 3 (flop) and 4 (turn) are
    /// valid for the outs tool; 0/5/other return `None`.
    pub fn from_board_count(count: usize) -> Option<OutsStreet> {
        match count {
            3 => Some(OutsStreet::Flop),
            4 => Some(OutsStreet::Turn),
            _ => None,
        }
    }
}

/// Result of an outs → odds calculation.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct OutsOdds {
    /// The outs count the calculation used (explicit or detected).
    pub outs: u32,
    /// Which street (drives cards-to-come and the 2/4 multiplier).
    pub street: OutsStreet,
    /// Number of unseen cards (52 − 2 hero − board).
    pub unseen: u32,
    /// Rule-of-thumb percentage: flop = outs×4 (capped), turn = outs×2.
    pub rule_2_4_pct: u32,
    /// EXACT probability of hitting at least one out by the river, as a
    /// percentage rounded to one decimal place.
    pub exact_by_river_pct: f64,
    /// EXACT probability of hitting an out on the very next card, as a
    /// percentage rounded to one decimal place.
    pub exact_next_card_pct: f64,
}

/// Compute hypergeometric hit odds for `outs` outs on `street` with `unseen`
/// unseen cards.
///
/// - next card  = outs / unseen
/// - by river (flop, 2 cards) = 1 − C(unseen−outs, 2) / C(unseen, 2)
/// - by river (turn, 1 card)  = outs / unseen (same as next card)
///
/// `outs` is clamped to `[0, unseen]` so an absurd caller value can never make
/// the probability exceed 100% or the combinatorics underflow. Percentages are
/// rounded to one decimal place; the 2/4-rule value is a whole percent (the flop
/// ×4 estimate is capped so it can't print a nonsensical >100%).
pub fn outs_to_odds(outs: u32, street: OutsStreet, unseen: u32) -> OutsOdds {
    let unseen = unseen.max(1); // never divide by zero
    let outs = outs.min(unseen);

    let next_card = outs as f64 / unseen as f64;

    let by_river = match street {
        OutsStreet::Turn => next_card,
        OutsStreet::Flop => {
            // 1 − P(miss both) = 1 − [C(unseen−outs, 2) / C(unseen, 2)].
            let miss = unseen.saturating_sub(outs);
            let miss_both = comb2(miss) / comb2(unseen);
            1.0 - miss_both
        }
    };

    // 2/4 rule: flop ×4, turn ×2. The ×4 flop estimate overshoots the true
    // hypergeometric value for large out counts, so cap the printed rule value
    // so it never claims an impossible >100%.
    let rule_mult = match street {
        OutsStreet::Flop => 4,
        OutsStreet::Turn => 2,
    };
    let rule_2_4_pct = (outs * rule_mult).min(100);

    OutsOdds {
        outs,
        street,
        unseen,
        rule_2_4_pct,
        exact_by_river_pct: round1(by_river * 100.0),
        exact_next_card_pct: round1(next_card * 100.0),
    }
}

/// C(n, 2) as an `f64` ( = n·(n−1)/2 ). Returns `0.0` for n < 2.
fn comb2(n: u32) -> f64 {
    if n < 2 {
        0.0
    } else {
        (n as f64) * ((n - 1) as f64) / 2.0
    }
}

/// Round a percentage to one decimal place.
fn round1(pct: f64) -> f64 {
    (pct * 10.0).round() / 10.0
}

// ===========================================================================
// 2. Draw auto-detection (best-effort, conservative)
// ===========================================================================

/// A detected drawing pattern and the conservative outs it implies.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DrawKind {
    /// Exactly four cards of one suit across hero + board → 9 outs to a flush.
    FlushDraw,
    /// Four to an open-ended straight (two ways to complete) → 8 outs.
    OpenEndedStraightDraw,
    /// Four to an inside / one-gap straight (one way to complete) → 4 outs.
    Gutshot,
}

impl DrawKind {
    /// Wire token for the `detected` array.
    pub fn as_str(self) -> &'static str {
        match self {
            DrawKind::FlushDraw => "flush_draw",
            DrawKind::OpenEndedStraightDraw => "open_ended_straight_draw",
            DrawKind::Gutshot => "gutshot",
        }
    }

    /// Conservative outs this draw contributes.
    pub fn outs(self) -> u32 {
        match self {
            DrawKind::FlushDraw => 9,
            DrawKind::OpenEndedStraightDraw => 8,
            DrawKind::Gutshot => 4,
        }
    }
}

/// Outcome of [`detect_draws`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DetectedDraws {
    /// Detected draw kinds, in a stable order (flush, then straight).
    pub draws: Vec<DrawKind>,
    /// Conservative total outs implied by the detected draws.
    ///
    /// To avoid OVERCOUNTING (the same out helping two draws), when both a flush
    /// draw and a straight draw are present we take the LARGER single draw's outs
    /// rather than naively summing — a deliberately conservative floor. When only
    /// straight draws are present we take the strongest (OESD over gutshot). A
    /// flush draw alone is 9; no draw is 0.
    pub outs: u32,
}

/// Best-effort draw detection over hero + board.
///
/// Designed to NEVER overcount. It detects three unambiguous patterns:
///   * flush draw — exactly four cards of one suit (a made 5-flush is not a draw)
///   * open-ended straight draw — four consecutive ranks with room to extend
///     both ends (8 outs)
///   * gutshot — four ranks to a five-card straight window with exactly one gap
///     (4 outs)
///
/// Only meaningful on the flop or turn (3 or 4 board cards). For any other board
/// size, or when nothing clean is found, returns an empty draw list with 0 outs
/// — the caller then requires an explicit outs value. Made hands (already a
/// straight/flush) are NOT reported as draws.
///
/// Straight detection treats the Ace as both high and low (A-2-3-4-5 wheel and
/// T-J-Q-K-A). Pairs/duplicate ranks collapse for straight purposes (a straight
/// uses distinct ranks). Returned outs combine flush + straight conservatively
/// (see [`DetectedDraws::outs`]).
pub fn detect_draws(hero: &HoleCards, board: &BoardCards) -> DetectedDraws {
    let mut cards: Vec<Card> = Vec::with_capacity(6);
    cards.push(hero.card1);
    cards.push(hero.card2);
    cards.extend(board.all_cards());

    // Only flop (5 cards total) or turn (6 cards total) are meaningful.
    let board_count = board.count();
    if board_count != 3 && board_count != 4 {
        return DetectedDraws {
            draws: Vec::new(),
            outs: 0,
        };
    }

    let mut draws = Vec::new();

    if has_flush_draw(&cards) {
        draws.push(DrawKind::FlushDraw);
    }

    if let Some(kind) = detect_straight_draw(&cards) {
        draws.push(kind);
    }

    // Conservative outs: never sum flush + straight (shared completing cards can
    // double-count). Take the single largest detected draw's outs.
    let outs = draws.iter().map(|d| d.outs()).max().unwrap_or(0);

    DetectedDraws { draws, outs }
}

/// True iff exactly four cards share one suit (a flush DRAW). Five+ of a suit is
/// a made flush, not a draw, so it does not count.
fn has_flush_draw(cards: &[Card]) -> bool {
    for &suit in Suit::ALL.iter() {
        let n = cards.iter().filter(|c| c.suit == suit).count();
        if n == 4 {
            return true;
        }
    }
    false
}

/// Detect the strongest straight draw (OESD > gutshot) among the distinct ranks,
/// or `None` if the hand already makes a straight or has no four-to-a-straight.
fn detect_straight_draw(cards: &[Card]) -> Option<DrawKind> {
    // Rank-presence bitmask, low→high. Bit i (0..=12) = Rank::ALL[i] present.
    // Ace also occupies a virtual "bit -1" via a separate wheel check.
    let mut present = [false; 13];
    for c in cards {
        present[rank_index(c.rank)] = true;
    }
    let ace = present[12];

    // If a made 5-straight already exists, it's not a draw.
    if makes_straight(&present, ace) {
        return None;
    }

    // Scan every 5-rank straight window (including the wheel A-2-3-4-5) and find
    // the best draw: a window we already have 4 of distinct ranks in.
    // Windows are the 10 high-card values 5..=A (top rank = window high).
    let mut best: Option<DrawKind> = None;

    // Standard windows: high card from Six(index 4) up to Ace(index 12). The
    // five-low window (2-3-4-5-6) is the lowest standard run; the A-low wheel is
    // handled separately below. Starting at index 4 keeps `high - 4` ≥ 0.
    for high in 4..=12usize {
        let in_window = |idx: usize| -> bool {
            // window ranks are high-4 ..= high
            present[idx]
        };
        let window_idxs = [high - 4, high - 3, high - 2, high - 1, high];
        let count = window_idxs.iter().filter(|&&i| in_window(i)).count();
        if count == 4 {
            let kind = classify_window(&present, &window_idxs);
            best = Some(stronger(best, kind));
        }
    }

    // Wheel window A-2-3-4-5 (ranks: Ace + Two..Five). Index the Ace specially.
    {
        let wheel = [ace, present[0], present[1], present[2], present[3]];
        let count = wheel.iter().filter(|&&p| p).count();
        if count == 4 {
            // The wheel can only ever be a gutshot for draw purposes here: a true
            // OESD requires open ends, and the wheel's low end (A) and high end
            // (6, which would extend 2-3-4-5-6) are asymmetric. We classify it by
            // which single card is missing — but since one end is the Ace
            // (non-extendable downward), treat a four-to-the-wheel as a GUTSHOT
            // (4 outs), the conservative call.
            best = Some(stronger(best, DrawKind::Gutshot));
        }
    }

    best
}

/// Classify a four-card straight window (exactly one of the five ranks missing)
/// as an OESD or a gutshot.
///
/// A window is an OESD only when the missing rank is at an END of the run AND the
/// run can extend on the OPEN side (i.e. the four held ranks are consecutive and
/// both extension cards exist on the board — represented by the missing card
/// being a true end with a further rank beyond). For the conservative tool we
/// classify by the SHAPE of the four held ranks:
///   * four CONSECUTIVE ranks (e.g. 6-7-8-9) that are NOT bounded by the deck
///     edge on both sides → OESD (8 outs)
///   * otherwise (one internal gap, or an edge-bounded run like A-K-Q-J) →
///     gutshot (4 outs)
fn classify_window(present: &[bool; 13], window: &[usize; 5]) -> DrawKind {
    // Find the single missing index in the window.
    let missing: Vec<usize> = window.iter().copied().filter(|&i| !present[i]).collect();
    if missing.len() != 1 {
        // Shouldn't happen (caller passed a count==4 window), be conservative.
        return DrawKind::Gutshot;
    }
    let miss = missing[0];
    let lo = window[0];
    let hi = window[4];

    // If the missing card is INTERNAL to the window (not at either end), the four
    // held ranks straddle a one-card hole → gutshot.
    if miss != lo && miss != hi {
        return DrawKind::Gutshot;
    }

    // Missing card is at an end ⇒ the four held ranks are consecutive
    // (e.g. window 5-6-7-8-9 missing 9 → held 5-6-7-8). This is an OESD ONLY
    // when BOTH outside ranks exist in the deck so it can complete two ways.
    // The four consecutive held ranks span [held_lo, held_hi]; the two
    // completing ranks are held_lo-1 and held_hi+1.
    let held_hi = if miss == lo {
        window[4] // dropped the low end ⇒ held run is window[1..=4]
    } else {
        window[3] // dropped the high end ⇒ held run is window[0..=3]
    };
    // The LOW end of a four-consecutive run is ALWAYS completable: by a lower rank
    // when one exists, or by the Ace via the wheel A-2-3-4-5 when the low card is a
    // Two. So held 2-3-4-5 is a true OESD (completes with A *or* 6 = 8 outs), not a
    // gutshot. Only the HIGH end can be deck-blocked — nothing extends above the
    // Ace — so a four-run topping out at the Ace (e.g. held J-Q-K-A, completes only
    // with T) is one-way, which we treat as a gutshot.
    if held_hi < 12 {
        DrawKind::OpenEndedStraightDraw
    } else {
        DrawKind::Gutshot
    }
}

/// Pick the stronger of an optional current best and a new draw kind
/// (OESD > Gutshot).
fn stronger(current: Option<DrawKind>, new: DrawKind) -> DrawKind {
    match current {
        Some(DrawKind::OpenEndedStraightDraw) => DrawKind::OpenEndedStraightDraw,
        Some(_) => {
            if new == DrawKind::OpenEndedStraightDraw {
                DrawKind::OpenEndedStraightDraw
            } else {
                DrawKind::Gutshot
            }
        }
        None => new,
    }
}

/// True iff the distinct ranks already make a five-card straight (incl. wheel).
fn makes_straight(present: &[bool; 13], ace: bool) -> bool {
    // Wheel A-2-3-4-5.
    if ace && present[0] && present[1] && present[2] && present[3] {
        return true;
    }
    // Standard 5-in-a-row windows.
    for high in 4..=12usize {
        if (high - 4..=high).all(|i| present[i]) {
            return true;
        }
    }
    false
}

/// Index of a rank in `Rank::ALL` (Two=0 .. Ace=12).
fn rank_index(rank: Rank) -> usize {
    match rank {
        Rank::Two => 0,
        Rank::Three => 1,
        Rank::Four => 2,
        Rank::Five => 3,
        Rank::Six => 4,
        Rank::Seven => 5,
        Rank::Eight => 6,
        Rank::Nine => 7,
        Rank::Ten => 8,
        Rank::Jack => 9,
        Rank::Queen => 10,
        Rank::King => 11,
        Rank::Ace => 12,
    }
}

// ===========================================================================
// 3. Pot odds → call / fold
// ===========================================================================

/// Verdict from comparing hero equity to the required equity.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PotOddsVerdict {
    /// Equity clears the requirement with margin → a +EV call.
    Call,
    /// Equity falls short of the requirement with margin → a fold.
    Fold,
    /// Equity is within ±[`POT_ODDS_MARGIN_PCT`] of the requirement → close.
    Marginal,
}

impl PotOddsVerdict {
    /// Wire token: `"call"` / `"fold"` / `"marginal"`.
    pub fn as_str(self) -> &'static str {
        match self {
            PotOddsVerdict::Call => "call",
            PotOddsVerdict::Fold => "fold",
            PotOddsVerdict::Marginal => "marginal",
        }
    }
}

/// Dead-zone (in equity percentage points) around the break-even point inside
/// which a call is graded `marginal` rather than a firm call/fold.
pub const POT_ODDS_MARGIN_PCT: f64 = 2.0;

/// Result of a pot-odds calculation.
#[derive(Debug, Clone, PartialEq)]
pub struct PotOdds {
    /// Pot before the call.
    pub pot: f64,
    /// Amount hero must call.
    pub to_call: f64,
    /// Pot-odds as a percentage: to_call / (pot + to_call) × 100, rounded to 1dp.
    /// Numerically identical to [`Self::equity_needed_pct`] — both express the
    /// break-even equity — but exposed under both names for the API.
    pub pot_odds_pct: f64,
    /// Equity (%) hero needs to break even on the call, rounded to 1dp.
    pub equity_needed_pct: f64,
    /// Simplified pot-odds ratio string `pot : to_call` (reward : risk), e.g.
    /// `"2:1"`. Consistent with [`Self::equity_needed_pct`] via the identity
    /// `equity_needed = 1 / (X + 1)` — e.g. `2:1` ⇒ 33.3%, `3:1` ⇒ 25%.
    pub ratio: String,
    /// Hero equity supplied by the caller (echoed), if any.
    pub equity_pct: Option<f64>,
    /// Verdict, present only when `equity_pct` was supplied.
    pub verdict: Option<PotOddsVerdict>,
    /// `equity_pct − equity_needed_pct`, rounded to 1dp, present only when
    /// `equity_pct` was supplied. Positive ⇒ profitable margin.
    pub margin_pct: Option<f64>,
}

/// Error from [`pot_odds`] — invalid (non-positive / non-finite) inputs.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PotOddsError {
    /// `pot` was not a positive, finite number.
    InvalidPot,
    /// `to_call` was not a positive, finite number.
    InvalidToCall,
    /// `equity_pct` was supplied but was not a finite value in `[0, 100]`.
    InvalidEquity,
}

/// Compute pot odds / required equity / a call-or-fold verdict.
///
/// `pot` and `to_call` must be positive finite numbers. `equity_pct`, when
/// supplied, must be a finite value in `[0, 100]`; it produces a verdict and a
/// margin. The break-even (required) equity is `to_call / (pot + to_call)`.
///
/// Verdict thresholds use [`POT_ODDS_MARGIN_PCT`]:
///   * margin >  +2pp → `Call`
///   * margin <  −2pp → `Fold`
///   * otherwise      → `Marginal`
pub fn pot_odds(pot: f64, to_call: f64, equity_pct: Option<f64>) -> Result<PotOdds, PotOddsError> {
    if !pot.is_finite() || pot <= 0.0 {
        return Err(PotOddsError::InvalidPot);
    }
    if !to_call.is_finite() || to_call <= 0.0 {
        return Err(PotOddsError::InvalidToCall);
    }
    // Guard against f64 overflow: `pot` and `to_call` are each finite, but their
    // sum can still reach +inf for astronomically large inputs, which would make
    // the required equity / ratio nonsense. Reject rather than emit a bogus value.
    let total = pot + to_call;
    if !total.is_finite() {
        return Err(PotOddsError::InvalidPot);
    }

    let needed = to_call / total * 100.0;
    let needed_r = round1(needed);

    let (echo_equity, verdict, margin) = match equity_pct {
        Some(eq) => {
            if !eq.is_finite() || !(0.0..=100.0).contains(&eq) {
                return Err(PotOddsError::InvalidEquity);
            }
            let margin = eq - needed;
            let v = if margin > POT_ODDS_MARGIN_PCT {
                PotOddsVerdict::Call
            } else if margin < -POT_ODDS_MARGIN_PCT {
                PotOddsVerdict::Fold
            } else {
                PotOddsVerdict::Marginal
            };
            (Some(round1(eq)), Some(v), Some(round1(margin)))
        }
        None => (None, None, None),
    };

    Ok(PotOdds {
        pot,
        to_call,
        pot_odds_pct: needed_r,
        equity_needed_pct: needed_r,
        ratio: simplify_ratio(pot, to_call),
        equity_pct: echo_equity,
        verdict,
        margin_pct: margin,
    })
}

/// Simplify a pot-odds ratio `reward:risk` (`pot : to_call`) to a readable
/// string.
///
/// `reward` is the pot you stand to win, `risk` is your call — the canonical
/// pot-odds framing where `equity_needed = 1 / (ratio + 1)`. When both sides are
/// (near-)integers AND the GCD reduces the ratio to a clean `a:1` (the risk side
/// divides the reward evenly), it prints that — e.g. `150:50` → `"3:1"`,
/// `100:25` → `"4:1"`. Otherwise (non-integer inputs, or a reduction that would
/// leave an ugly coprime `a:b`) it falls back to the `"X:1"` form with the
/// reward-per-unit-risk rounded to 1dp — the form poker players actually read.
fn simplify_ratio(reward: f64, risk: f64) -> String {
    if risk <= 0.0 {
        return "—".to_string();
    }
    // If both are (near-)integers, try to reduce to a clean `a:1` via GCD.
    let reward_i = reward.round();
    let risk_i = risk.round();
    let near_int = (reward - reward_i).abs() < 1e-9 && (risk - risk_i).abs() < 1e-9;
    if near_int && reward_i >= 1.0 && risk_i >= 1.0 {
        let a = reward_i as u64;
        let b = risk_i as u64;
        let g = gcd(a, b);
        if g > 0 && b / g == 1 {
            // Risk side reduces to 1 → clean "a:1".
            return format!("{}:1", a / g);
        }
    }
    // Fallback: express as "X:1" (reward per unit risked, 1dp).
    let x = round1(reward / risk);
    format!("{x}:1")
}

/// Greatest common divisor (Euclid).
fn gcd(mut a: u64, mut b: u64) -> u64 {
    while b != 0 {
        let t = b;
        b = a % b;
        a = t;
    }
    a
}

// ===========================================================================
// 4. Scare-card probability (exact hypergeometric over the run-out)
// ===========================================================================
//
// "What is the chance an over-card / a King / any of {A,K} appears by the
// river?" — pure combinatorics, NO Monte Carlo needed. Given the cards hero can
// already SEE (their hole cards + the current board), the remaining run-out is
// `k = 5 − board_len` cards drawn WITHOUT replacement from the `N` unseen cards.
// `S` of those unseen cards carry a target rank. Then:
//
//     P(none of the set lands) = C(N − S, k) / C(N, k)        (0 when N − S < k)
//     P(at least one lands)    = 1 − P(none)
//
// Everything here is integer-exact until the final ratio — `S`, `N`, `k` are
// small (≤ 52), and the binomials fit in `u128`, so the only floating step is
// the single division at the end. Deterministic, allocation-light.

/// Error from the scare-card calculation — bad/duplicate cards or an empty set.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ScareCardError {
    /// A card appears more than once across the known (hole + board) cards.
    DuplicateCard,
    /// More than 52 cards were supplied, or the run-out would exceed the deck.
    TooManyCards,
    /// The target rank set was empty — nothing to compute a probability for.
    EmptyRankSet,
    /// `cards_to_come` was supplied/derived as a value the deck can't satisfy
    /// (`k > unseen`).
    InvalidCardsToCome,
}

/// Result of a scare-card calculation.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct ScareCardOdds {
    /// EXACT probability that AT LEAST ONE target-rank card lands over the
    /// run-out, as a fraction in `[0, 1]`.
    pub p_at_least_one: f64,
    /// EXACT probability that NONE of the target ranks land, as a fraction in
    /// `[0, 1]`. Always `1 − p_at_least_one`.
    pub p_none: f64,
    /// `S` — number of UNSEEN cards whose rank is in the target set (after
    /// removing any target-rank cards already visible to hero).
    pub matching_unseen: u32,
    /// `N` — total number of unseen cards (`52 − known`).
    pub unseen_total: u32,
    /// `k` — number of cards still to come.
    pub cards_to_come: u32,
}

/// Compute the exact probability that a card of one of `target_ranks` appears
/// over the remaining run-out, given the cards hero can already see.
///
/// `known` is hero's hole cards plus the current board (every card removed from
/// the unseen pool). `target_ranks` is the set of "scare" ranks to watch for
/// (deduped internally, so passing `[Ace, Ace]` is the same as `[Ace]`).
/// `cards_to_come` is `k` — the number of board cards still to be dealt.
///
/// For the standard flop→river spot pass `known = 2 hole + 3 flop` (5 cards) and
/// `cards_to_come = 2`. See [`scare_card_from_board`] for the convenience wrapper
/// that derives `k` from the board.
///
/// # Errors
/// * [`ScareCardError::EmptyRankSet`] — `target_ranks` is empty.
/// * [`ScareCardError::DuplicateCard`] — a card appears twice in `known`.
/// * [`ScareCardError::TooManyCards`] — `known` exceeds 52 cards.
/// * [`ScareCardError::InvalidCardsToCome`] — `k` exceeds the unseen pool.
///
/// # Edge cases
/// * `k == 0` (river already complete) → `p_at_least_one == 0.0`, `p_none == 1.0`.
/// * `S == 0` (no matching card left in the deck) → `p_at_least_one == 0.0`.
/// * `N − S < k` (every remaining draw MUST include a target rank) →
///   `p_at_least_one == 1.0`.
pub fn scare_card(
    known: &[Card],
    target_ranks: &[Rank],
    cards_to_come: u32,
) -> Result<ScareCardOdds, ScareCardError> {
    if target_ranks.is_empty() {
        return Err(ScareCardError::EmptyRankSet);
    }
    if known.len() > 52 {
        return Err(ScareCardError::TooManyCards);
    }
    if has_duplicate_cards(known) {
        return Err(ScareCardError::DuplicateCard);
    }

    // Dedupe the target ranks → distinct rank count drives the deck total of 4×.
    let mut target_present = [false; 13];
    for r in target_ranks {
        target_present[rank_index(*r)] = true;
    }
    let distinct_targets = target_present.iter().filter(|&&p| p).count() as u32;

    // N = unseen pool = 52 − known.
    let n = 52u32 - known.len() as u32;

    // k cannot exceed the unseen pool — you can't deal more cards than remain.
    let k = cards_to_come;
    if k > n {
        return Err(ScareCardError::InvalidCardsToCome);
    }

    // S = unseen target-rank cards = (4 per distinct rank) − (target-rank cards
    // hero already sees). A target rank hero HOLDS is a blocker: it removes one
    // of that rank from the unseen pool, lowering S (the spec's blocker case).
    let known_targets = known
        .iter()
        .filter(|c| target_present[rank_index(c.rank)])
        .count() as u32;
    let s = 4 * distinct_targets - known_targets;

    // P(none) = C(N − S, k) / C(N, k). With k == 0 both binomials are 1 → p_none
    // = 1, p_at_least_one = 0 (no cards to come, nothing can land). With N − S < k
    // the numerator C(N−S, k) is 0 → p_none = 0, p_at_least_one = 1.
    let denom = comb_u128(n, k);
    let p_none = if denom == 0 {
        // Only reachable if k > n, already rejected above; defensive fallback.
        1.0
    } else {
        let numer = comb_u128(n.saturating_sub(s), k);
        numer as f64 / denom as f64
    };
    let p_at_least_one = 1.0 - p_none;

    Ok(ScareCardOdds {
        p_at_least_one,
        p_none,
        matching_unseen: s,
        unseen_total: n,
        cards_to_come: k,
    })
}

/// Convenience wrapper: derive `cards_to_come` from the board and run
/// [`scare_card`] over `hero hole + board` against an explicit rank set.
///
/// `k = 5 − board.count()`. Pre-flop (0 board) → 5 to come, flop → 2, turn → 1,
/// river → 0. Errors propagate from [`scare_card`].
pub fn scare_card_from_board(
    hero: &HoleCards,
    board: &BoardCards,
    target_ranks: &[Rank],
) -> Result<ScareCardOdds, ScareCardError> {
    let mut known: Vec<Card> = Vec::with_capacity(7);
    known.push(hero.card1);
    known.push(hero.card2);
    known.extend(board.all_cards());
    let board_len = board.count();
    // 5 community cards total; `5 − board_len` still to come.
    let k = (5 - board_len) as u32;
    scare_card(&known, target_ranks, k)
}

/// Derive the "overcards to the top board rank" set: every rank STRICTLY higher
/// than the highest rank currently on the board.
///
/// This is the canonical "scare card" notion — a turn/river card that out-ranks
/// the whole flop (e.g. an Ace or King landing on a `Q-7-2` flop). Returns the
/// ranks in ascending order. An EMPTY board (no top rank) or a board already
/// topped by an Ace yields an empty list — the caller treats that as "no
/// overcard possible" (and [`scare_card`] will reject an empty set, so callers
/// that need a probability should branch on `is_empty()` first).
pub fn overcards_to_board_top(board: &BoardCards) -> Vec<Rank> {
    let cards = board.all_cards();
    let top = match cards.iter().map(|c| c.rank).max() {
        Some(r) => r,
        None => return Vec::new(),
    };
    Rank::ALL.iter().copied().filter(|&r| r > top).collect()
}

/// True if `cards` contains a duplicate (same rank+suit twice).
fn has_duplicate_cards(cards: &[Card]) -> bool {
    for (i, a) in cards.iter().enumerate() {
        for b in &cards[i + 1..] {
            if a == b {
                return true;
            }
        }
    }
    false
}

/// Exact binomial coefficient C(n, k) as a `u128`. Returns 0 when `k > n`.
///
/// Uses the multiplicative form with division at each step (the running product
/// is always an exact integer), so it never overflows for the deck-sized inputs
/// here (n ≤ 52) and stays integer-exact — no factorial blow-up.
fn comb_u128(n: u32, k: u32) -> u128 {
    if k > n {
        return 0;
    }
    // C(n,k) == C(n, n−k); pick the smaller k to minimize iterations.
    let k = k.min(n - k) as u128;
    let n = n as u128;
    let mut result: u128 = 1;
    let mut i: u128 = 0;
    while i < k {
        // result = result * (n - i) / (i + 1); exact because C(n, i+1) is integral.
        result = result * (n - i) / (i + 1);
        i += 1;
    }
    result
}

// ===========================================================================
// Tests
// ===========================================================================

#[cfg(test)]
mod tests {
    use super::*;
    use crate::card::{Card, Rank, Suit};
    use crate::hand::{BoardCards, HoleCards};

    fn c(r: Rank, s: Suit) -> Card {
        Card::new(r, s)
    }

    // --- hypergeometric outs odds ---

    #[test]
    fn flush_draw_9_outs_on_flop() {
        // 9-out flush draw on the flop: 47 unseen, ≈ 35.0% by river, 19.1% next.
        // unseen = 52 − 2 hero − 3 board = 47.
        let odds = outs_to_odds(9, OutsStreet::Flop, 47);
        assert_eq!(odds.outs, 9);
        assert_eq!(odds.unseen, 47);
        // by river: 1 − C(38,2)/C(47,2) = 1 − 703/1081 = 0.34968... ≈ 35.0
        assert!(
            (odds.exact_by_river_pct - 35.0).abs() < 0.1,
            "9-out flush draw by river should be ≈35.0, got {}",
            odds.exact_by_river_pct
        );
        // next card: 9/47 = 0.19148... ≈ 19.1
        assert!(
            (odds.exact_next_card_pct - 19.1).abs() < 0.1,
            "9-out next card should be ≈19.1, got {}",
            odds.exact_next_card_pct
        );
        // 2/4 rule: flop ×4 = 36.
        assert_eq!(odds.rule_2_4_pct, 36);
    }

    #[test]
    fn flush_draw_9_outs_next_card_matches_spec_value() {
        // The spec quotes 19.6% for the "next card" reference; that value uses
        // unseen=46 (a turn-card perspective). Confirm 9/46 ≈ 19.6 so the math
        // is consistent regardless of which unseen the caller passes.
        let odds = outs_to_odds(9, OutsStreet::Turn, 46);
        assert!(
            (odds.exact_next_card_pct - 19.6).abs() < 0.1,
            "9/46 next card should be ≈19.6, got {}",
            odds.exact_next_card_pct
        );
    }

    #[test]
    fn oesd_8_outs_on_flop_is_about_31_5() {
        // 8-out OESD on the flop, 47 unseen: 1 − C(39,2)/C(47,2)
        // = 1 − 741/1081 = 0.31452 ≈ 31.5.
        let odds = outs_to_odds(8, OutsStreet::Flop, 47);
        assert!(
            (odds.exact_by_river_pct - 31.5).abs() < 0.1,
            "8-out OESD by river should be ≈31.5, got {}",
            odds.exact_by_river_pct
        );
        assert_eq!(odds.rule_2_4_pct, 32);
    }

    #[test]
    fn gutshot_4_outs_on_flop_is_about_16_5() {
        // 4-out gutshot on the flop, 47 unseen: 1 − C(43,2)/C(47,2)
        // = 1 − 903/1081 = 0.16466 ≈ 16.5.
        let odds = outs_to_odds(4, OutsStreet::Flop, 47);
        assert!(
            (odds.exact_by_river_pct - 16.5).abs() < 0.1,
            "4-out gutshot by river should be ≈16.5, got {}",
            odds.exact_by_river_pct
        );
        assert_eq!(odds.rule_2_4_pct, 16);
    }

    #[test]
    fn turn_by_river_equals_next_card() {
        // On the turn there's one card to come, so "by river" == "next card".
        let odds = outs_to_odds(9, OutsStreet::Turn, 46);
        assert_eq!(odds.exact_by_river_pct, odds.exact_next_card_pct);
    }

    #[test]
    fn outs_clamped_to_unseen_never_exceeds_100() {
        let odds = outs_to_odds(999, OutsStreet::Flop, 47);
        assert_eq!(odds.outs, 47, "outs clamped to unseen");
        assert!(odds.exact_by_river_pct <= 100.0);
        assert_eq!(odds.rule_2_4_pct, 100, "rule value capped at 100");
    }

    #[test]
    fn zero_outs_is_zero_percent() {
        let odds = outs_to_odds(0, OutsStreet::Flop, 47);
        assert_eq!(odds.exact_by_river_pct, 0.0);
        assert_eq!(odds.exact_next_card_pct, 0.0);
        assert_eq!(odds.rule_2_4_pct, 0);
    }

    // --- draw detection ---

    #[test]
    fn detects_clear_flush_draw() {
        // Hero AhKh, board Qh-7h-2c → four hearts = flush draw (9 outs).
        let hero = HoleCards::new(c(Rank::Ace, Suit::Hearts), c(Rank::King, Suit::Hearts));
        let board = BoardCards {
            flop: Some([
                c(Rank::Queen, Suit::Hearts),
                c(Rank::Seven, Suit::Hearts),
                c(Rank::Two, Suit::Clubs),
            ]),
            turn: None,
            river: None,
        };
        let d = detect_draws(&hero, &board);
        assert!(
            d.draws.contains(&DrawKind::FlushDraw),
            "must detect the flush draw, got {:?}",
            d.draws
        );
        assert_eq!(d.outs, 9, "flush draw is 9 outs");
    }

    #[test]
    fn detects_clear_oesd() {
        // Hero 8s9d, board 6h-7c-2s → held 6-7-8-9 = open-ended (8 outs: 5s & Ts).
        let hero = HoleCards::new(c(Rank::Eight, Suit::Spades), c(Rank::Nine, Suit::Diamonds));
        let board = BoardCards {
            flop: Some([
                c(Rank::Six, Suit::Hearts),
                c(Rank::Seven, Suit::Clubs),
                c(Rank::Two, Suit::Spades),
            ]),
            turn: None,
            river: None,
        };
        let d = detect_draws(&hero, &board);
        assert!(
            d.draws.contains(&DrawKind::OpenEndedStraightDraw),
            "must detect the OESD, got {:?}",
            d.draws
        );
        assert_eq!(d.outs, 8, "OESD is 8 outs");
    }

    #[test]
    fn detects_gutshot() {
        // Hero 9s5d, board 6h-7c-2s → 5-6-7- -9 (missing 8) = inside gutshot (4 outs).
        let hero = HoleCards::new(c(Rank::Nine, Suit::Spades), c(Rank::Five, Suit::Diamonds));
        let board = BoardCards {
            flop: Some([
                c(Rank::Six, Suit::Hearts),
                c(Rank::Seven, Suit::Clubs),
                c(Rank::Two, Suit::Spades),
            ]),
            turn: None,
            river: None,
        };
        let d = detect_draws(&hero, &board);
        assert!(
            d.draws.contains(&DrawKind::Gutshot),
            "must detect the gutshot, got {:?}",
            d.draws
        );
        assert!(
            !d.draws.contains(&DrawKind::OpenEndedStraightDraw),
            "an inside gutshot must NOT be reported as OESD"
        );
        assert_eq!(d.outs, 4, "gutshot is 4 outs");
    }

    #[test]
    fn no_draw_board_detects_nothing() {
        // Hero As2d, board 7h-9c-Kd → no flush draw, no four-to-a-straight.
        let hero = HoleCards::new(c(Rank::Ace, Suit::Spades), c(Rank::Two, Suit::Diamonds));
        let board = BoardCards {
            flop: Some([
                c(Rank::Seven, Suit::Hearts),
                c(Rank::Nine, Suit::Clubs),
                c(Rank::King, Suit::Diamonds),
            ]),
            turn: None,
            river: None,
        };
        let d = detect_draws(&hero, &board);
        assert!(d.draws.is_empty(), "no draws expected, got {:?}", d.draws);
        assert_eq!(d.outs, 0);
    }

    #[test]
    fn made_straight_is_not_a_draw() {
        // Hero 8s9d, board 6h-7c-Ts → already a made straight (6-7-8-9-T).
        let hero = HoleCards::new(c(Rank::Eight, Suit::Spades), c(Rank::Nine, Suit::Diamonds));
        let board = BoardCards {
            flop: Some([
                c(Rank::Six, Suit::Hearts),
                c(Rank::Seven, Suit::Clubs),
                c(Rank::Ten, Suit::Spades),
            ]),
            turn: None,
            river: None,
        };
        let d = detect_draws(&hero, &board);
        assert!(
            !d.draws.contains(&DrawKind::OpenEndedStraightDraw)
                && !d.draws.contains(&DrawKind::Gutshot),
            "a made straight must not be reported as a draw, got {:?}",
            d.draws
        );
    }

    #[test]
    fn made_flush_is_not_a_flush_draw() {
        // Hero AhKh, board Qh-7h-2h → five hearts = made flush, not a draw.
        let hero = HoleCards::new(c(Rank::Ace, Suit::Hearts), c(Rank::King, Suit::Hearts));
        let board = BoardCards {
            flop: Some([
                c(Rank::Queen, Suit::Hearts),
                c(Rank::Seven, Suit::Hearts),
                c(Rank::Two, Suit::Hearts),
            ]),
            turn: None,
            river: None,
        };
        let d = detect_draws(&hero, &board);
        assert!(
            !d.draws.contains(&DrawKind::FlushDraw),
            "a made flush must not be reported as a flush draw"
        );
    }

    #[test]
    fn combo_draw_takes_larger_outs_not_sum() {
        // Hero Th9h, board 8h-7c-Qh → flush draw (9) + OESD (6-... wait, held
        // 7-8-9-T = OESD). Conservative outs = max(9, 8) = 9, NOT 17.
        let hero = HoleCards::new(c(Rank::Ten, Suit::Hearts), c(Rank::Nine, Suit::Hearts));
        let board = BoardCards {
            flop: Some([
                c(Rank::Eight, Suit::Hearts),
                c(Rank::Seven, Suit::Clubs),
                c(Rank::Queen, Suit::Hearts),
            ]),
            turn: None,
            river: None,
        };
        let d = detect_draws(&hero, &board);
        assert!(d.draws.contains(&DrawKind::FlushDraw));
        assert!(d.draws.contains(&DrawKind::OpenEndedStraightDraw));
        assert_eq!(
            d.outs, 9,
            "combo draw must not naively sum outs (would overcount shared cards)"
        );
    }

    #[test]
    fn wheel_2345_is_oesd_not_gutshot() {
        // Hero 2s3d, board 4h-5c-9s → held 2-3-4-5 completes with an Ace (wheel
        // A-2-3-4-5) OR a Six (2-3-4-5-6) = open-ended, 8 outs. (Regression: the
        // low end was wrongly treated as deck-blocked → mis-classified gutshot.)
        let hero = HoleCards::new(c(Rank::Two, Suit::Spades), c(Rank::Three, Suit::Diamonds));
        let board = BoardCards {
            flop: Some([
                c(Rank::Four, Suit::Hearts),
                c(Rank::Five, Suit::Clubs),
                c(Rank::Nine, Suit::Spades),
            ]),
            turn: None,
            river: None,
        };
        let d = detect_draws(&hero, &board);
        assert!(
            d.draws.contains(&DrawKind::OpenEndedStraightDraw),
            "2-3-4-5 is open-ended (A or 6), got {:?}",
            d.draws
        );
        assert_eq!(
            d.outs, 8,
            "wheel-low OESD is 8 outs (four Aces + four Sixes)"
        );
    }

    #[test]
    fn pot_odds_overflow_inputs_rejected() {
        // Individually-finite but astronomically large inputs whose sum overflows
        // to +inf must be rejected, not produce a bogus required-equity / ratio.
        assert!(pot_odds(f64::MAX, f64::MAX, None).is_err());
    }

    #[test]
    fn edge_run_broadway_is_gutshot_not_oesd() {
        // Hero AsKd, board Qc-Jh-2s → held A-K-Q-J, missing T. The run is at the
        // top of the deck (can't extend past the Ace), so the only completing
        // card is the T → gutshot (4 outs), NOT an 8-out OESD.
        let hero = HoleCards::new(c(Rank::Ace, Suit::Spades), c(Rank::King, Suit::Diamonds));
        let board = BoardCards {
            flop: Some([
                c(Rank::Queen, Suit::Clubs),
                c(Rank::Jack, Suit::Hearts),
                c(Rank::Two, Suit::Spades),
            ]),
            turn: None,
            river: None,
        };
        let d = detect_draws(&hero, &board);
        assert!(
            d.draws.contains(&DrawKind::Gutshot),
            "Broadway draw is a one-way (gutshot), got {:?}",
            d.draws
        );
        assert!(
            !d.draws.contains(&DrawKind::OpenEndedStraightDraw),
            "an edge-bounded run must not be an OESD"
        );
    }

    #[test]
    fn wheel_draw_is_gutshot() {
        // Hero As2d, board 3h-4c-9s → A-2-3-4 (wheel draw, needs the 5) = gutshot.
        let hero = HoleCards::new(c(Rank::Ace, Suit::Spades), c(Rank::Two, Suit::Diamonds));
        let board = BoardCards {
            flop: Some([
                c(Rank::Three, Suit::Hearts),
                c(Rank::Four, Suit::Clubs),
                c(Rank::Nine, Suit::Spades),
            ]),
            turn: None,
            river: None,
        };
        let d = detect_draws(&hero, &board);
        assert!(
            d.draws.contains(&DrawKind::Gutshot),
            "wheel draw should be a gutshot, got {:?}",
            d.draws
        );
    }

    #[test]
    fn detection_skipped_off_flop_and_turn() {
        // Preflop (0 board) → no detection.
        let hero = HoleCards::new(c(Rank::Ace, Suit::Hearts), c(Rank::King, Suit::Hearts));
        let preflop = detect_draws(&hero, &BoardCards::empty());
        assert!(preflop.draws.is_empty());
        assert_eq!(preflop.outs, 0);
    }

    // --- pot odds ---

    #[test]
    fn half_pot_bet_needs_25pct() {
        // Pot 100, bet 50 (½ pot) → required equity 50/150 = 33.3%? No: a ½-pot
        // BET means hero calls 50 into a pot that already shows 100, so
        // to_call/(pot+to_call) = 50/150 = 33.3. The spec's "½-pot bet → 25%"
        // refers to pot=150 (after the bettor's 50 is already in) / to_call 50:
        // 50/200 = 25%. Test that arithmetic explicitly.
        let po = pot_odds(150.0, 50.0, None).unwrap();
        assert!(
            (po.equity_needed_pct - 25.0).abs() < 0.05,
            "½-pot bet (call 50 into 150) needs 25%, got {}",
            po.equity_needed_pct
        );
    }

    #[test]
    fn pot_size_bet_needs_33pct() {
        // Pot-size bet: pot 100, call 50 → 50/150 = 33.3%.
        let po = pot_odds(100.0, 50.0, None).unwrap();
        assert!(
            (po.equity_needed_pct - 33.3).abs() < 0.05,
            "call 50 into 100 needs 33.3%, got {}",
            po.equity_needed_pct
        );
        // Canonical pot odds are pot:to_call = 100:50 = 2:1, and 1/(2+1) = 33.3%
        // matches equity_needed — U18 (dual-AI OSS review) fixed the prior 3:1
        // that implied 25% and disagreed with equity_needed.
        assert_eq!(po.ratio, "2:1", "pot 100 : call 50 simplifies to 2:1");
        assert_eq!(po.pot_odds_pct, po.equity_needed_pct);
        assert!(po.verdict.is_none(), "no equity ⇒ no verdict");
        assert!(po.margin_pct.is_none());
    }

    #[test]
    fn ratio_and_equity_needed_are_consistent() {
        // U18: ratio X:1 must satisfy equity_needed = 1/(X+1) for clean spots.
        for (pot, call, ratio, needed) in [
            (150.0, 50.0, "3:1", 25.0),
            (100.0, 50.0, "2:1", 33.3),
            (100.0, 25.0, "4:1", 20.0),
        ] {
            let po = pot_odds(pot, call, None).unwrap();
            assert_eq!(po.ratio, ratio, "pot {pot} call {call}");
            assert!(
                (po.equity_needed_pct - needed).abs() < 0.05,
                "pot {pot} call {call}: needed {needed}, got {}",
                po.equity_needed_pct
            );
        }
    }

    #[test]
    fn verdict_call_when_equity_clears_with_margin() {
        let po = pot_odds(100.0, 50.0, Some(45.0)).unwrap();
        // needed 33.3, equity 45 → margin +11.7 → call.
        assert_eq!(po.verdict, Some(PotOddsVerdict::Call));
        assert!((po.margin_pct.unwrap() - 11.7).abs() < 0.05);
    }

    #[test]
    fn verdict_fold_when_equity_short_with_margin() {
        let po = pot_odds(100.0, 50.0, Some(20.0)).unwrap();
        // needed 33.3, equity 20 → margin -13.3 → fold.
        assert_eq!(po.verdict, Some(PotOddsVerdict::Fold));
        assert!(po.margin_pct.unwrap() < 0.0);
    }

    #[test]
    fn verdict_marginal_inside_dead_zone() {
        let po = pot_odds(100.0, 50.0, Some(34.0)).unwrap();
        // needed 33.3, equity 34 → margin +0.7 (< 2) → marginal.
        assert_eq!(po.verdict, Some(PotOddsVerdict::Marginal));
    }

    #[test]
    fn ratio_simplifies_via_gcd() {
        // pot 100, call 25 → reward 100 : risk 25 = 4:1 (needed 1/5 = 20%).
        let po = pot_odds(100.0, 25.0, None).unwrap();
        assert_eq!(po.ratio, "4:1");
    }

    #[test]
    fn ratio_non_integer_falls_back_to_x_to_1() {
        // pot 100, call 33 → reward 100 : 33 = 3.0:1 (rounded).
        let po = pot_odds(100.0, 33.0, None).unwrap();
        assert_eq!(po.ratio, "3:1");
    }

    #[test]
    fn rejects_non_positive_inputs() {
        assert_eq!(pot_odds(0.0, 50.0, None), Err(PotOddsError::InvalidPot));
        assert_eq!(pot_odds(100.0, 0.0, None), Err(PotOddsError::InvalidToCall));
        assert_eq!(
            pot_odds(f64::NAN, 50.0, None),
            Err(PotOddsError::InvalidPot)
        );
        assert_eq!(
            pot_odds(100.0, f64::INFINITY, None),
            Err(PotOddsError::InvalidToCall)
        );
    }

    #[test]
    fn rejects_out_of_range_equity() {
        assert_eq!(
            pot_odds(100.0, 50.0, Some(150.0)),
            Err(PotOddsError::InvalidEquity)
        );
        assert_eq!(
            pot_odds(100.0, 50.0, Some(-1.0)),
            Err(PotOddsError::InvalidEquity)
        );
        assert_eq!(
            pot_odds(100.0, 50.0, Some(f64::NAN)),
            Err(PotOddsError::InvalidEquity)
        );
    }

    // --- scare-card (exact hypergeometric) ---

    /// Tolerance for comparing an EXACT closed-form probability against a
    /// hand-computed reference fraction — generous slack, the math is exact.
    const SC_EPS: f64 = 1e-9;

    #[test]
    fn scare_card_flop_one_ace_by_river_no_aces_seen() {
        // Flop spot: hero Qh-Jc, board 8d-7s-2h (NO aces anywhere). Known = 5,
        // N = 47, S = 4 aces, k = 2 (flop→river).
        // P(≥1 ace) = 1 − C(43,2)/C(47,2) = 1 − 903/1081 = 178/1081 = 0.1646623…
        let known = vec![
            c(Rank::Queen, Suit::Hearts),
            c(Rank::Jack, Suit::Clubs),
            c(Rank::Eight, Suit::Diamonds),
            c(Rank::Seven, Suit::Spades),
            c(Rank::Two, Suit::Hearts),
        ];
        let odds = scare_card(&known, &[Rank::Ace], 2).unwrap();
        assert_eq!(odds.unseen_total, 47, "N = 52 − 5 known");
        assert_eq!(odds.matching_unseen, 4, "all four aces unseen");
        assert_eq!(odds.cards_to_come, 2);
        let expected = 178.0 / 1081.0;
        assert!(
            (odds.p_at_least_one - expected).abs() < SC_EPS,
            "P(≥1 ace) should be 178/1081 = {expected}, got {}",
            odds.p_at_least_one
        );
        assert!(
            (odds.p_none - (903.0 / 1081.0)).abs() < SC_EPS,
            "P(none) should be 903/1081, got {}",
            odds.p_none
        );
        // p_none and p_at_least_one always sum to exactly 1.
        assert!((odds.p_at_least_one + odds.p_none - 1.0).abs() < SC_EPS);
    }

    #[test]
    fn scare_card_turn_to_river_one_card_to_come() {
        // Turn spot, k = 1: hero Qh-Jc, board 8d-7s-2h-3c. Known = 6, N = 46,
        // S = 4 aces. P(≥1 ace) = 4/46 = 0.0869565…
        let known = vec![
            c(Rank::Queen, Suit::Hearts),
            c(Rank::Jack, Suit::Clubs),
            c(Rank::Eight, Suit::Diamonds),
            c(Rank::Seven, Suit::Spades),
            c(Rank::Two, Suit::Hearts),
            c(Rank::Three, Suit::Clubs),
        ];
        let odds = scare_card(&known, &[Rank::Ace], 1).unwrap();
        assert_eq!(odds.unseen_total, 46);
        assert_eq!(odds.matching_unseen, 4);
        assert_eq!(odds.cards_to_come, 1);
        let expected = 4.0 / 46.0;
        assert!(
            (odds.p_at_least_one - expected).abs() < SC_EPS,
            "single card to come: P(ace) = 4/46 = {expected}, got {}",
            odds.p_at_least_one
        );
    }

    #[test]
    fn scare_card_multi_rank_set_ace_or_king() {
        // Flop, target {A, K}, none seen: hero Qh-Jc, board 8d-7s-2h. N = 47,
        // S = 8, k = 2. P(≥1 of A/K) = 1 − C(39,2)/C(47,2) = 340/1081 = 0.3145236…
        let known = vec![
            c(Rank::Queen, Suit::Hearts),
            c(Rank::Jack, Suit::Clubs),
            c(Rank::Eight, Suit::Diamonds),
            c(Rank::Seven, Suit::Spades),
            c(Rank::Two, Suit::Hearts),
        ];
        let odds = scare_card(&known, &[Rank::Ace, Rank::King], 2).unwrap();
        assert_eq!(odds.matching_unseen, 8, "four aces + four kings unseen");
        let expected = 340.0 / 1081.0;
        assert!(
            (odds.p_at_least_one - expected).abs() < SC_EPS,
            "P(≥1 of A/K) should be 340/1081 = {expected}, got {}",
            odds.p_at_least_one
        );
    }

    #[test]
    fn scare_card_dedupes_duplicate_target_ranks() {
        // Passing [Ace, Ace] is identical to [Ace] — distinct rank count is 1.
        let known = vec![
            c(Rank::Queen, Suit::Hearts),
            c(Rank::Jack, Suit::Clubs),
            c(Rank::Eight, Suit::Diamonds),
            c(Rank::Seven, Suit::Spades),
            c(Rank::Two, Suit::Hearts),
        ];
        let once = scare_card(&known, &[Rank::Ace], 2).unwrap();
        let twice = scare_card(&known, &[Rank::Ace, Rank::Ace], 2).unwrap();
        assert_eq!(once, twice, "duplicate target ranks must collapse");
    }

    #[test]
    fn scare_card_blocker_in_hand_reduces_s() {
        // Hero HOLDS the Ace of spades (a blocker). Target {A}, flop board with
        // no aces: hero As-Jc, board 8d-7s-2h. Known = 5, N = 47, but S = 3
        // (only three aces remain unseen). P(≥1 ace) = 1 − C(44,2)/C(47,2)
        // = 135/1081 = 0.1248843… — strictly LOWER than the no-blocker 178/1081.
        let known = vec![
            c(Rank::Ace, Suit::Spades),
            c(Rank::Jack, Suit::Clubs),
            c(Rank::Eight, Suit::Diamonds),
            c(Rank::Seven, Suit::Spades),
            c(Rank::Two, Suit::Hearts),
        ];
        let odds = scare_card(&known, &[Rank::Ace], 2).unwrap();
        assert_eq!(odds.matching_unseen, 3, "hero's ace is a blocker → S = 3");
        let expected = 135.0 / 1081.0;
        assert!(
            (odds.p_at_least_one - expected).abs() < SC_EPS,
            "blocker case P(≥1 ace) = 135/1081 = {expected}, got {}",
            odds.p_at_least_one
        );
        // Sanity: strictly below the no-blocker value (178/1081).
        assert!(
            odds.p_at_least_one < 178.0 / 1081.0,
            "a blocker must lower the probability"
        );
    }

    #[test]
    fn scare_card_river_complete_k0_is_deterministic() {
        // k = 0: river already dealt, nothing to come. P(≥1) = 0, P(none) = 1
        // regardless of the target set.
        let known = vec![
            c(Rank::Queen, Suit::Hearts),
            c(Rank::Jack, Suit::Clubs),
            c(Rank::Eight, Suit::Diamonds),
            c(Rank::Seven, Suit::Spades),
            c(Rank::Two, Suit::Hearts),
            c(Rank::Three, Suit::Clubs),
            c(Rank::Four, Suit::Diamonds),
        ];
        let odds = scare_card(&known, &[Rank::Ace], 0).unwrap();
        assert_eq!(odds.cards_to_come, 0);
        assert_eq!(odds.p_at_least_one, 0.0, "no cards to come → never lands");
        assert_eq!(odds.p_none, 1.0);
    }

    #[test]
    fn scare_card_all_targets_already_dealt_is_zero() {
        // S = 0: every ace is already visible (4 in the known set), so no ace can
        // land. P(≥1 ace) = 0. (A contrived but valid known set.)
        let known = vec![
            c(Rank::Ace, Suit::Spades),
            c(Rank::Ace, Suit::Hearts),
            c(Rank::Ace, Suit::Diamonds),
            c(Rank::Ace, Suit::Clubs),
            c(Rank::Two, Suit::Hearts),
        ];
        let odds = scare_card(&known, &[Rank::Ace], 2).unwrap();
        assert_eq!(odds.matching_unseen, 0, "all four aces seen → S = 0");
        assert_eq!(odds.p_at_least_one, 0.0);
        assert_eq!(odds.p_none, 1.0);
    }

    #[test]
    fn scare_card_empty_rank_set_errors() {
        let known = vec![c(Rank::Queen, Suit::Hearts), c(Rank::Jack, Suit::Clubs)];
        assert_eq!(
            scare_card(&known, &[], 2),
            Err(ScareCardError::EmptyRankSet)
        );
    }

    #[test]
    fn scare_card_duplicate_known_card_errors() {
        // Qh appears twice in the known set.
        let known = vec![
            c(Rank::Queen, Suit::Hearts),
            c(Rank::Queen, Suit::Hearts),
            c(Rank::Eight, Suit::Diamonds),
        ];
        assert_eq!(
            scare_card(&known, &[Rank::Ace], 2),
            Err(ScareCardError::DuplicateCard)
        );
    }

    #[test]
    fn scare_card_k_exceeds_unseen_errors() {
        // A full deck of 52 known cards leaves N = 0; any k > 0 is impossible.
        let mut known = Vec::with_capacity(52);
        for &s in Suit::ALL.iter() {
            for &r in Rank::ALL.iter() {
                known.push(c(r, s));
            }
        }
        assert_eq!(known.len(), 52);
        assert_eq!(
            scare_card(&known, &[Rank::Ace], 1),
            Err(ScareCardError::InvalidCardsToCome)
        );
    }

    #[test]
    fn scare_card_n_minus_s_below_k_is_certain() {
        // Construct N − S < k so every remaining draw must include a target rank
        // → P(≥1) = 1. Use a tiny remaining pool: 50 known cards leave N = 2.
        // Target = every NON-Ace, NON-King rank still in the deck. We engineer a
        // known set where the 2 unseen cards are BOTH a target rank, S = 2, N = 2,
        // k = 2 → N − S = 0 < 2 → certain.
        // Simpler: keep all but two cards known; the two unseen are 5d and 6c.
        let mut known: Vec<Card> = Vec::new();
        for &s in Suit::ALL.iter() {
            for &r in Rank::ALL.iter() {
                let card = c(r, s);
                // Leave out exactly two target-rank cards (5d, 6c).
                if (r == Rank::Five && s == Suit::Diamonds) || (r == Rank::Six && s == Suit::Clubs)
                {
                    continue;
                }
                known.push(card);
            }
        }
        assert_eq!(known.len(), 50, "N = 2 unseen");
        let odds = scare_card(&known, &[Rank::Five, Rank::Six], 2).unwrap();
        assert_eq!(odds.unseen_total, 2);
        assert_eq!(odds.matching_unseen, 2, "both unseen cards are targets");
        assert_eq!(
            odds.p_at_least_one, 1.0,
            "N − S < k ⇒ a target rank is certain"
        );
        assert_eq!(odds.p_none, 0.0);
    }

    #[test]
    fn scare_card_from_board_derives_k_from_board() {
        // The wrapper derives k = 5 − board_len. Flop board → k = 2; identical to
        // the explicit flop call above (hero Qh-Jc, board 8d-7s-2h, target {A}).
        let hero = HoleCards::new(c(Rank::Queen, Suit::Hearts), c(Rank::Jack, Suit::Clubs));
        let board = BoardCards {
            flop: Some([
                c(Rank::Eight, Suit::Diamonds),
                c(Rank::Seven, Suit::Spades),
                c(Rank::Two, Suit::Hearts),
            ]),
            turn: None,
            river: None,
        };
        let odds = scare_card_from_board(&hero, &board, &[Rank::Ace]).unwrap();
        assert_eq!(odds.cards_to_come, 2, "flop → 2 cards to come");
        assert_eq!(odds.unseen_total, 47);
        let expected = 178.0 / 1081.0;
        assert!((odds.p_at_least_one - expected).abs() < SC_EPS);

        // Pre-flop board → k = 5.
        let pre = scare_card_from_board(&hero, &BoardCards::empty(), &[Rank::Ace]).unwrap();
        assert_eq!(pre.cards_to_come, 5);
        assert_eq!(pre.unseen_total, 50);
    }

    #[test]
    fn overcards_to_board_top_picks_strictly_higher_ranks() {
        // Board Q-7-2 → overcards are K and A.
        let board = BoardCards {
            flop: Some([
                c(Rank::Queen, Suit::Hearts),
                c(Rank::Seven, Suit::Spades),
                c(Rank::Two, Suit::Diamonds),
            ]),
            turn: None,
            river: None,
        };
        let over = overcards_to_board_top(&board);
        assert_eq!(over, vec![Rank::King, Rank::Ace], "Q-high → K, A are over");

        // Ace-high board → no overcard possible.
        let ace_board = BoardCards {
            flop: Some([
                c(Rank::Ace, Suit::Hearts),
                c(Rank::Seven, Suit::Spades),
                c(Rank::Two, Suit::Diamonds),
            ]),
            turn: None,
            river: None,
        };
        assert!(
            overcards_to_board_top(&ace_board).is_empty(),
            "nothing out-ranks an Ace"
        );

        // Empty board → no top rank → empty.
        assert!(overcards_to_board_top(&BoardCards::empty()).is_empty());
    }

    #[test]
    fn scare_card_overcards_to_q72_flop_matches_ak_set() {
        // Combine the helper + scare_card: board Q-7-2, hero 9d-8c (no over to
        // help). Overcards = {K, A}; none seen, so this equals the {A,K} multi-
        // rank case: S = 8, N = 47, k = 2 → 340/1081.
        let hero = HoleCards::new(c(Rank::Nine, Suit::Diamonds), c(Rank::Eight, Suit::Clubs));
        let board = BoardCards {
            flop: Some([
                c(Rank::Queen, Suit::Hearts),
                c(Rank::Seven, Suit::Spades),
                c(Rank::Two, Suit::Diamonds),
            ]),
            turn: None,
            river: None,
        };
        let over = overcards_to_board_top(&board);
        assert_eq!(over, vec![Rank::King, Rank::Ace]);
        let odds = scare_card_from_board(&hero, &board, &over).unwrap();
        assert_eq!(odds.matching_unseen, 8);
        let expected = 340.0 / 1081.0;
        assert!((odds.p_at_least_one - expected).abs() < SC_EPS);
    }

    #[test]
    fn comb_u128_known_values() {
        // Spot-check the binomial helper against known values.
        assert_eq!(comb_u128(47, 2), 1081);
        assert_eq!(comb_u128(43, 2), 903);
        assert_eq!(comb_u128(50, 5), 2_118_760);
        assert_eq!(comb_u128(5, 0), 1, "C(n,0) = 1");
        assert_eq!(comb_u128(2, 3), 0, "k > n → 0");
        assert_eq!(comb_u128(52, 52), 1, "C(n,n) = 1");
    }
}
