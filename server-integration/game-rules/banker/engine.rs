//! The shared sealed banker hand: every dealt seat bids for the bank at once,
//! the others bet against it at once, then every seat takes the game's own
//! decision (三公/牌九 reveal, 捞腌菜 draw) and the hand settles zero-sum
//! against the banker. A [`Variant`] supplies only its cards, that decision
//! and its payout. The seat, deck and settlement checks below are also what
//! 牛牛's own hand uses. No wallet or socket effects here.
use std::collections::HashSet;
use std::fmt::Debug;
use std::hash::Hash;
use std::ops::RangeInclusive;

use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};

use crate::game_hub::cards::CardOutcome;

pub const ANTE_TIERS: [u64; 3] = [1, 2, 5];
pub const MIN_SEATS: usize = 2;
pub const MAX_SEATS: usize = 6;
pub const MAX_BET: u8 = 5;

#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
pub struct HandConfig {
    pub ante: u64,
    pub max_seats: usize,
}

/// The rules one banker game adds to the shared hand.
pub trait Variant: Debug + Clone + Send + Sync + 'static {
    /// The game's wire names of the four steps.
    type Phase: Debug + Copy + Eq + Serialize + DeserializeOwned + Send + Sync;
    /// [`Self::Phase`] for [`Step::Bid`], [`Step::Bet`], [`Step::Play`] and
    /// [`Step::Complete`], in that order.
    const PHASES: [Self::Phase; 4];
    /// The room configuration the hand is dealt with.
    type Config: Debug + Clone + Copy + Serialize + DeserializeOwned + Send + Sync;
    type Card: Debug + Clone + Copy + Eq + Hash + Serialize + DeserializeOwned + Send + Sync;
    /// One seat's cards and its own [`Step::Play`] decision.
    type Dealt: Debug + Clone + Serialize + DeserializeOwned + Send + Sync;
    /// What the client shows the cards are worth.
    type Value: Serialize;
    const DECK: usize;
    const MAX_BID: u8;
    /// The largest payout per ante, banker bid and bet.
    const MAX_MULTIPLIER: u64;
    fn stakes(cfg: &Self::Config) -> HandConfig;
    fn valid(card: &Self::Card) -> bool;
    /// The cards the seat at `index` of the dealing order receives.
    fn deal(deck: &[Self::Card], index: usize) -> Self::Dealt;
    /// Whether this seat still owes its play decision.
    fn owes_play(hand: &Hand<Self>, player: &Player<Self>) -> bool;
    /// Applies the play decision of the seat at `index` of the dealing order.
    fn play(
        dealt: &mut Self::Dealt,
        deck: &[Self::Card],
        index: usize,
        value: u8,
    ) -> Result<(), &'static str>;
    /// Cards every viewer sees during play, before the result.
    fn public_in_play(_dealt: &Self::Dealt) -> bool {
        false
    }
    /// Whether the player beats the banker, and the payout multiplier.
    fn payout(dealt: &Self::Dealt, bank: &Self::Dealt) -> (bool, u64);
    /// A bot's play decision on its own cards.
    fn bot_play(_dealt: &Self::Dealt) -> u8 {
        0
    }
    /// The cards' client codes.
    fn codes(dealt: &Self::Dealt) -> Vec<String>;
    fn value(dealt: &Self::Dealt) -> Self::Value;
}

/// Where a hand is; its number is the phase `seq` the client echoes.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Step {
    Bid,
    Bet,
    Play,
    Complete,
}

pub fn min_stack<V: Variant>(cfg: &V::Config) -> u64 {
    V::stakes(cfg).ante * V::MAX_MULTIPLIER
}

/// A bot's stack refill: enough to bank the top bid against every top bet.
pub fn bot_refill_floor<V: Variant>(cfg: &V::Config) -> u64 {
    let stakes = V::stakes(cfg);
    stakes.ante
        * V::MAX_MULTIPLIER
        * u64::from(V::MAX_BID)
        * u64::from(MAX_BET)
        * stakes.max_seats.saturating_sub(1) as u64
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(bound = "V: Variant")]
pub struct Player<V: Variant> {
    pub seat: usize,
    pub stack: u64,
    pub dealt: V::Dealt,
    pub bid: Option<u8>,
    pub bet: Option<u8>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Outcome {
    pub winners: Vec<usize>,
    pub deltas: Vec<(usize, i64)>,
    pub pot: u64,
    pub showdown: bool,
}
impl Outcome {
    pub fn settlement(&self) -> CardOutcome {
        CardOutcome::of(&self.winners, self.pot, self.showdown, &self.deltas)
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(bound = "V: Variant")]
pub struct Hand<V: Variant> {
    pub cfg: V::Config,
    pub players: Vec<Player<V>>,
    pub step: Step,
    pub banker: Option<usize>,
    pub banker_bid: u8,
    pub outcome: Option<Outcome>,
    pub log: Vec<serde_json::Value>,
    // Random tie-break order and full deck are fixed before any private information leaves the server.
    pub deck: Vec<V::Card>,
    pub banker_order: Vec<usize>,
}

/// What a seat's owner sees of it beyond the public row: its bid while
/// bidding, its bet before play, and during play its own cards when they
/// are not public. `None` keeps the public field.
pub struct Sealed<'a, V: Variant> {
    pub bid: Option<u8>,
    pub bet: Option<u8>,
    pub dealt: Option<&'a V::Dealt>,
}

impl<V: Variant> Hand<V> {
    pub fn new(
        cfg: V::Config,
        seats: &[(usize, u64)],
        deck: Vec<V::Card>,
        banker_order: Vec<usize>,
    ) -> Result<Self, &'static str> {
        let stakes = V::stakes(&cfg);
        if !ANTE_TIERS.contains(&stakes.ante)
            || !seats_valid(
                stakes.max_seats,
                seats,
                min_stack::<V>(&cfg)..=i64::MAX as u64 - 45_000,
            )
            || !seats
                .iter()
                .any(|(_, b)| *b >= min_stack::<V>(&cfg) * seats.len().saturating_sub(1) as u64)
            || !deck_valid(&deck, V::DECK, V::valid)
            || !order_valid(&banker_order, seats)
        {
            return Err("invalid_hand");
        }
        let players = seats
            .iter()
            .enumerate()
            .map(|(i, (seat, stack))| Player {
                seat: *seat,
                stack: *stack,
                dealt: V::deal(&deck, i),
                bid: None,
                bet: None,
            })
            .collect();
        Ok(Self {
            cfg,
            players,
            step: Step::Bid,
            banker: None,
            banker_bid: 1,
            outcome: None,
            log: vec![],
            deck,
            banker_order,
        })
    }
    pub fn phase(&self) -> V::Phase {
        V::PHASES[self.step as usize]
    }
    pub fn seq(&self) -> u8 {
        self.step as u8
    }
    pub fn bid_limit(&self, seat: usize) -> u8 {
        let Some(p) = self.participant(seat) else {
            return 0;
        };
        let unit = V::stakes(&self.cfg).ante * V::MAX_MULTIPLIER;
        let opponents = self
            .players
            .iter()
            .filter(|p| p.seat != seat)
            .map(|p| p.stack / unit)
            .min()
            .unwrap_or(0);
        (p.stack / unit / (self.players.len() - 1) as u64)
            .min(opponents)
            .min(u64::from(V::MAX_BID)) as u8
    }
    pub fn bet_limit(&self, seat: usize) -> u8 {
        let Some(p) = self.participant(seat) else {
            return 0;
        };
        let Some(bank) = self.banker.and_then(|s| self.participant(s)) else {
            return 0;
        };
        let unit = V::stakes(&self.cfg).ante * V::MAX_MULTIPLIER * u64::from(self.banker_bid);
        (p.stack / unit)
            .min(bank.stack / unit / (self.players.len() - 1) as u64)
            .min(u64::from(MAX_BET)) as u8
    }
    pub fn participant(&self, seat: usize) -> Option<&Player<V>> {
        self.players.iter().find(|p| p.seat == seat)
    }
    pub fn finished(&self) -> bool {
        self.step == Step::Complete
    }
    pub fn needs_action(&self, seat: usize) -> bool {
        self.participant(seat).is_some_and(|p| match self.step {
            Step::Bid => p.bid.is_none(),
            Step::Bet => Some(seat) != self.banker && p.bet.is_none(),
            Step::Play => V::owes_play(self, p),
            Step::Complete => false,
        })
    }
    /// A dealt seat that has decided this step (or the hand is over).
    pub fn acted(&self, seat: usize) -> bool {
        self.participant(seat).is_some() && !self.needs_action(seat)
    }
    /// The seat's bid, public once bidding is over.
    pub fn public_bid(&self, seat: usize) -> Option<u8> {
        self.participant(seat)
            .filter(|_| self.step != Step::Bid)
            .and_then(|p| p.bid)
    }
    /// The seat's bet, public from the play step on.
    pub fn public_bet(&self, seat: usize) -> Option<u8> {
        self.participant(seat)
            .filter(|_| self.step >= Step::Play)
            .and_then(|p| p.bet)
    }
    pub fn act(
        &mut self,
        seat: usize,
        phase: V::Phase,
        seq: u8,
        value: u8,
    ) -> Result<(), &'static str> {
        if phase != self.phase() || seq != self.seq() {
            return Err("stale_action");
        }
        if !self.needs_action(seat) {
            return Err("already_acted");
        }
        let step = self.step;
        if (step == Step::Bid && value <= V::MAX_BID && value > self.bid_limit(seat))
            || (step == Step::Bet && (1..=MAX_BET).contains(&value) && value > self.bet_limit(seat))
        {
            return Err("insufficient_chips");
        }
        let index = self
            .players
            .iter()
            .position(|p| p.seat == seat)
            .ok_or("not_in_hand")?;
        let p = &mut self.players[index];
        match step {
            Step::Bid if value <= V::MAX_BID => p.bid = Some(value),
            Step::Bet if (1..=MAX_BET).contains(&value) => p.bet = Some(value),
            Step::Play => V::play(&mut p.dealt, &self.deck, index, value)?,
            _ => return Err("invalid_action"),
        }
        self.log
            .push(serde_json::json!({"phase":phase,"seat":seat,"value":value}));
        if self.players.iter().any(|p| self.needs_action(p.seat)) {
            return Ok(());
        }
        match step {
            Step::Bid => {
                let max = self.players.iter().filter_map(|p| p.bid).max().unwrap_or(0);
                self.banker = self.banker_order.iter().copied().find(|s| {
                    self.bid_limit(*s) >= max.max(1)
                        && self.participant(*s).is_some_and(|p| p.bid == Some(max))
                });
                if self.banker.is_none() {
                    return Err("insufficient_chips");
                }
                self.banker_bid = max.max(1);
                self.step = Step::Bet;
            }
            Step::Bet => {
                self.step = Step::Play;
                // A play step nobody owes (捞腌菜's banker natural) settles at once.
                if !self.players.iter().any(|p| self.needs_action(p.seat)) {
                    self.settle()?;
                    self.step = Step::Complete;
                }
            }
            Step::Play => {
                self.settle()?;
                self.step = Step::Complete;
            }
            Step::Complete => {}
        }
        Ok(())
    }
    /// The step clock ran out: every missing decision takes the cautious
    /// default (no bid, the minimum bet, no draw / reveal).
    pub fn timeout(&mut self) -> Result<(), &'static str> {
        let (phase, seq) = (self.phase(), self.seq());
        let value = if self.step == Step::Bet { 1 } else { 0 };
        let seats: Vec<_> = self
            .players
            .iter()
            .filter(|p| self.needs_action(p.seat))
            .map(|p| p.seat)
            .collect();
        for s in seats {
            self.act(s, phase, seq, value)?;
        }
        Ok(())
    }
    /// The seat's cards as `viewer` sees them: everyone's once the hand is
    /// over, during play the viewer's own and any the game shows openly.
    pub fn visible(&self, viewer: Option<usize>, seat: usize) -> Option<&V::Dealt> {
        let p = self.participant(seat)?;
        (self.finished()
            || (self.step == Step::Play && (viewer == Some(seat) || V::public_in_play(&p.dealt))))
        .then_some(&p.dealt)
    }
    pub fn sealed(&self, seat: usize) -> Sealed<'_, V> {
        let mut sealed = Sealed {
            bid: None,
            bet: None,
            dealt: None,
        };
        let Some(p) = self.participant(seat).filter(|_| !self.finished()) else {
            return sealed;
        };
        if self.step == Step::Bid {
            sealed.bid = p.bid;
        }
        if self.step != Step::Play {
            sealed.bet = p.bet;
        } else {
            sealed.dealt = self
                .visible(Some(seat), seat)
                .filter(|_| self.visible(None, seat).is_none());
        }
        sealed
    }
    fn settle(&mut self) -> Result<(), &'static str> {
        let banker = self.banker.ok_or("missing_banker")?;
        let bank = self.participant(banker).ok_or("missing_banker")?;
        let ante = V::stakes(&self.cfg).ante;
        let stakes = self.players.iter().filter(|p| p.seat != banker).map(|p| {
            let (won, multiplier) = V::payout(&p.dealt, &bank.dealt);
            let amount = ante
                .checked_mul(self.banker_bid as u64)
                .and_then(|x| x.checked_mul(p.bet.unwrap_or(1) as u64))
                .and_then(|x| x.checked_mul(multiplier))
                .ok_or("overflow")?;
            Ok(Stake {
                seat: p.seat,
                stack: p.stack,
                won,
                amount,
            })
        });
        let outcome = settle(banker, bank.stack, stakes)?;
        self.outcome = Some(outcome);
        Ok(())
    }
}

/// Two to `max_seats` distinct seats below `max_seats` (itself
/// [`MIN_SEATS`]..=[`MAX_SEATS`]), each holding a stack within `stacks`.
pub fn seats_valid(max_seats: usize, seats: &[(usize, u64)], stacks: RangeInclusive<u64>) -> bool {
    (MIN_SEATS..=MAX_SEATS).contains(&max_seats)
        && (MIN_SEATS..=max_seats).contains(&seats.len())
        && seats
            .iter()
            .all(|(s, b)| *s < max_seats && stacks.contains(b))
        && seats.iter().map(|s| s.0).collect::<HashSet<_>>().len() == seats.len()
}

/// The banker tie-break order names each dealt seat exactly once.
pub fn order_valid(order: &[usize], seats: &[(usize, u64)]) -> bool {
    order.len() == seats.len()
        && order.iter().copied().collect::<HashSet<_>>() == seats.iter().map(|s| s.0).collect()
}

/// `size` distinct cards, each a real one.
pub fn deck_valid<C: Eq + Hash>(deck: &[C], size: usize, valid: impl Fn(&C) -> bool) -> bool {
    deck.len() == size
        && deck.iter().all(valid)
        && deck.iter().collect::<HashSet<_>>().len() == size
}

/// One player's result against the banker.
pub struct Stake {
    pub seat: usize,
    pub stack: u64,
    pub won: bool,
    pub amount: u64,
}

/// Zero-sum settlement: each player wins or loses its amount to the banker,
/// and no seat may lose more than it holds. The banker's delta comes last.
pub fn settle(
    banker: usize,
    bank_stack: u64,
    stakes: impl IntoIterator<Item = Result<Stake, &'static str>>,
) -> Result<Outcome, &'static str> {
    let mut deltas = Vec::new();
    let mut bank_delta = 0i64;
    let mut pot = 0u64;
    for stake in stakes {
        let stake = stake?;
        let amount = i64::try_from(stake.amount).map_err(|_| "overflow")?;
        let delta = if stake.won { amount } else { -amount };
        if delta < 0 && delta.unsigned_abs() > stake.stack {
            return Err("insufficient_chips");
        }
        bank_delta = bank_delta.checked_sub(delta).ok_or("overflow")?;
        pot = pot.checked_add(amount as u64).ok_or("overflow")?;
        deltas.push((stake.seat, delta));
    }
    if bank_delta < 0 && bank_delta.unsigned_abs() > bank_stack {
        return Err("insufficient_chips");
    }
    deltas.push((banker, bank_delta));
    Ok(Outcome {
        winners: deltas.iter().filter(|d| d.1 > 0).map(|d| d.0).collect(),
        deltas,
        pot,
        showdown: true,
    })
}
