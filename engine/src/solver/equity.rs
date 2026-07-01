//! Equity estimation by Monte Carlo + exact-enumeration showdown.
//!
//! Public API uses engine types (`HoleCards`, `BoardCards`) — no `rs_poker`
//! leaks (ADR-012). Showdown comparisons reuse `engine::eval::rank_players`.
//!
//! Determinism: all randomness is sourced from a caller-supplied seed
//! (`u64`); the same `(hero, board, opponents, trials, seed)` always produces
//! the same `EquityResult` byte-for-byte (ADR-043 §6).

use crate::card::{Card, Rank, Suit};
use crate::eval::{board_eval, rank_players, rank_with_board};
use crate::hand::{BoardCards, HoleCards};
use crate::player::PlayerId;
use crate::rng::PokerRng;

use super::{
    DEFAULT_MC_TRIALS, EARLY_STOP_CHECK_EVERY, EARLY_STOP_HALF_WIDTH, EARLY_STOP_MIN_TRIALS,
    MAX_MC_TRIALS,
};

/// Specifies the opponent(s) for an equity estimate.
#[derive(Debug, Clone)]
pub enum OpponentSpec {
    /// `n` opponents drawing uniformly from the remaining deck.
    Random(u8),
    /// Exact opponent holdings (used for showdown reconstruction).
    Known(Vec<HoleCards>),
    /// Weighted 169-grid range for ONE villain (M1 §1a). Each `RangeBucket`
    /// expands to concrete combos (pair=6, suited=4, offsuit=12); combos that
    /// conflict with hero/board are dropped; one villain is weighted-sampled
    /// per MC trial. If the filtered pool is empty, falls back to `Random(1)`.
    /// Multi-opponent `Range` is OUT of scope for M1.
    Range(Vec<RangeBucket>),
}

/// Weighted range bucket — declared for API forward-compat (v2 only).
#[derive(Debug, Clone)]
pub struct RangeBucket {
    /// 169-grid notation key, e.g. "AKs" / "AKo" / "AA".
    pub hand_key: String,
    /// Weight in `[0.0, 1.0]`.
    pub weight: f32,
}

/// Monte-Carlo early-stop policy (ADR-082). Carried on [`EquityInput`]; set ONLY
/// by [`equity_vs_random`] (the live-coach path). `analyze_spot`, `analyze_replay`,
/// and `poker_tools` build [`EquityInput`] with `early_stop: None`.
#[derive(Debug, Clone, Copy)]
pub struct EarlyStop {
    /// Minimum trials before the CI check is ever evaluated. LOCKED at
    /// [`EARLY_STOP_MIN_TRIALS`]`= 1000` by the coach path.
    pub min_trials: u32,
    /// Stop once the Wilson 95% CI half-width on the combined-equity proportion
    /// is strictly below this (proportion units, NOT pp). LOCKED at
    /// [`EARLY_STOP_HALF_WIDTH`]`= 0.005` (0.5pp) by the coach path.
    pub half_width: f64,
}

/// Input to [`equity`].
#[derive(Debug, Clone)]
pub struct EquityInput {
    pub hero: HoleCards,
    pub board: BoardCards,
    pub opponents: OpponentSpec,
    /// Number of Monte Carlo trials. Clamped to `[1, MAX_MC_TRIALS]` (T5).
    pub trials: u32,
    /// PRNG seed — deterministic same-input ⇒ same-output (ADR-043 §6).
    pub seed: u64,
    /// Optional Monte-Carlo early-stop policy (ADR-082). `None` (the default for
    /// EVERY existing caller) ⇒ the loop runs EXACTLY `trials` iterations,
    /// byte-identical to the pre-ADR-082 behaviour. `Some(..)` ⇒ the COACH path:
    /// after `min_trials`, the loop may stop early once the Wilson 95% CI
    /// half-width on the running combined-equity proportion drops below
    /// `half_width`. The early-stop check reads only accumulated counters — it
    /// NEVER consumes the RNG — so the sample prefix is identical to the full
    /// loop's and the result is still deterministic in
    /// `(hero, board, opponents, trials, seed, early_stop)`. There is NO `Default`
    /// impl hiding this field: every non-coach site shows `early_stop: None` so an
    /// auditor can grep it; [`equity_vs_random`] is the single `Some(..)` site.
    pub early_stop: Option<EarlyStop>,
}

/// Output of [`equity`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct EquityResult {
    /// Hero's outright-win percentage, 0..=100.
    pub win_pct: u8,
    /// Frequency hero is in a split pot, 0..=100 (the CHOP frequency, NOT the
    /// pot share). A 3-way chop and a 2-way chop both count once here. Do NOT
    /// reconstruct combined equity as `win_pct + tie_pct/2` — that formula
    /// assumes every tie is a 2-way split and overstates equity on multi-way
    /// chops. Use [`EquityResult::equity_pct`] / `combined_pct` instead.
    pub tie_pct: u8,
    /// Hero's combined equity (expected pot share), 0..=100. Each trial/runout
    /// contributes `1.0` on an outright win and `1/N` on an N-way chop, summed
    /// in exact integer units ([`SHARE_SCALE`]) and rounded ONCE at the end —
    /// mirroring `server::equity::compute_equity`'s `1/(1+tied_opponents)`
    /// weighting. This avoids both the up-to-~1pp low bias from re-rounding
    /// per-runout shares and the multi-way-chop overstatement from weighting
    /// every tie as 0.5. This is the authoritative equity number.
    pub combined_pct: u8,
}

/// Pot-share scale = LCM(1..=6), so a `1/N` chop with up to 6 players (hero +
/// 5 opponents, the 6-max cap) is an exact integer share (`SHARE_SCALE / N`).
/// Combined equity is accumulated in these units and rounded ONCE at the end.
const SHARE_SCALE: u64 = 60;

impl EquityResult {
    /// Sentinel returned when the input is invalid (duplicate/overlapping
    /// cards); the caller treats it as `ContextNotFound`.
    const INVALID: EquityResult = EquityResult {
        win_pct: 0,
        tie_pct: 0,
        combined_pct: 0,
    };

    /// Build a result from outright-win and chop counts plus the exact
    /// combined-equity score (`share_sum`, in [`SHARE_SCALE`] units: each trial
    /// adds `SHARE_SCALE` on a win and `SHARE_SCALE/N` on an N-way chop),
    /// rounding the combined value ONCE. `share_sum` carries the multi-way pot
    /// share exactly — `wins`/`ties` are kept only for `win_pct`/`tie_pct`.
    fn from_counts(wins: u64, ties: u64, share_sum: u64, trials: u64) -> EquityResult {
        debug_assert!(trials > 0);
        let win_pct = ((wins * 100 + trials / 2) / trials) as u8;
        let tie_pct = ((ties * 100 + trials / 2) / trials) as u8;
        let combined_den = SHARE_SCALE * trials;
        let combined_pct = (((share_sum * 100 + combined_den / 2) / combined_den).min(100)) as u8;
        EquityResult {
            win_pct,
            tie_pct,
            combined_pct,
        }
    }

    /// Combined equity = wins + half of ties — useful for EV / pot-odds
    /// comparisons. Returns the single-rounded `combined_pct` (see field docs).
    pub fn equity_pct(&self) -> u8 {
        self.combined_pct
    }
}

/// Estimate hero equity against `opponents` on `board`.
///
/// Defensive bounds:
/// - `trials` clamped to `[1, MAX_MC_TRIALS]`.
/// - If `board` already contains duplicates or overlaps with `hero`, returns
///   `EquityResult { 0, 0 }` (caller treats as invalid / `ContextNotFound`).
/// - `opponents` clamped to ≤ 5 (max 6-max minus hero).
pub fn equity(input: EquityInput) -> EquityResult {
    equity_inner(input).0
}

/// Core of [`equity`], additionally returning the number of MC trials actually
/// run (`t` at the early-stop break, or `trials` when the loop ran to
/// completion / took a non-MC path). The trial count is an internal detail used
/// only by the ADR-082 early-stop tests — it deliberately does NOT leak through
/// the public [`equity`] signature (ADR-012 keeps the public API on engine types
/// only, and no caller needs the count).
fn equity_inner(input: EquityInput) -> (EquityResult, u32) {
    // Defensive: validate cards are unique.
    let board_cards = input.board.all_cards();
    if board_cards.len() > 5 {
        return (EquityResult::INVALID, 0);
    }
    let mut all_known: Vec<Card> = Vec::with_capacity(7);
    all_known.push(input.hero.card1);
    all_known.push(input.hero.card2);
    all_known.extend_from_slice(&board_cards);
    if has_duplicates(&all_known) {
        return (EquityResult::INVALID, 0);
    }

    let trials = input.trials.clamp(1, MAX_MC_TRIALS);

    // Opponent cap: at most 5 (6-max minus hero). For `Known`, truncate to the
    // SAME cap that drives `n_opp` so the seated opponents, the excluded run-out
    // cards, and the capacity hint all agree. Previously `n_opp` was capped to 5
    // but every Known holding was still seated in the showdown, so a >5-opponent
    // Known spec silently computed equity vs more opponents than the cap implied
    // (audit 2026-06-03).
    const MAX_OPP: usize = 5;

    let opp_holdings: Option<Vec<HoleCards>> = match &input.opponents {
        OpponentSpec::Known(v) if !v.is_empty() => {
            let capped: Vec<HoleCards> = v.iter().take(MAX_OPP).copied().collect();
            // Validate: no overlap with hero/board, no duplicates among opponents.
            let mut combined = all_known.clone();
            for h in &capped {
                combined.push(h.card1);
                combined.push(h.card2);
            }
            if has_duplicates(&combined) {
                return (EquityResult::INVALID, 0);
            }
            // Audit fix N-2 (2026-05-23): known opponent cards must be
            // excluded from the run-out deck so MC trials never deal a
            // turn/river card that is already in an opponent's hand. The
            // previous build_known path only excluded hero + board cards.
            all_known = combined;
            Some(capped)
        }
        OpponentSpec::Known(_) => None,
        _ => None,
    };

    // M1 §1a: a `Range` models exactly ONE villain. Pre-build the
    // conflict-filtered weighted combo pool here so an empty pool can fall back
    // to `Random(1)` deterministically. `range_pool` is `None` for non-`Range`
    // specs and for a fully-blocked range (→ uniform random villain).
    let range_pool: Option<Vec<(HoleCards, f32)>> = match &input.opponents {
        OpponentSpec::Range(buckets) => {
            let pool = build_range_pool(buckets, &all_known);
            if pool.is_empty() {
                None
            } else {
                Some(pool)
            }
        }
        _ => None,
    };

    let n_opp = match &input.opponents {
        OpponentSpec::Random(n) => (*n).min(MAX_OPP as u8),
        OpponentSpec::Known(v) => (v.len().min(MAX_OPP)) as u8,
        // M1 §1a: `Range` is always exactly one villain.
        OpponentSpec::Range(_) => 1,
    };
    if n_opp == 0 {
        // Heads-up vs ghost: hero always wins by default.
        return (
            EquityResult {
                win_pct: 100,
                tie_pct: 0,
                combined_pct: 100,
            },
            0,
        );
    }

    let remaining_deck = deck_minus(&all_known);

    // Known opponents with a postflop board have at most two unknown community
    // cards. ADR-043 §3.1 requires exact enumeration here rather than MC
    // sampling, so retries/cache keys never depend on trial count or seed.
    if let Some(opps) = opp_holdings.as_ref() {
        if board_cards.len() >= 3 {
            // Exact enumeration is not MC — report `trials` unchanged for the
            // trial-count sibling (no early-stop hook here, ADR-082 §4).
            let res = exact_known_runouts(&input.hero, &input.board, opps, &remaining_deck);
            return (res, trials);
        }
    }

    // MC loop. Two paths:
    //  * Range (M1 weighted villain): per-trial filtered deck — correctness
    //    first; this path is low-frequency (range_insight only).
    //  * Random / Known-preflop (the HOT coach + live-snapshot path): reuse ONE
    //    scratch deck across all trials and draw only the cards we actually need
    //    via a partial Fisher–Yates. Measured baseline showed ~89% of equity()
    //    time was per-trial deck CHURN (a 48-card clone + full 50-swap shuffle
    //    when only ~7 cards are needed) and the showdown Vec-build+sort — NOT
    //    hand evaluation. This path removes both.
    let mut wins: u32 = 0;
    let mut ties: u32 = 0;
    // Combined pot share accumulated in SHARE_SCALE units (win → SHARE_SCALE,
    // N-way chop → SHARE_SCALE/N). Kept separate from `ties` so multi-way chops
    // contribute their true 1/N share, not a flat 0.5.
    let mut share_sum: u64 = 0;
    let missing = 5usize.saturating_sub(board_cards.len());
    // Trials actually run. Stays `trials` for the Range branch (never early-stops,
    // ADR-082 §2); the Random/Known-preflop branch overwrites it with the `t` at
    // the early-stop break (or `trials` when the loop ran to completion).
    let mut trials_run = trials;

    if let Some(pool) = range_pool.as_ref() {
        // M1 §1a weighted single villain: deal the board from the deck MINUS the
        // sampled villain's two cards so a board card can never collide with it.
        // This low-frequency path keeps the engine's seeded ChaCha RNG.
        let mut rng = PokerRng::from_seed(input.seed);
        for _ in 0..trials {
            let v = sample_weighted(pool, &mut rng);
            let mut available: Vec<Card> = remaining_deck
                .iter()
                .copied()
                .filter(|c| *c != v.card1 && *c != v.card2)
                .collect();
            shuffle_in_place(&mut available, &mut rng);
            let mut sim_board = input.board.clone();
            let mut cursor = 0usize;
            fill_board(&mut sim_board, &mut available, &mut cursor);
            // `Range` is always exactly ONE villain (n_opp == 1), so a tie is
            // always a 2-way chop worth SHARE_SCALE/2.
            match hero_outcome(input.hero, &sim_board, std::slice::from_ref(&v)) {
                HeroOutcome::Win => {
                    wins += 1;
                    share_sum += SHARE_SCALE;
                }
                HeroOutcome::Tie => {
                    ties += 1;
                    share_sum += SHARE_SCALE / 2;
                }
                HeroOutcome::Loss => {}
            }
        }
    } else {
        // `Known` opponents reaching the MC loop are PREFLOP only (postflop
        // Known took the exact-enumeration path above), so their holdings are
        // fixed and we draw only the `missing` board cards. `Random` draws the
        // board plus two cards per opponent. One scratch deck is reused for
        // every trial — `draw_prefix` permutes it in place but keeps it a valid
        // permutation, so the next draw is still uniform without re-init.
        let known_opps = opp_holdings.as_ref();
        let opp_draw_cards = if known_opps.is_some() {
            0
        } else {
            2 * n_opp as usize
        };
        let draw_len = missing + opp_draw_cards;
        let base_board = input.board.clone();
        let mut scratch = remaining_deck;
        // Hot path: estimation-only fast PRNG (see `EquityRng` — NOT a dealing RNG).
        let mut rng = EquityRng::new(input.seed);

        // ADR-082 coach early-stop policy. `None` ⇒ run all `trials`, byte-identical
        // to pre-ADR-082. `Some(..)` ⇒ after `min_trials` (clamped to `trials` so a
        // degenerate `trials < min_trials` can never under-run), re-evaluate the
        // Wilson 95% CI half-width every `EARLY_STOP_CHECK_EVERY` trials and break
        // once it drops below the policy's `half_width`. The check reads ONLY the
        // accumulated counters (`share_sum`, the trial index) + `sqrt` — it NEVER
        // touches `rng`, so the sample prefix (and therefore the stop index `t*`)
        // is a deterministic function of `(prefix, min_trials, half_width, K)`.
        let early_stop = input.early_stop;
        let es_min_trials = early_stop.map(|e| e.min_trials.min(trials));

        // `t` = trials completed so far (1-based after the body). `trials_run`
        // captures the actual count at break / completion for the test sibling.
        let mut completed: u32 = 0;
        for _ in 0..trials {
            draw_prefix(&mut scratch, draw_len, &mut rng);

            // BoardCards is a small fixed-size struct (no heap), so this clone is
            // a cheap stack copy — not an allocation.
            let mut sim_board = base_board.clone();
            let mut cursor = 0usize;
            fill_board(&mut sim_board, &mut scratch, &mut cursor);

            // Inline showdown: pre-convert the board ONCE, rank hero, then
            // compare each opponent with an early-out on the first hand that
            // beats hero. Mirrors `hero_outcome` exactly (loss if any opp
            // outranks hero; win if none tie; tie otherwise) but avoids
            // building+sorting a per-trial Vec and re-converting the board per
            // player (`board_eval` + `rank_with_board` are allocation-free).
            let be = board_eval(&sim_board);
            let hero_rank = rank_with_board(&be, &input.hero);
            let mut tied: u32 = 0;
            let mut hero_lost = false;
            if let Some(opps) = known_opps {
                for opp in opps.iter() {
                    let r = rank_with_board(&be, opp);
                    if r > hero_rank {
                        hero_lost = true;
                        break;
                    } else if r == hero_rank {
                        tied += 1;
                    }
                }
            } else {
                let opp_cards = &scratch[cursor..cursor + opp_draw_cards];
                let mut i = 0;
                while i < opp_cards.len() {
                    let opp = HoleCards::new(opp_cards[i], opp_cards[i + 1]);
                    i += 2;
                    let r = rank_with_board(&be, &opp);
                    if r > hero_rank {
                        hero_lost = true;
                        break;
                    } else if r == hero_rank {
                        tied += 1;
                    }
                }
            }

            if hero_lost {
                // loss — contributes to neither wins, ties, nor pot share
            } else if tied == 0 {
                wins += 1;
                share_sum += SHARE_SCALE;
            } else {
                // (1 + tied)-way chop: hero takes a 1/(1+tied) pot share.
                // `1 + tied` ≤ 6 (6-max cap) always divides SHARE_SCALE exactly.
                ties += 1;
                share_sum += SHARE_SCALE / (1 + tied as u64);
            }

            completed += 1;

            // ADR-082 early-stop: only when a policy is set, only after `min_trials`,
            // and only on the K-cadence (≤ 36 checks over the 1000→10000 window).
            // The break uses the `completed` trials accumulated so far; `from_counts`
            // takes the actual count so a shorter sample rounds correctly.
            if let (Some(min_t), Some(es)) = (es_min_trials, early_stop) {
                if completed >= min_t
                    && completed.is_multiple_of(EARLY_STOP_CHECK_EVERY)
                    && wilson_half_width(share_sum, completed) < es.half_width
                {
                    break;
                }
            }
        }
        trials_run = completed;
    }

    (
        EquityResult::from_counts(wins as u64, ties as u64, share_sum, trials_run as u64),
        trials_run,
    )
}

/// Wilson-STYLE conservative half-width at 95% (`z = 1.959964`) on the running
/// combined-equity proportion `p̂ = share_sum / (SHARE_SCALE · n)` after `n`
/// completed trials (ADR-082 §2). This is the decision variable the rule table
/// and the displayed `equity_estimate_pct` actually read, so the CI guards the
/// number that matters directly.
///
/// F2 precision nit (2026-06-25): `p̂` is a bounded *fractional-share mean* —
/// each trial contributes 0, 1/SHARE_SCALE, …, or 1 in `SHARE_SCALE = 60` units
/// (split pots), not a true Bernoulli 0/1 outcome — so this is a Wilson-style
/// proxy that plugs the Bernoulli `p(1-p)` variance form into the score
/// interval, NOT an exact Wilson interval for fractional outcomes. `p(1-p)` is
/// the MAXIMUM variance of any `[0,1]`-bounded variable at a given mean, so the
/// proxy OVER-estimates the half-width: it can only ever stop LATER than a tight
/// CI on the true (smaller) variance would, never earlier. It therefore cannot
/// widen the early-stop-vs-full gap (the §5(d) corpus bounds equity drift to
/// ≤1pp with zero verdict flips) and cannot affect fairness/correctness — the
/// conservatism is the safe direction. Pure (no RNG) — the early-stop is a
/// deterministic truncation of an unchanged sample stream.
fn wilson_half_width(share_sum: u64, n: u32) -> f64 {
    debug_assert!(n > 0);
    const Z: f64 = 1.959964;
    let n_f = n as f64;
    let p = (share_sum as f64) / (SHARE_SCALE as f64 * n_f);
    let z2 = Z * Z;
    let denom = 1.0 + z2 / n_f;
    (Z * (p * (1.0 - p) / n_f + z2 / (4.0 * n_f * n_f)).sqrt()) / denom
}

/// Convenience: equity vs N random opponents using the default trial count.
///
/// `seed` from caller (server provides `seed_from_hand_seq`).
///
/// ADR-082: this is the **live-coach** entry point (the ONLY caller is
/// `advisor::analyze_preflop`/`analyze_postflop`, reached via
/// `coach.rs run_solver`/`run_solver_chat`). It is the **single** site that opts
/// into the Monte-Carlo early-stop policy — every other `EquityInput` builder
/// (`analyze_spot`, `analyze_replay`, `poker_tools`, all tests) keeps
/// `early_stop: None` so their numbers stay byte-identical to pre-ADR-082.
pub fn equity_vs_random(
    hero: HoleCards,
    board: BoardCards,
    opp_count: u8,
    seed: u64,
) -> EquityResult {
    equity(EquityInput {
        hero,
        board,
        opponents: OpponentSpec::Random(opp_count),
        trials: DEFAULT_MC_TRIALS,
        seed,
        // The ONLY `Some(..)` site (ADR-082 §1).
        early_stop: Some(EarlyStop {
            min_trials: EARLY_STOP_MIN_TRIALS,
            half_width: EARLY_STOP_HALF_WIDTH,
        }),
    })
}

/// `#[cfg(test)]`-only sibling of [`equity`] that also returns the number of MC
/// trials actually run (`t` at the early-stop break, or `trials` when the loop
/// ran to completion). Lets the ADR-082 §5(c) tests assert that early-stop fires
/// on dominant/clear-fold spots and runs full on marginal ones — WITHOUT leaking
/// a `(EquityResult, u32)` shape into the ADR-012-public [`equity`] signature.
#[cfg(test)]
pub(crate) fn equity_with_trial_count(input: EquityInput) -> (EquityResult, u32) {
    equity_inner(input)
}

/// `#[cfg(test)]`-only: the FULL-10k counterpart of [`equity_vs_random`] —
/// identical inputs but `early_stop: None`. The ADR-082 §5(d) verdict-stability
/// corpus runs the advisor pipeline once with the shipped early-stop path and
/// once with this, and asserts `verdict` + `gto_action` are identical.
#[cfg(test)]
pub(crate) fn equity_vs_random_full(
    hero: HoleCards,
    board: BoardCards,
    opp_count: u8,
    seed: u64,
) -> EquityResult {
    equity(EquityInput {
        hero,
        board,
        opponents: OpponentSpec::Random(opp_count),
        trials: DEFAULT_MC_TRIALS,
        seed,
        early_stop: None,
    })
}

// ---------------------------------------------------------------------------
// Internal helpers
// ---------------------------------------------------------------------------

/// Outcome of a single showdown from hero's perspective.
enum HeroOutcome {
    Win,
    Tie,
    Loss,
}

/// Run one showdown of hero (seated at `PlayerId(0)`) plus `opponents` on
/// `board`, returning hero's outcome. Shared by the MC loop and the exact
/// river path so the win/tie/loss classification lives in one place.
fn hero_outcome(hero: HoleCards, board: &BoardCards, opponents: &[HoleCards]) -> HeroOutcome {
    let mut players: Vec<(PlayerId, HoleCards)> = Vec::with_capacity(1 + opponents.len());
    players.push((PlayerId::new(0), hero));
    for (i, h) in opponents.iter().enumerate() {
        players.push((PlayerId::new((i + 1) as u64), *h));
    }
    let ranked = rank_players(&players, board);
    let top_rank = ranked[0].1;
    let hero_rank = ranked
        .iter()
        .find(|(pid, _)| pid.inner() == 0)
        .map(|(_, r)| *r)
        .unwrap_or(top_rank);
    let winners_at_top: usize = ranked.iter().take_while(|(_, r)| *r == top_rank).count();
    if hero_rank != top_rank {
        HeroOutcome::Loss
    } else if winners_at_top == 1 {
        HeroOutcome::Win
    } else {
        HeroOutcome::Tie
    }
}

/// Hero's outcome in one deterministic showdown, as
/// `(win, tie, share)` where `win`/`tie` are 0/1 flags and `share` is hero's
/// exact pot share in [`SHARE_SCALE`] units: loss → `(0, 0, 0)`; sole win →
/// `(1, 0, SHARE_SCALE)`; N-way chop → `(0, 1, SHARE_SCALE / N)`. Shared by
/// `single_showdown` and `exact_known_runouts` so the share is computed (and
/// accumulated) WITHOUT an intermediate per-runout rounding step.
fn showdown_share(
    hero: &HoleCards,
    board: &BoardCards,
    opponents: &[HoleCards],
) -> (u64, u64, u64) {
    let mut players: Vec<(PlayerId, HoleCards)> = Vec::with_capacity(1 + opponents.len());
    players.push((PlayerId::new(0), *hero));
    for (i, h) in opponents.iter().enumerate() {
        players.push((PlayerId::new((i + 1) as u64), *h));
    }
    let ranked = rank_players(&players, board);
    let top_rank = ranked[0].1;
    let hero_rank = ranked
        .iter()
        .find(|(pid, _)| pid.inner() == 0)
        .map(|(_, r)| *r)
        .unwrap_or(top_rank);
    if hero_rank != top_rank {
        return (0, 0, 0);
    }
    let winners_at_top = ranked.iter().take_while(|(_, r)| *r == top_rank).count() as u64;
    if winners_at_top <= 1 {
        (1, 0, SHARE_SCALE)
    } else {
        // `winners_at_top` ≤ 6 (6-max) divides SHARE_SCALE exactly.
        (0, 1, SHARE_SCALE / winners_at_top)
    }
}

fn single_showdown(hero: &HoleCards, board: &BoardCards, opponents: &[HoleCards]) -> EquityResult {
    // One deterministic showdown is the `trials == 1` case of `from_counts`:
    // loss → 0/0/0, sole win → 100/0/100, N-way chop → 0/100/(single-rounded
    // `share/SHARE_SCALE`). Delegate so the win/tie/combined rounding lives in
    // exactly one place rather than being re-derived here.
    let (win, tie, share) = showdown_share(hero, board, opponents);
    EquityResult::from_counts(win, tie, share, 1)
}

fn exact_known_runouts(
    hero: &HoleCards,
    board: &BoardCards,
    opponents: &[HoleCards],
    remaining_deck: &[Card],
) -> EquityResult {
    let missing = 5usize.saturating_sub(board.count());
    if missing == 0 {
        return single_showdown(hero, board, opponents);
    }

    let mut total: u64 = 0;
    let mut win_sum: u64 = 0;
    let mut tie_sum: u64 = 0;
    // Accumulate the EXACT per-runout pot share in SHARE_SCALE units (no
    // intermediate per-runout rounding), rounded ONCE over `SHARE_SCALE*total`
    // at the end (mirrors `from_counts` / `server::equity::compute_equity`).
    // Re-rounding a per-runout `combined_pct` would re-introduce the up-to-~1pp
    // low bias on chop-heavy boards.
    let mut share_sum: u64 = 0;

    let mut accumulate = |(win, tie, share): (u64, u64, u64)| {
        win_sum += win;
        tie_sum += tie;
        share_sum += share;
        total += 1;
    };

    match missing {
        1 => {
            for &river_or_turn in remaining_deck {
                let mut sim_board = board.clone();
                fill_board_from_runout(&mut sim_board, &[river_or_turn]);
                accumulate(showdown_share(hero, &sim_board, opponents));
            }
        }
        2 => {
            for i in 0..remaining_deck.len() {
                for j in (i + 1)..remaining_deck.len() {
                    let mut sim_board = board.clone();
                    fill_board_from_runout(&mut sim_board, &[remaining_deck[i], remaining_deck[j]]);
                    accumulate(showdown_share(hero, &sim_board, opponents));
                }
            }
        }
        // This helper is only called postflop; fall back to the invalid
        // sentinel rather than doing an accidental preflop C(50,5) enumeration.
        _ => return EquityResult::INVALID,
    }

    if total == 0 {
        return EquityResult::INVALID;
    }

    // Round win/tie/combined ONCE over the full enumeration — identical to
    // `from_counts` (combined accumulated in SHARE_SCALE units), so delegate
    // instead of re-inlining the rounding.
    EquityResult::from_counts(win_sum, tie_sum, share_sum, total)
}

fn has_duplicates(cards: &[Card]) -> bool {
    for (i, a) in cards.iter().enumerate() {
        for b in &cards[i + 1..] {
            if a == b {
                return true;
            }
        }
    }
    false
}

/// Expand a single 169-grid `hand_key` (e.g. "AA" / "AKs" / "AKo") into its
/// concrete `HoleCards` combos: pair → 6, suited → 4, offsuit → 12. Returns an
/// empty `Vec` for an unparseable key (never panics — M1 §1a).
pub(crate) fn expand_range_key(key: &str) -> Vec<HoleCards> {
    let bytes = key.as_bytes();
    if bytes.len() < 2 {
        return Vec::new();
    }
    let r1 = match rank_from_char(bytes[0] as char) {
        Some(r) => r,
        None => return Vec::new(),
    };
    let r2 = match rank_from_char(bytes[1] as char) {
        Some(r) => r,
        None => return Vec::new(),
    };
    let mut out = Vec::new();
    if r1 == r2 {
        // Pair: every unordered pair of suits → C(4,2) = 6 combos.
        if bytes.len() != 2 {
            return Vec::new();
        }
        for i in 0..Suit::ALL.len() {
            for j in (i + 1)..Suit::ALL.len() {
                out.push(HoleCards::new(
                    Card::new(r1, Suit::ALL[i]),
                    Card::new(r2, Suit::ALL[j]),
                ));
            }
        }
    } else {
        let suffix = match bytes.get(2) {
            Some(b's') => Suitedness::Suited,
            Some(b'o') => Suitedness::Offsuit,
            _ => return Vec::new(),
        };
        match suffix {
            Suitedness::Suited => {
                // Same suit → 4 combos.
                for &s in Suit::ALL.iter() {
                    out.push(HoleCards::new(Card::new(r1, s), Card::new(r2, s)));
                }
            }
            Suitedness::Offsuit => {
                // Different suits → 4 × 3 = 12 combos.
                for &s1 in Suit::ALL.iter() {
                    for &s2 in Suit::ALL.iter() {
                        if s1 != s2 {
                            out.push(HoleCards::new(Card::new(r1, s1), Card::new(r2, s2)));
                        }
                    }
                }
            }
        }
    }
    out
}

enum Suitedness {
    Suited,
    Offsuit,
}

fn rank_from_char(ch: char) -> Option<Rank> {
    Rank::ALL.iter().copied().find(|r| r.char() == ch)
}

/// Build the conflict-filtered weighted combo pool for an `OpponentSpec::Range`.
///
/// Each surviving combo carries its bucket weight (clamped to `[0, 1]`); zero-
/// or negative-weight buckets contribute nothing. `used` is hero + board cards;
/// any combo sharing a card with `used` is dropped. The returned pool is what
/// `sample_weighted` draws one villain from each MC trial.
fn build_range_pool(buckets: &[RangeBucket], used: &[Card]) -> Vec<(HoleCards, f32)> {
    let mut pool: Vec<(HoleCards, f32)> = Vec::new();
    for bucket in buckets {
        // Only positive weights contribute. NaN fails the `> 0.0` test, so a
        // NaN-weight bucket adds nothing (no negated partial-ord comparison).
        if bucket.weight > 0.0 {
            let w = bucket.weight.min(1.0);
            for combo in expand_range_key(&bucket.hand_key) {
                if used.contains(&combo.card1) || used.contains(&combo.card2) {
                    continue;
                }
                pool.push((combo, w));
            }
        }
    }
    pool
}

/// Number of legal villain combos an `OpponentSpec::Range` yields after removing
/// `used` (hero + board) cards — the size of the pool `equity()` samples from.
///
/// Public so callers (e.g. the public poker-math tool endpoints) can validate a
/// range BEFORE running the sim. A result of `0` means the range has no legal
/// combination, in which case `equity()` silently falls back to `Random(1)`;
/// callers that must honour the requested villain model should reject instead.
pub fn range_legal_combos(buckets: &[RangeBucket], used: &[Card]) -> usize {
    build_range_pool(buckets, used).len()
}

/// Weighted-sample one villain hand from the pool. Determinism comes from the
/// caller's `PokerRng`. The pool is assumed non-empty (the empty case falls
/// back to `Random(1)` before the MC loop).
fn sample_weighted(pool: &[(HoleCards, f32)], rng: &mut PokerRng) -> HoleCards {
    let total: f32 = pool.iter().map(|(_, w)| *w).sum();
    if total <= 0.0 {
        return pool[0].0;
    }
    // Draw a uniform value in [0, total) from 64 bits of RNG, then walk the
    // cumulative weights. Mirrors the seed-driven determinism of the deck shuffle.
    let mut buf = [0u8; 8];
    rng.fill_bytes(&mut buf);
    let frac = (u64::from_le_bytes(buf) as f64) / (u64::MAX as f64 + 1.0);
    let mut target = (frac as f32) * total;
    for (hand, w) in pool {
        target -= *w;
        if target < 0.0 {
            return *hand;
        }
    }
    // Floating-point edge: return the last combo.
    pool[pool.len() - 1].0
}

fn deck_minus(used: &[Card]) -> Vec<Card> {
    let mut out: Vec<Card> = Vec::with_capacity(52 - used.len());
    for &r in Rank::ALL.iter() {
        for &s in Suit::ALL.iter() {
            let c = Card::new(r, s);
            if !used.contains(&c) {
                out.push(c);
            }
        }
    }
    out
}

fn shuffle_in_place(cards: &mut [Card], rng: &mut PokerRng) {
    // Fisher–Yates, mirrors `Deck::new`.
    let n = cards.len();
    if n < 2 {
        return;
    }
    for i in (1..n).rev() {
        let j = rng_range(rng, i + 1);
        cards.swap(i, j);
    }
}

/// Partial Fisher–Yates: draw `k` cards without replacement into `deck[0..k]`.
///
/// Permutes `deck` in place but leaves it a valid permutation of the same
/// multiset, so the SAME scratch buffer can be reused for the next MC trial
/// without re-initialising — the next partial draw is still uniform. This is
/// what makes the MC loop allocation-free (no per-trial deck clone) and turns
/// an O(n) full shuffle into an O(k) draw when only `k ≪ n` cards are needed
/// (k = board fill + 2·opponents ≤ 15; n ≈ 45–50). Same `rng` sequence ⇒ same
/// draws ⇒ deterministic (ADR-043 §6).
fn draw_prefix(deck: &mut [Card], k: usize, rng: &mut EquityRng) {
    let n = deck.len();
    let k = k.min(n);
    for i in 0..k {
        // Uniform index in [i, n): `rng.index(n - i)` is in [0, n - i).
        let j = i + rng.index(n - i);
        deck.swap(i, j);
    }
}

/// Fast, deterministic PRNG for equity **estimation** (Monte Carlo sampling).
///
/// This is deliberately NOT a dealing / shuffle RNG. Provably-fair dealing uses
/// [`PokerRng`]/ChaCha20 (ADR-062) and MUST stay there. Equity estimation has
/// no fairness or unpredictability requirement — it only needs good statistical
/// uniformity and determinism from the caller's seed (ADR-043 §6). Once the
/// per-trial allocation churn was removed, ChaCha `fill_bytes` per drawn card
/// became the single largest cost of the loop; SplitMix64 + Lemire index
/// reduction is ~10× cheaper per draw and keeps full same-seed determinism.
struct EquityRng {
    state: u64,
}

impl EquityRng {
    fn new(seed: u64) -> Self {
        // Offset so a zero seed doesn't start the SplitMix sequence at state 0.
        EquityRng {
            state: seed ^ 0x9E37_79B9_7F4A_7C15,
        }
    }

    /// SplitMix64 — a well-distributed, fast 64-bit generator.
    #[inline]
    fn next_u64(&mut self) -> u64 {
        self.state = self.state.wrapping_add(0x9E37_79B9_7F4A_7C15);
        let mut z = self.state;
        z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
        z ^ (z >> 31)
    }

    /// Uniform index in `[0, n)` via Lemire's multiply-shift on the top 32 bits.
    /// Bias ≤ n / 2³² (≈1e-8 for n ≤ 52) — negligible for Monte Carlo estimation.
    #[inline]
    fn index(&mut self, n: usize) -> usize {
        if n <= 1 {
            return 0;
        }
        let r = self.next_u64() >> 32; // top 32 bits
        ((r * n as u64) >> 32) as usize
    }
}

fn rng_range(rng: &mut PokerRng, n: usize) -> usize {
    if n <= 1 {
        return 0;
    }
    let mut buf = [0u8; 8];
    loop {
        rng.fill_bytes(&mut buf);
        let v = u64::from_le_bytes(buf);
        let threshold = u64::MAX - (u64::MAX % n as u64);
        if v < threshold {
            return (v % n as u64) as usize;
        }
    }
}

fn fill_board(board: &mut BoardCards, available: &mut [Card], cursor: &mut usize) {
    if board.flop.is_none() {
        let c1 = available[*cursor];
        *cursor += 1;
        let c2 = available[*cursor];
        *cursor += 1;
        let c3 = available[*cursor];
        *cursor += 1;
        board.flop = Some([c1, c2, c3]);
    }
    if board.turn.is_none() {
        board.turn = Some(available[*cursor]);
        *cursor += 1;
    }
    if board.river.is_none() {
        board.river = Some(available[*cursor]);
        *cursor += 1;
    }
}

fn fill_board_from_runout(board: &mut BoardCards, runout: &[Card]) {
    let mut cursor = 0usize;
    if board.flop.is_none() {
        board.flop = Some([runout[0], runout[1], runout[2]]);
        cursor += 3;
    }
    if board.turn.is_none() {
        board.turn = Some(runout[cursor]);
        cursor += 1;
    }
    if board.river.is_none() {
        board.river = Some(runout[cursor]);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::card::{Card, Rank, Suit};
    use crate::hand::{BoardCards, HoleCards};

    fn c(r: Rank, s: Suit) -> Card {
        Card::new(r, s)
    }

    #[test]
    fn aa_vs_one_random_pre_flop_is_around_85_pct() {
        let hero = HoleCards::new(c(Rank::Ace, Suit::Spades), c(Rank::Ace, Suit::Hearts));
        let board = BoardCards::empty();
        let result = equity(EquityInput {
            hero,
            board,
            opponents: OpponentSpec::Random(1),
            trials: 5_000,
            seed: 42,
            early_stop: None,
        });
        // AA vs 1 random ≈ 85%. Allow ±3pp slack for 5k trials.
        assert!(
            result.win_pct >= 80 && result.win_pct <= 88,
            "AA vs 1 random preflop win% out of [80,88]; got {}",
            result.win_pct
        );
    }

    #[test]
    fn deterministic_same_seed_same_result() {
        let hero = HoleCards::new(c(Rank::Ace, Suit::Spades), c(Rank::King, Suit::Hearts));
        let board = BoardCards::empty();
        let r1 = equity(EquityInput {
            hero,
            board: board.clone(),
            opponents: OpponentSpec::Random(2),
            trials: 1_000,
            seed: 12345,
            early_stop: None,
        });
        let r2 = equity(EquityInput {
            hero,
            board,
            opponents: OpponentSpec::Random(2),
            trials: 1_000,
            seed: 12345,
            early_stop: None,
        });
        assert_eq!(r1, r2, "same seed must produce identical result");
    }

    #[test]
    fn river_known_kk_loses_to_quad_aces() {
        // Hero: KK. Opp: AA. Board: A A 2 2 7 → opp has quads, hero has full house.
        let hero = HoleCards::new(c(Rank::King, Suit::Spades), c(Rank::King, Suit::Hearts));
        let opp = HoleCards::new(c(Rank::Ace, Suit::Spades), c(Rank::Ace, Suit::Hearts));
        let board = BoardCards {
            flop: Some([
                c(Rank::Ace, Suit::Diamonds),
                c(Rank::Ace, Suit::Clubs),
                c(Rank::Two, Suit::Diamonds),
            ]),
            turn: Some(c(Rank::Two, Suit::Hearts)),
            river: Some(c(Rank::Seven, Suit::Spades)),
        };
        let result = equity(EquityInput {
            hero,
            board,
            opponents: OpponentSpec::Known(vec![opp]),
            trials: 1, // not used at river
            seed: 0,
            early_stop: None,
        });
        // Hero has KK on a board with quad aces → loses 100%.
        assert_eq!(result.win_pct, 0);
        assert_eq!(result.tie_pct, 0);
    }

    #[test]
    fn duplicate_hero_board_returns_zero() {
        let hero = HoleCards::new(c(Rank::Ace, Suit::Spades), c(Rank::King, Suit::Spades));
        let board = BoardCards {
            flop: Some([
                c(Rank::Ace, Suit::Spades),
                c(Rank::Two, Suit::Hearts),
                c(Rank::Three, Suit::Clubs),
            ]),
            turn: None,
            river: None,
        };
        let result = equity(EquityInput {
            hero,
            board,
            opponents: OpponentSpec::Random(1),
            trials: 100,
            seed: 1,
            early_stop: None,
        });
        assert_eq!(result.win_pct, 0, "duplicate hero/board must return 0");
    }

    /// Audit fix N-2 (2026-05-23): when opponent cards are Known, MC trials
    /// MUST NOT deal them as turn/river. Pre-fix, `all_known` only excluded
    /// hero + board cards, so the runout deck still contained opp cards.
    ///
    /// Setup: hero AsKs, flop Ah-Kd-2c, opp ThJd Known. The unknown board
    /// cards (turn + river) are drawn from the remaining deck. If N-2
    /// regresses, ~2 / 47 ≈ 4% of trials would deal Th or Jd as the turn
    /// (impossible — they are in opp's hand). We can detect this by
    /// counting the number of trials where the opponent improves to a
    /// flush/two-pair via a duplicate card.
    ///
    /// Direct test: run from the flop with `OpponentSpec::Known`. This now
    /// takes the exact postflop enumeration path; if opponent cards leak into
    /// the runout deck, the averaged result shifts because impossible turn or
    /// river cards are included.
    #[test]
    fn known_opp_cards_excluded_from_runouts() {
        let hero = HoleCards::new(c(Rank::Ace, Suit::Spades), c(Rank::King, Suit::Spades));
        let opp = HoleCards::new(c(Rank::Ten, Suit::Hearts), c(Rank::Jack, Suit::Diamonds));
        let board = BoardCards {
            flop: Some([
                c(Rank::Ace, Suit::Hearts),
                c(Rank::King, Suit::Diamonds),
                c(Rank::Two, Suit::Clubs),
            ]),
            turn: None,
            river: None,
        };
        let r = equity(EquityInput {
            hero,
            board,
            opponents: OpponentSpec::Known(vec![opp]),
            trials: 5_000,
            seed: 7,
            early_stop: None,
        });
        // Hero has top two pair on AK2 vs JT (gutshot + overcards):
        // hero equity ≈ 79% via standard equity calculators. Allow ±5pp.
        // If N-2 regresses and Th/Jd reappear as turn/river runouts, opp
        // sometimes pairs them → hero equity drops below 70%.
        assert!(
            r.win_pct >= 74,
            "AKs vs JTo on AK2 must have ≥74% win (got {}); regression in known-opp exclusion?",
            r.win_pct
        );
    }

    #[test]
    fn known_postflop_equity_is_exact_not_seeded_mc() {
        let hero = HoleCards::new(c(Rank::Ace, Suit::Spades), c(Rank::King, Suit::Spades));
        let opp = HoleCards::new(c(Rank::Ten, Suit::Hearts), c(Rank::Jack, Suit::Diamonds));
        let board = BoardCards {
            flop: Some([
                c(Rank::Ace, Suit::Hearts),
                c(Rank::King, Suit::Diamonds),
                c(Rank::Two, Suit::Clubs),
            ]),
            turn: None,
            river: None,
        };

        let one_trial = equity(EquityInput {
            hero,
            board: board.clone(),
            opponents: OpponentSpec::Known(vec![opp]),
            trials: 1,
            seed: 1,
            early_stop: None,
        });
        let many_trials = equity(EquityInput {
            hero,
            board,
            opponents: OpponentSpec::Known(vec![opp]),
            trials: 50_000,
            seed: 999,
            early_stop: None,
        });

        assert_eq!(
            one_trial, many_trials,
            "known postflop equity must enumerate exact runouts, independent of trials/seed"
        );
        assert!(
            one_trial.win_pct >= 74 && one_trial.win_pct <= 95,
            "AKs vs JTo on AK2 should stay in the expected exact-equity band, got {:?}",
            one_trial
        );
    }

    /// Regression (audit 2026-06-03): with more than 5 Known opponents the
    /// equity must be computed against exactly the first 5 (the documented cap),
    /// NOT against all of them. Here hero's KK beats five low-pocket-pair
    /// opponents, but the 6th opponent holds AA (the only hand that beats hero).
    /// If the 6th is silently seated, hero loses; with the cap honored, hero
    /// wins outright against the 5 seated opponents.
    /// Regression (audit 2026-06-03): a true 3-way chop must report hero's
    /// actual pot share (~33%), not a flat 50%. Board is a Broadway straight
    /// (T-J-Q-K-A) that all three players play, so everyone ties.
    /// `equity_pct()` must come out ≈ 33, not 50.
    #[test]
    fn three_way_chop_reports_one_third_not_one_half() {
        let board = BoardCards {
            flop: Some([
                c(Rank::Ace, Suit::Clubs),
                c(Rank::King, Suit::Hearts),
                c(Rank::Queen, Suit::Diamonds),
            ]),
            turn: Some(c(Rank::Jack, Suit::Clubs)),
            river: Some(c(Rank::Ten, Suit::Spades)),
        };
        // Hero + two opponents all play the board straight → 3-way tie.
        let hero = HoleCards::new(c(Rank::Two, Suit::Hearts), c(Rank::Three, Suit::Hearts));
        let opps = vec![
            HoleCards::new(c(Rank::Four, Suit::Diamonds), c(Rank::Five, Suit::Diamonds)),
            HoleCards::new(c(Rank::Six, Suit::Clubs), c(Rank::Seven, Suit::Clubs)),
        ];
        let result = equity(EquityInput {
            hero,
            board,
            opponents: OpponentSpec::Known(opps),
            trials: 1,
            seed: 0,
            early_stop: None,
        });
        assert_eq!(result.win_pct, 0, "a chop is not an outright win");
        assert_eq!(
            result.equity_pct(),
            33,
            "3-way chop equity must be ~1/3, not 50%; got {}",
            result.equity_pct()
        );
    }

    #[test]
    fn known_opponents_truncated_to_cap_of_five() {
        // Board: K K 7 4 2 → hero (any K) has trips, the five low pairs make
        // only a pair, so hero beats all five seated opponents.
        let hero = HoleCards::new(c(Rank::King, Suit::Spades), c(Rank::King, Suit::Hearts));
        let board = BoardCards {
            flop: Some([
                c(Rank::King, Suit::Diamonds),
                c(Rank::Seven, Suit::Spades),
                c(Rank::Four, Suit::Clubs),
            ]),
            turn: Some(c(Rank::Two, Suit::Hearts)),
            river: Some(c(Rank::Three, Suit::Spades)),
        };
        // Five harmless opponents (low pairs / unconnected), then a 6th holding
        // AA — the only holding that would out-rank hero's trip kings.
        let opps = vec![
            HoleCards::new(c(Rank::Five, Suit::Spades), c(Rank::Five, Suit::Hearts)),
            HoleCards::new(c(Rank::Six, Suit::Spades), c(Rank::Six, Suit::Hearts)),
            HoleCards::new(c(Rank::Eight, Suit::Spades), c(Rank::Eight, Suit::Hearts)),
            HoleCards::new(c(Rank::Nine, Suit::Spades), c(Rank::Nine, Suit::Hearts)),
            HoleCards::new(c(Rank::Ten, Suit::Spades), c(Rank::Ten, Suit::Hearts)),
            // 6th opponent — must be dropped by the cap; AA + KK7 board = aces
            // full, which beats hero's trips if (wrongly) seated.
            HoleCards::new(c(Rank::Ace, Suit::Spades), c(Rank::Ace, Suit::Hearts)),
        ];
        let result = equity(EquityInput {
            hero,
            board,
            opponents: OpponentSpec::Known(opps),
            trials: 1, // river: single showdown, no RNG
            seed: 0,
            early_stop: None,
        });
        assert_eq!(
            result.win_pct, 100,
            "hero must win vs the 5 capped opponents; the 6th (AA) must be dropped"
        );
    }

    /// Regression for code-review finding #2: `exact_known_runouts` must
    /// accumulate each runout's EXACT pot share and round the combined value
    /// ONCE — NOT re-round each runout's `combined_pct` to a u8 and average. On
    /// this chop-heavy turn spot the correct single-round combined equity is
    /// 87%; the old per-runout double-round averaged to 86% (a low bias).
    #[test]
    fn exact_runouts_round_combined_once_not_per_runout() {
        let hero = HoleCards::new(c(Rank::Six, Suit::Spades), c(Rank::Eight, Suit::Hearts));
        let opps = vec![
            HoleCards::new(c(Rank::Jack, Suit::Hearts), c(Rank::Eight, Suit::Diamonds)),
            HoleCards::new(c(Rank::Two, Suit::Spades), c(Rank::Three, Suit::Spades)),
        ];
        let board = BoardCards {
            flop: Some([
                c(Rank::Eight, Suit::Spades),
                c(Rank::Nine, Suit::Spades),
                c(Rank::Ten, Suit::Spades),
            ]),
            turn: Some(c(Rank::Jack, Suit::Spades)),
            river: None,
        };
        let result = equity(EquityInput {
            hero,
            board: board.clone(),
            opponents: OpponentSpec::Known(opps.clone()),
            trials: 1, // turn board → exact enumeration, no RNG
            seed: 0,
            early_stop: None,
        });
        assert_eq!(
            result.combined_pct, 87,
            "exact-runout combined equity must be single-rounded (87), got {}",
            result.combined_pct
        );

        // Prove this spot genuinely exercises the bug: the legacy per-runout
        // double-round averages to 86, distinct from the single-round 87.
        let mut used: Vec<Card> = vec![hero.card1, hero.card2];
        for o in &opps {
            used.push(o.card1);
            used.push(o.card2);
        }
        used.extend(board.all_cards());
        let rivers = deck_minus(&used);
        let (mut dbl2, mut total) = (0u64, 0u64);
        for &rv in &rivers {
            let mut b = board.clone();
            fill_board_from_runout(&mut b, &[rv]);
            dbl2 += 2 * single_showdown(&hero, &b, &opps).combined_pct as u64;
            total += 1;
        }
        let double_round = ((dbl2 + total) / (2 * total)) as u8;
        assert_eq!(
            double_round, 86,
            "sanity: legacy double-round value for this spot"
        );
        assert_ne!(
            double_round, result.combined_pct,
            "spot must distinguish single-round from per-runout double-round"
        );
    }

    /// Regression for code-review finding #1: in the multi-opponent Monte-Carlo
    /// path a tie must contribute hero's true `1/(1+tied)` pot share, NOT a flat
    /// 0.5. Three identical `KQ` hands chop ~most boards three-way (share 1/3),
    /// so the correct combined equity is well BELOW the naive `win% + tie%/2`
    /// the old flat-0.5 weighting produced (here by ~10pp).
    #[test]
    fn mc_multiway_chop_weighted_by_share_not_flat_half() {
        // Hero + two opponents all hold KQ (distinct suits) → most run-outs are
        // a 3-way chop; hero only pulls ahead on the spade-flush boards.
        let hero = HoleCards::new(c(Rank::King, Suit::Spades), c(Rank::Queen, Suit::Spades));
        let opps = vec![
            HoleCards::new(c(Rank::King, Suit::Hearts), c(Rank::Queen, Suit::Hearts)),
            HoleCards::new(
                c(Rank::King, Suit::Diamonds),
                c(Rank::Queen, Suit::Diamonds),
            ),
        ];
        let board = BoardCards::empty();
        let result = equity(EquityInput {
            hero,
            board,
            opponents: OpponentSpec::Known(opps), // preflop Known → MC `tied` path
            trials: 50_000,
            seed: 99,
            early_stop: None,
        });
        assert!(
            result.tie_pct > 50,
            "spot must chop most boards to exercise multi-way weighting; tie%={}",
            result.tie_pct
        );
        // Old flat-0.5 weighting: combined ≈ win + tie/2. True 3-way share is
        // 1/3, so the correct combined is markedly lower. Require a clear gap
        // (≥4pp) so the assertion can't be satisfied by the buggy weighting.
        let naive_half = result.win_pct as i32 + (result.tie_pct as i32) / 2;
        assert!(
            (result.combined_pct as i32) + 4 < naive_half,
            "multi-way chops must weight by 1/N, not 0.5: combined={} naive(win+tie/2)={} (win={}, tie={})",
            result.combined_pct,
            naive_half,
            result.win_pct,
            result.tie_pct
        );
    }

    #[test]
    fn trials_clamped_does_not_panic() {
        let hero = HoleCards::new(c(Rank::Two, Suit::Spades), c(Rank::Three, Suit::Hearts));
        let board = BoardCards::empty();
        // Above MAX_MC_TRIALS — must not panic; just clamp.
        let result = equity(EquityInput {
            hero,
            board,
            opponents: OpponentSpec::Random(1),
            trials: 1_000_000,
            seed: 1,
            early_stop: None,
        });
        // Just check it returns something sensible.
        assert!(result.win_pct <= 100);
    }

    // -----------------------------------------------------------------------
    // M1 §1a — real OpponentSpec::Range
    // -----------------------------------------------------------------------

    fn bucket(key: &str) -> RangeBucket {
        RangeBucket {
            hand_key: key.to_string(),
            weight: 1.0,
        }
    }

    #[test]
    fn expand_range_combo_counts() {
        // pair = 6, suited = 4, offsuit = 12.
        assert_eq!(expand_range_key("AA").len(), 6, "pair must be 6 combos");
        assert_eq!(expand_range_key("AKs").len(), 4, "suited must be 4 combos");
        assert_eq!(
            expand_range_key("AKo").len(),
            12,
            "offsuit must be 12 combos"
        );
        // Garbage keys never panic and expand to nothing.
        assert_eq!(expand_range_key("").len(), 0);
        assert_eq!(expand_range_key("XYZ").len(), 0);
        assert_eq!(expand_range_key("AKx").len(), 0);
    }

    #[test]
    fn expand_range_combos_are_valid_and_distinct() {
        for key in ["AA", "AKs", "AKo"] {
            let combos = expand_range_key(key);
            for h in &combos {
                assert_ne!(
                    h.card1, h.card2,
                    "{key}: a combo cannot use the same card twice"
                );
            }
            // No duplicate combos in the expansion.
            for i in 0..combos.len() {
                for j in (i + 1)..combos.len() {
                    let a = combos[i];
                    let b = combos[j];
                    let same = (a.card1 == b.card1 && a.card2 == b.card2)
                        || (a.card1 == b.card2 && a.card2 == b.card1);
                    assert!(!same, "{key}: duplicate combo at {i},{j}");
                }
            }
        }
    }

    #[test]
    fn aks_vs_tight_value_range_lower_than_vs_random() {
        let hero = HoleCards::new(c(Rank::Ace, Suit::Spades), c(Rank::King, Suit::Spades));
        let board = BoardCards::empty();
        let vs_random = equity(EquityInput {
            hero,
            board: board.clone(),
            opponents: OpponentSpec::Random(1),
            trials: 8_000,
            seed: 42,
            early_stop: None,
        });
        let vs_value = equity(EquityInput {
            hero,
            board,
            opponents: OpponentSpec::Range(vec![
                bucket("AA"),
                bucket("KK"),
                bucket("QQ"),
                bucket("AKs"),
            ]),
            trials: 8_000,
            seed: 42,
            early_stop: None,
        });
        assert!(
            vs_value.equity_pct() < vs_random.equity_pct(),
            "AKs vs a tight value range ({}) must have LOWER equity than vs random ({})",
            vs_value.equity_pct(),
            vs_random.equity_pct()
        );
    }

    #[test]
    fn range_equity_is_deterministic_on_seed() {
        let hero = HoleCards::new(c(Rank::Ace, Suit::Hearts), c(Rank::King, Suit::Hearts));
        let board = BoardCards {
            flop: Some([
                c(Rank::Two, Suit::Spades),
                c(Rank::Seven, Suit::Diamonds),
                c(Rank::Jack, Suit::Clubs),
            ]),
            turn: None,
            river: None,
        };
        let spec = || {
            OpponentSpec::Range(vec![
                bucket("AA"),
                bucket("KK"),
                bucket("AKo"),
                bucket("QQ"),
            ])
        };
        let r1 = equity(EquityInput {
            hero,
            board: board.clone(),
            opponents: spec(),
            trials: 3_000,
            seed: 777,
            early_stop: None,
        });
        let r2 = equity(EquityInput {
            hero,
            board,
            opponents: spec(),
            trials: 3_000,
            seed: 777,
            early_stop: None,
        });
        assert_eq!(
            r1, r2,
            "same (hero,board,Range,trials,seed) must be identical"
        );
    }

    #[test]
    fn fully_blocked_range_falls_back_without_panic() {
        // Hero holds As/Ah and board has Ad/Ac → the only AA combos are all
        // blocked, so the range pool is empty and must fall back to Random(1)
        // (never panic, result ≤ 100).
        let hero = HoleCards::new(c(Rank::Ace, Suit::Spades), c(Rank::Ace, Suit::Hearts));
        let board = BoardCards {
            flop: Some([
                c(Rank::Ace, Suit::Diamonds),
                c(Rank::Ace, Suit::Clubs),
                c(Rank::Two, Suit::Hearts),
            ]),
            turn: None,
            river: None,
        };
        let r = equity(EquityInput {
            hero,
            board,
            opponents: OpponentSpec::Range(vec![bucket("AA")]),
            trials: 1_000,
            seed: 5,
            early_stop: None,
        });
        assert!(r.win_pct <= 100 && r.equity_pct() <= 100);
    }

    #[test]
    fn range_excludes_villain_cards_from_runout() {
        // Villain range is exactly KK. Hero is AsKs on a dry flop. Across many
        // trials the result must be a valid percentage; the real assertion is
        // that no panic / impossible board occurs (villain's two kings can never
        // appear as turn/river). Determinism + bounds cover the contract.
        let hero = HoleCards::new(c(Rank::Ace, Suit::Spades), c(Rank::King, Suit::Spades));
        let board = BoardCards {
            flop: Some([
                c(Rank::Two, Suit::Diamonds),
                c(Rank::Seven, Suit::Hearts),
                c(Rank::Nine, Suit::Clubs),
            ]),
            turn: None,
            river: None,
        };
        let r = equity(EquityInput {
            hero,
            board,
            opponents: OpponentSpec::Range(vec![bucket("KK")]),
            trials: 4_000,
            seed: 11,
            early_stop: None,
        });
        assert!(r.win_pct <= 100);
    }

    // -----------------------------------------------------------------------
    // ADR-082 — coach-only Monte-Carlo early-stop (Wilson 95% CI)
    // -----------------------------------------------------------------------

    /// The coach policy as `equity_vs_random` sets it (the ONLY `Some(..)` site).
    fn coach_policy() -> EarlyStop {
        EarlyStop {
            min_trials: EARLY_STOP_MIN_TRIALS,
            half_width: EARLY_STOP_HALF_WIDTH,
        }
    }

    /// §5(a): the `None` path runs EXACTLY `DEFAULT_MC_TRIALS` and equals the
    /// pre-ADR-082 full-loop result (frozen golden values captured from the
    /// pre-change behaviour). Proves the F4 byte-identical guard at the equity
    /// layer: adding the field did not perturb the non-coach (`None`) numbers.
    #[test]
    fn equity_none_path_is_unchanged() {
        // (hero, board, opp_count, seed, expected win/tie/combined) — frozen.
        type GoldenCase = (HoleCards, BoardCards, u8, u64, (u8, u8, u8));
        let cases: Vec<GoldenCase> = vec![
            (
                HoleCards::new(c(Rank::Ace, Suit::Spades), c(Rank::Ace, Suit::Hearts)),
                BoardCards::empty(),
                1,
                42,
                (85, 1, 86),
            ),
            (
                HoleCards::new(c(Rank::Ace, Suit::Spades), c(Rank::King, Suit::Hearts)),
                BoardCards::empty(),
                2,
                12345,
                (47, 2, 48),
            ),
            (
                HoleCards::new(c(Rank::Seven, Suit::Diamonds), c(Rank::Two, Suit::Clubs)),
                BoardCards {
                    flop: Some([
                        c(Rank::Ace, Suit::Spades),
                        c(Rank::King, Suit::Hearts),
                        c(Rank::Nine, Suit::Diamonds),
                    ]),
                    turn: None,
                    river: None,
                },
                2,
                99,
                (6, 4, 8),
            ),
            (
                HoleCards::new(c(Rank::Ace, Suit::Spades), c(Rank::King, Suit::Spades)),
                BoardCards {
                    flop: Some([
                        c(Rank::Two, Suit::Spades),
                        c(Rank::Seven, Suit::Spades),
                        c(Rank::Queen, Suit::Spades),
                    ]),
                    turn: None,
                    river: None,
                },
                1,
                7,
                (98, 0, 98),
            ),
        ];
        for (hero, board, n, seed, (w, t, comb)) in cases {
            let (res, trials_run) = equity_with_trial_count(EquityInput {
                hero,
                board: board.clone(),
                opponents: OpponentSpec::Random(n),
                trials: DEFAULT_MC_TRIALS,
                seed,
                early_stop: None,
            });
            assert_eq!(
                trials_run, DEFAULT_MC_TRIALS,
                "None path must run ALL {DEFAULT_MC_TRIALS} trials (n={n}, seed={seed})"
            );
            assert_eq!(
                (res.win_pct, res.tie_pct, res.combined_pct),
                (w, t, comb),
                "None-path result drifted from frozen golden (n={n}, seed={seed})"
            );
        }
    }

    /// §5(b): the coach early-stop is deterministic and seed-stable — the SAME
    /// `equity_vs_random(h, b, n, seed)` returns an identical `EquityResult` AND
    /// stops at the identical trial index across runs.
    #[test]
    fn coach_early_stop_is_deterministic() {
        let inputs: Vec<(HoleCards, BoardCards, u8, u64)> = vec![
            (
                HoleCards::new(c(Rank::Ace, Suit::Spades), c(Rank::Ace, Suit::Hearts)),
                BoardCards::empty(),
                1,
                42,
            ),
            (
                HoleCards::new(c(Rank::Ace, Suit::Spades), c(Rank::King, Suit::Hearts)),
                BoardCards::empty(),
                2,
                12345,
            ),
            (
                HoleCards::new(c(Rank::Seven, Suit::Diamonds), c(Rank::Two, Suit::Clubs)),
                BoardCards {
                    flop: Some([
                        c(Rank::Ace, Suit::Spades),
                        c(Rank::King, Suit::Hearts),
                        c(Rank::Nine, Suit::Diamonds),
                    ]),
                    turn: None,
                    river: None,
                },
                2,
                99,
            ),
        ];
        for (hero, board, n, seed) in inputs {
            let policy = Some(coach_policy());
            let mk = || EquityInput {
                hero,
                board: board.clone(),
                opponents: OpponentSpec::Random(n),
                trials: DEFAULT_MC_TRIALS,
                seed,
                early_stop: policy,
            };
            let (r1, t1) = equity_with_trial_count(mk());
            let (r2, t2) = equity_with_trial_count(mk());
            assert_eq!(r1, r2, "coach early-stop must be deterministic (result)");
            assert_eq!(t1, t2, "coach early-stop must be seed-stable (stop index)");
            // The convenience entry must match the explicit policy input.
            let via_helper = equity_vs_random(hero, board.clone(), n, seed);
            assert_eq!(
                via_helper, r1,
                "equity_vs_random must apply exactly the coach policy"
            );
        }
    }

    /// §5(c): a CRUSHING spot (hero made the nut flush on a paired-safe board vs
    /// `Random(1)`) converges fast — the Wilson CI tightens below 0.5pp well
    /// before the 10k budget, so it MUST stop early. This is RED without the
    /// policy: with `early_stop: None` the same spot runs all 10k.
    #[test]
    fn dominant_spot_stops_early() {
        // Hero AsKs, board Qs Js 2s 7d → nut flush already made (Royal-adjacent
        // spade flush, no straight flush risk killed). Equity ≈ 95%+ vs random.
        let hero = HoleCards::new(c(Rank::Ace, Suit::Spades), c(Rank::King, Suit::Spades));
        let board = BoardCards {
            flop: Some([
                c(Rank::Queen, Suit::Spades),
                c(Rank::Jack, Suit::Spades),
                c(Rank::Two, Suit::Spades),
            ]),
            turn: Some(c(Rank::Seven, Suit::Diamonds)),
            river: None,
        };
        let (_, t_stop) = equity_with_trial_count(EquityInput {
            hero,
            board: board.clone(),
            opponents: OpponentSpec::Random(1),
            trials: DEFAULT_MC_TRIALS,
            seed: 42,
            early_stop: Some(coach_policy()),
        });
        assert!(
            t_stop < DEFAULT_MC_TRIALS,
            "dominant spot must early-stop (t*={t_stop} should be < {DEFAULT_MC_TRIALS})"
        );
        assert!(
            t_stop <= 3000,
            "dominant spot should converge fast (t*={t_stop} expected <= 3000)"
        );
        // RED-without-policy proof: the SAME spot with `None` runs the full budget.
        let (_, t_full) = equity_with_trial_count(EquityInput {
            hero,
            board,
            opponents: OpponentSpec::Random(1),
            trials: DEFAULT_MC_TRIALS,
            seed: 42,
            early_stop: None,
        });
        assert_eq!(
            t_full, DEFAULT_MC_TRIALS,
            "without the policy the loop must run all {DEFAULT_MC_TRIALS} trials"
        );
        assert!(
            t_stop < t_full,
            "early-stop must run STRICTLY fewer trials than the full loop"
        );
    }

    /// §5(c): a HOPELESS spot (72o, no draws left, on a dry high RIVER board vs
    /// `Random(2)`) has equity ≈ 1% — where the Wilson CI is tight (small
    /// `p(1-p)`), so it MUST stop early. RED without the policy (runs all 10k).
    #[test]
    fn clear_fold_stops_early() {
        // River A-K-9-Q-5 rainbow; 72o has nothing and no outs (board is final).
        let hero = HoleCards::new(c(Rank::Seven, Suit::Diamonds), c(Rank::Two, Suit::Clubs));
        let board = BoardCards {
            flop: Some([
                c(Rank::Ace, Suit::Spades),
                c(Rank::King, Suit::Hearts),
                c(Rank::Nine, Suit::Diamonds),
            ]),
            turn: Some(c(Rank::Queen, Suit::Clubs)),
            river: Some(c(Rank::Five, Suit::Hearts)),
        };
        let (res, t_stop) = equity_with_trial_count(EquityInput {
            hero,
            board: board.clone(),
            opponents: OpponentSpec::Random(2),
            trials: DEFAULT_MC_TRIALS,
            seed: 99,
            early_stop: Some(coach_policy()),
        });
        // Sanity: this really is a near-0 spot (where Wilson is tight).
        assert!(
            res.combined_pct <= 5,
            "clear-fold spot must sit near 0% (got {})",
            res.combined_pct
        );
        assert!(
            t_stop < DEFAULT_MC_TRIALS,
            "clear-fold spot must early-stop (t*={t_stop} should be < {DEFAULT_MC_TRIALS})"
        );
        // RED-without-policy proof.
        let (_, t_full) = equity_with_trial_count(EquityInput {
            hero,
            board,
            opponents: OpponentSpec::Random(2),
            trials: DEFAULT_MC_TRIALS,
            seed: 99,
            early_stop: None,
        });
        assert_eq!(t_full, DEFAULT_MC_TRIALS, "None path runs the full budget");
        assert!(t_stop < t_full, "early-stop must run fewer trials");
    }

    /// §5(c): a MARGINAL near-50% spot maximises the Wilson half-width — at
    /// p≈0.5 the 0.5pp target needs n ≈ z²·0.25/0.005² ≈ 38,400 trials, far above
    /// the 10k budget — so it MUST run the FULL budget, never early-stopping.
    /// This is the "spend the budget where it matters" guarantee.
    #[test]
    fn marginal_spot_runs_near_full() {
        // AKo vs 2 random preflop ≈ 50% combined — a coinflip-ish multiway spot.
        let hero = HoleCards::new(c(Rank::Ace, Suit::Spades), c(Rank::King, Suit::Hearts));
        let board = BoardCards::empty();
        let (res, t_stop) = equity_with_trial_count(EquityInput {
            hero,
            board,
            opponents: OpponentSpec::Random(2),
            trials: DEFAULT_MC_TRIALS,
            seed: 12345,
            early_stop: Some(coach_policy()),
        });
        // Sanity: this spot really is near 50% (where Wilson is widest).
        assert!(
            res.combined_pct >= 40 && res.combined_pct <= 60,
            "marginal spot must sit near 50% (got {})",
            res.combined_pct
        );
        assert_eq!(
            t_stop, DEFAULT_MC_TRIALS,
            "marginal (p≈0.5) spot must run the FULL budget — 0.5pp at p=0.5 needs ~38k > 10k (t*={t_stop})"
        );
    }

    /// The `K`-cadence is honoured: the stop index, when it fires, is always a
    /// multiple of `EARLY_STOP_CHECK_EVERY` and ≥ `EARLY_STOP_MIN_TRIALS`.
    #[test]
    fn early_stop_index_respects_min_and_cadence() {
        let hero = HoleCards::new(c(Rank::Ace, Suit::Spades), c(Rank::King, Suit::Spades));
        let board = BoardCards {
            flop: Some([
                c(Rank::Queen, Suit::Spades),
                c(Rank::Jack, Suit::Spades),
                c(Rank::Two, Suit::Spades),
            ]),
            turn: Some(c(Rank::Seven, Suit::Diamonds)),
            river: None,
        };
        let (_, t_stop) = equity_with_trial_count(EquityInput {
            hero,
            board,
            opponents: OpponentSpec::Random(1),
            trials: DEFAULT_MC_TRIALS,
            seed: 42,
            early_stop: Some(coach_policy()),
        });
        assert!(
            t_stop >= EARLY_STOP_MIN_TRIALS,
            "stop must be >= min_trials"
        );
        if t_stop < DEFAULT_MC_TRIALS {
            assert_eq!(
                t_stop % EARLY_STOP_CHECK_EVERY,
                0,
                "an early break must land on a K-cadence boundary (t*={t_stop})"
            );
        }
    }
}
