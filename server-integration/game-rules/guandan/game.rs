use super::rules::{self, Card, Pattern, Play};
use rand::Rng;
use serde::{Deserialize, Serialize};

pub const MAX_DEALS: u8 = 24;
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Phase {
    Waiting,
    Tribute,
    Return,
    Playing,
    DealEnd,
    Finished,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Exchange {
    pub from: usize,
    pub to: usize,
    pub tribute: Card,
    pub returned: Option<Card>,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DealResult {
    pub deal: u8,
    pub order: Vec<usize>,
    pub team: usize,
    pub gain: u8,
    pub levels: [u8; 2],
    pub passed_ace: bool,
}
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PublicPlay {
    pub seat: usize,
    pub cards: Vec<Card>,
    pub pattern: Pattern,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Game {
    #[serde(default)]
    pub proofs: Vec<crate::card_proofs::Deal>,
    #[serde(default)]
    pub proof_public: Vec<Vec<usize>>,
    #[serde(default)]
    pub proof_start_pending: bool,
    #[serde(default)]
    pub pending_entropy: Option<crate::zjh::fairness::Pending>,
    pub id: uuid::Uuid,
    pub phase: Phase,
    pub deal: u8,
    pub levels: [u8; 2],
    pub level: u8,
    pub hands: [Vec<Card>; 4],
    pub turn: usize,
    pub target: Option<Play>,
    pub leader: usize,
    pub passed: [bool; 4],
    pub order: Vec<usize>,
    pub previous: Vec<usize>,
    pub tributes: [Option<Card>; 4],
    pub exchanges: Vec<Exchange>,
    pub anti: bool,
    pub results: Vec<DealResult>,
    pub winner: Option<usize>,
    pub played: Vec<PublicPlay>,
    /// Wall-clock start of the match; feeds the daily report's play time.
    pub started_at: Option<chrono::DateTime<chrono::Utc>>,
    /// Deal limit for this match ([`MAX_DEALS`]).
    pub max_deals: u8,
}
#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Serialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum Action {
    Play { cards: Vec<Card>, pattern: Pattern },
    Pass,
    Tribute { card: Card },
    Return { card: Card },
}
impl Default for Game {
    fn default() -> Self {
        Self {
            proofs: Vec::new(),
            proof_public: Vec::new(),
            proof_start_pending: false,
            pending_entropy: None,
            id: uuid::Uuid::new_v4(),
            phase: Phase::Waiting,
            deal: 0,
            levels: [2, 2],
            level: 2,
            hands: std::array::from_fn(|_| Vec::new()),
            turn: 0,
            target: None,
            leader: 0,
            passed: [false; 4],
            order: Vec::new(),
            previous: Vec::new(),
            tributes: [None; 4],
            exchanges: Vec::new(),
            anti: false,
            results: Vec::new(),
            winner: None,
            played: Vec::new(),
            started_at: None,
            max_deals: MAX_DEALS,
        }
    }
}
impl Game {
    pub fn deal(&mut self, rng: &mut impl Rng, roster: Vec<(u8, Option<uuid::Uuid>)>) -> bool {
        let owners = roster.iter().map(|(s, u)| (*s as usize, *u)).collect();
        if self
            .pending_entropy
            .as_ref()
            .is_some_and(|p| p.owners != owners)
        {
            self.pending_entropy = None;
        }
        if self.pending_entropy.is_none() {
            self.proof_start_pending = true;
            let Some(proof) =
                crate::card_proofs::take(mental_poker::card_proof::Game::Guandan, roster.clone())
            else {
                return false;
            };
            self.pending_entropy = Some(crate::zjh::fairness::Pending::for_cards(
                proof,
                owners,
                roster.iter().filter_map(|(s, u)| u.map(|_| *s)).collect(),
            ));
            return false; // Persist and publish the lock before accepting seeds.
        }
        let pending = self.pending_entropy.as_mut().unwrap();
        pending.advance();
        let Some(proof) = pending.finish_cards() else {
            return false;
        };
        self.pending_entropy = None;
        self.proof_start_pending = false;
        let deck: Vec<Card> = proof.cards.iter().map(|&c| c as u8).collect();
        self.proofs.push(proof);
        self.proof_public.push(Vec::new());
        self.hands = std::array::from_fn(|s| deck.iter().skip(s).step_by(4).copied().collect());
        // A uniformly selected non-joker/non-heart-level marker determines first lead.
        self.turn = if self.previous.is_empty() {
            let markers: Vec<_> = deck
                .iter()
                .enumerate()
                .filter(|(_, c)| rules::rank(**c) < 15 && !rules::wild(**c, 2))
                .collect();
            markers[rng.gen_range(0..markers.len())].0 % 4
        } else {
            self.previous[0]
        };
        self.deal += 1;
        self.target = None;
        self.passed = [false; 4];
        self.order.clear();
        self.tributes = [None; 4];
        self.exchanges.clear();
        self.anti = false;
        self.phase = Phase::Playing;
        self.played.clear();
        if !self.previous.is_empty() {
            let donors = self.donors();
            self.anti = donors
                .iter()
                .flat_map(|&s| &self.hands[s])
                .filter(|&&c| rules::rank(c) == 16)
                .count()
                == 2;
            if !self.anti {
                self.phase = Phase::Tribute;
            }
        }
        true
    }
    fn donors(&self) -> Vec<usize> {
        if self.previous.is_empty() {
            return vec![];
        }
        if self.previous[0] % 2 == self.previous[1] % 2 {
            vec![self.previous[2], self.previous[3]]
        } else {
            vec![self.previous[3]]
        }
    }
    pub fn choices(&self, seat: usize) -> Vec<Card> {
        if seat >= 4 {
            return vec![];
        }
        let h = &self.hands[seat];
        if self.phase == Phase::Tribute
            && self.donors().contains(&seat)
            && self.tributes[seat].is_none()
        {
            let max = h
                .iter()
                .filter(|&&c| !rules::wild(c, self.level))
                .map(|&c| rules::power(rules::rank(c), self.level))
                .max();
            return h
                .iter()
                .copied()
                .filter(|&c| {
                    !rules::wild(c, self.level)
                        && Some(rules::power(rules::rank(c), self.level)) == max
                })
                .collect();
        }
        if self.phase == Phase::Return
            && self
                .exchanges
                .iter()
                .any(|e| e.to == seat && e.returned.is_none())
        {
            let low: Vec<_> = h
                .iter()
                .copied()
                .filter(|&c| rules::power(rules::rank(c), self.level) <= 10)
                .collect();
            if !low.is_empty() {
                return low;
            }
            let min = h
                .iter()
                .map(|&c| rules::power(rules::rank(c), self.level))
                .min();
            return h
                .iter()
                .copied()
                .filter(|&c| Some(rules::power(rules::rank(c), self.level)) == min)
                .collect();
        }
        vec![]
    }
    pub fn apply(&mut self, seat: usize, action: Action) -> Result<(), &'static str> {
        if seat >= 4 {
            return Err("not_seated");
        }
        match action {
            Action::Tribute { card } => {
                if self.phase != Phase::Tribute || !self.choices(seat).contains(&card) {
                    return Err("invalid_tribute");
                }
                self.tributes[seat] = Some(card);
                let donors = self.donors();
                if donors.iter().all(|&s| self.tributes[s].is_some()) {
                    let first = self.previous[0];
                    let mut sorted = donors;
                    sorted.sort_by_key(|&s| {
                        (
                            std::cmp::Reverse(rules::power(
                                rules::rank(self.tributes[s].unwrap()),
                                self.level,
                            )),
                            s != (first + 1) % 4,
                        )
                    });
                    self.turn = sorted[0];
                    self.exchanges = sorted
                        .iter()
                        .enumerate()
                        .map(|(i, &s)| Exchange {
                            from: s,
                            to: if i == 0 { first } else { (first + 2) % 4 },
                            tribute: self.tributes[s].unwrap(),
                            returned: None,
                        })
                        .collect();
                    // Transfer simultaneously; chosen tribute identities remain public from this point.
                    for e in &self.exchanges {
                        self.hands[e.from].retain(|&c| c != e.tribute);
                        self.hands[e.to].push(e.tribute);
                    }
                    self.phase = Phase::Return;
                }
            }
            Action::Return { card } => {
                if self.phase != Phase::Return || !self.choices(seat).contains(&card) {
                    return Err("invalid_return");
                }
                self.exchanges
                    .iter_mut()
                    .find(|e| e.to == seat)
                    .ok_or("invalid_return")?
                    .returned = Some(card);
                if self.exchanges.iter().all(|e| e.returned.is_some()) {
                    for e in &self.exchanges {
                        let c = e.returned.unwrap();
                        self.hands[e.to].retain(|&x| x != c);
                        self.hands[e.from].push(c);
                    }
                    self.phase = Phase::Playing;
                }
            }
            Action::Play { cards, pattern } => {
                if self.phase != Phase::Playing || seat != self.turn {
                    return Err("not_your_turn");
                }
                if !cards.iter().all(|c| self.hands[seat].contains(c))
                    || !rules::valid(&cards, pattern, self.level)
                {
                    return Err("invalid_play");
                }
                if self
                    .target
                    .as_ref()
                    .is_some_and(|t| !pattern.beats(t.pattern, self.level))
                {
                    return Err("must_beat");
                }
                self.hands[seat].retain(|c| !cards.contains(c));
                let cards = rules::display_order(&cards, pattern, self.level);
                self.played.push(PublicPlay {
                    seat,
                    cards: cards.clone(),
                    pattern,
                });
                self.target = Some(Play { cards, pattern });
                self.leader = seat;
                self.passed = [false; 4];
                if self.hands[seat].is_empty() {
                    self.order.push(seat);
                }
                if self.order.len() == 3
                    || (self.order.len() == 2 && self.order[0] % 2 == self.order[1] % 2)
                {
                    self.finish_deal();
                } else {
                    self.advance(seat);
                }
            }
            Action::Pass => {
                if self.phase != Phase::Playing || seat != self.turn {
                    return Err("not_your_turn");
                }
                if self.target.is_none() {
                    return Err("must_lead");
                }
                self.passed[seat] = true;
                self.advance(seat);
            }
        }
        if let (Some(proof), Some(public)) = (self.proofs.last(), self.proof_public.last_mut()) {
            let visible = self
                .played
                .iter()
                .flat_map(|p| p.cards.iter().copied())
                .chain(self.exchanges.iter().flat_map(|e| {
                    std::iter::once(e.tribute)
                        .chain(e.returned.filter(|_| self.phase != Phase::Return))
                }));
            for card in visible {
                if let Some(position) = proof.cards.iter().position(|id| *id == u16::from(card)) {
                    public.push(position);
                }
            }
            public.sort_unstable();
            public.dedup();
        }
        Ok(())
    }
    fn advance(&mut self, seat: usize) {
        let everyone_passed = (0..4)
            .filter(|&s| s != self.leader && !self.hands[s].is_empty())
            .all(|s| self.passed[s]);
        if everyone_passed {
            self.turn = if self.hands[self.leader].is_empty() {
                (self.leader + 2) % 4
            } else {
                self.leader
            };
            self.target = None;
            self.passed = [false; 4];
        } else {
            self.turn = (1..=4)
                .map(|n| (seat + n) % 4)
                .find(|&s| !self.hands[s].is_empty())
                .unwrap_or(seat);
        }
    }
    fn finish_deal(&mut self) {
        for s in 0..4 {
            if !self.order.contains(&s) {
                self.order.push(s);
            }
        }
        let team = self.order[0] % 2;
        let partner = self
            .order
            .iter()
            .position(|&s| s == (self.order[0] + 2) % 4)
            .unwrap();
        let gain = 4 - partner as u8;
        let passed_ace = self.level == 14 && self.levels[team] == 14 && partner < 3;
        self.levels[team] = (self.levels[team] + gain).min(14);
        self.level = self.levels[team];
        self.results.push(DealResult {
            deal: self.deal,
            order: self.order.clone(),
            team,
            gain,
            levels: self.levels,
            passed_ace,
        });
        self.previous = self.order.clone();
        if passed_ace || self.deal >= self.max_deals {
            self.phase = Phase::Finished;
            self.winner = if passed_ace {
                Some(team)
            } else {
                match self.levels[0].cmp(&self.levels[1]) {
                    std::cmp::Ordering::Greater => Some(0),
                    std::cmp::Ordering::Less => Some(1),
                    _ => None,
                }
            };
        } else {
            self.phase = Phase::DealEnd;
        }
    }
    pub fn bot_action(&self, seat: usize) -> Option<Action> {
        let choices = self.choices(seat);
        if !choices.is_empty() {
            let card = if self.phase == Phase::Return {
                super::bot::choose_return(&self.hands[seat], &choices, self.level)?
            } else {
                choices[0]
            };
            return Some(if self.phase == Phase::Tribute {
                Action::Tribute { card }
            } else {
                Action::Return { card }
            });
        }
        if self.phase != Phase::Playing || self.turn != seat {
            return None;
        }
        let legal = rules::moves(
            &self.hands[seat],
            self.level,
            self.target.as_ref().map(|t| t.pattern),
        );
        Some(super::bot::choose(super::bot::Observation {
            hand: &self.hands[seat],
            legal: &legal,
            seat,
            level: self.level,
            target: self.target.as_ref(),
            leader: self.leader,
            passed: self.passed,
            remaining: std::array::from_fn(|i| self.hands[i].len()),
            played: &self.played,
            order: &self.order,
        }))
    }
}
