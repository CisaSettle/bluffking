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
pub fn rank_hand(hole: &HoleCards, board: &BoardCards) -> HandRank {
    let mut all_cards: Vec<RsCard> = Vec::with_capacity(7);
    all_cards.push(card_to_rs(hole.card1));
    all_cards.push(card_to_rs(hole.card2));
    for c in board.all_cards() {
        all_cards.push(card_to_rs(c));
    }
    // `Hand::new_with_cards` is bitset-backed and silently DEDUPLICATES
    // identical cards — a duplicate (e.g. a hole card that collides with a board
    // card) collapses the 7-card multiset to fewer distinct cards and yields a
    // silently WRONG rank. Engine callers always pass disjoint hole+board cards
    // (the deck deals unique cards; the solver pre-validates), so this is a
    // latent guard: surface the invariant violation loudly in dev/tests instead
    // of returning a quietly wrong rank in production (audit 2026-06-03).
    debug_assert!(
        !has_duplicate_cards(&all_cards),
        "rank_hand received duplicate/overlapping cards; result would be wrong"
    );
    let hand = Hand::new_with_cards(all_cards);
    rs_rank_to_hand_rank(hand.rank())
}

/// True if any two of the `rs_poker` cards are identical. Used only by a
/// `debug_assert!` in [`rank_hand`].
fn has_duplicate_cards(cards: &[RsCard]) -> bool {
    for (i, a) in cards.iter().enumerate() {
        for b in &cards[i + 1..] {
            if a == b {
                return true;
            }
        }
    }
    false
}

/// Rank multiple players' hands against the same board, returning a list sorted
/// winner-first (highest `HandRank` first). Ties are preserved in order.
///
/// Returns `Vec<(PlayerId, HandRank)>` with the strongest hand first.
pub fn rank_players(
    players: &[(PlayerId, HoleCards)],
    board: &BoardCards,
) -> Vec<(PlayerId, HandRank)> {
    let mut ranked: Vec<(PlayerId, HandRank)> = players
        .iter()
        .map(|(pid, hole)| (*pid, rank_hand(hole, board)))
        .collect();

    // Sort descending — best hand first.
    ranked.sort_by_key(|item| Reverse(item.1));
    ranked
}

// ---------------------------------------------------------------------------
// Allocation-free fast path for the equity Monte Carlo hot loop
// ---------------------------------------------------------------------------
//
// `rank_hand` allocates two throwaway `Vec`s per call (the `RsCard` buffer plus
// the `board.all_cards()` vec) and re-converts the shared board for every
// player. In the MC loop the board is IDENTICAL for hero + all opponents within
// a trial, so we pre-convert it ONCE into an `rs_poker` bitset `Hand` (which is
// `Copy`) and evaluate each player by copying that bitset and inserting only
// their two hole cards — no heap allocation, no per-player board re-conversion.
// `rs_poker` types stay INTERNAL (ADR-012: none may appear in a public
// signature): `BoardEval` is `pub(crate)` and never leaves the engine crate.

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
    BoardEval { hand }
}

/// Rank a player's two hole cards against a pre-converted board, allocation-free.
/// Equivalent to `rank_hand(hole, board)` for the same disjoint card set — the
/// underlying `Hand::rank()` is bitset-backed, so insertion order is irrelevant.
pub fn rank_with_board(be: &BoardEval, hole: &HoleCards) -> HandRank {
    let mut hand = be.hand; // `Hand` wraps a `Copy` CardBitSet — this is a stack copy.
    hand.insert(card_to_rs(hole.card1));
    hand.insert(card_to_rs(hole.card2));
    rs_rank_to_hand_rank(hand.rank())
}

#[cfg(test)]
mod fast_eval_tests {
    use super::*;
    use crate::card::{Rank, Suit};

    fn c(r: Rank, s: Suit) -> Card {
        Card::new(r, s)
    }

    /// The allocation-free fast path must agree with `rank_hand` on every board
    /// street, for many disjoint hole/board combinations.
    #[test]
    fn rank_with_board_matches_rank_hand() {
        let boards = [
            // preflop
            BoardCards::empty(),
            // flop
            BoardCards {
                flop: Some([
                    c(Rank::King, Suit::Diamonds),
                    c(Rank::Seven, Suit::Clubs),
                    c(Rank::Two, Suit::Hearts),
                ]),
                turn: None,
                river: None,
            },
            // turn
            BoardCards {
                flop: Some([
                    c(Rank::Queen, Suit::Spades),
                    c(Rank::Jack, Suit::Spades),
                    c(Rank::Two, Suit::Hearts),
                ]),
                turn: Some(c(Rank::Five, Suit::Diamonds)),
                river: None,
            },
            // river
            BoardCards {
                flop: Some([
                    c(Rank::Queen, Suit::Spades),
                    c(Rank::Jack, Suit::Spades),
                    c(Rank::Two, Suit::Hearts),
                ]),
                turn: Some(c(Rank::Five, Suit::Diamonds)),
                river: Some(c(Rank::Nine, Suit::Clubs)),
            },
        ];
        // A spread of hole cards that don't collide with the boards above.
        let holes = [
            HoleCards::new(c(Rank::Ace, Suit::Spades), c(Rank::Ace, Suit::Hearts)),
            HoleCards::new(c(Rank::Ace, Suit::Clubs), c(Rank::King, Suit::Hearts)),
            HoleCards::new(c(Rank::Ten, Suit::Hearts), c(Rank::Nine, Suit::Hearts)),
            HoleCards::new(
                c(Rank::Three, Suit::Diamonds),
                c(Rank::Four, Suit::Diamonds),
            ),
        ];
        for board in &boards {
            let be = board_eval(board);
            let board_set = board.all_cards();
            for hole in &holes {
                // Skip combos that collide with the board (fast path and
                // rank_hand both assume disjoint cards).
                if board_set.contains(&hole.card1) || board_set.contains(&hole.card2) {
                    continue;
                }
                assert_eq!(
                    rank_with_board(&be, hole),
                    rank_hand(hole, board),
                    "fast eval disagreed with rank_hand for hole {hole:?} board {board:?}"
                );
            }
        }
    }
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

impl RankDescription {
    /// Minimal description carrying only the category — used when caller
    /// cannot supply hole + board (e.g. legacy ShowdownEntry without cards).
    pub fn category_only(category: impl Into<String>) -> Self {
        Self {
            category: category.into(),
            primary: None,
            secondary: None,
            kicker: None,
            board: Vec::new(),
        }
    }
}

fn rank_str(r: Rank) -> String {
    r.char().to_string()
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
    let mut cards: Vec<Card> = Vec::with_capacity(7);
    cards.push(hole.card1);
    cards.push(hole.card2);
    for c in board.all_cards() {
        cards.push(c);
    }
    // Descending by rank, suits arbitrary.
    cards.sort_by_key(|c| Reverse(c.rank));

    // Count occurrences of each rank.
    let mut rank_counts: Vec<(Rank, usize)> = Vec::new();
    for r in Rank::ALL.iter().rev() {
        let n = cards.iter().filter(|c| c.rank == *r).count();
        if n > 0 {
            rank_counts.push((*r, n));
        }
    }

    let category = rank.name().to_string();
    match rank {
        HandRank::HighCard(_) => {
            // Top 5 distinct ranks descending.
            let mut ranks_desc: Vec<Rank> = cards.iter().map(|c| c.rank).collect();
            ranks_desc.dedup();
            let take: Vec<Rank> = ranks_desc.into_iter().take(5).collect();
            let primary = take.first().copied().map(rank_str);
            let kicker = take.get(1).copied().map(rank_str);
            let board = take.into_iter().map(rank_str).collect();
            RankDescription {
                category,
                primary,
                secondary: None,
                kicker,
                board,
            }
        }
        HandRank::OnePair(_) => {
            let pair = rank_counts.iter().find(|(_, n)| *n >= 2).map(|(r, _)| *r);
            let kickers: Vec<Rank> = cards
                .iter()
                .map(|c| c.rank)
                .filter(|r| Some(*r) != pair)
                .fold(Vec::new(), |mut acc, r| {
                    if !acc.contains(&r) {
                        acc.push(r);
                    }
                    acc
                });
            let board: Vec<String> = kickers.iter().take(3).map(|r| rank_str(*r)).collect();
            RankDescription {
                category,
                primary: pair.map(rank_str),
                secondary: None,
                kicker: kickers.first().copied().map(rank_str),
                board,
            }
        }
        HandRank::TwoPair(_) => {
            let pairs: Vec<Rank> = rank_counts
                .iter()
                .filter(|(_, n)| *n >= 2)
                .map(|(r, _)| *r)
                .collect();
            let high = pairs.first().copied();
            let low = pairs.get(1).copied();
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
            let trip = rank_counts.iter().find(|(_, n)| *n >= 3).map(|(r, _)| *r);
            let kickers: Vec<Rank> = cards
                .iter()
                .map(|c| c.rank)
                .filter(|r| Some(*r) != trip)
                .fold(Vec::new(), |mut acc, r| {
                    if !acc.contains(&r) {
                        acc.push(r);
                    }
                    acc
                });
            let board: Vec<String> = kickers.iter().take(2).map(|r| rank_str(*r)).collect();
            RankDescription {
                category,
                primary: trip.map(rank_str),
                secondary: None,
                kicker: kickers.first().copied().map(rank_str),
                board,
            }
        }
        HandRank::Straight(_) => {
            let high = straight_high_card(&cards);
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
            let suit = flush_suit(&cards);
            let top_5: Vec<Rank> = match suit {
                Some(s) => {
                    let mut rs: Vec<Rank> = cards
                        .iter()
                        .filter(|c| c.suit == s)
                        .map(|c| c.rank)
                        .collect();
                    rs.sort_by_key(|r| Reverse(*r));
                    rs.into_iter().take(5).collect()
                }
                None => Vec::new(),
            };
            RankDescription {
                category,
                primary: top_5.first().copied().map(rank_str),
                secondary: None,
                kicker: None,
                board: top_5.into_iter().map(rank_str).collect(),
            }
        }
        HandRank::FullHouse(_) => {
            // The trip rank is the highest count-3-or-4 rank.
            // The pair rank is the highest count-2-or-3 rank that is not the trip.
            let trip = rank_counts.iter().find(|(_, n)| *n >= 3).map(|(r, _)| *r);
            let pair = rank_counts
                .iter()
                .find(|(r, n)| *n >= 2 && Some(*r) != trip)
                .map(|(r, _)| *r);
            RankDescription {
                category,
                primary: trip.map(rank_str),
                secondary: pair.map(rank_str),
                kicker: None,
                board: Vec::new(),
            }
        }
        HandRank::FourOfAKind(_) => {
            let quad = rank_counts.iter().find(|(_, n)| *n >= 4).map(|(r, _)| *r);
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
            let suit = flush_suit(&cards);
            let flush_cards: Vec<Card> = match suit {
                Some(s) => cards.iter().copied().filter(|c| c.suit == s).collect(),
                None => Vec::new(),
            };
            let high = straight_high_card(&flush_cards);
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::card::{Card, Rank, Suit};
    use crate::hand::{BoardCards, HoleCards};

    fn card(r: Rank, s: Suit) -> Card {
        Card::new(r, s)
    }

    fn hole(c1: Card, c2: Card) -> HoleCards {
        HoleCards::new(c1, c2)
    }

    fn board5(c1: Card, c2: Card, c3: Card, c4: Card, c5: Card) -> BoardCards {
        BoardCards {
            flop: Some([c1, c2, c3]),
            turn: Some(c4),
            river: Some(c5),
        }
    }

    fn board3(c1: Card, c2: Card, c3: Card) -> BoardCards {
        BoardCards {
            flop: Some([c1, c2, c3]),
            turn: None,
            river: None,
        }
    }

    // 9 canonical hand category tests

    #[test]
    fn high_card() {
        let h = hole(
            card(Rank::Ace, Suit::Spades),
            card(Rank::King, Suit::Diamonds),
        );
        let b = board5(
            card(Rank::Two, Suit::Clubs),
            card(Rank::Five, Suit::Hearts),
            card(Rank::Seven, Suit::Diamonds),
            card(Rank::Nine, Suit::Clubs),
            card(Rank::Jack, Suit::Spades),
        );
        assert!(matches!(rank_hand(&h, &b), HandRank::HighCard(_)));
    }

    #[test]
    fn one_pair() {
        let h = hole(
            card(Rank::Ace, Suit::Spades),
            card(Rank::Ace, Suit::Diamonds),
        );
        let b = board5(
            card(Rank::Two, Suit::Clubs),
            card(Rank::Five, Suit::Hearts),
            card(Rank::Seven, Suit::Diamonds),
            card(Rank::Nine, Suit::Clubs),
            card(Rank::Jack, Suit::Spades),
        );
        assert!(matches!(rank_hand(&h, &b), HandRank::OnePair(_)));
    }

    #[test]
    fn two_pair() {
        let h = hole(
            card(Rank::Ace, Suit::Spades),
            card(Rank::Ace, Suit::Diamonds),
        );
        let b = board5(
            card(Rank::King, Suit::Clubs),
            card(Rank::King, Suit::Hearts),
            card(Rank::Seven, Suit::Diamonds),
            card(Rank::Nine, Suit::Clubs),
            card(Rank::Jack, Suit::Spades),
        );
        assert!(matches!(rank_hand(&h, &b), HandRank::TwoPair(_)));
    }

    #[test]
    fn three_of_a_kind() {
        let h = hole(
            card(Rank::Ace, Suit::Spades),
            card(Rank::Ace, Suit::Diamonds),
        );
        let b = board5(
            card(Rank::Ace, Suit::Clubs),
            card(Rank::King, Suit::Hearts),
            card(Rank::Seven, Suit::Diamonds),
            card(Rank::Nine, Suit::Clubs),
            card(Rank::Jack, Suit::Spades),
        );
        assert!(matches!(rank_hand(&h, &b), HandRank::ThreeOfAKind(_)));
    }

    #[test]
    fn straight() {
        let h = hole(
            card(Rank::Ace, Suit::Spades),
            card(Rank::King, Suit::Diamonds),
        );
        let b = board5(
            card(Rank::Queen, Suit::Clubs),
            card(Rank::Jack, Suit::Hearts),
            card(Rank::Ten, Suit::Diamonds),
            card(Rank::Two, Suit::Clubs),
            card(Rank::Three, Suit::Spades),
        );
        assert!(matches!(rank_hand(&h, &b), HandRank::Straight(_)));
    }

    #[test]
    fn flush() {
        let h = hole(
            card(Rank::Ace, Suit::Spades),
            card(Rank::King, Suit::Spades),
        );
        let b = board5(
            card(Rank::Queen, Suit::Spades),
            card(Rank::Jack, Suit::Spades),
            card(Rank::Nine, Suit::Spades),
            card(Rank::Two, Suit::Clubs),
            card(Rank::Three, Suit::Hearts),
        );
        assert!(matches!(rank_hand(&h, &b), HandRank::Flush(_)));
    }

    #[test]
    fn full_house() {
        let h = hole(
            card(Rank::Ace, Suit::Spades),
            card(Rank::Ace, Suit::Diamonds),
        );
        let b = board5(
            card(Rank::Ace, Suit::Clubs),
            card(Rank::King, Suit::Hearts),
            card(Rank::King, Suit::Diamonds),
            card(Rank::Two, Suit::Clubs),
            card(Rank::Three, Suit::Spades),
        );
        assert!(matches!(rank_hand(&h, &b), HandRank::FullHouse(_)));
    }

    #[test]
    fn four_of_a_kind() {
        let h = hole(
            card(Rank::Ace, Suit::Spades),
            card(Rank::Ace, Suit::Diamonds),
        );
        let b = board5(
            card(Rank::Ace, Suit::Clubs),
            card(Rank::Ace, Suit::Hearts),
            card(Rank::Seven, Suit::Diamonds),
            card(Rank::Nine, Suit::Clubs),
            card(Rank::Jack, Suit::Spades),
        );
        assert!(matches!(rank_hand(&h, &b), HandRank::FourOfAKind(_)));
    }

    #[test]
    fn straight_flush() {
        let h = hole(
            card(Rank::Ace, Suit::Spades),
            card(Rank::King, Suit::Spades),
        );
        let b = board5(
            card(Rank::Queen, Suit::Spades),
            card(Rank::Jack, Suit::Spades),
            card(Rank::Ten, Suit::Spades),
            card(Rank::Two, Suit::Clubs),
            card(Rank::Three, Suit::Hearts),
        );
        assert!(matches!(rank_hand(&h, &b), HandRank::StraightFlush(_)));
    }

    // Tiebreaker ordering tests

    #[test]
    fn pair_of_aces_beats_pair_of_kings() {
        let board = board3(
            card(Rank::Two, Suit::Clubs),
            card(Rank::Five, Suit::Diamonds),
            card(Rank::Nine, Suit::Hearts),
        );
        let aces = rank_hand(
            &hole(
                card(Rank::Ace, Suit::Spades),
                card(Rank::Ace, Suit::Diamonds),
            ),
            &board,
        );
        let kings = rank_hand(
            &hole(
                card(Rank::King, Suit::Spades),
                card(Rank::King, Suit::Diamonds),
            ),
            &board,
        );
        assert!(aces > kings);
    }

    #[test]
    fn higher_straight_beats_lower_straight() {
        let board = board3(
            card(Rank::Six, Suit::Clubs),
            card(Rank::Seven, Suit::Diamonds),
            card(Rank::Eight, Suit::Hearts),
        );
        let broadway = rank_hand(
            &hole(
                card(Rank::Nine, Suit::Spades),
                card(Rank::Ten, Suit::Diamonds),
            ),
            &board,
        );
        let lower = rank_hand(
            &hole(
                card(Rank::Four, Suit::Spades),
                card(Rank::Five, Suit::Diamonds),
            ),
            &board,
        );
        assert!(broadway > lower);
    }

    #[test]
    fn flush_beats_straight() {
        let board = board3(
            card(Rank::Six, Suit::Spades),
            card(Rank::Seven, Suit::Spades),
            card(Rank::Eight, Suit::Spades),
        );
        let flush = rank_hand(
            &hole(
                card(Rank::Nine, Suit::Spades),
                card(Rank::Two, Suit::Spades),
            ),
            &board,
        );
        let straight = rank_hand(
            &hole(
                card(Rank::Nine, Suit::Diamonds),
                card(Rank::Ten, Suit::Clubs),
            ),
            &board,
        );
        assert!(flush > straight);
    }

    #[test]
    fn full_house_beats_flush() {
        let board = board3(
            card(Rank::Ace, Suit::Clubs),
            card(Rank::Ace, Suit::Hearts),
            card(Rank::King, Suit::Spades),
        );
        let fh = rank_hand(
            &hole(
                card(Rank::Ace, Suit::Spades),
                card(Rank::King, Suit::Diamonds),
            ),
            &board,
        );
        let flush_hand = rank_hand(
            &hole(
                card(Rank::Two, Suit::Spades),
                card(Rank::Five, Suit::Spades),
            ),
            &BoardCards {
                flop: Some([
                    card(Rank::Seven, Suit::Spades),
                    card(Rank::Nine, Suit::Spades),
                    card(Rank::Jack, Suit::Spades),
                ]),
                turn: None,
                river: None,
            },
        );
        assert!(matches!(fh, HandRank::FullHouse(_)));
        assert!(matches!(flush_hand, HandRank::Flush(_)));
        assert!(fh > flush_hand);
    }

    #[test]
    fn quads_beats_full_house() {
        let board = board3(
            card(Rank::Ace, Suit::Clubs),
            card(Rank::Ace, Suit::Hearts),
            card(Rank::King, Suit::Spades),
        );
        let quads = rank_hand(
            &hole(
                card(Rank::Ace, Suit::Spades),
                card(Rank::Ace, Suit::Diamonds),
            ),
            &board,
        );
        let fh = rank_hand(
            &hole(
                card(Rank::King, Suit::Clubs),
                card(Rank::King, Suit::Diamonds),
            ),
            &board,
        );
        assert!(matches!(quads, HandRank::FourOfAKind(_)));
        assert!(matches!(fh, HandRank::FullHouse(_)));
        assert!(quads > fh);
    }

    // rank_players tests

    #[test]
    fn rank_players_sorts_winner_first() {
        let board = board5(
            card(Rank::Two, Suit::Clubs),
            card(Rank::Five, Suit::Hearts),
            card(Rank::Seven, Suit::Diamonds),
            card(Rank::Nine, Suit::Clubs),
            card(Rank::Jack, Suit::Spades),
        );
        let players = vec![
            (
                PlayerId::new(1),
                hole(card(Rank::Two, Suit::Hearts), card(Rank::Two, Suit::Spades)), // trips
            ),
            (
                PlayerId::new(2),
                hole(
                    card(Rank::Ace, Suit::Spades),
                    card(Rank::King, Suit::Diamonds),
                ), // high card
            ),
        ];
        let ranked = rank_players(&players, &board);
        assert_eq!(ranked[0].0, PlayerId::new(1)); // trips wins
        assert_eq!(ranked[1].0, PlayerId::new(2));
    }

    #[test]
    fn rank_players_three_way_tie() {
        // All players have ace-high with the same board: it's a split pot.
        let board = board5(
            card(Rank::Ace, Suit::Clubs),
            card(Rank::King, Suit::Hearts),
            card(Rank::Queen, Suit::Diamonds),
            card(Rank::Jack, Suit::Clubs),
            card(Rank::Ten, Suit::Spades),
        );
        // All players have the same straight on board — tie.
        let players = vec![
            (
                PlayerId::new(1),
                hole(
                    card(Rank::Two, Suit::Hearts),
                    card(Rank::Three, Suit::Hearts),
                ),
            ),
            (
                PlayerId::new(2),
                hole(
                    card(Rank::Four, Suit::Diamonds),
                    card(Rank::Five, Suit::Diamonds),
                ),
            ),
            (
                PlayerId::new(3),
                hole(card(Rank::Six, Suit::Clubs), card(Rank::Seven, Suit::Clubs)),
            ),
        ];
        let ranked = rank_players(&players, &board);
        // All three share the same Straight rank — confirm they all have the same rank value.
        assert!(matches!(ranked[0].1, HandRank::Straight(_)));
        assert!(matches!(ranked[1].1, HandRank::Straight(_)));
        assert!(matches!(ranked[2].1, HandRank::Straight(_)));
        assert_eq!(ranked[0].1, ranked[1].1);
        assert_eq!(ranked[1].1, ranked[2].1);
    }

    // ------------------------------------------------------------------
    // RankDescription tests
    // ------------------------------------------------------------------

    #[test]
    fn describe_hand_two_pair_aces_tens_kicker_king() {
        // AA TT board with a King as the next-best card.
        let h = hole(
            card(Rank::Ace, Suit::Spades),
            card(Rank::Ace, Suit::Diamonds),
        );
        let b = board5(
            card(Rank::Ten, Suit::Clubs),
            card(Rank::Ten, Suit::Hearts),
            card(Rank::King, Suit::Diamonds),
            card(Rank::Three, Suit::Clubs),
            card(Rank::Two, Suit::Spades),
        );
        let d = describe_hand(&h, &b);
        assert_eq!(d.category, "two_pair");
        assert_eq!(d.primary.as_deref(), Some("A"));
        assert_eq!(d.secondary.as_deref(), Some("T"));
        assert_eq!(d.kicker.as_deref(), Some("K"));
    }

    #[test]
    fn describe_hand_ace_high_flush_lists_top_five_in_descending_order() {
        let h = hole(
            card(Rank::Ace, Suit::Spades),
            card(Rank::Nine, Suit::Spades),
        );
        let b = board5(
            card(Rank::King, Suit::Spades),
            card(Rank::Seven, Suit::Spades),
            card(Rank::Two, Suit::Spades),
            card(Rank::Three, Suit::Hearts),
            card(Rank::Four, Suit::Clubs),
        );
        let d = describe_hand(&h, &b);
        assert_eq!(d.category, "flush");
        assert_eq!(d.primary.as_deref(), Some("A"));
        assert_eq!(d.board, vec!["A", "K", "9", "7", "2"]);
    }

    #[test]
    fn describe_hand_broadway_straight_ten_to_ace() {
        let h = hole(
            card(Rank::Ace, Suit::Spades),
            card(Rank::King, Suit::Diamonds),
        );
        let b = board5(
            card(Rank::Queen, Suit::Clubs),
            card(Rank::Jack, Suit::Hearts),
            card(Rank::Ten, Suit::Diamonds),
            card(Rank::Two, Suit::Clubs),
            card(Rank::Three, Suit::Spades),
        );
        let d = describe_hand(&h, &b);
        assert_eq!(d.category, "straight");
        // primary = high (A), kicker = low (T)
        assert_eq!(d.primary.as_deref(), Some("A"));
        assert_eq!(d.kicker.as_deref(), Some("T"));
        assert_eq!(d.board, vec!["A", "K", "Q", "J", "T"]);
    }

    #[test]
    fn describe_hand_wheel_straight_five_high_ace_low() {
        // A-2-3-4-5: high=5, low=A by convention.
        let h = hole(
            card(Rank::Ace, Suit::Spades),
            card(Rank::Two, Suit::Diamonds),
        );
        let b = board5(
            card(Rank::Three, Suit::Clubs),
            card(Rank::Four, Suit::Hearts),
            card(Rank::Five, Suit::Diamonds),
            card(Rank::King, Suit::Clubs),
            card(Rank::Queen, Suit::Spades),
        );
        let d = describe_hand(&h, &b);
        assert_eq!(d.category, "straight");
        assert_eq!(d.primary.as_deref(), Some("5"));
        assert_eq!(d.kicker.as_deref(), Some("A"));
        assert_eq!(d.board, vec!["5", "4", "3", "2", "A"]);
    }

    #[test]
    fn describe_hand_full_house_aces_over_kings() {
        let h = hole(
            card(Rank::Ace, Suit::Spades),
            card(Rank::Ace, Suit::Diamonds),
        );
        let b = board5(
            card(Rank::Ace, Suit::Clubs),
            card(Rank::King, Suit::Hearts),
            card(Rank::King, Suit::Diamonds),
            card(Rank::Two, Suit::Clubs),
            card(Rank::Three, Suit::Spades),
        );
        let d = describe_hand(&h, &b);
        assert_eq!(d.category, "full_house");
        assert_eq!(d.primary.as_deref(), Some("A")); // trip rank
        assert_eq!(d.secondary.as_deref(), Some("K")); // pair rank
    }

    #[test]
    fn describe_hand_quads_with_kicker() {
        let h = hole(
            card(Rank::Ace, Suit::Spades),
            card(Rank::Ace, Suit::Diamonds),
        );
        let b = board5(
            card(Rank::Ace, Suit::Clubs),
            card(Rank::Ace, Suit::Hearts),
            card(Rank::King, Suit::Diamonds),
            card(Rank::Two, Suit::Clubs),
            card(Rank::Three, Suit::Spades),
        );
        let d = describe_hand(&h, &b);
        assert_eq!(d.category, "four_of_a_kind");
        assert_eq!(d.primary.as_deref(), Some("A"));
        assert_eq!(d.kicker.as_deref(), Some("K"));
    }

    #[test]
    fn describe_hand_royal_flush_high_ace() {
        let h = hole(
            card(Rank::Ace, Suit::Spades),
            card(Rank::King, Suit::Spades),
        );
        let b = board5(
            card(Rank::Queen, Suit::Spades),
            card(Rank::Jack, Suit::Spades),
            card(Rank::Ten, Suit::Spades),
            card(Rank::Two, Suit::Clubs),
            card(Rank::Three, Suit::Hearts),
        );
        let d = describe_hand(&h, &b);
        assert_eq!(d.category, "straight_flush");
        assert_eq!(d.primary.as_deref(), Some("A"));
        assert_eq!(d.board, vec!["A", "K", "Q", "J", "T"]);
    }

    #[test]
    fn describe_hand_one_pair_includes_top_kicker() {
        let h = hole(
            card(Rank::Ace, Suit::Spades),
            card(Rank::King, Suit::Diamonds),
        );
        let b = board5(
            card(Rank::Ace, Suit::Clubs),
            card(Rank::Seven, Suit::Hearts),
            card(Rank::Four, Suit::Diamonds),
            card(Rank::Two, Suit::Clubs),
            card(Rank::Three, Suit::Spades),
        );
        let d = describe_hand(&h, &b);
        assert_eq!(d.category, "one_pair");
        assert_eq!(d.primary.as_deref(), Some("A"));
        assert_eq!(d.kicker.as_deref(), Some("K"));
    }

    #[test]
    fn describe_hand_high_card_top_two_ranks() {
        let h = hole(
            card(Rank::Ace, Suit::Spades),
            card(Rank::Queen, Suit::Diamonds),
        );
        let b = board5(
            card(Rank::Nine, Suit::Clubs),
            card(Rank::Seven, Suit::Hearts),
            card(Rank::Four, Suit::Diamonds),
            card(Rank::Two, Suit::Clubs),
            card(Rank::Three, Suit::Spades),
        );
        let d = describe_hand(&h, &b);
        assert_eq!(d.category, "high_card");
        assert_eq!(d.primary.as_deref(), Some("A"));
        assert_eq!(d.kicker.as_deref(), Some("Q"));
    }

    #[test]
    fn describe_hand_serializes_with_skips() {
        // Verify the optional fields are skipped when None / empty (so wire JSON
        // doesn't carry noise).
        let h = hole(
            card(Rank::Ace, Suit::Spades),
            card(Rank::Ace, Suit::Diamonds),
        );
        let b = board5(
            card(Rank::Ace, Suit::Clubs),
            card(Rank::Ace, Suit::Hearts),
            card(Rank::King, Suit::Diamonds),
            card(Rank::Two, Suit::Clubs),
            card(Rank::Three, Suit::Spades),
        );
        let d = describe_hand(&h, &b);
        let json = serde_json::to_string(&d).unwrap();
        assert!(json.contains("\"category\":\"four_of_a_kind\""), "{json}");
        assert!(json.contains("\"primary\":\"A\""), "{json}");
        assert!(json.contains("\"kicker\":\"K\""), "{json}");
        // secondary is None and board is empty → omitted from JSON.
        assert!(!json.contains("\"secondary\""), "{json}");
        assert!(!json.contains("\"board\""), "{json}");
    }
}
