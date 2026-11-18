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

use crate::card::{Card, Rank};
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
/// Hero must PARTICIPATE in a draw for it to count (POKER-PRO-F1 / BUG-201): a
/// four-flush needs at least one hero card of that suit, and a four-to-a-straight
/// needs at least one hero rank inside the five-rank window that the board does
/// not already supply. A pattern lying entirely on the board improves every
/// opponent identically and is not hero's draw (0 outs).
///
/// Straight detection treats the Ace as both high and low (A-2-3-4-5 wheel and
/// T-J-Q-K-A). Pairs/duplicate ranks collapse for straight purposes (a straight
/// uses distinct ranks). Returned outs combine flush + straight conservatively
/// (see [`DetectedDraws::outs`]).
pub fn detect_draws(hero: &HoleCards, board: &BoardCards) -> DetectedDraws {
    let features = super::draws::draws(hero, board);
    let mut draws = Vec::new();
    if features.flush {
        draws.push(DrawKind::FlushDraw);
    }
    if features.open_ended {
        draws.push(DrawKind::OpenEndedStraightDraw);
    } else if features.gutshot {
        draws.push(DrawKind::Gutshot);
    }
    let outs = draws.iter().map(|d| d.outs()).max().unwrap_or(0);
    DetectedDraws { draws, outs }
}

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

/// Pot odds as the equity a call needs, `to_call / (pot + to_call)`, in percent
/// and unrounded; 0 when nothing is owed. Each caller rounds for its own use:
/// whole percent for coach labels, one decimal in the pot-odds tool, and a
/// floor for the bots' integer thresholds. (The spot analyser keeps its own
/// `f32` big-blind version: widening `f32` decimals to `f64` moves half-percent ties.)
pub fn pot_odds_pct(pot: f64, to_call: f64) -> f64 {
    let total = pot + to_call;
    if to_call <= 0.0 || total <= 0.0 {
        return 0.0;
    }
    to_call * 100.0 / total
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

    let needed = pot_odds_pct(pot, to_call);
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
    if !crate::card::cards_unique(known.iter().copied()) {
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
