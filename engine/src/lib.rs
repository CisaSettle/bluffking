//! Texas Hold'em poker engine.
//!
//! Pure logic crate — no async, no IO, no DB. Given a player list, dealer
//! position, and RNG, plays a complete hand and returns a [`HandResult`].
//!
//! # Design constraints
//! - No `rs_poker` types in any public signature (see ADR-012).
//! - No `tokio`, `sqlx`, or `axum` dependencies.
//! - Public *data* types (cards, actions, snapshots, results) derive
//!   `Debug + Clone` and usually `PartialEq`; stateful handles such as
//!   [`GameHand`] and [`rng::PokerRng`] intentionally derive none of these.
//!   (U44, dual-AI OSS review: the old "all public types" claim was false.)
//! - `cargo test -p engine` runs without a live DB.

// Rust review standard (L2): pure-logic crate — forbid `unsafe` outright so any
// future `unsafe` is a hard compile error, not a review judgement call.
#![forbid(unsafe_code)]

pub mod action;
pub mod card;
pub mod deck;
pub mod eval;
pub mod event;
pub mod game;
pub mod hand;
pub mod player;
pub mod rng;
pub mod round;
pub mod solver;

// Re-export the main public API.
pub use action::{ActionError, ActionRecord, BlindKind, PlayerAction};
pub use card::{Card, Rank, Suit};
pub use deck::Deck;
pub use eval::{describe_hand, rank_hand, rank_players, HandRank, RankDescription};
pub use event::EngineEvent;
pub use game::{
    blind_positions, BlindFinishError, BlindInjectError, GameHand, GameSnapshot, HandResult,
    HandSeat, HoleSlot,
};
pub use hand::{BoardCards, HoleCards, Street};
pub use player::{Chips, PlayerId, Position, Seat};
pub use rng::{DeckSeed, PokerRng};
pub use round::{BettingRound, SidePot};
