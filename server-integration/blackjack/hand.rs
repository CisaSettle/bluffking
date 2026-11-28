use serde::{Deserialize, Serialize};

#[derive(Clone, Copy, Serialize, Deserialize)]
pub struct Config {
    pub ante: u64,
}
#[derive(Clone, Copy, Debug, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Move {
    Hit,
    Stand,
    Double,
}
#[derive(Clone, Serialize, Deserialize)]
pub struct Player {
    pub seat: usize,
    pub stack: u64,
    pub cards: Vec<u8>,
    pub bet: u64,
    pub done: bool,
    pub delta: i64,
}
#[derive(Clone, Serialize, Deserialize)]
pub struct Hand {
    #[serde(default)]
    pub proof_positions: std::collections::BTreeMap<usize, Vec<usize>>,
    #[serde(default)]
    pub dealer_positions: Vec<usize>,
    pub players: Vec<Player>,
    pub dealer: Vec<u8>,
    deck: Vec<u8>,
    pub seq: u32,
    pub finished: bool,
}
pub fn score(cards: &[u8]) -> u8 {
    let mut n: u16 = cards
        .iter()
        .map(|c| {
            if c % 13 == 0 {
                11
            } else {
                (c % 13 + 1).min(10) as u16
            }
        })
        .sum();
    for _ in cards.iter().filter(|c| **c % 13 == 0) {
        if n > 21 {
            n -= 10;
        }
    }
    n as u8
}
impl Hand {
    pub fn new(
        cfg: Config,
        seats: &[(usize, u64)],
        mut deck: Vec<u8>,
    ) -> Result<Self, &'static str> {
        if seats.is_empty()
            || seats.len() > 6
            || cfg.ante == 0
            || seats.iter().any(|(_, s)| *s < cfg.ante)
            || deck.len() != 52
        {
            return Err("invalid_deal");
        }
        let mut seen = [false; 52];
        for &c in &deck {
            if c > 51 || seen[c as usize] {
                return Err("invalid_deck");
            }
            seen[c as usize] = true;
        }
        let proof_positions = seats
            .iter()
            .enumerate()
            .map(|(i, (seat, _))| (*seat, vec![51 - 2 * i, 50 - 2 * i]))
            .collect();
        let players = seats
            .iter()
            .map(|&(seat, stack)| Player {
                seat,
                stack,
                cards: vec![deck.pop().unwrap(), deck.pop().unwrap()],
                bet: cfg.ante,
                done: false,
                delta: 0,
            })
            .collect();
        let dealer_positions = vec![deck.len() - 1, deck.len() - 2];
        let dealer = vec![deck.pop().unwrap(), deck.pop().unwrap()];
        let mut h = Self {
            proof_positions,
            dealer_positions,
            players,
            dealer,
            deck,
            seq: 0,
            finished: false,
        };
        for p in &mut h.players {
            p.done = score(&p.cards) == 21 || score(&h.dealer) == 21;
        }
        h.settle_if_done();
        Ok(h)
    }
    pub fn turn(&self) -> Option<usize> {
        self.players.iter().find(|p| !p.done).map(|p| p.seat)
    }
    /// Doubling needs the first two cards and a stack covering twice the bet.
    pub fn can_double(&self, seat: usize) -> bool {
        self.players
            .iter()
            .any(|p| p.seat == seat && p.cards.len() == 2 && p.stack >= p.bet * 2)
    }
    /// Seats whose rows the actions from cursor `from` (an earlier `seq`) on
    /// changed. Players act strictly in seat order and every action is one
    /// stand or one hit/double card, so each player's actions occupy one
    /// contiguous span of `0..seq`.
    pub fn acted_since(&self, from: u32, ante: u64) -> impl Iterator<Item = usize> + '_ {
        let mut end = 0u32;
        self.players.iter().filter_map(move |p| {
            let start = end;
            // A stand leaves the opening bet and a live total (a double, a
            // bust or 21 finishes the seat without one).
            let stood = p.done && score(&p.cards) < 21 && p.bet == ante;
            end += (p.cards.len() - 2) as u32 + u32::from(stood);
            (start < end && start < self.seq && end > from).then_some(p.seat)
        })
    }
    pub fn act(&mut self, seat: usize, m: Move) -> Result<(), String> {
        if self.finished || self.turn() != Some(seat) {
            return Err("not_your_turn".into());
        }
        if matches!(m, Move::Double) && !self.can_double(seat) {
            return Err("invalid_double".into());
        }
        let p = self.players.iter_mut().find(|p| p.seat == seat).unwrap();
        match m {
            Move::Stand => p.done = true,
            Move::Hit | Move::Double => {
                let c = self.deck.pop().ok_or("empty_deck")?;
                p.cards.push(c);
                self.proof_positions
                    .entry(seat)
                    .or_default()
                    .push(self.deck.len());
                if matches!(m, Move::Double) {
                    p.bet *= 2;
                    p.done = true;
                }
                p.done |= score(&p.cards) >= 21;
            }
        }
        self.seq += 1;
        self.settle_if_done();
        Ok(())
    }
    fn settle_if_done(&mut self) {
        if self.turn().is_some() {
            return;
        }
        if self.players.iter().any(|p| score(&p.cards) <= 21) {
            while score(&self.dealer) < 17 {
                let Some(c) = self.deck.pop() else { break };
                self.dealer.push(c);
                self.dealer_positions.push(self.deck.len());
            }
        }
        let d = score(&self.dealer);
        let natural = d == 21 && self.dealer.len() == 2;
        for p in &mut self.players {
            let n = score(&p.cards);
            let b = p.bet as i64;
            p.delta = if n > 21 {
                -b
            } else if n == 21 && p.cards.len() == 2 {
                if natural {
                    0
                } else {
                    b * 3 / 2
                }
            } else if natural {
                -b
            } else if d > 21 || n > d {
                b
            } else if n == d {
                0
            } else {
                -b
            };
        }
        self.finished = true;
    }
}
