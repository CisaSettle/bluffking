//! BUG-108 (fail-closed) PROCESS test for the `mp-verify` CLI exit codes.
//!
//! `report()` (mp_verify.rs) maps a `verify()` Ok to exit 0 ONLY for a
//! cryptographically [`Sound`] transcript; a dev-only MOCK transcript replays
//! consistently but is NOT provably fair, so it exits 3 (a distinct non-zero
//! code) — a consumer that gates on exit 0 can therefore never treat a mock
//! replay as a passed fairness check. The pure-Rust `verify_fairness` unit tests
//! cover the in-process verdict; THIS test pins the machine-readable PROCESS
//! exit code automation/exports actually gate on, so a regression to exit 0 on a
//! mock replay is caught.

use std::path::PathBuf;
use std::process::Command;

/// Absolute path to the compiled `mp-verify` binary (cargo sets this for the
/// integration-test harness).
fn mp_verify_bin() -> &'static str {
    env!("CARGO_BIN_EXE_mp-verify")
}

/// Absolute path to a committed, frozen, cryptographically SOUND real-crypto
/// transcript fixture (real re-encryption shuffle + real threshold decryption +
/// real Ed25519 signing → `SchemeSoundness::Sound`). Generated once from
/// `build_sound_reenc_transcript` (phase4_server_blind.rs) and frozen as a
/// conformance vector alongside `mp_conformance.json` — verification is
/// deterministic over the bytes, so a sound transcript stays sound.
fn sound_transcript_fixture() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/vectors/mp_sound_reenc.json")
}

fn code_of(mut cmd: Command) -> i32 {
    let status = cmd.status().expect("mp-verify binary runs");
    status
        .code()
        .expect("mp-verify exits with a code (not a signal)")
}

#[test]
fn mp_verify_mock_transcript_exits_three() {
    // `--demo` deals a hand with the production MOCK provider
    // (`MentalPokerDealingProvider::deterministic`) and verifies it: a consistent
    // mock replay → exit 3, NEVER 0.
    let mut cmd = Command::new(mp_verify_bin());
    cmd.arg("--demo");
    let code = code_of(cmd);
    assert_eq!(
        code, 3,
        "a mock-crypto replay must exit 3 (NOT provably fair), got {code}"
    );
    assert_ne!(code, 0, "a mock replay must never exit 0 (verified-fair)");
}

#[test]
fn mp_verify_mock_transcript_file_exits_three() {
    // Same fail-closed verdict via the file path: write the mock transcript with
    // `--demo <out>` (also exits 3), then verify the FILE → exit 3.
    let out = unique_temp_path("mp-verify-mock-transcript", "json");

    let mut write_cmd = Command::new(mp_verify_bin());
    write_cmd.arg("--demo").arg(&out);
    let write_code = code_of(write_cmd);
    assert_eq!(
        write_code, 3,
        "`--demo <out>` also reports the mock as exit 3"
    );
    assert!(out.exists(), "the transcript file was written");

    let mut verify_cmd = Command::new(mp_verify_bin());
    verify_cmd.arg(&out);
    let verify_code = code_of(verify_cmd);

    let _ = std::fs::remove_file(&out);
    assert_eq!(
        verify_code, 3,
        "verifying a written mock transcript FILE must exit 3, got {verify_code}"
    );
}

#[test]
fn mp_verify_real_crypto_transcript_exits_zero() {
    // The SUCCESS-path PROCESS counterpart to the exit-3 mock tests: a fully
    // real-crypto SOUND transcript (`SchemeSoundness::Sound`) must drive the
    // `report()` Sound branch → exit 0 (mp_verify.rs). The in-process verdict is
    // covered by `verify_fairness_accepts_real_crypto_transcript`
    // (phase4_server_blind.rs); THIS pins the machine-readable exit code an
    // automation/export consumer gates "verified-fair" on, so a regression that
    // stopped exiting 0 on a sound transcript (a fairness FALSE NEGATIVE) is
    // caught at the process boundary.
    let fixture = sound_transcript_fixture();
    assert!(
        fixture.exists(),
        "sound transcript fixture must be committed at {}",
        fixture.display()
    );
    let mut cmd = Command::new(mp_verify_bin());
    cmd.arg(&fixture);
    let code = code_of(cmd);
    assert_eq!(
        code, 0,
        "a cryptographically sound real-crypto transcript must exit 0 \
         (verified + provably fair), got {code}"
    );
}

#[test]
fn mp_verify_usage_and_bad_path_are_distinct_non_fairness_codes() {
    // Sanity that exit 3 is specific to "replays but not provably fair", not a
    // catch-all: no args → usage (2); a missing file → IO error (2).
    assert_eq!(
        code_of(Command::new(mp_verify_bin())),
        2,
        "no args → usage 2"
    );

    let mut missing = Command::new(mp_verify_bin());
    missing.arg("/no/such/transcript/path-bug108.json");
    assert_eq!(code_of(missing), 2, "missing file → IO error exit 2");
}

fn unique_temp_path(stem: &str, ext: &str) -> PathBuf {
    use std::time::{SystemTime, UNIX_EPOCH};
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    let pid = std::process::id();
    std::env::temp_dir().join(format!("{stem}-{pid}-{nanos}.{ext}"))
}
