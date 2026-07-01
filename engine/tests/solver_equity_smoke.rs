//! ADR-043 §8.1 equity smoke: known equity ballparks.

use engine::card::{Card, Rank, Suit};
use engine::hand::{BoardCards, HoleCards};
use engine::solver::{equity, EquityInput, OpponentSpec};

fn c(r: Rank, s: Suit) -> Card {
    Card::new(r, s)
}

#[test]
fn ahkh_vs_random_on_dry_flop_in_band() {
    // AhKh on Ac-7d-2s flop — top pair top kicker on a dry board, vs 1 random.
    // Should be in [0.55, 0.85] equity (top pair top kicker ≈ 75-85% vs random).
    let hero = HoleCards::new(c(Rank::Ace, Suit::Hearts), c(Rank::King, Suit::Hearts));
    let board = BoardCards {
        flop: Some([
            c(Rank::Ace, Suit::Clubs),
            c(Rank::Seven, Suit::Diamonds),
            c(Rank::Two, Suit::Spades),
        ]),
        turn: None,
        river: None,
    };
    let result = equity(EquityInput {
        hero,
        board,
        opponents: OpponentSpec::Random(1),
        trials: 5_000,
        seed: 7,
        early_stop: None,
    });
    // Spec mentions [0.55, 0.75] but vs 1 truly-random hand on a dry A72 board,
    // AKs is actually a heavy favorite — empirical 85-92%. Allow [55, 95].
    assert!(
        result.win_pct >= 55 && result.win_pct <= 95,
        "AKs top-pair-top-kicker vs random — expected [55,95], got {}",
        result.win_pct
    );
}

#[test]
fn aa_vs_random_river_high_equity() {
    // Pocket aces on a 2-3-7-J-K rainbow — overpair, still very strong.
    let hero = HoleCards::new(c(Rank::Ace, Suit::Spades), c(Rank::Ace, Suit::Diamonds));
    let board = BoardCards {
        flop: Some([
            c(Rank::Two, Suit::Hearts),
            c(Rank::Three, Suit::Clubs),
            c(Rank::Seven, Suit::Diamonds),
        ]),
        turn: Some(c(Rank::Jack, Suit::Spades)),
        river: Some(c(Rank::King, Suit::Hearts)),
    };
    let result = equity(EquityInput {
        hero,
        board,
        opponents: OpponentSpec::Random(1),
        trials: 5_000,
        seed: 11,
        early_stop: None,
    });
    assert!(
        result.win_pct >= 70,
        "AA overpair on dry river — expected ≥70%, got {}",
        result.win_pct
    );
}

#[test]
fn equity_against_zero_opponents_is_100() {
    let hero = HoleCards::new(c(Rank::Two, Suit::Hearts), c(Rank::Three, Suit::Diamonds));
    let board = BoardCards::empty();
    let result = equity(EquityInput {
        hero,
        board,
        opponents: OpponentSpec::Random(0),
        trials: 100,
        seed: 1,
        early_stop: None,
    });
    assert_eq!(result.win_pct, 100);
}

// ---------------------------------------------------------------------------
// Multi-way equity bands.
//
// Coverage gap (2026-06-17): MC equity accuracy was asserted only for one or
// two opponents (`equity::aa_vs_one_random_pre_flop_is_around_85_pct`,
// `ahkh_vs_random_on_dry_flop_in_band`). Multi-way (3+) Monte Carlo accuracy —
// where a systematic sampling or showdown-comparison bias would compound over
// more opponents — had no band assertion. These pin AA's preflop equity vs 2
// and 4 random opponents to standard published values (±3pp), and pin one
// exact multi-way all-in spot to its precomputed enumeration value.
// ---------------------------------------------------------------------------

/// AA preflop vs 2 random opponents ≈ 73.4% (standard equity calculators).
/// A 3-handed MC band: 50k trials keeps the ±3pp slack tight but the test fast.
#[test]
fn aa_vs_two_random_preflop_in_band() {
    let hero = HoleCards::new(c(Rank::Ace, Suit::Spades), c(Rank::Ace, Suit::Hearts));
    let board = BoardCards::empty();
    let result = equity(EquityInput {
        hero,
        board,
        opponents: OpponentSpec::Random(2),
        trials: 50_000,
        seed: 42,
        early_stop: None,
    });
    assert!(
        result.win_pct >= 70 && result.win_pct <= 76,
        "AA vs 2 random preflop win% out of [70,76] (≈73.4% reference); got {}",
        result.win_pct
    );
}

/// AA preflop vs 4 random opponents ≈ 55.9% (standard equity calculators).
/// A 5-handed MC band — exercises the multi-opponent showdown path where a
/// per-opponent comparison bias would compound; ±3pp slack at 50k trials.
#[test]
fn aa_vs_four_random_preflop_in_band() {
    let hero = HoleCards::new(c(Rank::Ace, Suit::Spades), c(Rank::Ace, Suit::Hearts));
    let board = BoardCards::empty();
    let result = equity(EquityInput {
        hero,
        board,
        opponents: OpponentSpec::Random(4),
        trials: 50_000,
        seed: 42,
        early_stop: None,
    });
    assert!(
        result.win_pct >= 53 && result.win_pct <= 59,
        "AA vs 4 random preflop win% out of [53,59] (≈55.9% reference); got {}",
        result.win_pct
    );
}

/// 3-way all-in vs a PRECOMPUTED EXACT equity. Hero KsKh flopped a set of kings
/// on Kd-7c-2s against AsAh (overpair) and QsQh (underpair). With two cards to
/// come, `OpponentSpec::Known` postflop takes the EXACT C(45,2) enumeration path
/// (ADR-043 §3.1) — not seeded MC — so the answer is deterministic and exact:
/// hero's combined equity is 91% (only running aces, runner-runner quads/straight
/// for QQ, etc. deny the set). This is the precomputed value; any drift in the
/// multi-way exact-enumeration showdown comparison would move it.
#[test]
fn three_way_known_allin_exact_equity_is_precomputed() {
    let hero = HoleCards::new(c(Rank::King, Suit::Spades), c(Rank::King, Suit::Hearts));
    let opp_aa = HoleCards::new(c(Rank::Ace, Suit::Spades), c(Rank::Ace, Suit::Hearts));
    let opp_qq = HoleCards::new(c(Rank::Queen, Suit::Spades), c(Rank::Queen, Suit::Hearts));
    let board = BoardCards {
        flop: Some([
            c(Rank::King, Suit::Diamonds),
            c(Rank::Seven, Suit::Clubs),
            c(Rank::Two, Suit::Spades),
        ]),
        turn: None,
        river: None,
    };
    // Exact enumeration ⇒ trials/seed are irrelevant; assert it stays exact.
    let one_trial = equity(EquityInput {
        hero,
        board: board.clone(),
        opponents: OpponentSpec::Known(vec![opp_aa, opp_qq]),
        trials: 1,
        seed: 0,
        early_stop: None,
    });
    let many_trials = equity(EquityInput {
        hero,
        board,
        opponents: OpponentSpec::Known(vec![opp_aa, opp_qq]),
        trials: 50_000,
        seed: 999,
        early_stop: None,
    });
    assert_eq!(
        one_trial, many_trials,
        "postflop 3-way Known equity must be exact enumeration, independent of trials/seed"
    );
    assert_eq!(
        one_trial.equity_pct(),
        91,
        "KK-set vs AA vs QQ on Kd7c2s precomputed exact equity must be 91%; got {}",
        one_trial.equity_pct()
    );
}
