//! Clean-room generator for `engine/data/preflop_v2.json`.
//!
//! ## What this is (HONESTY — read first)
//!
//! This regenerates `engine/data/preflop_v2.json` from a genuine
//! **discounted-regret CFR variant (DCFR-style coefficients)
//! approximate-equilibrium solve** over a SIMPLIFIED 6-max / 100bb preflop tree
//! (`engine::solver::preflop_cfr`). The update form is a discounted-regret
//! *variant* using the canonical DCFR `(α,β,γ) = (1.5, 0, 2)` coefficients — NOT
//! the canonical DCFR update (see `preflop_cfr.rs` for the exact form). It is
//! **NOT a true
//! postflop GTO solve** — the terminal values substitute all-in equity + a
//! documented per-class **equity-realization factor** `R` for true postflop EV
//! (the engine has no postflop solver). So the equilibrium MATH is real (regret
//! matching, opponent reactions modelled in-tree, mixed frequencies emitted), but
//! the absolute frequencies are only as correct as the `R` / sizing model.
//!
//! Honest label everywhere (code `SolveMethod::CfrEquityRealization`, badge, JSON
//! `_source`): **"CFR approx. equilibrium · equity-realization"** — NEVER "GTO".
//!
//! ## Method
//!
//! 1. Precompute a deterministic, seeded 169×169 all-in-equity matrix (engine's
//!    OWN Monte-Carlo equity engine, `engine::solver::equity`).
//! 2. Run full-traversal DCFR per (position, bucket) as a 2-player game: hero
//!    {fold, continue} vs a LINE-ANCHORED villain {fold, call/raise} (the villain
//!    range is the spot's prior action — a wide field for opens, a narrow
//!    value+bluff raise range for the 3bet/4bet lines). Terminals: fold (exact
//!    uncontested), all-in (matrix equity), see-flop (`equity × R`). The position
//!    lever is a "best-of-N defenders" model + a minimum-defense-frequency floor;
//!    these and the sizing tree are documented MODEL PARAMETERS, not solved.
//! 3. Emit hero's converged CONTINUE frequency per class as MIXED frequencies
//!    (upgrade over the previous binary chart), rounded to 2 decimals so float
//!    noise can never perturb the bytes. Premiums (AA/KK/QQ/AKs) are
//!    force-included; obvious trash is hard-excluded; the structural invariants
//!    (vs_4bet ⊆ vs_3bet, RFI widths monotone UTG⊆MP⊆CO⊆BTN, AA in every
//!    non-empty bucket) are enforced as a safety net for the contract guard.
//!
//! ## Reproducibility
//!
//! A fixed equity-matrix seed, a fixed MC trial count, a fixed DCFR iteration
//! count, a deterministic traversal (canonical 169 order, NO HashMap iteration),
//! and a stable hand-written JSON emitter together give byte-identical output
//! every run, in debug and release. (DETERMINISM is exact for any trial/iter
//! count — those only set the precision; the seed makes the sequence fixed.)
//!
//! ## Run
//!
//! ```text
//! cargo run -p engine --example gen_preflop_ranges --release > engine/data/preflop_v2.json
//! # verify reproducibility (no diff against the committed file):
//! cargo run -p engine --example gen_preflop_ranges --release | diff - engine/data/preflop_v2.json
//! ```
//! (`--release` is ~10× faster; the OUTPUT is identical to a debug run.)

use engine::solver::preflop_cfr::{
    canonical_keys, solve, Bucket, Position, CFR_ITERS, EQUITY_SEED, EQUITY_TRIALS,
};

/// Premiums force-included in EVERY continuing bucket at full frequency (never
/// let the model drop the nuts, and satisfy the contract's AA/KK/AKs invariant).
const ALWAYS_IN: [&str; 4] = ["AA", "KK", "QQ", "AKs"];

/// A class is admitted to a bucket iff its rounded continue frequency clears this
/// floor — removes sub-1% "dust" so the published chart is clean.
const INCLUDE_FLOOR: f32 = 0.05;

fn main() {
    let keys = canonical_keys();
    let res = solve(&keys);

    // Collect rounded continue frequency per (position, bucket, key). Index by
    // the canonical key order so iteration is deterministic (NO HashMap).
    // `freqs[pi][bi][ci]` = published frequency in [0,1] (0 ⇒ folded/absent).
    let mut freqs: Vec<Vec<Vec<f32>>> = vec![vec![vec![0.0; keys.len()]; 5]; 6];

    for (pi, _pos) in Position::ALL.iter().enumerate() {
        for (bi, _bucket) in Bucket::ALL.iter().enumerate() {
            let Some(solved) = &res.buckets[pi][bi] else {
                continue; // structurally empty (BB RFI / UTG facing_open)
            };
            for (ci, key) in keys.iter().enumerate() {
                let mut f = round2(solved.freq[ci] as f32);
                if is_trash(key) {
                    f = 0.0;
                }
                if ALWAYS_IN.contains(&key.as_str()) {
                    f = 1.0;
                }
                if f < INCLUDE_FLOOR {
                    f = 0.0;
                }
                freqs[pi][bi][ci] = f;
            }
        }
    }

    // ---- Structural invariants (safety net for the contract guard) ----
    enforce_invariants(&keys, &mut freqs);

    // ---- Emit byte-stable JSON ----
    print!("{}", emit_json(&keys, &freqs));
}

/// Round a frequency to 2 decimals (stable, midpoint-away-from-zero) so float
/// noise can never perturb the emitted bytes.
fn round2(f: f32) -> f32 {
    (f * 100.0).round() / 100.0
}

/// Obvious trash that should never enter ANY range (a hard floor independent of
/// the solve): low, unsuited, unconnected hands (e.g. 72o, 92o, T2o).
fn is_trash(key: &str) -> bool {
    let b = key.as_bytes();
    // pairs and any suited / any-ace hand are never pure trash.
    if b.len() == 2 {
        return false; // pair
    }
    if b.len() != 3 {
        return true;
    }
    let suited = b[2] == b's';
    let hi = rank_ord(b[0]);
    let lo = rank_ord(b[1]);
    let (hi, lo) = if hi >= lo { (hi, lo) } else { (lo, hi) };
    if suited || hi == 12 {
        return false; // suited anything / any ace
    }
    let gap = hi - lo;
    let hi_broadway = hi >= 8; // Ten or better
    if !hi_broadway && gap >= 4 {
        return true;
    }
    if !hi_broadway && lo < 4 {
        // low card below Six with a non-broadway top
        return true;
    }
    false
}

/// Rank ordinal 0..=12 for the single-char rank code (2..A).
fn rank_ord(ch: u8) -> i32 {
    match ch {
        b'2' => 0,
        b'3' => 1,
        b'4' => 2,
        b'5' => 3,
        b'6' => 4,
        b'7' => 5,
        b'8' => 6,
        b'9' => 7,
        b'T' => 8,
        b'J' => 9,
        b'Q' => 10,
        b'K' => 11,
        b'A' => 12,
        _ => -1,
    }
}

/// Enforce the poker-fundamental structural invariants the contract guard checks:
///  * vs_4bet ⊆ vs_3bet per position, AND vs_4bet_freq ≤ vs_3bet_freq per key
///    (the continuing range only tightens — a hand cannot continue MORE OFTEN
///    facing a 4-bet than facing a 3-bet). F6 (2026-06-25): the prior net only
///    fixed KEY membership (add to vs_3bet if missing), NOT the frequency — a
///    model that emitted vs_4bet_freq > vs_3bet_freq for a hand PRESENT in both
///    would have slipped past. We now raise vs_3bet to `max(vs_3bet, vs_4bet)`
///    per key, guaranteeing the freq-monotone invariant the contract test asserts.
///  * RFI widths monotone UTG⊆MP⊆CO⊆BTN by class count — the solve gives this;
///    we assert it here so a future model change can't silently break the guard.
///  * AA present in every non-empty bucket — guaranteed by ALWAYS_IN.
fn enforce_invariants(keys: &[String], freqs: &mut [Vec<Vec<f32>>]) {
    const VS3: usize = 2;
    const VS4: usize = 3;
    let n = keys.len();
    for row in freqs.iter_mut() {
        // Read vs_4bet first (immutable copy) to avoid aliasing the row twice.
        let vs4: Vec<f32> = row[VS4].clone();
        for (ci, &v4) in vs4.iter().enumerate().take(n) {
            if v4 > 0.0 && row[VS3][ci] < v4 {
                // The hand continues vs a 4-bet MORE OFTEN than (or is absent
                // from) vs a 3-bet — impossible for a monotone continuing range.
                // Raise vs_3bet to at least the vs_4bet frequency so BOTH the
                // membership (vs_4bet ⊆ vs_3bet) AND the frequency-tightening
                // (vs_4bet_freq ≤ vs_3bet_freq) invariants hold. The model
                // already gives this; this is a safety net for the contract guard.
                row[VS3][ci] = v4;
            }
        }
    }
    // Assert RFI monotonicity (UTG=0, MP=1, CO=2, BTN=3 in Position::ALL order).
    let rfi_count = |pi: usize| freqs[pi][0].iter().filter(|&&f| f > 0.0).count();
    let (utg, mp, co, btn) = (rfi_count(0), rfi_count(1), rfi_count(2), rfi_count(3));
    assert!(
        utg <= mp && mp <= co && co <= btn && utg < btn,
        "RFI widths must widen UTG<=MP<=CO<=BTN (strict UTG<BTN); got UTG={utg} MP={mp} CO={co} BTN={btn}"
    );
}

// ---------------------------------------------------------------------------
// JSON emitter (stable, hand-written for byte determinism)
// ---------------------------------------------------------------------------

const POS_KEYS: [&str; 6] = ["UTG", "MP", "CO", "BTN", "SB", "BB"];
const BUCKET_KEYS: [&str; 5] = ["RFI", "facing_open", "vs_3bet", "vs_4bet", "facing_limp"];

fn emit_json(keys: &[String], freqs: &[Vec<Vec<f32>>]) -> String {
    let mut out = String::new();
    out.push_str("{\n");
    out.push_str(&format!("  \"_source\": {},\n", json_str(&source())));
    out.push_str(&format!("  \"_doc\": {},\n", json_str(DOC)));
    out.push_str("  \"version\": 3,\n");
    out.push_str("  \"table_size\": \"6max\",\n");
    out.push_str("  \"stack_bucket_bb\": 100,\n");
    out.push_str("  \"ranges\": {\n");

    for (pi, pos) in POS_KEYS.iter().enumerate() {
        out.push_str(&format!("    \"{pos}\": {{\n"));
        for (bi, bucket) in BUCKET_KEYS.iter().enumerate() {
            // Entries in canonical key order, only those with freq > 0.
            let entries: Vec<String> = keys
                .iter()
                .enumerate()
                .filter(|(ci, _)| freqs[pi][bi][*ci] > 0.0)
                .map(|(ci, k)| format!("\"{k}\": {}", fmt_freq(freqs[pi][bi][ci])))
                .collect();
            if entries.is_empty() {
                out.push_str(&format!("      \"{bucket}\": {{}}"));
            } else {
                out.push_str(&format!("      \"{bucket}\": {{\n"));
                for (idx, chunk) in entries.chunks(10).enumerate() {
                    out.push_str("        ");
                    out.push_str(&chunk.join(", "));
                    let last_chunk = (idx + 1) * 10 >= entries.len();
                    if last_chunk {
                        out.push('\n');
                    } else {
                        out.push_str(",\n");
                    }
                }
                out.push_str("      }");
            }
            if bi + 1 < BUCKET_KEYS.len() {
                out.push_str(",\n");
            } else {
                out.push('\n');
            }
        }
        out.push_str("    }");
        if pi + 1 < POS_KEYS.len() {
            out.push_str(",\n");
        } else {
            out.push('\n');
        }
    }

    out.push_str("  }\n");
    out.push_str("}\n");
    out
}

/// Format a frequency for JSON: `1.0` for full-frequency, else 2-decimal (no
/// trailing-zero ambiguity — always exactly 2 decimals for mixes, `1.0` for
/// pure). The consumer (`preflop_charts::RawCharts`) parses `f32`.
fn fmt_freq(f: f32) -> String {
    if (f - 1.0).abs() < 1e-6 {
        "1.0".to_string()
    } else {
        // 2 decimals; `round2` already quantized so this is exact.
        format!("{f:.2}")
    }
}

/// JSON-escape a string into a quoted literal.
fn json_str(s: &str) -> String {
    let mut out = String::with_capacity(s.len() + 2);
    out.push('"');
    for ch in s.chars() {
        match ch {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            _ => out.push(ch),
        }
    }
    out.push('"');
    out
}

/// The `_source` provenance/method statement (carries the honest label, the
/// seed, the iteration count, the R-factor model, and the clean-room attestation
/// that the contract guard checks).
fn source() -> String {
    format!(
        "BluffKing clean-room generated by engine/examples/gen_preflop_ranges.rs via a \
discounted-regret CFR variant (DCFR-style coefficients) approximate-equilibrium solve over a \
SIMPLIFIED 6-max/100bb preflop tree (engine::solver::preflop_cfr). METHOD: CFR approx. equilibrium \
with all-in-equity + per-class \
equity-realization terminal EV — this is NOT a true postflop GTO solve and is NOT \
game-theory-optimal. The equilibrium math is genuine (regret matching, opponent reactions modelled \
in-tree, mixed frequencies emitted), but terminal values substitute all-in equity (BluffKing's OWN \
Monte-Carlo equity engine, engine::solver::equity) plus a documented bounded equity-realization \
factor R in [0.70,1.12] for true postflop EV (the engine has no postflop solver). MODEL PARAMETERS \
(fixed inputs, NOT solved): open 2.5bb, 3-bet 3.2x open, 4-bet 2.2x 3-bet, 5-bet jam; villain 3-bet \
freq ~11%, 4-bet freq ~5.5%; a best-of-N-defenders position model + minimum-defense-frequency floor. \
A different sizing/R model yields different ranges. Premiums (AA/KK/QQ/AKs) are force-included; trash \
is excluded; vs_4bet tightens below vs_3bet; RFI widths widen UTG<MP<CO<BTN. HONEST LABEL: \
\"CFR approx. equilibrium · equity-realization\" — NEVER \"GTO\". Generated entirely from BluffKing's \
own equity engine + CFR; no third-party charts, solver outputs, or commercial range datasets were \
used or consulted. REPRODUCIBLE: equity-matrix seed {EQUITY_SEED:#018x}, {EQUITY_TRIALS} MC trials \
per cell, {CFR_ITERS} discounted-regret CFR iterations (DCFR-style coefficients, variant update \
form), deterministic traversal + stable JSON emitter => \
byte-identical regen. Regenerate with `cargo run -p engine --example gen_preflop_ranges --release`."
    )
}

const DOC: &str = "Keys are 169-grid notation: pairs 'AA'..'22', suited 'AKs'..'32s', offsuit \
'AKo'..'32o'. Frequency is hero's CONTINUE frequency in [0,1] (mixed strategies: e.g. 0.5 = continue \
half the time); absent = fold (frequency 0). 'Continue' means raise for RFI/facing_open/vs_3bet/ \
facing_limp and jam for vs_4bet. Action buckets: 'RFI' (open-raise unopened), 'facing_open' (single \
raiser ahead, no 3bet yet), 'vs_3bet' (continuing after our open got 3bet), 'vs_4bet' (continue \
facing a 4bet), 'facing_limp' (iso-raise vs limp). Positions UTG/MP/CO/BTN/SB/BB (HJ folds into MP \
for 6-max). BB has no RFI; UTG has no facing_open. v3 (CFR approx. equilibrium + equity-realization) \
emits MIXED frequencies; v1/v2 were binary.";
