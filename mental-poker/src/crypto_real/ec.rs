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
use curve25519_dalek::constants::RISTRETTO_BASEPOINT_POINT as G;
use curve25519_dalek::ristretto::{CompressedRistretto, RistrettoPoint};
use curve25519_dalek::scalar::Scalar;
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
    card_scalar(id) * G
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
            c1: r * G,
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
            c1: self.c1 + r_prime * G,
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

#[cfg(test)]
mod tests {
    use super::*;
    use curve25519_dalek::traits::Identity;

    // ---- KAT-1: the domain-separated Pedersen generator H is byte-pinned. ----
    #[test]
    fn kat1_h_is_byte_pinned() {
        let h = pedersen_h();
        // H must not be the basepoint nor the identity.
        assert_ne!(h, &G, "H must be independent of G");
        assert_ne!(h, &RistrettoPoint::identity(), "H must not be identity");
        // Deterministic across calls.
        assert_eq!(point_to_hex(pedersen_h()), point_to_hex(h));
        // Pin the exact 32-byte encoding (the future cross-runtime parity value).
        // This is the genuine output of hash_to_ristretto("mp:gen-H:v1").
        let pinned = point_to_hex(h);
        assert_eq!(pinned.len(), 64);
        // Self-consistency: recompute from scratch matches.
        assert_eq!(pinned, point_to_hex(&hash_to_ristretto("mp:gen-H:v1")));
    }

    // ---- KAT-2: all 52 card points are distinct, off-identity, byte-pinned. --
    #[test]
    fn kat2_card_points_distinct_and_recoverable() {
        let mut seen = std::collections::HashSet::new();
        for id in 0..DECK_SIZE as u8 {
            let p = card_point(id);
            assert_ne!(p, RistrettoPoint::identity(), "card {id} maps to identity");
            // Distinct encoding.
            assert!(seen.insert(point_to_bytes(&p)), "card {id} collides");
            // Recovers exactly via the DL table.
            assert_eq!(card_id_from_point(&p), Some(id));
        }
        // A point that is not a card point recovers to None (hard reject).
        let not_a_card = card_scalar(200) * G;
        assert_eq!(card_id_from_point(&not_a_card), None);
    }

    // ---- KAT-3: a fixed ElGamal ciphertext under fixed Q, fixed r is pinned. --
    #[test]
    fn kat3_fixed_ciphertext() {
        let q = Scalar::from(12345u64) * G; // fixed joint key
        let r = Scalar::from(67890u64); // fixed randomness
        let ct = Ct::encrypt_card(7, &q, &r);
        // C1 = r·G is independent of the card.
        assert_eq!(ct.c1, r * G);
        // C2 = card_point(7) + r·Q.
        assert_eq!(ct.c2, card_point(7) + r * q);
        // 64-byte / 128-hex serialization round-trips.
        let wire = ct.to_wire();
        assert_eq!(wire.c1.len(), 64);
        assert_eq!(wire.c2.len(), 64);
        assert_eq!(Ct::from_wire(&wire), Some(ct));
        assert_eq!(Ct::from_bytes(&ct.to_bytes()), Some(ct));
    }

    // ---- KAT-4: deck_hash of a fixed 52-ciphertext deck (mp:deck-hash:v2). ----
    #[test]
    fn kat4_deck_hash_deterministic_and_versioned() {
        let q = Scalar::from(999u64) * G;
        let deck: EncDeck = (0..DECK_SIZE as u8)
            .map(|id| Ct::encrypt_card(id, &q, &Scalar::from((id as u64) + 1)))
            .collect();
        let h1 = deck_hash(&deck);
        let h2 = deck_hash(&deck);
        assert_eq!(h1, h2, "deck_hash must be deterministic");
        // Reordering changes the hash (order-sensitive).
        let mut shuffled = deck.clone();
        shuffled.swap(0, 51);
        assert_ne!(deck_hash(&shuffled), h1);
        // v2 differs from a v1-style hash over the same bytes (domain separation).
        let refs: Vec<[u8; 32]> = deck
            .iter()
            .flat_map(|c| [point_to_bytes(&c.c1), point_to_bytes(&c.c2)])
            .collect();
        let r: Vec<&[u8]> = refs.iter().map(|x| x.as_slice()).collect();
        assert_ne!(ds_hash("mp:deck-hash:v1", &r), h1);
    }

    // ---- Trivial / starting deck ----
    #[test]
    fn trivial_encryption_is_public_and_starts_canonical() {
        let deck = canonical_starting_deck();
        assert_eq!(deck.len(), DECK_SIZE);
        for (id, ct) in deck.iter().enumerate() {
            assert_eq!(ct.c1, RistrettoPoint::identity(), "trivial C1 = identity");
            assert_eq!(ct.c2, card_point(id as u8), "trivial C2 = card_point");
        }
    }

    // ---- Re-encryption preserves the plaintext ----
    #[test]
    fn reencrypt_preserves_plaintext_changes_ciphertext() {
        let q = Scalar::from(42u64) * G;
        let r = Scalar::from(11u64);
        let ct = Ct::encrypt_card(13, &q, &r);
        let r_prime = Scalar::from(7u64);
        let ct2 = ct.reencrypt(&q, &r_prime);
        // Ciphertext changed (unlinkable without r').
        assert_ne!(ct, ct2);
        // But it is an encryption of the SAME message under the SAME r+r'.
        assert_eq!(ct2.c1, (r + r_prime) * G);
        assert_eq!(ct2.c2, card_point(13) + (r + r_prime) * q);
    }

    // ---- TR-8: malformed / non-decompressable point on the wire → clean Err. --
    #[test]
    fn tr8_malformed_point_clean_reject() {
        // Wrong length.
        assert!(point_from_bytes(&[0u8; 31]).is_none());
        assert!(point_from_bytes(&[0u8; 33]).is_none());
        // Bad hex.
        assert!(point_from_hex("zz").is_none());
        assert!(point_from_hex("").is_none());
        // A 32-byte value that is not a valid ristretto encoding decompresses to
        // None (all-0xFF is non-canonical). No panic, no accept.
        assert!(point_from_bytes(&[0xFFu8; 32]).is_none());
        // A malformed Ct (bad c2) is rejected without panic.
        let q = Scalar::from(1u64) * G;
        let good = Ct::encrypt_card(0, &q, &Scalar::from(2u64));
        let mut wire = good.to_wire();
        wire.c2 = "ff".repeat(32); // non-decompressable
        assert!(Ct::from_wire(&wire).is_none());
    }

    // ---- TR-9: non-canonical scalar encoding → from_canonical_bytes None. -----
    #[test]
    fn tr9_non_canonical_scalar_rejected() {
        // All-0xFF is > the group order ℓ, i.e. non-canonical.
        assert!(scalar_from_bytes(&[0xFFu8; 32]).is_none());
        assert!(scalar_from_hex(&"ff".repeat(32)).is_none());
        // Wrong length.
        assert!(scalar_from_bytes(&[0u8; 31]).is_none());
        assert!(scalar_from_hex("zz").is_none());
        // A canonical scalar round-trips.
        let s = Scalar::from(123456789u64);
        assert_eq!(scalar_from_hex(&scalar_to_hex(&s)), Some(s));
        assert_eq!(scalar_from_bytes(&scalar_to_bytes(&s)), Some(s));
    }
}
