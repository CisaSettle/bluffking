//! One 炸金花 hand as a pure state machine (no IO, no clocks, no RNG).
//!
//! Betting unit: a blind (闷牌) player pays the current level, a player who
//! has looked pays the room's look multiplier times the level. Raising lifts the
//! level in ante increments. Each payment is capped at the room's bet limit.
//! Compare (比牌) is allowed from round 2 and costs
//! `compare_mult` call units (room setting, default two), and a tie loses for the
//! initiator. After `max_rounds` full rounds all remaining players are forced to
//! show down. An optional 赖子 rank makes every card of that rank wild, and
//! an optional 大小王 setting adds two wild jokers to the deck
//! ([`rules::evaluate_with`]); a hand made with a 赖子 collects the 喜钱 bonus
//! only when the room enables `wild_bonus`.
//!
//! 五选三 (room setting): each seat is dealt five cards and keeps three. A
//! player picks after looking, before any further bet, and the two discards
//! are then public. A seat that has not picked plays the best three of its
//! five: a compare or the end of the hand fixes that choice, and a blind
//! seat's discards stay hidden until it looks or its cards are shown.
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
pub const DEFAULT_LOOK_MULT: u64 = 2;
pub const MAX_LOOK_MULT: u64 = 3;
pub const DEFAULT_BET_CAP_ANTES: u64 = 100;
pub const MAX_BET_CAP_ANTES: u64 = 1000;
pub const DEFAULT_BUY_IN_CAP_ANTES: u64 = 200;
pub const MAX_BUY_IN_CAP_ANTES: u64 = 1000;
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
    #[serde(default = "default_look_mult")]
    pub look_mult: u64,
    #[serde(default = "default_bet_cap_antes")]
    pub bet_cap_antes: u64,
    /// 五选三: five cards per seat, keep three, discards public. Absent
    /// (false) in an ordinary room.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub five_pick: bool,
}

fn default_compare_mult() -> u64 {
    DEFAULT_COMPARE_MULT
}

fn default_look_mult() -> u64 {
    DEFAULT_LOOK_MULT
}

fn default_bet_cap_antes() -> u64 {
    DEFAULT_BET_CAP_ANTES
}

impl HandConfig {
    pub fn wild(&self) -> rules::Wild {
        rules::Wild {
            rank: self.wild_rank,
            jokers: self.jokers,
        }
    }
    pub fn max_level(&self) -> u64 {
        self.ante * self.bet_cap_antes
    }
    pub fn bet_multiplier(&self, looked: bool) -> u64 {
        if looked {
            self.look_mult
        } else {
            1
        }
    }
    /// Cards dealt to each seat.
    pub fn hand_size(&self) -> usize {
        if self.five_pick {
            FIVE_PICK_DEALT
        } else {
            3
        }
    }
}

/// 五选三 deals this many cards to each seat.
pub const FIVE_PICK_DEALT: usize = 5;

pub fn min_stack(cfg: &HandConfig) -> u64 {
    cfg.ante + (cfg.ante * cfg.bet_multiplier(true)).min(cfg.max_level())
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct Participant {
    pub seat: usize,
    pub stack_start: u64,
    /// The three cards the seat plays. 五选三: the best three of `dealt`
    /// until `keep` fixes them.
    pub cards: [Card; 3],
    /// 五选三: all five dealt cards in deal order; empty in a three-card room.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub dealt: Vec<Card>,
    /// 五选三: the `dealt` slots of `cards` once fixed.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub keep: Option<[u8; 3]>,
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
    /// The three cards are final: always in a three-card room.
    pub fn picked(&self) -> bool {
        self.dealt.is_empty() || self.keep.is_some()
    }
    /// 五选三: the slots and cards set aside once the pick is fixed.
    pub fn discards(&self) -> Option<([u8; 2], [Card; 2])> {
        let keep = self.keep?;
        let mut slots = (0..self.dealt.len() as u8).filter(|s| !keep.contains(s));
        let slots = [slots.next()?, slots.next()?];
        Some((slots, slots.map(|s| self.dealt[usize::from(s)])))
    }
    fn best_keep(&self, wild: rules::Wild) -> ([u8; 3], HandValue) {
        rules::best_three(&self.dealt, wild)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Action {
    Look,
    /// 五选三: keep these three `dealt` slots.
    Pick { keep: [u8; 3] },
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
    /// 五选三: the looked seat must pick its three cards before betting.
    MustPick,
    InvalidPick,
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
    /// 五选三: `seat`'s two discards, public from here on.
    Discard {
        seat: usize,
        slots: [u8; 2],
        cards: [String; 2],
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
    /// 五选三: looked and still choosing; betting waits for the pick.
    pub can_pick: bool,
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

enum AllInLayer {
    Return { idx: usize, amount: u64 },
    Award { eligible: Vec<usize>, amount: u64 },
}

impl Hand {
    /// `seats` = `(seat, stack)` for every dealt-in player, any order.
    /// `deck` must hold at least `cfg.hand_size() * seats.len()` cards.
    pub fn new(
        cfg: HandConfig,
        seats: &[(usize, u64)],
        dealer_seat: usize,
        deck: &[Card],
    ) -> Result<Self, ActionError> {
        let mut seats = seats.to_vec();
        seats.sort_by_key(|s| s.0);
        let size = cfg.hand_size();
        if seats.len() < 2 || deck.len() < seats.len() * size {
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
                let dealt = &deck[i * size..(i + 1) * size];
                let mut p = Participant {
                    seat,
                    stack_start: stack,
                    cards: [dealt[0], dealt[1], dealt[2]],
                    dealt: if cfg.five_pick {
                        dealt.to_vec()
                    } else {
                        Vec::new()
                    },
                    keep: None,
                    value: rules::evaluate_with(&[dealt[0], dealt[1], dealt[2]], cfg.wild()),
                    looked: false,
                    folded: false,
                    lost_compare: false,
                    contributed: cfg.ante,
                    last_acted_round: 0,
                };
                if cfg.five_pick {
                    let (keep, value) = p.best_keep(cfg.wild());
                    p.cards = keep.map(|s| p.dealt[usize::from(s)]);
                    p.value = value;
                }
                p
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
        let mult = self.cfg.bet_multiplier(self.players[idx].looked);
        (self.level * mult).min(self.cfg.max_level())
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
            can_pick: p.looked && !p.picked(),
            can_fold: true,
            ..Legal::default()
        };
        if self.to_act != idx {
            return legal;
        }
        legal.your_turn = true;
        if legal.can_pick {
            return legal;
        }
        let unit = self.unit(idx);
        let mult = self.cfg.bet_multiplier(p.looked);
        let available = p.stack_start - p.contributed;
        legal.call_cost = (unit <= available).then_some(unit);
        legal.all_in_cost = (unit >= available).then_some(available);
        let next = self.level / self.cfg.ante + 1;
        let max = available.min(self.cfg.max_level()) / mult / self.cfg.ante;
        legal.raises = (next..=max)
            .map(|m| m * self.cfg.ante)
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
                // A pick fixed while blind (a won compare) is public now.
                self.log_discards(idx);
                return Ok(());
            }
            Action::Pick { keep } => {
                let p = &self.players[idx];
                let n = p.dealt.len() as u8;
                let distinct = keep[0] != keep[1] && keep[0] != keep[2] && keep[1] != keep[2];
                if !p.looked || p.picked() || !distinct || keep.iter().any(|&s| s >= n) {
                    return Err(ActionError::InvalidPick);
                }
                self.set_keep(idx, keep);
                self.log_discards(idx);
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
        if self.players[idx].looked && !self.players[idx].picked() {
            return Err(ActionError::MustPick);
        }
        match action {
            Action::Call => {
                let amount = self.unit(idx);
                self.pay(idx, amount)?;
                self.log.push(LogEntry::Call { seat, amount });
            }
            Action::Raise { level } => {
                let mult = self.cfg.bet_multiplier(self.players[idx].looked);
                let valid = level > self.level
                    && level <= self.cfg.max_level() / mult
                    && level.is_multiple_of(self.cfg.ante);
                if !valid {
                    return Err(ActionError::InvalidRaise);
                }
                let amount = level * mult;
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
                self.fix_pick(idx);
                self.fix_pick(t);
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
            Action::Look | Action::Pick { .. } | Action::Fold => unreachable!(),
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

    fn set_keep(&mut self, idx: usize, keep: [u8; 3]) {
        let wild = self.cfg.wild();
        let p = &mut self.players[idx];
        p.cards = keep.map(|s| p.dealt[usize::from(s)]);
        p.value = rules::evaluate_with(&p.cards, wild);
        p.keep = Some(keep);
    }

    /// 五选三: an undecided seat plays its best three from here on. A looked
    /// seat's discards become public; a blind seat's wait for its look.
    fn fix_pick(&mut self, idx: usize) {
        if self.players[idx].picked() {
            return;
        }
        let (keep, _) = self.players[idx].best_keep(self.cfg.wild());
        self.set_keep(idx, keep);
        self.log_discards(idx);
    }

    fn log_discards(&mut self, idx: usize) {
        let p = &self.players[idx];
        if let (true, Some((slots, cards))) = (p.looked, p.discards()) {
            self.log.push(LogEntry::Discard {
                seat: p.seat,
                slots,
                cards: cards.map(Card::code),
            });
        }
    }

    /// The live seats' cards decide the hand: fix every open pick.
    fn fix_live_picks(&mut self) {
        for idx in 0..self.players.len() {
            if self.players[idx].active() {
                self.fix_pick(idx);
            }
        }
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

    /// Nobody else is left, so nothing is compared: an undecided winner's
    /// cards stay unshown unless the bonus (or a voluntary show) opens them.
    fn finish_last_standing(&mut self) {
        let winner = self
            .players
            .iter()
            .position(|p| p.active())
            .expect("one active player");
        self.settle(vec![winner], false);
    }

    fn finish_showdown(&mut self) {
        self.fix_live_picks();
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
            self.all_in_awards(dealer_idx)
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
                self.fix_pick(*winner);
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

    /// Price the public pot layers if this seat spends its remaining chips
    /// and ends betting. The bot supplies its equity against each eligible
    /// opponent from public action reads and its own cards.
    pub(super) fn all_in_expected_payout(
        &self,
        seat: usize,
        equity_against: impl Fn(usize) -> f64,
    ) -> f64 {
        let idx = self.idx(seat).expect("acting seat");
        self.all_in_layers(Some(idx))
            .into_iter()
            .map(|layer| match layer {
                AllInLayer::Return { idx: owner, amount } if owner == idx => amount as f64,
                AllInLayer::Award { eligible, amount } if eligible.contains(&idx) => {
                    let probability: f64 = eligible
                        .into_iter()
                        .filter(|&i| i != idx)
                        .map(|i| equity_against(self.players[i].seat))
                        .product();
                    probability * amount as f64
                }
                _ => 0.0,
            })
            .sum()
    }

    fn all_in_layers(&self, all_in_seat: Option<usize>) -> Vec<AllInLayer> {
        let contributions: Vec<u64> = self
            .players
            .iter()
            .enumerate()
            .map(|(i, p)| {
                if all_in_seat == Some(i) {
                    p.stack_start
                } else {
                    p.contributed
                }
            })
            .collect();
        let mut levels = contributions.clone();
        levels.sort_unstable();
        levels.dedup();
        let mut layers = Vec::with_capacity(levels.len());
        let mut previous = 0;
        for level in levels {
            let contributors: Vec<usize> = contributions
                .iter()
                .enumerate()
                .filter_map(|(i, &contributed)| (contributed >= level).then_some(i))
                .collect();
            let amount = (level - previous) * contributors.len() as u64;
            previous = level;
            if amount == 0 {
                continue;
            }
            if let [i] = contributors.as_slice() {
                if self.players[*i].active() {
                    layers.push(AllInLayer::Return { idx: *i, amount });
                    continue;
                }
            }
            let mut eligible: Vec<usize> = contributors
                .into_iter()
                .filter(|&i| self.players[i].active())
                .collect();
            if eligible.is_empty() {
                // ZJH permits an off-turn fold after unequal payments. Dead
                // money with no eligible contributor stays with the best live
                // hand; folding never earns an uncalled-bet refund.
                eligible.extend(
                    self.players
                        .iter()
                        .enumerate()
                        .filter_map(|(i, p)| p.active().then_some(i)),
                );
            }
            layers.push(AllInLayer::Award { eligible, amount });
        }
        layers
    }

    /// Keep original contributions and the gross pot for history and bonus
    /// exposure. Returns are payouts, not winnings or fresh bonus funds.
    fn all_in_awards(&mut self, dealer_idx: usize) -> (Vec<u64>, Vec<usize>, u64) {
        let n = self.players.len();
        let mut awards = vec![0; n];
        let mut pot_winners = Vec::new();
        let mut returned = 0;
        for layer in self.all_in_layers(None) {
            let (eligible, amount) = match layer {
                AllInLayer::Return { idx, amount } => {
                    awards[idx] += amount;
                    returned += amount;
                    self.log.push(LogEntry::UncalledReturn {
                        seat: self.players[idx].seat,
                        amount,
                    });
                    continue;
                }
                AllInLayer::Award { eligible, amount } => (eligible, amount),
            };
            let values: Vec<HandValue> = eligible.iter().map(|&i| self.players[i].value).collect();
            let mut layer_winners: Vec<usize> = rules::showdown_winners(&values)
                .into_iter()
                .map(|i| eligible[i])
                .collect();
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
        if let Ok(idx) = self.idx(seat) {
            self.fix_pick(idx);
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

    /// Every card dealt to `target` (five in 五选三) under the same rule as
    /// [`Self::visible_cards`].
    pub fn visible_dealt(&self, viewer: Option<usize>, target: usize) -> Option<Vec<Card>> {
        let cards = self.visible_cards(viewer, target)?;
        let p = self.participant(target)?;
        Some(if p.dealt.is_empty() {
            cards.to_vec()
        } else {
            p.dealt.clone()
        })
    }

    /// 五选三: discards everyone may see (the seat looked and its pick is fixed).
    pub fn public_discards(&self, target: usize) -> Option<([u8; 2], [Card; 2])> {
        self.participant(target)
            .filter(|p| p.looked)
            .and_then(Participant::discards)
    }

    /// 五选三: the slots `target`'s three cards occupy in [`Self::visible_dealt`]
    /// (fixed, or the best three while undecided) and whether they are final.
    pub fn keep_slots(&self, target: usize) -> Option<([u8; 3], bool)> {
        let p = self.participant(target).filter(|p| !p.dealt.is_empty())?;
        Some(match p.keep {
            Some(keep) => (keep, true),
            None => (p.best_keep(self.cfg.wild()).0, false),
        })
    }
}
