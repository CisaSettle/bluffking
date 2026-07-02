//! Poker RNG abstraction (ADR-012, amended by ADR-062 §2 / Goal ①).
//!
//! Wraps either `OsRng` (production) or `ChaCha20Rng` (tests).
//! The seed is a 256-bit `[u8; 32]` value ([`DeckSeed`]). For production hands
//! it is 32 random bytes drawn from `OsRng` and used directly as the full
//! ChaCha20 256-bit key; for tests it is a `u64` widened deterministically (see
//! [`PokerRng::from_seed`]). Production hands remain reproducible from their
//! recorded 32-byte seed.
//!
//! ORBIT (audit 2026-06-03 limitation, RESOLVED for production by Goal ①/ADR-062):
//! [`PokerRng::from_os`] now seeds ChaCha20 with a FULL 256-bit OS-random key
//! (`ChaCha20Rng::from_seed`), so the deck shuffle can reach effectively all of
//! the 52! (~2^225) possible orderings — the orbit is no longer reduced to 2^64
//! for production (random) hands. The seed is full 256-bit OS entropy expanded
//! through a CSPRNG (the standard "extract once, expand" pattern); "every deck
//! ordering is reachable and cryptographically unpredictable" (NOT "absolutely
//! random" — it is still a deterministic CSPRNG expansion, which is correct and
//! standard).
//!
//! [`PokerRng::from_seed`] is the **test/debug** constructor and intentionally
//! keeps the legacy `u64` expansion (`ChaCha20Rng::seed_from_u64`) so every
//! existing fixed-seed fixture produces the identical 52-card order (zero card
//! churn). It widens the `u64` into the low 8 little-endian bytes of the stored
//! 32-byte seed; the original `u64` is recoverable from `seed()[..8]`.

use rand::RngCore;
use rand::SeedableRng;
use rand_chacha::ChaCha20Rng;

/// 32-byte (256-bit) dealing seed. Replaces the former `u64` deck seed
/// (ADR-062 §2). The full 256-bit width lets the shuffle reach all 52!
/// orderings for production hands.
pub type DeckSeed = [u8; 32];

/// Widen a `u64` into a [`DeckSeed`]: the `u64` occupies the low 8 bytes
/// (little-endian), the remaining 24 bytes are zero. This is the documented,
/// stable expansion used by [`PokerRng::from_seed`] and by the server's
/// legacy-BIGINT read path so old `u64` seeds map deterministically to bytes.
pub fn widen(seed: u64) -> DeckSeed {
    let mut s = [0u8; 32];
    s[..8].copy_from_slice(&seed.to_le_bytes());
    s
}

/// Opaque RNG wrapper used by the engine.
///
/// Constructors:
/// - [`PokerRng::from_os`] — production: 32 bytes from `OsRng` as a full
///   ChaCha20 256-bit key (full-orbit, unpredictable).
/// - [`PokerRng::from_seed`] — test/debug: deterministic from a fixed `u64`
///   (legacy `seed_from_u64` expansion — stable fixtures).
/// - [`PokerRng::from_seed_bytes`] — deterministic from a full 256-bit seed.
pub struct PokerRng {
    inner: ChaCha20Rng,
    seed: DeckSeed,
}

impl PokerRng {
    /// Production constructor. Draws 32 random bytes from `OsRng` and uses them
    /// as the full ChaCha20 256-bit key (`ChaCha20Rng::from_seed`), so the
    /// reachable shuffle orbit is the full 52! (ADR-062 §2). Stores the 32-byte
    /// key as the recorded seed.
    pub fn from_os() -> Self {
        use rand::rngs::OsRng;
        let mut os = OsRng;
        let mut seed = [0u8; 32];
        os.fill_bytes(&mut seed);
        Self {
            inner: ChaCha20Rng::from_seed(seed),
            seed,
        }
    }

    /// Test/debug constructor. Produces a fully deterministic RNG from a fixed
    /// `u64` seed, using the **legacy** `ChaCha20Rng::seed_from_u64` expansion
    /// so every existing fixed-seed fixture produces the identical 52-card
    /// order. The stored seed is the widened (`widen`) 32-byte form.
    /// **Never use this in production code.**
    pub fn from_seed(seed: u64) -> Self {
        Self {
            inner: ChaCha20Rng::seed_from_u64(seed),
            seed: widen(seed),
        }
    }

    /// Deterministic constructor from a full 256-bit seed (mental-poker / future
    /// use). Uses the 32-byte value directly as the ChaCha20 key.
    pub fn from_seed_bytes(seed: DeckSeed) -> Self {
        Self {
            inner: ChaCha20Rng::from_seed(seed),
            seed,
        }
    }

    /// The 256-bit seed used to construct this RNG. Record this in the `hands`
    /// row (`deck_seed_b` BYTEA). For a [`PokerRng::from_seed`] RNG the low 8
    /// little-endian bytes recover the original `u64`.
    ///
    /// # Not a byte-level round-trip for `from_seed` (U67, dual-AI OSS review)
    ///
    /// For [`PokerRng::from_os`] / [`PokerRng::from_seed_bytes`] RNGs, feeding
    /// this value back into [`PokerRng::from_seed_bytes`] reproduces the
    /// identical stream (the bytes ARE the ChaCha20 key). For a
    /// [`PokerRng::from_seed`] RNG it does **not**: `from_seed(u64)` expands the
    /// `u64` via `ChaCha20Rng::seed_from_u64` (a hashed expansion), while the
    /// stored seed is only the [`widen`]ed byte form — so
    /// `from_seed_bytes(rng.seed())` yields a DIFFERENT deck. To reproduce a
    /// `from_seed` stream, recover the `u64` from `seed()[..8]` (little-endian)
    /// and call [`PokerRng::from_seed`] again.
    pub fn seed(&self) -> DeckSeed {
        self.seed
    }

    /// Fill `dest` with pseudo-random bytes. Internal use only.
    pub(crate) fn fill_bytes(&mut self, dest: &mut [u8]) {
        self.inner.fill_bytes(dest);
    }
}
