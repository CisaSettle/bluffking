//! Verifiable re-encryption shuffle — **cross-vendor AI-audited (ADR-076/077/078);
//! open-source + verifiable (ADR-063 §3, spec §3 / §3.6)**.
//!
//! This is the single highest-risk primitive of the server-blind increment and
//! the one with **no audited off-the-shelf ristretto crate** (ADR-063 §2). It
//! implements [`ShuffleProofProvider`](crate::crypto::ShuffleProofProvider) with
//! a **sound** sigma-based re-encryption-shuffle argument over ristretto255,
//! made non-interactive with a `merlin` Fiat–Shamir transcript. It ships behind
//! the spec §3.6 interim discriminator [`SCHEME`] = `"reenc-shuffle-ristretto-v1"`
//! (NOT the full Bayer–Groth `"bg-shuffle-ristretto-v1"` — see "What this proves"
//! and "Honest caveats").
//!
//! ## What it proves (the soundness contract)
//!
//! Given an input ElGamal-ciphertext deck `D_in` and an output deck `D_out` under
//! the joint key `Q`, the proof convinces a verifier that **there exists a
//! permutation `π` of `0..N` and re-encryption randomness `{ρ_k}` such that**
//!
//! ```text
//!     D_out[k] = reencrypt( D_in[π(k)], ρ_k )      for every k
//! ```
//!
//! i.e. `D_out` is exactly `D_in` permuted and re-encrypted — **no card is
//! swapped, replaced, dropped, or duplicated** (threat T5). A malicious shuffler
//! that mutates any output ciphertext, or applies a non-permutation map, makes
//! `verify_shuffle` return `false`. This is the exact attack the
//! `MockShuffleProofProvider` fails to catch.
//!
//! ## Soundness mechanism (why a card swap is caught)
//!
//! ElGamal is additively homomorphic. For any public weight vector
//! `e = (e_0,…,e_{N-1})`, if `D_out[k] = reencrypt(D_in[π(k)], ρ_k)` then
//!
//! ```text
//!   Σ_k e_k·D_out[k]  =  Σ_k e_k·D_in[π(k)]  +  ( R·G , R·Q )       (R = Σ_k e_k·ρ_k)
//!                     =  Σ_j f_j·D_in[j]      +  ( R·G , R·Q )       (f_j = e_{π⁻¹(j)})
//! ```
//!
//! so `Σ_k e_k·D_out[k] − Σ_j f_j·D_in[j]` is a **re-encryption of the identity**
//! `(R·G, R·Q)` — its message component is the identity point. The argument has
//! two Fiat–Shamir rounds:
//!
//! - **Part A (re-encryption equality).** The prover Pedersen-commits to the
//!   permuted weights `f = (f_0,…,f_{N-1})` (so `π` stays hidden) and proves, in
//!   committed form, that `Σ_k e_k·D_out[k] − Σ_j f_j·D_in[j] = (R·G, R·Q)` for a
//!   known `R` — a Chaum–Pedersen proof of knowledge of `R` over the *two* bases
//!   `G` and `Q`. If a card was replaced, this difference is **not** a clean
//!   re-encryption of zero for a random `e`, so the proof of `(R·G, R·Q)` fails.
//! - **Part B (permutation).** The committed weights `f` must be a genuine
//!   permutation of the public `e`. By Schwartz–Zippel / Neff: at a second random
//!   challenge `x`, prove `∏_j (x − f_j) = ∏_k (x − e_k)`. Two multisets are equal
//!   iff their characteristic polynomials match, which (for random `x` over a
//!   ≈2²⁵² field) holds iff `{f_j}` is a permutation of `{e_k}`. This blocks a
//!   "duplicate/drop a card" attack (threat T5 / TR-2): a drop or duplicate makes
//!   `f` not a permutation of `e`, so Part B fails.
//!
//! A re-encryption-**only** (identity permutation) shuffle and a permute-**only**
//! (zero re-encryption randomness) shuffle are both genuine special cases and
//! BOTH still verify (the brief's two positive cases).
//!
//! ## Fiat–Shamir binding (threat T6 — non-transferability)
//!
//! The challenge weights `e`, the challenge `x`, and every sigma challenge are
//! squeezed from a `merlin` transcript that first absorbs
//! `(party, round, Q, deck_hash(D_in), deck_hash(D_out))` and every ciphertext.
//! Lifting a valid proof onto a different round / party / input deck changes the
//! squeezed challenges, so the proof fails (TR-3).
//!
//! ## Honest caveats (read before trusting this)
//!
//! - **Interim, not Bayer–Groth.** This is the sound sigma-based interim the spec
//!   §3.6 explicitly permits, not the succinct Bayer–Groth argument. Proof size is
//!   `O(N)` (it carries the `f` commitments) rather than `O(√N)`; that is a perf
//!   trade-off the Milestone-B bench measures, not a soundness gap.
//! - **Zero-knowledge of the permutation** is argued informally here (the `f`
//!   commitments and `R`/`x`-polynomial blinders hide `π`); a rigorous ZK proof
//!   and a constant-time review are **follow-up hardening items**, not increment-1
//!   claims. The *soundness* property (a swap is rejected) is what the TR-1/2/3
//!   tests gate and is the non-negotiable for this increment.
//! - **Cross-vendor AI-audited (ADR-076/077/078); open-source + verifiable.** GA'd
//!   for the engine-blind table class by ADR-070 (which lifted the ADR-063 cage); in
//!   production reachable ONLY for engine-blind sessions via
//!   `resolve_mp_crypto_mode`. `guard_provider_allowed` still keeps the generic
//!   `mental_poker_production` provider rejected at startup.

use crate::crypto::{ShuffleProof, ShuffleProofProvider};
use crate::crypto_real::dkg::challenge_scalar;
use crate::crypto_real::ec::{
    deck_hash, is_identity_pubkey, point_from_hex, point_to_hex, scalar_from_hex, scalar_to_hex,
    Ct, CtWire,
};
use crate::hash::{hex_hash, Hash};
use curve25519_dalek::constants::RISTRETTO_BASEPOINT_POINT as G;
use curve25519_dalek::ristretto::RistrettoPoint;
use curve25519_dalek::scalar::Scalar;
use curve25519_dalek::traits::{Identity, MultiscalarMul};
use merlin::Transcript;
use rand_core::{CryptoRng, RngCore};
use serde::{Deserialize, Serialize};

use super::ec::pedersen_h;

/// Scheme identifier for the (sound, sigma-based interim) re-encryption shuffle
/// (spec §3.6). The full Bayer–Groth `"bg-shuffle-ristretto-v1"` is a later
/// increment; this discriminator keeps the verifier dispatch unambiguous.
pub const SCHEME: &str = "reenc-shuffle-ristretto-v1";

// ===========================================================================
// Wire form of the argument (carried in `ShuffleProof.attestation` as JSON-hex)
// ===========================================================================

/// The full re-encryption-shuffle argument, serialized into
/// [`ShuffleProof::attestation`].
///
/// The trait only passes deck *hashes* (`crypto.rs:71`), so the actual input and
/// output ciphertext decks travel inside the attestation; the verifier
/// recomputes `deck_hash` of each and checks it equals the bound `input_hash` /
/// `output_hash` (otherwise the proof is about a different deck → reject).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ShuffleArgument {
    /// The input ciphertext deck the proof consumes.
    pub input_deck: Vec<CtWire>,
    /// The output ciphertext deck the proof produces.
    pub output_deck: Vec<CtWire>,
    /// The joint public key `Q` (so the verifier can recompute `e_k·Q` etc.).
    pub joint_key: String,
    /// Per-element Pedersen commitments to the permuted weights `f_j`:
    /// `Cf_j = f_j·G + b_j·H`.
    pub f_commitments: Vec<String>,
    /// Part A — Chaum–Pedersen proof that the homomorphic difference is a clean
    /// re-encryption of the identity: `Δ = (R·G, R·Q)` with knowledge of `R`,
    /// AND the committed `f` reproduces `Σ f_j·D_in[j]`.
    pub reenc: ReencProof,
    /// Part B — permutation argument: `{f_j}` is a permutation of `{e_k}`.
    pub perm: PermProof,
}

/// Part A: proves the homomorphic difference is a re-encryption of zero and the
/// committed weights `f` were used honestly.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ReencProof {
    /// `T1 = w·G` (commitment for the `R`-knowledge sigma over base `G`).
    pub t1: String,
    /// `T2 = w·Q` (commitment for the `R`-knowledge sigma over base `Q`).
    pub t2: String,
    /// `Tf = (Σ_j v_j·D_in1[j]) + (Σ_j v_j·D_in2[j])·0 …` — see code: the
    /// commitment binding the `f`-blinders to the input-deck linear combination.
    /// Encoded as the pair `(Tf1, Tf2)` over the two ciphertext coordinates.
    pub tf1: String,
    /// Second coordinate of the `f`-linear-combination commitment.
    pub tf2: String,
    /// `Tb_j = v_j·G + u_j·H` — **per-element** commitments binding each `f_j`
    /// (via the SAME `v_j` used in `Tf`) to its individual `Cf_j` opening, so the
    /// homomorphic sum (`Tf`) and the committed `f` (Part B) use index-matched
    /// `f_j` — not merely an equal aggregate (closes the redistribution gap).
    pub tb: Vec<String>,
    /// Response `z_R = w + c·R`.
    pub z_r: String,
    /// Responses `z_f_j = v_j + c·f_j` (one per element).
    pub z_f: Vec<String>,
    /// Responses `z_b_j = u_j + c·b_j` (one per element; blinders of `Cf_j`).
    pub z_b: Vec<String>,
}

/// Part B: proves `{f_j}` is a permutation of the public weights `{e_k}` via the
/// Neff product / Schwartz–Zippel argument `∏_j (x − f_j) == ∏_k (x − e_k)`.
///
/// `e` is public, so the RHS `target = ∏_k (x − e_k)` is computable by the
/// verifier. The committed `f` is multiplied up through a chain of Pedersen
/// commitments to the running products `P_t = ∏_{j≤t}(x − f_j)`, each step proved
/// with a **sound Schnorr multiplication argument** (the
/// `z_x·C_y + z_rz·H == B' + c·C_z` form — see `verify_mul_step`). The final
/// running product is bound to the public `target` by revealing its blind
/// (`final_blind`) and checking `Cp_{N-1} == target·G + final_blind·H` — sound
/// and leak-free because `target` is already public.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PermProof {
    /// Commitments to the running products `P_t = ∏_{j≤t} (x − f_j)`:
    /// `Cp_t = P_t·G + s_t·H` (`t = 0..N-1`).
    pub p_commitments: Vec<String>,
    /// One sound multiplication step per `t = 0..N-1`, proving
    /// `P_t = P_{t-1}·(x − f_t)` (with `P_{-1} := 1`).
    pub steps: Vec<MulStep>,
    /// The blind `s_{N-1}` of the final product commitment, revealed so the
    /// verifier can check `Cp_{N-1} == target·G + s_{N-1}·H` (target is public).
    pub final_blind: String,
}

/// A sound Schnorr multiplication step proving `z = x·y` for committed
/// `C_x = x·G + r_x·H`, `C_y = y·G + r_y·H`, `C_z = z·G + r_z·H`.
///
/// Here for step `t`: `x` = previous running product `P_{t-1}` (commitment
/// `C_x = Cp_{t-1}`, or `1·G` for `t = 0`), `y` = the factor `(x_chal − f_t)`
/// (commitment `C_y = Cfac_t = x_chal·G − Cf_t`, reconstructed by the verifier),
/// `z` = the new running product `P_t` (commitment `C_z = Cp_t`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct MulStep {
    /// `B = b·G + r_b·H` (commitment for the `x`-knowledge sigma).
    pub b_pt: String,
    /// `B' = b·C_y + r_zb·H` (commitment for the multiplication relation).
    pub b_prime: String,
    /// `z_x = b + c·x`.
    pub z_x: String,
    /// `z_rx = r_b + c·r_x`.
    pub z_rx: String,
    /// `z_rz = r_zb + c·(r_z − x·r_y)`.
    pub z_rz: String,
}

// ===========================================================================
// Fiat–Shamir transcript helpers (the challenge binding — threat T6)
// ===========================================================================

/// Absorb the public statement into a fresh merlin transcript: `(party, round,
/// Q, deck_hash(D_in), deck_hash(D_out))` + every ciphertext of both decks. This
/// binds the proof to exactly this shuffle (T6); a re-context'd proof squeezes
/// different challenges and fails.
fn statement_transcript(
    party: &str,
    round: u32,
    q: &RistrettoPoint,
    d_in: &[Ct],
    d_out: &[Ct],
) -> Transcript {
    let mut t = Transcript::new(b"mp:shuffle:reenc:v1");
    t.append_message(b"party", party.as_bytes());
    t.append_u64(b"round", round as u64);
    t.append_message(b"Q", q.compress().as_bytes());
    t.append_message(b"in_hash", &deck_hash(d_in));
    t.append_message(b"out_hash", &deck_hash(d_out));
    t.append_u64(b"n", d_in.len() as u64);
    for ct in d_in {
        t.append_message(b"in_c1", ct.c1.compress().as_bytes());
        t.append_message(b"in_c2", ct.c2.compress().as_bytes());
    }
    for ct in d_out {
        t.append_message(b"out_c1", ct.c1.compress().as_bytes());
        t.append_message(b"out_c2", ct.c2.compress().as_bytes());
    }
    t
}

/// Squeeze the public per-element challenge weight vector `e = (e_0..e_{N-1})`.
fn challenge_weights(t: &mut Transcript, n: usize) -> Vec<Scalar> {
    (0..n)
        .map(|i| {
            t.append_u64(b"e_index", i as u64);
            challenge_scalar(t, b"e")
        })
        .collect()
}

// ===========================================================================
// Prover
// ===========================================================================

/// A re-encryption shuffle a party performed: the input deck, the output deck,
/// the permutation `π` (`output[k] = reencrypt(input[π[k]], ρ[k])`), the
/// re-encryption randomness, and the joint key. Held only by the prover.
#[derive(Debug, Clone)]
pub struct Shuffle {
    /// Input ciphertext deck.
    pub input: Vec<Ct>,
    /// Output ciphertext deck.
    pub output: Vec<Ct>,
    /// `pi[k]` = the input index that became output index `k`.
    pub pi: Vec<usize>,
    /// `rho[k]` = the fresh re-encryption randomness applied at output index `k`.
    pub rho: Vec<Scalar>,
    /// Joint public key `Q`.
    pub joint_key: RistrettoPoint,
}

impl Shuffle {
    /// Perform a fresh Fisher–Yates permutation + fresh re-encryption of `input`
    /// under `q`, producing the output deck and the secret witness.
    pub fn perform<R: RngCore + CryptoRng>(
        input: Vec<Ct>,
        q: &RistrettoPoint,
        rng: &mut R,
    ) -> Self {
        let n = input.len();
        // Fisher–Yates permutation over OsRng (the caller passes OsRng).
        let mut pi: Vec<usize> = (0..n).collect();
        for i in (1..n).rev() {
            // Uniform j in 0..=i via rejection sampling on a fresh scalar's low bytes.
            let j = uniform_below(rng, i + 1);
            pi.swap(i, j);
        }
        let rho: Vec<Scalar> = (0..n).map(|_| Scalar::random(rng)).collect();
        let output: Vec<Ct> = (0..n).map(|k| input[pi[k]].reencrypt(q, &rho[k])).collect();
        Shuffle {
            input,
            output,
            pi,
            rho,
            joint_key: *q,
        }
    }

    /// Build the non-interactive proof.
    pub fn prove<R: RngCore + CryptoRng>(
        &self,
        party: &str,
        round: u32,
        rng: &mut R,
    ) -> ShuffleProof {
        let n = self.input.len();
        let h = *pedersen_h();
        let q = self.joint_key;

        let mut t = statement_transcript(party, round, &q, &self.input, &self.output);
        let e = challenge_weights(&mut t, n);

        // f_j = e_{π⁻¹(j)} : the challenge weight that lands on input index j.
        // π[k] = input index for output k ⇒ for input index j = π[k], f_j = e_k.
        let mut f = vec![Scalar::ZERO; n];
        for (k, &j) in self.pi.iter().enumerate() {
            f[j] = e[k];
        }

        // Pedersen-commit to f: Cf_j = f_j·G + b_j·H.
        let b: Vec<Scalar> = (0..n).map(|_| Scalar::random(rng)).collect();
        let cf: Vec<RistrettoPoint> = (0..n).map(|j| f[j] * G + b[j] * h).collect();

        // R = Σ_k e_k · ρ_k  (the aggregate re-encryption randomness).
        let r_agg: Scalar = (0..n).map(|k| e[k] * self.rho[k]).sum();

        // Absorb the f commitments before deriving the Part-A challenge.
        for cf_j in &cf {
            t.append_message(b"Cf", cf_j.compress().as_bytes());
        }

        // ---- Part A: prove Δ = Σ e_k·D_out[k] − Σ f_j·D_in[j] = (R·G, R·Q). ----
        // Sigma over R (two bases G, Q), AND over the f_j / b_j used to form
        // Σ f_j·D_in[j] and Σ b_j·H. One joint challenge c_a.
        let w = Scalar::random(rng); // blinder for R
        let v: Vec<Scalar> = (0..n).map(|_| Scalar::random(rng)).collect(); // blinders for f_j
        let u: Vec<Scalar> = (0..n).map(|_| Scalar::random(rng)).collect(); // blinders for b_j

        let t1 = w * G;
        let t2 = w * q;
        // Tf = Σ_j v_j·D_in[j]  (a ciphertext: bind blinders to the input deck).
        let tf1: RistrettoPoint = (0..n).map(|j| v[j] * self.input[j].c1).sum();
        let tf2: RistrettoPoint = (0..n).map(|j| v[j] * self.input[j].c2).sum();
        // Tb_j = v_j·G + u_j·H — PER-ELEMENT, binding the same v_j to each f
        // commitment's opening (so Part A's f_j and the committed Cf_j are
        // index-matched, not just equal in aggregate).
        let tb_pts: Vec<RistrettoPoint> = (0..n).map(|j| v[j] * G + u[j] * h).collect();

        t.append_message(b"A_T1", t1.compress().as_bytes());
        t.append_message(b"A_T2", t2.compress().as_bytes());
        t.append_message(b"A_Tf1", tf1.compress().as_bytes());
        t.append_message(b"A_Tf2", tf2.compress().as_bytes());
        for tb_j in &tb_pts {
            t.append_message(b"A_Tb", tb_j.compress().as_bytes());
        }
        let c_a = challenge_scalar(&mut t, b"c_a");

        let z_r = w + c_a * r_agg;
        let z_f: Vec<Scalar> = (0..n).map(|j| v[j] + c_a * f[j]).collect();
        let z_b: Vec<Scalar> = (0..n).map(|j| u[j] + c_a * b[j]).collect();

        let reenc = ReencProof {
            t1: point_to_hex(&t1),
            t2: point_to_hex(&t2),
            tf1: point_to_hex(&tf1),
            tf2: point_to_hex(&tf2),
            tb: tb_pts.iter().map(point_to_hex).collect(),
            z_r: scalar_to_hex(&z_r),
            z_f: z_f.iter().map(scalar_to_hex).collect(),
            z_b: z_b.iter().map(scalar_to_hex).collect(),
        };

        // ---- Part B: permutation argument ∏(x − f_j) == ∏(x − e_k). ----
        let x = {
            // squeeze the polynomial challenge AFTER Part A is bound.
            challenge_scalar(&mut t, b"x")
        };
        let perm = prove_permutation(&mut t, &f, &b, &x, &h, rng);

        ShuffleProof {
            scheme: SCHEME.to_string(),
            input_deck_hash: hex_hash(&deck_hash(&self.input)),
            output_deck_hash: hex_hash(&deck_hash(&self.output)),
            attestation: serialize_argument(&ShuffleArgument {
                input_deck: self.input.iter().map(|c| c.to_wire()).collect(),
                output_deck: self.output.iter().map(|c| c.to_wire()).collect(),
                joint_key: point_to_hex(&q),
                f_commitments: cf.iter().map(point_to_hex).collect(),
                reenc,
                perm,
            }),
        }
    }
}

/// Prove `∏_j (x − f_j) == ∏_k (x − e_k)` in committed form (the Neff /
/// Schwartz–Zippel permutation argument).
///
/// `e` is public, so `target = ∏_k (x − e_k)` is computable by the verifier. The
/// prover multiplies the committed factors `(x − f_j)` up through a chain of
/// running-product commitments `Cp_t = P_t·G + s_t·H`, proving each step
/// `P_t = P_{t-1}·(x − f_t)` with a sound multiplication argument, then reveals
/// the final blind `s_{N-1}` so the verifier checks `Cp_{N-1} == target·G +
/// s_{N-1}·H`. If `{f_j}` is not a permutation of `{e_k}`, the products differ at
/// the random `x` (S–Z), so the final commitment cannot open to `target`.
///
/// The factor commitment is `Cfac_j = x·G − Cf_j = (x − f_j)·G + (−b_j)·H` —
/// reconstructed by the verifier from public `x` and the committed `Cf_j`, so no
/// extra factor commitment is transmitted.
fn prove_permutation<R: RngCore + CryptoRng>(
    t: &mut Transcript,
    f: &[Scalar],
    b: &[Scalar],
    x: &Scalar,
    h: &RistrettoPoint,
    rng: &mut R,
) -> PermProof {
    let n = f.len();
    // Factor value (x − f_j) and its Pedersen blind (−b_j).
    let factor: Vec<Scalar> = (0..n).map(|j| x - f[j]).collect();
    let factor_blind: Vec<Scalar> = (0..n).map(|j| -b[j]).collect();

    // Running products P_t = ∏_{j≤t}(x − f_j) and fresh commitment blinds s_t.
    let mut prod = Vec::with_capacity(n);
    let mut prod_blind = Vec::with_capacity(n);
    let mut acc = Scalar::ONE;
    for fac in &factor {
        acc *= fac;
        prod.push(acc);
        prod_blind.push(Scalar::random(rng));
    }
    let cp: Vec<RistrettoPoint> = (0..n).map(|i| prod[i] * G + prod_blind[i] * h).collect();
    for cp_t in &cp {
        t.append_message(b"Cp", cp_t.compress().as_bytes());
    }

    let mut steps = Vec::with_capacity(n);
    for tdx in 0..n {
        // x_val = P_{t-1} (with P_{-1} := 1, blind 0), y_val = factor_t,
        // z_val = P_t. C_y = Cfac_t = x·G − Cf_t (reconstructed by verifier).
        let (x_val, r_x) = if tdx == 0 {
            (Scalar::ONE, Scalar::ZERO)
        } else {
            (prod[tdx - 1], prod_blind[tdx - 1])
        };
        let r_y = factor_blind[tdx];
        let r_z = prod_blind[tdx];
        let c_y = x * G - (f[tdx] * G + b[tdx] * h); // = Cfac_t

        // Sound multiplication proof for z = x·y (Schnorr form).
        let bb = Scalar::random(rng);
        let r_b = Scalar::random(rng);
        let r_zb = Scalar::random(rng);
        let b_pt = bb * G + r_b * h; // B
        let b_prime = bb * c_y + r_zb * h; // B'
        t.append_message(b"M_B", b_pt.compress().as_bytes());
        t.append_message(b"M_Bp", b_prime.compress().as_bytes());
        let c = challenge_scalar(t, b"c_mul");

        let z_x = bb + c * x_val;
        let z_rx = r_b + c * r_x;
        let z_rz = r_zb + c * (r_z - x_val * r_y);

        steps.push(MulStep {
            b_pt: point_to_hex(&b_pt),
            b_prime: point_to_hex(&b_prime),
            z_x: scalar_to_hex(&z_x),
            z_rx: scalar_to_hex(&z_rx),
            z_rz: scalar_to_hex(&z_rz),
        });
    }

    PermProof {
        p_commitments: cp.iter().map(point_to_hex).collect(),
        steps,
        final_blind: scalar_to_hex(&prod_blind[n - 1]),
    }
}

// ===========================================================================
// Verifier
// ===========================================================================

/// Verify a re-encryption shuffle argument. Returns `false` (never panics) on any
/// malformed field, deck-hash mismatch, or failed sigma/permutation check.
fn verify_argument(
    party: &str,
    round: u32,
    input_hash: &Hash,
    output_hash: &Hash,
    expected_joint_key: Option<&str>,
    proof: &ShuffleProof,
) -> bool {
    if proof.scheme != SCHEME {
        return false;
    }
    let arg: ShuffleArgument = match deserialize_argument(&proof.attestation) {
        Some(a) => a,
        None => return false,
    };
    // Decode the decks + joint key (T8/T9 clean rejects).
    let d_in = match decode_deck(&arg.input_deck) {
        Some(d) => d,
        None => return false,
    };
    let d_out = match decode_deck(&arg.output_deck) {
        Some(d) => d,
        None => return false,
    };
    let q = match point_from_hex(&arg.joint_key) {
        Some(p) => p,
        None => return false,
    };
    // F-CRYPTO-15 (code-review #8): reject an IDENTITY joint key unconditionally —
    // including when no `expected_joint_key` is pinned (with Q = identity the
    // re-encryption term r'·Q is identity, so C2 = M and the argument still
    // verifies for the degenerate key). A floor defense even if a caller forgets
    // to pin the DKG key. See `ec::is_identity_pubkey` for the full rationale.
    if is_identity_pubkey(&q) {
        return false;
    }
    // F2 (KEY BINDING — soundness): the shuffle re-encrypts under the joint key
    // `Q`, so its soundness is *relative to whatever Q the proof was built with*.
    // Trusting `arg.joint_key` blindly lets a malicious shuffler prove a perfectly
    // valid shuffle under a key IT controls — re-encrypting the deck under its own
    // key so it can later decrypt every card. The verifier MUST pin the proof to
    // the externally-known DKG joint key. When an expected key is supplied, the
    // attestation's Q must equal it (canonical-encoding compare, malformed →
    // reject).
    if let Some(expected_hex) = expected_joint_key {
        let expected = match point_from_hex(expected_hex) {
            Some(p) => p,
            None => return false,
        };
        if q != expected {
            return false;
        }
    }
    let n = d_in.len();
    if n == 0 || d_out.len() != n {
        return false;
    }
    // The proof must be ABOUT the decks bound by the trait's hashes (T6 — the
    // verifier never trusts the embedded decks blindly; they must hash to the
    // committed input/output hashes, which the caller binds to the transcript).
    if deck_hash(&d_in) != *input_hash || deck_hash(&d_out) != *output_hash {
        return false;
    }
    if proof.input_deck_hash != hex_hash(input_hash)
        || proof.output_deck_hash != hex_hash(output_hash)
    {
        return false;
    }

    let h = *pedersen_h();

    // Re-derive the public challenges from the SAME transcript binding (T6).
    let mut t = statement_transcript(party, round, &q, &d_in, &d_out);
    let e = challenge_weights(&mut t, n);

    // Decode f commitments.
    if arg.f_commitments.len() != n {
        return false;
    }
    let cf: Vec<RistrettoPoint> = match arg
        .f_commitments
        .iter()
        .map(|s| point_from_hex(s))
        .collect::<Option<Vec<_>>>()
    {
        Some(v) => v,
        None => return false,
    };
    for cf_j in &cf {
        t.append_message(b"Cf", cf_j.compress().as_bytes());
    }

    // ---- Part A verification ----
    if !verify_reenc(&mut t, &arg.reenc, &d_in, &d_out, &cf, &e, &q, &h, n) {
        return false;
    }

    // ---- Part B verification ----
    let x = challenge_scalar(&mut t, b"x");
    verify_permutation(&mut t, &arg.perm, &cf, &e, &x, &h, n)
}

/// Verify Part A: the homomorphic difference is `(R·G, R·Q)` and the `f`
/// commitments were used to form `Σ f_j·D_in[j]`.
#[allow(clippy::too_many_arguments)]
fn verify_reenc(
    t: &mut Transcript,
    p: &ReencProof,
    d_in: &[Ct],
    d_out: &[Ct],
    cf: &[RistrettoPoint],
    e: &[Scalar],
    q: &RistrettoPoint,
    h: &RistrettoPoint,
    n: usize,
) -> bool {
    if p.z_f.len() != n || p.z_b.len() != n {
        return false;
    }
    let t1 = match point_from_hex(&p.t1) {
        Some(v) => v,
        None => return false,
    };
    let t2 = match point_from_hex(&p.t2) {
        Some(v) => v,
        None => return false,
    };
    let tf1 = match point_from_hex(&p.tf1) {
        Some(v) => v,
        None => return false,
    };
    let tf2 = match point_from_hex(&p.tf2) {
        Some(v) => v,
        None => return false,
    };
    if p.tb.len() != n {
        return false;
    }
    let tb: Vec<RistrettoPoint> = match p.tb.iter().map(|s| point_from_hex(s)).collect() {
        Some(v) => v,
        None => return false,
    };
    let z_r = match scalar_from_hex(&p.z_r) {
        Some(v) => v,
        None => return false,
    };
    let z_f: Vec<Scalar> = match p.z_f.iter().map(|s| scalar_from_hex(s)).collect() {
        Some(v) => v,
        None => return false,
    };
    let z_b: Vec<Scalar> = match p.z_b.iter().map(|s| scalar_from_hex(s)).collect() {
        Some(v) => v,
        None => return false,
    };

    t.append_message(b"A_T1", t1.compress().as_bytes());
    t.append_message(b"A_T2", t2.compress().as_bytes());
    t.append_message(b"A_Tf1", tf1.compress().as_bytes());
    t.append_message(b"A_Tf2", tf2.compress().as_bytes());
    for tb_j in &tb {
        t.append_message(b"A_Tb", tb_j.compress().as_bytes());
    }
    let c = challenge_scalar(t, b"c_a");

    // Δ = Σ_k e_k·D_out[k] − Σ_? … but Σ f_j·D_in[j] is hidden; we verify the
    // sigma equations that bind everything together.
    //
    // (1) The aggregate output combination (public, computable by verifier):
    //       Lhs1 = Σ_k e_k·D_out[k].c1 ;  Lhs2 = Σ_k e_k·D_out[k].c2
    let lhs1: RistrettoPoint = msm(e, &d_out.iter().map(|c| c.c1).collect::<Vec<_>>());
    let lhs2: RistrettoPoint = msm(e, &d_out.iter().map(|c| c.c2).collect::<Vec<_>>());

    // (2) f-knowledge over the input deck:  z_f·D_in == Tf + c·(Σ f_j·D_in[j]).
    //     We don't know Σ f_j·D_in[j] directly, but the homomorphic relation says
    //     Σ f_j·D_in[j] = Lhs − (R·G, R·Q). So:
    //       Σ z_f_j·D_in[j].c1 == Tf1 + c·( Lhs1 − R·G )
    //       Σ z_f_j·D_in[j].c2 == Tf2 + c·( Lhs2 − R·Q )
    //     and R·G, R·Q are pinned by the sigma over R:
    //       z_r·G == T1 + c·R·G   ⇒   we substitute (z_r·G − T1)/c = R·G, but
    //     to avoid division we check the combined equations directly:
    //
    //   Eq-A1:  Σ z_f_j·D_in[j].c1 + z_r·G  ==  Tf1 + T1 + c·Lhs1
    //   Eq-A2:  Σ z_f_j·D_in[j].c2 + z_r·Q  ==  Tf2 + T2 + c·Lhs2
    //
    // Derivation: substituting z_f_j = v_j + c·f_j and z_r = w + c·R,
    //   LHS-A1 = Σ v_j·D_in[j].c1 + c·Σ f_j·D_in[j].c1 + w·G + c·R·G
    //          = Tf1 + w·G + c·(Σ f_j·D_in[j].c1 + R·G)
    //   and Σ f_j·D_in[j].c1 + R·G = Lhs1 (the homomorphic identity, c1 coord),
    //   so LHS-A1 = Tf1 + T1 + c·Lhs1 = RHS-A1. A cheating output deck breaks
    //   the homomorphic identity, so Eq-A holds only for a genuine shuffle.
    let zf_din_c1: RistrettoPoint = msm(&z_f, &d_in.iter().map(|c| c.c1).collect::<Vec<_>>());
    let zf_din_c2: RistrettoPoint = msm(&z_f, &d_in.iter().map(|c| c.c2).collect::<Vec<_>>());

    let eq_a1 = (zf_din_c1 + z_r * G) == (tf1 + t1 + c * lhs1);
    let eq_a2 = (zf_din_c2 + z_r * q) == (tf2 + t2 + c * lhs2);

    // (3) PER-ELEMENT binding of z_f_j / z_b_j to each f commitment
    //     Cf_j = f_j·G + b_j·H:
    //       z_f_j·G + z_b_j·H  ==  Tb_j + c·Cf_j     for every j
    //   Because the SAME v_j appear in Tf (Eq-A) and Tb_j (here), the f_j used in
    //   the homomorphic sum are index-matched to the committed Cf_j (which Part B
    //   proves is a permutation of e). This closes the "redistribute f_j across
    //   indices keeping the sum" gap that a single aggregate check would leave.
    for j in 0..n {
        if (z_f[j] * G + z_b[j] * h) != (tb[j] + c * cf[j]) {
            return false;
        }
    }

    eq_a1 && eq_a2
}

/// Verify Part B: the committed `f` is a permutation of public `e` via the
/// chained product `∏_j (x − f_j) == ∏_k (x − e_k)`.
///
/// Each step proves `P_t = P_{t-1}·(x − f_t)` with the sound Schnorr
/// multiplication argument; the final running-product commitment is opened to the
/// public `target = ∏_k (x − e_k)` via the revealed `final_blind`.
fn verify_permutation(
    t: &mut Transcript,
    p: &PermProof,
    cf: &[RistrettoPoint],
    e: &[Scalar],
    x: &Scalar,
    h: &RistrettoPoint,
    n: usize,
) -> bool {
    if p.p_commitments.len() != n || p.steps.len() != n {
        return false;
    }
    let cp: Vec<RistrettoPoint> = match p
        .p_commitments
        .iter()
        .map(|s| point_from_hex(s))
        .collect::<Option<Vec<_>>>()
    {
        Some(v) => v,
        None => return false,
    };
    for cp_t in &cp {
        t.append_message(b"Cp", cp_t.compress().as_bytes());
    }

    // C_y for step t = factor commitment Cfac_t = x·G − Cf_t (committed value
    // x − f_t, blind −b_t), reconstructed from public x and the committed Cf_t.
    for tdx in 0..n {
        let step = &p.steps[tdx];
        // C_x = Cp_{t-1} (or 1·G for t == 0), C_y = Cfac_t, C_z = Cp_t.
        let c_x = if tdx == 0 { G } else { cp[tdx - 1] };
        let c_y = x * G - cf[tdx];
        let c_z = cp[tdx];

        let b_pt = match point_from_hex(&step.b_pt) {
            Some(v) => v,
            None => return false,
        };
        let b_prime = match point_from_hex(&step.b_prime) {
            Some(v) => v,
            None => return false,
        };
        t.append_message(b"M_B", b_pt.compress().as_bytes());
        t.append_message(b"M_Bp", b_prime.compress().as_bytes());
        let c = challenge_scalar(t, b"c_mul");

        let z_x = match scalar_from_hex(&step.z_x) {
            Some(v) => v,
            None => return false,
        };
        let z_rx = match scalar_from_hex(&step.z_rx) {
            Some(v) => v,
            None => return false,
        };
        let z_rz = match scalar_from_hex(&step.z_rz) {
            Some(v) => v,
            None => return false,
        };

        // (1) knowledge of x_val (the previous product) opening C_x:
        //       z_x·G + z_rx·H == B + c·C_x
        if (z_x * G + z_rx * h) != (b_pt + c * c_x) {
            return false;
        }
        // (2) the multiplication relation z = x·y, i.e. P_t == P_{t-1}·(x − f_t):
        //       z_x·C_y + z_rz·H == B' + c·C_z
        //   (sound: LHS − RHS = c·(x_val·y_val − z_val)·G, zero iff the product holds)
        if (z_x * c_y + z_rz * h) != (b_prime + c * c_z) {
            return false;
        }
    }

    // Final: the top running product must equal the public target ∏_k (x − e_k).
    // The prover reveals the final blind; we check Cp_{n-1} == target·G + s·H.
    // (target is public, so revealing s leaks nothing about the permutation.)
    let target: Scalar = e.iter().fold(Scalar::ONE, |acc, ek| acc * (x - ek));
    let final_blind = match scalar_from_hex(&p.final_blind) {
        Some(v) => v,
        None => return false,
    };
    cp[n - 1] == (target * G + final_blind * h)
}

// ===========================================================================
// Helpers: MSM, deck (de)coding, attestation (de)serialization, uniform sampling
// ===========================================================================

/// Multi-scalar multiplication `Σ scalars[i]·points[i]` (constant-time MSM).
fn msm(scalars: &[Scalar], points: &[RistrettoPoint]) -> RistrettoPoint {
    if scalars.is_empty() {
        return RistrettoPoint::identity();
    }
    RistrettoPoint::multiscalar_mul(scalars.iter().copied(), points.iter().copied())
}

fn decode_deck(wire: &[CtWire]) -> Option<Vec<Ct>> {
    wire.iter().map(Ct::from_wire).collect()
}

fn serialize_argument(arg: &ShuffleArgument) -> String {
    // Canonical JSON → hex (matches the spec §3.4 "canonical-JSON-then-hex").
    let bytes =
        crate::hash::canonical_json(&serde_json::to_value(arg).expect("serialize argument"));
    hex::encode(bytes)
}

fn deserialize_argument(att: &str) -> Option<ShuffleArgument> {
    let bytes = hex::decode(att).ok()?;
    serde_json::from_slice(&bytes).ok()
}

/// F3 (verifier anchor): decode the **output ciphertext deck** carried in a real
/// shuffle proof's attestation. The offline verifier uses this on the last
/// shuffle round to recover the committed final ciphertext deck, then anchors
/// every threshold-decryption open to `deck[deck_index]` (so a prover cannot open
/// a ciphertext it never committed to). Returns `None` for a non-real / malformed
/// attestation.
pub fn output_deck_from_proof(proof: &ShuffleProof) -> Option<Vec<Ct>> {
    if proof.scheme != SCHEME {
        return None;
    }
    let arg = deserialize_argument(&proof.attestation)?;
    decode_deck(&arg.output_deck)
}

/// Uniform integer in `0..bound` via rejection sampling on fresh random bytes
/// (no modulo bias). `bound` is small (≤52), so rejection is rare.
fn uniform_below<R: RngCore + CryptoRng>(rng: &mut R, bound: usize) -> usize {
    debug_assert!(bound > 0);
    if bound == 1 {
        return 0;
    }
    let bound = bound as u64;
    // Largest multiple of `bound` that fits in u64; reject above it.
    let zone = u64::MAX - (u64::MAX % bound);
    loop {
        let mut buf = [0u8; 8];
        rng.fill_bytes(&mut buf);
        let v = u64::from_le_bytes(buf);
        if v < zone {
            return (v % bound) as usize;
        }
    }
}

// ===========================================================================
// The trait impl (the seam — crypto.rs:71)
// ===========================================================================

/// Real verifiable re-encryption shuffle provider — **cross-vendor AI-audited
/// (ADR-076/077/078); open-source + verifiable (ADR-063)**.
///
/// Implements [`ShuffleProofProvider`] with the sound sigma-based interim
/// argument (scheme [`SCHEME`]). The prover holds the input/output decks + the
/// witness in a [`Shuffle`]; the verifier reconstructs the decks from the
/// attestation and checks they hash to the bound `input_hash`/`output_hash`.
///
/// The provider is constructed with the [`Shuffle`] witness for `prove_shuffle`
/// (the trait does not pass decks); `verify_shuffle` is fully self-contained
/// (everything it needs is in the proof + the bound hashes).
pub struct RealShuffleProofProvider {
    shuffle: Option<Shuffle>,
    /// F2: the DKG-derived joint key (64-hex) every shuffle proof MUST be bound
    /// to. When set, `verify_shuffle` rejects a proof whose attestation declares
    /// a different `Q` (a shuffler proving under a key it controls). When `None`,
    /// the bound key falls back to whatever the trait caller passes in
    /// `expected_joint_key`; if BOTH are absent the proof is only self-consistent
    /// (used by the lowest-level positive round-trip tests that already control
    /// the key by construction).
    expected_joint_key: Option<String>,
}

impl RealShuffleProofProvider {
    /// A verify-only provider (no witness, no pinned key). `prove_shuffle` panics
    /// if called. Callers that know the DKG joint key should prefer
    /// [`verifier_with_expected_key`](Self::verifier_with_expected_key) so the
    /// proof is bound to it (F2).
    pub fn verifier() -> Self {
        RealShuffleProofProvider {
            shuffle: None,
            expected_joint_key: None,
        }
    }

    /// A verify-only provider that PINS the DKG joint key (F2). Every proof it
    /// accepts must declare exactly this `Q` in its attestation.
    pub fn verifier_with_expected_key(joint_key_hex: impl Into<String>) -> Self {
        RealShuffleProofProvider {
            shuffle: None,
            expected_joint_key: Some(joint_key_hex.into()),
        }
    }

    /// A prove-and-verify provider holding the shuffle witness.
    pub fn with_witness(shuffle: Shuffle) -> Self {
        RealShuffleProofProvider {
            shuffle: Some(shuffle),
            expected_joint_key: None,
        }
    }
}

impl ShuffleProofProvider for RealShuffleProofProvider {
    fn scheme(&self) -> &'static str {
        SCHEME
    }

    fn prove_shuffle(
        &self,
        party: &str,
        round: u32,
        _input_hash: &Hash,
        _output_hash: &Hash,
    ) -> ShuffleProof {
        let shuffle = self.shuffle.as_ref().expect(
            "RealShuffleProofProvider::prove_shuffle requires a witness (use with_witness)",
        );
        // Prove uses real OS randomness for the sigma blinders.
        let mut rng = rand::rngs::OsRng;
        shuffle.prove(party, round, &mut rng)
    }

    fn verify_shuffle(
        &self,
        party: &str,
        round: u32,
        input_hash: &Hash,
        output_hash: &Hash,
        expected_joint_key: Option<&str>,
        proof: &ShuffleProof,
    ) -> bool {
        // F2: bind to a joint key if either the provider pins one or the caller
        // supplies one. A provider-pinned key takes precedence; if both are
        // present they must agree (else the proof is rejected — a caller and the
        // provider disagreeing on the encryption key is never a valid shuffle).
        let pinned = self.expected_joint_key.as_deref();
        let expected = match (pinned, expected_joint_key) {
            (Some(a), Some(b)) if a != b => return false,
            (Some(a), _) => Some(a),
            (None, b) => b,
        };
        verify_argument(party, round, input_hash, output_hash, expected, proof)
    }
}

#[cfg(test)]
mod tests;
