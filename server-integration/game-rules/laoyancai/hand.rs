//! 捞腌菜 on the shared banker hand ([`crate::banker::engine`]): two cards
//! each, then a simultaneous choice to draw a third against the banker.
use serde::{Deserialize, Serialize};

use super::room::Laoyancai;
use super::rules;
use crate::banker::card::{self, Card};
use crate::banker::engine::{self, Player, Step, Variant};

/// The room's configuration and its continuous-banker state.
#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
pub struct HandConfig {
    pub ante: u64,
    pub max_seats: usize,
    #[serde(default)]
    pub continuous_banker: bool,
    #[serde(default)]
    pub banker_user: Option<uuid::Uuid>,
    #[serde(default)]
    pub bank_offer: Option<uuid::Uuid>,
    #[serde(default)]
    pub bank_revision: u64,
}
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Phase {
    Bid,
    Bet,
    Draw,
    Complete,
}
/// A seat's cards and its draw decision.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Dealt {
    pub cards: Vec<Card>,
    pub draw: Option<bool>,
}

pub type Hand = engine::Hand<Laoyancai>;

/// A two-card 8 or 9 is a natural: it never draws and shows at once.
fn natural(dealt: &Dealt) -> bool {
    rules::evaluate(&dealt.cards).natural
}

impl Variant for Laoyancai {
    type Phase = Phase;
    const PHASES: [Phase; 4] = [Phase::Bid, Phase::Bet, Phase::Draw, Phase::Complete];
    type Config = HandConfig;
    type Card = Card;
    type Dealt = Dealt;
    type Value = rules::Value;
    const DECK: usize = 52;
    /// Bidding only claims the bank (1×).
    const MAX_BID: u8 = 1;
    const MAX_MULTIPLIER: u64 = 5;
    fn stakes(cfg: &HandConfig) -> engine::HandConfig {
        engine::HandConfig {
            ante: cfg.ante,
            max_seats: cfg.max_seats,
        }
    }
    fn valid(card: &Card) -> bool {
        card.valid()
    }
    /// Each seat owns three deck positions; the third is its draw.
    fn deal(deck: &[Card], index: usize) -> Dealt {
        Dealt {
            cards: deck[index * 3..index * 3 + 2].to_vec(),
            draw: None,
        }
    }
    /// A banker natural closes drawing for the entire table.
    fn owes_play(hand: &Hand, player: &Player<Self>) -> bool {
        !hand
            .banker
            .and_then(|s| hand.participant(s))
            .is_some_and(|bank| natural(&bank.dealt))
            && player.dealt.draw.is_none()
            && !natural(&player.dealt)
    }
    fn play(dealt: &mut Dealt, deck: &[Card], index: usize, value: u8) -> Result<(), &'static str> {
        if value > 1 {
            return Err("invalid_action");
        }
        dealt.draw = Some(value == 1);
        if value == 1 {
            dealt.cards.push(deck[index * 3 + 2]);
        }
        Ok(())
    }
    fn public_in_play(dealt: &Dealt) -> bool {
        natural(dealt)
    }
    /// Higher points win; the winning hand's multiplier pays and a tie pushes.
    fn payout(dealt: &Dealt, bank: &Dealt) -> (bool, u64) {
        let value = rules::evaluate(&dealt.cards);
        let bank = rules::evaluate(&bank.cards);
        let won = value.points > bank.points;
        let multiplier = if value.points == bank.points {
            0
        } else if won {
            value.multiplier
        } else {
            bank.multiplier
        };
        (won, multiplier)
    }
    /// A bot draws on five or fewer.
    fn bot_play(dealt: &Dealt) -> u8 {
        u8::from(rules::evaluate(&dealt.cards).points <= 5)
    }
    fn codes(dealt: &Dealt) -> Vec<String> {
        card::codes(&dealt.cards)
    }
    fn value(dealt: &Dealt) -> rules::Value {
        rules::evaluate(&dealt.cards)
    }
}

impl Hand {
    /// Freeze a funded banker before any cards become visible.
    pub fn set_banker(&mut self, seat: usize) -> Result<(), &'static str> {
        if self.step != Step::Bid || self.bid_limit(seat) < 1 {
            return Err("insufficient_chips");
        }
        self.banker = Some(seat);
        self.banker_bid = 1;
        self.step = Step::Bet;
        Ok(())
    }
}
