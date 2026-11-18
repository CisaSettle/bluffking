//! One 炸金花 hand as a pure state machine (no IO, no clocks, no RNG).
//!
//! Betting unit: a blind (闷牌) player pays the current level, a player who
//! has looked pays twice the level. Raising lifts the level in ante increments.
//! Each payment is capped at 200 antes. Compare (比牌) is allowed from round 2, costs
//! `compare_mult` call units (room setting, default two), and a tie loses for the
//! initiator. After `max_rounds` full rounds all remaining players are forced to
//! show down. An optional 赖子 rank makes every card of that rank wild, and
//! an optional 大小王 setting adds two wild jokers to the deck
//! ([`rules::evaluate_with`]); a hand made with a 赖子 collects the 喜钱 bonus
//! only when the room enables `wild_bonus`.
//!
//! Admission funds the ante and one looked minimum call. Later actions are
//! limited by the remaining stack; folding stays available. When the remaining
//! stack cannot cover more than a call, all-in pays what is left and immediately
//! shows down every remaining hand. Any action exhausting a live player's stack
//! also ends betting with a showdown. Those showdowns settle contribution
//! layers separately and return unmatched live bets. Other hands keep the
//! ordinary ZJH single-pot rules, including unequal blind/seen payments.

use std::collections::HashSet;

use serde::Serialize;

use super::rules::{self, Card, Category, HandValue};

/// Conservative bot sizing preferences, not the legal raise limit.
pub const RAISE_MULTS: [u64; 4] = [2, 3, 5, 10];
pub const MAX_BET_ANTES: u64 = 200;
pub const DEFAULT_MAX_ROUNDS: u8 = 3;
pub const MIN_ROUNDS: u8 = 3;
pub const MAX_ROUNDS: u8 = 10;
/// 比牌 price in call units; the room creator picks 1..=MAX.
pub const DEFAULT_COMPARE_MULT: u64 = 2;
pub const MAX_COMPARE_MULT: u64 = 5;
pub const STRAIGHT_FLUSH_BONUS_ANTES: u64 = 5;
pub const LEOPARD_BONUS_ANTES: u64 = 10;

pub const MAX_BONUS_ANTES: u64 = 100;

#[derive(Debug, Clone, Copy, serde::Serialize, serde::Deserialize)]
pub struct BonusConfig {
    pub straight_flush: u64,
    pub leopard: u64,
}

impl Default for BonusConfig {
    fn default() -> Self {
        Self {
            straight_flush: STRAIGHT_FLUSH_BONUS_ANTES,
            leopard: LEOPARD_BONUS_ANTES,
        }
    }
}

impl BonusConfig {
    pub fn valid(self) -> bool {
        self.straight_flush <= MAX_BONUS_ANTES && self.leopard <= MAX_BONUS_ANTES
    }
}

#[derive(Debug, Clone, Copy, serde::Serialize, serde::Deserialize)]
pub struct HandConfig {
    pub ante: u64,
    pub max_rounds: u8,
    pub bonus: BonusConfig,
    /// Every card of this rank (2..=14) is a 赖子; `None` = natural deck.
    #[serde(default)]
    pub wild_rank: Option<u8>,
    /// The deck holds the small and big joker, both 赖子 (54 cards).
    #[serde(default)]
    pub jokers: bool,
    /// Whether a 顺金/豹子 made with a 赖子 collects the 喜钱 bonus.
    #[serde(default)]
    pub wild_bonus: bool,
    /// Rooms checkpointed before this setting existed priced 比牌 at two units.
    #[serde(default = "default_compare_mult")]
    pub compare_mult: u64,
}

fn default_compare_mult() -> u64 {
    DEFAULT_COMPARE_MULT
}

impl HandConfig {
    pub fn wild(&self) -> rules::Wild {
        rules::Wild {
            rank: self.wild_rank,
            jokers: self.jokers,
        }
    }
    pub fn max_level(&self) -> u64 {
        self.ante * MAX_BET_ANTES
    }
}

pub fn min_stack(cfg: &HandConfig) -> u64 {
    cfg.ante * 3
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct Participant {
    pub seat: usize,
    pub stack_start: u64,
    pub cards: [Card; 3],
    pub value: HandValue,
    pub looked: bool,
    pub folded: bool,
    pub lost_compare: bool,
    pub contributed: u64,
    last_acted_round: u8,
}

impl Participant {
    pub fn active(&self) -> bool {
        !self.folded
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Action {
    Look,
    Call,
    Raise { level: u64 },
    Fold,
    Compare { target: usize },
    AllIn,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ActionError {
    NotInHand,
    HandFinished,
    NotYourTurn,
    AlreadyFolded,
    AlreadyLooked,
    InvalidRaise,
    CompareNotAllowed,
    AllInNotAllowed,
    InvalidTarget,
    InsufficientChips,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, serde::Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum LogEntry {
    ShowCards {
        seat: usize,
    },
    Ante {
        seat: usize,
        amount: u64,
    },
    Look {
        seat: usize,
    },
    Call {
        seat: usize,
        amount: u64,
    },
    Raise {
        seat: usize,
        level: u64,
        amount: u64,
    },
    Fold {
        seat: usize,
        timeout: bool,
    },
    Compare {
        seat: usize,
        target: usize,
        winner: usize,
        amount: u64,
    },
    /// 孤注一掷: `seat` paid its remaining `amount` and compared with
    /// `target`, the only other live player. No longer dealt (an all-in is
    /// [`LogEntry::AllInShowdown`] since 2026-11-09); settled rounds saved
    /// before then carry it, and history readers still read those logs.
    AllIn {
        seat: usize,
        target: usize,
        winner: usize,
        amount: u64,
    },
    AllInShowdown {
        seat: usize,
        amount: u64,
    },
    Showdown {
        seats: Vec<usize>,
    },
    PotAward {
        seats: Vec<usize>,
        amount: u64,
    },
    UncalledReturn {
        seat: usize,
        amount: u64,
    },
    Bonus {
        seat: usize,
        category: Category,
        amount: u64,
        payments: Vec<(usize, u64)>,
    },
    Win {
        seats: Vec<usize>,
        pot: u64,
    },
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, serde::Deserialize)]
pub struct Outcome {
    pub winners: Vec<usize>,
    pub showdown: bool,
    pub pot: u64,
    /// `(seat, net chips)` for every participant; sums to zero.
    pub deltas: Vec<(usize, i64)>,
    /// Separate from the pot; included in net deltas.
    pub bonus: Option<Bonus>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, serde::Deserialize)]
pub struct Bonus {
    pub seat: usize,
    pub category: Category,
    pub amount: u64,
    pub payments: Vec<(usize, u64)>,
}

#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub struct RaiseOption {
    pub level: u64,
    pub cost: u64,
}

#[derive(Debug, Clone, Default, Serialize, PartialEq, Eq)]
pub struct Legal {
    pub can_show_cards: bool,
    pub can_look: bool,
    pub can_fold: bool,
    pub your_turn: bool,
    pub call_cost: Option<u64>,
    pub raises: Vec<RaiseOption>,
    pub compare_cost: Option<u64>,
    pub compare_targets: Vec<usize>,
    /// Remaining stack, offered when it is no greater than the call price.
    pub all_in_cost: Option<u64>,
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct Hand {
    pub cfg: HandConfig,
    pub players: Vec<Participant>,
    pub dealer_seat: usize,
    to_act: usize,
    pub round: u8,
    pub level: u64,
    pub pot: u64,
    pub log: Vec<LogEntry>,
    /// `(viewer seat, target seat)` pairs revealed by a compare.
    reveals: HashSet<(usize, usize)>,
    showdown_seats: Vec<usize>,
    pub outcome: Option<Outcome>,
    pub zk: Option<super::fairness::Stored>,
}

impl Hand {
    /// `seats` = `(seat, stack)` for every dealt-in player, any order.
    /// `deck` must hold at least `3 * seats.len()` cards.
    pub fn new(
        cfg: HandConfig,
        seats: &[(usize, u64)],
        dealer_seat: usize,
        deck: &[Card],
    ) -> Result<Self, ActionError> {
        let mut seats = seats.to_vec();
        seats.sort_by_key(|s| s.0);
        if seats.len() < 2 || deck.len() < seats.len() * 3 {
            return Err(ActionError::NotInHand);
        }
        let need = min_stack(&cfg);
        if seats.iter().any(|&(_, stack)| stack < need) {
            return Err(ActionError::InsufficientChips);
        }
        let players: Vec<Participant> = seats
            .iter()
            .enumerate()
            .map(|(i, &(seat, stack))| {
                let cards = [deck[i * 3], deck[i * 3 + 1], deck[i * 3 + 2]];
                Participant {
                    seat,
                    stack_start: stack,
                    cards,
                    value: rules::evaluate_with(&cards, cfg.wild()),
                    looked: false,
                    folded: false,
                    lost_compare: false,
                    contributed: cfg.ante,
                    last_acted_round: 0,
                }
            })
            .collect();
        let log = players
            .iter()
            .map(|p| LogEntry::Ante {
                seat: p.seat,
                amount: cfg.ante,
            })
            .collect();
        let dealer_idx = players
            .iter()
            .position(|p| p.seat == dealer_seat)
            .unwrap_or(0);
        let n = players.len();
        let pot = cfg.ante * n as u64;
        Ok(Self {
            cfg,
            dealer_seat: players[dealer_idx].seat,
            to_act: (dealer_idx + 1) % n,
            round: 1,
            level: cfg.ante,
            pot,
            log,
            reveals: HashSet::new(),
            showdown_seats: Vec::new(),
            outcome: None,
            zk: None,
            players,
        })
    }

    pub fn finished(&self) -> bool {
        self.outcome.is_some()
    }

    pub fn to_act_seat(&self) -> Option<usize> {
        (!self.finished()).then(|| self.players[self.to_act].seat)
    }

    pub fn participant(&self, seat: usize) -> Option<&Participant> {
        self.players.iter().find(|p| p.seat == seat)
    }

    /// The most a folded `seat` can still lose: what it put in, plus the
    /// premium-hand bonus a sole winner collects from every other dealt seat
    /// (capped by its uncommitted stack, as settlement charges it).
    /// `None` while the seat may still win or bet.
    pub fn folded_exposure(&self, seat: usize) -> Option<u64> {
        let p = self.participant(seat).filter(|p| p.folded)?;
        let bonus = self
            .cfg
            .ante
            .saturating_mul(self.cfg.bonus.straight_flush.max(self.cfg.bonus.leopard));
        Some(p.contributed.saturating_add(bonus).min(p.stack_start))
    }

    fn idx(&self, seat: usize) -> Result<usize, ActionError> {
        self.players
            .iter()
            .position(|p| p.seat == seat)
            .ok_or(ActionError::NotInHand)
    }

    fn unit(&self, idx: usize) -> u64 {
        let amount = if self.players[idx].looked {
            self.level * 2
        } else {
            self.level
        };
        amount.min(self.cfg.max_level())
    }

    fn compare_price(&self, idx: usize) -> u64 {
        (self.unit(idx) * self.cfg.compare_mult).min(self.cfg.max_level())
    }

    fn active_count(&self) -> usize {
        self.players.iter().filter(|p| p.active()).count()
    }

    pub fn legal(&self, seat: usize) -> Legal {
        let Ok(idx) = self.idx(seat) else {
            return Legal::default();
        };
        let p = &self.players[idx];
        if self.finished() || p.folded {
            return Legal {
                can_show_cards: self.can_show_cards(seat),
                ..Legal::default()
            };
        }
        let mut legal = Legal {
            can_look: !p.looked,
            can_fold: true,
            ..Legal::default()
        };
        if self.to_act != idx {
            return legal;
        }
        legal.your_turn = true;
        let unit = self.unit(idx);
        let mult = if p.looked { 2 } else { 1 };
        let available = p.stack_start - p.contributed;
        legal.call_cost = (unit <= available).then_some(unit);
        legal.all_in_cost = (unit >= available).then_some(available);
        legal.raises = (2..=MAX_BET_ANTES)
            .map(|m| m * self.cfg.ante)
            .filter(|&level| {
                level > self.level && level * mult <= available.min(self.cfg.max_level())
            })
            .map(|level| RaiseOption {
                level,
                cost: level * mult,
            })
            .collect();
        let compare_price = self.compare_price(idx);
        if self.round >= 2 && compare_price <= available {
            legal.compare_cost = Some(compare_price);
            legal.compare_targets = self
                .players
                .iter()
                .filter(|o| o.active() && o.seat != seat)
                .map(|o| o.seat)
                .collect();
        }
        legal
    }

    fn pay(&mut self, idx: usize, amount: u64) -> Result<(), ActionError> {
        let p = &mut self.players[idx];
        if p.stack_start < p.contributed + amount {
            return Err(ActionError::InsufficientChips);
        }
        p.contributed += amount;
        self.pot += amount;
        Ok(())
    }

    pub fn apply(&mut self, seat: usize, action: Action) -> Result<(), ActionError> {
        self.apply_inner(seat, action, false)
    }

    /// Turn clock expired: the player on turn folds.
    pub fn timeout(&mut self) -> Option<usize> {
        let seat = self.to_act_seat()?;
        self.apply_inner(seat, Action::Fold, true).ok()?;
        Some(seat)
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
        let idx = self.idx(seat)?;
        if self.players[idx].folded {
            return Err(ActionError::AlreadyFolded);
        }
        match action {
            Action::Look => {
                if self.players[idx].looked {
                    return Err(ActionError::AlreadyLooked);
                }
                self.players[idx].looked = true;
                self.log.push(LogEntry::Look { seat });
                return Ok(());
            }
            Action::Fold => {
                self.players[idx].folded = true;
                self.log.push(LogEntry::Fold { seat, timeout });
                if self.active_count() == 1 {
                    self.finish_last_standing();
                } else if self.to_act == idx {
                    self.advance();
                }
                return Ok(());
            }
            _ => {}
        }
        if self.to_act != idx {
            return Err(ActionError::NotYourTurn);
        }
        match action {
            Action::Call => {
                let amount = self.unit(idx);
                self.pay(idx, amount)?;
                self.log.push(LogEntry::Call { seat, amount });
            }
            Action::Raise { level } => {
                let mult = if self.players[idx].looked { 2 } else { 1 };
                let valid = level > self.level
                    && level <= self.cfg.max_level() / mult
                    && level.is_multiple_of(self.cfg.ante);
                if !valid {
                    return Err(ActionError::InvalidRaise);
                }
                let amount = if self.players[idx].looked {
                    level * 2
                } else {
                    level
                };
                self.pay(idx, amount)?;
                self.level = level;
                self.log.push(LogEntry::Raise {
                    seat,
                    level,
                    amount,
                });
            }
            Action::Compare { target } => {
                if self.round < 2 {
                    return Err(ActionError::CompareNotAllowed);
                }
                let t = self.idx(target).map_err(|_| ActionError::InvalidTarget)?;
                if t == idx || self.players[t].folded {
                    return Err(ActionError::InvalidTarget);
                }
                let amount = self.compare_price(idx);
                self.pay(idx, amount)?;
                let winner = self.resolve_compare(idx, t);
                self.log.push(LogEntry::Compare {
                    seat,
                    target,
                    winner,
                    amount,
                });
            }
            Action::AllIn => {
                let amount = self.players[idx].stack_start - self.players[idx].contributed;
                if self.unit(idx) < amount {
                    return Err(ActionError::AllInNotAllowed);
                }
                self.pay(idx, amount)?;
                self.log.push(LogEntry::AllInShowdown { seat, amount });
                self.finish_showdown();
                return Ok(());
            }
            Action::Look | Action::Fold => unreachable!(),
        }
        self.players[idx].last_acted_round = self.round;
        if self.active_count() == 1 {
            self.finish_last_standing();
        } else if self.players[idx].active()
            && self.players[idx].contributed == self.players[idx].stack_start
        {
            self.finish_showdown();
        } else {
            self.advance();
        }
        Ok(())
    }

    /// Initiator `idx` against `t`: the loser is out (a tie loses for the
    /// initiator) and the pair see each other's cards, a blind winner's only
    /// once it is no longer blind (`visible_cards`). Returns the winner seat.
    fn resolve_compare(&mut self, idx: usize, t: usize) -> usize {
        let initiator_wins = rules::compare(&self.players[idx].value, &self.players[t].value)
            == std::cmp::Ordering::Greater;
        let (winner, loser) = if initiator_wins { (idx, t) } else { (t, idx) };
        self.players[loser].folded = true;
        self.players[loser].lost_compare = true;
        let (seat, target) = (self.players[idx].seat, self.players[t].seat);
        self.reveals.insert((seat, target));
        self.reveals.insert((target, seat));
        self.players[winner].seat
    }

    fn advance(&mut self) {
        let n = self.players.len();
        let mut next = (self.to_act + 1) % n;
        while self.players[next].folded {
            next = (next + 1) % n;
        }
        if self.players[next].last_acted_round >= self.round {
            self.round += 1;
            if self.round > self.cfg.max_rounds {
                self.round = self.cfg.max_rounds;
                self.finish_showdown();
                return;
            }
        }
        self.to_act = next;
    }

    fn finish_last_standing(&mut self) {
        let winner = self
            .players
            .iter()
            .position(|p| p.active())
            .expect("one active player");
        self.settle(vec![winner], false);
    }

    fn finish_showdown(&mut self) {
        let active: Vec<usize> = (0..self.players.len())
            .filter(|&i| self.players[i].active())
            .collect();
        let values: Vec<HandValue> = active.iter().map(|&i| self.players[i].value).collect();
        let winners = rules::showdown_winners(&values)
            .into_iter()
            .map(|w| active[w])
            .collect();
        self.showdown_seats = active.iter().map(|&i| self.players[i].seat).collect();
        self.log.push(LogEntry::Showdown {
            seats: self.showdown_seats.clone(),
        });
        self.settle(winners, true);
    }

    /// `winners` = participant indices. Odd chips go to the first winner
    /// clockwise from the dealer.
    fn settle(&mut self, mut winners: Vec<usize>, showdown: bool) {
        let n = self.players.len();
        let dealer_idx = self
            .players
            .iter()
            .position(|p| p.seat == self.dealer_seat)
            .unwrap_or(0);
        winners.sort_by_key(|&i| (i + n - dealer_idx - 1) % n);
        let layered = showdown
            && self
                .players
                .iter()
                .any(|p| p.active() && p.contributed == p.stack_start);
        let (awards, pot_winners, returned) = if layered {
            self.all_in_awards(&winners, dealer_idx)
        } else {
            let mut awards = vec![0; n];
            self.split_award(self.pot, &winners, &mut awards);
            (awards, winners.clone(), 0)
        };
        let mut deltas: Vec<(usize, i64)> = self
            .players
            .iter()
            .enumerate()
            .map(|(i, p)| (p.seat, awards[i] as i64 - p.contributed as i64))
            .collect();
        // A sole winning premium hand collects from every other dealt seat,
        // including folds. Only uncommitted table chips can pay; no debt,
        // wallet top-up or extra pot contribution is created. Ties pay none,
        // nor do hands made with a 赖子 unless the room enables it.
        let bonus = if let [winner] = winners.as_slice() {
            let category = self.players[*winner].value.category;
            let mult = match category {
                _ if self.players[*winner].value.wild && !self.cfg.wild_bonus => 0,
                Category::StraightFlush => self.cfg.bonus.straight_flush,
                Category::Leopard => self.cfg.bonus.leopard,
                _ => 0,
            };
            if mult > 0 {
                let due = self.cfg.ante.saturating_mul(mult);
                let mut payments = Vec::with_capacity(n - 1);
                let mut amount = 0;
                for (i, p) in self.players.iter().enumerate() {
                    if i == *winner {
                        continue;
                    }
                    let paid = due.min(p.stack_start.saturating_sub(p.contributed));
                    deltas[i].1 -= paid as i64;
                    amount += paid;
                    payments.push((p.seat, paid));
                }
                deltas[*winner].1 += amount as i64;
                let seat = self.players[*winner].seat;
                // Public proof for the charge, even when everyone else folded.
                if !self.showdown_seats.contains(&seat) {
                    self.showdown_seats.push(seat);
                }
                self.log.push(LogEntry::Bonus {
                    seat,
                    category,
                    amount,
                    payments: payments.clone(),
                });
                Some(Bonus {
                    seat,
                    category,
                    amount,
                    payments,
                })
            } else {
                None
            }
        } else {
            None
        };
        let seats: Vec<usize> = pot_winners.iter().map(|&i| self.players[i].seat).collect();
        self.log.push(LogEntry::Win {
            seats: seats.clone(),
            pot: self.pot - returned,
        });
        self.outcome = Some(Outcome {
            winners: seats,
            showdown,
            pot: self.pot,
            deltas,
            bonus,
        });
    }

    /// `winners` is already ordered clockwise from the dealer.
    fn split_award(&self, amount: u64, winners: &[usize], awards: &mut [u64]) {
        let share = amount / winners.len() as u64;
        for &i in winners {
            awards[i] += share;
        }
        awards[winners[0]] += amount % winners.len() as u64;
    }

    /// Keep original contributions and the gross pot for history and bonus
    /// exposure. Returns are payouts, not winnings or fresh bonus funds.
    fn all_in_awards(&mut self, best: &[usize], dealer_idx: usize) -> (Vec<u64>, Vec<usize>, u64) {
        let n = self.players.len();
        let mut levels: Vec<u64> = self.players.iter().map(|p| p.contributed).collect();
        levels.sort_unstable();
        levels.dedup();
        let mut awards = vec![0; n];
        let mut pot_winners = Vec::new();
        let mut returned = 0;
        let mut previous = 0;
        for level in levels {
            let contributors: Vec<usize> = (0..n)
                .filter(|&i| self.players[i].contributed >= level)
                .collect();
            let amount = (level - previous) * contributors.len() as u64;
            previous = level;
            if amount == 0 {
                continue;
            }
            if let [i] = contributors.as_slice() {
                if self.players[*i].active() {
                    awards[*i] += amount;
                    returned += amount;
                    self.log.push(LogEntry::UncalledReturn {
                        seat: self.players[*i].seat,
                        amount,
                    });
                    continue;
                }
            }
            let eligible: Vec<usize> = contributors
                .into_iter()
                .filter(|&i| self.players[i].active())
                .collect();
            let mut layer_winners = if eligible.is_empty() {
                // ZJH permits an off-turn fold after unequal payments. Dead
                // money with no eligible contributor stays with the best live
                // hand; folding never earns an uncalled-bet refund.
                best.to_vec()
            } else {
                let values: Vec<HandValue> =
                    eligible.iter().map(|&i| self.players[i].value).collect();
                rules::showdown_winners(&values)
                    .into_iter()
                    .map(|i| eligible[i])
                    .collect()
            };
            layer_winners.sort_by_key(|&i| (i + n - dealer_idx - 1) % n);
            self.split_award(amount, &layer_winners, &mut awards);
            self.log.push(LogEntry::PotAward {
                seats: layer_winners
                    .iter()
                    .map(|&i| self.players[i].seat)
                    .collect(),
                amount,
            });
            pot_winners.extend(layer_winners);
        }
        pot_winners.sort_unstable();
        pot_winners.dedup();
        pot_winners.sort_by_key(|&i| (i + n - dealer_idx - 1) % n);
        (awards, pot_winners, returned)
    }

    pub fn can_show_cards(&self, seat: usize) -> bool {
        self.outcome
            .as_ref()
            .is_some_and(|outcome| outcome.winners.contains(&seat))
            && !self.showdown_seats.contains(&seat)
    }

    pub fn show_cards(&mut self, seat: usize) -> bool {
        if !self.can_show_cards(seat) {
            return false;
        }
        self.showdown_seats.push(seat);
        self.log.push(LogEntry::ShowCards { seat });
        true
    }

    /// Cards `target` holds as seen by `viewer` (`None` = spectator). A seat
    /// sees its own cards once it looked, once it is out of the hand (folded or
    /// lost a compare) and after the hand; a blind compare winner stays blind.
    /// The compare loser sees a blind winner's cards only under that same
    /// condition: otherwise a second account (or a colluder) that lost on
    /// purpose could tell the winner its cards while it keeps the blind price.
    pub fn visible_cards(&self, viewer: Option<usize>, target: usize) -> Option<[Card; 3]> {
        let p = self.participant(target)?;
        let unblind = p.looked || p.folded || self.finished();
        let own = viewer == Some(target) && unblind;
        let compared = unblind && viewer.is_some_and(|v| self.reveals.contains(&(v, target)));
        let shown = self.showdown_seats.contains(&target);
        (own || compared || shown).then_some(p.cards)
    }
}
