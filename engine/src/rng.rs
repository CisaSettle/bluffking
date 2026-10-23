//! Poker CSPRNG using a reproducible 256-bit ChaCha20 seed.
//! Production seeds use 32 fresh bytes from the operating system.

use rand::RngCore;
use rand::SeedableRng;
use rand_chacha::ChaCha20Rng;

/// 32-byte (256-bit) dealing seed. Replaces the former `u64` deck seed
/// (ADR-062 §2). The full 256-bit width lets the shuffle reach all 52!
/// orderings for production hands.
pub type DeckSeed = [u8; 32];

/// Opaque RNG wrapper used by the engine.
///
/// Constructors:
/// - [`PokerRng::from_os`] — production: 32 bytes from `OsRng` as a full
///   ChaCha20 256-bit key (full-orbit, unpredictable).
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

    /// Deterministic constructor from a full 256-bit seed (mental-poker / future
    /// use). Uses the 32-byte value directly as the ChaCha20 key.
    pub fn from_seed_bytes(seed: DeckSeed) -> Self {
        Self {
            inner: ChaCha20Rng::from_seed(seed),
            seed,
        }
    }

    /// The full reproducibility seed recorded with the hand.
    pub fn seed(&self) -> DeckSeed {
        self.seed
    }

    /// Fill `dest` with pseudo-random bytes. Internal use only.
    pub(crate) fn fill_bytes(&mut self, dest: &mut [u8]) {
        self.inner.fill_bytes(dest);
    }
}
