//! Hand evaluation via `rs_poker`.
//!
//! `rs_poker` types are used **only** internally in this module.
//! No `rs_poker` type appears in any public signature (ADR-012 / OQ-1).

use std::cmp::Reverse;

use crate::card::{Card, Rank, Suit};
use crate::hand::{BoardCards, HoleCards};
use crate::player::PlayerId;

// Re-export rs_poker types locally; they must NOT be re-exported outside this module.
use rs_poker::core::{Card as RsCard, Hand, Rankable, Suit as RsSuit, Value as RsValue};

/// All nine hand rank categories, in ascending order.
///
/// The inner `u32` is a category-specific strength value from `rs_poker`.
/// Two hands of the same category can be compared by the inner value.
///
/// Serialized as `{"category": "high_card", "strength": 12345}`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum HandRank {
    /// No meaningful combination — highest card wins.
    HighCard(u32),
    /// Two cards of the same rank.
    OnePair(u32),
    /// Two different pairs.
    TwoPair(u32),
    /// Three cards of the same rank.
    ThreeOfAKind(u32),
    /// Five cards in consecutive rank order.
    Straight(u32),
    /// Five cards of the same suit.
    Flush(u32),
    /// Three of one rank plus two of another.
    FullHouse(u32),
    /// Four cards of the same rank.
    FourOfAKind(u32),
    /// Five consecutive cards of the same suit.
    StraightFlush(u32),
}

impl serde::Serialize for HandRank {
    fn serialize<S: serde::Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        use serde::ser::SerializeStruct;
        let (category, strength) = match self {
            HandRank::HighCard(v) => ("high_card", *v),
            HandRank::OnePair(v) => ("one_pair", *v),
            HandRank::TwoPair(v) => ("two_pair", *v),
            HandRank::ThreeOfAKind(v) => ("three_of_a_kind", *v),
            HandRank::Straight(v) => ("straight", *v),
            HandRank::Flush(v) => ("flush", *v),
            HandRank::FullHouse(v) => ("full_house", *v),
            HandRank::FourOfAKind(v) => ("four_of_a_kind", *v),
            HandRank::StraightFlush(v) => ("straight_flush", *v),
        };
        let mut st = s.serialize_struct("HandRank", 2)?;
        st.serialize_field("category", category)?;
        st.serialize_field("strength", &strength)?;
        st.end()
    }
}

impl<'de> serde::Deserialize<'de> for HandRank {
    fn deserialize<D: serde::Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        use serde::de::{self, MapAccess, Visitor};
        struct HandRankVisitor;
        impl<'de> Visitor<'de> for HandRankVisitor {
            type Value = HandRank;
            fn expecting(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
                f.write_str("a HandRank object with 'category' and 'strength' fields")
            }
            fn visit_map<A: MapAccess<'de>>(self, mut map: A) -> Result<HandRank, A::Error> {
                let mut category: Option<String> = None;
                let mut strength: Option<u32> = None;
                while let Some(key) = map.next_key::<String>()? {
                    match key.as_str() {
                        "category" => {
                            category = Some(map.next_value()?);
                        }
                        "strength" => {
                            strength = Some(map.next_value()?);
                        }
                        _ => {
                            let _: serde::de::IgnoredAny = map.next_value()?;
                        }
                    }
                }
                let cat = category.ok_or_else(|| de::Error::missing_field("category"))?;
                let str_ = strength.ok_or_else(|| de::Error::missing_field("strength"))?;
                Ok(match cat.as_str() {
                    "high_card" => HandRank::HighCard(str_),
                    "one_pair" => HandRank::OnePair(str_),
                    "two_pair" => HandRank::TwoPair(str_),
                    "three_of_a_kind" => HandRank::ThreeOfAKind(str_),
                    "straight" => HandRank::Straight(str_),
                    "flush" => HandRank::Flush(str_),
                    "full_house" => HandRank::FullHouse(str_),
                    "four_of_a_kind" => HandRank::FourOfAKind(str_),
                    "straight_flush" => HandRank::StraightFlush(str_),
                    other => {
                        return Err(de::Error::unknown_variant(
                            other,
                            &[
                                "high_card",
                                "one_pair",
                                "two_pair",
                                "three_of_a_kind",
                                "straight",
                                "flush",
                                "full_house",
                                "four_of_a_kind",
                                "straight_flush",
                            ],
                        ))
                    }
                })
            }
        }
        d.deserialize_struct("HandRank", &["category", "strength"], HandRankVisitor)
    }
}

impl HandRank {
    /// A short lowercase string describing the category (for logging / WS frames).
    pub fn name(&self) -> &'static str {
        match self {
            HandRank::HighCard(_) => "high_card",
            HandRank::OnePair(_) => "one_pair",
            HandRank::TwoPair(_) => "two_pair",
            HandRank::ThreeOfAKind(_) => "three_of_a_kind",
            HandRank::Straight(_) => "straight",
            HandRank::Flush(_) => "flush",
            HandRank::FullHouse(_) => "full_house",
            HandRank::FourOfAKind(_) => "four_of_a_kind",
            HandRank::StraightFlush(_) => "straight_flush",
        }
    }
}

/// Evaluate the best 5-of-7 (or 5-of-6, 5-of-5) hand for a single player.
///
/// Always evaluates the 2 hole cards plus 0–5 community cards. With fewer than
/// five total cards it still returns a valid (partial) [`HandRank`] — e.g. a
/// 2-card preflop input yields a `HighCard`/`OnePair` rank. There is no
/// insufficient-cards signal; callers that need one must check card counts
/// themselves (the doc previously claimed a non-existent `None` return).
///
/// # Correctness — duplicate-free cards required (U22, dual-AI OSS review)
///
/// The hole + board cards MUST be pairwise distinct. A duplicate (e.g. a hole
/// card colliding with a board card) is only caught by a `debug_assert!`: it
/// **panics in debug builds** but in release builds the bitset-backed
/// evaluator silently deduplicates and returns a **wrong rank**. There is
/// deliberately no release-time check on this hot path — the engine
/// guarantees the invariant internally (the deck deals unique cards; the
/// solver pre-validates), so external callers must do the same.
pub fn rank_hand(hole: &HoleCards, board: &BoardCards) -> HandRank {
    rank_with_board(&board_eval(board), hole)
}

/// Rank multiple players' hands against the same board, returning a list sorted
/// winner-first (highest `HandRank` first). Ties are preserved in order.
///
/// Returns `Vec<(PlayerId, HandRank)>` with the strongest hand first.
pub fn rank_players(
    players: &[(PlayerId, HoleCards)],
    board: &BoardCards,
) -> Vec<(PlayerId, HandRank)> {
    let board = board_eval(board);
    let mut ranked: Vec<(PlayerId, HandRank)> = players
        .iter()
        .map(|(pid, hole)| (*pid, rank_with_board(&board, hole)))
        .collect();

    // Sort descending — best hand first.
    ranked.sort_by_key(|item| Reverse(item.1));
    ranked
}

// ---------------------------------------------------------------------------
// Allocation-free fast path for the equity Monte Carlo hot loop
// ---------------------------------------------------------------------------
//
// Convert the shared board once and copy its bitset for each player. All hand
// evaluation entrypoints use this path; external poker types stay private.

/// A board pre-converted to an `rs_poker` bitset, reusable across the players in
/// one Monte Carlo trial. The wrapped `rs_poker` type is a PRIVATE field, so no
/// `rs_poker` type appears in any public signature (ADR-012): callers can only
/// obtain a `BoardEval` from [`board_eval`] and consume it via [`rank_with_board`].
/// Exposed `pub` so `server::equity` (the live-snapshot MC, which must not
/// depend on `rs_poker`) can share this allocation-free evaluator.
#[derive(Clone, Copy)]
pub struct BoardEval {
    hand: Hand,
}

/// Convert a board into a reusable [`BoardEval`], allocation-free. Reads the
/// `flop`/`turn`/`river` fields directly (no intermediate `all_cards()` `Vec`).
pub fn board_eval(board: &BoardCards) -> BoardEval {
    let mut hand = Hand::new();
    if let Some([a, b, c]) = board.flop {
        hand.insert(card_to_rs(a));
        hand.insert(card_to_rs(b));
        hand.insert(card_to_rs(c));
    }
    if let Some(t) = board.turn {
        hand.insert(card_to_rs(t));
    }
    if let Some(r) = board.river {
        hand.insert(card_to_rs(r));
    }
    debug_assert_eq!(
        hand.count(),
        board.count(),
        "board contains duplicate cards"
    );
    BoardEval { hand }
}

/// Rank a player's two hole cards against a pre-converted board, allocation-free.
/// Equivalent to `rank_hand(hole, board)` for the same disjoint card set — the
/// underlying `Hand::rank()` is bitset-backed, so insertion order is irrelevant.
pub fn rank_with_board(be: &BoardEval, hole: &HoleCards) -> HandRank {
    let mut hand = be.hand; // `Hand` wraps a `Copy` CardBitSet — this is a stack copy.
    hand.insert(card_to_rs(hole.card1));
    hand.insert(card_to_rs(hole.card2));
    debug_assert_eq!(
        hand.count(),
        be.hand.count() + 2,
        "rank_hand received duplicate/overlapping cards; result would be wrong"
    );
    rs_rank_to_hand_rank(hand.rank())
}

// ---------------------------------------------------------------------------
// RankDescription — full-fidelity human-readable hand description
// ---------------------------------------------------------------------------
//
// Cardrooms label hands with FULL specificity ("Two Pair, Aces and Tens",
// "Flush, Ace high", "Straight, Ten to Ace") not just the category name.
// The kicker information needed for that is implicit in the 7 cards (2 hole +
// up to 5 board) but rs_poker's `Rank` only exposes a category + tiebreaker
// `u32`. `describe_hand` re-derives the kickers in plain Rust so the wire
// layer can ship structured data and the client can format the bilingual
// banner string from it.

/// Structured, lossless description of a player's best 5-of-7 hand.
///
/// Pure data: serde-serialisable and free of `rs_poker` types. The client
/// composes the bilingual banner string from these fields:
///
/// | category          | primary       | secondary  | kicker     | board                 |
/// |-------------------|---------------|------------|------------|-----------------------|
/// | high_card         | top rank      | —          | 2nd rank   | top 5 ranks desc      |
/// | one_pair          | pair rank     | —          | top kicker | top 3 kickers desc    |
/// | two_pair          | high pair     | low pair   | top kicker | —                     |
/// | three_of_a_kind   | trip rank     | —          | top kicker | top 2 kickers desc    |
/// | straight          | straight high | —          | straight low (high - 4 ranks) | 5 ranks desc |
/// | flush             | flush high    | —          | —          | 5 flush ranks desc    |
/// | full_house        | trip rank     | pair rank  | —          | —                     |
/// | four_of_a_kind    | quad rank     | —          | top kicker | —                     |
/// | straight_flush    | high (A => royal) | —      | —          | 5 ranks desc          |
///
/// Rank characters use the single-char abbreviations from
/// [`Rank::char`] (`'2'..'9'`, `'T'`, `'J'`, `'Q'`, `'K'`, `'A'`) wrapped in a
/// one-char `String` so JSON consumers don't need locale-specific tokenisation.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct RankDescription {
    /// Mirrors `HandRank::name()` — e.g. `"two_pair"`, `"flush"`.
    pub category: String,
    /// Primary rank for the category. Always `Some` for non-`high_card` ranks.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub primary: Option<String>,
    /// Secondary rank: low pair for `two_pair`, pair rank for `full_house`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub secondary: Option<String>,
    /// Top kicker rank (when meaningful for the category).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub kicker: Option<String>,
    /// Top relevant ranks in descending order (up to 5). Useful for client
    /// formatters that want to show "5-4-3-2 high" or print the flush layout.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub board: Vec<String>,
}

fn rank_str(r: Rank) -> String {
    // One `char` -> one-char `String`: the wire shape (see `RankDescription`).
    String::from(r.char())
}

/// Derive a [`RankDescription`] for a player's hole cards against the board.
///
/// Re-evaluates the hand (cheap — `rank_hand` does the same work) so the
/// category is guaranteed to match the canonical evaluator output. The
/// kickers are derived independently from the 7-card multiset using
/// Hold'em-standard tiebreak rules.
///
/// Pre-flop / flop-only boards are tolerated: the description reflects the
/// best hand possible with the cards currently dealt.
pub fn describe_hand(hole: &HoleCards, board: &BoardCards) -> RankDescription {
    let rank = rank_hand(hole, board);
    // At most 7 cards — kept in a stack buffer. This runs once per viewer per
    // broadcast snapshot (hero hand-strength banner), so the intermediate
    // `Vec`s it used to build were pure allocator traffic; only the `String`s
    // that go on the wire are heap-allocated now.
    let mut card_buf = [hole.card1; 7];
    card_buf[1] = hole.card2;
    let mut n = 2usize;
    if let Some(flop) = board.flop {
        card_buf[n..n + 3].copy_from_slice(&flop);
        n += 3;
    }
    if let Some(turn) = board.turn {
        card_buf[n] = turn;
        n += 1;
    }
    if let Some(river) = board.river {
        card_buf[n] = river;
        n += 1;
    }
    let cards: &mut [Card] = &mut card_buf[..n];
    // Descending by rank, suits arbitrary. Stable, so cards of equal rank keep
    // the hole-then-board order the old `Vec` push order produced.
    cards.sort_by_key(|c| Reverse(c.rank));

    // Count occurrences of each rank once (indexed by rank, not a Vec of pairs).
    let mut counts = [0u8; 13];
    for c in cards.iter() {
        counts[c.rank as usize] += 1;
    }
    // Highest rank (descending, as the old `rank_counts` vec was ordered) with
    // at least `min` occurrences, optionally skipping one already-used rank.
    let highest_with = |min: u8, exclude: Option<Rank>| -> Option<Rank> {
        Rank::ALL
            .iter()
            .rev()
            .copied()
            .find(|r| counts[*r as usize] >= min && Some(*r) != exclude)
    };
    // Distinct ranks in `cards` order (descending), skipping `exclude`.
    let distinct_ranks_except = |exclude: Option<Rank>| -> ([Rank; 7], usize) {
        let mut out = [Rank::Two; 7];
        let mut len = 0usize;
        for c in cards.iter() {
            if Some(c.rank) == exclude || out[..len].contains(&c.rank) {
                continue;
            }
            out[len] = c.rank;
            len += 1;
        }
        (out, len)
    };

    let category = rank.name().to_string();
    match rank {
        HandRank::HighCard(_) => {
            // Top 5 distinct ranks descending.
            let (ranks, len) = distinct_ranks_except(None);
            let take = &ranks[..len.min(5)];
            RankDescription {
                category,
                primary: take.first().copied().map(rank_str),
                secondary: None,
                kicker: take.get(1).copied().map(rank_str),
                board: take.iter().copied().map(rank_str).collect(),
            }
        }
        HandRank::OnePair(_) => {
            let pair = highest_with(2, None);
            let (kickers, len) = distinct_ranks_except(pair);
            let kickers = &kickers[..len];
            RankDescription {
                category,
                primary: pair.map(rank_str),
                secondary: None,
                kicker: kickers.first().copied().map(rank_str),
                board: kickers.iter().take(3).copied().map(rank_str).collect(),
            }
        }
        HandRank::TwoPair(_) => {
            let high = highest_with(2, None);
            let low = highest_with(2, high);
            let kicker = cards
                .iter()
                .map(|c| c.rank)
                .find(|r| Some(*r) != high && Some(*r) != low);
            RankDescription {
                category,
                primary: high.map(rank_str),
                secondary: low.map(rank_str),
                kicker: kicker.map(rank_str),
                board: Vec::new(),
            }
        }
        HandRank::ThreeOfAKind(_) => {
            let trip = highest_with(3, None);
            let (kickers, len) = distinct_ranks_except(trip);
            let kickers = &kickers[..len];
            RankDescription {
                category,
                primary: trip.map(rank_str),
                secondary: None,
                kicker: kickers.first().copied().map(rank_str),
                board: kickers.iter().take(2).copied().map(rank_str).collect(),
            }
        }
        HandRank::Straight(_) => {
            let high = straight_high_card(cards);
            let low = high.map(straight_low_for);
            let board = match high {
                Some(h) => five_straight_ranks(h)
                    .iter()
                    .map(|r| rank_str(*r))
                    .collect(),
                None => Vec::new(),
            };
            RankDescription {
                category,
                primary: high.map(rank_str),
                secondary: None,
                kicker: low.map(rank_str),
                board,
            }
        }
        HandRank::Flush(_) => {
            // `cards` is already sorted descending, so the flush ranks come out
            // descending without a second sort.
            let suit = flush_suit(cards);
            let mut top_5 = [Rank::Two; 5];
            let mut len = 0usize;
            if let Some(s) = suit {
                for c in cards.iter().filter(|c| c.suit == s) {
                    if len == 5 {
                        break;
                    }
                    top_5[len] = c.rank;
                    len += 1;
                }
            }
            let top_5 = &top_5[..len];
            RankDescription {
                category,
                primary: top_5.first().copied().map(rank_str),
                secondary: None,
                kicker: None,
                board: top_5.iter().copied().map(rank_str).collect(),
            }
        }
        HandRank::FullHouse(_) => {
            // The trip rank is the highest count-3-or-4 rank.
            // The pair rank is the highest count-2-or-3 rank that is not the trip.
            let trip = highest_with(3, None);
            let pair = highest_with(2, trip);
            RankDescription {
                category,
                primary: trip.map(rank_str),
                secondary: pair.map(rank_str),
                kicker: None,
                board: Vec::new(),
            }
        }
        HandRank::FourOfAKind(_) => {
            let quad = highest_with(4, None);
            let kicker = cards.iter().map(|c| c.rank).find(|r| Some(*r) != quad);
            RankDescription {
                category,
                primary: quad.map(rank_str),
                secondary: None,
                kicker: kicker.map(rank_str),
                board: Vec::new(),
            }
        }
        HandRank::StraightFlush(_) => {
            let suit = flush_suit(cards);
            let mut flush_buf = [hole.card1; 7];
            let mut len = 0usize;
            if let Some(s) = suit {
                for c in cards.iter().filter(|c| c.suit == s) {
                    flush_buf[len] = *c;
                    len += 1;
                }
            }
            let high = straight_high_card(&flush_buf[..len]);
            let board = match high {
                Some(h) => five_straight_ranks(h)
                    .iter()
                    .map(|r| rank_str(*r))
                    .collect(),
                None => Vec::new(),
            };
            RankDescription {
                category,
                primary: high.map(rank_str),
                secondary: None,
                kicker: None,
                board,
            }
        }
    }
}

/// Find the high card of the highest straight present in `cards`.
///
/// Handles the wheel (A-2-3-4-5) by treating Ace as low: returns `Rank::Five`
/// in that case. Returns `None` if no straight is present (caller already
/// determined `HandRank::Straight` so this should not happen — defensive).
fn straight_high_card(cards: &[Card]) -> Option<Rank> {
    let mut present = [false; 13];
    for c in cards {
        present[c.rank as usize] = true;
    }
    // Iterate from highest (Ace) downward; check 5-card consecutive runs.
    // Ranks indexed 0=Two ... 12=Ace.
    for high_idx in (4..=12).rev() {
        if (high_idx - 4..=high_idx).all(|i| present[i]) {
            return Some(Rank::ALL[high_idx]);
        }
    }
    // Wheel: A-2-3-4-5 (Ace acts as 1).
    if present[12] && present[0] && present[1] && present[2] && present[3] {
        return Some(Rank::Five);
    }
    None
}

/// Low end of a straight whose high card is `high` (wheel returns Ace).
fn straight_low_for(high: Rank) -> Rank {
    // For wheel A-2-3-4-5, high is Five and low is Ace (functionally) — but
    // the conventional reading is "Five-high straight, Ace to Five" so we
    // report the low end as Ace (Rank::Ace). For every other straight the
    // low end is high - 4.
    if high == Rank::Five {
        Rank::Ace
    } else {
        let idx = high as usize;
        Rank::ALL[idx - 4]
    }
}

/// The 5 ranks of a straight with high card `high`, descending.
fn five_straight_ranks(high: Rank) -> [Rank; 5] {
    if high == Rank::Five {
        // Wheel: 5-4-3-2-A
        [Rank::Five, Rank::Four, Rank::Three, Rank::Two, Rank::Ace]
    } else {
        let idx = high as usize;
        [
            Rank::ALL[idx],
            Rank::ALL[idx - 1],
            Rank::ALL[idx - 2],
            Rank::ALL[idx - 3],
            Rank::ALL[idx - 4],
        ]
    }
}

/// The suit that occurs ≥ 5 times in `cards`, if any.
fn flush_suit(cards: &[Card]) -> Option<Suit> {
    for s in Suit::ALL {
        let n = cards.iter().filter(|c| c.suit == s).count();
        if n >= 5 {
            return Some(s);
        }
    }
    None
}

// ---------------------------------------------------------------------------
// Internal conversion helpers
// ---------------------------------------------------------------------------

fn card_to_rs(card: Card) -> RsCard {
    let value = match card.rank {
        Rank::Two => RsValue::Two,
        Rank::Three => RsValue::Three,
        Rank::Four => RsValue::Four,
        Rank::Five => RsValue::Five,
        Rank::Six => RsValue::Six,
        Rank::Seven => RsValue::Seven,
        Rank::Eight => RsValue::Eight,
        Rank::Nine => RsValue::Nine,
        Rank::Ten => RsValue::Ten,
        Rank::Jack => RsValue::Jack,
        Rank::Queen => RsValue::Queen,
        Rank::King => RsValue::King,
        Rank::Ace => RsValue::Ace,
    };
    let suit = match card.suit {
        Suit::Spades => RsSuit::Spade,
        Suit::Hearts => RsSuit::Heart,
        Suit::Diamonds => RsSuit::Diamond,
        Suit::Clubs => RsSuit::Club,
    };
    RsCard { value, suit }
}

fn rs_rank_to_hand_rank(rank: rs_poker::core::Rank) -> HandRank {
    match rank {
        rs_poker::core::Rank::HighCard(v) => HandRank::HighCard(v),
        rs_poker::core::Rank::OnePair(v) => HandRank::OnePair(v),
        rs_poker::core::Rank::TwoPair(v) => HandRank::TwoPair(v),
        rs_poker::core::Rank::ThreeOfAKind(v) => HandRank::ThreeOfAKind(v),
        rs_poker::core::Rank::Straight(v) => HandRank::Straight(v),
        rs_poker::core::Rank::Flush(v) => HandRank::Flush(v),
        rs_poker::core::Rank::FullHouse(v) => HandRank::FullHouse(v),
        rs_poker::core::Rank::FourOfAKind(v) => HandRank::FourOfAKind(v),
        rs_poker::core::Rank::StraightFlush(v) => HandRank::StraightFlush(v),
    }
}
