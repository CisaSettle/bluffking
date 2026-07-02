//! Static preflop range lookups (ADR-043 §3.2).
//!
//! Data sourced from `engine/data/preflop_v2.json`, embedded at compile time
//! via `include_str!` and parsed once on first use (`OnceLock`).

use std::collections::HashMap;
use std::sync::OnceLock;

use serde::Deserialize;

use crate::card::Rank;
use crate::hand::HoleCards;

/// Embedded preflop chart JSON. Bump filename + `PREFLOP_V` if you edit
/// this file (ADR-043 §3.2 versioning rule).
///
/// Version state (U45, dual-AI OSS review — keep in sync when bumping):
/// the file is `preflop_v2.json` (filename kept for include stability), its
/// embedded `"version"` field is **3** (CFR approx.-equilibrium regen with
/// MIXED frequencies; v1/v2 were binary), and the solver-output version
/// constant `super::PREFLOP_V` is **4**. History: v2 added the `facing_open`
/// action bucket — distinct from `RFI` (no opens yet) per audit fix B-2
/// (R4 ADR-043 audit, 2026-05-23); v3 replaced the hand-authored binary
/// charts with the CFR-generated mixed-frequency charts.
const PREFLOP_V2_JSON: &str = include_str!("../../data/preflop_v2.json");

/// One chart cell: the recommended preflop action and the published mix
/// frequency (JSON v3 emits genuinely mixed values; v1/v2 were binary).
#[derive(Debug, Clone, Copy)]
pub struct ChartCell {
    /// Published mix frequency in `[0.0, 1.0]`. Mixed since JSON v3
    /// (v1/v2 were binary {0.0, 1.0}). // U45 (dual-AI OSS review)
    pub frequency: f32,
}

#[derive(Debug, Deserialize)]
struct RawCharts {
    #[serde(default, rename = "version")]
    _version: u32,
    #[serde(default, rename = "table_size")]
    _table_size: String,
    #[serde(default, rename = "stack_bucket_bb")]
    _stack_bucket_bb: u32,
    ranges: HashMap<String, HashMap<String, HashMap<String, f32>>>,
}

static CHARTS: OnceLock<RawCharts> = OnceLock::new();

fn load() -> &'static RawCharts {
    CHARTS.get_or_init(|| {
        serde_json::from_str(PREFLOP_V2_JSON)
            .expect("preflop_v2.json must parse; this is a build-time invariant")
    })
}

/// 169-grid key for a starting hand. Lower-case "s"/"o" suffix matches
/// the JSON keys (e.g. "AKs", "JJ", "T9o").
pub type HandKey = String;

/// Position bucket (6max).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PositionBucket {
    UTG,
    MP,
    CO,
    BTN,
    SB,
    BB,
}

impl PositionBucket {
    pub fn key(&self) -> &'static str {
        match self {
            PositionBucket::UTG => "UTG",
            PositionBucket::MP => "MP",
            PositionBucket::CO => "CO",
            PositionBucket::BTN => "BTN",
            PositionBucket::SB => "SB",
            PositionBucket::BB => "BB",
        }
    }

    /// Map back to a representative engine `Position` (inverse of
    /// [`position_to_bucket`]; `MP` → `Hijack`). Used by the spot analyzer to
    /// reuse the advisor's `Position`-keyed rule pipeline (M1 §1b).
    pub fn to_position(&self) -> crate::player::Position {
        use crate::player::Position;
        match self {
            PositionBucket::UTG => Position::Utg,
            PositionBucket::MP => Position::Hijack,
            PositionBucket::CO => Position::Cutoff,
            PositionBucket::BTN => Position::Dealer,
            PositionBucket::SB => Position::SmallBlind,
            PositionBucket::BB => Position::BigBlind,
        }
    }
}

/// Preflop action bucket.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ActionBucket {
    Rfi,
    /// One opener ahead, no 3-bet yet (preflop_v2 — audit fix B-2).
    FacingOpen,
    Vs3bet,
    Vs4bet,
    FacingLimp,
}

impl ActionBucket {
    pub fn key(&self) -> &'static str {
        match self {
            ActionBucket::Rfi => "RFI",
            ActionBucket::FacingOpen => "facing_open",
            ActionBucket::Vs3bet => "vs_3bet",
            ActionBucket::Vs4bet => "vs_4bet",
            ActionBucket::FacingLimp => "facing_limp",
        }
    }
}

/// Compute the 169-grid hand key for a pair of hole cards.
///
/// - Pair: `"AA"`, `"22"`.
/// - Suited: higher rank first + "s" (e.g. `"AKs"`).
/// - Offsuit: higher rank first + "o" (e.g. `"AKo"`).
pub fn hand_key(hole: HoleCards) -> HandKey {
    let (r1, r2) = (hole.card1.rank, hole.card2.rank);
    let (high, low) = if r1 >= r2 { (r1, r2) } else { (r2, r1) };
    let high_ch = high.char();
    let low_ch = low.char();
    if r1 == r2 {
        format!("{high_ch}{low_ch}")
    } else if hole.card1.suit == hole.card2.suit {
        format!("{high_ch}{low_ch}s")
    } else {
        format!("{high_ch}{low_ch}o")
    }
}

/// Look up the (position, action_bucket, hand_key) triple in the chart.
///
/// Returns `None` if the entry is absent (treat as fold per ADR-043 §3.4.1
/// rule 3 / fallthrough).
pub fn lookup(position: PositionBucket, action: ActionBucket, key: &str) -> Option<ChartCell> {
    let charts = load();
    let pos_map = charts.ranges.get(position.key())?;
    let action_map = pos_map.get(action.key())?;
    let freq = action_map.get(key).copied()?;
    Some(ChartCell { frequency: freq })
}

/// Enumerate all 169 canonical hand keys, in a stable order: pairs first
/// (AA..22), then suited (AKs..32s), then offsuit (AKo..32o). High rank first
/// within each non-pair key. Used by the solver grid (M1 §1b).
pub fn all_hand_keys() -> Vec<HandKey> {
    // Ranks high→low for human-grid order.
    let ranks: Vec<Rank> = Rank::ALL.iter().rev().copied().collect();
    let mut keys = Vec::with_capacity(169);
    // Pairs.
    for &r in &ranks {
        keys.push(format!("{0}{0}", r.char()));
    }
    // Suited.
    for i in 0..ranks.len() {
        for j in (i + 1)..ranks.len() {
            keys.push(format!("{}{}s", ranks[i].char(), ranks[j].char()));
        }
    }
    // Offsuit.
    for i in 0..ranks.len() {
        for j in (i + 1)..ranks.len() {
            keys.push(format!("{}{}o", ranks[i].char(), ranks[j].char()));
        }
    }
    keys
}

/// Number of concrete 2-card combos for a 169-grid key: pair=6, suited=4,
/// offsuit=12. Garbage keys (no `s`/`o` suffix, wrong length) → 0.
pub fn combos_for_key(key: &str) -> u32 {
    let bytes = key.as_bytes();
    match bytes.len() {
        2 if bytes[0] == bytes[1] => 6, // pair, e.g. "AA"
        3 if bytes[2] == b's' => 4,     // suited
        3 if bytes[2] == b'o' => 12,    // offsuit
        _ => 0,
    }
}

/// All `(hand_key, frequency)` entries present in the chart for a
/// `(position, action)` bucket. Absent entries (folds) are NOT included.
/// Empty `Vec` when the bucket has no chart data (e.g. UTG facing_open).
pub fn range_entries(position: PositionBucket, action: ActionBucket) -> Vec<(HandKey, f32)> {
    let charts = load();
    let Some(pos_map) = charts.ranges.get(position.key()) else {
        return Vec::new();
    };
    let Some(action_map) = pos_map.get(action.key()) else {
        return Vec::new();
    };
    let mut out: Vec<(HandKey, f32)> = action_map.iter().map(|(k, v)| (k.clone(), *v)).collect();
    // Stable order so callers (combos sum, RangeBucket build) are deterministic.
    out.sort_by(|a, b| a.0.cmp(&b.0));
    out
}

/// Map a `Position` from the engine to a 6-max `PositionBucket`.
///
/// HJ / LJ / UTG+1 / UTG+2 fold into `MP` for v1 (6-max only — ADR-043 §3.2,
/// §10 future work).
pub fn position_to_bucket(position: crate::player::Position) -> PositionBucket {
    use crate::player::Position::*;
    match position {
        Utg => PositionBucket::UTG,
        UtgPlus1 | UtgPlus2 | Lojack => PositionBucket::MP,
        Hijack => PositionBucket::MP,
        Cutoff => PositionBucket::CO,
        Dealer => PositionBucket::BTN,
        SmallBlind => PositionBucket::SB,
        BigBlind => PositionBucket::BB,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::card::{Card, Rank, Suit};

    fn cards(r1: Rank, s1: Suit, r2: Rank, s2: Suit) -> HoleCards {
        HoleCards::new(Card::new(r1, s1), Card::new(r2, s2))
    }

    #[test]
    fn hand_key_pair() {
        let k = hand_key(cards(Rank::Ace, Suit::Spades, Rank::Ace, Suit::Hearts));
        assert_eq!(k, "AA");
    }

    #[test]
    fn hand_key_suited() {
        let k = hand_key(cards(Rank::Ace, Suit::Spades, Rank::King, Suit::Spades));
        assert_eq!(k, "AKs");
    }

    #[test]
    fn hand_key_offsuit() {
        let k = hand_key(cards(Rank::Ace, Suit::Spades, Rank::King, Suit::Hearts));
        assert_eq!(k, "AKo");
    }

    #[test]
    fn hand_key_orders_higher_first() {
        let k = hand_key(cards(
            Rank::Three,
            Suit::Diamonds,
            Rank::Ace,
            Suit::Diamonds,
        ));
        assert_eq!(k, "A3s");
    }

    #[test]
    fn aa_in_utg_rfi() {
        let cell = lookup(PositionBucket::UTG, ActionBucket::Rfi, "AA").unwrap();
        assert!(cell.frequency >= 0.5, "AA must be a UTG RFI hand");
    }

    #[test]
    fn lookup_unknown_hand_returns_none() {
        // 72o is never an open from UTG.
        assert!(lookup(PositionBucket::UTG, ActionBucket::Rfi, "72o").is_none());
    }

    #[test]
    fn aa_in_vs_4bet_continues() {
        let cell = lookup(PositionBucket::BTN, ActionBucket::Vs4bet, "AA").unwrap();
        assert!(cell.frequency >= 0.5);
    }

    #[test]
    fn bb_rfi_is_empty() {
        assert!(lookup(PositionBucket::BB, ActionBucket::Rfi, "AA").is_none());
    }
}
