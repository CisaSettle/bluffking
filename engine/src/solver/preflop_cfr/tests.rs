//! Preflop-CFR unit tests (moved out of `preflop_cfr.rs`: the inline test
//! module was more than half the file). Same module path, same `use super::*`.

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
