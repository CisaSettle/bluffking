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

/// Chip count (unsigned 32-bit, matches `INT UNSIGNED` in MySQL).
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
    /// // Active seats [0,1,2,3,4,5], dealer=0, asking about seat 2.
    /// let pos = Position::for_seat(2, 0, &[0, 1, 2, 3, 4, 5]);
    /// // 2 steps after dealer in 6 seats → UTG+1 (dist=2 → BB, not UTG+1).
    /// // Actually dist=2 → BigBlind in 6-max.
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
                // 5-handed: BTN, SB, BB, UTG, HJ.
                match dist {
                    0 => Position::Dealer,
                    1 => Position::SmallBlind,
                    2 => Position::BigBlind,
                    3 => Position::Utg,
                    _ => Position::Hijack,
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

#[cfg(test)]
mod position_tests {
    use super::*;

    /// ADR-026 §2: all 9 Position variants must return the correct canonical
    /// English short label. Replay client and coach depend on this mapping.
    #[test]
    fn position_short_label_all_variants() {
        assert_eq!(Position::Utg.short_label(), "UTG");
        assert_eq!(Position::UtgPlus1.short_label(), "UTG+1");
        assert_eq!(Position::UtgPlus2.short_label(), "UTG+2");
        assert_eq!(Position::Lojack.short_label(), "LJ");
        assert_eq!(Position::Hijack.short_label(), "HJ");
        assert_eq!(Position::Cutoff.short_label(), "CO");
        assert_eq!(Position::Dealer.short_label(), "BTN");
        assert_eq!(Position::SmallBlind.short_label(), "SB");
        assert_eq!(Position::BigBlind.short_label(), "BB");
    }

    /// P1-2 regression: 9-handed table uses GTO Wizard / PioSolver canonical labels.
    /// UTG, UTG+1, UTG+2, LJ, HJ, CO, BTN, SB, BB — no "MP".
    #[test]
    fn position_labels_9max_use_lojack_not_mp() {
        // 9 seats [0..8], dealer=0.
        let active: Vec<u8> = (0..9).collect();
        let dealer = 0u8;
        let labels: std::collections::HashSet<String> = active
            .iter()
            .map(|&s| {
                Position::for_seat(s, dealer, &active)
                    .short_label()
                    .to_string()
            })
            .collect();
        let expected: std::collections::HashSet<String> =
            ["UTG", "UTG+1", "UTG+2", "LJ", "HJ", "CO", "BTN", "SB", "BB"]
                .iter()
                .map(|s| s.to_string())
                .collect();
        assert_eq!(labels, expected, "9-max must use LJ not MP");
        assert!(!labels.contains("MP"), "MP must not appear in 9-max labels");
    }

    /// Issue 2 regression: non-contiguous active seats must not map two seats
    /// to the same position.
    #[test]
    fn non_contiguous_seats_no_duplicate_positions() {
        // 4-handed game, seat 2 busted → active seats [0, 1, 3], dealer=1.
        let active = [0u8, 1, 3];
        let dealer = 1u8;
        let positions: Vec<Position> = active
            .iter()
            .map(|&s| Position::for_seat(s, dealer, &active))
            .collect();
        // All positions must be distinct.
        let mut seen = std::collections::HashSet::new();
        for pos in &positions {
            assert!(
                seen.insert(*pos),
                "Duplicate position {:?} for seats {:?} with dealer={}",
                pos,
                active,
                dealer
            );
        }
        // Dealer seat (1) → BTN distance=0 → Dealer.
        assert_eq!(
            Position::for_seat(1, dealer, &active),
            Position::Dealer,
            "seat 1 is the dealer"
        );
        // Seat 3: rank after dealer (rank 1) → distance = (2 - 1 + 3) % 3 = 1 → SB.
        assert_eq!(
            Position::for_seat(3, dealer, &active),
            Position::SmallBlind,
            "seat 3 should be SB"
        );
        // Seat 0: distance = (0 - 1 + 3) % 3 = 2 → BB.
        assert_eq!(
            Position::for_seat(0, dealer, &active),
            Position::BigBlind,
            "seat 0 should be BB"
        );
    }

    /// Defensive (audit 2026-06-03): when `dealer_seat` is absent from
    /// `active_seats` (stale/busted button in a snapshot), `for_seat` must not
    /// silently treat the first active seat as the button and mislabel every
    /// position. It returns `Dealer` for the queried seat (mirroring the
    /// unknown-`seat` branch) instead of a phantom rank-0 distance.
    #[test]
    fn missing_dealer_seat_does_not_mislabel_off_phantom_button() {
        // active seats [0,1,2], dealer_seat=5 is not present.
        let active = [0u8, 1, 2];
        assert_eq!(
            Position::for_seat(1, 5, &active),
            Position::Dealer,
            "absent dealer must not produce a phantom rank-0 button labelling"
        );
    }

    #[test]
    fn contiguous_4_seats_correct_positions() {
        // Dealer=0: BTN=0, SB=1, BB=2, UTG=3.
        let active = [0u8, 1, 2, 3];
        assert_eq!(Position::for_seat(0, 0, &active), Position::Dealer);
        assert_eq!(Position::for_seat(1, 0, &active), Position::SmallBlind);
        assert_eq!(Position::for_seat(2, 0, &active), Position::BigBlind);
        assert_eq!(Position::for_seat(3, 0, &active), Position::Utg);
    }

    #[test]
    fn heads_up_dealer_is_sb() {
        let active = [0u8, 1];
        assert_eq!(Position::for_seat(0, 0, &active), Position::SmallBlind);
        assert_eq!(Position::for_seat(1, 0, &active), Position::BigBlind);
    }

    /// PP-P2-1 regression: 9-max position ORDER must be exactly
    /// UTG, UTG+1, UTG+2, LJ, HJ, CO, BTN, SB, BB (clockwise from BTN).
    ///
    /// This test verifies the strict ordering — not just the set membership —
    /// so that LJ is confirmed to sit between UTG+2 and HJ (dist=6 in 9-max).
    #[test]
    fn nine_max_position_order_includes_lj_between_utg2_and_hj() {
        // 9 seats [0..8], dealer=0.
        let active: Vec<u8> = (0..9).collect();
        let dealer = 0u8;

        // Expected order by distance from dealer (clockwise):
        // dist 0 → BTN, 1 → SB, 2 → BB, 3 → UTG, 4 → UTG+1, 5 → UTG+2,
        // dist 6 → LJ, 7 → HJ, 8 → CO.
        let expected_by_dist = ["BTN", "SB", "BB", "UTG", "UTG+1", "UTG+2", "LJ", "HJ", "CO"];

        for (dist, &expected_label) in expected_by_dist.iter().enumerate() {
            // seat at distance `dist` from dealer 0 is seat `dist` (since active=[0..8]).
            let seat = dist as u8;
            let pos = Position::for_seat(seat, dealer, &active);
            assert_eq!(
                pos.short_label(),
                expected_label,
                "9-max dist={dist}: expected {expected_label} but got {}",
                pos.short_label()
            );
        }

        // Explicitly verify LJ is at dist=6 (between UTG+2 at dist=5 and HJ at dist=7).
        assert_eq!(
            Position::for_seat(5, dealer, &active).short_label(),
            "UTG+2",
            "dist=5 must be UTG+2"
        );
        assert_eq!(
            Position::for_seat(6, dealer, &active).short_label(),
            "LJ",
            "dist=6 must be LJ (not MP or any other label)"
        );
        assert_eq!(
            Position::for_seat(7, dealer, &active).short_label(),
            "HJ",
            "dist=7 must be HJ"
        );
    }
}
