//! Categorical hand-strength classifier (ADR-043 §3.3).
//!
//! Pure function `classify(hole, board) -> HandStrength`. No equity, no
//! opponent context — strictly the hero's made hand + draws.

use crate::card::{Card, Rank, Suit};
use crate::hand::{BoardCards, HoleCards};

/// 13 ordered hand-strength bands. `Ord` follows the value-of-hand
/// progression: a stronger hand is "greater".
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, serde::Serialize)]
#[serde(rename_all = "snake_case")]
pub enum HandStrength {
    /// No pair, no draws — pure bluff candidate.
    PureBluffNoEquity,
    /// Gutshot or backdoor flush / straight.
    DrawWeak,
    /// Open-ended straight, flush draw, or combo draw (≥ 8 outs).
    DrawStrong,
    /// Pair below middle pair (incl. underpair to board).
    PairWeak,
    /// Middle pair on board.
    PairMiddle,
    /// Top pair with a weak kicker (≤ T, or A on non-A board).
    PairTopWeakKicker,
    /// Top pair with a strong kicker (J+, or A on A-high board).
    PairTopStrongKicker,
    /// Pocket pair higher than the top board card.
    Overpair,
    /// Two pair (any combination).
    TwoPair,
    /// Three of a kind via pocket pair + matching board card.
    Set,
    /// Five-card straight.
    Straight,
    /// Five cards same suit.
    Flush,
    /// Full house, quads, or straight flush.
    FullHousePlus,
}

impl HandStrength {
    /// All 13 variants, in canonical ascending order. Used by tests + by
    /// the `templates_zh` exhaustive check.
    pub fn all() -> [HandStrength; 13] {
        [
            HandStrength::PureBluffNoEquity,
            HandStrength::DrawWeak,
            HandStrength::DrawStrong,
            HandStrength::PairWeak,
            HandStrength::PairMiddle,
            HandStrength::PairTopWeakKicker,
            HandStrength::PairTopStrongKicker,
            HandStrength::Overpair,
            HandStrength::TwoPair,
            HandStrength::Set,
            HandStrength::Straight,
            HandStrength::Flush,
            HandStrength::FullHousePlus,
        ]
    }
}

/// Classify the hero hand on the given board.
///
/// Preflop (`board` is empty): returns a coarse pocket-pair classification —
/// `Overpair` for JJ+, `PairMiddle` for 77–TT, `PairWeak` for ≤66 and any
/// unpaired hand. Templates can still reference these; advisor uses preflop
/// charts as the primary signal.
pub fn classify(hole: HoleCards, board: &BoardCards) -> HandStrength {
    let board_cards = board.all_cards();

    // Preflop coarse classification.
    if board_cards.is_empty() {
        if hole.card1.rank == hole.card2.rank {
            let r = hole.card1.rank;
            return if r >= Rank::Jack {
                HandStrength::Overpair
            } else if r >= Rank::Seven {
                HandStrength::PairMiddle
            } else {
                HandStrength::PairWeak
            };
        }
        return HandStrength::PairWeak;
    }

    let hole_cards = [hole.card1, hole.card2];
    let all_cards: Vec<Card> = hole_cards
        .iter()
        .chain(board_cards.iter())
        .copied()
        .collect();

    // ---- Detect made hands top-down ----

    // `is_full_house_plus` / `is_two_pair` count pairs/trips across the hole+board
    // union with no requirement that hero's cards improve the PAIR STRUCTURE. On a
    // paired / double-paired board a hero whose cards don't strengthen the boat /
    // top-two pairs merely "plays the board" (near-zero relative equity — any
    // opponent card that re-pairs the board makes a hand that dominates it), yet
    // was labelled FullHousePlus / TwoPair. Compare hero+board against the board
    // alone: credit the strong band only when hero strengthens the pair structure
    // (a higher boat, or a higher top-two pair — a bare pocket pair below the
    // board's pairs, or a mere kicker, does NOT count); otherwise → `PairWeak`
    // (weak dominated made hand, still has chop value, so not a pure bluff).
    // `is_set` / `classify_pair` already require a hero contribution.
    // OSS dual-AI review 2026-07-01 (finding C1).
    let combined_counts = rank_counts(&all_cards);
    let board_counts = rank_counts(&board_cards);
    if is_full_house_plus(&all_cards) {
        // Quads and straight flushes are near-nut even when hero "plays the board":
        // unlike a plain full house (which any opponent card re-pairing the board
        // beats), they are not dominated, so they always keep the strong band —
        // this also covers hero MAKING quads on a full-house board (e.g. Kc7d on
        // KsKhKdQcQs). Otherwise it is a plain full house: credit hero only when
        // hero's boat is strictly better than the board's own boat.
        let near_nut = is_straight_flush(&all_cards) || combined_counts.iter().any(|&n| n >= 4);
        let hero_improves = near_nut
            || match (
                full_house_key(&combined_counts),
                full_house_key(&board_counts),
            ) {
                (Some(hero), Some(board_alone)) => hero > board_alone,
                (Some(_), None) => true,
                (None, _) => true,
            };
        return if hero_improves {
            HandStrength::FullHousePlus
        } else {
            HandStrength::PairWeak
        };
    }
    if is_flush(&all_cards) {
        return HandStrength::Flush;
    }
    if is_straight(&all_cards) {
        return HandStrength::Straight;
    }
    if is_set(hole, &board_cards) {
        return HandStrength::Set;
    }
    if is_two_pair(&all_cards) {
        let hero_improves = match (
            top_two_pair_ranks(&combined_counts),
            top_two_pair_ranks(&board_counts),
        ) {
            (Some(hero), Some(board_alone)) => hero > board_alone,
            (Some(_), None) => true,
            (None, _) => false,
        };
        return if hero_improves {
            HandStrength::TwoPair
        } else {
            HandStrength::PairWeak
        };
    }

    // Pair classification.
    if let Some(pair_kind) = classify_pair(hole, &board_cards) {
        return pair_kind;
    }

    // ---- Draws (no made hand at least a pair) ----
    let board_suits = board_cards.iter().map(|c| c.suit).collect::<Vec<_>>();
    let hole_suits = [hole.card1.suit, hole.card2.suit];
    let flush_draw = has_flush_draw(&hole_suits, &board_suits);

    let oesd = has_open_ended_straight_draw(&all_cards);
    let gutshot = !oesd && has_gutshot(&all_cards);

    if flush_draw || oesd {
        return HandStrength::DrawStrong;
    }
    if gutshot || has_backdoor_flush(&hole_suits, &board_suits) {
        return HandStrength::DrawWeak;
    }

    HandStrength::PureBluffNoEquity
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

fn rank_counts(cards: &[Card]) -> [u8; 13] {
    let mut counts = [0u8; 13];
    for c in cards {
        counts[c.rank as usize] += 1;
    }
    counts
}

fn suit_counts(cards: &[Card]) -> [u8; 4] {
    let mut counts = [0u8; 4];
    for c in cards {
        let i = match c.suit {
            Suit::Spades => 0,
            Suit::Hearts => 1,
            Suit::Diamonds => 2,
            Suit::Clubs => 3,
        };
        counts[i] += 1;
    }
    counts
}

fn is_full_house_plus(cards: &[Card]) -> bool {
    let counts = rank_counts(cards);
    let mut four = false;
    let mut three_count = 0u8;
    let mut pair_count = 0u8;
    for c in counts.iter() {
        if *c >= 4 {
            four = true;
        }
        if *c >= 3 {
            three_count += 1;
        }
        if *c >= 2 {
            pair_count += 1;
        }
    }
    if four {
        return true;
    }
    if three_count >= 1 && pair_count >= 2 {
        return true;
    }
    if three_count >= 2 {
        // two trips ⇒ full house (use higher trip as trips, lower as pair).
        return true;
    }
    // Straight-flush detection delegated — handled holistically with rs_poker
    // in eval.rs. For HandStrength we only need FullHousePlus to cover boats,
    // quads, straight flushes; we approximate by also testing the combination.
    is_straight_flush(cards)
}

fn is_straight_flush(cards: &[Card]) -> bool {
    // Group by suit and check straight within each suited subset.
    for suit_label in Suit::ALL {
        let subset: Vec<Card> = cards
            .iter()
            .copied()
            .filter(|c| c.suit == suit_label)
            .collect();
        if subset.len() >= 5 && is_straight(&subset) {
            return true;
        }
    }
    false
}

fn is_flush(cards: &[Card]) -> bool {
    suit_counts(cards).iter().any(|c| *c >= 5)
}

fn is_straight(cards: &[Card]) -> bool {
    // Build a bitmask of ranks present.
    let mut bits: u16 = 0;
    for c in cards {
        bits |= 1u16 << (c.rank as u16);
    }
    // Wheel: A,2,3,4,5 — treat Ace as low.
    let wheel_mask: u16 = (1 << (Rank::Ace as u16))
        | (1 << (Rank::Two as u16))
        | (1 << (Rank::Three as u16))
        | (1 << (Rank::Four as u16))
        | (1 << (Rank::Five as u16));
    if bits & wheel_mask == wheel_mask {
        return true;
    }
    // 5-in-a-row anywhere.
    let mut run = bits;
    for _ in 0..4 {
        run &= run << 1;
    }
    run != 0
}

fn is_set(hole: HoleCards, board: &[Card]) -> bool {
    // Pocket-pair set: both hole cards plus a matching board card.
    if hole.card1.rank == hole.card2.rank && board.iter().any(|c| c.rank == hole.card1.rank) {
        return true;
    }
    // One-card trips: a single hole card pairs a board rank that is itself
    // already paired (e.g. K8 on a K-K-3 board → three Kings). This is genuine
    // three-of-a-kind using a hole card and belongs in the `Set` band, not in
    // `classify_pair` where it was understated as top-pair (audit 2026-06-03).
    for hc in [hole.card1, hole.card2] {
        let board_matches = board.iter().filter(|c| c.rank == hc.rank).count();
        if board_matches >= 2 {
            return true;
        }
    }
    false
}

fn is_two_pair(cards: &[Card]) -> bool {
    let counts = rank_counts(cards);
    counts.iter().filter(|c| **c >= 2).count() >= 2
}

/// The two highest paired ranks (count ≥ 2), highest first, if at least two
/// exist — i.e. the two-pair a hand makes. Comparing this for hole+board vs the
/// board alone tells us whether hero's cards strengthen the two pair or merely
/// play the board's (OSS dual-AI review 2026-07-01, finding C1).
fn top_two_pair_ranks(counts: &[u8; 13]) -> Option<(usize, usize)> {
    let pairs: Vec<usize> = (0..13).rev().filter(|&r| counts[r] >= 2).collect();
    (pairs.len() >= 2).then(|| (pairs[0], pairs[1]))
}

/// The `(trips_rank, pair_rank)` of the best full house makeable from `counts`
/// via pair counts (full house or quads; straight flushes are handled
/// separately). Highest trips, then the highest OTHER rank paired. `None` when
/// there is no full house. Comparing hole+board vs the board alone tells us
/// whether hero strengthens the boat or merely plays the board's.
fn full_house_key(counts: &[u8; 13]) -> Option<(usize, usize)> {
    let trip = (0..13).rev().find(|&r| counts[r] >= 3)?;
    let pair = (0..13).rev().find(|&r| r != trip && counts[r] >= 2)?;
    Some((trip, pair))
}

fn top_board_rank(board: &[Card]) -> Rank {
    board.iter().map(|c| c.rank).max().unwrap_or(Rank::Two)
}

fn classify_pair(hole: HoleCards, board: &[Card]) -> Option<HandStrength> {
    // Find any pair involving hero (pocket pair or pair with board).
    let hole_ranks = [hole.card1.rank, hole.card2.rank];
    let mut board_ranks: Vec<Rank> = board.iter().map(|c| c.rank).collect();
    let top = top_board_rank(board);

    // Pocket pair.
    if hole.card1.rank == hole.card2.rank {
        let pair_rank = hole.card1.rank;
        if pair_rank > top {
            return Some(HandStrength::Overpair);
        }
        // Underpair to board — treat as PairWeak.
        return Some(HandStrength::PairWeak);
    }

    // Look for any rank that matches between hole and board.
    let pair_rank = hole_ranks
        .iter()
        .copied()
        .find(|hr| board_ranks.iter().any(|br| br == hr));
    let pair_rank = pair_rank?;

    // Kicker is the OTHER hole card.
    let kicker = if hole_ranks[0] == pair_rank {
        hole_ranks[1]
    } else {
        hole_ranks[0]
    };

    if pair_rank == top {
        // Top pair.
        let strong_kicker = kicker >= Rank::Jack || (top == Rank::Ace && kicker == Rank::Ace);
        return Some(if strong_kicker {
            HandStrength::PairTopStrongKicker
        } else {
            HandStrength::PairTopWeakKicker
        });
    }

    // Second-highest rank on board = middle pair (for 3+ board cards).
    board_ranks.sort_by(|a, b| b.cmp(a));
    if board.len() >= 2 && pair_rank == board_ranks[1] {
        return Some(HandStrength::PairMiddle);
    }
    Some(HandStrength::PairWeak)
}

fn has_flush_draw(hole_suits: &[Suit; 2], board_suits: &[Suit]) -> bool {
    for suit in Suit::ALL {
        let hole_n = hole_suits.iter().filter(|s| **s == suit).count();
        let board_n = board_suits.iter().filter(|s| **s == suit).count();
        // Flush draw = exactly 4 of one suit, of which at least 1 is in hero's hole.
        if hole_n + board_n == 4 && hole_n >= 1 {
            return true;
        }
    }
    false
}

fn has_backdoor_flush(hole_suits: &[Suit; 2], board_suits: &[Suit]) -> bool {
    // Backdoor flush = exactly 3 of one suit on flop, hero holds at least one.
    if board_suits.len() != 3 {
        return false;
    }
    for suit in Suit::ALL {
        let hole_n = hole_suits.iter().filter(|s| **s == suit).count();
        let board_n = board_suits.iter().filter(|s| **s == suit).count();
        if hole_n + board_n == 3 && hole_n >= 1 {
            return true;
        }
    }
    false
}

fn has_open_ended_straight_draw(cards: &[Card]) -> bool {
    let mut bits: u16 = 0;
    for c in cards {
        bits |= 1u16 << (c.rank as u16);
    }
    // OESD = 4 consecutive ranks with potential to extend on both ends
    // (so not at the very top or bottom). We scan ranks 2..=K for a 4-run
    // and ensure both endpoints have room to extend.
    let low_idx = Rank::Two as u16;
    let high_idx = Rank::Ace as u16;
    let ace_present = bits & (1 << high_idx) != 0;
    for start in low_idx..=high_idx - 3 {
        let mask: u16 = (1 << start) | (1 << (start + 1)) | (1 << (start + 2)) | (1 << (start + 3));
        if bits & mask == mask {
            // OESD must have extension room on BOTH sides — not a one-ended
            // wheel/broadway run.
            //
            // Bottom edge 2-3-4-5 is genuinely two-ended despite `start ==
            // low_idx`: the Ace completes the wheel (A-2-3-4-5) on the low side
            // and a Six completes 2-3-4-5-6 on the high side, so it is a real
            // 8-out draw (audit 2026-06-03). Note an Ace already in `bits`
            // means the run is in fact A-2-3-4-5 (a made straight, caught by
            // `is_straight` upstream) rather than a draw — exclude that case.
            let has_low_ext = start > low_idx || (start == low_idx && !ace_present);
            let has_high_ext = start + 3 < high_idx;
            if has_low_ext && has_high_ext {
                return true;
            }
        }
    }
    false
}

fn has_gutshot(cards: &[Card]) -> bool {
    let mut bits: u16 = 0;
    for c in cards {
        bits |= 1u16 << (c.rank as u16);
    }
    let low_idx = Rank::Two as u16;
    let high_idx = Rank::Ace as u16;
    // Any 5-rank window with exactly 4 bits set is a gutshot.
    for start in low_idx..=high_idx - 4 {
        let window: u16 = (1 << start)
            | (1 << (start + 1))
            | (1 << (start + 2))
            | (1 << (start + 3))
            | (1 << (start + 4));
        let present = (bits & window).count_ones();
        if present == 4 {
            return true;
        }
    }
    // Wheel gutshot: A-2-3-4-5 with one missing.
    let wheel_ranks = [
        Rank::Ace as u16,
        Rank::Two as u16,
        Rank::Three as u16,
        Rank::Four as u16,
        Rank::Five as u16,
    ];
    let mut wheel_mask: u16 = 0;
    for r in wheel_ranks.iter() {
        wheel_mask |= 1 << r;
    }
    if (bits & wheel_mask).count_ones() == 4 {
        return true;
    }
    false
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::card::{Card, Rank, Suit};

    fn c(r: Rank, s: Suit) -> Card {
        Card::new(r, s)
    }

    fn b3(c1: Card, c2: Card, c3: Card) -> BoardCards {
        BoardCards {
            flop: Some([c1, c2, c3]),
            turn: None,
            river: None,
        }
    }

    fn b5(c1: Card, c2: Card, c3: Card, c4: Card, c5: Card) -> BoardCards {
        BoardCards {
            flop: Some([c1, c2, c3]),
            turn: Some(c4),
            river: Some(c5),
        }
    }

    fn h(c1: Card, c2: Card) -> HoleCards {
        HoleCards::new(c1, c2)
    }

    #[test]
    fn pure_bluff_classification() {
        // 72o on AK4 with no draws.
        let hole = h(c(Rank::Seven, Suit::Diamonds), c(Rank::Two, Suit::Hearts));
        let board = b3(
            c(Rank::Ace, Suit::Spades),
            c(Rank::King, Suit::Spades),
            c(Rank::Four, Suit::Clubs),
        );
        assert_eq!(classify(hole, &board), HandStrength::PureBluffNoEquity);
    }

    #[test]
    fn draw_weak_gutshot() {
        // 9J on T-K-4 — gutshot to Q only (needs 9-T-J-Q-K).
        // Ranks present: 4, 9, T, J, K. No 4-in-a-row, but 9-T-J-K is 4 of 5
        // in the 9..K window (only Q missing) → gutshot.
        let hole = h(c(Rank::Nine, Suit::Diamonds), c(Rank::Jack, Suit::Clubs));
        let board = b3(
            c(Rank::Ten, Suit::Hearts),
            c(Rank::King, Suit::Spades),
            c(Rank::Four, Suit::Diamonds),
        );
        let result = classify(hole, &board);
        assert!(
            matches!(result, HandStrength::DrawWeak),
            "expected DrawWeak (gutshot); got {:?}",
            result
        );
    }

    /// Regression (audit 2026-06-03): the bottom-edge 2-3-4-5 run is a genuine
    /// 8-out open-ended straight draw (completes with an Ace via the wheel, or a
    /// Six), so it must classify as `DrawStrong`, not be demoted to a gutshot
    /// (`DrawWeak`). Hero 54 on a 2-3-K board has exactly 2-3-4-5 with no Ace.
    #[test]
    fn bottom_edge_2345_is_open_ended_draw() {
        let hole = h(c(Rank::Five, Suit::Spades), c(Rank::Four, Suit::Hearts));
        let board = b3(
            c(Rank::Two, Suit::Diamonds),
            c(Rank::Three, Suit::Clubs),
            c(Rank::King, Suit::Hearts),
        );
        assert_eq!(
            classify(hole, &board),
            HandStrength::DrawStrong,
            "2-3-4-5 is an 8-out OESD (A or 6 completes), not a gutshot"
        );
    }

    #[test]
    fn draw_strong_flush_draw() {
        // AhKh on Jh-7c-2d — nut flush draw.
        let hole = h(c(Rank::Ace, Suit::Hearts), c(Rank::King, Suit::Hearts));
        let board = b3(
            c(Rank::Jack, Suit::Hearts),
            c(Rank::Seven, Suit::Clubs),
            c(Rank::Two, Suit::Hearts),
        );
        assert_eq!(classify(hole, &board), HandStrength::DrawStrong);
    }

    #[test]
    fn pair_weak_underpair() {
        // 55 on K-Q-8.
        let hole = h(c(Rank::Five, Suit::Spades), c(Rank::Five, Suit::Hearts));
        let board = b3(
            c(Rank::King, Suit::Spades),
            c(Rank::Queen, Suit::Hearts),
            c(Rank::Eight, Suit::Clubs),
        );
        assert_eq!(classify(hole, &board), HandStrength::PairWeak);
    }

    #[test]
    fn pair_middle() {
        // 8 on board K-8-3 with 8x.
        let hole = h(c(Rank::Eight, Suit::Diamonds), c(Rank::Four, Suit::Hearts));
        let board = b3(
            c(Rank::King, Suit::Spades),
            c(Rank::Eight, Suit::Hearts),
            c(Rank::Three, Suit::Clubs),
        );
        assert_eq!(classify(hole, &board), HandStrength::PairMiddle);
    }

    #[test]
    fn pair_top_weak_kicker() {
        // K6 on K-8-3.
        let hole = h(c(Rank::King, Suit::Diamonds), c(Rank::Six, Suit::Hearts));
        let board = b3(
            c(Rank::King, Suit::Spades),
            c(Rank::Eight, Suit::Hearts),
            c(Rank::Three, Suit::Clubs),
        );
        assert_eq!(classify(hole, &board), HandStrength::PairTopWeakKicker);
    }

    #[test]
    fn pair_top_strong_kicker() {
        // AK on K-8-3.
        let hole = h(c(Rank::Ace, Suit::Diamonds), c(Rank::King, Suit::Hearts));
        let board = b3(
            c(Rank::King, Suit::Spades),
            c(Rank::Eight, Suit::Hearts),
            c(Rank::Three, Suit::Clubs),
        );
        assert_eq!(classify(hole, &board), HandStrength::PairTopStrongKicker);
    }

    #[test]
    fn overpair() {
        // AA on K-8-3.
        let hole = h(c(Rank::Ace, Suit::Spades), c(Rank::Ace, Suit::Hearts));
        let board = b3(
            c(Rank::King, Suit::Spades),
            c(Rank::Eight, Suit::Hearts),
            c(Rank::Three, Suit::Clubs),
        );
        assert_eq!(classify(hole, &board), HandStrength::Overpair);
    }

    #[test]
    fn two_pair() {
        // AK on A-K-3.
        let hole = h(c(Rank::Ace, Suit::Spades), c(Rank::King, Suit::Hearts));
        let board = b3(
            c(Rank::Ace, Suit::Diamonds),
            c(Rank::King, Suit::Clubs),
            c(Rank::Three, Suit::Clubs),
        );
        assert_eq!(classify(hole, &board), HandStrength::TwoPair);
    }

    #[test]
    fn set_on_flop() {
        // 88 on K-8-3.
        let hole = h(c(Rank::Eight, Suit::Spades), c(Rank::Eight, Suit::Hearts));
        let board = b3(
            c(Rank::King, Suit::Spades),
            c(Rank::Eight, Suit::Diamonds),
            c(Rank::Three, Suit::Clubs),
        );
        assert_eq!(classify(hole, &board), HandStrength::Set);
    }

    /// Regression (audit 2026-06-03): one-card trips on a paired board are
    /// three-of-a-kind and must classify as `Set`, not top-pair. Hero K8 on a
    /// K-K-3 board holds three Kings — previously fell through to `classify_pair`
    /// and was labelled `PairTopWeakKicker`, understating the hand by 3 bands.
    #[test]
    fn one_card_trips_on_paired_board_is_set() {
        let hole = h(c(Rank::King, Suit::Spades), c(Rank::Eight, Suit::Hearts));
        let board = b3(
            c(Rank::King, Suit::Diamonds),
            c(Rank::King, Suit::Clubs),
            c(Rank::Three, Suit::Hearts),
        );
        assert_eq!(
            classify(hole, &board),
            HandStrength::Set,
            "K8 on KK3 is trip Kings (Set band), not top pair"
        );
    }

    #[test]
    fn straight() {
        // 67 on 5-8-9.
        let hole = h(c(Rank::Six, Suit::Hearts), c(Rank::Seven, Suit::Diamonds));
        let board = b3(
            c(Rank::Five, Suit::Clubs),
            c(Rank::Eight, Suit::Spades),
            c(Rank::Nine, Suit::Hearts),
        );
        assert_eq!(classify(hole, &board), HandStrength::Straight);
    }

    #[test]
    fn flush_made() {
        // AhKh on Jh-7h-2h.
        let hole = h(c(Rank::Ace, Suit::Hearts), c(Rank::King, Suit::Hearts));
        let board = b3(
            c(Rank::Jack, Suit::Hearts),
            c(Rank::Seven, Suit::Hearts),
            c(Rank::Two, Suit::Hearts),
        );
        assert_eq!(classify(hole, &board), HandStrength::Flush);
    }

    #[test]
    fn full_house_plus_quads() {
        // AA on A-A-A-K-3.
        let hole = h(c(Rank::Ace, Suit::Spades), c(Rank::Ace, Suit::Hearts));
        let board = b5(
            c(Rank::Ace, Suit::Diamonds),
            c(Rank::Ace, Suit::Clubs),
            c(Rank::King, Suit::Hearts),
            c(Rank::Three, Suit::Hearts),
            c(Rank::Three, Suit::Diamonds),
        );
        assert_eq!(classify(hole, &board), HandStrength::FullHousePlus);
    }

    #[test]
    fn all_returns_13_unique() {
        let all = HandStrength::all();
        assert_eq!(all.len(), 13);
        let mut sorted = all.to_vec();
        sorted.sort();
        let mut deduped = sorted.clone();
        deduped.dedup();
        assert_eq!(deduped.len(), 13, "all variants must be unique");
    }

    // OSS dual-AI review 2026-07-01 (finding C1): a hero who does not contribute
    // to the board's pairing merely "plays the board" and must NOT be shown at a
    // strong made-hand band.
    #[test]
    fn plays_the_board_double_pair_is_not_hero_two_pair() {
        // 7♦2♣ on K♠K♥Q♦Q♣3♠: hero's cards pair nothing; hero plays the board's
        // two pair (any K/Q makes a boat that dominates it) → weak, not TwoPair.
        let hole = h(c(Rank::Seven, Suit::Diamonds), c(Rank::Two, Suit::Clubs));
        let board = b5(
            c(Rank::King, Suit::Spades),
            c(Rank::King, Suit::Hearts),
            c(Rank::Queen, Suit::Diamonds),
            c(Rank::Queen, Suit::Clubs),
            c(Rank::Three, Suit::Spades),
        );
        assert_eq!(classify(hole, &board), HandStrength::PairWeak);
    }

    #[test]
    fn plays_the_board_boat_is_not_hero_full_house() {
        // 7♦2♣ on K♠K♥K♦Q♣Q♠: the board is itself a full house; hero contributes
        // nothing → must be downgraded from FullHousePlus.
        let hole = h(c(Rank::Seven, Suit::Diamonds), c(Rank::Two, Suit::Clubs));
        let board = b5(
            c(Rank::King, Suit::Spades),
            c(Rank::King, Suit::Hearts),
            c(Rank::King, Suit::Diamonds),
            c(Rank::Queen, Suit::Clubs),
            c(Rank::Queen, Suit::Spades),
        );
        assert_ne!(classify(hole, &board), HandStrength::FullHousePlus);
    }

    #[test]
    fn hero_contributed_two_pair_still_two_pair() {
        // Contrast: Q♣ pairs the board's Q, so hero genuinely makes two pair
        // (KK + QQ using hero's Q) — must remain TwoPair, not be downgraded.
        let hole = h(c(Rank::Queen, Suit::Clubs), c(Rank::Jack, Suit::Diamonds));
        let board = b5(
            c(Rank::King, Suit::Spades),
            c(Rank::King, Suit::Hearts),
            c(Rank::Queen, Suit::Diamonds),
            c(Rank::Three, Suit::Spades),
            c(Rank::Eight, Suit::Clubs),
        );
        assert_eq!(classify(hole, &board), HandStrength::TwoPair);
    }

    // Codex round-3 catch: a LOW pocket pair below the board's pairs still just
    // plays the board — it must NOT be credited the strong band.
    #[test]
    fn low_pocket_pair_below_board_pairs_plays_the_board() {
        // 2♠2♥ on K♠K♥Q♦Q♣3♠: best five is the board's KKQQ3; the 22 never plays.
        let hole = h(c(Rank::Two, Suit::Spades), c(Rank::Two, Suit::Hearts));
        let two_pair_board = b5(
            c(Rank::King, Suit::Spades),
            c(Rank::King, Suit::Hearts),
            c(Rank::Queen, Suit::Diamonds),
            c(Rank::Queen, Suit::Clubs),
            c(Rank::Three, Suit::Spades),
        );
        assert_eq!(classify(hole, &two_pair_board), HandStrength::PairWeak);
        // 2♠2♥ on K♠K♥K♦Q♣Q♠ (board is itself a full house): 22 doesn't improve.
        let boat_board = b5(
            c(Rank::King, Suit::Spades),
            c(Rank::King, Suit::Hearts),
            c(Rank::King, Suit::Diamonds),
            c(Rank::Queen, Suit::Clubs),
            c(Rank::Queen, Suit::Spades),
        );
        assert_ne!(classify(hole, &boat_board), HandStrength::FullHousePlus);
    }

    #[test]
    fn high_pocket_pair_above_board_pair_improves() {
        // A♠A♥ on K♠K♥Q♦Q♣3♠: hero's AA is the top pair → two pair AA over KK,
        // strictly better than the board's KKQQ → genuine TwoPair.
        let hole = h(c(Rank::Ace, Suit::Spades), c(Rank::Ace, Suit::Hearts));
        let board = b5(
            c(Rank::King, Suit::Spades),
            c(Rank::King, Suit::Hearts),
            c(Rank::Queen, Suit::Diamonds),
            c(Rank::Queen, Suit::Clubs),
            c(Rank::Three, Suit::Spades),
        );
        assert_eq!(classify(hole, &board), HandStrength::TwoPair);
    }

    // Codex round-4 catch: hero MAKING quads on a full-house board is near-nut
    // (not a dominated board-play) and must stay FullHousePlus.
    #[test]
    fn hero_quads_on_full_house_board_is_full_house_plus() {
        let board = b5(
            c(Rank::King, Suit::Spades),
            c(Rank::King, Suit::Hearts),
            c(Rank::King, Suit::Diamonds),
            c(Rank::Queen, Suit::Clubs),
            c(Rank::Queen, Suit::Spades),
        );
        // K♣ makes quad Kings.
        let quad_kings = h(c(Rank::King, Suit::Clubs), c(Rank::Seven, Suit::Diamonds));
        assert_eq!(classify(quad_kings, &board), HandStrength::FullHousePlus);
        // Q♦Q♥ makes quad Queens (beats the board's KKKQQ full house).
        let quad_queens = h(c(Rank::Queen, Suit::Diamonds), c(Rank::Queen, Suit::Hearts));
        assert_eq!(classify(quad_queens, &board), HandStrength::FullHousePlus);
    }
}
