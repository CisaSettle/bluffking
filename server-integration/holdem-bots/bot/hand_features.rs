//! Hand features for bot.

use super::*;

// ---------------------------------------------------------------------------
// Postflop classifiers
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub(super) enum MadeHand {
    Air,
    /// The board already supplies the made-hand category and the hole cards
    /// contribute only kickers (or do not improve the board's exact
    /// straight/flush+ rank). This is deliberately weaker than `WeakMade`:
    /// every remaining player shares the core hand, so it must never inherit
    /// the automatic OnePair/TripsPlus continue and raise branches.
    BoardMade,
    /// ADR-047 §5.4 — marginal showdown bucket between Air and OnePair.
    WeakMade,
    OnePair,
    TwoPair,
    TripsPlus,
    StraightFlushPlus,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BoardTexture {
    Dry,
    Wet,
}

pub(super) fn made_hand_bucket(hole: HoleCards, board: &BoardCards) -> MadeHand {
    if board.count() == 0 {
        return MadeHand::Air;
    }
    made_hand_bucket_with_rank(engine::rank_hand(&hole, board), hole, board)
}

/// [`made_hand_bucket`] for a precomputed seven-card rank (range enumeration
/// ranks each combination once).
pub(super) fn made_hand_bucket_with_rank(
    rank: HandRank,
    hole: HoleCards,
    board: &BoardCards,
) -> MadeHand {
    if board_supplies_made_hand(rank, hole, board) {
        return MadeHand::BoardMade;
    }
    match rank {
        HandRank::StraightFlush(_) | HandRank::FourOfAKind(_) | HandRank::FullHouse(_) => {
            MadeHand::StraightFlushPlus
        }
        HandRank::Flush(_) | HandRank::Straight(_) | HandRank::ThreeOfAKind(_) => {
            MadeHand::TripsPlus
        }
        HandRank::TwoPair(_) => {
            // BUG-209 — "two pair" that is only a board pair plus a hole-card
            // pair BELOW the board's top rank (4x on 4-A-J-A, 55 on A-A-J-4)
            // is bottom/second pair in strength: every A-x has trips and every
            // higher pair-with-board beats it. Rate it like the equivalent
            // one-pair hand instead of the genuine two-pair bucket.
            match pseudo_two_pair_below_board_top(hole, board) {
                Some(bucket) => bucket,
                None => MadeHand::TwoPair,
            }
        }
        HandRank::OnePair(_) => {
            // ADR-047 §5.4 — underpair on overcard board drops to WeakMade.
            if has_underpair(hole, board) {
                MadeHand::WeakMade
            } else {
                MadeHand::OnePair
            }
        }
        HandRank::HighCard(_) => {
            // Ace-high with backdoor gets promoted to WeakMade.
            if hole_has_ace(hole) && has_backdoor(hole, board) {
                MadeHand::WeakMade
            } else {
                MadeHand::Air
            }
        }
    }
}

/// BUG-209 — a seven-card `TwoPair` whose board contribution is only paired
/// ranks (one or two plain pairs), while the hole cards add a pair strictly
/// BELOW the board's top rank. `A-A-J-4 + 4x` is aces-and-fours on paper but
/// plays as bottom pair (any A-x has trips, any J-x has a higher two pair);
/// likewise a third pair on a double-paired board (`9-9-5-5-2 + 77`,
/// `9-9-5-5-7 + 7x`) only nudges the board's own two pair (any 9-x or 5-x is a
/// full house, any higher pocket pair outranks it). Neither may inherit the
/// genuine `TwoPair` continue/raise branches. Returns the one-pair-equivalent
/// bucket: `WeakMade` for a pocket underpair (mirrors `has_underpair`), else
/// `OnePair` (bottom/second pair). `None` when the two pair is real: top pair
/// or an overpair with the board pair (`8-8-K-2-3 + Kx`, `8-8-K + AA`,
/// `K-9-9-5-5 + Kx`, `9-9-5-5-2 + JJ`). A hole pair below both board pairs
/// never reaches here (`board_supplies_made_hand` rates it `BoardMade`).
pub(super) fn pseudo_two_pair_below_board_top(
    hole: HoleCards,
    board: &BoardCards,
) -> Option<MadeHand> {
    let cards = board.all_cards();
    let mut rank_counts = [0u8; 13];
    for card in &cards {
        rank_counts[rank_index(card.rank) as usize] += 1;
    }
    let paired_ranks: Vec<usize> = rank_counts
        .iter()
        .enumerate()
        .filter_map(|(rank, &count)| (count >= 2).then_some(rank))
        .collect();
    // One or two paired ranks on the board, each a plain pair (a tripled rank
    // cannot combine with a hole pair into a seven-card TwoPair).
    if !matches!(paired_ranks.len(), 1 | 2) || paired_ranks.iter().any(|&r| rank_counts[r] != 2) {
        return None;
    }
    let top = board_top_rank_idx(board);
    let r1 = rank_index(hole.card1.rank);
    let r2 = rank_index(hole.card2.rank);
    if r1 == r2 {
        // Pocket pair (not on the board, else it would be trips+): the second
        // pair is the pocket pair itself.
        return (r1 < top).then_some(MadeHand::WeakMade);
    }
    // Unpaired hole: the hole pair is the highest hole card that matches a
    // (non-paired) board rank.
    let hole_pair = [r1, r2]
        .into_iter()
        .filter(|&r| rank_counts[r as usize] == 1)
        .max()?;
    (hole_pair < top).then_some(MadeHand::OnePair)
}

/// Whether the community cards already provide the core made-hand category.
///
/// Pair/trips are treated as shared whenever the board itself has the relevant
/// rank multiplicity: a hole-card kicker does not turn shared 8s or shared trip
/// aces into an ordinary pair/trips value hand. Two-pair compares the actual
/// paired ranks so a pocket pair that displaces the board's lower pair remains
/// a genuine improvement. For made hands that require five cards, exact
/// board-rank equality preserves genuine hole-card improvements.
pub(super) fn board_supplies_made_hand(
    rank: HandRank,
    hole: HoleCards,
    board: &BoardCards,
) -> bool {
    let cards = board.all_cards();
    let mut rank_counts = [0u8; 13];
    for card in &cards {
        rank_counts[rank_index(card.rank) as usize] += 1;
    }

    // `rank` is the final seven-card category, not the board's category. This
    // makes the multiplicity checks deliberately asymmetric:
    //
    // - board 8-8-K-2-3 + Kx ranks as TwoPair, but the board has only one
    //   paired rank, so the hole-card improvement is preserved;
    // - the same board + 8x ranks as Trips, but the board has no tripled rank,
    //   so the hole-card improvement is preserved;
    // - the same board + Ax still ranks as OnePair, so only the shared pair is
    //   downgraded even though the ace improves its kicker.
    //
    // Exact HandRank equality is unsuitable for pair/trips because it would
    // treat kicker-only improvements as new made hands and recreate the two
    // PRD-002 river failures.
    match rank {
        HandRank::OnePair(_) => rank_counts.iter().any(|&count| count >= 2),
        HandRank::TwoPair(_) => {
            let board_pairs = best_two_paired_ranks(&rank_counts);
            let mut combined_counts = rank_counts;
            combined_counts[rank_index(hole.card1.rank) as usize] += 1;
            combined_counts[rank_index(hole.card2.rank) as usize] += 1;
            board_pairs.is_some() && best_two_paired_ranks(&combined_counts) == board_pairs
        }
        HandRank::ThreeOfAKind(_) => rank_counts.iter().any(|&count| count >= 3),
        HandRank::Straight(_)
        | HandRank::Flush(_)
        | HandRank::FullHouse(_)
        | HandRank::FourOfAKind(_)
        | HandRank::StraightFlush(_) => rank_board_only(board) == Some(rank),
        HandRank::HighCard(_) => false,
    }
}

/// Highest and second-highest ranks with at least two cards.
pub(super) fn best_two_paired_ranks(rank_counts: &[u8; 13]) -> Option<[usize; 2]> {
    let mut paired = rank_counts
        .iter()
        .enumerate()
        .rev()
        .filter_map(|(rank, &count)| (count >= 2).then_some(rank));
    Some([paired.next()?, paired.next()?])
}

/// Evaluate the five-card board as a standalone poker hand. `rank_hand` is
/// card-position agnostic (best five from all supplied cards), so splitting
/// the board into two pseudo hole cards plus a three-card pseudo flop gives
/// the exact board-only rank without introducing a second evaluator.
pub(super) fn rank_board_only(board: &BoardCards) -> Option<HandRank> {
    let cards = board.all_cards();
    if cards.len() != 5 {
        return None;
    }
    let pseudo_hole = HoleCards::new(cards[0], cards[1]);
    let pseudo_board = BoardCards {
        flop: Some([cards[2], cards[3], cards[4]]),
        turn: None,
        river: None,
    };
    Some(engine::rank_hand(&pseudo_hole, &pseudo_board))
}

/// Whether an exact five-card board hand is impossible for either private card
/// to improve. These boards are forced chops, so folding would be a poker-rules
/// error and raising would manufacture value that every remaining player owns.
pub(super) fn board_made_hand_is_unbeatable(rank: HandRank, board: &BoardCards) -> bool {
    let cards = board.all_cards();
    if cards.len() != 5 || rank_board_only(board) != Some(rank) {
        return false;
    }

    match rank {
        HandRank::StraightFlush(_) => {
            // A lower board straight flush can still be improved by a higher
            // suited private card. Only ten-to-ace (a royal flush) is locked.
            let suit = cards[0].suit;
            cards.iter().all(|card| card.suit == suit)
                && [Rank::Ten, Rank::Jack, Rank::Queen, Rank::King, Rank::Ace]
                    .iter()
                    .all(|rank| cards.iter().any(|card| card.rank == *rank))
        }
        HandRank::FourOfAKind(_) => {
            let mut rank_counts = [0u8; 13];
            for card in &cards {
                rank_counts[rank_index(card.rank) as usize] += 1;
            }

            let Some(quad_idx) = rank_counts.iter().position(|&count| count == 4) else {
                return false;
            };
            let Some(kicker_idx) = rank_counts.iter().position(|&count| count == 1) else {
                return false;
            };
            let highest_legal_kicker = if quad_idx == rank_index(Rank::Ace) as usize {
                rank_index(Rank::King) as usize
            } else {
                rank_index(Rank::Ace) as usize
            };
            kicker_idx == highest_legal_kicker
        }
        _ => false,
    }
}

/// Pocket pair strictly below the highest board rank (ADR-047 §5.4).
pub(super) fn has_underpair(hole: HoleCards, board: &BoardCards) -> bool {
    let r1 = hole.card1.rank;
    let r2 = hole.card2.rank;
    if r1 != r2 {
        return false;
    }
    rank_index(r1) < board_top_rank_idx(board)
}

/// True if the made one-pair is "strong" — top pair (a hole card pairs the
/// highest board rank) or an over-pair (a pocket pair above the board top).
/// Used to thin-value-bet against a field that calls too wide (ADR-024 leak
/// fix, measured via gto_selfplay).
pub(super) fn has_top_pair_or_overpair(hole: HoleCards, board: &BoardCards) -> bool {
    let top = board_top_rank_idx(board);
    if top < 0 {
        return false;
    }
    let r1 = rank_index(hole.card1.rank);
    let r2 = rank_index(hole.card2.rank);
    // Over-pair: pocket pair strictly above the board top.
    if hole.card1.rank == hole.card2.rank {
        return r1 > top;
    }
    // Top pair: one hole card matches the highest board rank.
    r1 == top || r2 == top
}

/// Either card in `hole` is an Ace.
pub(super) fn hole_has_ace(hole: HoleCards) -> bool {
    hole.card1.rank == Rank::Ace || hole.card2.rank == Rank::Ace
}

/// Backdoor draw on the flop (ADR-047 §5.4). Only meaningful on the flop.
pub(super) fn has_backdoor(hole: HoleCards, board: &BoardCards) -> bool {
    let flop = match board.flop {
        Some(f) => f,
        None => return false,
    };
    if board.turn.is_some() || board.river.is_some() {
        return false;
    }
    let cards = [hole.card1, hole.card2, flop[0], flop[1], flop[2]];
    let mut suit_counts = [0u8; 4];
    for c in cards.iter() {
        suit_counts[c.suit as usize] += 1;
    }
    let backdoor_flush = suit_counts.iter().any(|&n| n >= 3);

    let mut rank_set: Vec<i32> = cards.iter().map(|c| rank_index(c.rank)).collect();
    rank_set.sort();
    rank_set.dedup();
    let mut backdoor_straight = false;
    for start in 0..=8i32 {
        let in_window = rank_set
            .iter()
            .filter(|&&r| r >= start && r <= start + 4)
            .count();
        if in_window >= 3 {
            backdoor_straight = true;
            break;
        }
    }
    backdoor_flush || backdoor_straight
}

/// Highest board rank index, or -1 if board is empty.
pub(super) fn board_top_rank_idx(board: &BoardCards) -> i32 {
    let mut top: i32 = -1;
    if let Some(flop) = board.flop {
        for c in flop.iter() {
            let i = rank_index(c.rank);
            if i > top {
                top = i;
            }
        }
    }
    if let Some(t) = board.turn {
        let i = rank_index(t.rank);
        if i > top {
            top = i;
        }
    }
    if let Some(r) = board.river {
        let i = rank_index(r.rank);
        if i > top {
            top = i;
        }
    }
    top
}

/// Draw strength in approximate outs (for flop/turn; 0 on river since no more cards).
pub(super) fn draw_strength(hole: HoleCards, board: &BoardCards) -> u8 {
    let features = engine::solver::draws::draws(&hole, board);
    let flush_draw = features.flush;
    let oesd = features.open_ended;
    let gutshot = features.gutshot;

    // Combo draw: 12+ outs.
    if flush_draw && (oesd || gutshot) {
        return 12;
    }
    if flush_draw {
        return 9;
    }
    if oesd {
        return 8;
    }
    if gutshot {
        return 4;
    }
    0
}

pub(super) fn board_texture(board: &BoardCards) -> BoardTexture {
    let flop = match board.flop {
        Some(f) => f,
        None => return BoardTexture::Dry,
    };

    // Flush draw possible: any two flop cards share a suit.
    // (Turn/river cards don't retroactively affect whether the flop texture is wet.)
    let [a, b, c] = flop;
    let flush_draw = a.suit == b.suit || b.suit == c.suit || a.suit == c.suit;

    // Straight draw possible (ADR-024 §4.2):
    //   - Sort the three ranks.
    //   - Spread (max-min) must be <=4 (fits inside a 5-rank window).
    //   - AND at least two adjacent ranks must be within gap=2 of each other
    //     (open-ender or close gutshot — "two_card_close").
    //   Without two_card_close, a board like 2-6-K (spread=4 across just the 2&6
    //   pair) would wrongly qualify; the old code made this mistake.
    let mut ranks = [rank_index(a.rank), rank_index(b.rank), rank_index(c.rank)];
    ranks.sort();
    let spread = ranks[2] - ranks[0];
    let two_card_close = (ranks[1] - ranks[0] <= 2) || (ranks[2] - ranks[1] <= 2);
    let straight_draw = spread <= 4 && two_card_close;

    if flush_draw || straight_draw {
        BoardTexture::Wet
    } else {
        BoardTexture::Dry
    }
}

// ---------------------------------------------------------------------------
// Rank helpers (re-used from original bot.rs)
// ---------------------------------------------------------------------------

/// Sort hole card ranks high → low.
pub(super) fn sorted_ranks(hole: HoleCards) -> (Rank, Rank) {
    let r1 = hole.card1.rank;
    let r2 = hole.card2.rank;
    if r1 >= r2 {
        (r1, r2)
    } else {
        (r2, r1)
    }
}

pub(super) fn rank_index(r: Rank) -> i32 {
    match r {
        Rank::Two => 0,
        Rank::Three => 1,
        Rank::Four => 2,
        Rank::Five => 3,
        Rank::Six => 4,
        Rank::Seven => 5,
        Rank::Eight => 6,
        Rank::Nine => 7,
        Rank::Ten => 8,
        Rank::Jack => 9,
        Rank::Queen => 10,
        Rank::King => 11,
        Rank::Ace => 12,
    }
}

pub(super) fn rank_chen_value(r: Rank) -> i32 {
    match r {
        Rank::Ace => 10,
        Rank::King => 8,
        Rank::Queen => 7,
        Rank::Jack => 6,
        Rank::Ten => 5,
        Rank::Nine => 4,
        Rank::Eight | Rank::Seven => 3,
        Rank::Six | Rank::Five => 2,
        Rank::Four | Rank::Three | Rank::Two => 1,
    }
}
