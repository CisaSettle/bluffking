//! Milestone-B performance bench for the **server-blind per-hand deal** —
//! **Cross-vendor AI-audited (ADR-076/077/078); open-source + verifiable (ADR-063 §8 / ADR-062 §4 Gate-B)**.
//!
//! This is the Gate-B latency artifact: it times a FULL per-hand server-blind
//! deal end to end with the real `crypto_real` primitives (no mock, no DB), for
//! table sizes `n ∈ {2, 3, 4, 6}` players, and breaks the total down into its
//! four cost centres so the decision gate (ADR-062 §4) can see WHERE the time
//! goes (and which part dominates the prod-VPS / WASM / mobile budget).
//!
//! ## What "one hand" means here (mirrors `tests/phase4_server_blind.rs::rt3`)
//!
//! A complete server-blind deal for `n` all-human, bot-free parties:
//!
//!  1. **DKG** — `n`-of-`n` Feldman/Pedersen-committed DKG (`DkgRun::simulate`)
//!     + an independent `verify_dkg` (every Schnorr PoK + commitment open).
//!  2. **Encrypt 52** — build the starting ElGamal-ciphertext deck `D_0`
//!     (trivial encryption of card order 0..51 under the joint key `Q`).
//!  3. **Shuffle ×n WITH proof gen + verify** — round-robin: each of the `n`
//!     parties performs a fresh Fisher–Yates + re-encryption shuffle, PROVES it
//!     (the sound sigma re-encryption-shuffle argument), and the proof is
//!     VERIFIED before the next party shuffles. This is `O(n · N)` group ops on
//!     a 52-card deck and is the dominant cost.
//!  4. **Open 52 + Chaum–Pedersen verify** — n-of-n partial decryption of every
//!     dealt index: each party contributes `D_i = x_i·C1` + a DLEQ proof, and
//!     `verify_and_open` checks all `n` DLEQs + recovers each card id. (A real
//!     hand opens far fewer than 52 indices — `2n` hole + ≤5 board — so this is a
//!     deliberate WORST CASE; the per-card cost lets you scale down.)
//!  5. **Ed25519 transcript signing** — sign + verify a representative set of
//!     per-hand transcript events with the real asymmetric provider.
//!
//! The coordinator/server holds **no** secret share throughout (server-blind);
//! the bench simulates all `n` parties locally with the real OS CSPRNG.
//!
//! ## Status
//!
//! NOT a correctness gate (the TR-*/RT-* tests are). GA'd for the engine-blind
//! table class by ADR-070; cross-vendor AI-audited (ADR-076/077/078),
//! open-source + verifiable. This bench simulates all parties locally
//! (`guard_provider_allowed` keeps the generic `mental_poker_production`
//! provider rejected at startup). A latency measurement only.
//!
//! ## Run
//!
//! ```bash
//! cargo bench -p mental-poker --bench phase4_deal          # full criterion
//! cargo bench -p mental-poker --bench phase4_deal -- --quick   # fast pass
//! ```
//!
//! No database required.

use criterion::{criterion_group, criterion_main, BenchmarkId, Criterion};
use std::hint::black_box;

use mental_poker::crypto::ShuffleProofProvider;
use mental_poker::crypto_real::decrypt::{
    partial_decrypt, verify_and_open, ThresholdDecryptionProof, SCHEME,
};
use mental_poker::crypto_real::dkg::{verify_dkg, DkgRun};
use mental_poker::crypto_real::ec::{canonical_starting_deck, deck_hash, Ct, DECK_SIZE};
use mental_poker::crypto_real::ed25519_signer::Ed25519SignatureProvider;
use mental_poker::crypto_real::shuffle::{RealShuffleProofProvider, Shuffle};
use mental_poker::signing::SignatureProvider;

use curve25519_dalek::ristretto::RistrettoPoint;
use rand::rngs::OsRng;

/// Table sizes to bench (all-human, bot-free; ADR-063 §7). n=2 heads-up through
/// n=6 (a full short-handed ring).
const TABLE_SIZES: &[usize] = &[2, 3, 4, 6];

/// Representative count of per-hand transcript events to sign with Ed25519. A
/// real hand emits on the order of a few dozen signed events (DKG commit/reveal
/// per party, shuffle proof per party, per-card open shares, action log). We use
/// a fixed, n-scaled estimate so the signing cost is visible but not dominant.
fn signed_event_count(n: usize) -> usize {
    // ~ DKG (2 per party) + shuffle (1 per party) + opens (a few per party) +
    // a handful of protocol-state events. Order-of-magnitude, not exact.
    2 * n + n + 4 * n + 8
}

// ---------------------------------------------------------------------------
// Per-phase helpers (each returns its product so the optimizer can't elide it)
// ---------------------------------------------------------------------------

/// Phase 1 — DKG: simulate `n` parties + independently verify the whole DKG.
fn phase_dkg(n: usize, rng: &mut OsRng) -> DkgRun {
    let run = DkgRun::simulate(n, rng);
    // Independent verifier recomputes Q = Σ Q_i and checks every PoK/commitment.
    let q = verify_dkg(&run.commitments, &run.shares).expect("DKG verifies");
    debug_assert_eq!(q, run.joint_key);
    run
}

/// Phase 2 — encrypt the starting 52-card ciphertext deck `D_0`.
fn phase_encrypt(_q: &RistrettoPoint) -> Vec<Ct> {
    canonical_starting_deck()
}

/// Phase 3 — round-robin verifiable shuffle: each of `n` parties performs +
/// proves a shuffle, and every proof is verified before the next shuffles.
/// Returns the final shuffled deck.
fn phase_shuffle(n: usize, q: &RistrettoPoint, start: &[Ct], rng: &mut OsRng) -> Vec<Ct> {
    // F2: pin the verifier to the real joint key.
    let verifier = RealShuffleProofProvider::verifier_with_expected_key(
        mental_poker::crypto_real::ec::point_to_hex(q),
    );
    let mut deck = start.to_vec();
    for r in 0..n {
        let party = format!("party:{r}");
        let input = deck.clone();
        let shuffle = Shuffle::perform(input.clone(), q, rng);
        let ih = deck_hash(&input);
        let oh = deck_hash(&shuffle.output);
        let proof = shuffle.prove(&party, r as u32, rng);
        let ok = verifier.verify_shuffle(&party, r as u32, &ih, &oh, None, &proof);
        debug_assert!(ok, "honest shuffle proof must verify");
        deck = shuffle.output;
    }
    deck
}

/// Number of card indices a REAL hold'em hand actually opens: `2n` hole cards
/// (two per player) + 5 community (flop+turn+river). Far fewer than 52 — the
/// rest of the deck stays encrypted forever (and mucked hole cards are never
/// opened at all).
fn realistic_open_count(n: usize) -> usize {
    2 * n + 5
}

/// Phase 4 — open the first `count` dealt indices: n-of-n partial decryption +
/// Chaum–Pedersen verify + card recovery. Returns the count opened. With
/// `count = DECK_SIZE` this is the 52-card worst case; with
/// `count = realistic_open_count(n)` it is a real hand's actual open load.
fn phase_open_n(run: &DkgRun, deck: &[Ct], count: usize, rng: &mut OsRng) -> usize {
    let pks: Vec<(String, RistrettoPoint)> = run
        .parties
        .iter()
        .map(|p| (p.party_id.clone(), p.q_i))
        .collect();
    let mut opened = 0usize;
    for (idx, ct) in deck.iter().take(count).enumerate() {
        let proof = ThresholdDecryptionProof {
            scheme: SCHEME.to_string(),
            shares: run
                .parties
                .iter()
                .map(|p| partial_decrypt(p, idx as u32, ct, rng))
                .collect(),
        };
        let id = verify_and_open(idx as u32, ct, &pks, &proof).expect("opens");
        debug_assert!((id as usize) < DECK_SIZE);
        opened += 1;
    }
    opened
}

/// Open every dealt index (worst case, all 52).
fn phase_open(run: &DkgRun, deck: &[Ct], rng: &mut OsRng) -> usize {
    phase_open_n(run, deck, DECK_SIZE, rng)
}

/// Phase 5 — Ed25519 sign + verify a representative set of transcript events.
fn phase_sign(n: usize) -> usize {
    // Each of the n parties holds its own secret seed; build one provider that
    // can sign as all of them (the bench measures raw sign+verify throughput,
    // not the key-distribution choreography).
    let mut provider = Ed25519SignatureProvider::new();
    for i in 0..n {
        let seed = [i as u8 + 1; 32];
        provider = provider.with_signer(format!("party:{i}"), &seed);
    }
    let events = signed_event_count(n);
    let mut verified = 0usize;
    for ev in 0..events {
        let signer = format!("party:{}", ev % n);
        let msg = format!("mp:event:{ev}:hand-transcript-payload").into_bytes();
        let sig = provider.sign(&signer, &msg);
        if provider.verify(&signer, &msg, &sig) {
            verified += 1;
        }
    }
    verified
}

// ---------------------------------------------------------------------------
// Full per-hand deal (the headline Gate-B number)
// ---------------------------------------------------------------------------

/// One complete server-blind deal for `n` parties, opening `open_count` indices
/// (52 = worst case; `realistic_open_count(n)` = a real hand).
fn full_hand_open(n: usize, open_count: usize, rng: &mut OsRng) {
    let run = phase_dkg(n, rng);
    let q = run.joint_key;
    let start = phase_encrypt(&q);
    let deck = phase_shuffle(n, &q, &start, rng);
    let opened = phase_open_n(&run, &deck, open_count, rng);
    debug_assert_eq!(opened, open_count);
    let _ = phase_sign(n);
    black_box((opened, deck.len()));
}

/// Worst-case full hand: open all 52 indices.
fn full_hand(n: usize, rng: &mut OsRng) {
    full_hand_open(n, DECK_SIZE, rng);
}

// ---------------------------------------------------------------------------
// Criterion groups
// ---------------------------------------------------------------------------

/// The headline (WORST CASE): full per-hand deal opening all 52 indices, for
/// n = 2,3,4,6. A real hand opens far fewer (see `bench_full_hand_realistic`).
fn bench_full_hand(c: &mut Criterion) {
    let mut group = c.benchmark_group("phase4_full_hand_deal");
    // The full hand (52-card worst-case open) is heavy; keep sample counts low
    // enough that `cargo bench` finishes in a few minutes on the dev box.
    group.sample_size(10);
    for &n in TABLE_SIZES {
        group.bench_with_input(BenchmarkId::from_parameter(n), &n, |b, &n| {
            let mut rng = OsRng;
            b.iter(|| full_hand(black_box(n), &mut rng));
        });
    }
    group.finish();
}

/// The Gate-B headline (REALISTIC): full per-hand deal opening only `2n+5`
/// indices (the hole + board cards a real hold'em hand actually reveals), for
/// n = 2,3,4,6. This is the number that maps to live-play latency.
fn bench_full_hand_realistic(c: &mut Criterion) {
    let mut group = c.benchmark_group("phase4_full_hand_realistic");
    group.sample_size(10);
    for &n in TABLE_SIZES {
        group.bench_with_input(BenchmarkId::from_parameter(n), &n, |b, &n| {
            let mut rng = OsRng;
            let open_count = realistic_open_count(n);
            b.iter(|| full_hand_open(black_box(n), open_count, &mut rng));
        });
    }
    group.finish();
}

/// Per-phase breakdown for each table size (so the gate sees the split).
fn bench_phases(c: &mut Criterion) {
    // --- DKG ---
    {
        let mut group = c.benchmark_group("phase4_dkg");
        group.sample_size(20);
        for &n in TABLE_SIZES {
            group.bench_with_input(BenchmarkId::from_parameter(n), &n, |b, &n| {
                let mut rng = OsRng;
                b.iter(|| black_box(phase_dkg(black_box(n), &mut rng)));
            });
        }
        group.finish();
    }

    // --- encrypt 52 (n-independent; bench once at n=2) ---
    {
        let mut group = c.benchmark_group("phase4_encrypt52");
        group.sample_size(50);
        let q = DkgRun::simulate(2, &mut OsRng).joint_key;
        group.bench_function("build_D0", |b| {
            b.iter(|| black_box(phase_encrypt(black_box(&q))));
        });
        group.finish();
    }

    // --- round-robin shuffle (perform + prove + verify), ×n rounds ---
    {
        let mut group = c.benchmark_group("phase4_shuffle_roundrobin");
        group.sample_size(10);
        for &n in TABLE_SIZES {
            group.bench_with_input(BenchmarkId::from_parameter(n), &n, |b, &n| {
                let mut rng = OsRng;
                let run = DkgRun::simulate(n, &mut rng);
                let q = run.joint_key;
                let start = canonical_starting_deck();
                b.iter(|| black_box(phase_shuffle(black_box(n), &q, &start, &mut rng)));
            });
        }
        group.finish();
    }

    // --- single shuffle (perform + prove + verify) — the per-round unit cost ---
    {
        let mut group = c.benchmark_group("phase4_shuffle_single_round");
        group.sample_size(20);
        let mut rng = OsRng;
        let run = DkgRun::simulate(2, &mut rng);
        let q = run.joint_key;
        let start = canonical_starting_deck();
        let verifier = RealShuffleProofProvider::verifier_with_expected_key(
            mental_poker::crypto_real::ec::point_to_hex(&q),
        );
        group.bench_function("perform_prove_verify", |b| {
            b.iter(|| {
                let shuffle = Shuffle::perform(start.clone(), &q, &mut rng);
                let ih = deck_hash(&start);
                let oh = deck_hash(&shuffle.output);
                let proof = shuffle.prove("party:0", 0, &mut rng);
                let ok = verifier.verify_shuffle("party:0", 0, &ih, &oh, None, &proof);
                black_box(ok)
            });
        });
        group.finish();
    }

    // --- open 52 (partial decrypt + CP verify), n-of-n ---
    {
        let mut group = c.benchmark_group("phase4_open52");
        group.sample_size(10);
        for &n in TABLE_SIZES {
            group.bench_with_input(BenchmarkId::from_parameter(n), &n, |b, &n| {
                let mut rng = OsRng;
                let run = DkgRun::simulate(n, &mut rng);
                let deck = canonical_starting_deck();
                b.iter(|| black_box(phase_open(&run, &deck, &mut rng)));
            });
        }
        group.finish();
    }

    // --- open a SINGLE card (partial decrypt + CP verify), n-of-n — per-card cost ---
    {
        let mut group = c.benchmark_group("phase4_open_single_card");
        group.sample_size(30);
        for &n in TABLE_SIZES {
            group.bench_with_input(BenchmarkId::from_parameter(n), &n, |b, &n| {
                let mut rng = OsRng;
                let run = DkgRun::simulate(n, &mut rng);
                let pks: Vec<(String, RistrettoPoint)> = run
                    .parties
                    .iter()
                    .map(|p| (p.party_id.clone(), p.q_i))
                    .collect();
                let ct = Ct::encrypt_card(
                    7,
                    &run.joint_key,
                    &curve25519_dalek::scalar::Scalar::random(&mut rng),
                );
                b.iter(|| {
                    let proof = ThresholdDecryptionProof {
                        scheme: SCHEME.to_string(),
                        shares: run
                            .parties
                            .iter()
                            .map(|p| partial_decrypt(p, 0, &ct, &mut rng))
                            .collect(),
                    };
                    black_box(verify_and_open(0, &ct, &pks, &proof).expect("opens"))
                });
            });
        }
        group.finish();
    }

    // --- Ed25519 transcript signing (sign + verify the per-hand event set) ---
    {
        let mut group = c.benchmark_group("phase4_ed25519_signing");
        group.sample_size(30);
        for &n in TABLE_SIZES {
            group.bench_with_input(BenchmarkId::from_parameter(n), &n, |b, &n| {
                b.iter(|| black_box(phase_sign(black_box(n))));
            });
        }
        group.finish();
    }
}

criterion_group!(
    benches,
    bench_full_hand,
    bench_full_hand_realistic,
    bench_phases
);
criterion_main!(benches);
