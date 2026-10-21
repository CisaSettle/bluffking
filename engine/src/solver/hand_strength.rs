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
    // Preflop coarse classification.
    if board.count() == 0 {
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

    // Hole + board live in stack buffers (max 2 + 5 = 7 cards): this function
    // is called per decision by the advisor, and the old `Vec` per board /
    // per union / per suit showed up as pure allocator traffic.
    debug_assert!(board.count() <= 5);
    let mut board_buf = [hole.card1; 5];
    let mut board_n = 0usize;
    if let Some(flop) = board.flop {
        board_buf[..3].copy_from_slice(&flop);
        board_n = 3;
    }
    if let Some(turn) = board.turn {
        board_buf[board_n] = turn;
        board_n += 1;
    }
    if let Some(river) = board.river {
        board_buf[board_n] = river;
        board_n += 1;
    }
    let board_cards: &[Card] = &board_buf[..board_n];
    let mut all_buf = [hole.card1; 7];
    all_buf[1] = hole.card2;
    all_buf[2..2 + board_n].copy_from_slice(board_cards);
    let all_cards: &[Card] = &all_buf[..2 + board_n];

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
    //
    // Rank counts / suit counts / rank bitmasks are computed ONCE here for the
    // union and once for the board alone, then threaded through every test.
    let combined = CardStats::new(all_cards);
    let board_stats = CardStats::new(board_cards);
    let combined_counts = combined.rank_counts;
    let board_counts = board_stats.rank_counts;
    if combined.is_full_house_plus() {
        // Quads and straight flushes are near-nut even when hero "plays the board":
        // unlike a plain full house (which any opponent card re-pairing the board
        // beats), they are not dominated, so they always keep the strong band —
        // this also covers hero MAKING quads on a full-house board (e.g. Kc7d on
        // KsKhKdQcQs). Otherwise it is a plain full house: credit hero only when
        // hero's boat is strictly better than the board's own boat.
        let near_nut = combined.is_straight_flush() || combined_counts.iter().any(|&n| n >= 4);
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
    if combined.is_flush() {
        return HandStrength::Flush;
    }
    if combined.is_straight() {
        return HandStrength::Straight;
    }
    if is_set(hole, board_cards) {
        return HandStrength::Set;
    }
    if combined.is_two_pair() {
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
    if let Some(pair_kind) = classify_pair(hole, board_cards) {
        return pair_kind;
    }

    // ---- Draws (no made hand at least a pair) ----
    let hole_suits = [hole.card1.suit, hole.card2.suit];
    let flush_draw = has_flush_draw(&hole_suits, &board_stats.suit_counts);

    // Hero must PARTICIPATE in a straight draw — a 4-run/gutshot window living
    // entirely on the board plays every hand equally and is not hero's draw
    // (mirrors the `hole_n >= 1` requirement in `has_flush_draw`). U19 (dual-AI
    // OSS review): Ac2d on 5h6s7d8c must not read as an OESD.
    let hole_bits = (1u16 << hole.card1.rank as u16) | (1u16 << hole.card2.rank as u16);
    let oesd = has_open_ended_straight_draw(combined.rank_bits, hole_bits);
    let gutshot = !oesd && has_gutshot(combined.rank_bits, hole_bits);

    if flush_draw || oesd {
        return HandStrength::DrawStrong;
    }
    if gutshot || has_backdoor_flush(&hole_suits, &board_stats.suit_counts, board_n) {
        return HandStrength::DrawWeak;
    }

    HandStrength::PureBluffNoEquity
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

/// One pass over a card set, shared by every made-hand test below: rank
/// multiplicities, per-suit card counts, the union rank bitmask, and a rank
/// bitmask per suit (which makes the straight-flush test a bit-shift instead of
/// four filtered `Vec<Card>` allocations). Built once per `classify` for the
/// hole+board union and once for the board alone, replacing the three separate
/// `rank_counts` passes the classifier used to make.
#[derive(Clone, Copy)]
struct CardStats {
    rank_counts: [u8; 13],
    suit_counts: [u8; 4],
    /// Union of `1 << rank` over all cards.
    rank_bits: u16,
    /// Per suit, the union of `1 << rank` over that suit's cards.
    suit_rank_bits: [u16; 4],
}

impl CardStats {
    fn new(cards: &[Card]) -> Self {
        let mut stats = Self {
            rank_counts: [0; 13],
            suit_counts: [0; 4],
            rank_bits: 0,
            suit_rank_bits: [0; 4],
        };
        for card in cards {
            let suit = suit_index(card.suit);
            let bit = 1u16 << (card.rank as u16);
            stats.rank_counts[card.rank as usize] += 1;
            stats.suit_counts[suit] += 1;
            stats.rank_bits |= bit;
            stats.suit_rank_bits[suit] |= bit;
        }
        stats
    }

    fn is_flush(&self) -> bool {
        self.suit_counts.iter().any(|c| *c >= 5)
    }

    fn is_straight(&self) -> bool {
        straight_in_bits(self.rank_bits)
    }

    /// A straight inside one suit. `straight_in_bits` already requires five
    /// distinct consecutive ranks of that suit, which implies the old
    /// "subset.len() >= 5" precondition.
    fn is_straight_flush(&self) -> bool {
        self.suit_rank_bits.iter().copied().any(straight_in_bits)
    }

    fn is_two_pair(&self) -> bool {
        self.rank_counts.iter().filter(|c| **c >= 2).count() >= 2
    }

    fn is_full_house_plus(&self) -> bool {
        let mut four = false;
        let mut three_count = 0u8;
        let mut pair_count = 0u8;
        for c in self.rank_counts.iter() {
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
        self.is_straight_flush()
    }
}

fn suit_index(suit: Suit) -> usize {
    match suit {
        Suit::Spades => 0,
        Suit::Hearts => 1,
        Suit::Diamonds => 2,
        Suit::Clubs => 3,
    }
}

/// Five consecutive ranks (or the wheel) inside a rank bitmask.
fn straight_in_bits(bits: u16) -> bool {
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

/// The two highest paired ranks (count ≥ 2), highest first, if at least two
/// exist — i.e. the two-pair a hand makes. Comparing this for hole+board vs the
/// board alone tells us whether hero's cards strengthen the two pair or merely
/// play the board's (OSS dual-AI review 2026-07-01, finding C1).
fn top_two_pair_ranks(counts: &[u8; 13]) -> Option<(usize, usize)> {
    let mut pairs = (0..13).rev().filter(|&r| counts[r] >= 2);
    Some((pairs.next()?, pairs.next()?))
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
    debug_assert!(board.len() <= 5);
    let mut board_ranks_buf = [Rank::Two; 5];
    let board_len = board.len().min(5);
    for (slot, card) in board_ranks_buf.iter_mut().zip(board.iter()) {
        *slot = card.rank;
    }
    let board_ranks = &mut board_ranks_buf[..board_len];
    let top = top_board_rank(board);

    // Pocket pair.
    if hole.card1.rank == hole.card2.rank {
        let pair_rank = hole.card1.rank;
        if pair_rank > top {
            return Some(HandStrength::Overpair);
        }
        // A pocket pair below the top board card but ABOVE the second board rank
        // plays like middle pair — it beats every board pair except the top one
        // (U36, dual-AI OSS review). Only a pair at/below the second rank is weak.
        board_ranks.sort_by(|a, b| b.cmp(a));
        if board.len() >= 2 && pair_rank > board_ranks[1] {
            return Some(HandStrength::PairMiddle);
        }
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

fn has_flush_draw(hole_suits: &[Suit; 2], board_suit_counts: &[u8; 4]) -> bool {
    for suit in Suit::ALL {
        let hole_n = hole_suits.iter().filter(|s| **s == suit).count();
        let board_n = board_suit_counts[suit_index(suit)] as usize;
        // Flush draw = exactly 4 of one suit, of which at least 1 is in hero's hole.
        if hole_n + board_n == 4 && hole_n >= 1 {
            return true;
        }
    }
    false
}

fn has_backdoor_flush(
    hole_suits: &[Suit; 2],
    board_suit_counts: &[u8; 4],
    board_len: usize,
) -> bool {
    // Backdoor flush = exactly 3 of one suit on flop, hero holds at least one.
    if board_len != 3 {
        return false;
    }
    for suit in Suit::ALL {
        let hole_n = hole_suits.iter().filter(|s| **s == suit).count();
        let board_n = board_suit_counts[suit_index(suit)] as usize;
        if hole_n + board_n == 3 && hole_n >= 1 {
            return true;
        }
    }
    false
}

/// `bits` is the union rank bitmask of hole+board (see [`CardStats`]).
fn has_open_ended_straight_draw(bits: u16, hole_bits: u16) -> bool {
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
            // Hero must hold at least one card of the 4-run — a board-only run is
            // not hero's draw (U19).
            if has_low_ext && has_high_ext && (mask & hole_bits) != 0 {
                return true;
            }
        }
    }
    false
}

/// `bits` is the union rank bitmask of hole+board (see [`CardStats`]).
fn has_gutshot(bits: u16, hole_bits: u16) -> bool {
    let low_idx = Rank::Two as u16;
    let high_idx = Rank::Ace as u16;
    // Any 5-rank window with exactly 4 bits set is a gutshot, PROVIDED hero holds
    // at least one of those ranks — a board-only gutshot is not hero's draw (U19).
    for start in low_idx..=high_idx - 4 {
        let window: u16 = (1 << start)
            | (1 << (start + 1))
            | (1 << (start + 2))
            | (1 << (start + 3))
            | (1 << (start + 4));
        let present = (bits & window).count_ones();
        if present == 4 && (window & hole_bits) != 0 {
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
    if (bits & wheel_mask).count_ones() == 4 && (wheel_mask & hole_bits) != 0 {
        return true;
    }
    false
}
