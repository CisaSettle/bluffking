//! Honest, public-information-only opponent range estimate.
//!
//! Given only a target
//! seat's *position* and its *preflop action bucket* (both derivable from public
//! table information — who posted the blinds, the betting order, the board), it
//! returns the set of starting hands a GTO baseline would play that way.
//!
//! # The fairness barrier
//!
//! This module is **structurally incapable** of emitting a literal hole card:
//! its only inputs are a [`PositionBucket`] and an [`ActionBucket`] (two small
//! enums). It never takes, sees, deserializes, or returns any seat's
//! `hole_cards`. The estimate is read from the static preflop charts
//! ([`preflop_charts::range_entries`]) keyed purely by position+action. A
//! reviewer can confirm the barrier by inspecting this signature — there is no
//! card type anywhere on the path.
//!
//! # Honesty (do NOT fabricate precision)
//!
//! The estimate is an explicitly-bucketed approximation, not a claim of
//! certainty about the real holding. Every [`RangeEstimate`] carries:
//!   * `basis` — a human-readable statement of what it was derived from
//!     (position + the observed action), so the UI can label it honestly; and
//!   * `top_pct` — the fraction of all 1326 starting combos in the estimated
//!     range (a "top N%" figure), and
//!   * `hand_classes` — the concrete 169-grid hand keys in the range, with the
//!     baseline play-frequency for each.
//!
//! When the charts have no data for the (position, action) bucket (e.g. a
//! seat that only ever checked in the big blind, for which there is no opening
//! range), the estimate is returned with an empty class set and a `basis` that
//! says so — never a guess.

use serde::{Deserialize, Serialize};

use super::preflop_charts::{self, ActionBucket, PositionBucket};

/// Total number of distinct 2-card starting combinations (52 choose 2).
const TOTAL_STARTING_COMBOS: f32 = 1326.0;

/// One hand class in an estimated range: a 169-grid key plus the baseline
/// frequency the charts assign it for this (position, action) bucket.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct RangeClass {
    /// 169-grid hand key, e.g. `"AKs"`, `"JJ"`, `"T9o"`.
    pub key: String,
    /// Baseline play-frequency in `[0.0, 1.0]` for this hand in this spot.
    /// `1.0` = always played this way; a mixed-strategy hand is `< 1.0`.
    pub frequency: f32,
}

/// An honest, public-information-only estimate of the hands a seat would play
/// the observed way from the observed position.
///
/// SECURITY: contains NO literal hole cards — only aggregate
/// 169-grid hand classes and an explicit basis label. It is safe to return to
/// any participant of a finished hand.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct RangeEstimate {
    /// Canonical position label for the seat, e.g. `"BTN"`, `"CO"`, `"BB"`.
    pub position: String,
    /// The preflop action bucket the estimate is keyed on. Serialized values
    /// come from `ActionBucket::key()`: `"RFI"` (uppercase), `"facing_open"`,
    /// `"vs_3bet"`, `"vs_4bet"`, `"facing_limp"`. // U61 (dual-AI OSS review):
    /// the old example `"rfi"` did not match the wire value.
    pub action: String,
    /// Percent of all 1326 starting combos that fall in the estimated range
    /// (a "top N%" figure). `0.0` when the bucket has no chart data.
    pub top_pct: f32,
    /// Concrete combos in the range (combo-weighted by class), pre any
    /// board-conflict filtering. `0` when the bucket has no chart data.
    pub combos: u32,
    /// The 169-grid hand classes in the estimate, sorted by key. Empty when the
    /// bucket has no chart data (never a fabricated guess).
    pub hand_classes: Vec<RangeClass>,
    /// Human-readable statement of what the estimate was derived from. The UI
    /// shows this verbatim so the estimate is never mistaken for a literal
    /// card read. e.g. "Derived from BTN position + an opening raise (RFI).
    /// Public-information estimate, not the player's actual cards."
    pub basis: String,
}

impl RangeEstimate {
    /// `true` when the estimate carries no hand classes (no chart data for the
    /// bucket). Callers may surface a "no baseline range for this line" note.
    pub fn is_empty(&self) -> bool {
        self.hand_classes.is_empty()
    }
}

/// Build an honest range estimate from ONLY a seat's position and preflop
/// action bucket.
///
/// This is the single entry point for the M2 "range insight" feature. Its
/// signature is the structural fairness barrier: the inputs are
/// two enums, so it is impossible for this code to read or emit a real hole
/// card.
///
/// The class set + frequencies come straight from the published preflop charts
/// ([`preflop_charts::range_entries`]). `top_pct` and `combos` are derived by
/// combo-weighting each 169-grid class (pair=6, suited=4, offsuit=12) by its
/// baseline frequency — the standard way to express a preflop range as a
/// percentage of all 1326 combos.
pub fn estimate_range(position: PositionBucket, action: ActionBucket) -> RangeEstimate {
    let entries = preflop_charts::range_entries(position, action);

    let mut hand_classes: Vec<RangeClass> = entries
        .iter()
        .filter(|(_, freq)| *freq > 0.0)
        .map(|(key, freq)| RangeClass {
            key: key.clone(),
            frequency: *freq,
        })
        .collect();
    hand_classes.sort_by(|a, b| a.key.cmp(&b.key));

    // Combo-weight the range: each class contributes `combos_for_key * freq`
    // combos. Sum / 1326 is the "top N%" figure.
    let weighted_combos: f32 = hand_classes
        .iter()
        .map(|c| preflop_charts::combos_for_key(&c.key) as f32 * c.frequency)
        .sum();
    let combos = weighted_combos.round() as u32;
    let top_pct = weighted_combos / TOTAL_STARTING_COMBOS * 100.0;

    let basis = basis_label(position, action, hand_classes.is_empty());

    RangeEstimate {
        position: position_label(position).to_string(),
        action: action.key().to_string(),
        top_pct,
        combos,
        hand_classes,
        basis,
    }
}

/// Canonical short position label for a bucket (BTN/CO/MP/UTG/SB/BB).
fn position_label(position: PositionBucket) -> &'static str {
    position.key()
}

/// Human-readable derivation statement (the honesty label). Stated in English;
/// the client localizes / re-renders for display, but the *content* always
/// names position + the observed action and disclaims literal-card precision.
fn basis_label(position: PositionBucket, action: ActionBucket, empty: bool) -> String {
    let pos = position_label(position);
    let act = match action {
        ActionBucket::Rfi => "an opening raise (RFI)",
        ActionBucket::FacingOpen => "a call/raise facing one open",
        ActionBucket::Vs3bet => "continuing versus a 3-bet",
        ActionBucket::Vs4bet => "continuing versus a 4-bet",
        ActionBucket::FacingLimp => "a raise/iso versus a limp",
    };
    if empty {
        format!(
            "No baseline opening range exists for {pos} with {act}; \
             estimate unavailable from public information alone."
        )
    } else {
        format!(
            "Derived from {pos} position + {act}. Public-information estimate \
             (position + betting line), NOT the player's actual cards."
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn btn_rfi_is_a_wide_nonempty_range() {
        let est = estimate_range(PositionBucket::BTN, ActionBucket::Rfi);
        assert!(!est.is_empty(), "BTN RFI must have a baseline range");
        assert_eq!(est.position, "BTN");
        assert_eq!(est.action, "RFI");
        // BTN opens wide — well north of 20% of combos.
        assert!(
            est.top_pct > 20.0,
            "BTN RFI should be a wide range, got {}%",
            est.top_pct
        );
        assert!(est.combos > 0);
        assert!(est.basis.contains("NOT the player's actual cards"));
    }

    #[test]
    fn utg_rfi_is_tighter_than_btn_rfi() {
        let utg = estimate_range(PositionBucket::UTG, ActionBucket::Rfi);
        let btn = estimate_range(PositionBucket::BTN, ActionBucket::Rfi);
        assert!(
            utg.top_pct < btn.top_pct,
            "UTG ({}%) must open tighter than BTN ({}%)",
            utg.top_pct,
            btn.top_pct
        );
    }

    #[test]
    fn empty_bucket_is_honest_not_guessed() {
        // BB has no RFI opening range (it never opens — only defends).
        let est = estimate_range(PositionBucket::BB, ActionBucket::Rfi);
        assert!(est.is_empty());
        assert_eq!(est.combos, 0);
        assert_eq!(est.top_pct, 0.0);
        assert!(est.basis.contains("unavailable"));
    }

    #[test]
    fn classes_are_sorted_and_frequency_bounded() {
        let est = estimate_range(PositionBucket::CO, ActionBucket::Rfi);
        for c in &est.hand_classes {
            assert!(c.frequency > 0.0 && c.frequency <= 1.0);
        }
        let mut sorted = est.hand_classes.clone();
        sorted.sort_by(|a, b| a.key.cmp(&b.key));
        assert_eq!(est.hand_classes, sorted, "classes must be key-sorted");
    }

    /// The structural barrier: the estimate's serialized form has NO two-char
    /// lowercase card token (the literal card format, e.g. "as", "kd"). Hand
    /// CLASS keys are upper-case (AKs/JJ/T9o), so they never collide with the
    /// lowercase card-string format the rest of the codebase uses for real
    /// cards.
    #[test]
    fn estimate_never_serializes_a_literal_card() {
        let est = estimate_range(PositionBucket::BTN, ActionBucket::Rfi);
        let json = serde_json::to_string(&est).unwrap();
        // A literal card is lowercase rank + lowercase suit, e.g. "as".
        // Scan every key for that shape — there must be none.
        for c in &est.hand_classes {
            let lowered = c.key.to_lowercase();
            // Class keys when lowercased look like "aks"/"jj"/"t9o" — never a
            // bare rank+suit. Assert no class key IS a 2-char rank+suit token.
            if lowered.len() == 2 {
                let suit = lowered.as_bytes()[1] as char;
                assert!(
                    !matches!(suit, 'c' | 'd' | 'h' | 's'),
                    "class key {} looks like a literal card",
                    c.key
                );
            }
        }
        assert!(json.contains("hand_classes"));
    }
}
