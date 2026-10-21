//! Elliptic-curve primitives over **ristretto255** — **cross-vendor AI-audited
//! (ADR-076/077/078); open-source + verifiable (ADR-063 §1, spec §1)**.
//!
//! This is the shared arithmetic substrate for the server-blind dealing
//! increment: ElGamal ciphertexts, the card-id ↔ message-point encoding + 52-
//! entry discrete-log recovery table, the domain-separated Pedersen generator
//! `H`, and the canonical byte/hex codecs (with hard decode-reject paths, threat
//! T9). Every byte encoding here is the **parity contract** (spec §1) so the
//! future WASM(web) / flutter_rust_bridge(mobile) runtimes can match.
//!
//! ## Why ristretto255
//!
//! `curve25519-dalek`'s `RistrettoPoint` is a **prime-order** group: a
//! successful `decompress()` yields a valid group element with **no** cofactor
//! clearing or subgroup check needed — exactly what mental poker (ElGamal,
//! Chaum–Pedersen, the shuffle commitments) wants. Field arithmetic uses the
//! formally-verified (fiat-crypto / Coq-generated) backend, and the lineage was
//! audited by Quarkslab (2019). The *Phase-4 integration* as a whole is
//! cross-vendor AI-audited (ADR-076/077/078) and independently verifiable.
//!
//! ## Status
//!
//! Cross-vendor AI-audited (ADR-076/077/078); open-source + verifiable. GA'd for
//! the engine-blind table class by ADR-070 (which lifted the ADR-063 cage); in
//! production these run ONLY for engine-blind sessions (`resolve_mp_crypto_mode`).

use crate::hash::ds_hash;
// PERF: the `precomputed-tables` fixed-base table for G. `&s * GT` is a
// constant-time fixed-base ladder (~4x a variable-base mult), so it is safe on
// prover/secret-dependent paths too.
use curve25519_dalek::constants::RISTRETTO_BASEPOINT_TABLE as GT;
use curve25519_dalek::ristretto::{CompressedRistretto, RistrettoPoint};
use curve25519_dalek::scalar::Scalar;
use rand_core::RngCore;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha512};
use std::collections::HashMap;
use std::sync::OnceLock;

/// Number of cards in a standard deck (mirrors [`crate::card_id::DECK_SIZE`]).
pub const DECK_SIZE: usize = 52;

// ---------------------------------------------------------------------------
// §1.1 / §1.2 — canonical byte & hex codecs (the parity contract; T9 rejects)
// ---------------------------------------------------------------------------

/// Compress a `RistrettoPoint` to its canonical 32 bytes (`CompressedRistretto`).
pub fn point_to_bytes(p: &RistrettoPoint) -> [u8; 32] {
    p.compress().to_bytes()
}

/// Decompress 32 bytes back into a `RistrettoPoint`.
///
/// Returns `None` on a non-decompressable encoding — a **hard reject**, never a
/// panic (threat T9 / TR-8). ristretto255 is prime-order, so a successful
/// decompress is a valid group element; no further subgroup check is needed.
pub fn point_from_bytes(b: &[u8]) -> Option<RistrettoPoint> {
    if b.len() != 32 {
        return None;
    }
    CompressedRistretto::from_slice(b).ok()?.decompress()
}

/// `true` when `q` is the **identity** element and must be rejected as a party
/// public key (backend review F-CRYPTO-15). This is the single shared definition
/// of "bad party key" — every consuming verifier calls it instead of inlining
/// the check, so the rejection rationale lives in one place and a new consuming
/// site can't silently re-open the gap by forgetting the predicate.
///
/// A party that registers the identity (`x_i = 0`) as its shuffle/DKG public key
/// contributes nothing while still passing the per-party proofs: the Schnorr PoK
/// verifies trivially for `x = 0` (`s·G == R + c·Q` reduces to `s·G == R`), a
/// shuffle's re-encryption term `r'·Q` becomes the identity (so `C2 == M`, every
/// card in the clear), and a threshold-decryption share `D_i = x_i·C1` is the
/// identity with a trivially-valid DLEQ. Any one of these silently degrades
/// n-of-n confidentiality to (n-1)-of-(n-1). Each call site still keeps its own
/// reject (defense-in-depth across independent trust boundaries — DKG, shuffle,
/// threshold-decrypt, transcript verifier); this only dedupes the predicate.
pub fn is_identity_pubkey(q: &RistrettoPoint) -> bool {
    use curve25519_dalek::traits::IsIdentity;
    q.is_identity()
}

/// Lower-case 64-char hex of a point's canonical 32-byte encoding.
pub fn point_to_hex(p: &RistrettoPoint) -> String {
    hex::encode(point_to_bytes(p))
}

/// Parse a point from lower-case hex. `None` on bad hex, wrong length, or a
/// non-decompressable point (T9 / TR-8 — clean reject, no panic).
pub fn point_from_hex(s: &str) -> Option<RistrettoPoint> {
    let bytes = hex::decode(s).ok()?;
    point_from_bytes(&bytes)
}

/// Canonical 32-byte little-endian scalar encoding (reduced form).
pub fn scalar_to_bytes(s: &Scalar) -> [u8; 32] {
    s.to_bytes()
}

/// Decode a scalar from 32 canonical little-endian bytes.
///
/// Rejects **non-canonical** encodings (`Scalar::from_canonical_bytes` returns
/// `None` — threat T9 / TR-9), so a malleated scalar cannot slip through.
pub fn scalar_from_bytes(b: &[u8]) -> Option<Scalar> {
    let arr: [u8; 32] = b.try_into().ok()?;
    Option::from(Scalar::from_canonical_bytes(arr))
}

/// Lower-case 64-char hex of a canonical scalar.
pub fn scalar_to_hex(s: &Scalar) -> String {
    hex::encode(scalar_to_bytes(s))
}

/// Parse a scalar from lower-case hex. `None` on bad hex, wrong length, or a
/// non-canonical encoding (T9 / TR-9).
pub fn scalar_from_hex(s: &str) -> Option<Scalar> {
    let bytes = hex::decode(s).ok()?;
    scalar_from_bytes(&bytes)
}

// ---------------------------------------------------------------------------
// §1.3 — domain-separated Pedersen generator H (log_G(H) unknown to everyone)
// ---------------------------------------------------------------------------

/// The fixed second generator `H` for Pedersen commitments, derived by
/// hash-to-group from a fixed domain so that `log_G(H)` is unknown to all
/// parties. NEVER reuse `G` as `H`.
///
/// `H = hash_to_ristretto("mp:gen-H:v1")` per spec §1.3. Computed once.
pub fn pedersen_h() -> &'static RistrettoPoint {
    static H: OnceLock<RistrettoPoint> = OnceLock::new();
    H.get_or_init(|| hash_to_ristretto("mp:gen-H:v1"))
}

/// Hash a domain string to a ristretto255 point (spec §1.3).
///
/// Expands the 32-byte `ds_hash` to the 64 wide bytes `from_uniform_bytes`
/// requires by hashing two domain-separated `ds_hash` outputs through SHA-512.
pub fn hash_to_ristretto(domain: &str) -> RistrettoPoint {
    let part0 = ds_hash("mp:h2g:v1", &[domain.as_bytes()]);
    let part1 = ds_hash("mp:h2g:v1", &[domain.as_bytes(), &[1u8]]);
    let mut wide = [0u8; 64];
    let mut hasher = Sha512::new();
    hasher.update(part0);
    hasher.update(part1);
    wide.copy_from_slice(&hasher.finalize());
    RistrettoPoint::from_uniform_bytes(&wide)
}

/// A fresh 128-bit verifier-side batching weight.
///
/// **Why batching is sound (the "small-exponent" test).** A family of group
/// equations `LHS_j == RHS_j` (j = 0..m) is equivalent, up to a negligible
/// error, to the SINGLE equation `Σ_j ρ_j·(LHS_j − RHS_j) == O` for weights
/// `ρ_j` drawn uniformly and INDEPENDENTLY *after* the proof is fixed. If any
/// individual equation fails, its difference `Δ_j` is a non-identity point, and
/// the weighted sum vanishes only for weight vectors lying in one coset of a
/// proper subgroup of the weight space — probability ≤ 2⁻¹²⁸ for 128-bit
/// weights. Crucially the weights are **per index**, so per-index binding
/// arguments survive: a prover who redistributes values across indices changes
/// at least one `Δ_j` and is caught with the same probability.
///
/// The weights come from OS randomness (`rand::rngs::OsRng` → `getrandom`; on
/// wasm32 that is the browser CSPRNG via the crate's `getrandom` "js" feature),
/// NOT from the Fiat–Shamir transcript, so a malicious prover cannot grind
/// them. They are verifier-local and never transmitted.
pub(crate) fn batch_weight<R: RngCore>(rng: &mut R) -> Scalar {
    let mut b = [0u8; 16];
    rng.fill_bytes(&mut b);
    Scalar::from(u128::from_le_bytes(b))
}

// ---------------------------------------------------------------------------
// §1.5 — card-id ↔ message-point encoding + 52-entry DL recovery table
// ---------------------------------------------------------------------------

/// The scalar encoding a card id: `Scalar::from(id + 1)`. The `+1` keeps id 0
/// off the identity and every card point distinct (spec §1.5).
pub fn card_scalar(id: u8) -> Scalar {
    Scalar::from((id as u64) + 1)
}

/// The message point encoding a card id: `card_scalar(id) · G`.
pub fn card_point(id: u8) -> RistrettoPoint {
    &card_scalar(id) * GT
}

/// Recover a card id from a decrypted message point via the 52-entry DL table.
///
/// Returns `None` if the point is not one of the 52 card points (a hard reject —
/// a wrong/forged opening cannot be silently coerced into a valid card id).
pub fn card_id_from_point(m: &RistrettoPoint) -> Option<u8> {
    recovery_table().get(&point_to_bytes(m)).copied()
}

/// The `{ card_point(id).compress() → id }` lookup, built once (52 fixed-base
/// mults at first use, then a hashmap lookup per open).
fn recovery_table() -> &'static HashMap<[u8; 32], u8> {
    static TABLE: OnceLock<HashMap<[u8; 32], u8>> = OnceLock::new();
    TABLE.get_or_init(|| {
        let mut m = HashMap::with_capacity(DECK_SIZE);
        for id in 0..DECK_SIZE as u8 {
            m.insert(point_to_bytes(&card_point(id)), id);
        }
        m
    })
}

// ---------------------------------------------------------------------------
// §1.4 — ElGamal ciphertext `Ct = (C1, C2) = (r·G, M + r·Q)`
// ---------------------------------------------------------------------------

/// An ElGamal ciphertext of a message point `M` under a joint public key `Q`:
/// `Ct = (C1, C2) = (r·G, M + r·Q)` (spec §1.4).
///
/// On the wire / in the transcript it serializes as
/// `{"c1": "<64hex>", "c2": "<64hex>"}` (C1 first; 128 hex chars total). The
/// in-memory form holds the decompressed points; (de)serialization goes through
/// the canonical hex codecs (and so rejects malformed points / T8).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Ct {
    /// `C1 = r·G`.
    pub c1: RistrettoPoint,
    /// `C2 = M + r·Q`.
    pub c2: RistrettoPoint,
}

impl Ct {
    /// Encrypt a message point `m` under joint key `q` with randomness `r`.
    pub fn encrypt(m: &RistrettoPoint, q: &RistrettoPoint, r: &Scalar) -> Self {
        Ct {
            c1: r * GT,
            c2: m + r * q,
        }
    }

    /// Encrypt a card id under joint key `q` with randomness `r`.
    pub fn encrypt_card(id: u8, q: &RistrettoPoint, r: &Scalar) -> Self {
        Ct::encrypt(&card_point(id), q, r)
    }

    /// The trivial (public) encryption of a message point: `r = 0`, so
    /// `Ct = (identity, M)`. The starting deck `D_0` is the trivial encryption
    /// of the canonical card order (spec §1.6).
    pub fn trivial(m: &RistrettoPoint) -> Self {
        Ct {
            c1: RistrettoPoint::default(), // identity
            c2: *m,
        }
    }

    /// Re-encrypt with fresh randomness `r'` (the shuffle's per-card op): adds
    /// `r'·G` to `C1` and `r'·Q` to `C2`, preserving the plaintext `M`
    /// (spec §1.4 / §3.2).
    pub fn reencrypt(&self, q: &RistrettoPoint, r_prime: &Scalar) -> Self {
        Ct {
            c1: self.c1 + r_prime * GT,
            c2: self.c2 + r_prime * q,
        }
    }

    /// `C1.compress() ‖ C2.compress()` = 64 bytes (C1 first).
    pub fn to_bytes(&self) -> [u8; 64] {
        let mut out = [0u8; 64];
        out[..32].copy_from_slice(&point_to_bytes(&self.c1));
        out[32..].copy_from_slice(&point_to_bytes(&self.c2));
        out
    }

    /// Decode from 64 bytes (`C1 ‖ C2`). `None` if either point is malformed
    /// (T8 — clean reject, no panic).
    pub fn from_bytes(b: &[u8]) -> Option<Self> {
        if b.len() != 64 {
            return None;
        }
        Some(Ct {
            c1: point_from_bytes(&b[..32])?,
            c2: point_from_bytes(&b[32..])?,
        })
    }

    /// Serializable hex form `{c1, c2}`.
    pub fn to_wire(&self) -> CtWire {
        CtWire {
            c1: point_to_hex(&self.c1),
            c2: point_to_hex(&self.c2),
        }
    }

    /// Decode from the hex wire form. `None` on any malformed point (T8).
    pub fn from_wire(w: &CtWire) -> Option<Self> {
        Some(Ct {
            c1: point_from_hex(&w.c1)?,
            c2: point_from_hex(&w.c2)?,
        })
    }
}

/// JSON wire form of a [`Ct`]: `{"c1": "<64hex>", "c2": "<64hex>"}` (spec §1.4).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CtWire {
    /// `C1` as lower-case 64-char hex.
    pub c1: String,
    /// `C2` as lower-case 64-char hex.
    pub c2: String,
}

// ---------------------------------------------------------------------------
// §1.6 — deck hash (mp:deck-hash:v2) over the ciphertext deck
// ---------------------------------------------------------------------------

/// The deck-in-flight: 52 ElGamal ciphertexts.
pub type EncDeck = Vec<Ct>;

/// `deck_hash` over a ciphertext deck (the value published as the deck
/// commitment): `ds_hash("mp:deck-hash:v2", [for each Ct: C1.compress(),
/// C2.compress()])` — 104 length-prefixed 32-byte parts (spec §1.6). `v2`
/// distinguishes it from the Phase-1 commitment-form `mp:deck-hash:v1`.
pub fn deck_hash(deck: &[Ct]) -> crate::hash::Hash {
    let mut parts: Vec<[u8; 32]> = Vec::with_capacity(deck.len() * 2);
    for ct in deck {
        parts.push(point_to_bytes(&ct.c1));
        parts.push(point_to_bytes(&ct.c2));
    }
    let refs: Vec<&[u8]> = parts.iter().map(|p| p.as_slice()).collect();
    ds_hash("mp:deck-hash:v2", &refs)
}

/// The canonical starting deck `D_0` = trivial encryption of card order
/// `0..52` under any `Q` (the trivial encryption ignores `Q`, since `r = 0`).
pub fn canonical_starting_deck() -> EncDeck {
    (0..DECK_SIZE as u8)
        .map(|id| Ct::trivial(&card_point(id)))
        .collect()
}
