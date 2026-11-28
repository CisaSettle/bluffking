//! Simultaneous, sealed decisions; no wallet or socket effects here.
use super::rules::{self, Card};
use super::settings::Rules;
use crate::banker::engine::{self, Outcome, Stake};
use serde::{Deserialize, Serialize};
#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
pub struct HandConfig {
    pub ante: u64,
    pub max_seats: usize,
    pub rules: Rules,
}
pub fn min_stack(c: &HandConfig) -> u64 {
    c.rules.min_bet * c.rules.multipliers.max()
}
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Phase {
    Bid,
    Bet,
    Reveal,
    Complete,
    Offer,
    Buy,
    Blocked,
    /// Engine-blind only: the players' devices register keys and shuffle.
    Deal,
    /// Engine-blind only: every bet is in; the players release their shares.
    Open,
    /// Engine-blind only: a player could not finish the encrypted deal or
    /// opening, so the hand ends with no settlement.
    Void,
}
impl Phase {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Bid => "bid",
            Self::Bet => "bet",
            Self::Reveal => "reveal",
            Self::Complete => "complete",
            Self::Offer => "offer",
            Self::Buy => "buy",
            Self::Blocked => "blocked",
            Self::Deal => "deal",
            Self::Open => "open",
            Self::Void => "void",
        }
    }

    pub fn seq(self) -> u8 {
        match self {
            Self::Bid => 0,
            Self::Bet => 1,
            Self::Reveal => 2,
            Self::Complete => 3,
            Self::Offer => 4,
            Self::Buy => 5,
            Self::Blocked => 6,
            Self::Deal => 7,
            Self::Open => 8,
            Self::Void => 9,
        }
    }
}
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Player {
    pub seat: usize,
    pub stack: u64,
    pub cards: [Card; 5],
    /// Cards are fixed for the hand; keep evaluation shared across settlement
    /// and all public/private deliveries, as poker does with engine results.
    #[serde(skip)]
    value: std::sync::OnceLock<rules::Value>,
    pub bid: Option<u8>,
    pub bet: Option<u64>,
    pub revealed: bool,
    #[serde(default)]
    pub buy: Option<bool>,
}
impl Player {
    fn value(&self) -> rules::Value {
        *self.value.get_or_init(|| rules::evaluate(&self.cards))
    }
}
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Hand {
    pub cfg: HandConfig,
    pub players: Vec<Player>,
    pub phase: Phase,
    pub banker: Option<usize>,
    pub outcome: Option<Outcome>,
    pub log: Vec<serde_json::Value>,
    // Random tie-break order and full deck are fixed before any private information leaves the server.
    pub deck: Vec<Card>,
    pub banker_order: Vec<usize>,
    #[serde(default)]
    pub host_seat: Option<usize>,
    /// The seat the banker invited to take the bank (free) in [`Phase::Buy`].
    #[serde(default)]
    pub sale_target: Option<usize>,
    /// The seat that held the bank before an accepted handoff; the buyer is
    /// then `banker`.
    #[serde(default)]
    pub sold_by: Option<usize>,
    #[serde(default)]
    pub fairness: Option<super::fairness::Secret>,
    /// Engine-blind hands: the encrypted deal. `players[].cards` hold
    /// placeholders until every seat has released its shares.
    #[serde(default)]
    pub eb: Option<Box<super::engine_blind::Deal>>,
    /// The bank inherited from the previous hand, applied by
    /// [`Hand::start_bidding`] (engine-blind: once the encrypted deck is fixed).
    #[serde(default)]
    pub inherited_banker: Option<usize>,
    /// `(seat, phase, ms)` for each decision a connected human sent
    /// themselves, measured from the phase clock. Server-only: read by the
    /// review-only collusion watcher, never sent to clients or persisted
    /// outside the room checkpoint.
    #[serde(default)]
    pub decision_ms: Vec<(usize, Phase, u32)>,
}
impl Hand {
    /// A hand dealt from a server deck. The caller sets `host_seat` and
    /// `inherited_banker`, then opens decisions with [`Hand::start_bidding`].
    pub fn new(
        cfg: HandConfig,
        seats: &[(usize, u64)],
        deck: Vec<Card>,
        banker_order: Vec<usize>,
    ) -> Result<Self, &'static str> {
        if !engine::deck_valid(&deck, 52, |c| c.valid()) {
            return Err("invalid_hand");
        }
        let mut hand = Self::build(cfg, seats, banker_order, |i, n| deck[i * 5 + n])?;
        hand.deck = deck;
        Ok(hand)
    }
    /// An engine-blind hand: no card exists anywhere yet. It waits in
    /// [`Phase::Deal`] until the players' encrypted deck is fixed.
    pub fn new_blind(
        cfg: HandConfig,
        seats: &[(usize, u64)],
        banker_order: Vec<usize>,
        deal: super::engine_blind::Deal,
    ) -> Result<Self, &'static str> {
        let mut hand = Self::build(cfg, seats, banker_order, |_, _| Card { rank: 0, suit: 0 })?;
        hand.phase = Phase::Deal;
        hand.eb = Some(Box::new(deal));
        Ok(hand)
    }
    fn build(
        cfg: HandConfig,
        seats: &[(usize, u64)],
        banker_order: Vec<usize>,
        card: impl Fn(usize, usize) -> Card,
    ) -> Result<Self, &'static str> {
        if !(super::room::MIN_ANTE..=super::room::MAX_ANTE).contains(&cfg.ante)
            || !cfg.rules.valid(cfg.ante, cfg.max_seats)
            || !engine::seats_valid(
                cfg.max_seats,
                seats,
                min_stack(&cfg)..=i64::MAX as u64 - cfg.rules.reserve(cfg.max_seats).max(15_000),
            )
            || !engine::order_valid(&banker_order, seats)
        {
            return Err("invalid_hand");
        }
        let players = seats
            .iter()
            .enumerate()
            .map(|(i, (seat, stack))| Player {
                seat: *seat,
                stack: *stack,
                cards: std::array::from_fn(|n| card(i, n)),
                value: std::sync::OnceLock::new(),
                bid: None,
                bet: None,
                revealed: false,
                buy: None,
            })
            .collect();
        Ok(Self {
            cfg,
            players,
            phase: Phase::Bid,
            banker: None,
            outcome: None,
            log: vec![],
            deck: vec![],
            banker_order,
            host_seat: None,
            sale_target: None,
            sold_by: None,
            fairness: None,
            eb: None,
            inherited_banker: None,
            decision_ms: vec![],
        })
    }
    /// The deck is fixed: decisions open. An inherited bank skips bidding.
    pub fn start_bidding(&mut self) {
        self.phase = Phase::Bid;
        // 抢庄牛牛 bids every hand afresh; nothing carries the bank forward.
        let grab = self.cfg.rules.grab;
        if let Some(banker) = self
            .inherited_banker
            .take()
            .filter(|s| !grab && self.can_bank(*s))
        {
            for p in &mut self.players {
                p.bid = None;
            }
            self.banker = Some(banker);
            self.phase = Phase::Offer;
            return;
        }
        let hand = self;
        // A seat that cannot fund the bank has only one legal bid: it passes
        // at once instead of waiting on a tap or the phase clock.
        let short: Vec<usize> = hand
            .players
            .iter()
            .map(|p| p.seat)
            .filter(|s| hand.max_bid(*s) == 0)
            .collect();
        for p in hand.players.iter_mut().filter(|p| short.contains(&p.seat)) {
            p.bid = Some(0);
        }
        // Admission guarantees one seat that can bank; without one the bid
        // phase is already decided (no seat can take the bank either way).
        if !hand.players.iter().any(|p| hand.needs_action(p.seat)) {
            hand.resolve_bids();
        }
    }
    /// Engine-blind: every seat's cards are opened.
    pub fn open_cards(&mut self, cards: Vec<[Card; 5]>) -> Result<(), &'static str> {
        if self.phase != Phase::Open || cards.len() != self.players.len() {
            return Err("invalid_open");
        }
        for (p, c) in self.players.iter_mut().zip(cards) {
            p.cards = c;
            p.value.take();
        }
        self.phase = Phase::Reveal;
        Ok(())
    }
    /// Engine-blind: end the hand with no settlement.
    pub fn void(&mut self, reason: &'static str, seat: Option<usize>) {
        if matches!(self.phase, Phase::Complete | Phase::Void) {
            return;
        }
        self.phase = Phase::Void;
        if let Some(deal) = self.eb.as_mut() {
            deal.void = Some((reason.to_string(), seat));
        }
        self.log
            .push(serde_json::json!({"phase":"void","reason":reason,"seat":seat}));
    }
    /// Admission only funds one minimum wager. Banking funds all active opponents.
    pub fn can_bank(&self, seat: usize) -> bool {
        self.participant(seat).is_some_and(|p| {
            p.stack
                >= self.cfg.rules.min_bet
                    * self.cfg.rules.multipliers.max()
                    * (self.players.len() - 1) as u64
        })
    }
    /// The highest bid this seat can fund: 抢 (1) when it can bank, and with
    /// 抢庄牛牛 the top `k` at which its stack covers every opponent's minimum
    /// bet ×`k` and every opponent covers losing one to it. 0 means 不抢 only.
    pub fn max_bid(&self, seat: usize) -> u8 {
        let Some(me) = self.participant(seat) else {
            return 0;
        };
        let r = self.cfg.rules;
        let unit = r.min_bet.saturating_mul(r.multipliers.max());
        let opponents = (self.players.len() - 1) as u64;
        let cover = |k: u64| {
            me.stack >= unit.saturating_mul(opponents).saturating_mul(k)
                && self
                    .players
                    .iter()
                    .all(|p| p.seat == seat || p.stack >= unit.saturating_mul(k))
        };
        (1..=r.bid_cap())
            .take_while(|k| cover(*k))
            .last()
            .unwrap_or(0) as u8
    }
    /// 抢庄牛牛: the banker's winning bid multiplies every transfer (an
    /// all-pass fallback banker counts as 1×). Always 1 otherwise.
    pub fn bank_multiple(&self) -> u64 {
        if !self.cfg.rules.grab {
            return 1;
        }
        self.banker
            .and_then(|s| self.participant(s))
            .and_then(|p| p.bid)
            .map_or(1, |bid| u64::from(bid).max(1))
    }
    /// Fixed equal banker allocation keeps simultaneous sealed bets independent.
    pub fn bet_limit(&self, seat: usize) -> u64 {
        let r = self.cfg.rules;
        let Some(p) = self.participant(seat) else {
            return 0;
        };
        let Some(bank) = self.banker.and_then(|s| self.participant(s)) else {
            return 0;
        };
        let worst = r.multipliers.max() * self.bank_multiple();
        r.max_bet
            .min(p.stack / worst)
            .min(bank.stack / (self.players.len() - 1) as u64 / worst)
    }
    pub fn participant(&self, seat: usize) -> Option<&Player> {
        self.players.iter().find(|p| p.seat == seat)
    }
    pub fn finished(&self) -> bool {
        self.phase == Phase::Complete
    }
    pub fn needs_action(&self, seat: usize) -> bool {
        self.participant(seat).is_some_and(|p| match self.phase {
            Phase::Bid => p.bid.is_none(),
            Phase::Bet => Some(seat) != self.banker && p.bet.is_none(),
            Phase::Reveal => !p.revealed,
            Phase::Offer => Some(seat) == self.banker,
            Phase::Buy => {
                Some(seat) != self.banker && p.buy.is_none() && self.sale_target == Some(seat)
            }
            Phase::Complete | Phase::Blocked | Phase::Deal | Phase::Open | Phase::Void => false,
        })
    }
    /// Reveal value: a three-card index bitmask, 0 for no bull, 32 for a special hand.
    /// The server chooses the same valid answer for bots, absent seats and timeouts.
    pub fn reveal_value(&self, seat: usize) -> u64 {
        if !self.cfg.rules.manual_group {
            return 0;
        }
        let Some(p) = self.participant(seat) else {
            return 0;
        };
        let value = p.value();
        if value.bull > 10 {
            32
        } else {
            value
                .triple
                .map_or(0, |triple| triple.iter().fold(0, |mask, i| mask | (1 << i)))
        }
    }
    /// Keep the player's chosen grouping in the existing durable action log.
    pub fn shown_value(&self, seat: usize) -> Option<rules::Value> {
        let p = self.participant(seat)?;
        if self.cfg.rules.manual_group && !p.revealed && !self.finished() {
            return None;
        }
        let mut value = p.value();
        if self.cfg.rules.manual_group && (1..=10).contains(&value.bull) {
            if let Some(mask) = self
                .log
                .iter()
                .find(|entry| {
                    entry["phase"] == "reveal" && entry["seat"].as_u64() == Some(seat as u64)
                })
                .and_then(|entry| entry["value"].as_u64())
            {
                let indices: Vec<usize> = (0..5).filter(|i| mask & (1 << i) != 0).collect();
                value.triple = indices.try_into().ok();
            }
        }
        Some(value)
    }
    fn validate_group(&self, seat: usize, mask: u64) -> Result<(), &'static str> {
        let cards = &self.participant(seat).ok_or("not_in_hand")?.cards;
        let value = rules::evaluate(cards);
        if mask == 32 {
            return if value.bull > 10 {
                Ok(())
            } else {
                Err("not_special_hand")
            };
        }
        if value.bull > 10 {
            return Err("confirm_special_hand");
        }
        if mask == 0 {
            return if value.bull == 0 {
                Ok(())
            } else {
                Err("bull_group_exists")
            };
        }
        if mask > 31 || mask.count_ones() != 3 {
            return Err("select_three_cards");
        }
        let total: u8 = cards
            .iter()
            .enumerate()
            .filter(|(i, _)| mask & (1 << i) != 0)
            .map(|(_, c)| rules::points(*c))
            .sum();
        if !total.is_multiple_of(10) {
            return Err("invalid_bull_group");
        }
        Ok(())
    }
    pub fn act(
        &mut self,
        seat: usize,
        phase: Phase,
        seq: u8,
        value: u64,
    ) -> Result<(), &'static str> {
        if phase != self.phase || seq != self.phase.seq() {
            return Err("stale_action");
        }
        if !self.needs_action(seat) {
            return Err("already_acted");
        }
        if phase == Phase::Reveal && self.cfg.rules.manual_group {
            self.validate_group(seat, value)?;
        }
        if phase == Phase::Bid
            && (1..=self.cfg.rules.bid_cap()).contains(&value)
            && value > u64::from(self.max_bid(seat))
        {
            return Err("insufficient_chips");
        }
        if phase == Phase::Bet && self.cfg.rules.bet_allowed(value) && value > self.bet_limit(seat)
        {
            return Err("insufficient_chips");
        }
        if phase == Phase::Offer {
            // Zero continues play; a positive value names the invited seat + 1.
            let target = value.checked_sub(1).and_then(|n| usize::try_from(n).ok());
            if value != 0
                && !target.is_some_and(|s| Some(s) != self.banker && self.participant(s).is_some())
            {
                return Err("invalid_action");
            }
            self.sale_target = target;
            self.phase = if target.is_some() {
                Phase::Buy
            } else {
                Phase::Bet
            };
            self.log
                .push(serde_json::json!({"phase":phase,"seat":seat,"value":value}));
            return Ok(());
        }
        if phase == Phase::Buy && value == 1 {
            let seller = self.banker.ok_or("missing_banker")?;
            let buyer = self.participant(seat).ok_or("not_in_hand")?;
            if buyer.stack
                < self.cfg.rules.min_bet
                    * self.cfg.rules.multipliers.max()
                    * (self.players.len() - 1) as u64
            {
                return Err("insufficient_chips");
            }
            self.sold_by = Some(seller);
            self.banker = Some(seat);
            self.phase = Phase::Bet;
            self.log
                .push(serde_json::json!({"phase":phase,"seat":seat,"value":value}));
            return Ok(());
        }
        let p = self
            .players
            .iter_mut()
            .find(|p| p.seat == seat)
            .ok_or("not_in_hand")?;
        match phase {
            Phase::Bid if value <= self.cfg.rules.bid_cap() => p.bid = Some(value as u8),
            Phase::Bet if self.cfg.rules.bet_allowed(value) => p.bet = Some(value),
            Phase::Buy if value == 0 => p.buy = Some(false),
            Phase::Reveal if value == 0 || self.cfg.rules.manual_group => p.revealed = true,
            _ => return Err("invalid_action"),
        }
        self.log
            .push(serde_json::json!({"phase":phase,"seat":seat,"value":value}));
        if self.players.iter().any(|p| self.needs_action(p.seat)) {
            return Ok(());
        }
        match phase {
            Phase::Bid => self.resolve_bids(),
            Phase::Bet => {
                self.phase = if self.eb.is_some() {
                    Phase::Open
                } else {
                    Phase::Reveal
                }
            }
            Phase::Reveal => {
                self.settle()?;
                self.phase = Phase::Complete;
            }
            Phase::Buy => self.phase = Phase::Bet,
            Phase::Complete
            | Phase::Blocked
            | Phase::Offer
            | Phase::Deal
            | Phase::Open
            | Phase::Void => {}
        }
        Ok(())
    }
    /// Every bid is in: the highest bid (first in the random order) banks.
    /// With no volunteer the host banks when dealt in and funded; otherwise
    /// the first funded seat in the random order does, so play never stalls.
    fn resolve_bids(&mut self) {
        let max = self.players.iter().filter_map(|p| p.bid).max().unwrap_or(0);
        self.banker = if max == 0 {
            self.host_seat
                .into_iter()
                .chain(self.banker_order.iter().copied())
                .find(|s| self.can_bank(*s))
        } else {
            self.banker_order
                .iter()
                .copied()
                .find(|s| self.participant(*s).is_some_and(|p| p.bid == Some(max)))
        };
        self.phase = if self.banker.is_some() {
            Phase::Bet
        } else {
            Phase::Blocked
        };
    }
    pub fn timeout(&mut self) -> Result<(), &'static str> {
        let phase = self.phase;
        if matches!(phase, Phase::Deal | Phase::Open) {
            let missing = self
                .eb
                .as_ref()
                .and_then(|d| d.waiting_on().first().copied());
            self.void(
                if phase == Phase::Deal {
                    "deal_timeout"
                } else {
                    "open_timeout"
                },
                missing,
            );
            return Ok(());
        }
        let seats: Vec<_> = self
            .players
            .iter()
            .filter(|p| self.needs_action(p.seat))
            .map(|p| p.seat)
            .collect();
        for s in seats {
            self.act(
                s,
                phase,
                phase.seq(),
                if phase == Phase::Bet {
                    self.cfg.rules.min_bet
                } else if phase == Phase::Reveal {
                    self.reveal_value(s)
                } else {
                    0
                },
            )?;
        }
        Ok(())
    }
    pub fn visible_cards(&self, viewer: Option<usize>, seat: usize) -> Option<Vec<Card>> {
        let p = self.participant(seat)?;
        if !matches!(self.phase, Phase::Reveal | Phase::Complete) {
            return None;
        }
        if self.finished() {
            return Some(p.cards.to_vec());
        }
        if viewer != Some(seat) {
            return None;
        }
        Some(p.cards.to_vec())
    }
    fn settle(&mut self) -> Result<(), &'static str> {
        let banker = self.banker.ok_or("missing_banker")?;
        let bank_multiple = self.bank_multiple();
        let bank = self.participant(banker).ok_or("missing_banker")?;
        let bank_value = bank.value();
        let multipliers = self.cfg.rules.multipliers;
        let stakes = self.players.iter().filter(|p| p.seat != banker).map(|p| {
            let v = p.value();
            let won = v.key() > bank_value.key();
            let amount = p
                .bet
                .unwrap_or(self.cfg.rules.min_bet)
                .checked_mul(multipliers.get(if won { v.bull } else { bank_value.bull }))
                .and_then(|n| n.checked_mul(bank_multiple))
                .ok_or("overflow")?;
            Ok(Stake {
                seat: p.seat,
                stack: p.stack,
                won,
                amount,
            })
        });
        let outcome = engine::settle(banker, bank.stack, stakes)?;
        self.outcome = Some(outcome);
        Ok(())
    }
}
