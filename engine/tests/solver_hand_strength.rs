//! ADR-043 §3.3 — one test per HandStrength variant.
//!
//! Engineers a representative hand for each band.

use engine::card::{Card, Rank, Suit};
use engine::hand::{BoardCards, HoleCards};
use engine::solver::{classify, HandStrength};

fn c(r: Rank, s: Suit) -> Card {
    Card::new(r, s)
}

fn h(c1: Card, c2: Card) -> HoleCards {
    HoleCards::new(c1, c2)
}

fn b3(c1: Card, c2: Card, c3: Card) -> BoardCards {
    BoardCards {
        flop: Some([c1, c2, c3]),
        turn: None,
        river: None,
    }
}

#[test]
fn pure_bluff_no_equity_band() {
    // 7-2 on A-K-Q rainbow — no pair, no straight/flush draw.
    let hole = h(c(Rank::Seven, Suit::Diamonds), c(Rank::Two, Suit::Hearts));
    let board = b3(
        c(Rank::Ace, Suit::Spades),
        c(Rank::King, Suit::Clubs),
        c(Rank::Queen, Suit::Hearts),
    );
    assert_eq!(classify(hole, &board), HandStrength::PureBluffNoEquity);
}

#[test]
fn draw_weak_band() {
    // 9J on T-K-4 — gutshot to Q only.
    let hole = h(c(Rank::Nine, Suit::Diamonds), c(Rank::Jack, Suit::Clubs));
    let board = b3(
        c(Rank::Ten, Suit::Hearts),
        c(Rank::King, Suit::Spades),
        c(Rank::Four, Suit::Diamonds),
    );
    assert_eq!(classify(hole, &board), HandStrength::DrawWeak);
}

#[test]
fn draw_strong_band() {
    // AhKh on Jh-7c-2h — nut flush draw.
    let hole = h(c(Rank::Ace, Suit::Hearts), c(Rank::King, Suit::Hearts));
    let board = b3(
        c(Rank::Jack, Suit::Hearts),
        c(Rank::Seven, Suit::Clubs),
        c(Rank::Two, Suit::Hearts),
    );
    assert_eq!(classify(hole, &board), HandStrength::DrawStrong);
}

#[test]
fn pair_weak_band() {
    // 5-5 on K-Q-8 — underpair.
    let hole = h(c(Rank::Five, Suit::Spades), c(Rank::Five, Suit::Hearts));
    let board = b3(
        c(Rank::King, Suit::Spades),
        c(Rank::Queen, Suit::Hearts),
        c(Rank::Eight, Suit::Clubs),
    );
    assert_eq!(classify(hole, &board), HandStrength::PairWeak);
}

#[test]
fn pair_middle_band() {
    // 8x on K-8-3.
    let hole = h(c(Rank::Eight, Suit::Diamonds), c(Rank::Four, Suit::Hearts));
    let board = b3(
        c(Rank::King, Suit::Spades),
        c(Rank::Eight, Suit::Hearts),
        c(Rank::Three, Suit::Clubs),
    );
    assert_eq!(classify(hole, &board), HandStrength::PairMiddle);
}

#[test]
fn pair_top_weak_kicker_band() {
    // K6 on K-8-3 — top pair, low kicker.
    let hole = h(c(Rank::King, Suit::Diamonds), c(Rank::Six, Suit::Hearts));
    let board = b3(
        c(Rank::King, Suit::Spades),
        c(Rank::Eight, Suit::Hearts),
        c(Rank::Three, Suit::Clubs),
    );
    assert_eq!(classify(hole, &board), HandStrength::PairTopWeakKicker);
}

#[test]
fn pair_top_strong_kicker_band() {
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
fn overpair_band() {
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
fn two_pair_band() {
    let hole = h(c(Rank::Ace, Suit::Spades), c(Rank::King, Suit::Hearts));
    let board = b3(
        c(Rank::Ace, Suit::Diamonds),
        c(Rank::King, Suit::Clubs),
        c(Rank::Three, Suit::Clubs),
    );
    assert_eq!(classify(hole, &board), HandStrength::TwoPair);
}

#[test]
fn set_band() {
    let hole = h(c(Rank::Eight, Suit::Spades), c(Rank::Eight, Suit::Hearts));
    let board = b3(
        c(Rank::King, Suit::Spades),
        c(Rank::Eight, Suit::Diamonds),
        c(Rank::Three, Suit::Clubs),
    );
    assert_eq!(classify(hole, &board), HandStrength::Set);
}

#[test]
fn straight_band() {
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
fn flush_band() {
    let hole = h(c(Rank::Ace, Suit::Hearts), c(Rank::King, Suit::Hearts));
    let board = b3(
        c(Rank::Jack, Suit::Hearts),
        c(Rank::Seven, Suit::Hearts),
        c(Rank::Two, Suit::Hearts),
    );
    assert_eq!(classify(hole, &board), HandStrength::Flush);
}

#[test]
fn full_house_plus_band() {
    let hole = h(c(Rank::Ace, Suit::Spades), c(Rank::Ace, Suit::Hearts));
    let board = BoardCards {
        flop: Some([
            c(Rank::Ace, Suit::Diamonds),
            c(Rank::Ace, Suit::Clubs),
            c(Rank::King, Suit::Hearts),
        ]),
        turn: Some(c(Rank::Three, Suit::Hearts)),
        river: Some(c(Rank::Three, Suit::Diamonds)),
    };
    assert_eq!(classify(hole, &board), HandStrength::FullHousePlus);
}
