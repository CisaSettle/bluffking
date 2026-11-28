//! Authoritative bidding, trick rotation and zero-sum capped settlement.
use super::rules::{self, Card, Kind, Pattern};
use serde::Serialize;
pub const MAX_MULTIPLIER: u64 = 16;
#[derive(Debug, Clone, Copy, serde::Serialize, serde::Deserialize)]
pub struct HandConfig {
    pub ante: u64,
}
pub fn min_stack(cfg: &HandConfig) -> u64 {
    cfg.ante
}
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct Participant {
    pub seat: usize,
    pub cards: Vec<Card>,
    pub initial: Vec<Card>,
    pub played: u32,
}
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Action {
    Bid { score: u8 },
    Play { cards: Vec<Card> },
    Pass,
}
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ActionError {
    NotInHand,
    HandFinished,
    NotYourTurn,
    InvalidBid,
    /// Cards not in the player's hand (or an unparsable/duplicate set).
    InvalidCards,
    /// The cards form no legal combination (牌型不成立).
    InvalidPattern,
    /// A legal combination that does not beat the play on the table (压不过).
    CannotBeat,
    CannotPass,
    InsufficientChips,
}
#[derive(Debug, Clone, Serialize, serde::Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum LogEntry {
    Bid {
        seat: usize,
        score: u8,
    },
    Play {
        seat: usize,
        cards: Vec<String>,
        pattern: Pattern,
    },
    Pass {
        seat: usize,
    },
}
impl LogEntry {
    pub fn seat(&self) -> usize {
        match self {
            LogEntry::Bid { seat, .. } | LogEntry::Play { seat, .. } | LogEntry::Pass { seat } => {
                *seat
            }
        }
    }
}
#[derive(Debug, Clone, Serialize, serde::Deserialize)]
pub struct Outcome {
    pub winners: Vec<usize>,
    pub pot: u64,
    pub showdown: bool,
    pub spring: bool,
    pub deltas: Vec<(usize, i64)>,
}
#[derive(Debug, Clone, Default, Serialize)]
pub struct Legal {
    pub your_turn: bool,
    pub bids: Vec<u8>,
    pub can_pass: bool,
    pub hints: Vec<Vec<String>>,
}
/// Persisted actor-authored feedback, never inferred from public card movement.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct TimeoutAction {
    pub user: uuid::Uuid,
    pub seat: usize,
    pub revision: u64,
    pub action: String,
}
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct Hand {
    pub cfg: HandConfig,
    pub opening_stacks: [u64; 3],
    pub players: Vec<Participant>,
    pub bottom: Vec<Card>,
    pub landlord: Option<usize>,
    pub bid: u8,
    pub bids_taken: usize,
    pub multiplier: u64,
    pub to_act: usize,
    pub last: Option<(usize, Vec<Card>, Pattern)>,
    pub passes: u8,
    pub log: Vec<LogEntry>,
    pub outcome: Option<Outcome>,
    pub redeal: bool,
    #[serde(default)]
    pub last_timeout: Option<TimeoutAction>,
    /// `log.len()` after the action that last set or cleared `last_timeout`:
    /// that action changed the private view of a seat that may not have acted.
    #[serde(skip)]
    pub timeout_changed_at: usize,
}
impl Hand {
    pub fn new(
        cfg: HandConfig,
        seats: &[(usize, u64)],
        first: usize,
        deck: &[Card],
    ) -> Result<Self, ActionError> {
        if seats.len() != 3
            || seats.iter().enumerate().any(|(i, s)| s.0 != i)
            || first > 2
            || cfg.ante == 0
            || cfg.ante > super::room::ANTE_TIERS[2]
        {
            return Err(ActionError::NotInHand);
        }
        if seats.iter().any(|s| s.1 < min_stack(&cfg)) {
            return Err(ActionError::InsufficientChips);
        }
        if deck.len() != 54
            || deck.iter().any(|c| c.0 >= 54)
            || deck.iter().collect::<std::collections::HashSet<_>>().len() != 54
        {
            return Err(ActionError::InvalidCards);
        }
        let players = (0..3)
            .map(|i| {
                let mut cs = deck[i * 17..(i + 1) * 17].to_vec();
                cs.sort_by(|a, b| b.cmp(a));
                Participant {
                    seat: i,
                    cards: cs.clone(),
                    initial: cs,
                    played: 0,
                }
            })
            .collect();
        Ok(Self {
            cfg,
            opening_stacks: std::array::from_fn(|i| seats[i].1),
            players,
            bottom: deck[51..].to_vec(),
            landlord: None,
            bid: 0,
            bids_taken: 0,
            multiplier: 1,
            to_act: first,
            last: None,
            passes: 0,
            log: vec![],
            outcome: None,
            redeal: false,
            last_timeout: None,
            timeout_changed_at: 0,
        })
    }
    pub fn finished(&self) -> bool {
        self.outcome.is_some() || self.redeal
    }
    pub fn to_act_seat(&self) -> Option<usize> {
        (!self.finished()).then_some(self.to_act)
    }
    pub fn participant(&self, seat: usize) -> Option<&Participant> {
        self.players.get(seat)
    }
    pub fn visible_cards(&self, viewer: Option<usize>, target: usize) -> Option<&[Card]> {
        let p = self.participant(target)?;
        (viewer == Some(target) || self.outcome.is_some()).then_some(p.cards.as_slice())
    }
    pub fn target(&self) -> Option<Pattern> {
        self.last.as_ref().map(|(_, _, p)| *p)
    }
    pub fn legal(&self, seat: usize) -> Legal {
        if self.to_act_seat() != Some(seat) {
            return Legal::default();
        }
        if self.landlord.is_none() {
            return Legal {
                your_turn: true,
                bids: std::iter::once(0).chain(self.bid + 1..=3).collect(),
                ..Legal::default()
            };
        }
        Legal {
            your_turn: true,
            bids: vec![],
            can_pass: self.last.is_some(),
            hints: rules::moves(&self.players[seat].cards, self.target())
                .into_iter()
                .take(32)
                .map(|cs| cs.iter().map(|c| c.code()).collect())
                .collect(),
        }
    }
    pub fn apply(&mut self, seat: usize, action: Action) -> Result<(), ActionError> {
        if self.finished() {
            return Err(ActionError::HandFinished);
        }
        if seat >= 3 {
            return Err(ActionError::NotInHand);
        }
        if seat != self.to_act {
            return Err(ActionError::NotYourTurn);
        }
        if self.landlord.is_none() {
            let Action::Bid { score } = action else {
                return Err(ActionError::InvalidBid);
            };
            if score > 3 || (score != 0 && score <= self.bid) {
                return Err(ActionError::InvalidBid);
            }
            self.log.push(LogEntry::Bid { seat, score });
            self.bids_taken += 1;
            if score > 0 {
                self.bid = score;
            }
            if score == 3 || self.bids_taken == 3 {
                if self.bid == 0 {
                    self.redeal = true;
                    return Ok(());
                }
                let landlord = self
                    .log
                    .iter()
                    .rev()
                    .find_map(|e| match e {
                        LogEntry::Bid { seat, score } if *score == self.bid => Some(*seat),
                        _ => None,
                    })
                    .expect("positive bid exists");
                self.landlord = Some(landlord);
                self.to_act = landlord;
                let p = &mut self.players[landlord];
                p.cards.extend(&self.bottom);
                p.cards.sort_by(|a, b| b.cmp(a));
                p.initial = p.cards.clone();
            } else {
                self.to_act = (seat + 1) % 3;
            }
            return Ok(());
        }
        match action {
            Action::Bid { .. } => return Err(ActionError::InvalidBid),
            Action::Pass => {
                if self.last.is_none() {
                    return Err(ActionError::CannotPass);
                }
                self.log.push(LogEntry::Pass { seat });
                self.passes += 1;
                if self.passes == 2 {
                    self.last = None;
                    self.passes = 0;
                }
            }
            Action::Play { cards } => {
                let unique = cards.iter().collect::<std::collections::HashSet<_>>().len();
                if cards.is_empty()
                    || unique != cards.len()
                    || cards.iter().any(|c| !self.players[seat].cards.contains(c))
                {
                    return Err(ActionError::InvalidCards);
                }
                let p = rules::evaluate(&cards).ok_or(ActionError::InvalidPattern)?;
                if self.target().is_some_and(|t| !p.beats(t)) {
                    return Err(ActionError::CannotBeat);
                }
                self.players[seat].cards.retain(|c| !cards.contains(c));
                self.players[seat].played += 1;
                self.log.push(LogEntry::Play {
                    seat,
                    cards: cards.iter().map(|c| c.code()).collect(),
                    pattern: p,
                });
                self.last = Some((seat, cards, p));
                self.passes = 0;
                if matches!(p.kind, Kind::Bomb | Kind::Rocket) {
                    self.multiplier = (self.multiplier * 2).min(MAX_MULTIPLIER);
                }
                if self.players[seat].cards.is_empty() {
                    self.finish(seat);
                }
            }
        }
        self.to_act = (seat + 1) % 3;
        Ok(())
    }
    fn finish(&mut self, winner: usize) {
        let landlord = self.landlord.expect("playing");
        let landlord_won = winner == landlord;
        let spring = if landlord_won {
            self.players
                .iter()
                .filter(|p| p.seat != landlord)
                .all(|p| p.played == 0)
        } else {
            self.players[landlord].played == 1
        };
        if spring {
            self.multiplier = (self.multiplier * 2).min(MAX_MULTIPLIER);
        }
        let unit = (self.cfg.ante * u64::from(self.bid) * self.multiplier) as i64;
        let farmers: Vec<_> = (0..3).filter(|s| *s != landlord).collect();
        let stacks = self.opening_stacks;
        let mut payments = [unit as u64; 2];
        for i in 0..2 {
            payments[i] = payments[i].min(stacks[farmers[i]]);
        }
        let total = payments.iter().sum::<u64>();
        if total > stacks[landlord] {
            payments[0] = payments[0] * stacks[landlord] / total;
            payments[1] = stacks[landlord] - payments[0];
        }
        let mut deltas = vec![(0, 0i64), (1, 0i64), (2, 0i64)];
        for i in 0..2 {
            let delta = payments[i] as i64 * if landlord_won { -1 } else { 1 };
            deltas[farmers[i]].1 = delta;
            deltas[landlord].1 -= delta;
        }
        self.outcome = Some(Outcome {
            winners: (0..3)
                .filter(|s| (*s == landlord) == landlord_won)
                .collect(),
            pot: payments.iter().sum(),
            showdown: true,
            spring,
            deltas,
        });
    }
}
