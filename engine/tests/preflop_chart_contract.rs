//! Clean-room contract guard for `engine/data/preflop_v2.json` (codex audit F3;
//! upgraded for the CFR approx.-equilibrium solve).
//!
//! Two RED-first regression tests protect the clean-room provenance, the HONEST
//! labelling, and the poker-sanity of the committed preflop chart:
//!
//! 1. `preflop_generator_output_matches_committed_json` — runs the
//!    `gen_preflop_ranges` example (a discounted-regret CFR variant
//!    approx.-equilibrium solve) and asserts
//!    its stdout is BYTE-IDENTICAL to the committed `engine/data/preflop_v2.json`.
//!    If anyone hand-edits the data file (re-introducing untracked/third-party
//!    values) or changes the generator without regenerating, this goes RED. (Run
//!    with `--release`; the output is identical in debug and release — see test.)
//! 2. `preflop_chart_contract_preserves_clean_room_and_sanity` — parses the
//!    committed file directly and asserts (a) the `_source`/`_doc` carry NO
//!    third-party provenance token (gto wizard / pokercoaching / piosolver /
//!    gto+/ snowie / …), name the CFR + equity-realization method PRECISELY, and
//!    NEVER positively claim "GTO" (the founder's #1 honesty rule); and (b) the
//!    grid honours poker-fundamental invariants (AA/KK/AKs in every RFI, 72o
//!    nowhere, vs_4bet ⊆ vs_3bet per position, RFI widths monotone
//!    UTG ⊆ MP ⊆ CO ⊆ BTN). No DB needed (pure data).

use std::collections::BTreeMap;
use std::path::PathBuf;
use std::process::Command;

use serde::Deserialize;

/// The committed data file, embedded at compile time so the parse test never
/// depends on cwd.
const COMMITTED: &str = include_str!("../data/preflop_v2.json");

#[derive(Debug, Deserialize)]
struct Charts {
    #[serde(rename = "_source")]
    source: String,
    #[serde(rename = "_doc")]
    doc: String,
    ranges: BTreeMap<String, BTreeMap<String, BTreeMap<String, f32>>>,
}

fn parse() -> Charts {
    serde_json::from_str(COMMITTED).expect("committed preflop_v2.json must parse")
}

/// `engine/data/preflop_v2.json` resolved from this crate's manifest dir.
fn committed_path() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("data")
        .join("preflop_v2.json")
}

#[test]
fn preflop_generator_output_matches_committed_json() {
    // Run the example generator and capture stdout. `env!("CARGO")` is the
    // cargo that launched the test runner, so this re-uses the same toolchain.
    // `--release` is used because the generator runs a full discounted-regret CFR
    // solve over a
    // deterministic 169×169 equity matrix; the OUTPUT is byte-identical in debug
    // and release (determinism is from the fixed seed, not the opt level), so
    // running release here keeps the test fast without changing what it checks.
    let out = Command::new(env!("CARGO"))
        .args([
            "run",
            "--quiet",
            "--release",
            "-p",
            "engine",
            "--example",
            "gen_preflop_ranges",
        ])
        .current_dir(env!("CARGO_MANIFEST_DIR"))
        .output()
        .expect("failed to run gen_preflop_ranges example");

    assert!(
        out.status.success(),
        "generator exited non-zero; stderr:\n{}",
        String::from_utf8_lossy(&out.stderr)
    );

    let generated = String::from_utf8(out.stdout).expect("generator stdout must be UTF-8");
    let committed = std::fs::read_to_string(committed_path()).expect("read committed json");

    assert_eq!(
        generated, committed,
        "gen_preflop_ranges output is NOT byte-identical to engine/data/preflop_v2.json. \
         Regenerate with `cargo run -p engine --example gen_preflop_ranges --release > engine/data/preflop_v2.json`."
    );
}

#[test]
fn preflop_chart_contract_preserves_clean_room_and_sanity() {
    let charts = parse();

    // (a) Clean-room: no third-party provenance token anywhere in the metadata.
    let meta = format!("{} {}", charts.source, charts.doc).to_lowercase();
    for taint in [
        "gto wizard",
        "gtowizard",
        "pokercoaching",
        "poker coaching",
        "piosolver",
        "pio solver",
        "gto+",
        "simple postflop",
        "snowie",
        "monkersolver",
        "monker",
    ] {
        assert!(
            !meta.contains(taint),
            "_source/_doc must not name third-party source `{taint}` (clean-room provenance)"
        );
    }
    // Positive attestation present.
    assert!(
        meta.contains("clean-room"),
        "_source should attest the data is clean-room generated"
    );
    // Honesty: must explicitly disclaim game-theory-optimal (it is an
    // approximate equilibrium of a SIMPLIFIED model, not a true postflop GTO
    // solve).
    assert!(
        meta.contains("not game-theory-optimal"),
        "_source must explicitly disclaim GTO (CFR approx. equilibrium, NOT GTO)"
    );
    // Honesty: the method must be named PRECISELY — a genuine CFR/equilibrium
    // solve whose terminal EV is an equity-realization approximation. The
    // qualifier ("equity-realization") is MANDATORY and must never collapse to a
    // bare "GTO" claim (the founder's #1 rule).
    assert!(
        meta.contains("cfr") && meta.contains("equity-realization"),
        "_source must name the CFR approx.-equilibrium + equity-realization method"
    );
    assert!(
        meta.contains("not a true postflop gto solve"),
        "_source must state it is NOT a true postflop GTO solve"
    );
    // Honesty (F8, 2026-06-25): the DCFR label must NOT overclaim canonical DCFR.
    // The update form is a discounted-regret VARIANT using DCFR-style
    // coefficients (see preflop_cfr.rs DCFR_ALPHA/BETA/GAMMA + its honesty
    // docstring), not the canonical DCFR update. The `_source` must carry the
    // "variant" qualifier and the "DCFR-style coefficients" framing — never a
    // flat "Discounted-CFR (DCFR) solve".
    assert!(
        meta.contains("discounted-regret cfr variant") && meta.contains("dcfr-style coefficients"),
        "_source must label the method a discounted-regret CFR variant with \
         DCFR-style coefficients (not canonical DCFR)"
    );
    assert!(
        !meta.contains("discounted-cfr (dcfr) approximate-equilibrium solve"),
        "_source must NOT flatly claim a canonical \
         'Discounted-CFR (DCFR) ... solve' (it is a variant)"
    );
    // It must NEVER assert the data IS GTO / game-theory-optimal. (We only allow
    // the word in a NEGATED disclaimer like 'not ... game-theory-optimal' / 'not
    // a true postflop GTO solve'; a bare positive GTO claim is forbidden.)
    for bad in [
        "is game-theory-optimal",
        "true gto",
        "is gto",
        "gto-optimal",
    ] {
        assert!(
            !meta.contains(bad),
            "_source must NEVER positively claim GTO (found `{bad}`)"
        );
    }

    let r = &charts.ranges;
    let positions = ["UTG", "MP", "CO", "BTN", "SB", "BB"];

    // (b) Invariant: AA/KK/AKs in every position that opens (everyone but BB).
    for pos in ["UTG", "MP", "CO", "BTN", "SB"] {
        let rfi = r.get(pos).and_then(|p| p.get("RFI")).unwrap_or_else(|| {
            panic!("{pos}.RFI bucket missing");
        });
        for premium in ["AA", "KK", "AKs"] {
            assert!(rfi.contains_key(premium), "{premium} must be in {pos} RFI");
        }
    }
    // BB never opens unopened.
    assert!(
        r["BB"]["RFI"].is_empty(),
        "BB RFI must be empty (BB cannot open unopened)"
    );
    // UTG cannot face an open.
    assert!(
        r["UTG"]["facing_open"].is_empty(),
        "UTG facing_open must be empty (UTG cannot face an open)"
    );

    // Invariant: 72o (the worst hand) is never in ANY bucket.
    for pos in positions {
        for (bucket, hands) in &r[pos] {
            assert!(
                !hands.contains_key("72o"),
                "72o must never appear (found in {pos}.{bucket})"
            );
        }
    }

    // Invariant: facing a 4-bet tightens — vs_4bet ⊆ vs_3bet per position, AND
    // (F6, 2026-06-25) the per-key FREQUENCY also tightens: a hand cannot continue
    // MORE OFTEN facing a 4-bet than facing a 3-bet. KEY membership alone is not
    // enough — vs_4bet_freq ≤ vs_3bet_freq must hold for every shared key.
    for pos in positions {
        let v4 = &r[pos]["vs_4bet"];
        let v3 = &r[pos]["vs_3bet"];
        for (k, &f4) in v4 {
            let f3 = v3.get(k).copied().unwrap_or_else(|| {
                panic!(
                    "{pos}: vs_4bet hand {k} must also be in vs_3bet (continuing range only tightens)"
                )
            });
            assert!(
                f4 <= f3 + 1e-6,
                "{pos}: vs_4bet freq for {k} ({f4}) must be ≤ vs_3bet freq ({f3}) \
                 (continuing range only tightens — cannot continue more vs a 4-bet)"
            );
        }
    }

    // Invariant: RFI open widths are monotone non-decreasing UTG ⊆ MP ⊆ CO ⊆ BTN
    // (earlier position opens tighter). Use class COUNT as the width proxy and
    // also assert true set-containment direction on the count.
    let rfi_count = |pos: &str| r[pos]["RFI"].len();
    let (utg, mp, co, btn) = (
        rfi_count("UTG"),
        rfi_count("MP"),
        rfi_count("CO"),
        rfi_count("BTN"),
    );
    assert!(
        utg <= mp && mp <= co && co <= btn,
        "RFI widths must widen UTG<=MP<=CO<=BTN; got UTG={utg} MP={mp} CO={co} BTN={btn}"
    );
    assert!(utg < btn, "UTG RFI must be strictly tighter than BTN RFI");

    // Invariant: every continuing bucket that is non-empty contains AA (the nuts
    // never folds), and no frequency is outside [0, 1].
    for pos in positions {
        for (bucket, hands) in &r[pos] {
            if hands.is_empty() {
                continue;
            }
            for (k, &f) in hands {
                assert!(
                    (0.0..=1.0).contains(&f),
                    "{pos}.{bucket}.{k} frequency {f} out of [0,1]"
                );
            }
            assert!(
                hands.contains_key("AA"),
                "{pos}.{bucket} is non-empty but is missing AA (the nuts must always continue)"
            );
        }
    }
}
