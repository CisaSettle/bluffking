//! One 十三水 hand: thirteen cards each, simultaneous arrangement, pairwise
//! row comparison and zero-sum settlement capped at each table stack.
use super::rules::{self, Card, Rows, Special};
use serde::{Deserialize, Serialize};

pub const ANTE_TIERS: [u64; 3] = [1, 2, 5];
pub const MIN_SEATS: usize = 2;
pub const MAX_SEATS: usize = 4;
/// Antes a seat must hold to play: one opponent's 打枪 at its usual worst.
pub const MIN_STACK_ANTES: u64 = 36;

#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
pub struct HandConfig {
    pub ante: u64,
    pub max_seats: usize,
}
pub fn min_stack(c: &HandConfig) -> u64 {
    c.ante * MIN_STACK_ANTES
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Phase {
    Arrange,
    Complete,
}
impl Phase {
    pub fn seq(self) -> u8 {
        match self {
            Self::Arrange => 0,
            Self::Complete => 1,
        }
    }
}

/// The client's submission: card codes per row and whether to claim a special.
#[derive(Debug, Clone, Deserialize)]
pub struct Submit {
    pub front: Vec<String>,
    pub middle: Vec<String>,
    pub back: Vec<String>,
    pub special: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Arrangement {
    pub rows: Rows,
    pub special: Option<Special>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Player {
    pub seat: usize,
    pub stack: u64,
    /// Dealt cards, strongest first.
    pub cards: Vec<Card>,
    pub special_available: Option<Special>,
    pub suggestion: Arrangement,
    pub submitted: Option<Arrangement>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Gun {
    A,
    B,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Pair {
    pub a: usize,
    pub b: usize,
    /// 道 for `a` per row (negative: `b` won it).
    pub rows: [i64; 3],
    pub special: bool,
    pub gun: Option<Gun>,
    /// Net 道 for `a` after 打枪 / 全垒打.
    pub points: i64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Outcome {
    pub winners: Vec<usize>,
    pub deltas: Vec<(usize, i64)>,
    pub pot: u64,
    pub showdown: bool,
    pub pairs: Vec<Pair>,
    pub homerun: Option<usize>,
    pub points: Vec<(usize, i64)>,
    pub capped: Vec<usize>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Hand {
    pub cfg: HandConfig,
    pub players: Vec<Player>,
    pub phase: Phase,
    pub outcome: Option<Outcome>,
    pub log: Vec<serde_json::Value>,
}

impl Hand {
    pub fn new(cfg: HandConfig, seats: &[(usize, u64)], deck: &[Card]) -> Result<Self, &'static str> {
        let distinct = |v: Vec<usize>| v.iter().collect::<std::collections::HashSet<_>>().len() == v.len();
        if !(MIN_SEATS..=MAX_SEATS).contains(&cfg.max_seats)
            || !(MIN_SEATS..=cfg.max_seats).contains(&seats.len())
            || !ANTE_TIERS.contains(&cfg.ante)
            || seats
                .iter()
                .any(|(s, b)| *s >= cfg.max_seats || *b < min_stack(&cfg) || *b > i64::MAX as u64 / 4)
            || !distinct(seats.iter().map(|s| s.0).collect())
            || deck.len() != 52
            || deck.iter().any(|c| !c.valid())
            || deck.iter().collect::<std::collections::HashSet<_>>().len() != 52
        {
            return Err("invalid_hand");
        }
        let players = seats
            .iter()
            .enumerate()
            .map(|(i, (seat, stack))| {
                let mut cards = deck[i * rules::HAND..(i + 1) * rules::HAND].to_vec();
                cards.sort_by(|a, b| b.cmp(a));
                let special_available = rules::special(&cards);
                Player {
                    seat: *seat,
                    stack: *stack,
                    suggestion: Arrangement {
                        rows: rules::best(&cards),
                        special: special_available,
                    },
                    special_available,
                    cards,
                    submitted: None,
                }
            })
            .collect();
        Ok(Self {
            cfg,
            players,
            phase: Phase::Arrange,
            outcome: None,
            log: vec![],
        })
    }
    pub fn participant(&self, seat: usize) -> Option<&Player> {
        self.players.iter().find(|p| p.seat == seat)
    }
    pub fn finished(&self) -> bool {
        self.phase == Phase::Complete
    }
    pub fn needs_action(&self, seat: usize) -> bool {
        self.phase == Phase::Arrange && self.participant(seat).is_some_and(|p| p.submitted.is_none())
    }
    fn arrangement(p: &Player, submit: &Submit) -> Result<Arrangement, &'static str> {
        let parse = |codes: &[String], len: usize| -> Result<Vec<Card>, &'static str> {
            if codes.len() != len {
                return Err("invalid_arrangement");
            }
            codes
                .iter()
                .map(|c| Card::parse(c).ok_or("invalid_arrangement"))
                .collect()
        };
        let rows = Rows {
            front: parse(&submit.front, rules::ROWS[0])?,
            middle: parse(&submit.middle, rules::ROWS[1])?,
            back: parse(&submit.back, rules::ROWS[2])?,
        };
        let mut all = rows.all();
        all.sort_by(|a, b| b.cmp(a));
        if all != p.cards {
            return Err("invalid_arrangement");
        }
        let special = if submit.special {
            Some(p.special_available.ok_or("special_unavailable")?)
        } else {
            None
        };
        // A claimed special is not compared row by row.
        if special.is_none() && rows.foul() {
            return Err("foul");
        }
        Ok(Arrangement { rows, special })
    }
    pub fn act(&mut self, seat: usize, phase: Phase, seq: u8, submit: &Submit) -> Result<(), &'static str> {
        if phase != self.phase || seq != self.phase.seq() {
            return Err("stale_action");
        }
        let index = self.players.iter().position(|p| p.seat == seat).ok_or("not_in_hand")?;
        if !self.needs_action(seat) {
            return Err("already_acted");
        }
        let arrangement = Self::arrangement(&self.players[index], submit)?;
        self.submit(index, arrangement)
    }
    fn submit(&mut self, index: usize, arrangement: Arrangement) -> Result<(), &'static str> {
        let p = &mut self.players[index];
        p.submitted = Some(arrangement);
        self.log.push(serde_json::json!({"phase": Phase::Arrange, "seat": p.seat}));
        if self.players.iter().all(|p| p.submitted.is_some()) {
            self.settle()?;
            self.phase = Phase::Complete;
        }
        Ok(())
    }
    /// The server's arrangement for `seat` (bots, timeouts, departed seats).
    pub fn auto(&mut self, seat: usize) -> Result<(), &'static str> {
        if !self.needs_action(seat) {
            return Ok(());
        }
        let index = self.players.iter().position(|p| p.seat == seat).ok_or("not_in_hand")?;
        let suggestion = self.players[index].suggestion.clone();
        self.submit(index, suggestion)
    }
    pub fn timeout(&mut self) -> Result<(), &'static str> {
        let seats: Vec<_> = self
            .players
            .iter()
            .filter(|p| self.needs_action(p.seat))
            .map(|p| p.seat)
            .collect();
        for seat in seats {
            self.auto(seat)?;
        }
        Ok(())
    }

    fn compare(a: &Arrangement, b: &Arrangement) -> Pair {
        let (sa, sb) = (
            a.special.map_or(0, Special::points),
            b.special.map_or(0, Special::points),
        );
        if sa > 0 || sb > 0 {
            let points = match sa.cmp(&sb) {
                std::cmp::Ordering::Greater => sa,
                std::cmp::Ordering::Less => -sb,
                std::cmp::Ordering::Equal => 0,
            };
            return Pair { a: 0, b: 0, rows: [0; 3], special: true, gun: None, points };
        }
        let (ea, eb) = (a.rows.evals(), b.rows.evals());
        let rows: [i64; 3] = std::array::from_fn(|r| match ea[r].key().cmp(&eb[r].key()) {
            std::cmp::Ordering::Greater => rules::row_points(r, ea[r].kind),
            std::cmp::Ordering::Less => -rules::row_points(r, eb[r].kind),
            std::cmp::Ordering::Equal => 0,
        });
        let gun = if rows.iter().all(|x| *x > 0) {
            Some(Gun::A)
        } else if rows.iter().all(|x| *x < 0) {
            Some(Gun::B)
        } else {
            None
        };
        let sum: i64 = rows.iter().sum();
        Pair { a: 0, b: 0, rows, special: false, gun, points: if gun.is_some() { sum * 2 } else { sum } }
    }

    fn settle(&mut self) -> Result<(), &'static str> {
        let arranged: Vec<(usize, u64, &Arrangement)> = self
            .players
            .iter()
            .map(|p| Ok((p.seat, p.stack, p.submitted.as_ref().ok_or("missing_arrangement")?)))
            .collect::<Result<_, &'static str>>()?;
        let mut pairs = Vec::new();
        for i in 0..arranged.len() {
            for j in i + 1..arranged.len() {
                let mut pair = Self::compare(arranged[i].2, arranged[j].2);
                pair.a = arranged[i].0;
                pair.b = arranged[j].0;
                pairs.push(pair);
            }
        }
        // 全垒打: one seat shoots every opponent at a table of three or more.
        let homerun = (arranged.len() >= 3)
            .then(|| {
                arranged.iter().map(|x| x.0).find(|seat| {
                    pairs.iter().filter(|p| p.a == *seat || p.b == *seat).all(|p| {
                        (p.a == *seat && p.gun == Some(Gun::A)) || (p.b == *seat && p.gun == Some(Gun::B))
                    })
                })
            })
            .flatten();
        if let Some(seat) = homerun {
            for p in pairs.iter_mut().filter(|p| p.a == seat || p.b == seat) {
                p.points = p.points.checked_mul(2).ok_or("overflow")?;
            }
        }
        let points: Vec<(usize, i64)> = arranged
            .iter()
            .map(|(seat, _, _)| {
                let net = pairs
                    .iter()
                    .map(|p| if p.a == *seat { p.points } else if p.b == *seat { -p.points } else { 0 })
                    .sum();
                (*seat, net)
            })
            .collect();
        // Transfers payer → payee in chips, each payer capped at its stack.
        let ante = u128::from(self.cfg.ante);
        let mut transfers: Vec<(usize, usize, u128)> = pairs
            .iter()
            .filter(|p| p.points != 0)
            .map(|p| {
                let amount = u128::from(p.points.unsigned_abs()) * ante;
                if p.points > 0 { (p.b, p.a, amount) } else { (p.a, p.b, amount) }
            })
            .collect();
        let mut capped = Vec::new();
        for (seat, stack, _) in &arranged {
            let owed: u128 = transfers.iter().filter(|t| t.0 == *seat).map(|t| t.2).sum();
            let stack = u128::from(*stack);
            if owed > stack {
                capped.push(*seat);
                for t in transfers.iter_mut().filter(|t| t.0 == *seat) {
                    t.2 = t.2 * stack / owed;
                }
            }
        }
        let mut deltas = Vec::new();
        let mut pot = 0u64;
        for (seat, _, _) in &arranged {
            let gain: u128 = transfers.iter().filter(|t| t.1 == *seat).map(|t| t.2).sum();
            let loss: u128 = transfers.iter().filter(|t| t.0 == *seat).map(|t| t.2).sum();
            let delta = i64::try_from(gain).map_err(|_| "overflow")?
                - i64::try_from(loss).map_err(|_| "overflow")?;
            deltas.push((*seat, delta));
        }
        for t in &transfers {
            pot = pot.checked_add(u64::try_from(t.2).map_err(|_| "overflow")?).ok_or("overflow")?;
        }
        self.outcome = Some(Outcome {
            winners: deltas.iter().filter(|d| d.1 > 0).map(|d| d.0).collect(),
            deltas,
            pot,
            showdown: true,
            pairs,
            homerun,
            points,
            capped,
        });
        Ok(())
    }
}
