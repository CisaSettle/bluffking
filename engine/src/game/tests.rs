//! `GameHand` tests grouped by betting, blind, preview, and settlement behavior.

use super::pots::distribute_pots;
use super::*;
use crate::card::{Card, Rank, Suit};

fn pid(n: u64) -> PlayerId {
    PlayerId::new(n)
}
fn c(n: u32) -> Chips {
    Chips(n)
}
fn card(r: Rank, s: Suit) -> Card {
    Card::new(r, s)
}

#[path = "tests/action_facts.rs"]
mod action_facts;
#[path = "tests/blinds_and_positions.rs"]
mod blinds_and_positions;
#[path = "tests/previews.rs"]
mod previews;
#[path = "tests/settlement.rs"]
mod settlement;
#[path = "tests/straddles.rs"]
mod straddles;
