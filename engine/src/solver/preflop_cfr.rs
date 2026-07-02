//! Discounted-CFR preflop solver over a SIMPLIFIED 6-max / 100bb tree.
//!
//! ## HONESTY (read this first)
//!
//! This is **NOT a true postflop GTO solve.** It is a Discounted-CFR (DCFR)
//! *approximate equilibrium* of a deliberately simplified game whose terminal
//! values substitute **all-in equity** + a documented **per-class
//! equity-realization factor** `R` for true postflop EV. The engine has no
//! postflop solver (none exists in the repo), so a postflop-coupled terminal EV
//! is out of scope; building one would be research-grade. Therefore:
//!
//! * The equilibrium math is **real** — regret matching, discounted regret/
//!   strategy accumulation, opponent reactions modelled inside the tree, and the
//!   converged average strategy is **low-exploitability *within this simplified
//!   game*** — this is MEASURED, not asserted: `bucket_exploitability` computes a
//!   NashConv-style best-response gap against the same model, and the test
//!   `dcfr_exploitability_is_low_at_configured_iters` asserts it is below a
//!   documented threshold at [`CFR_ITERS`].
//! * The absolute action frequencies are only as correct as the `R` model.
//! * The PUBLISHED chart is **CFR-derived then POST-PROCESSED**: the generator
//!   (`gen_preflop_ranges::enforce_invariants`) rounds, drops sub-floor dust,
//!   force-includes the premiums (AA/KK/QQ/AKs), hard-excludes obvious trash, and
//!   force-adds any `vs_4bet` hand back into `vs_3bet` to keep `vs_4bet ⊆ vs_3bet`.
//!   So the emitted ranges are an equilibrium output WITH a documented hand-rule
//!   overlay — honestly "CFR-derived, post-processed," not a bare equilibrium dump.
//! * The honest label is **"CFR approx. equilibrium · equity-realization"** —
//!   it MUST NEVER be called "GTO". See [`crate::solver::spot::SolveMethod`].
//!
//! Honest fidelity ceiling: *directionally sound, model-dependent.* Strictly
//! more principled than a heuristic threshold chart (it models opponent
//! reactions and yields mixed frequencies), but it is an equilibrium of a model,
//! not of real poker.
//!
//! ## Model parameters (FIXED inputs, NOT solved)
//!
//! The bet-sizing tree (open / 3-bet / 4-bet / jam sizes) and the `R`-factor
//! bands are *model parameters*, documented here and in the emitted `_source`.
//! A different sizing tree or `R` table yields different ranges — so the output
//! is never over-claimed as the unique solution to real 100bb hold'em.
//!
//! * stacks: 100 bb effective.
//! * blinds: SB 0.5 bb, BB 1.0 bb.
//! * open (RFI / iso): to 2.5 bb.
//! * 3-bet: to 3.2× the open.
//! * 4-bet: to 2.2× the 3-bet.
//!
//! ## EV model (model (b))
//!
//! Per terminal, from the actor's perspective, in bb. HONESTY (F1): the FOLD and
//! ALL-IN terminals are zero-sum (one player's gain is the other's loss); the
//! SEE-FLOP terminal is **general-sum** — each side applies its OWN realization
//! haircut `R`, so the two shares need not sum to 1. The game is therefore solved
//! as a general-sum game by per-player best-response (NashConv) convergence, NOT
//! as a zero-sum game with a single value (see `solve_bucket_full`).
//!
//! * **fold** — the folder forfeits chips already invested; the other player
//!   wins the current pot uncontested (EXACT, zero-sum).
//! * **all-in** — pot is decided by preflop **all-in equity** `e` (from the
//!   cached deterministic 169×169 matrix): EV = `e * final_pot - invested`. Both
//!   sides use RAW `e` / `1-e`, so the all-in terminal is strictly ZERO-SUM.
//! * **see-flop** (a call that closes the action) — valued as
//!   `pot_share = e * R(class, line) * final_pot - invested`, where
//!   `R ∈ [0.70, 1.12]` is the bounded equity-realization factor: the in-position
//!   / initiative side realizes MORE than raw equity, the out-of-position caller
//!   LESS, and WEAK hands realize their equity poorly (an equity tilt). `R` is a
//!   documented model assumption, NOT a solved value. Because hero and villain use
//!   independent `R`, this terminal is GENERAL-SUM (shares need not sum to 1).
//!
//! ## Determinism / reproducibility
//!
//! A fixed equity-matrix seed, a fixed MC trial count, a fixed DCFR iteration
//! count, and a deterministic traversal (arrays indexed by the canonical 169
//! order, NO `HashMap` iteration) together give a byte-identical strategy every
//! run, in debug and release. The emitter (`gen_preflop_ranges`) rounds
//! frequencies to a fixed number of decimals so float noise cannot perturb the
//! bytes.
//!
//! Pure: no IO, no async, no `rs_poker` in public signatures (ADR-012).

use crate::card::{Card, Rank, Suit};
use crate::hand::{BoardCards, HoleCards};
use crate::solver::equity::{equity, EquityInput, OpponentSpec};
use crate::solver::preflop_charts::all_hand_keys;

/// Number of canonical 169 hand classes.
pub const N_CLASSES: usize = 169;

/// Monte-Carlo trials per all-in-equity matrix cell. Fixed for reproducibility;
/// the engine clamps to `MAX_MC_TRIALS`. The per-cell value is the INTEGER
/// `combined_pct` (already quantized to 1%), and the emitted chart frequencies
/// are rounded to 2 decimals, so 8k trials give ample precision while keeping
/// the build to seconds (release) / a minute or two (debug). Determinism is
/// EXACT for any trial count (fixed seed) — trials only set the precision.
pub const EQUITY_TRIALS: u32 = 8_000;

/// Fixed seed for the all-in-equity matrix. One seed; the per-cell seed is
/// derived deterministically from the (hero, villain) class indices so each cell
/// is an independent, reproducible estimate.
pub const EQUITY_SEED: u64 = 0xB1FF_C0DE_1234_5678;

/// DCFR iteration count. Fixed for reproducibility AND chosen so the strategy
/// has converged to low exploitability *of this simplified model* — MEASURED by
/// the test `dcfr_exploitability_is_low_at_configured_iters` (via
/// `bucket_exploitability`, a NashConv-style best-response gap), NOT merely
/// asserted. DCFR converges fast on a 2-action tree; 2000 iters is well past the
/// knee (the measured per-bucket gap is a small fraction of a big blind).
pub const CFR_ITERS: u32 = 2_000;

/// Discounted-CFR-style exponents (Brown & Sandholm 2019, "Discounted CFR").
/// `(α,β,γ) = (1.5, 0, 2)` are the canonical DCFR COEFFICIENTS. HONESTY (F4,
/// 2026-06-25): the UPDATE FORM here is a *variant*, not canonical DCFR. We apply
/// `R_t = (R_{t-1} + r_t) * disc`, where `disc` is selected by the sign of the
/// INSTANT regret `r_t` (positive → `t^α/(t^α+1)`, negative → `t^β/(t^β+1)`); the
/// strategy sum is discounted by `(t/(t+1))^γ`. Canonical DCFR instead discounts
/// the ACCUMULATED regret by a factor chosen on the ACCUMULATED regret's sign and
/// THEN adds `r_t`: `R_t = R_{t-1} * disc(sign(R_{t-1})) + r_t`. Both are
/// discounted-regret schemes that converge fast + stably on a 2-action tree, and
/// the exploitability test measures the actual convergence of THIS form — so the
/// honest description is "a discounted-regret variant with DCFR coefficients,"
/// NOT "the canonical DCFR update." U62 (dual-AI OSS review): the STRATEGY
/// averaging is likewise a variant — it is increment-scaled per iteration
/// (asymptotically near-uniform), not the exact accumulated `(t/(t+1))^γ`
/// weighting; the same "variant, not canonical" caveat applies to it.
const DCFR_ALPHA: f64 = 1.5;
const DCFR_BETA: f64 = 0.0;
const DCFR_GAMMA: f64 = 2.0;

// --------------------------------------------------------------------------
// Model parameters (fixed sizing tree + dead money), in big blinds.
// --------------------------------------------------------------------------

const SB: f64 = 0.5;
const BB: f64 = 1.0;
/// Effective stack in bb (the all-in / 5-bet-jam terminal size).
const STACK: f64 = 100.0;
const OPEN_TO: f64 = 2.5;
/// 3-bet sizing as a multiple of the open.
const THREEBET_MULT: f64 = 3.2;
/// 4-bet sizing as a multiple of the 3-bet.
const FOURBET_MULT: f64 = 2.2;

// --------------------------------------------------------------------------
// Position / bucket enums (chart order)
// --------------------------------------------------------------------------

/// 6-max position in chart order.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Position {
    Utg,
    Mp,
    Co,
    Btn,
    Sb,
    Bb,
}

impl Position {
    pub const ALL: [Position; 6] = [
        Position::Utg,
        Position::Mp,
        Position::Co,
        Position::Btn,
        Position::Sb,
        Position::Bb,
    ];

    pub fn key(self) -> &'static str {
        match self {
            Position::Utg => "UTG",
            Position::Mp => "MP",
            Position::Co => "CO",
            Position::Btn => "BTN",
            Position::Sb => "SB",
            Position::Bb => "BB",
        }
    }

    /// Whether hero acts in position (closer to the button) in a typical
    /// confrontation. Used only to pick the realization band `R`.
    fn is_late(self) -> bool {
        matches!(self, Position::Co | Position::Btn)
    }
}

/// Action bucket in chart order.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Bucket {
    Rfi,
    FacingOpen,
    Vs3bet,
    Vs4bet,
    FacingLimp,
}

impl Bucket {
    pub const ALL: [Bucket; 5] = [
        Bucket::Rfi,
        Bucket::FacingOpen,
        Bucket::Vs3bet,
        Bucket::Vs4bet,
        Bucket::FacingLimp,
    ];

    pub fn key(self) -> &'static str {
        match self {
            Bucket::Rfi => "RFI",
            Bucket::FacingOpen => "facing_open",
            Bucket::Vs3bet => "vs_3bet",
            Bucket::Vs4bet => "vs_4bet",
            Bucket::FacingLimp => "facing_limp",
        }
    }
}

// --------------------------------------------------------------------------
// All-in equity matrix
// --------------------------------------------------------------------------

/// Parse a 169-grid key into (high, low, is_pair, is_suited).
fn parse_key(key: &str) -> Option<(Rank, Rank, bool, bool)> {
    let b = key.as_bytes();
    let r1 = rank_from_char(b[0] as char)?;
    if b.len() == 2 && b[0] == b[1] {
        return Some((r1, r1, true, false));
    }
    if b.len() == 3 {
        let r2 = rank_from_char(b[1] as char)?;
        let suited = match b[2] {
            b's' => true,
            b'o' => false,
            _ => return None,
        };
        let (hi, lo) = if r1 >= r2 { (r1, r2) } else { (r2, r1) };
        return Some((hi, lo, false, suited));
    }
    None
}

fn rank_from_char(ch: char) -> Option<Rank> {
    Rank::ALL.iter().copied().find(|r| r.char() == ch)
}

/// A representative concrete combo for a 169 key. Hero uses Spades/Hearts;
/// villain uses Diamonds/Clubs so the two reps never share a concrete card and the
/// per-cell showdown is always legal, deterministically.
///
/// HONESTY (F3, 2026-06-25) — FIXED REPRESENTATIVE COMBO, not class-averaged:
/// suits are irrelevant for vs-RANDOM equity (all cards unknown), but for a
/// specific class-vs-class showdown they are NOT strictly irrelevant — when both
/// hands SHARE a suit they slightly reduce each other's flush outs. Because hero is
/// always Spades/Hearts and villain always Diamonds/Clubs, a same-suit clash is
/// NEVER sampled, so each matrix cell is the equity of ONE representative combo
/// pair, not the average over all legal combo pairs for the class pair. The bias is
/// sub-1% and `combined_pct` is quantized to whole percent (then chart frequencies
/// rounded to 2 dp), so the PUBLISHED ranges are unaffected — but the matrix is
/// honestly "169×169 fixed-representative-combo all-in equities," not an exact
/// class-averaged matrix. (Class-averaging would loop legal combo pairs per
/// class-pair; deferred as not worth the build cost for a sub-quantum effect.)
fn rep_combo(key: &str, hero: bool) -> Option<HoleCards> {
    let (hi, lo, is_pair, suited) = parse_key(key)?;
    let (s_main, s_alt) = if hero {
        (Suit::Spades, Suit::Hearts)
    } else {
        (Suit::Diamonds, Suit::Clubs)
    };
    if is_pair {
        return Some(HoleCards::new(Card::new(hi, s_main), Card::new(lo, s_alt)));
    }
    if suited {
        Some(HoleCards::new(Card::new(hi, s_main), Card::new(lo, s_main)))
    } else {
        Some(HoleCards::new(Card::new(hi, s_main), Card::new(lo, s_alt)))
    }
}

/// Deterministic per-cell seed from class indices (so the full matrix is a fixed
/// sequence of independent estimates regardless of traversal order).
fn cell_seed(hi: usize, vi: usize) -> u64 {
    EQUITY_SEED
        .wrapping_add((hi as u64).wrapping_mul(0x9E37_79B9_7F4A_7C15))
        .wrapping_add((vi as u64).wrapping_mul(0xC2B2_AE3D_27D4_EB4F))
}

/// `eq[h][v]` = hero class `h`'s combined all-in equity (0..1) vs villain class
/// `v`, preflop, all five community cards to come.
pub struct EquityMatrix {
    /// Row-major `N_CLASSES * N_CLASSES`, equity in [0,1].
    eq: Vec<f64>,
    /// HU equity vs a uniformly random hand, per class (0..1). The base strength
    /// signal used for sanity checks.
    vs_random: Vec<f64>,
}

impl EquityMatrix {
    #[inline]
    pub fn get(&self, hero: usize, villain: usize) -> f64 {
        self.eq[hero * N_CLASSES + villain]
    }

    #[inline]
    pub fn vs_random(&self, class: usize) -> f64 {
        self.vs_random[class]
    }

    /// Build the deterministic 169×169 all-in-equity matrix. Single-threaded,
    /// deterministic. Exploits SYMMETRY — only the upper triangle (h < v) is
    /// estimated; the mirror cell is `eq[v][h] = 1 - eq[h][v]` (a preflop all-in
    /// is zero-sum in equity mass, the tiny chop term folded into the rounding).
    /// The diagonal (same class, distinct reps) is estimated directly. This
    /// halves the ~28.5k cells to ~14.3k. Determinism unaffected (fixed seeds).
    ///
    /// # Panics
    /// In debug builds, panics unless `keys.len() == N_CLASSES` (169). `keys` must
    /// be the canonical 169-class list (`all_hand_keys()`); other slices produce a
    /// mis-shaped matrix or an out-of-range access downstream (U26, dual-AI OSS
    /// review — pass `all_hand_keys()`).
    pub fn build(keys: &[String]) -> EquityMatrix {
        let n = keys.len();
        debug_assert_eq!(n, N_CLASSES);
        let mut eq = vec![0.0f64; n * n];
        let mut vs_random = vec![0.0f64; n];

        let reps_hero: Vec<HoleCards> = keys
            .iter()
            .map(|k| rep_combo(k, true).expect("valid 169 key"))
            .collect();
        let reps_vill: Vec<HoleCards> = keys
            .iter()
            .map(|k| rep_combo(k, false).expect("valid 169 key"))
            .collect();

        // vs-random per class.
        for h in 0..n {
            let r = equity(EquityInput {
                hero: reps_hero[h],
                board: BoardCards::empty(),
                opponents: OpponentSpec::Random(1),
                trials: EQUITY_TRIALS,
                // U63 (dual-AI OSS review): `usize::MAX / 2` as the villain-slot
                // sentinel is platform-width-dependent (differs on 32- vs 64-bit),
                // so byte-reproducibility of the generated charts is only
                // guaranteed on the build/CI/prod targets, which are all 64-bit.
                // A 32-bit build would produce a different vs_random seed.
                seed: cell_seed(h, usize::MAX / 2),
                // ADR-082: chart generation is NOT the coach — full trial count.
                early_stop: None,
            });
            vs_random[h] = r.combined_pct as f64 / 100.0;
        }

        // Upper triangle + diagonal; mirror the lower triangle.
        for h in 0..n {
            for v in h..n {
                let e = equity(EquityInput {
                    hero: reps_hero[h],
                    board: BoardCards::empty(),
                    opponents: OpponentSpec::Known(vec![reps_vill[v]]),
                    trials: EQUITY_TRIALS,
                    seed: cell_seed(h, v),
                    // ADR-082: chart generation is NOT the coach — full trial count.
                    early_stop: None,
                })
                .combined_pct as f64
                    / 100.0;
                eq[h * n + v] = e;
                if v != h {
                    eq[v * n + h] = 1.0 - e;
                }
            }
        }

        EquityMatrix { eq, vs_random }
    }
}

// --------------------------------------------------------------------------
// Equity realization factor R(class, line)
// --------------------------------------------------------------------------

/// Bounded equity-realization factor in `[0.70, 1.12]`, applied at see-flop
/// terminals. A DOCUMENTED model assumption (not solved):
///
/// * The aggressor / in-position side realizes MORE than raw equity (initiative
///   + position let it bet-fold-bluff and see free cards): `R > 1`.
/// * The out-of-position caller realizes LESS: `R < 1`.
/// * Pocket pairs realize well (set-mining implied odds); suited connectors
///   realize a touch better than offsuit junk (flush/straight realization).
/// * WEAK hands realize their equity POORLY — the bottom of a range gets bluffed
///   off, can't stack off, and folds out its equity (well-documented poker
///   theory). `R` therefore DECLINES with the hand's all-in equity vs random:
///   premiums realize ~full, marginal hands realize ~0.75. This equity tilt is
///   the dominant lever that keeps opening widths poker-sane.
///
/// `R ∈ [0.70, 1.12]`, a documented model assumption (not solved).
fn realization(
    class: usize,
    keys: &[String],
    matrix: &EquityMatrix,
    in_position: bool,
    is_aggressor: bool,
) -> f64 {
    let key = &keys[class];
    let base: f64 = match (in_position, is_aggressor) {
        (true, true) => 1.04,
        (true, false) => 0.98,
        (false, true) => 0.94,
        (false, false) => 0.86,
    };
    let mut r = base;

    // Equity tilt: hands far below "premium" realize less. Map vs-random equity
    // e∈[~0.32, ~0.86] to a tilt in [-0.18, +0.06] centred near e≈0.62 (a strong
    // broadway / mid pair). Weak hands (e≈0.4) lose ~0.12; premiums gain ~0.05.
    let e = matrix.vs_random(class);
    let tilt = ((e - 0.62) * 0.55).clamp(-0.18, 0.06);
    r += tilt;

    if let Some((hi, lo, is_pair, suited)) = parse_key(key) {
        if is_pair {
            // Set-mining implied odds: pairs realize well.
            r += 0.05;
        } else if suited {
            r += 0.03;
            let gap = (hi as i32) - (lo as i32);
            if gap <= 2 {
                r += 0.02; // suited connectors / one-gappers
            }
        } else if hi < Rank::Ten {
            // Offsuit junk realizes poorly (no flush, weak straights).
            r -= 0.04;
        }
    }
    r.clamp(0.70, 1.12)
}

// --------------------------------------------------------------------------
// The DCFR solver — one bucket = one 2-player extensive game.
// --------------------------------------------------------------------------

/// A solved per-(position,bucket) hero strategy: continue frequency per class.
pub struct SolvedBucket {
    /// `freq[class]` = hero's CONTINUE frequency (raise OR call, the bucket's
    /// non-fold action), in [0,1].
    pub freq: [f64; N_CLASSES],
}

/// Output of a full solve: strategies keyed by (position, bucket) chart order.
pub struct SolveResult {
    /// `[position][bucket]` (chart order). `None` for the structurally-empty
    /// buckets (BB RFI, UTG facing_open).
    pub buckets: Vec<Vec<Option<SolvedBucket>>>,
    pub matrix: EquityMatrix,
}

/// Run the full preflop DCFR solve and return per-(position,bucket) continue
/// frequencies. Deterministic given the fixed seeds + iteration count.
pub fn solve(keys: &[String]) -> SolveResult {
    let matrix = EquityMatrix::build(keys);
    solve_with_matrix(keys, matrix)
}

/// The class prior from combo counts (pair=6, suited=4, offsuit=12), normalized
/// to sum to 1. This is the "any two cards" dealing prior.
fn class_prior(keys: &[String]) -> Vec<f64> {
    let counts: Vec<f64> = keys.iter().map(|k| combos_for_key(k) as f64).collect();
    let sum: f64 = counts.iter().sum();
    counts.iter().map(|c| c / sum).collect()
}

/// A villain RANGE in this spot: per-class participation weight (NOT normalized
/// to 1 — it is `dealing_prior[class] * villain_continue_freq[class]`). The CFR
/// hero best-responds against this concrete range, which is what makes opening /
/// continuing junk -EV (the villain that is HERE holds a strong, narrow range).
type VillainRange = Vec<f64>;
fn solve_with_matrix(keys: &[String], matrix: EquityMatrix) -> SolveResult {
    let n = N_CLASSES;
    let prior = class_prior(keys);

    // A flat reach (full dealing prior) for spots where the villain is the field
    // collapsed to a representative defender (RFI / iso): villain is a single
    // opponent who will defend with a regret-matched range vs hero's open. The
    // CFR co-solves villain's defend, so we seed it with the dealing prior.
    let flat: VillainRange = prior.clone();

    // ---- Stage 1: RFI per position (villain = one representative defender) ----
    let mut rfi: Vec<Option<SolvedBucket>> = Vec::with_capacity(6);
    for &pos in &Position::ALL {
        if matches!(pos, Position::Bb) {
            rfi.push(None);
        } else {
            rfi.push(Some(solve_bucket(pos, Bucket::Rfi, keys, &matrix, &flat)));
        }
    }

    // The opener range hero faces in `facing_open` = the WIDEST opener (BTN RFI
    // continue reach) — a defender must continue vs the loosest realistic open.
    let widest_open = rfi[3]
        .as_ref()
        .map(continue_reach)
        .unwrap_or_else(|| flat.clone());

    // Escalated-line villain ranges are RAISE ranges, not continue ranges: when
    // villain 3-bets / 4-bets it holds a TIGHT value+bluff range, NOT every hand
    // it would continue with. We model these as the strongest `frac` of all
    // hands (by all-in equity vs random) — documented model parameters (the
    // villain 3-bet / 4-bet frequencies). A 4-bet range MUST be near-premium so
    // hero folds most hands facing it (poker fundamentals + the vs_4bet ⊆ vs_3bet
    // contract invariant).
    let villain_3bet = top_fraction_range(&matrix, &prior, THREEBET_FREQ);
    let villain_4bet = top_fraction_range(&matrix, &prior, FOURBET_FREQ);

    let mut out: Vec<Vec<Option<SolvedBucket>>> = (0..6).map(|_| Vec::with_capacity(5)).collect();

    for (pi, &pos) in Position::ALL.iter().enumerate() {
        for &bucket in &Bucket::ALL {
            let solved = match bucket {
                Bucket::Rfi => rfi[pi].as_ref().map(clone_bucket),
                Bucket::FacingOpen => {
                    if matches!(pos, Position::Utg) {
                        None
                    } else {
                        // Villain = the opener's range (widest RFI proxy).
                        Some(solve_bucket(
                            pos,
                            Bucket::FacingOpen,
                            keys,
                            &matrix,
                            &widest_open,
                        ))
                    }
                }
                // Hero opened, villain 3-bet → villain holds a tight 3-bet range.
                Bucket::Vs3bet => Some(solve_bucket(
                    pos,
                    Bucket::Vs3bet,
                    keys,
                    &matrix,
                    &villain_3bet,
                )),
                // Hero 3-bet, villain 4-bet → villain holds a near-premium 4-bet
                // range → hero must be very strong to continue.
                Bucket::Vs4bet => Some(solve_bucket(
                    pos,
                    Bucket::Vs4bet,
                    keys,
                    &matrix,
                    &villain_4bet,
                )),
                Bucket::FacingLimp => {
                    // Villain = a wide limp/call range (the loose limper) → iso
                    // is wide. Proxy with the full dealing prior.
                    Some(solve_bucket(pos, Bucket::FacingLimp, keys, &matrix, &flat))
                }
            };
            out[pi].push(solved);
        }
    }
    let _ = n;

    SolveResult {
        buckets: out,
        matrix,
    }
}

/// Villain 3-bet frequency (model parameter): the fraction of all hands that
/// constitutes a representative 3-bet range (value + bluff). ~11% of hands.
const THREEBET_FREQ: f64 = 0.11;
/// Villain 4-bet frequency (model parameter): a near-premium range. ~5.5%.
const FOURBET_FREQ: f64 = 0.055;
/// Minimum-defense-frequency floor (model parameter): facing a ~2.5x open the
/// field cannot fold more than `1 - MDF_FLOOR` per defender without becoming
/// exploitable. Caps the model's steal-equity credit so late-position opens stay
/// in a believable band. ~0.40 ≈ a conservative MDF for a small open.
const MDF_FLOOR: f64 = 0.40;

/// A villain RAISE range = the strongest `frac` of all hands by all-in equity vs
/// a random hand, weighted by the dealing prior. Deterministic (stable sort by
/// (equity desc, class index asc)). Returns participation weights per class.
fn top_fraction_range(matrix: &EquityMatrix, prior: &[f64], frac: f64) -> VillainRange {
    let n = N_CLASSES;
    // Order classes by vs-random equity, strongest first; ties broken by index.
    let mut order: Vec<usize> = (0..n).collect();
    order.sort_by(|&a, &b| {
        matrix
            .vs_random(b)
            .partial_cmp(&matrix.vs_random(a))
            .unwrap_or(std::cmp::Ordering::Equal)
            .then(a.cmp(&b))
    });
    let mut range = vec![0.0f64; n];
    let mut mass = 0.0;
    for &c in &order {
        if mass >= frac {
            break;
        }
        range[c] = prior[c];
        mass += prior[c];
    }
    range
}

/// The villain participation range a downstream bucket faces = the solved
/// CONTINUE reach of an upstream bucket: `dealing_prior[c] * continue_freq[c]`.
fn continue_reach(b: &SolvedBucket) -> VillainRange {
    // Recompute the dealing prior here (cheap) so this is self-contained.
    let keys = all_hand_keys();
    let prior = class_prior(&keys);
    (0..N_CLASSES).map(|c| prior[c] * b.freq[c]).collect()
}

fn clone_bucket(b: &SolvedBucket) -> SolvedBucket {
    SolvedBucket { freq: b.freq }
}

/// Number of players still to act BEHIND hero when hero open-raises (RFI / iso).
/// This is the position lever: UTG (5 behind) opens far tighter than BTN (2). An
/// open is contested if ANY of the `n` defenders wakes up, and the showdown is
/// then vs the STRONGEST contesting defender — a "best of N" effect that tightens
/// early-position opens naturally, NOT by a hand-tuned per-seat threshold.
fn players_behind(pos: Position) -> u32 {
    match pos {
        Position::Utg => 5, // MP, CO, BTN, SB, BB
        Position::Mp => 4,  // CO, BTN, SB, BB
        Position::Co => 3,  // BTN, SB, BB
        Position::Btn => 2, // SB, BB
        Position::Sb => 1,  // BB only
        Position::Bb => 0,  // BB never opens unopened
    }
}

/// Pot/invested accounting for a bucket's terminal nodes, in bb.
struct PotModel {
    /// Dead money in the pot before the contested decision (blinds + limp).
    dead: f64,
    /// Hero's invested-so-far when reaching the decision.
    hero_in: f64,
    /// Villain's invested-so-far when reaching the decision.
    vill_in: f64,
    /// Hero's TOTAL committed if hero CONTINUES (call/raise).
    hero_cont: f64,
    /// Villain's TOTAL committed in the modelled continuation.
    vill_cont: f64,
    /// Whether hero's continue is an ALL-IN (skip the R realization haircut).
    hero_allin: bool,
    /// Whether hero is in position for the realization band.
    hero_ip: bool,
    /// Whether hero is the aggressor (raiser) on the continue.
    hero_aggressor: bool,
    /// Number of independent defenders behind hero (RFI / iso only; 1 for the
    /// already-heads-up escalated lines). Drives the "best of N" position effect.
    n_defenders: u32,
    /// Whether the minimum-defense-frequency floor applies (open / iso steal
    /// spots where defenders get a price). False for the escalated 3bet/4bet
    /// lines, where MDF math is different.
    mdf_applies: bool,
}

/// Build the pot model for a (position, bucket) from the fixed sizing tree.
fn pot_model(pos: Position, bucket: Bucket) -> PotModel {
    let blinds = SB + BB;
    match bucket {
        Bucket::Rfi | Bucket::FacingLimp => {
            // facing_limp: a limper has matched the BB (≈1bb dead money). The
            // limper is a STICKY caller who realizes its equity, and the blind(s)
            // can still wake up behind — model 2 defenders so the iso needs
            // genuine equity (a sane ~30-50% iso, not "any two"). RFI: defenders
            // behind = the position lever.
            let extra = if matches!(bucket, Bucket::FacingLimp) {
                BB
            } else {
                0.0
            };
            let n_def = if matches!(bucket, Bucket::FacingLimp) {
                2
            } else {
                players_behind(pos).max(1)
            };
            PotModel {
                dead: blinds + extra,
                hero_in: 0.0,
                vill_in: BB, // the blind/limp a defender already posted (proxy)
                hero_cont: OPEN_TO,
                vill_cont: OPEN_TO, // a defender calls the open
                hero_allin: false,
                hero_ip: pos.is_late(),
                hero_aggressor: true,
                n_defenders: n_def,
                mdf_applies: true,
            }
        }
        Bucket::FacingOpen => {
            let open = OPEN_TO;
            let threebet = open * THREEBET_MULT;
            PotModel {
                dead: blinds,
                hero_in: 0.0,
                vill_in: open,
                hero_cont: threebet,
                vill_cont: threebet, // opener calls the 3-bet
                hero_allin: false,
                hero_ip: pos.is_late(),
                hero_aggressor: true,
                n_defenders: 1,
                mdf_applies: false,
            }
        }
        Bucket::Vs3bet => {
            let open = OPEN_TO;
            let threebet = open * THREEBET_MULT;
            let fourbet = threebet * FOURBET_MULT;
            PotModel {
                dead: blinds,
                hero_in: open,
                vill_in: threebet,
                hero_cont: fourbet,
                vill_cont: fourbet, // 3-bettor calls the 4-bet
                hero_allin: false,
                hero_ip: pos.is_late(),
                hero_aggressor: true,
                n_defenders: 1,
                mdf_applies: false,
            }
        }
        Bucket::Vs4bet => {
            // Facing a 4-bet at 100bb, hero's continue is a 5-bet JAM (all-in):
            // the dominant continuing action is to get it in with the nuts, not
            // flat. The all-in terminal uses raw all-in equity (NO R haircut) but
            // risks the FULL stack — so only hands with high equity vs the
            // near-premium 4-bet range continue. This makes vs_4bet the TIGHTEST
            // continuing bucket (vs_4bet ⊆ vs_3bet), as poker fundamentals
            // require.
            let open = OPEN_TO;
            let threebet = open * THREEBET_MULT;
            PotModel {
                dead: blinds,
                hero_in: threebet,
                vill_in: threebet * FOURBET_MULT,
                hero_cont: STACK, // hero jams all-in
                vill_cont: STACK, // villain (4-bettor) calls the jam
                hero_allin: true, // all-in terminal: no realization haircut
                hero_ip: pos.is_late(),
                hero_aggressor: true,
                n_defenders: 1,
                mdf_applies: false,
            }
        }
    }
}

/// Solve ONE (position, bucket) as a 2-player GENERAL-SUM extensive game via DCFR,
/// tracked by per-player best-response (NashConv) convergence — NOT a zero-sum
/// solve. HONESTY (F1, 2026-06-25): only the ALL-IN terminal (`pm.hero_allin`, the
/// vs_4bet 5-bet jam) is strictly zero-sum, because both sides realize RAW equity
/// (`e` and `1-e` sum to 1). The non-all-in SEE-FLOP terminal applies independent
/// per-player realization haircuts — hero share `e*R_hero`, villain share
/// `(1-e)*R_vill` — which do NOT sum to 1 when `R ≠ 1`, so that terminal (and
/// therefore the bucket as a whole) is GENERAL-SUM. DCFR / regret matching still
/// converges each player toward its own best response, and `bucket_exploitability`
/// reports the per-player best-response gap (a valid deviation-incentive /
/// NashConv measure in a general-sum game), but there is no single zero-sum "game
/// value" to quote. See `non_allin_terminal_is_general_sum` for the asserted
/// distinction from the all-in zero-sum line.
///
/// Players: HERO (the seat we publish a strategy for) and VILLAIN (the single
/// opponent that created this spot). Hero is dealt a class with the natural
/// combo-count prior. VILLAIN's participation per class is `villain_range[v]` —
/// a line-anchored weight (`dealing_prior[v] * upstream_continue_freq[v]`), so
/// in escalated lines (vs_3bet / vs_4bet) the villain holds a NARROW, premium
/// range and hero must be strong to continue. The villain ALSO best-responds
/// (fold/call) to hero's continue via its own regret matching, weighted by that
/// range — this is what models the opponent reaction.
///
/// Tree (from hero's decision):
///   hero: {FOLD, CONTINUE}
///     FOLD     → terminal: hero forfeits `hero_in`.
///     CONTINUE → villain: {FOLD, CALL}
///        villain FOLD → terminal: hero wins dead + villain's invested stake.
///        villain CALL → terminal: showdown EV from the matrix + R (general-sum
///                       at a see-flop terminal; zero-sum only when all-in).
///
/// We return hero's converged average CONTINUE frequency per class.
fn solve_bucket(
    pos: Position,
    bucket: Bucket,
    keys: &[String],
    matrix: &EquityMatrix,
    villain_range: &VillainRange,
) -> SolvedBucket {
    let full = solve_bucket_full(pos, bucket, keys, matrix, villain_range);
    SolvedBucket {
        freq: full.hero_freq,
    }
}

/// Both players' converged AVERAGE strategies for one (position, bucket), plus the
/// fixed model needed to measure exploitability against them. `hero_freq[h]` is
/// hero's average CONTINUE frequency; `vill_call[v]` is the villain's average CALL
/// frequency (only meaningful where `vrange[v] > 0`). `vrange` is the normalized
/// line-anchored villain participation distribution. The chart-published value is
/// exactly `hero_freq` — `solve_bucket` is a thin wrapper, so adding this does NOT
/// change any emitted frequency.
///
/// Most fields are read only by the test-only `bucket_exploitability`; in a
/// non-test build only `hero_freq` is consumed, so the rest are allowed dead.
#[cfg_attr(not(test), allow(dead_code))]
struct BucketSolution {
    hero_freq: [f64; N_CLASSES],
    /// Villain's converged average CALL frequency per class.
    vill_call: [f64; N_CLASSES],
    /// Normalized villain participation distribution (probability per class).
    vrange: Vec<f64>,
    /// The pot/sizing model for this bucket (terminal EV inputs).
    pm: PotModel,
    /// Hero's per-class realization multiplier (R), already 1.0 on all-in lines.
    hero_r: Vec<f64>,
    /// Villain's per-class realization multiplier (R).
    vill_r: Vec<f64>,
}

/// HERO's showdown-terminal bb EV vs a single villain class, given hero's raw
/// equity `e`, hero's realization multiplier `r`, whether the terminal is ALL-IN,
/// the `final_pot`, and hero's invested `cont`. ALL-IN ⇒ RAW equity (no `R`
/// haircut); see-flop ⇒ `(e·r)` clamped to a probability. THE single definition
/// of the hero terminal — both `solve_bucket_full_iters` and the (test-only)
/// `bucket_exploitability` call THIS, so they can never drift apart (F7).
#[inline]
fn hero_showdown_ev(e: f64, r: f64, allin: bool, final_pot: f64, cont: f64) -> f64 {
    let realized = if allin { e } else { (e * r).clamp(0.0, 1.0) };
    realized * final_pot - cont
}

/// VILLAIN's call-terminal bb EV vs a single hero class. `e` is the VILLAIN's raw
/// equity (`1 - matrix.get(hero, vill)`), `r` the villain realization multiplier.
/// F2/F7 (symmetry): when the terminal is ALL-IN (the vs_4bet 5-bet jam the
/// villain calls) BOTH sides realize RAW equity — NO `R` haircut on the villain
/// side either, exactly like `hero_showdown_ev`. Applying `R` only to the villain
/// on an all-in line was the asymmetry F2 fixed. THE single definition of the
/// villain terminal — the solver AND the exploitability measurement both call
/// THIS, so a regression to `(e·r)` on an all-in line is impossible to ship
/// unnoticed (F7 asserts this function directly).
#[inline]
fn villain_call_ev(e: f64, r: f64, allin: bool, final_pot: f64, cont: f64) -> f64 {
    let realized = if allin { e } else { (e * r).clamp(0.0, 1.0) };
    realized * final_pot - cont
}

fn solve_bucket_full(
    pos: Position,
    bucket: Bucket,
    keys: &[String],
    matrix: &EquityMatrix,
    villain_range: &VillainRange,
) -> BucketSolution {
    solve_bucket_full_iters(pos, bucket, keys, matrix, villain_range, CFR_ITERS)
}

fn solve_bucket_full_iters(
    pos: Position,
    bucket: Bucket,
    keys: &[String],
    matrix: &EquityMatrix,
    villain_range: &VillainRange,
    iters: u32,
) -> BucketSolution {
    let n = N_CLASSES;
    let pm = pot_model(pos, bucket);

    // Hero's dealing prior (combo count, normalized): pair=6, suited=4, offsuit=12.
    let prior = class_prior(keys);

    // Villain participation weight per class, normalized so it is a probability
    // distribution over the villain's actual range in this spot. A villain that
    // 4-bet holds a narrow range → this concentrates mass on premiums.
    let vr_sum: f64 = villain_range.iter().sum();
    let vrange: Vec<f64> = if vr_sum > 0.0 {
        villain_range.iter().map(|w| w / vr_sum).collect()
    } else {
        prior.clone()
    };

    // Hero realization multiplier per class (R); villain mirror (opponent side).
    let hero_r: Vec<f64> = (0..n)
        .map(|c| {
            if pm.hero_allin {
                1.0
            } else {
                realization(c, keys, matrix, pm.hero_ip, pm.hero_aggressor)
            }
        })
        .collect();
    let vill_r: Vec<f64> = (0..n)
        .map(|c| realization(c, keys, matrix, !pm.hero_ip, false))
        .collect();

    // Final pot if both commit `hero_cont` / `vill_cont` plus dead money.
    let final_pot = pm.dead + pm.hero_cont + pm.vill_cont;

    // ---- terminal EV helpers (each player's bb EV; GENERAL-SUM at a see-flop
    // terminal because hero/villain use independent `R`, ZERO-SUM only all-in) ----
    // Terminals go through the SHARED `hero_showdown_ev` / `villain_call_ev` so the
    // solver and `bucket_exploitability` can never diverge (F7). Both skip the `R`
    // realization haircut on an all-in line — the F2 symmetry.
    let hero_ev_showdown = |h: usize, v: usize| -> f64 {
        hero_showdown_ev(
            matrix.get(h, v),
            hero_r[h],
            pm.hero_allin,
            final_pot,
            pm.hero_cont,
        )
    };
    // Villain folds → hero wins the whole pot (dead + both stakes) and gets their
    // own `hero_in` back, so hero's NET is `dead + vill_in`. U06 (dual-AI OSS
    // review): the prior `- pm.hero_in` double-subtracted hero's investment,
    // understating fold equity and skewing the vs_3bet/vs_4bet continue ranges.
    // Matches the module spec ("hero wins dead + villain's invested stake").
    let hero_ev_vill_fold = pm.dead + pm.vill_in;
    let hero_ev_fold = -pm.hero_in;
    // Chip conservation at the fold terminal: hero's gain + villain's loss = dead.
    debug_assert!(
        (hero_ev_vill_fold + (-pm.vill_in) - pm.dead).abs() < 1e-9,
        "fold terminal violates chip conservation: {} + {} != {}",
        hero_ev_vill_fold,
        -pm.vill_in,
        pm.dead
    );

    let vill_ev_call = |h: usize, v: usize| -> f64 {
        villain_call_ev(
            1.0 - matrix.get(h, v), // villain's raw equity vs hero class
            vill_r[v],
            pm.hero_allin,
            final_pot,
            pm.vill_cont,
        )
    };
    let vill_ev_fold = -pm.vill_in;

    // ---- DCFR state ----
    // Hero: 2 actions {fold=0, continue=1}. Villain: 2 actions {fold=0, call=1}.
    let mut hero_regret = vec![[0.0f64; 2]; n];
    let mut hero_strat_sum = vec![[0.0f64; 2]; n];
    let mut vill_regret = vec![[0.0f64; 2]; n];
    let mut vill_strat_sum = vec![[0.0f64; 2]; n];

    let regret_match = |r: &[f64; 2]| -> [f64; 2] {
        let p0 = r[0].max(0.0);
        let p1 = r[1].max(0.0);
        let s = p0 + p1;
        if s > 0.0 {
            [p0 / s, p1 / s]
        } else {
            [0.5, 0.5]
        }
    };

    for t in 1..=iters {
        let tf = t as f64;
        let pos_disc = {
            let ta = tf.powf(DCFR_ALPHA);
            ta / (ta + 1.0)
        };
        let neg_disc = {
            let tb = tf.powf(DCFR_BETA);
            tb / (tb + 1.0)
        };
        let strat_disc = (tf / (tf + 1.0)).powf(DCFR_GAMMA);

        let hero_strat: Vec<[f64; 2]> = hero_regret.iter().map(regret_match).collect();
        let vill_strat: Vec<[f64; 2]> = vill_regret.iter().map(regret_match).collect();

        // Per-defender field stats vs the LINE-ANCHORED villain range `vrange`:
        //  * `p_fold` — one defender's probability of folding (mass-weighted).
        //  * `call_dist[v]` — the contesting-defender class distribution
        //    (vrange × call), normalized; the showdown is vs a hand from here.
        // With `n_defenders` independent defenders the open is uncontested only
        // if ALL fold (`p_fold^N`), and a contest faces the STRONGEST of N —
        // approximated by `best_of_n` sharpening of `call_dist` toward villain
        // classes with higher equity vs hero. This is the position lever.
        let nd = pm.n_defenders.max(1);
        let mut p_fold_one = 0.0;
        let mut call_mass = 0.0;
        for v in 0..n {
            let pv = vrange[v];
            if pv <= 0.0 {
                continue;
            }
            p_fold_one += pv * vill_strat[v][0];
            call_mass += pv * vill_strat[v][1];
        }
        // Minimum-defense-frequency (MDF) floor — a documented model parameter.
        // Facing a 2.5x open the field gets a price and CANNOT fold more than its
        // MDF without becoming exploitable; capping `p_fold_one` at `1 - MDF`
        // removes the model's tendency to over-credit steal equity and keeps
        // late-position opens in a believable band. Applied only when hero is the
        // pre-emptive raiser facing defenders (RFI / iso), where MDF applies.
        if pm.mdf_applies && nd >= 1 {
            p_fold_one = p_fold_one.min(1.0 - MDF_FLOOR);
        }
        let p_all_fold = p_fold_one.powi(nd as i32);

        // --- Hero update ---
        for h in 0..n {
            // Showdown EV vs a contesting defender, drawn from the call dist and,
            // for N>1, biased toward the part of the range that beats hero
            // (best-of-N order statistic, approximated by an equity-power weight).
            let ev_fold = hero_ev_fold;
            let ev_cont = if call_mass <= 0.0 {
                // No defender ever continues → open always steals.
                hero_ev_vill_fold
            } else {
                let mut sd = 0.0;
                let mut wsum = 0.0;
                for v in 0..n {
                    let pv = vrange[v] * vill_strat[v][1];
                    if pv <= 0.0 {
                        continue;
                    }
                    // best-of-N: weight a contesting class by its strength vs
                    // hero raised to (N-1). For N=1 this is the plain call dist.
                    let v_eq_vs_hero = 1.0 - matrix.get(h, v);
                    let w = pv * v_eq_vs_hero.powi((nd - 1) as i32);
                    sd += w * hero_ev_showdown(h, v);
                    wsum += w;
                }
                let contest_ev = if wsum > 0.0 {
                    sd / wsum
                } else {
                    hero_ev_vill_fold
                };
                p_all_fold * hero_ev_vill_fold + (1.0 - p_all_fold) * contest_ev
            };
            let strat = hero_strat[h];
            let node_ev = strat[0] * ev_fold + strat[1] * ev_cont;
            let regrets = [ev_fold - node_ev, ev_cont - node_ev];
            for a in 0..2 {
                let disc = if regrets[a] >= 0.0 {
                    pos_disc
                } else {
                    neg_disc
                };
                hero_regret[h][a] = (hero_regret[h][a] + regrets[a]) * disc;
                hero_strat_sum[h][a] += strat_disc * prior[h] * strat[a];
            }
        }

        // --- Villain update --- (villain only faces hero's CONTINUE range; its
        // own participation is weighted by the line-anchored `vrange`).
        let hero_cont_reach: Vec<f64> = (0..n).map(|h| prior[h] * hero_strat[h][1]).collect();
        let cont_mass: f64 = hero_cont_reach.iter().sum();
        for v in 0..n {
            if vrange[v] <= 0.0 {
                continue;
            }
            let mut ev_call = 0.0;
            if cont_mass > 0.0 {
                for (h, &rw) in hero_cont_reach.iter().enumerate() {
                    if rw <= 0.0 {
                        continue;
                    }
                    ev_call += rw * vill_ev_call(h, v);
                }
                ev_call /= cont_mass;
            }
            let ev_fold = vill_ev_fold;
            let strat = vill_strat[v];
            let node_ev = strat[0] * ev_fold + strat[1] * ev_call;
            let regrets = [ev_fold - node_ev, ev_call - node_ev];
            // Reach weight for the average strategy: the villain's range mass ×
            // how often it is actually facing a continue this iteration.
            let reach = vrange[v] * cont_mass.max(1e-12);
            for a in 0..2 {
                let disc = if regrets[a] >= 0.0 {
                    pos_disc
                } else {
                    neg_disc
                };
                vill_regret[v][a] = (vill_regret[v][a] + regrets[a]) * disc;
                vill_strat_sum[v][a] += strat_disc * reach * strat[a];
            }
        }
    }

    // Average hero strategy → continue frequency per class.
    let mut hero_freq = [0.0f64; N_CLASSES];
    for (h, slot) in hero_freq.iter_mut().enumerate() {
        let s = hero_strat_sum[h][0] + hero_strat_sum[h][1];
        *slot = if s > 0.0 {
            hero_strat_sum[h][1] / s
        } else {
            0.0
        };
    }
    // Average villain strategy → call frequency per class (used by the
    // exploitability measurement; does NOT affect the published hero chart).
    let mut vill_call = [0.0f64; N_CLASSES];
    for (v, slot) in vill_call.iter_mut().enumerate() {
        let s = vill_strat_sum[v][0] + vill_strat_sum[v][1];
        *slot = if s > 0.0 {
            vill_strat_sum[v][1] / s
        } else {
            0.0
        };
    }
    BucketSolution {
        hero_freq,
        vill_call,
        vrange,
        pm,
        hero_r,
        vill_r,
    }
}

// --------------------------------------------------------------------------
// Exploitability measurement (best response vs the converged average pair).
// --------------------------------------------------------------------------

/// Decomposed best-response gaps for one solved bucket, in bb, measured AGAINST
/// THE SAME simplified model the solver optimized (NOT real poker — see the
/// module docstring). Each gap is how much a player could win by switching to a
/// best response while the OTHER player keeps its converged average strategy:
///
/// ```text
/// hero_gap = BR_hero_value − hero_avg_value   (>= 0)
/// vill_gap = BR_vill_value − vill_avg_value   (>= 0)
/// ```
///
/// HONESTY — why the split matters: the PUBLISHED artifact is the HERO strategy,
/// so `hero_gap` is the exploitability of what we actually ship. `vill_gap` is the
/// opponent's best-response gap; in the tightest line (`vs_4bet`) hero continues
/// only ~3.7% of the time, so the villain's call/fold node is almost OFF-PATH and
/// its residual NashConv is concentrated on a node that is rarely reached — a
/// well-known CFR property, not a defect of the published hero chart. We therefore
/// report BOTH the raw `vill_gap` AND a REACH-WEIGHTED total (`vill_gap` scaled by
/// hero's continue mass, i.e. the probability the villain node is actually
/// reached), which honestly reflects on-path exploitability.
#[cfg(test)]
struct Exploitability {
    /// Hero best-response gap (exploitability of the PUBLISHED strategy), bb.
    hero_gap: f64,
    /// Villain best-response gap, bb (raw, includes rarely-reached nodes).
    vill_gap: f64,
    /// Probability hero reaches the villain decision (hero's continue mass).
    hero_cont_mass: f64,
}

#[cfg(test)]
impl Exploitability {
    /// Reach-weighted NashConv, bb: hero gap (always on-path) + villain gap scaled
    /// by how often hero actually continues to the villain node.
    fn reach_weighted(&self) -> f64 {
        self.hero_gap + self.vill_gap * self.hero_cont_mass
    }
}

/// Compute the decomposed exploitability of a solved bucket. Reuses the EXACT same
/// fold / vill-fold / showdown terminal definitions as `solve_bucket_full` (incl.
/// the F2 all-in symmetry and the best-of-N defender field model), so the numbers
/// are the regrets the solver itself drove toward zero.
#[cfg(test)]
fn bucket_exploitability(sol: &BucketSolution, matrix: &EquityMatrix) -> Exploitability {
    let n = N_CLASSES;
    let pm = &sol.pm;
    let prior = class_prior(&all_hand_keys());

    let final_pot = pm.dead + pm.hero_cont + pm.vill_cont;
    let hero_ev_fold = -pm.hero_in;
    // U06: villain fold pays hero the whole pot; net = dead + vill_in (see solver).
    let hero_ev_vill_fold = pm.dead + pm.vill_in;
    let vill_ev_fold = -pm.vill_in;

    // SAME shared terminal definitions the solver used (F7) — incl. the F2 all-in
    // raw-equity symmetry — so the measured gaps are the regrets the solver itself
    // drove toward zero, not a re-derived (and potentially divergent) copy.
    let hero_ev_showdown = |h: usize, v: usize| -> f64 {
        hero_showdown_ev(
            matrix.get(h, v),
            sol.hero_r[h],
            pm.hero_allin,
            final_pot,
            pm.hero_cont,
        )
    };
    let vill_ev_call = |h: usize, v: usize| -> f64 {
        villain_call_ev(
            1.0 - matrix.get(h, v),
            sol.vill_r[v],
            pm.hero_allin,
            final_pot,
            pm.vill_cont,
        )
    };

    let nd = pm.n_defenders.max(1);

    // ---- Field stats vs the villain AVERAGE strategy (mirrors the solver) ----
    let mut p_fold_one = 0.0;
    for v in 0..n {
        let pv = sol.vrange[v];
        if pv <= 0.0 {
            continue;
        }
        p_fold_one += pv * (1.0 - sol.vill_call[v]);
    }
    if pm.mdf_applies && nd >= 1 {
        p_fold_one = p_fold_one.min(1.0 - MDF_FLOOR);
    }
    let p_all_fold = p_fold_one.powi(nd as i32);
    let call_mass: f64 = (0..n).map(|v| sol.vrange[v] * sol.vill_call[v]).sum();

    // Hero's CONTINUE EV per class vs the villain average strategy.
    let hero_cont_ev = |h: usize| -> f64 {
        if call_mass <= 0.0 {
            return hero_ev_vill_fold;
        }
        let mut sd = 0.0;
        let mut wsum = 0.0;
        for v in 0..n {
            let pv = sol.vrange[v] * sol.vill_call[v];
            if pv <= 0.0 {
                continue;
            }
            let v_eq_vs_hero = 1.0 - matrix.get(h, v);
            let w = pv * v_eq_vs_hero.powi((nd - 1) as i32);
            sd += w * hero_ev_showdown(h, v);
            wsum += w;
        }
        let contest_ev = if wsum > 0.0 {
            sd / wsum
        } else {
            hero_ev_vill_fold
        };
        p_all_fold * hero_ev_vill_fold + (1.0 - p_all_fold) * contest_ev
    };

    // Hero: average value vs best-response value (prior-weighted).
    let mut hero_avg = 0.0;
    let mut hero_br = 0.0;
    for (h, &f) in sol.hero_freq.iter().enumerate() {
        let cont = hero_cont_ev(h);
        hero_avg += prior[h] * (f * cont + (1.0 - f) * hero_ev_fold);
        hero_br += prior[h] * cont.max(hero_ev_fold);
    }

    // Villain faces hero's CONTINUE reach (prior × hero continue freq).
    let hero_cont_reach: Vec<f64> = (0..n).map(|h| prior[h] * sol.hero_freq[h]).collect();
    let cont_mass: f64 = hero_cont_reach.iter().sum();
    let vill_call_ev = |v: usize| -> f64 {
        if cont_mass <= 0.0 {
            return 0.0;
        }
        let mut ev = 0.0;
        for (h, &rw) in hero_cont_reach.iter().enumerate() {
            if rw <= 0.0 {
                continue;
            }
            ev += rw * vill_ev_call(h, v);
        }
        ev / cont_mass
    };

    // Villain: average value vs best-response value (vrange-weighted).
    let mut vill_avg = 0.0;
    let mut vill_br = 0.0;
    for v in 0..n {
        let pv = sol.vrange[v];
        if pv <= 0.0 {
            continue;
        }
        let call = vill_call_ev(v);
        let c = sol.vill_call[v];
        vill_avg += pv * (c * call + (1.0 - c) * vill_ev_fold);
        vill_br += pv * call.max(vill_ev_fold);
    }

    Exploitability {
        hero_gap: hero_br - hero_avg,
        vill_gap: vill_br - vill_avg,
        hero_cont_mass: cont_mass,
    }
}

/// Combo count for a 169 key (pair=6, suited=4, offsuit=12).
fn combos_for_key(key: &str) -> u32 {
    let b = key.as_bytes();
    match b.len() {
        2 if b[0] == b[1] => 6,
        3 if b[2] == b's' => 4,
        3 if b[2] == b'o' => 12,
        _ => 0,
    }
}

// --------------------------------------------------------------------------
// Convenience: the canonical key order (re-exported for the generator/tests).
// --------------------------------------------------------------------------

/// Canonical 169 hand-key order (pairs, suited, offsuit), from
/// `preflop_charts::all_hand_keys`.
pub fn canonical_keys() -> Vec<String> {
    all_hand_keys()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn key_index(keys: &[String], k: &str) -> usize {
        keys.iter().position(|s| s == k).expect("key present")
    }

    /// Reconstruct, for the exploitability tests, the EXACT per-(position,bucket)
    /// villain range that the PUBLISHED solve (`solve_with_matrix`) uses, and the
    /// list of every NON-empty published (position,bucket) pair. F5 (2026-06-25):
    /// the old exploitability test hard-coded `flat` for `facing_open`, but the
    /// shipped `facing_open` solve faces `widest_open` (BTN RFI continue reach), so
    /// it measured a DIFFERENT solve than what ships. This rebuilds the upstream
    /// ranges identically (flat / widest_open / villain_3bet / villain_4bet) so the
    /// measured exploitability is of the ACTUAL published strategy.
    struct PublishedRanges {
        flat: VillainRange,
        widest_open: VillainRange,
        villain_3bet: VillainRange,
        villain_4bet: VillainRange,
    }

    impl PublishedRanges {
        fn build(keys: &[String], matrix: &EquityMatrix) -> Self {
            let prior = class_prior(keys);
            let flat: VillainRange = prior.clone();
            // widest_open = BTN RFI continue reach (matches solve_with_matrix).
            let btn_rfi = solve_bucket(Position::Btn, Bucket::Rfi, keys, matrix, &flat);
            let widest_open = continue_reach(&btn_rfi);
            let villain_3bet = top_fraction_range(matrix, &prior, THREEBET_FREQ);
            let villain_4bet = top_fraction_range(matrix, &prior, FOURBET_FREQ);
            PublishedRanges {
                flat,
                widest_open,
                villain_3bet,
                villain_4bet,
            }
        }

        /// The villain range for one published (position,bucket) — `None` for the
        /// structurally-empty cells (BB RFI, UTG facing_open), mirroring exactly the
        /// dispatch in `solve_with_matrix`.
        fn villain_range(&self, pos: Position, bucket: Bucket) -> Option<&VillainRange> {
            match bucket {
                Bucket::Rfi => {
                    if matches!(pos, Position::Bb) {
                        None
                    } else {
                        Some(&self.flat)
                    }
                }
                Bucket::FacingOpen => {
                    if matches!(pos, Position::Utg) {
                        None
                    } else {
                        Some(&self.widest_open)
                    }
                }
                Bucket::Vs3bet => Some(&self.villain_3bet),
                Bucket::Vs4bet => Some(&self.villain_4bet),
                Bucket::FacingLimp => Some(&self.flat),
            }
        }
    }

    /// One consolidated test that builds the (expensive) all-in-equity matrix
    /// ONCE and exercises every property: matrix sanity, R bands, determinism,
    /// premiums/trash, and the position width lever (UTG opens tighter than BTN).
    /// Kept as a single `#[test]` so `cargo test -p engine` rebuilds the matrix
    /// only once instead of per-test (it is the dominant cost in debug).
    #[test]
    fn cfr_solver_matrix_and_strategies_are_sound() {
        let keys = canonical_keys();
        let m = EquityMatrix::build(&keys);
        let vr = class_prior(&keys);

        // --- Matrix sanity: AA dominates 72o, both ways; vs-random ordering. ---
        let aa = key_index(&keys, "AA");
        let kk = key_index(&keys, "KK");
        let aks = key_index(&keys, "AKs");
        let t72 = key_index(&keys, "72o");
        assert!(
            m.get(aa, t72) > 0.80,
            "AA vs 72o all-in equity should be >0.80, got {}",
            m.get(aa, t72)
        );
        assert!(
            m.get(t72, aa) < 0.20,
            "72o vs AA all-in equity should be <0.20, got {}",
            m.get(t72, aa)
        );
        assert!(m.vs_random(aa) > 0.80 && m.vs_random(t72) < 0.40);

        // --- R bands bounded for every class / position / aggressor combo. ---
        for c in 0..N_CLASSES {
            for &ip in &[true, false] {
                for &agg in &[true, false] {
                    let r = realization(c, &keys, &m, ip, agg);
                    assert!(
                        (0.70..=1.12).contains(&r),
                        "R out of band for {} ip={ip} agg={agg}: {r}",
                        keys[c]
                    );
                }
            }
        }

        // --- Determinism: same matrix + same args ⇒ bit-identical strategy. ---
        let a = solve_bucket(Position::Btn, Bucket::Rfi, &keys, &m, &vr);
        let b = solve_bucket(Position::Btn, Bucket::Rfi, &keys, &m, &vr);
        for c in 0..N_CLASSES {
            assert_eq!(a.freq[c].to_bits(), b.freq[c].to_bits(), "freq diff at {c}");
        }

        // --- UTG RFI: premiums open, trash folds. ---
        let utg = solve_bucket(Position::Utg, Bucket::Rfi, &keys, &m, &vr);
        assert!(utg.freq[aa] > 0.9, "AA must open UTG, got {}", utg.freq[aa]);
        assert!(utg.freq[kk] > 0.9, "KK must open UTG, got {}", utg.freq[kk]);
        assert!(
            utg.freq[aks] > 0.9,
            "AKs must open UTG, got {}",
            utg.freq[aks]
        );
        assert!(
            utg.freq[t72] < 0.1,
            "72o must fold UTG, got {}",
            utg.freq[t72]
        );

        // --- Position lever: BTN opens strictly wider than UTG. `b` is BTN RFI. ---
        let count = |s: &SolvedBucket| s.freq.iter().filter(|&&f| f >= 0.5).count();
        assert!(
            count(&b) > count(&utg),
            "BTN ({}) must open wider than UTG ({})",
            count(&b),
            count(&utg)
        );
    }

    /// Convergence smoke check (honesty): the module + `CFR_ITERS` doc claim the
    /// converged average strategy is "low-exploitability of this simplified game."
    /// This MEASURES that on a REPRESENTATIVE (position, bucket) per bucket kind —
    /// computing a per-player best-response gap against the SAME model the solver
    /// optimized (`bucket_exploitability`) and asserting a documented threshold at
    /// `CFR_ITERS`. The villain ranges are reconstructed to EXACTLY match the
    /// published solve (F5: `facing_open` faces `widest_open`, not `flat`). The
    /// EXHAUSTIVE per-published-bucket version is
    /// `dcfr_exploitability_covers_all_published_buckets`.
    ///
    /// HONESTY — what is and is NOT low here (measured during authoring, 2000 iters;
    /// magnitudes are indicative, the asserted bounds are the contract):
    ///
    /// | bucket          | hero_gap (published) | vill_gap (raw) | hero_cont_mass | reach-weighted |
    /// |-----------------|----------------------|----------------|----------------|----------------|
    /// | BTN RFI         | small                | small          | ~0.66          | small          |
    /// | CO facing_open  | small                | small          | ~0.5           | small          |
    /// | MP vs_3bet      | small                | small          | ~0.07          | small          |
    /// | CO vs_4bet      | small                | large (~0.66)  | ~0.037         | small          |
    /// | BTN facing_limp | small                | small          | ~0.66          | small          |
    ///
    /// The PUBLISHED artifact is the HERO strategy, and its gap (`hero_gap`) is small
    /// in EVERY bucket. The one large raw number — the villain best response in
    /// `vs_4bet` — is concentrated on a node hero reaches only ~3.7% of the time (a
    /// near-OFF-PATH 5-bet-jam call), so on a reach-weighted basis it is small too.
    /// We assert BOTH: a bound on the published hero strategy AND a looser bound on
    /// reach-weighted NashConv — and DOCUMENT the raw villain off-path residual
    /// rather than hiding it. All numbers are the gap of the SIMPLIFIED MODEL, NOT
    /// real-poker exploitability (module docstring HONESTY).
    #[test]
    fn dcfr_exploitability_is_low_at_configured_iters() {
        /// Documented ceiling on the PUBLISHED hero strategy's best-response gap, bb.
        const MAX_HERO_GAP_BB: f64 = 0.05;
        /// Documented ceiling on reach-weighted NashConv (on-path exploitability), bb.
        const MAX_REACH_WEIGHTED_BB: f64 = 0.10;

        let keys = canonical_keys();
        let matrix = EquityMatrix::build(&keys);
        // F5: villain ranges reconstructed to EXACTLY match the published solve
        // (facing_open faces `widest_open`, NOT `flat`).
        let pr = PublishedRanges::build(&keys, &matrix);

        // A representative published (position,bucket) per bucket kind.
        let cases: [(Position, Bucket); 5] = [
            (Position::Btn, Bucket::Rfi),
            (Position::Co, Bucket::FacingOpen),
            (Position::Mp, Bucket::Vs3bet),
            (Position::Co, Bucket::Vs4bet),
            (Position::Btn, Bucket::FacingLimp),
        ];

        for (pos, bucket) in cases {
            let vr = pr
                .villain_range(pos, bucket)
                .expect("representative case is a published bucket");
            let sol = solve_bucket_full(pos, bucket, &keys, &matrix, vr);
            let e = bucket_exploitability(&sol, &matrix);
            // Best-response gaps are non-negative by construction.
            assert!(
                e.hero_gap >= -1e-9 && e.vill_gap >= -1e-9,
                "best-response gap must be >= 0 for {pos:?}/{bucket:?}: \
                 hero={:.6} vill={:.6}",
                e.hero_gap,
                e.vill_gap
            );
            // The PUBLISHED (hero) strategy is low-exploitability in EVERY bucket.
            assert!(
                e.hero_gap < MAX_HERO_GAP_BB,
                "published hero strategy NOT converged for {pos:?}/{bucket:?}: \
                 hero_gap {:.5} bb ≥ {MAX_HERO_GAP_BB} bb at CFR_ITERS={CFR_ITERS}",
                e.hero_gap
            );
            // On-path (reach-weighted) NashConv is low in EVERY bucket too.
            assert!(
                e.reach_weighted() < MAX_REACH_WEIGHTED_BB,
                "reach-weighted NashConv NOT low for {pos:?}/{bucket:?}: {:.5} bb \
                 ≥ {MAX_REACH_WEIGHTED_BB} bb (hero_gap={:.5}, vill_gap={:.5}, \
                 hero_cont_mass={:.5}) at CFR_ITERS={CFR_ITERS}",
                e.reach_weighted(),
                e.hero_gap,
                e.vill_gap,
                e.hero_cont_mass
            );
        }
    }

    /// F5 (RED-first, 2026-06-25): the representative test above only checks ONE
    /// (position, bucket) per bucket kind, and the prior version measured
    /// `facing_open` against the WRONG villain range (`flat` instead of the shipped
    /// `widest_open`), so the published `facing_open` strategy's exploitability was
    /// unverified. This iterates EVERY non-empty published (position, bucket) — all
    /// six positions × five buckets minus the two structurally-empty cells (BB RFI,
    /// UTG facing_open) = 28 buckets — rebuilding each one's villain range EXACTLY as
    /// `solve_with_matrix` does, and asserts the published HERO strategy's
    /// best-response gap is low in ALL of them (with the same documented
    /// reach-weighted bound on on-path NashConv). This is the artifact that actually
    /// ships, so this is the honest convergence guarantee.
    #[test]
    fn dcfr_exploitability_covers_all_published_buckets() {
        /// Ceiling on the PUBLISHED hero strategy's best-response gap, bb. Looser
        /// than the representative test's 0.05 because some escalated lines (vs_3bet
        /// off the tightest seats) carry a slightly larger residual at 2000 iters;
        /// still a small fraction of a big blind.
        const MAX_HERO_GAP_BB: f64 = 0.08;
        /// Ceiling on reach-weighted NashConv (on-path exploitability), bb.
        const MAX_REACH_WEIGHTED_BB: f64 = 0.12;

        let keys = canonical_keys();
        let matrix = EquityMatrix::build(&keys);
        let pr = PublishedRanges::build(&keys, &matrix);

        let mut checked = 0usize;
        for &pos in &Position::ALL {
            for &bucket in &Bucket::ALL {
                let Some(vr) = pr.villain_range(pos, bucket) else {
                    // Structurally-empty published cell (BB RFI, UTG facing_open).
                    continue;
                };
                let sol = solve_bucket_full(pos, bucket, &keys, &matrix, vr);
                let e = bucket_exploitability(&sol, &matrix);
                assert!(
                    e.hero_gap >= -1e-9 && e.vill_gap >= -1e-9,
                    "best-response gap must be >= 0 for {pos:?}/{bucket:?}: \
                     hero={:.6} vill={:.6}",
                    e.hero_gap,
                    e.vill_gap
                );
                assert!(
                    e.hero_gap < MAX_HERO_GAP_BB,
                    "published hero strategy NOT converged for {pos:?}/{bucket:?}: \
                     hero_gap {:.5} bb ≥ {MAX_HERO_GAP_BB} bb at CFR_ITERS={CFR_ITERS}",
                    e.hero_gap
                );
                assert!(
                    e.reach_weighted() < MAX_REACH_WEIGHTED_BB,
                    "reach-weighted NashConv NOT low for {pos:?}/{bucket:?}: {:.5} bb \
                     ≥ {MAX_REACH_WEIGHTED_BB} bb (hero_gap={:.5}, vill_gap={:.5}, \
                     hero_cont_mass={:.5}) at CFR_ITERS={CFR_ITERS}",
                    e.reach_weighted(),
                    e.hero_gap,
                    e.vill_gap,
                    e.hero_cont_mass
                );
                checked += 1;
            }
        }
        // 6 positions × 5 buckets − 2 empty cells (BB RFI, UTG facing_open) = 28.
        assert_eq!(
            checked, 28,
            "must cover every non-empty published bucket (got {checked})"
        );
    }

    /// Map a CFR `Position`/`Bucket` to the chart string keys (they share the same
    /// labels — "UTG"/"RFI"/etc), so the test can read the SHIPPED, post-processed
    /// frequencies straight out of the committed `preflop_v2.json`.
    fn shipped_hero_freq(pos: Position, bucket: Bucket, keys: &[String]) -> [f64; N_CLASSES] {
        use crate::solver::preflop_charts::{self, ActionBucket, PositionBucket};
        let pb = match pos {
            Position::Utg => PositionBucket::UTG,
            Position::Mp => PositionBucket::MP,
            Position::Co => PositionBucket::CO,
            Position::Btn => PositionBucket::BTN,
            Position::Sb => PositionBucket::SB,
            Position::Bb => PositionBucket::BB,
        };
        let ab = match bucket {
            Bucket::Rfi => ActionBucket::Rfi,
            Bucket::FacingOpen => ActionBucket::FacingOpen,
            Bucket::Vs3bet => ActionBucket::Vs3bet,
            Bucket::Vs4bet => ActionBucket::Vs4bet,
            Bucket::FacingLimp => ActionBucket::FacingLimp,
        };
        let mut f = [0.0f64; N_CLASSES];
        for (ci, key) in keys.iter().enumerate() {
            if let Some(cell) = preflop_charts::lookup(pb, ab, key) {
                f[ci] = cell.frequency as f64;
            }
        }
        f
    }

    /// F5 (2026-06-25): the convergence tests above measure the RAW average
    /// strategy out of `solve_bucket_full`. But the artifact that actually SHIPS is
    /// the POST-PROCESSED `preflop_v2.json` (round-to-2dp, trash→0, ALWAYS_IN→1.0,
    /// INCLUDE_FLOOR, enforce_invariants). The post-processing can move
    /// frequencies away from the converged strategy, so the shipped bytes'
    /// exploitability was UNMEASURED. This loads the SHIPPED hero frequencies
    /// straight from the committed JSON and measures THEIR best-response gap
    /// against the SAME simplified model (the solved villain side + pot model + R),
    /// for every non-empty published bucket. This is the honest convergence claim
    /// for "the artifact that actually ships".
    #[test]
    fn shipped_postprocessed_chart_is_low_exploitability() {
        /// Ceiling on the SHIPPED (post-processed) hero strategy's best-response
        /// gap, bb. Looser than the raw-strategy bound because rounding +
        /// force-include/floor perturb the converged frequencies; still a small
        /// fraction of a big blind, i.e. the published bytes are near-equilibrium.
        const MAX_SHIPPED_HERO_GAP_BB: f64 = 0.20;

        let keys = canonical_keys();
        let matrix = EquityMatrix::build(&keys);
        let pr = PublishedRanges::build(&keys, &matrix);

        let mut checked = 0usize;
        for &pos in &Position::ALL {
            for &bucket in &Bucket::ALL {
                let Some(vr) = pr.villain_range(pos, bucket) else {
                    continue; // structurally-empty published cell
                };
                // Solve to get the converged VILLAIN side + model (pm, R, vrange),
                // then OVERRIDE hero with the shipped post-processed frequencies.
                let mut sol = solve_bucket_full(pos, bucket, &keys, &matrix, vr);
                sol.hero_freq = shipped_hero_freq(pos, bucket, &keys);

                let e = bucket_exploitability(&sol, &matrix);
                assert!(
                    e.hero_gap >= -1e-9,
                    "shipped hero best-response gap must be >= 0 for {pos:?}/{bucket:?}: {:.6}",
                    e.hero_gap
                );
                assert!(
                    e.hero_gap < MAX_SHIPPED_HERO_GAP_BB,
                    "SHIPPED (post-processed) hero strategy NOT near-equilibrium for \
                     {pos:?}/{bucket:?}: hero_gap {:.5} bb ≥ {MAX_SHIPPED_HERO_GAP_BB} bb \
                     — the committed preflop_v2.json bytes are too exploitable",
                    e.hero_gap
                );
                checked += 1;
            }
        }
        assert_eq!(
            checked, 28,
            "must cover every non-empty published bucket (got {checked})"
        );
    }

    /// F2/F7 (math/symmetry): in the `vs_4bet` bucket hero's continue is an ALL-IN
    /// (5-bet jam) and the villain calls the jam — a TRUE all-in showdown, so BOTH
    /// sides must value the terminal at RAW equity (no `R` realization haircut).
    ///
    /// F7 (2026-06-25): this now invokes the ACTUAL implementation
    /// (`villain_call_ev` / `hero_showdown_ev`, the SAME free functions the solver
    /// and `bucket_exploitability` call) rather than re-deriving the formula
    /// inline. A regression of the villain terminal to `(1-e)·R` on an all-in line
    /// — the exact bug F7 flagged — now FAILS here, because we assert the real
    /// function returns RAW `(1-e)·pot − cont` (R ignored) when `allin = true`, AND
    /// that it DOES apply `R` when `allin = false`.
    #[test]
    fn vs4bet_allin_terminal_uses_raw_equity_for_both_players() {
        let keys = canonical_keys();
        let matrix = EquityMatrix::build(&keys);
        let prior = class_prior(&keys);
        let villain_4bet = top_fraction_range(&matrix, &prior, FOURBET_FREQ);

        // The all-in bucket: hero_allin must be true and BOTH R vectors all 1.0.
        let sol = solve_bucket_full(Position::Co, Bucket::Vs4bet, &keys, &matrix, &villain_4bet);
        assert!(
            sol.pm.hero_allin,
            "vs_4bet hero continue must be an all-in jam"
        );
        for (c, &r) in sol.hero_r.iter().enumerate() {
            assert_eq!(
                r.to_bits(),
                1.0f64.to_bits(),
                "hero R must be 1.0 (raw equity) on the all-in vs_4bet line for {}",
                keys[c]
            );
        }

        // A NON-all-in bucket (vs_3bet) must, by contrast, apply a non-trivial R
        // haircut on at least some classes — proving the all-in branch is what
        // suppresses R, not a global change.
        let sol3 = solve_bucket_full(Position::Co, Bucket::Vs3bet, &keys, &matrix, &villain_4bet);
        assert!(
            !sol3.pm.hero_allin,
            "vs_3bet hero continue is NOT an all-in"
        );
        let any_haircut = (0..N_CLASSES).any(|c| sol3.vill_r[c] < 1.0 - 1e-9);
        assert!(
            any_haircut,
            "non-all-in bucket must keep the villain R realization haircut"
        );

        // ---- F7: exercise the REAL terminal-EV functions (not a re-derivation) ----
        let pm = pot_model(Position::Co, Bucket::Vs4bet);
        assert!(pm.hero_allin, "vs_4bet pot model must be all-in");
        let final_pot = pm.dead + pm.hero_cont + pm.vill_cont;
        let aa = key_index(&keys, "AA");
        let t72 = key_index(&keys, "72o");
        let e = matrix.get(aa, t72); // hero AA raw equity vs 72o
        let vill_e = 1.0 - e; // villain 72o raw equity vs AA

        // Pick a DELIBERATELY non-1.0 R so that a (1-e)·R regression would shift the
        // number measurably; the all-in branch must IGNORE it.
        let bogus_r = 0.5_f64;
        let expected_raw_vill = vill_e * final_pot - pm.vill_cont; // raw, NO R
        let expected_raw_hero = e * final_pot - pm.hero_cont; // raw, NO R

        // The IMPLEMENTATION's villain terminal on the all-in line must equal RAW
        // (1-e) EV regardless of R — this is the assertion that catches the F7
        // regression (it would instead return `vill_e·bogus_r·final_pot − cont`).
        let impl_vill_allin = villain_call_ev(vill_e, bogus_r, true, final_pot, pm.vill_cont);
        assert!(
            (impl_vill_allin - expected_raw_vill).abs() < 1e-12,
            "all-in villain terminal must use RAW (1-e) and IGNORE R: got {impl_vill_allin}, \
             want {expected_raw_vill} (a (1-e)·R regression would give \
             {})",
            vill_e * bogus_r * final_pot - pm.vill_cont
        );
        let impl_hero_allin = hero_showdown_ev(e, bogus_r, true, final_pot, pm.hero_cont);
        assert!(
            (impl_hero_allin - expected_raw_hero).abs() < 1e-12,
            "all-in hero terminal must use RAW e and IGNORE R"
        );

        // Symmetry: on the all-in line the two raw shares sum to the zero-sum
        // identity (final_pot − total invested) — no realization leakage.
        let total_invested = pm.hero_cont + pm.vill_cont;
        assert!(
            (impl_hero_allin + impl_vill_allin - (final_pot - total_invested)).abs() < 1e-9,
            "all-in showdown must be zero-sum at raw equity (no R leakage)"
        );

        // Conversely, the SAME function on a NON-all-in line MUST apply R (proving
        // the all-in raw-equity behavior is gated on `allin`, not unconditional).
        let impl_vill_seeflop = villain_call_ev(vill_e, bogus_r, false, final_pot, pm.vill_cont);
        let expected_r_vill = (vill_e * bogus_r).clamp(0.0, 1.0) * final_pot - pm.vill_cont;
        assert!(
            (impl_vill_seeflop - expected_r_vill).abs() < 1e-12,
            "see-flop villain terminal MUST apply the R haircut: got {impl_vill_seeflop}, \
             want {expected_r_vill}"
        );
        assert!(
            (impl_vill_seeflop - impl_vill_allin).abs() > 1e-6,
            "R must materially change the villain terminal (else the test proves nothing)"
        );
    }

    /// F1 (honesty): the non-all-in SEE-FLOP terminal is GENERAL-SUM, not zero-sum.
    /// Each side realizes `e*R_hero` / `(1-e)*R_vill`, and when `R ≠ 1` those shares
    /// do NOT sum to 1, so hero_ev + vill_ev ≠ final_pot − total_invested. This
    /// asserts the distinction the docstrings now make: only the all-in line is
    /// strictly zero-sum; the see-flop line (e.g. vs_3bet, where hero continues by
    /// 4-betting and the realization haircut applies) leaks a measurable amount, so
    /// the game is solved by per-player best-response convergence, not as zero-sum.
    #[test]
    fn non_allin_terminal_is_general_sum() {
        let keys = canonical_keys();
        let matrix = EquityMatrix::build(&keys);

        // A NON-all-in bucket: vs_3bet (hero continue = 4-bet, R applies).
        let pos = Position::Co;
        let bucket = Bucket::Vs3bet;
        let pm = pot_model(pos, bucket);
        assert!(!pm.hero_allin, "vs_3bet must be a NON-all-in see-flop line");

        // Reconstruct the per-side realization multipliers exactly as the solver.
        let hero_r = |c: usize| realization(c, &keys, &matrix, pm.hero_ip, pm.hero_aggressor);
        let vill_r = |c: usize| realization(c, &keys, &matrix, !pm.hero_ip, false);

        let final_pot = pm.dead + pm.hero_cont + pm.vill_cont;
        let total_invested = pm.hero_cont + pm.vill_cont;

        // Pick a class pair where BOTH realization factors are off 1.0 so the
        // general-sum leak is unambiguous. 72o realizes poorly OOP; AA realizes
        // above raw IP — neither R is exactly 1.0.
        let aa = key_index(&keys, "AA");
        let t72 = key_index(&keys, "72o");
        let rh = hero_r(aa);
        let rv = vill_r(t72);
        assert!(
            (rh - 1.0).abs() > 1e-6 || (rv - 1.0).abs() > 1e-6,
            "need at least one R ≠ 1.0 to demonstrate general-sum (R_hero={rh}, R_vill={rv})"
        );

        let e = matrix.get(aa, t72);
        let hero_ev = (e * rh).clamp(0.0, 1.0) * final_pot - pm.hero_cont;
        let vill_ev = ((1.0 - e) * rv).clamp(0.0, 1.0) * final_pot - pm.vill_cont;

        // Zero-sum would require hero_ev + vill_ev == final_pot − total_invested.
        // With independent R the realized shares ≠ 1 → a measurable leak.
        let leak = (hero_ev + vill_ev) - (final_pot - total_invested);
        assert!(
            leak.abs() > 1e-6,
            "see-flop terminal must be GENERAL-SUM (independent R): expected a \
             non-zero leak, got {leak} (R_hero={rh}, R_vill={rv}, e={e})"
        );
    }
}
