//! Ranges for bot.

use super::*;
use std::sync::OnceLock;

// ---------------------------------------------------------------------------
// Hand-strength classifiers
// ---------------------------------------------------------------------------

/// Rank 169 distinct starting hand-types by Chen-modified score, 0 = best (AA), 168 = worst (72o).
///
/// Builds a sorted hand-type table once via `OnceLock`, then a single lookup
/// per call. Pairs are stored with `suited=false` (a pair is neither suited
/// nor offsuit; the convention is arbitrary but consistent).
pub(super) fn starting_hand_rank_table() -> &'static [(Rank, Rank, bool); 169] {
    static TABLE: OnceLock<[(Rank, Rank, bool); 169]> = OnceLock::new();

    TABLE.get_or_init(|| {
        // Enumerate all 169 hand-types: 13 pairs + 78 suited + 78 offsuit.
        let mut all: Vec<(Rank, Rank, bool, i32)> = Vec::with_capacity(169);
        for i in 0..13usize {
            for j in 0..=i {
                let hh = Rank::ALL[i];
                let ll = Rank::ALL[j];
                if i == j {
                    all.push((hh, ll, false, chen_score_extended(hh, ll, false)));
                } else {
                    all.push((hh, ll, true, chen_score_extended(hh, ll, true)));
                    all.push((hh, ll, false, chen_score_extended(hh, ll, false)));
                }
            }
        }
        // Sort: highest score first; deterministic tie-break by (high rank desc,
        // low rank desc, suited > offsuit).
        all.sort_by(|a, b| {
            b.3.cmp(&a.3)
                .then_with(|| rank_index(b.0).cmp(&rank_index(a.0)))
                .then_with(|| rank_index(b.1).cmp(&rank_index(a.1)))
                .then_with(|| (b.2 as i32).cmp(&(a.2 as i32)))
        });
        let mut out = [(Rank::Two, Rank::Two, false); 169];
        for (idx, (h, l, s, _)) in all.into_iter().enumerate() {
            out[idx] = (h, l, s);
        }
        out
    })
}

pub(super) fn hand_rank_169(hole: HoleCards) -> u32 {
    let table = starting_hand_rank_table();
    let target = hand_class(hole);
    table
        .iter()
        .position(|&t| t == target)
        .map(|p| p as u32)
        .unwrap_or(168)
}

/// Starting-hand class of a holding: `(high, low, suited)`; pairs are stored
/// with `suited = false` (the table convention).
pub fn hand_class(hole: HoleCards) -> (Rank, Rank, bool) {
    let (h, l) = sorted_ranks(hole);
    let suited = h != l && hole.card1.suit == hole.card2.suit;
    (h, l, suited)
}

pub(super) fn class_combos((high, low, suited): (Rank, Rank, bool)) -> u32 {
    if high == low {
        6
    } else if suited {
        4
    } else {
        12
    }
}

/// Probability that `hole` belongs to the top `pct` of all 1,326 unordered
/// two-card combinations taken in `order`. Whole hand types are included in
/// order; only the final boundary type is mixed. Pairs contribute 6 combos,
/// suited non-pairs 4, and offsuit non-pairs 12.
pub(super) fn range_mix_in_order(order: &PriorityOrder, hole: HoleCards, pct: f64) -> f64 {
    class_mix_in_order(order, hand_class(hole), pct)
}

/// A priority order of the 169 hand classes with its per-class position and
/// the combinations ranked ahead of each position precomputed, so a class
/// lookup is O(1) instead of a scan (the range models ask once per class or
/// per combination, and nested residual lookups made a scan cubic).
#[derive(Debug, PartialEq)]
pub struct PriorityOrder {
    classes: [(Rank, Rank, bool); 169],
    /// Position of each `(high, low, suited)` slot; 168 for a slot that is
    /// not a hand class of the order.
    position: [u8; 13 * 13 * 2],
    /// Combinations of every class ranked before each position.
    prior_combos: [u32; 169],
}

impl PriorityOrder {
    pub(super) fn new(classes: [(Rank, Rank, bool); 169]) -> Self {
        let mut position = [168u8; 13 * 13 * 2];
        // Reverse so the first occurrence wins, as a forward scan would.
        for (index, &class) in classes.iter().enumerate().rev() {
            position[class_slot(class)] = index as u8;
        }
        let mut prior_combos = [0u32; 169];
        let mut combos = 0u32;
        for (prior, &class) in prior_combos.iter_mut().zip(&classes) {
            *prior = combos;
            combos += class_combos(class);
        }
        Self {
            classes,
            position,
            prior_combos,
        }
    }

    /// Index of `class` in the order (168 when absent).
    pub(super) fn position(&self, class: (Rank, Rank, bool)) -> usize {
        usize::from(self.position[class_slot(class)])
    }
}

impl std::ops::Deref for PriorityOrder {
    type Target = [(Rank, Rank, bool); 169];
    fn deref(&self) -> &Self::Target {
        &self.classes
    }
}

fn class_slot((high, low, suited): (Rank, Rank, bool)) -> usize {
    ((rank_index(high) * 13 + rank_index(low)) * 2) as usize + usize::from(suited)
}

// ---------------------------------------------------------------------------
// Position-specific raise-first-in priority orders (GTO-anchored pass L1)
// ---------------------------------------------------------------------------

/// Seat family used to pick the raise-first-in priority order.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OpenBucket {
    /// UTG, UTG+1, UTG+2, LJ (and the BB option after limps).
    Early = 0,
    Hijack = 1,
    Cutoff = 2,
    Button = 3,
    SmallBlind = 4,
}

impl OpenBucket {
    pub fn for_position(position: Position) -> OpenBucket {
        match position {
            Position::Utg
            | Position::UtgPlus1
            | Position::UtgPlus2
            | Position::Lojack
            | Position::BigBlind => OpenBucket::Early,
            Position::Hijack => OpenBucket::Hijack,
            Position::Cutoff => OpenBucket::Cutoff,
            Position::Dealer => OpenBucket::Button,
            Position::SmallBlind => OpenBucket::SmallBlind,
        }
    }
}

// Hand-authored standard-practice opening priorities (6-max, 100bb, no rake,
// 2.5bb/3bb opens), strongest first. The ClubRegular widths pick a prefix and
// mix exactly one boundary class (combo fraction in brackets): UTG 16% → ATo
// [0.85], HJ 21% → A9o [0.04], CO 27% → 64s [0.01], BTN 45% → K8o [0.89],
// SB 40% → T9o [0.70] (see `reference_widths_land_on_the_authored_boundary_classes`).
// Hand classes not listed follow in the position-blind Chen order, which only
// the widest caricatures (LAG/Maniac late seats) reach. These are hand-authored
// standard-practice orderings, not solver output: UTG keeps every suited ace and
// 44+ pure, mixes ATo, and opens J9s/98s before the smallest pairs 33/22.
pub(super) const RFI_PRIORITY_EARLY: &str = "AA KK QQ JJ TT AKs AKo AQs 99 AJs KQs AQo 88 ATs KJs 77 QJs KTs JTs AJo \
66 A9s QTs 55 A8s KQo A5s T9s A4s A7s A6s A3s A2s 44 K9s J9s 98s ATo 33 22 Q9s KJo 87s T8s 97s QJo 76s \
K8s A9o KTo J8s 65s Q8s T7s 86s K7s 54s QTo JTo K6s A8o K5s 75s A7o Q7s J7s K4s T6s 96s K3s K2s Q6s \
A6o A5o A4o K9o Q9o J9o T9o 64s 53s Q5s Q4s J6s 85s 74s A3o A2o K8o T8o 98o Q3s Q2s";
pub(super) const RFI_PRIORITY_HIJACK: &str = "AA KK QQ JJ TT AKs AKo AQs 99 AJs KQs AQo 88 ATs KJs 77 QJs KTs JTs AJo \
66 A9s QTs 55 A8s KQo A5s T9s 44 A4s A7s 33 A6s A3s 22 A2s K9s ATo J9s 98s 87s Q9s KJo T8s 76s 97s QJo \
K8s 65s A9o J8s KTo Q8s 54s T7s 86s K7s QTo JTo K6s 75s A8o K5s K4s 64s Q7s J7s T6s 96s K3s K2s A7o 53s \
Q6s A6o A5o A4o K9o Q9o J9o T9o Q5s Q4s J6s 85s 74s A3o A2o K8o T8o 98o Q3s Q2s";
pub(super) const RFI_PRIORITY_CUTOFF: &str = "AA KK QQ JJ TT AKs AKo AQs 99 AJs KQs AQo 88 ATs KJs 77 QJs KTs JTs AJo \
66 A9s QTs 55 A8s KQo A5s T9s 44 A4s A7s 33 A6s A3s 22 A2s K9s ATo J9s 98s 87s Q9s KJo T8s 76s 97s K8s \
QJo 65s J8s A9o KTo 54s Q8s T7s 86s K7s QTo JTo K6s 75s 64s A8o K5s K4s 53s Q7s J7s T6s 96s K3s K2s A7o \
Q6s A6o A5o 43s A4o K9o Q9o J9o T9o Q5s Q4s J6s 85s 74s A3o A2o K8o T8o 98o Q3s Q2s J5s 63s 87o";
pub(super) const RFI_PRIORITY_BUTTON: &str = "AA KK QQ JJ TT AKs AKo AQs 99 AJs KQs AQo 88 ATs KJs 77 QJs KTs JTs AJo \
66 A9s QTs 55 A8s KQo A5s T9s 44 A4s A7s 33 A6s A3s 22 A2s K9s ATo J9s 98s 87s Q9s KJo T8s 76s 97s K8s \
QJo 65s J8s 54s A9o KTo Q8s T7s 86s K7s 75s K6s 64s QTo JTo K5s K4s 53s A8o K3s K2s Q7s J7s T6s 96s 43s \
A7o Q6s 85s 74s A6o A5o A4o Q5s Q4s J6s K9o Q9o J9o T9o 63s A3o A2o Q3s Q2s J5s 95s 84s 73s K8o T8o 98o \
52s J4s 87o K7o Q8o J8o T7o 97o 42s 32s J3s J2s T5s 76o K6o";
pub(super) const RFI_PRIORITY_SMALL_BLIND: &str = "AA KK QQ JJ TT AKs AKo AQs 99 AJs KQs AQo 88 ATs KJs 77 QJs KTs JTs \
AJo 66 A9s QTs 55 A8s KQo A5s T9s 44 A4s A7s 33 A6s A3s 22 A2s K9s ATo J9s 98s 87s Q9s KJo T8s 76s 97s \
K8s QJo 65s J8s 54s A9o KTo Q8s T7s 86s K7s 75s K6s 64s QTo JTo K5s K4s 53s A8o K3s K2s Q7s J7s T6s 96s \
43s A7o Q6s 85s 74s A6o A5o A4o Q5s Q4s J6s K9o Q9o J9o T9o 63s A3o A2o Q3s Q2s J5s 95s 84s 73s K8o T8o \
98o";

pub(super) fn build_rfi_order(authored: &str) -> [(Rank, Rank, bool); 169] {
    let mut out: Vec<(Rank, Rank, bool)> = Vec::with_capacity(169);
    for token in authored.split_whitespace() {
        let class = engine::card::parse_hand_class(token)
            .unwrap_or_else(|| panic!("invalid RFI priority token {token}"));
        assert!(
            !out.contains(&class),
            "duplicate RFI priority token {token}"
        );
        out.push(class);
    }
    for &class in starting_hand_rank_table().iter() {
        if !out.contains(&class) {
            out.push(class);
        }
    }
    out.try_into()
        .unwrap_or_else(|v: Vec<_>| panic!("RFI order has {} classes, expected 169", v.len()))
}

/// Raise-first-in priority order for a seat family: the authored prefix
/// followed by every remaining class in Chen order. Built once per process.
pub fn rfi_priority_order(bucket: OpenBucket) -> &'static PriorityOrder {
    static TABLES: OnceLock<[PriorityOrder; 5]> = OnceLock::new();
    &TABLES.get_or_init(|| {
        [
            RFI_PRIORITY_EARLY,
            RFI_PRIORITY_HIJACK,
            RFI_PRIORITY_CUTOFF,
            RFI_PRIORITY_BUTTON,
            RFI_PRIORITY_SMALL_BLIND,
        ]
        .map(|authored| PriorityOrder::new(build_rfi_order(authored)))
    })[bucket as usize]
}

/// Probability that `hole` opens from `bucket` with a width of `pct` percent
/// of all 1,326 combinations (boundary class mixed exactly).
pub fn open_range_mix(hole: HoleCards, bucket: OpenBucket, pct: f64) -> f64 {
    range_mix_in_order(rfi_priority_order(bucket), hole, pct)
}

/// Same as [`range_mix_in_order`] for a hand class instead of a holding.
pub(super) fn class_mix_in_order(
    order: &PriorityOrder,
    class: (Rank, Rank, bool),
    pct: f64,
) -> f64 {
    let rank = order.position(class);
    let prior_combos = order.prior_combos[rank];
    let current_combos = class_combos(order[rank]);
    let target_combos = 1_326.0 * pct.clamp(0.0, 100.0) / 100.0;
    ((target_combos - prior_combos as f64) / current_combos as f64).clamp(0.0, 1.0)
}

/// [`range_mix_in_order`] where each class only brings `residual(class)` of
/// its combos (the probability mass not already consumed by an earlier
/// decision). Returns the conditional probability of selecting `hole` given
/// that earlier decision did not fire, so the aggregate frequency over all
/// 1,326 combos is exactly `pct`.
pub(super) fn range_mix_in_order_residual(
    order: &PriorityOrder,
    hole: HoleCards,
    pct: f64,
    residual: impl Fn((Rank, Rank, bool)) -> f64,
) -> f64 {
    let target = hand_class(hole);
    let own_residual = residual(target).clamp(0.0, 1.0);
    if own_residual <= 0.0 {
        return 0.0;
    }
    let rank = order.position(target);
    let prior_combos: f64 = order[..rank]
        .iter()
        .copied()
        .map(|c| class_combos(c) as f64 * residual(c).clamp(0.0, 1.0))
        .sum();
    let current_combos = class_combos(order[rank]) as f64 * own_residual;
    let target_combos = 1_326.0 * pct.clamp(0.0, 100.0) / 100.0;
    ((target_combos - prior_combos) / current_combos).clamp(0.0, 1.0)
}

/// One representative holding for a hand class.
pub(super) fn class_sample_hole((high, low, suited): (Rank, Rank, bool)) -> HoleCards {
    use engine::{Card, Suit};
    let s2 = if suited { Suit::Spades } else { Suit::Hearts };
    HoleCards {
        card1: Card {
            rank: high,
            suit: Suit::Spades,
        },
        card2: Card {
            rank: low,
            suit: s2,
        },
    }
}

// ---------------------------------------------------------------------------
// Facing-a-raise priority orders (GTO-anchored pass L2)
// ---------------------------------------------------------------------------

// Polarised re-raise priority: premiums, then the best blockers/bluffs (suited
// wheel aces), then linear value, with suited connectors behind the Kx/Qx
// suited blockers so a 100bb squeeze stays value-heavy.
pub(super) const THREE_BET_ORDER: &str = "AA KK QQ AKs AKo JJ A5s A4s AQs TT KQs A3s A2s AJs 99 KJs AQo ATs QJs JTs \
88 T9s KTs 98s A9s A8s K9s Q9s 54s 76s 65s 87s AJo KQo 77 J9s A7s A6s 66 K8s Q8s J8s T8s 97s 86s 75s \
64s 55 44 33 22 ATo KJo QJo JTo K7s K6s K5s K4s K3s K2s Q7s J7s T7s 96s 85s 74s 63s 53s 43s A9o A8o \
KTo QTo T9o";
// Flat-call priority facing one open: playable pairs, suited broadways and
// connectors first, offsuit broadways and weak suited hands later.
pub(super) const CALL_ORDER: &str = "JJ TT 99 88 77 AQs AJs KQs ATs KJs QJs JTs T9s 66 55 98s 87s KTs QTs 76s A9s \
A8s A5s A4s 44 33 22 65s AQo AJo KQo A7s A6s A3s A2s J9s T8s 97s 54s K9s Q9s 86s 75s 64s ATo KJo QJo \
JTo K8s K7s K6s K5s K4s K3s K2s Q8s J8s 53s 43s 96s 85s 74s A9o A8o KTo QTo T9o Q7s Q6s Q5s Q4s Q3s \
Q2s J7s T7s A7o A6o A5o A4o J9o T8o 98o K9o Q9o 63s 52s 42s 32s J6s J5s J4s J3s J2s T6s T5s 95s 84s \
73s A3o A2o K8o K7o 87o 76o 65o";
// 4-bet bluff priority: suited wheel aces (ace blocker + wheel equity), then
// suited Kx/Qx blockers and the best suited connectors.
pub(super) const FOUR_BET_BLUFF_ORDER: &str =
    "A5s A4s A3s A2s K9s Q9s J9s T9s 98s 87s 76s 65s K8s KQo KJo Q8s J8s \
T8s 97s 86s 75s 54s";
// Flat-call priority facing a 3-bet: hands that play well deep and
// in position, premiums excluded because they 4-bet.
pub(super) const THREE_BET_CALL_ORDER: &str =
    "JJ TT 99 AQs KQs AJs AQo 88 KJs QJs JTs T9s ATs 77 98s 66 87s A5s \
A4s 55 KQo 44 33 22 76s 65s A9s KTs QTs A3s A2s J9s T8s 97s 54s";

pub(super) fn build_priority_order(authored: &str) -> [(Rank, Rank, bool); 169] {
    build_rfi_order(authored)
}

pub(super) fn three_bet_order() -> &'static PriorityOrder {
    static T: OnceLock<PriorityOrder> = OnceLock::new();
    T.get_or_init(|| PriorityOrder::new(build_priority_order(THREE_BET_ORDER)))
}

pub(super) fn call_order() -> &'static PriorityOrder {
    static T: OnceLock<PriorityOrder> = OnceLock::new();
    T.get_or_init(|| PriorityOrder::new(build_priority_order(CALL_ORDER)))
}

pub(super) fn four_bet_bluff_order() -> &'static PriorityOrder {
    static T: OnceLock<PriorityOrder> = OnceLock::new();
    T.get_or_init(|| PriorityOrder::new(build_priority_order(FOUR_BET_BLUFF_ORDER)))
}

pub(super) fn three_bet_call_order() -> &'static PriorityOrder {
    static T: OnceLock<PriorityOrder> = OnceLock::new();
    T.get_or_init(|| PriorityOrder::new(build_priority_order(THREE_BET_CALL_ORDER)))
}

/// Extended Chen score that gives a continuous ranking.
pub(super) fn chen_score_extended(h: Rank, l: Rank, suited: bool) -> i32 {
    let mut score = rank_chen_value(h);

    if h == l {
        score = score.max(5) * 2;
        return score;
    }

    let h_idx = rank_index(h);
    let l_idx = rank_index(l);
    let gap = h_idx - l_idx - 1;
    score -= match gap {
        0 => 0,
        1 => 1,
        2 => 2,
        3 => 4,
        _ => 5,
    };

    if gap <= 1 && h <= Rank::Queen {
        score += 1;
    }

    if suited {
        score += 2;
    }

    score
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub(super) enum PreflopStrength {
    Weak,
    Bluff3bet,
    Strong,
    Premium,
}

/// Classify preflop strength for facing-open decisions (ADR-024 §4.1.2).
pub(super) fn preflop_hand_strength(hole: HoleCards, pos: Position) -> PreflopStrength {
    let (h, l) = sorted_ranks(hole);
    let suited = hole.card1.suit == hole.card2.suit;
    let pair = h == l;

    // Premium: JJ+, AKs, AKo.
    if pair && h >= Rank::Jack {
        return PreflopStrength::Premium;
    }
    if h == Rank::Ace && l == Rank::King {
        return PreflopStrength::Premium;
    }

    // Bluff 3-bet: A5s/A4s/KQs (polarised 3-bet range).
    if h == Rank::Ace && (l == Rank::Five || l == Rank::Four) && suited {
        return PreflopStrength::Bluff3bet;
    }
    if h == Rank::King && l == Rank::Queen && suited {
        return PreflopStrength::Bluff3bet;
    }

    // Strong: pairs 22-TT, KQ-AT broadways, suited connectors 76s+, suited Axs.
    if pair {
        return PreflopStrength::Strong;
    }
    // AXs (any ace suited).
    if h == Rank::Ace && suited {
        return PreflopStrength::Strong;
    }
    // Broadways: KQ, KJ, QJ, JT, AT.
    let h_idx = rank_index(h);
    let l_idx = rank_index(l);
    if h_idx >= rank_index(Rank::Jack) && l_idx >= rank_index(Rank::Ten) {
        return PreflopStrength::Strong;
    }
    // ATo, KQo.
    if h == Rank::Ace && l == Rank::Ten {
        return PreflopStrength::Strong;
    }
    if h == Rank::King && l == Rank::Queen {
        return PreflopStrength::Strong;
    }
    // Suited connectors 76s+.
    if suited && h_idx - l_idx == 1 && l_idx >= rank_index(Rank::Six) {
        return PreflopStrength::Strong;
    }

    // Tighten call range out of position.
    if matches!(
        pos,
        Position::SmallBlind | Position::BigBlind | Position::Utg
    ) {
        return PreflopStrength::Weak;
    }

    // Weak by default.
    PreflopStrength::Weak
}
