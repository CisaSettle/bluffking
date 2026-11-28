use super::rules::{self, Kind, Pattern, Turn, FIRST_CARD};
use crate::ddz::rules::Card;
use rand::seq::SliceRandom;
use serde::{Deserialize, Serialize};
use uuid::Uuid;

pub const SEATS: usize = 3;
const HAND_SIZE: usize = 16;

#[derive(Deserialize)]
#[serde(tag = "t", rename_all = "snake_case")]
pub enum Action {
    Play { cards: Vec<Card> },
    Pass,
    Resume,
}

#[derive(Clone, Serialize, Deserialize)]
pub struct Play {
    pub seat: usize,
    pub cards: Vec<Card>,
    pub pattern: Pattern,
}

#[derive(Clone, Copy, Serialize, Deserialize)]
pub struct RoundResult {
    pub winner: usize,
    pub remaining: [usize; SEATS],
    pub bombs: [i32; SEATS],
    pub delta: [i32; SEATS],
}

#[derive(Clone, Serialize, Deserialize)]
pub struct Match {
    pub id: Uuid,
    pub revision: u64,
    pub limit: usize,
    pub results: Vec<RoundResult>,
    pub turn: usize,
    pub hands: [Vec<Card>; SEATS],
    pub last: Option<Play>,
    pub passed: Option<usize>,
    pub opening: bool,
    pub bombs: [i32; SEATS],
    pub auto: [bool; SEATS],
    pub between_rounds: bool,
    pub abandoned: bool,
    /// The private hand that changed in the current ordinary action.
    changed_hand: Option<usize>,
    /// The prior actor's hint must be cleared even if they passed.
    previous_turn: Option<usize>,
}

impl Match {
    pub fn new(limit: usize) -> Self {
        let mut m = Self {
            id: Uuid::new_v4(),
            revision: 0,
            limit,
            results: Vec::new(),
            turn: 0,
            hands: Default::default(),
            last: None,
            passed: None,
            opening: true,
            bombs: [0; SEATS],
            auto: [false; SEATS],
            between_rounds: false,
            abandoned: false,
            changed_hand: None,
            previous_turn: None,
        };
        m.deal();
        m
    }

    pub fn deal(&mut self) {
        let mut deck: Vec<_> = rules::deck().collect();
        deck.shuffle(&mut rand::rngs::OsRng);
        self.hands = std::array::from_fn(|seat| {
            let mut hand = deck[seat * HAND_SIZE..(seat + 1) * HAND_SIZE].to_vec();
            hand.sort_unstable();
            hand
        });
        self.opening = self.results.is_empty();
        self.turn = self.results.last().map_or_else(
            || {
                self.hands
                    .iter()
                    .position(|hand| hand.contains(&FIRST_CARD))
                    .unwrap()
            },
            |result| result.winner,
        );
        self.last = None;
        self.passed = None;
        self.bombs = [0; SEATS];
        self.between_rounds = false;
        self.changed_hand = None;
        self.previous_turn = None;
        self.revision += 1;
    }

    pub fn finished(&self) -> bool {
        self.abandoned || self.results.len() == self.limit
    }

    pub fn counts(&self) -> [usize; SEATS] {
        self.hands.each_ref().map(Vec::len)
    }

    pub fn totals(&self) -> [i32; SEATS] {
        std::array::from_fn(|seat| self.results.iter().map(|result| result.delta[seat]).sum())
    }

    pub fn winners(&self) -> Vec<usize> {
        if self.abandoned || !self.finished() {
            return Vec::new();
        }
        let totals = self.totals();
        let high = totals.iter().max().unwrap();
        (0..SEATS).filter(|&seat| totals[seat] == *high).collect()
    }

    fn decision(&self) -> Turn<'_> {
        Turn {
            hand: &self.hands[self.turn],
            target: self.last.as_ref().map(|play| play.pattern),
            opening: self.opening,
            next_single: self.hands[next(self.turn)].len() == 1,
        }
    }

    pub fn hint(&self, seat: usize) -> Option<Vec<Card>> {
        (!self.finished() && !self.between_rounds && self.turn == seat && !self.auto[seat])
            .then(|| self.decision().suggestion())
            .flatten()
    }

    pub fn hand_changed(&self, seat: usize) -> bool {
        self.changed_hand == Some(seat)
    }

    pub fn hint_changed(&self, seat: usize) -> bool {
        self.turn == seat || self.previous_turn == Some(seat)
    }

    pub fn apply(&mut self, seat: usize, action: Action) -> Result<(), &'static str> {
        if self.finished() {
            return Err("match_over");
        }
        if matches!(action, Action::Resume)
            && self.auto[seat]
            && (self.between_rounds || self.turn == seat)
        {
            self.auto[seat] = false;
            self.changed_hand = None;
            self.previous_turn = None;
            self.revision += 1;
            return Ok(());
        }
        if self.between_rounds {
            return Err("round_over");
        }
        if seat != self.turn {
            return Err("not_your_turn");
        }
        if self.auto[seat] {
            return Err("auto_play");
        }
        self.act(action)
    }

    pub fn automatic(&mut self, timed_out: bool) {
        if self.finished() {
            return;
        }
        if self.between_rounds {
            self.deal();
            return;
        }
        let seat = self.turn;
        let action = self
            .decision()
            .suggestion()
            .map_or(Action::Pass, |cards| Action::Play { cards });
        if self.act(action).is_ok() && timed_out {
            self.auto[seat] = true;
        }
    }

    fn act(&mut self, action: Action) -> Result<(), &'static str> {
        let seat = self.turn;
        let changed_hand = match action {
            Action::Play { mut cards } => {
                let pattern = self.decision().check(&cards)?;
                cards.sort_unstable();
                self.hands[seat].retain(|card| cards.binary_search(card).is_err());
                self.opening = false;
                self.passed = None;
                self.last = Some(Play {
                    seat,
                    cards,
                    pattern,
                });
                if pattern.kind == Kind::Bomb {
                    for (i, score) in self.bombs.iter_mut().enumerate() {
                        *score += if i == seat { 10 } else { -5 };
                    }
                }
                if self.hands[seat].is_empty() {
                    self.finish_round(seat);
                }
                Some(seat)
            }
            Action::Pass => {
                if self.last.is_none() || self.decision().suggestion().is_some() {
                    return Err("must_play");
                }
                if self.passed.is_some() {
                    self.last = None;
                }
                self.passed = Some(seat);
                None
            }
            Action::Resume => return Err("not_available"),
        };
        self.changed_hand = changed_hand;
        self.previous_turn = Some(seat);
        self.revision += 1;
        if !self.between_rounds && !self.finished() {
            self.turn = next(seat);
        }
        Ok(())
    }

    fn finish_round(&mut self, winner: usize) {
        let remaining = self.counts();
        let penalties = remaining.map(|left| {
            if left == HAND_SIZE {
                -32
            } else {
                -(left as i32)
            }
        });
        let mut delta = std::array::from_fn(|seat| self.bombs[seat] + penalties[seat]);
        delta[winner] -= penalties.iter().sum::<i32>();
        self.results.push(RoundResult {
            winner,
            remaining,
            bombs: self.bombs,
            delta,
        });
        self.between_rounds = self.results.len() < self.limit;
    }

    pub fn valid(&self, limit: usize) -> bool {
        if self.limit != limit
            || self.turn >= SEATS
            || self.results.len() > limit
            || self.results.iter().any(|row| row.winner >= SEATS)
            || self.last.as_ref().is_some_and(|play| {
                play.seat >= SEATS || play.cards.iter().any(|&c| !rules::in_deck(c))
            })
            || self.passed.is_some_and(|seat| seat >= SEATS)
            || self.between_rounds && (self.results.is_empty() || self.finished())
        {
            return false;
        }
        let mut seen = 0u64;
        for hand in &self.hands {
            if hand.len() > HAND_SIZE || hand.windows(2).any(|pair| pair[0] >= pair[1]) {
                return false;
            }
            for &card in hand {
                if !rules::in_deck(card) || seen & (1 << card.0) != 0 {
                    return false;
                }
                seen |= 1 << card.0;
            }
        }
        if self.opening {
            return self.results.is_empty()
                && self.hands[self.turn].contains(&FIRST_CARD)
                && self.counts() == [HAND_SIZE; SEATS]
                && self.last.is_none();
        }
        self.finished() || self.between_rounds || self.hands.iter().all(|hand| !hand.is_empty())
    }
}

fn next(seat: usize) -> usize {
    (seat + 1) % SEATS
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn timeout_match_progresses_and_preserves_zero_sum_scores() {
        for limit in [4, 8] {
            let mut m = Match::new(limit);
            assert_eq!(m.counts(), [16; SEATS]);
            assert!(m.hands[m.turn].contains(&FIRST_CARD));
            assert_eq!(m.apply(m.turn, Action::Pass), Err("must_play"));
            let timed_out = m.turn;
            m.automatic(true);
            assert!(m.auto[timed_out]);
            assert!(m.last.as_ref().unwrap().cards.contains(&FIRST_CARD));
            for _ in 0..1000 {
                if m.finished() {
                    break;
                }
                let revision = m.revision;
                if m.between_rounds {
                    let winner = m.results.last().unwrap().winner;
                    assert!(m.results.last().unwrap().delta.iter().sum::<i32>() == 0);
                    if m.auto[timed_out] {
                        m.apply(timed_out, Action::Resume).unwrap();
                        assert!(!m.auto[timed_out]);
                    }
                    m.automatic(false);
                    assert_eq!(m.turn, winner);
                    assert!(!m.opening);
                    assert_eq!(m.counts(), [16; SEATS]);
                } else {
                    m.automatic(false);
                }
                assert!(m.revision > revision, "stalled at revision {revision}");
                assert!(m.valid(limit));
            }
            assert_eq!(m.results.len(), limit);
            let totals = m.totals();
            assert_eq!(totals.iter().sum::<i32>(), 0);
            assert_eq!(
                m.winners(),
                (0..SEATS)
                    .filter(|&i| totals[i] == *totals.iter().max().unwrap())
                    .collect::<Vec<_>>()
            );
        }
    }

    #[test]
    fn bomb_rewards_each_time_and_an_unplayed_hand_pays_double() {
        let mut m = Match::new(4);
        let bomb = vec![Card(4), Card(5), Card(6), Card(7)];
        m.opening = false;
        m.turn = 0;
        m.hands[0] = bomb.clone();
        m.hands[1] = vec![Card(0)];
        m.hands[2] = rules::deck().filter(|c| c.0 >= 8).take(16).collect();
        m.apply(0, Action::Play { cards: bomb }).unwrap();
        assert_eq!(m.results[0].bombs, [10, -5, -5]);
        assert_eq!(m.results[0].remaining, [0, 1, 16]);
        assert_eq!(m.results[0].delta, [43, -6, -37]);
        assert_eq!(m.totals(), [43, -6, -37]);
    }
}
