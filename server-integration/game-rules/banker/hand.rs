//! 三公 and 牌九 on the shared banker hand ([`super::engine`]): a seat's
//! holding is dealt face down and its play decision is turning it over.
use std::marker::PhantomData;

use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};

pub use super::engine::HandConfig;
use super::engine::Variant;

/// A dealt holding supplies only the rules that differ between these games.
pub trait Holding:
    std::fmt::Debug + Clone + Copy + Serialize + DeserializeOwned + Send + Sync + 'static
{
    type Card: std::fmt::Debug
        + Clone
        + Copy
        + Eq
        + std::hash::Hash
        + Serialize
        + DeserializeOwned
        + Send
        + Sync;
    const DECK: usize;
    const WIDTH: usize;
    const MAX_MULTIPLIER: u64;
    fn valid(card: &Self::Card) -> bool;
    fn deal(cards: &[Self::Card]) -> Self;
    fn payout(self, banker: Self) -> (bool, u64);
    fn codes(self) -> Vec<String>;
    fn value(self) -> serde_json::Value;
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Phase {
    Bid,
    Bet,
    Reveal,
    Complete,
}

/// A seat's holding and whether it has turned it over.
#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
#[serde(bound = "H: Holding")]
pub struct Revealing<H: Holding> {
    pub holding: H,
    pub revealed: bool,
}

/// The [`Variant`] of the games that deal an `H`.
#[derive(Debug, Clone, Copy)]
pub struct Reveal<H>(PhantomData<H>);

pub type Hand<H> = super::engine::Hand<Reveal<H>>;

impl<H: Holding> Variant for Reveal<H> {
    type Phase = Phase;
    const PHASES: [Phase; 4] = [Phase::Bid, Phase::Bet, Phase::Reveal, Phase::Complete];
    type Config = HandConfig;
    type Card = H::Card;
    type Dealt = Revealing<H>;
    type Value = serde_json::Value;
    const DECK: usize = H::DECK;
    const MAX_BID: u8 = 4;
    const MAX_MULTIPLIER: u64 = H::MAX_MULTIPLIER;
    fn stakes(cfg: &HandConfig) -> HandConfig {
        *cfg
    }
    fn valid(card: &H::Card) -> bool {
        H::valid(card)
    }
    fn deal(deck: &[H::Card], index: usize) -> Revealing<H> {
        Revealing {
            holding: H::deal(&deck[index * H::WIDTH..(index + 1) * H::WIDTH]),
            revealed: false,
        }
    }
    fn owes_play(_hand: &Hand<H>, player: &super::engine::Player<Self>) -> bool {
        !player.dealt.revealed
    }
    fn play(
        dealt: &mut Revealing<H>,
        _deck: &[H::Card],
        _index: usize,
        value: u8,
    ) -> Result<(), &'static str> {
        if value != 0 {
            return Err("invalid_action");
        }
        dealt.revealed = true;
        Ok(())
    }
    fn payout(dealt: &Revealing<H>, bank: &Revealing<H>) -> (bool, u64) {
        dealt.holding.payout(bank.holding)
    }
    fn codes(dealt: &Revealing<H>) -> Vec<String> {
        dealt.holding.codes()
    }
    fn value(dealt: &Revealing<H>) -> serde_json::Value {
        dealt.holding.value()
    }
}
