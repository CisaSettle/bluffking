//! One 短牌德州 hand as a pure state machine (no IO, no clocks, no RNG).
//!
//! The standard opening uses dead antes plus a live button blind. Optional
//! openings use small/big blinds, a live straddle, or a flop-first Bomb Pot.
//! Raises follow the largest full increment on the street; cumulative short
//! all-ins can reopen action (TDA 49). Pots are layered by total contribution.

use crate::poker_control::RunoutVotes;
use engine::betting::{action_reopened, pot_layers, runout_halves, Contribution, PotPolicy};
use serde::{Deserialize, Serialize};
use std::sync::OnceLock;

use super::rules::{self, Card, Category, HandValue};

/// Button blind = this many antes.
pub const BUTTON_BLIND_ANTES: u64 = 1;

#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
pub struct HandConfig {
    pub ante: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub table_ante: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub small_blind: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub straddle_bb: Option<u8>,
}

impl HandConfig {
    pub fn forced_ante(&self) -> u64 {
        self.table_ante.map_or_else(
            || {
                if self.small_blind.is_none() {
                    self.ante
                } else {
                    0
                }
            },
            u64::from,
        )
    }

    pub fn button_blind(&self) -> u64 {
        if self.small_blind.is_some() {
            return 0;
        }
        self.ante * BUTTON_BLIND_ANTES
    }

    pub fn big_blind(&self) -> u64 {
        self.small_blind
            .map_or_else(|| self.button_blind(), |_| self.ante)
    }
}

/// Admission covers the dead ante and one full opening blind.
pub fn min_stack(cfg: &HandConfig) -> u64 {
    cfg.big_blind() + cfg.forced_ante()
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Street {
    Preflop,
    Flop,
    Turn,
    River,
    Showdown,
}

/// One post-flop ranking for this seat. Cloning a checkpoint copies fixed-size
/// data only; saving it writes only the underlying dealt cards.
#[derive(Debug, Clone)]
struct StreetValue {
    cards: [Card; 7],
    count: usize,
    value: HandValue,
    best: [Card; 5],
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Participant {
    pub seat: usize,
    pub stack_start: u64,
    pub cards: [Card; 2],
    #[serde(skip)]
    street_value: OnceLock<StreetValue>,
    pub folded: bool,
    /// Everything put in this hand, antes included.
    pub contributed: u64,
    /// Live chips in front of the player on the current street.
    pub street_bet: u64,
    /// Acted on this street.
    acted: bool,
    /// The street's top bet right after this player last acted. Raising
    /// reopens once the top bet has grown by a full raise since then.
    faced: u64,
}

impl Participant {
    pub fn stack(&self) -> u64 {
        self.stack_start - self.contributed
    }
    pub fn all_in(&self) -> bool {
        !self.folded && self.stack() == 0
    }
    fn can_bet(&self) -> bool {
        !self.folded && self.stack() > 0
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Action {
    Fold,
    Check,
    Call,
    /// Bet or raise so the street bet totals `to`.
    RaiseTo(u64),
    AllIn,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ActionError {
    NotInHand,
    HandFinished,
    NotYourTurn,
    AlreadyFolded,
    CannotCheck,
    NothingToCall,
    InvalidRaise,
    RaiseNotAllowed,
    InsufficientChips,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum LogEntry {
    Ante {
        seat: usize,
        amount: u64,
    },
    ButtonBlind {
        seat: usize,
        amount: u64,
    },
    SmallBlind {
        seat: usize,
        amount: u64,
    },
    BigBlind {
        seat: usize,
        amount: u64,
    },
    Straddle {
        seat: usize,
        amount: u64,
    },
    BombPot {
        seat: usize,
        amount: u64,
    },
    Fold {
        seat: usize,
        timeout: bool,
    },
    Check {
        seat: usize,
        timeout: bool,
    },
    Call {
        seat: usize,
        amount: u64,
        all_in: bool,
    },
    /// First wager of a postflop street.
    Bet {
        seat: usize,
        to: u64,
        amount: u64,
        all_in: bool,
    },
    Raise {
        seat: usize,
        to: u64,
        amount: u64,
        all_in: bool,
    },
    /// Uncalled chips returned at the end of a street.
    Return {
        seat: usize,
        amount: u64,
    },
    Street {
        street: Street,
        cards: Vec<String>,
    },
    Showdown {
        seats: Vec<usize>,
    },
    RunTwice {
        run: RunTwice,
    },
    Win {
        pot: usize,
        seats: Vec<usize>,
        amount: u64,
    },
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Shown {
    pub seat: usize,
    pub cards: Vec<String>,
    pub category: Category,
    /// The five cards that make the hand.
    pub best: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PotResult {
    pub amount: u64,
    pub winners: Vec<usize>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RunAward {
    pub seat: usize,
    pub amount: u64,
    pub category: Category,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RunTwice {
    pub shared: usize,
    pub board: Vec<String>,
    pub first: Vec<RunAward>,
    pub second: Vec<RunAward>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Outcome {
    /// Seats that won chips from a pot.
    pub winners: Vec<usize>,
    pub showdown: bool,
    pub pot: u64,
    pub pots: Vec<PotResult>,
    pub shown: Vec<Shown>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub run_twice: Option<RunTwice>,
    /// `(seat, net chips)` for every participant; sums to zero.
    pub deltas: Vec<(usize, i64)>,
}

#[derive(Debug, Clone, Default, Serialize, PartialEq, Eq)]
pub struct Legal {
    pub your_turn: bool,
    pub can_check: bool,
    /// Chips needed to call (capped at the stack); `None` when checking.
    pub call_cost: Option<u64>,
    pub can_raise: bool,
    /// Smallest legal bet/raise total (the all-in total when that is less).
    pub min_raise_to: u64,
    /// The all-in total.
    pub max_raise_to: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Hand {
    pub cfg: HandConfig,
    pub players: Vec<Participant>,
    pub dealer_seat: usize,
    pub street: Street,
    /// The five board cards, fixed at the deal; only `board_len` are public.
    board_cards: [Card; 5],
    board_len: usize,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    runout: Option<Runout>,
    to_act: Option<usize>,
    /// Highest street bet.
    pub current_bet: u64,
    /// Size of the last full bet/raise increment this street.
    pub min_raise: u64,
    pub log: Vec<LogEntry>,
    pub outcome: Option<Outcome>,
    /// `(seat, card position)` a mucked player chose to show after the hand.
    #[serde(default)]
    pub voluntary_shows: Vec<(usize, usize)>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct Runout {
    /// Fresh cards from the same committed shuffle, following the first board.
    fresh: [Card; 5],
    votes: RunoutVotes,
}

impl Hand {
    /// `seats` = `(seat, stack)` for every dealt-in player, any order.
    /// `deck` includes the committed tail for a possible second board.
    pub fn new(
        cfg: HandConfig,
        seats: &[(usize, u64)],
        dealer_seat: usize,
        deck: &[Card],
        bomb_pot: Option<u64>,
        runout_voters: &[u8],
    ) -> Result<Self, ActionError> {
        let mut seats = seats.to_vec();
        seats.sort_by_key(|s| s.0);
        let n = seats.len();
        if n < 2 || deck.len() < n * 2 + 5 {
            return Err(ActionError::NotInHand);
        }
        if seats.iter().any(|&(_, stack)| stack == 0) {
            return Err(ActionError::InsufficientChips);
        }
        let players: Vec<Participant> = seats
            .iter()
            .enumerate()
            .map(|(i, &(seat, stack))| Participant {
                seat,
                stack_start: stack,
                cards: [deck[i], deck[i + n]],
                street_value: OnceLock::new(),
                folded: false,
                contributed: 0,
                street_bet: 0,
                acted: false,
                faced: 0,
            })
            .collect();
        let board_cards = [
            deck[2 * n],
            deck[2 * n + 1],
            deck[2 * n + 2],
            deck[2 * n + 3],
            deck[2 * n + 4],
        ];
        let runout = if runout_voters.is_empty() {
            None
        } else {
            let fresh = deck
                .get(2 * n + 5..2 * n + 10)
                .and_then(|cards| cards.try_into().ok())
                .ok_or(ActionError::NotInHand)?;
            Some(Runout {
                fresh,
                votes: RunoutVotes::new(runout_voters.iter().copied()),
            })
        };
        let button = players
            .iter()
            .position(|p| p.seat == dealer_seat)
            .unwrap_or(0);
        let bb = cfg.big_blind();
        let mut hand = Self {
            cfg,
            dealer_seat: players[button].seat,
            players,
            street: Street::Preflop,
            board_cards,
            board_len: 0,
            runout,
            to_act: None,
            current_bet: 0,
            min_raise: bb,
            log: Vec::with_capacity(n + 8),
            outcome: None,
            voluntary_shows: Vec::new(),
        };
        if let Some(amount) = bomb_pot {
            for p in &mut hand.players {
                p.contributed = amount.min(p.stack_start);
                hand.log.push(LogEntry::BombPot {
                    seat: p.seat,
                    amount: p.contributed,
                });
            }
            hand.deal_street();
            hand.advance(button);
            return Ok(hand);
        }

        let ante = cfg.forced_ante();
        if ante > 0 {
            for p in &mut hand.players {
                p.contributed = ante.min(p.stack_start);
                hand.log.push(LogEntry::Ante {
                    seat: p.seat,
                    amount: p.contributed,
                });
            }
        }

        let last_blind = if let Some(small_blind) = cfg.small_blind {
            let (small, big) = engine::game::blind_positions(button, n);
            hand.post_live(small, small_blind, |seat, amount| LogEntry::SmallBlind {
                seat,
                amount,
            });
            hand.post_live(big, bb, |seat, amount| LogEntry::BigBlind { seat, amount });
            big
        } else {
            hand.post_live(button, bb, |seat, amount| LogEntry::ButtonBlind {
                seat,
                amount,
            });
            button
        };

        // A short all-in blind still sets the nominal bring-in for other stacks.
        hand.current_bet = bb;
        let first_from = if let Some(multiplier) = cfg.straddle_bb.filter(|_| n >= 3) {
            let straddle = (last_blind + 1) % n;
            let target = bb * u64::from(multiplier);
            let amount = hand.post_live(straddle, target, |seat, amount| LogEntry::Straddle {
                seat,
                amount,
            });
            if amount == target {
                hand.min_raise = target;
            }
            straddle
        } else {
            last_blind
        };
        hand.advance(first_from);
        Ok(hand)
    }

    fn post_live(
        &mut self,
        index: usize,
        target: u64,
        entry: impl FnOnce(usize, u64) -> LogEntry,
    ) -> u64 {
        let player = &mut self.players[index];
        let amount = target.min(player.stack());
        player.contributed += amount;
        player.street_bet = amount;
        self.current_bet = self.current_bet.max(amount);
        self.log.push(entry(player.seat, amount));
        amount
    }

    pub fn finished(&self) -> bool {
        self.outcome.is_some()
    }

    pub(super) fn runout_votes(&self) -> Option<&RunoutVotes> {
        if self.to_act.is_none() && !self.finished() {
            self.runout.as_ref().map(|runout| &runout.votes)
        } else {
            None
        }
    }

    pub(super) fn vote_runout(&mut self, seat: u8, twice: bool) -> bool {
        if self.runout_votes().is_none() {
            return false;
        }
        self.runout
            .as_mut()
            .is_some_and(|runout| runout.votes.record(seat, twice))
    }

    pub(super) fn finish_runout(&mut self) {
        if self.runout_votes().is_none() {
            return;
        }
        let runout = self.runout.take().expect("open runout vote");
        let second = if runout.votes.decision() == Some(true) {
            let shared = self.board_len;
            let mut board = self.board_cards;
            board[shared..].copy_from_slice(&runout.fresh[..5 - shared]);
            Some((shared, board))
        } else {
            None
        };
        while self.street != Street::River {
            self.deal_street();
        }
        self.showdown(second);
    }

    pub fn to_act_seat(&self) -> Option<usize> {
        self.to_act
            .filter(|_| !self.finished())
            .map(|i| self.players[i].seat)
    }

    pub fn participant(&self, seat: usize) -> Option<&Participant> {
        self.players.iter().find(|p| p.seat == seat)
    }

    /// Community cards dealt so far.
    pub fn board(&self) -> &[Card] {
        &self.board_cards[..self.board_len]
    }

    /// Total chips in the middle, antes and live bets included.
    pub fn pot(&self) -> u64 {
        self.players.iter().map(|p| p.contributed).sum()
    }

    fn button_idx(&self) -> usize {
        self.players
            .iter()
            .position(|p| p.seat == self.dealer_seat)
            .unwrap_or(0)
    }

    fn idx(&self, seat: usize) -> Result<usize, ActionError> {
        self.players
            .iter()
            .position(|p| p.seat == seat)
            .ok_or(ActionError::NotInHand)
    }

    fn needs_action(&self, i: usize) -> bool {
        let p = &self.players[i];
        p.can_bet() && (p.street_bet < self.current_bet || !p.acted)
    }

    /// The first player after index `after` (clockwise) who still has to act.
    fn next_to_act(&self, after: usize) -> Option<usize> {
        let n = self.players.len();
        (1..=n)
            .map(|k| (after + k) % n)
            .find(|&i| self.needs_action(i))
    }

    fn live_count(&self) -> usize {
        self.players.iter().filter(|p| !p.folded).count()
    }

    /// The best hand `seat` holds with the board dealt so far.
    pub fn value_of(&self, seat: usize) -> Option<(HandValue, Vec<Card>)> {
        let p = self.participant(seat)?;
        let mut cards = [p.cards[0]; 7];
        cards[1] = p.cards[1];
        let len = 2 + self.board().len();
        cards[2..len].copy_from_slice(self.board());
        if len < 5 {
            return Some(rules::best_hand(&cards[..len]));
        }
        let cached = p.street_value.get_or_init(|| {
            let (value, best) = rules::best_five(&cards[..len]);
            StreetValue {
                cards,
                count: len,
                value,
                best,
            }
        });
        // A participant can be cloned or replaced. The actual hole cards and
        // visible board, including their order, identify the exact tie-break.
        if cached.count == len && cached.cards == cards {
            Some((cached.value, cached.best.to_vec()))
        } else {
            Some(rules::best_hand(&cards[..len]))
        }
    }

    pub fn legal(&self, seat: usize) -> Legal {
        let Ok(i) = self.idx(seat) else {
            return Legal::default();
        };
        if self.finished() || self.to_act != Some(i) {
            return Legal::default();
        }
        let p = &self.players[i];
        let owed = self.current_bet - p.street_bet;
        let max_to = p.street_bet + p.stack();
        let others_can_bet = self
            .players
            .iter()
            .enumerate()
            .any(|(j, q)| j != i && q.can_bet());
        // Raising needs chips beyond the call, an action reopened by a full
        // raise (single or cumulative), and someone left who can still call.
        let reopened = !p.acted || action_reopened(self.current_bet, p.faced, self.min_raise);
        let can_raise = reopened && max_to > self.current_bet && others_can_bet;
        Legal {
            your_turn: true,
            can_check: owed == 0,
            call_cost: (owed > 0).then(|| owed.min(p.stack())),
            can_raise,
            min_raise_to: (self.current_bet + self.min_raise).min(max_to),
            max_raise_to: max_to,
        }
    }

    pub fn apply(&mut self, seat: usize, action: Action) -> Result<(), ActionError> {
        self.apply_inner(seat, action, false)
    }

    /// The clock ran out: check when that is free, otherwise fold.
    pub fn timeout(&mut self) {
        let Some(seat) = self.to_act_seat() else {
            return;
        };
        let action = if self.legal(seat).can_check {
            Action::Check
        } else {
            Action::Fold
        };
        let _ = self.apply_inner(seat, action, true);
    }

    /// Resolve an idle room immediately using the ordinary timeout rules.
    pub fn finish_idle(&mut self) -> bool {
        // With only check/fold decisions, each seat acts at most once on each
        // of four streets. All-in runouts use the existing engine settlement.
        for _ in 0..self.players.len() * 4 {
            if self.finished() {
                return true;
            }
            if self.runout_votes().is_some() {
                self.finish_runout();
            } else {
                self.timeout();
            }
        }
        self.finished()
    }

    /// A player left the table: fold them whether or not it is their turn.
    pub fn force_fold(&mut self, seat: usize) {
        let Ok(i) = self.idx(seat) else { return };
        if let Some(runout) = &mut self.runout {
            runout
                .votes
                .retain_seats(|voter| usize::from(voter) != seat);
        }
        // An all-in player has nothing left to decide and keeps their claim.
        if self.finished() || self.players[i].folded || self.players[i].all_in() {
            return;
        }
        if self.to_act == Some(i) {
            let _ = self.apply_inner(seat, Action::Fold, true);
            return;
        }
        self.players[i].folded = true;
        self.log.push(LogEntry::Fold {
            seat,
            timeout: true,
        });
        if self.live_count() == 1 {
            self.finish_uncontested();
        } else {
            self.cap_lone_player_call();
            if self.to_act.is_none_or(|t| !self.needs_action(t)) || self.betting_closed() {
                self.advance(i);
            }
        }
    }

    fn apply_inner(
        &mut self,
        seat: usize,
        action: Action,
        timeout: bool,
    ) -> Result<(), ActionError> {
        if self.finished() {
            return Err(ActionError::HandFinished);
        }
        let i = self.idx(seat)?;
        if self.players[i].folded {
            return Err(ActionError::AlreadyFolded);
        }
        if self.to_act != Some(i) {
            return Err(ActionError::NotYourTurn);
        }
        let legal = self.legal(seat);
        let action = match action {
            Action::AllIn => {
                let to = legal.max_raise_to;
                if to > self.current_bet && legal.can_raise {
                    Action::RaiseTo(to)
                } else if legal.call_cost.is_some() {
                    Action::Call
                } else {
                    return Err(ActionError::RaiseNotAllowed);
                }
            }
            a => a,
        };
        match action {
            Action::Fold => {
                self.players[i].folded = true;
                self.log.push(LogEntry::Fold { seat, timeout });
            }
            Action::Check => {
                if !legal.can_check {
                    return Err(ActionError::CannotCheck);
                }
                self.log.push(LogEntry::Check { seat, timeout });
            }
            Action::Call => {
                let Some(amount) = legal.call_cost else {
                    return Err(ActionError::NothingToCall);
                };
                let p = &mut self.players[i];
                p.contributed += amount;
                p.street_bet += amount;
                let all_in = p.stack() == 0;
                self.log.push(LogEntry::Call {
                    seat,
                    amount,
                    all_in,
                });
            }
            Action::RaiseTo(to) => {
                if !legal.can_raise {
                    return Err(ActionError::RaiseNotAllowed);
                }
                if to > legal.max_raise_to {
                    return Err(ActionError::InsufficientChips);
                }
                let all_in = to == legal.max_raise_to;
                if to <= self.current_bet || (to < legal.min_raise_to && !all_in) {
                    return Err(ActionError::InvalidRaise);
                }
                let increment = to - self.current_bet;
                let full = increment >= self.min_raise;
                let opening = self.current_bet == 0;
                let p = &mut self.players[i];
                let amount = to - p.street_bet;
                p.contributed += amount;
                p.street_bet = to;
                if full {
                    self.min_raise = increment;
                }
                self.current_bet = to;
                let entry = if opening {
                    LogEntry::Bet {
                        seat,
                        to,
                        amount,
                        all_in,
                    }
                } else {
                    LogEntry::Raise {
                        seat,
                        to,
                        amount,
                        all_in,
                    }
                };
                self.log.push(entry);
            }
            Action::AllIn => unreachable!("normalized above"),
        }
        let p = &mut self.players[i];
        p.acted = true;
        p.faced = self.current_bet;
        if self.live_count() == 1 {
            self.finish_uncontested();
        } else {
            self.advance(i);
        }
        Ok(())
    }

    /// No more betting is possible: fewer than two players can still put
    /// chips in, and none of them owes a call.
    fn betting_closed(&self) -> bool {
        let mut can_bet = self.players.iter().filter(|p| p.can_bet());
        can_bet.clone().count() < 2 && !can_bet.any(|p| p.street_bet < self.current_bet)
    }

    /// A lone live stack only has to match chips that a live opponent posted.
    fn cap_lone_player_call(&mut self) {
        let live = self.players.iter().filter(|p| !p.folded);
        if live.clone().filter(|p| p.can_bet()).count() == 1 {
            self.current_bet = live.map(|p| p.street_bet).max().unwrap_or(0);
        }
    }

    /// Hands the turn on, or closes the street when nobody owes an action.
    fn advance(&mut self, from: usize) {
        self.cap_lone_player_call();
        if !self.betting_closed() {
            if let Some(next) = self.next_to_act(from) {
                self.to_act = Some(next);
                return;
            }
        }
        self.to_act = None;
        self.return_uncalled();
        if self.street < Street::River && self.betting_closed() {
            if let Some(runout) = &mut self.runout {
                runout.votes.retain_seats(|seat| {
                    self.players
                        .iter()
                        .any(|p| p.seat == usize::from(seat) && !p.folded)
                });
                if !runout.votes.seats().is_empty() {
                    return;
                }
            }
        }
        // Betting continues only while two or more players can still bet.
        loop {
            if self.street == Street::River {
                self.showdown(None);
                return;
            }
            self.deal_street();
            if self.players.iter().filter(|p| p.can_bet()).count() >= 2 {
                let button = self.button_idx();
                self.to_act = self.next_to_act(button);
                if self.to_act.is_some() {
                    return;
                }
            }
        }
    }

    fn deal_street(&mut self) {
        let (street, len) = match self.street {
            Street::Preflop => (Street::Flop, 3),
            Street::Flop => (Street::Turn, 4),
            _ => (Street::River, 5),
        };
        let from = self.board_len;
        self.street = street;
        self.board_len = len;
        self.current_bet = 0;
        self.min_raise = self.cfg.big_blind();
        for p in self.players.iter_mut() {
            p.street_value.take();
            p.street_bet = 0;
            p.acted = false;
            p.faced = 0;
        }
        self.log.push(LogEntry::Street {
            street,
            cards: self.board_cards[from..len]
                .iter()
                .map(|c| c.code())
                .collect(),
        });
    }

    /// The street's top bet, where nobody matched it, goes back to its owner.
    fn return_uncalled(&mut self) {
        let mut bets: Vec<(u64, usize)> = self
            .players
            .iter()
            .enumerate()
            .map(|(i, p)| (p.street_bet, i))
            .collect();
        bets.sort_by(|a, b| b.cmp(a));
        let (top, i) = bets[0];
        let second = bets.get(1).map_or(0, |b| b.0);
        if top > second {
            let amount = top - second;
            let p = &mut self.players[i];
            p.street_bet -= amount;
            p.contributed -= amount;
            self.current_bet = p.street_bet;
            self.log.push(LogEntry::Return {
                seat: p.seat,
                amount,
            });
        }
    }

    fn finish_uncontested(&mut self) {
        self.to_act = None;
        self.return_uncalled();
        let winner = self
            .players
            .iter()
            .position(|p| !p.folded)
            .expect("one live player");
        let pot = self.pot();
        let seat = self.players[winner].seat;
        self.log.push(LogEntry::Win {
            pot: 0,
            seats: vec![seat],
            amount: pot,
        });
        let mut payouts = vec![0u64; self.players.len()];
        payouts[winner] = pot;
        self.settle(
            payouts,
            vec![PotResult {
                amount: pot,
                winners: vec![seat],
            }],
            false,
            Vec::new(),
        );
    }

    fn showdown(&mut self, second: Option<(usize, [Card; 5])>) {
        self.street = Street::Showdown;
        self.to_act = None;
        let n = self.players.len();
        let values: Vec<Option<(HandValue, Vec<Card>)>> = self
            .players
            .iter()
            .map(|p| (!p.folded).then(|| self.value_of(p.seat)).flatten())
            .collect();
        // Clockwise from the first seat left of the button: odd chips and
        // the reveal order both start there.
        let button = self.button_idx();
        let order: Vec<usize> = (1..=n).map(|k| (button + k) % n).collect();
        let shown_seats: Vec<usize> = order
            .iter()
            .filter(|&&i| !self.players[i].folded)
            .map(|&i| self.players[i].seat)
            .collect();
        self.log.push(LogEntry::Showdown { seats: shown_seats });

        let contributions: Vec<_> = order
            .iter()
            .map(|&i| Contribution {
                id: i,
                amount: self.players[i].contributed,
                live: !self.players[i].folded,
            })
            .collect();
        let second_values = second.as_ref().map(|(_, board)| {
            self.players
                .iter()
                .map(|p| {
                    if p.folded {
                        return None;
                    }
                    let [a, b, c, d, e] = *board;
                    Some(rules::best_hand(&[p.cards[0], p.cards[1], a, b, c, d, e]).0)
                })
                .collect::<Vec<_>>()
        });
        let first_values: Vec<_> = values
            .iter()
            .map(|value| value.as_ref().map(|v| v.0))
            .collect();
        let mut payouts = vec![0u64; n];
        let mut first_payouts = vec![0u64; n];
        let mut second_payouts = vec![0u64; n];
        let mut pots = Vec::new();
        for layer in pot_layers(&contributions, PotPolicy::SweepFoldedExcess) {
            let amount = layer.amount;
            let eligible = layer.eligible;
            let (first, second) = if second_values.is_some() {
                runout_halves(amount)
            } else {
                (amount, 0)
            };
            let mut winners = award_pot(first, &eligible, &first_values, &mut first_payouts);
            if let Some(values) = &second_values {
                for winner in award_pot(second, &eligible, values, &mut second_payouts) {
                    if !winners.contains(&winner) {
                        winners.push(winner);
                    }
                }
            }
            let seats: Vec<_> = winners.iter().map(|&i| self.players[i].seat).collect();
            self.log.push(LogEntry::Win {
                pot: pots.len(),
                seats: seats.clone(),
                amount,
            });
            pots.push(PotResult {
                amount,
                winners: seats,
            });
        }
        for i in 0..n {
            payouts[i] = first_payouts[i] + second_payouts[i];
        }
        let run_twice = second
            .zip(second_values)
            .map(|((shared, board), second_values)| {
                let awards = |amounts: &[u64], values: &[Option<HandValue>]| {
                    order
                        .iter()
                        .filter_map(|&i| {
                            let amount = amounts[i];
                            let category = values[i]?.category;
                            (amount > 0).then_some(RunAward {
                                seat: self.players[i].seat,
                                amount,
                                category,
                            })
                        })
                        .collect()
                };
                RunTwice {
                    shared,
                    board: board.iter().map(|card| card.code()).collect(),
                    first: awards(&first_payouts, &first_values),
                    second: awards(&second_payouts, &second_values),
                }
            });
        if let Some(run) = &run_twice {
            self.log.push(LogEntry::RunTwice { run: run.clone() });
        }
        let shown = order
            .iter()
            .filter_map(|&i| {
                let (v, best) = values[i].as_ref()?;
                let p = &self.players[i];
                Some(Shown {
                    seat: p.seat,
                    cards: p.cards.iter().map(|c| c.code()).collect(),
                    category: v.category,
                    best: best.iter().map(|c| c.code()).collect(),
                })
            })
            .collect();
        self.settle(payouts, pots, true, shown);
        self.outcome.as_mut().expect("showdown settled").run_twice = run_twice;
    }

    fn settle(
        &mut self,
        payouts: Vec<u64>,
        pots: Vec<PotResult>,
        showdown: bool,
        shown: Vec<Shown>,
    ) {
        let pot = self.pot();
        debug_assert_eq!(payouts.iter().sum::<u64>(), pot, "chips conserved");
        let deltas: Vec<(usize, i64)> = self
            .players
            .iter()
            .zip(&payouts)
            .map(|(p, &won)| (p.seat, won as i64 - p.contributed as i64))
            .collect();
        let winners = self
            .players
            .iter()
            .zip(&payouts)
            .filter(|(_, &won)| won > 0)
            .map(|(p, _)| p.seat)
            .collect();
        self.outcome = Some(Outcome {
            winners,
            showdown,
            pot,
            pots,
            shown,
            run_twice: None,
            deltas,
        });
    }

    /// After the hand, a player whose cards were not shown down (a folder or
    /// the uncontested winner) may show either card. Returns whether anything
    /// new became public.
    pub fn show_cards(&mut self, seat: usize, indices: &[usize]) -> bool {
        let Some(outcome) = &self.outcome else {
            return false;
        };
        if self.participant(seat).is_none() || outcome.shown.iter().any(|s| s.seat == seat) {
            return false;
        }
        let mut changed = false;
        for &index in indices.iter().take(2) {
            if index < 2 && !self.voluntary_shows.contains(&(seat, index)) {
                self.voluntary_shows.push((seat, index));
                changed = true;
            }
        }
        changed
    }

    /// 发发看: the board cards a finished hand never dealt, in deal order.
    /// Empty while the hand runs and when the board reached the river.
    pub fn undealt_board(&self) -> &[Card] {
        if self.finished() {
            &self.board_cards[self.board_len..]
        } else {
            &[]
        }
    }

    /// The positions `seat` showed voluntarily, public to every viewer.
    pub fn voluntary_cards(&self, seat: usize) -> Option<[Option<Card>; 2]> {
        let p = self.participant(seat)?;
        let shown = |i: usize| {
            self.voluntary_shows
                .contains(&(seat, i))
                .then_some(p.cards[i])
        };
        let cards = [shown(0), shown(1)];
        cards.iter().any(Option::is_some).then_some(cards)
    }

    /// Cards `viewer` may see at `seat`: their own, and every hand shown down.
    pub fn visible_cards(&self, viewer: Option<usize>, seat: usize) -> Option<[Card; 2]> {
        let p = self.participant(seat)?;
        let shown = self
            .outcome
            .as_ref()
            .is_some_and(|o| o.shown.iter().any(|s| s.seat == seat));
        (viewer == Some(seat) || shown).then_some(p.cards)
    }
}

fn award_pot(
    amount: u64,
    eligible: &[usize],
    values: &[Option<HandValue>],
    payouts: &mut [u64],
) -> Vec<usize> {
    let best = eligible
        .iter()
        .filter_map(|&i| values[i])
        .max()
        .expect("eligible live player");
    let winners: Vec<_> = eligible
        .iter()
        .copied()
        .filter(|&i| values[i] == Some(best))
        .collect();
    let count = winners.len() as u64;
    for (position, &winner) in winners.iter().enumerate() {
        payouts[winner] += amount / count + u64::from((position as u64) < amount % count);
    }
    if amount == 0 {
        Vec::new()
    } else {
        winners
    }
}

#[cfg(test)]
mod opening_tests {
    use super::*;

    #[test]
    fn independent_ante_adds_to_the_pot_without_changing_the_call() {
        let cfg = HandConfig {
            ante: 20,
            table_ante: Some(7),
            small_blind: Some(10),
            straddle_bb: None,
        };
        let seats = [(0, 200), (1, 200), (2, 200)];
        let deck = rules::full_deck();
        let hand = Hand::new(cfg, &seats, 0, &deck, None, &[]).unwrap();
        assert_eq!(min_stack(&cfg), 27);
        assert_eq!(hand.pot(), 51);
        assert_eq!(hand.legal(0).call_cost, Some(20));
        assert_eq!(hand.log[0], LogEntry::Ante { seat: 0, amount: 7 });

        let bomb = Hand::new(cfg, &seats, 0, &deck, Some(60), &[]).unwrap();
        assert_eq!(bomb.pot(), 180);

        let off = HandConfig {
            small_blind: None,
            table_ante: Some(0),
            ..cfg
        };
        let button_only = Hand::new(off, &seats, 0, &deck, None, &[]).unwrap();
        assert_eq!(button_only.pot(), 20);
        assert_eq!(off.button_blind(), 20);
    }

    #[test]
    fn custom_blinds_and_straddle_follow_occupied_seats() {
        let cfg = HandConfig {
            ante: 20,
            table_ante: None,
            small_blind: Some(10),
            straddle_bb: Some(2),
        };
        let deck = rules::full_deck();
        let hand = Hand::new(
            cfg,
            &[(0, 200), (2, 200), (3, 200), (5, 200)],
            2,
            &deck,
            None,
            &[],
        )
        .unwrap();
        assert_eq!(hand.to_act_seat(), Some(2));
        assert_eq!(
            hand.log,
            [
                LogEntry::SmallBlind {
                    seat: 3,
                    amount: 10
                },
                LogEntry::BigBlind {
                    seat: 5,
                    amount: 20
                },
                LogEntry::Straddle {
                    seat: 0,
                    amount: 40
                },
            ]
        );
        assert_eq!(hand.legal(2).min_raise_to, 80);

        let heads_up = Hand::new(cfg, &[(0, 200), (5, 200)], 5, &deck, None, &[]).unwrap();
        assert_eq!(heads_up.to_act_seat(), Some(5));
        assert_eq!(heads_up.pot(), 30);
    }

    #[test]
    fn default_straddle_comes_after_button_and_capped_post_keeps_big_blind_increment() {
        let cfg = HandConfig {
            ante: 20,
            table_ante: None,
            small_blind: None,
            straddle_bb: Some(2),
        };
        let hand = Hand::new(
            cfg,
            &[(0, 200), (2, 50), (5, 200)],
            0,
            &rules::full_deck(),
            None,
            &[],
        )
        .unwrap();
        assert_eq!(hand.to_act_seat(), Some(5));
        assert_eq!(hand.pot(), 110);
        assert_eq!(hand.legal(5).call_cost, Some(30));
        assert_eq!(hand.legal(5).min_raise_to, 50);
    }

    #[test]
    fn short_blind_keeps_the_bring_in_until_one_stack_can_bet() {
        let cfg = HandConfig {
            ante: 100,
            table_ante: None,
            small_blind: Some(50),
            straddle_bb: None,
        };
        let deck = rules::full_deck();
        let mut hand = Hand::new(cfg, &[(0, 300), (1, 300), (2, 75)], 0, &deck, None, &[]).unwrap();
        assert_eq!(hand.pot(), 125);
        assert_eq!(hand.legal(0).call_cost, Some(100));
        assert_eq!(hand.legal(0).min_raise_to, 200);
        hand.apply(0, Action::Fold).unwrap();
        assert_eq!(hand.legal(1).call_cost, Some(25));
        assert!(!hand.legal(1).can_raise);
        hand.apply(1, Action::Call).unwrap();
        assert!(hand.finished());
        assert_eq!(hand.pot(), 150);

        let heads_up = Hand::new(cfg, &[(0, 300), (2, 30)], 0, &deck, None, &[]).unwrap();
        assert!(heads_up.finished());
        assert_eq!(heads_up.pot(), 60);

        let default_opening = HandConfig {
            small_blind: None,
            ..cfg
        };
        let hand = Hand::new(
            default_opening,
            &[(0, 150), (1, 300), (2, 300)],
            0,
            &deck,
            None,
            &[],
        )
        .unwrap();
        assert_eq!(hand.pot(), 350);
        assert_eq!(hand.legal(1).call_cost, Some(100));
        assert_eq!(hand.legal(1).min_raise_to, 200);
    }

    #[test]
    fn bomb_pot_starts_on_flop_and_preserves_short_stack_claim() {
        let cfg = HandConfig {
            ante: 20,
            table_ante: None,
            small_blind: Some(10),
            straddle_bb: Some(2),
        };
        let hand = Hand::new(
            cfg,
            &[(0, 200), (2, 45), (5, 200)],
            0,
            &rules::full_deck(),
            Some(100),
            &[],
        )
        .unwrap();
        assert_eq!(hand.street, Street::Flop);
        assert_eq!(hand.to_act_seat(), Some(5));
        assert_eq!(hand.board().len(), 3);
        assert_eq!(hand.pot(), 245);
        assert!(hand.participant(2).unwrap().all_in());
        assert!(hand.legal(5).can_check);
        assert_eq!(hand.legal(5).min_raise_to, 20);
        assert!(matches!(
            hand.log.as_slice(),
            [
                LogEntry::BombPot { amount: 100, .. },
                LogEntry::BombPot { amount: 45, .. },
                LogEntry::BombPot { amount: 100, .. },
                LogEntry::Street {
                    street: Street::Flop,
                    ..
                },
            ]
        ));
    }
}
