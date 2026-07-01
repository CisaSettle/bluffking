//! Performance baseline + regression bench for the equity / hand-eval hot path.
//!
//! Two cost centers drive backend CPU on the coach + live-snapshot surfaces:
//!   1. `engine::rank_hand` — the per-7-card evaluator (called hero + each
//!      opponent, every Monte Carlo trial).
//!   2. `engine::solver::equity::equity` — the MC loop (deck draw + showdown),
//!      run at 10k trials by the coach solver (advisor.rs) and ~600 iters by
//!      the live snapshot equity surface (server::equity, same algorithm).
//!
//! These scenarios mirror the real call sites so a before/after run proves the
//! optimization on the actual hot path, not a synthetic micro-bench.
//!
//! Run:  cargo bench -p engine --bench equity_bench

use criterion::{criterion_group, criterion_main, Criterion};
use engine::solver::equity::{equity, EquityInput, OpponentSpec};
use engine::{rank_hand, BoardCards, Card, HoleCards, Rank, Suit};
use std::hint::black_box;

fn card(r: Rank, s: Suit) -> Card {
    Card::new(r, s)
}

fn aa() -> HoleCards {
    HoleCards::new(card(Rank::Ace, Suit::Spades), card(Rank::Ace, Suit::Hearts))
}

fn flop_board() -> BoardCards {
    BoardCards {
        flop: Some([
            card(Rank::King, Suit::Diamonds),
            card(Rank::Seven, Suit::Clubs),
            card(Rank::Two, Suit::Hearts),
        ]),
        turn: None,
        river: None,
    }
}

/// Isolate the inner evaluator: full 7-card hand (2 hole + 5 board).
fn bench_rank_hand(crit: &mut Criterion) {
    let hero = HoleCards::new(
        card(Rank::Ace, Suit::Spades),
        card(Rank::King, Suit::Spades),
    );
    let board = BoardCards {
        flop: Some([
            card(Rank::Queen, Suit::Spades),
            card(Rank::Jack, Suit::Spades),
            card(Rank::Two, Suit::Hearts),
        ]),
        turn: Some(card(Rank::Five, Suit::Diamonds)),
        river: Some(card(Rank::Nine, Suit::Clubs)),
    };
    crit.bench_function("rank_hand_7card", |b| {
        b.iter(|| rank_hand(black_box(&hero), black_box(&board)))
    });
}

/// The end-to-end MC equity calls at the real trial counts / shapes.
fn bench_equity(crit: &mut Criterion) {
    let mut g = crit.benchmark_group("equity");
    g.sample_size(20);

    // Coach solver heavy path: preflop, 1 opponent, 10k trials (advisor.rs).
    g.bench_function("preflop_1opp_10k", |b| {
        b.iter(|| {
            equity(black_box(EquityInput {
                hero: aa(),
                board: BoardCards::empty(),
                opponents: OpponentSpec::Random(1),
                trials: 10_000,
                seed: 0xC0FFEE,
                early_stop: None,
            }))
        })
    });

    // Coach solver heaviest realistic spot: preflop, 3 opponents, 10k trials.
    g.bench_function("preflop_3opp_10k", |b| {
        b.iter(|| {
            equity(black_box(EquityInput {
                hero: aa(),
                board: BoardCards::empty(),
                opponents: OpponentSpec::Random(3),
                trials: 10_000,
                seed: 0xC0FFEE,
                early_stop: None,
            }))
        })
    });

    // Live snapshot gameplay path proxy: flop, 2 opponents, 600 iters
    // (server::equity::compute_equity DEFAULT_ITERATIONS = 600, same algorithm).
    g.bench_function("flop_2opp_600", |b| {
        b.iter(|| {
            equity(black_box(EquityInput {
                hero: aa(),
                board: flop_board(),
                opponents: OpponentSpec::Random(2),
                trials: 600,
                seed: 0xC0FFEE,
                early_stop: None,
            }))
        })
    });

    g.finish();
}

criterion_group!(benches, bench_rank_hand, bench_equity);
criterion_main!(benches);
