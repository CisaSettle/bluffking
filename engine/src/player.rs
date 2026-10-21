//! Player identity and table position types.

use serde::{Deserialize, Serialize};

/// An opaque player identifier (wraps a `u64`).
///
/// In the server layer, this maps to the user's DB primary key bytes
/// interpreted as a `u64`. The engine treats it as an opaque identifier.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct PlayerId(pub u64);

impl PlayerId {
    /// Create a new `PlayerId`.
    pub const fn new(id: u64) -> Self {
        Self(id)
    }

    /// Get the inner `u64`.
    pub const fn inner(self) -> u64 {
        self.0
    }
}

impl std::fmt::Display for PlayerId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.0)
    }
}

/// Chip count, as an unsigned 32-bit integer.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub struct Chips(pub u32);

impl Chips {
    /// Zero chips.
    pub const ZERO: Self = Self(0);

    /// Get the inner `u32`.
    pub const fn inner(self) -> u32 {
        self.0
    }

    /// Saturating addition.
    pub fn saturating_add(self, other: Self) -> Self {
        Self(self.0.saturating_add(other.0))
    }

    /// Checked subtraction. Returns `None` if `other > self`.
    pub fn checked_sub(self, other: Self) -> Option<Self> {
        self.0.checked_sub(other.0).map(Self)
    }

    /// Saturating subtraction.
    pub fn saturating_sub(self, other: Self) -> Self {
        Self(self.0.saturating_sub(other.0))
    }
}

impl std::fmt::Display for Chips {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.0)
    }
}

impl From<u32> for Chips {
    fn from(v: u32) -> Self {
        Self(v)
    }
}

impl From<Chips> for u32 {
    fn from(c: Chips) -> u32 {
        c.0
    }
}

/// Seat index (0-based, up to 8 for a 9-seat table).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct Seat(pub u8);

impl Seat {
    /// Create a new `Seat`.
    pub const fn new(seat: u8) -> Self {
        Self(seat)
    }

    /// Get the inner `u8`.
    pub const fn inner(self) -> u8 {
        self.0
    }
}

impl std::fmt::Display for Seat {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.0)
    }
}

/// A player's position relative to the dealer button.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Position {
    /// Dealer / button (also small blind in heads-up).
    Dealer,
    /// Small blind.
    SmallBlind,
    /// Big blind.
    BigBlind,
    /// Under the gun (first to act preflop in 4+ player games).
    Utg,
    /// UTG+1.
    UtgPlus1,
    /// UTG+2.
    UtgPlus2,
    /// Lojack (9-max seat 4 from dealer; GTO Wizard / PioSolver standard).
    ///
    /// Replaces "MP" in 9-max; "MP" is a 6-max concept. The canonical
    /// 9-max labelling (GTO Wizard / 2+2 consensus) is:
    /// UTG, UTG+1, UTG+2, LJ, HJ, CO, BTN, SB, BB.
    Lojack,
    /// Hijack (2 seats right of the button).
    Hijack,
    /// Cutoff (1 seat right of the button).
    Cutoff,
}

impl Position {
    /// Canonical English short label for persistence and replay.
    ///
    /// Returns a `&'static str` that never allocates. Used by the M6-3
    /// decision-context persistence path (ADR-026 §7) so that the replay viewer
    /// and coach can display position labels without re-deriving them.
    ///
    /// Labels: "UTG" | "UTG+1" | "UTG+2" | "LJ" | "HJ" | "CO" | "BTN" | "SB" | "BB".
    ///
    /// "LJ" is the canonical 9-max label (GTO Wizard / PioSolver / 2+2 consensus).
    /// "MP" is not used — it is a 6-max concept.
    pub fn short_label(&self) -> &'static str {
        match self {
            Position::Utg => "UTG",
            Position::UtgPlus1 => "UTG+1",
            Position::UtgPlus2 => "UTG+2",
            Position::Lojack => "LJ",
            Position::Hijack => "HJ",
            Position::Cutoff => "CO",
            Position::Dealer => "BTN",
            Position::SmallBlind => "SB",
            Position::BigBlind => "BB",
        }
    }

    /// Compute a player's position given their seat index, the dealer's seat
    /// index, and the **ordered list of all active seat indices** at the table.
    ///
    /// Using the active seat list (rather than just `num_seats`) ensures
    /// correctness when seat numbers are non-contiguous — e.g. after a bust-out
    /// leaves seats `[0, 1, 3]` in a 4-handed game.
    ///
    /// Position assignments:
    /// - **Heads-up (n=2):** dealer = SB, opponent = BB.
    /// - **3-handed (n=3):** dealer = BTN, +1 = SB, +2 = BB.
    /// - **4+ players (n≥4):** standard 6-max labels working backwards from BTN:
    ///   BTN, SB, BB, UTG, UTG+1, UTG+2, MP, HJ, CO (in seat order from dealer).
    ///
    /// `seat` and `dealer_seat` are seat indices (not necessarily 0-based or
    /// contiguous). `active_seats` must contain both `seat` and `dealer_seat`.
    ///
    /// # Example
    /// ```
    /// use engine::Position;
    /// // 6-max, dealer=0. Seat 2 is two seats after the button → the big blind.
    /// let pos = Position::for_seat(2, 0, &[0, 1, 2, 3, 4, 5]);
    /// assert_eq!(pos, Position::BigBlind);
    /// ```
    pub fn for_seat(seat: u8, dealer_seat: u8, active_seats: &[u8]) -> Position {
        let n = active_seats.len();
        if n <= 1 {
            return Position::Dealer;
        }

        // Find rank of dealer and target seat in the active list.
        // Distance = (rank_of_seat - rank_of_dealer + n) % n.
        let dealer_rank = match active_seats.iter().position(|&s| s == dealer_seat) {
            Some(r) => r,
            None => {
                // Dealer seat is not in the active list (stale/busted button in a
                // snapshot). The distance math is undefined; rather than silently
                // mislabelling every seat off a phantom rank-0 button, report the
                // queried seat as Dealer (mirrors the unknown-`seat` branch below).
                // Defensive only — callers should always pass a dealer in
                // `active_seats`.
                return Position::Dealer;
            }
        };
        let seat_rank = match active_seats.iter().position(|&s| s == seat) {
            Some(r) => r,
            None => {
                // Seat not found in active list — treat as dealer (defensive).
                return Position::Dealer;
            }
        };

        let dist = (seat_rank + n - dealer_rank) % n;
        let dist = dist as u8;
        let num = n as u8;

        match num {
            2 => {
                // Heads-up: dealer = SB, opponent = BB.
                match dist {
                    0 => Position::SmallBlind, // dealer IS SB heads-up
                    _ => Position::BigBlind,
                }
            }
            3 => {
                // 3-handed: BTN=0, SB=1, BB=2.
                match dist {
                    0 => Position::Dealer,
                    1 => Position::SmallBlind,
                    _ => Position::BigBlind,
                }
            }
            4 => {
                // 4-handed: BTN, SB, BB, UTG.
                match dist {
                    0 => Position::Dealer,
                    1 => Position::SmallBlind,
                    2 => Position::BigBlind,
                    _ => Position::Utg,
                }
            }
            5 => {
                // 5-handed: BTN, SB, BB, UTG, CO. The seat immediately right of
                // the button (dist=4) is the Cutoff, not the Hijack — matching the
                // n>=6 arm (`num-1 => Cutoff`) and standard convention (U25,
                // dual-AI OSS review). Mislabelling it Hijack bucketed the CO as
                // MP in the preflop charts.
                match dist {
                    0 => Position::Dealer,
                    1 => Position::SmallBlind,
                    2 => Position::BigBlind,
                    3 => Position::Utg,
                    _ => Position::Cutoff,
                }
            }
            _ => {
                // 6+ players: standard labels.
                // 9-max canonical (GTO Wizard / PioSolver / 2+2 consensus):
                //   BTN(0), SB(1), BB(2), UTG(3), UTG+1(4), UTG+2(5), LJ(6), HJ(n-2), CO(n-1).
                // "MP" is a 6-max concept and is not used for 7+ player tables.
                // For 6-max (n=6): BTN(0), SB(1), BB(2), UTG(3), HJ(4), CO(5).
                match dist {
                    0 => Position::Dealer,
                    1 => Position::SmallBlind,
                    2 => Position::BigBlind,
                    d if d == num - 1 => Position::Cutoff,
                    d if d == num - 2 => Position::Hijack,
                    3 => Position::Utg,
                    4 => Position::UtgPlus1,
                    5 => Position::UtgPlus2,
                    _ => Position::Lojack,
                }
            }
        }
    }
}
