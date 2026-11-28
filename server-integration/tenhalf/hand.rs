//! Ten and a Half: four seats, a fixed player banker, integer half-points.
use serde::{Deserialize, Serialize};

#[derive(Clone, Copy, Serialize, Deserialize)]
pub struct Config {
    pub ante: u64,
    pub banker_user: Option<uuid::Uuid>,
    pub banker_base: u64,
    pub banker_seat: Option<usize>,
    pub banker_start_net: i64,
    pub banker_waiting: bool,
    pub retire_requested: bool,
    /// Card ids already dealt from the current 52-card shoe in earlier hands.
    pub shoe_used: u64,
    /// Bumped on every shuffle so clients can announce it.
    pub shoe_epoch: u32,
    /// The last hand whose cards `shoe_used` holds; `None` for a fresh shoe.
    pub shoe_last_hand: Option<uuid::Uuid>,
}
#[derive(Clone, Copy, Debug, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Move {
    Hit,
    Stand,
    /// Busted banker only (埋牌): give up the hand at once. Challengers who have
    /// already played compare as usual; those still waiting win by default.
    Bury,
    Bet {
        target: usize,
        amount: u64,
    },
}
#[derive(Clone, Serialize, Deserialize)]
pub struct Player {
    pub seat: usize,
    pub cards: Vec<u8>,
    pub bet: u64,
    pub done: bool,
    pub delta: i64,
    pub expected_delta: i64,
    pub voided: bool,
    /// The hand's `seq` when an action last changed this seat's public row;
    /// the room's action event sends only rows changed since its cursor.
    /// Never persisted: a restored unfinished hand is abandoned.
    #[serde(skip)]
    pub changed_at: u32,
}
#[derive(Clone, Serialize, Deserialize)]
pub struct SideBet {
    pub seat: usize,
    pub target: usize,
    pub amount: u64,
}
#[derive(Clone, Serialize, Deserialize)]
pub struct Hand {
    pub proof_positions: std::collections::BTreeMap<usize, Vec<usize>>,
    pub players: Vec<Player>,
    /// This hand's committed deck in proof order; it is never reordered.
    deck: Vec<u8>,
    /// Proof positions already drawn this hand.
    drawn: u64,
    /// Shoe cards dealt in earlier hands; draws skip them until the shoe runs out.
    pub discards: u64,
    /// The hand whose end left the shoe at `discards`; `None` for a fresh shoe.
    pub shoe_from: Option<uuid::Uuid>,
    /// The shoe ran out mid-hand and the discards were shuffled back in.
    pub reshuffled: bool,
    /// The room has folded this hand's shoe state into its config.
    pub shoe_synced: bool,
    /// The banker asked to shuffle during this hand; the shoe is shuffled
    /// once this hand ends, never mid-hand.
    pub shuffle_queued: bool,
    /// The busted banker gave up the hand (埋牌).
    pub banker_buried: bool,
    pub seq: u32,
    pub finished: bool,
    pub banker: usize,
    pub dice: [u8; 2],
    pub deal_order: Vec<usize>,
    pub betting: bool,
    pub revealing: bool,
    pub revealed: usize,
    pub side_pending: Vec<usize>,
    /// Append order is the server-confirmed payout priority.
    pub side_bets: Vec<SideBet>,
    pub opening_stacks: std::collections::BTreeMap<usize, u64>,
}
pub fn score(cards: &[u8]) -> u8 {
    cards
        .iter()
        .map(|c| if c % 13 >= 10 { 1 } else { (c % 13 + 1) * 2 })
        .sum()
}
impl Hand {
    pub fn turn(&self) -> Option<usize> {
        if self.finished {
            return None;
        }
        if self.betting {
            return self.side_pending.first().copied();
        }
        if self.revealing {
            return self.deal_order.get(self.revealed).copied();
        }
        self.deal_order
            .iter()
            .copied()
            .find(|seat| self.players.iter().any(|p| p.seat == *seat && !p.done))
    }
    pub fn act(&mut self, seat: usize, action: Move) -> Result<(), String> {
        if self.betting {
            return self.place_side_bet(seat, action);
        }
        if self.finished || self.revealing || self.turn() != Some(seat) {
            return Err("not_your_turn".into());
        }
        let banker = seat == self.banker;
        let cards = self
            .players
            .iter()
            .find(|p| p.seat == seat)
            .ok_or("invalid_hand")?
            .cards
            .clone();
        let busted = score(&cards) > 21;
        match action {
            Move::Bet { .. } => return Err("invalid_action".into()),
            Move::Stand => self.touch(seat)?.done = true,
            // A busted banker may only stand (and keep the bust hidden) or bury.
            Move::Bury if banker && busted => {
                self.banker_buried = true;
                self.end_all_turns();
            }
            Move::Bury => return Err("invalid_action".into()),
            Move::Hit if banker && busted => return Err("invalid_action".into()),
            Move::Hit => {
                let (card, position) = self.draw()?;
                self.proof_positions.entry(seat).or_default().push(position);
                let p = self.touch(seat)?;
                p.cards.push(card);
                // Ten-and-a-half or five cards end any turn and show the hand; a
                // challenger's bust does too. A banker's bust stays hidden and,
                // even on the fifth card, leaves the choice to stand or bury.
                let bust = score(&p.cards) > 21;
                p.done = score(&p.cards) == 21
                    || if banker {
                        p.cards.len() == 5 && !bust
                    } else {
                        bust || p.cards.len() == 5
                    };
                // The banker's ten-and-a-half or five dragons beats every challenger
                // (ties go to the banker), so the hand ends for everyone at once.
                if banker && table_rank(&p.cards) == 2 {
                    self.end_all_turns();
                }
            }
        }
        self.seq += 1;
        if self.players.iter().any(|p| !p.done) {
            return Ok(());
        }
        self.revealing = true;
        self.skip_open_reveals()
    }
}

impl Hand {
    fn player_mut(&mut self, seat: usize) -> Result<&mut Player, &'static str> {
        self.players
            .iter_mut()
            .find(|p| p.seat == seat)
            .ok_or("invalid_hand")
    }
    /// The player whose public row this action changes.
    fn touch(&mut self, seat: usize) -> Result<&mut Player, &'static str> {
        let seq = self.seq;
        let p = self.player_mut(seat)?;
        p.changed_at = seq;
        Ok(p)
    }
    fn end_all_turns(&mut self) {
        let seq = self.seq;
        for p in self.players.iter_mut().filter(|p| p.seat < 4) {
            p.done = true;
            p.changed_at = seq;
        }
    }
    /// The next card of the shoe: the highest undrawn proof position whose card
    /// was not dealt in an earlier hand. Once the shoe is spent, the earlier
    /// discards (never cards on the table) are shuffled back in, in proof order.
    fn draw(&mut self) -> Result<(u8, usize), &'static str> {
        let undrawn = |i: &usize| self.drawn & (1u64 << i) == 0;
        let position = (0..self.deck.len())
            .rev()
            .filter(undrawn)
            .find(|i| self.discards & (1u64 << self.deck[*i]) == 0)
            .or_else(|| (0..self.deck.len()).rev().find(undrawn))
            .ok_or("empty_deck")?;
        let card = self.deck[position];
        if self.discards & (1u64 << card) != 0 {
            self.reshuffled = true;
        }
        self.drawn |= 1u64 << position;
        Ok((card, position))
    }
    /// Positions opened publicly once the hand settles: every position the
    /// draws examined, so a verifier can replay `draw` from the starting
    /// discards. Without a mid-hand reshuffle that is the top of the deck down
    /// to the lowest drawn position (drawn cards plus the skipped discards);
    /// a reshuffle examined the whole deck. Every opened card was either dealt
    /// in this hand or dealt face up in an earlier hand of the same shoe.
    pub fn settled_positions(&self) -> Vec<usize> {
        let lowest = if self.reshuffled {
            Some(0)
        } else {
            (0..self.deck.len()).find(|i| self.drawn & (1u64 << i) != 0)
        };
        lowest.map_or_else(Vec::new, |low| (low..self.deck.len()).collect())
    }
    /// Each main seat's cards, indexed by seat.
    pub fn main_cards(&self) -> Vec<Vec<u8>> {
        (0..4)
            .map(|s| {
                self.players
                    .iter()
                    .find(|p| p.seat == s)
                    .map_or_else(Vec::new, |p| p.cards.clone())
            })
            .collect()
    }
    /// Card ids already dealt from this shoe when the hand began.
    pub fn starting_discards(&self) -> Vec<u8> {
        (0..52u8)
            .filter(|c| self.discards & (1u64 << c) != 0)
            .collect()
    }
    fn dealt(&self) -> u64 {
        (0..self.deck.len())
            .filter(|i| self.drawn & (1u64 << i) != 0)
            .fold(0, |m, i| m | 1u64 << self.deck[i])
    }
    /// Cards the shoe has used so far. A hand that had to reshuffle mid-hand,
    /// or had a shuffle queued, starts a fresh shoe once it ends.
    pub fn shoe_used(&self) -> u64 {
        match (self.reshuffled, self.finished) {
            (_, true) if self.reshuffled || self.shuffle_queued => 0,
            (true, _) => self.dealt(),
            (false, _) => self.discards | self.dealt(),
        }
    }
    /// The banker's shuffle during a hand (betting included) waits for the
    /// hand to end, so it can never change what a hand in progress draws or
    /// react to bets already placed. Returns whether anything changed.
    pub fn queue_shuffle(&mut self) -> bool {
        !std::mem::replace(&mut self.shuffle_queued, true)
    }
}
/// 0 = bust, 2 = ten-and-a-half or five dragons (one tier, both pay double),
/// 1 = ordinary total.
pub fn table_rank(cards: &[u8]) -> u8 {
    if score(cards) > 21 {
        0
    } else if cards.len() == 5 || score(cards) == 21 {
        2
    } else {
        1
    }
}
/// Comparison total in half-points: five dragons count as ten-and-a-half.
fn compare_score(cards: &[u8]) -> u8 {
    if table_rank(cards) == 2 {
        21
    } else {
        score(cards)
    }
}
/// The dice that pick the first seat dealt, derived from the deal's card
/// commitment and the player seeds that ordered its deck. Both are fixed
/// before the deck is known, so the server cannot choose the dice after
/// seeing the cards; a verifier recomputes them from the proof bundle.
pub fn dice_from(commitment: &str, seeds: &std::collections::BTreeMap<u8, [u8; 32]>) -> [u8; 2] {
    let mut parts: Vec<&[u8]> = vec![commitment.as_bytes()];
    let seats: Vec<[u8; 1]> = seeds.keys().map(|s| [*s]).collect();
    for ((_, seed), seat) in seeds.iter().zip(&seats) {
        parts.push(seat);
        parts.push(seed);
    }
    let h = mental_poker::hash::ds_hash("tenhalf:dice:v1", &parts);
    let mut low = [0u8; 8];
    low.copy_from_slice(&h[..8]);
    let v = u64::from_le_bytes(low);
    [(v % 6) as u8 + 1, (v / 6 % 6) as u8 + 1]
}
impl Hand {
    pub fn new_four(
        cfg: Config,
        seats: &[(usize, u64)],
        deck: Vec<u8>,
        banker: usize,
        dice: [u8; 2],
    ) -> Result<Self, &'static str> {
        if !(4..=12).contains(&seats.len())
            || cfg.ante == 0
            || cfg.ante > 10
            || !(0..4).all(|s| seats.iter().filter(|(seat, _)| *seat == s).count() == 1)
            || seats
                .iter()
                .map(|(seat, _)| *seat)
                .collect::<std::collections::BTreeSet<_>>()
                .len()
                != seats.len()
            || banker >= 4
            || !dice.iter().all(|d| (1..=6).contains(d))
            || seats.iter().any(|(s, stack)| {
                *s >= 12
                    || *stack < if *s < 4 { cfg.ante } else { 1 }
                    || *stack > i64::MAX as u64 / 32
            })
            || deck.len() != 52
        {
            return Err("invalid_deal");
        }
        let mut seen = [false; 52];
        for &c in &deck {
            if c >= 52 || seen[c as usize] {
                return Err("invalid_deck");
            }
            seen[c as usize] = true;
        }
        let mut h = Self {
            proof_positions: Default::default(),
            players: vec![],
            deck,
            drawn: 0,
            discards: cfg.shoe_used,
            shoe_from: cfg.shoe_last_hand,
            reshuffled: false,
            shoe_synced: false,
            shuffle_queued: false,
            banker_buried: false,
            seq: 0,
            finished: false,
            banker,
            dice,
            deal_order: vec![],
            betting: true,
            revealing: false,
            revealed: 0,
            side_pending: seats
                .iter()
                .filter_map(|(seat, _)| (*seat != banker).then_some(*seat))
                .collect(),
            side_bets: vec![],
            opening_stacks: seats.iter().copied().collect(),
        };
        for seat in (0..4).chain(
            seats
                .iter()
                .filter_map(|(seat, _)| (*seat >= 4).then_some(*seat)),
        ) {
            h.players.push(Player {
                seat,
                cards: vec![],
                bet: if seat < 4 { cfg.ante } else { 0 },
                done: seat >= 4,
                delta: 0,
                expected_delta: 0,
                voided: false,
                changed_at: 0,
            });
        }
        Ok(h)
    }
    pub fn can_act(&self, seat: usize) -> bool {
        !self.finished
            && !self.revealing
            && if self.betting {
                self.side_pending.contains(&seat)
            } else {
                self.turn() == Some(seat)
            }
    }
    fn deal_main_cards(&mut self) -> Result<(), &'static str> {
        if self.deal_order.is_empty() {
            let banker = self.banker;
            // Screen seats increase bottom -> right -> top -> left; clockwise decreases.
            let first = (banker + 4 - (usize::from(self.dice[0] + self.dice[1]) - 1) % 4) % 4;
            self.deal_order = (0..4).map(|offset| (first + 4 - offset) % 4).collect();
        }
        for seat in self.deal_order.clone() {
            let (card, position) = self.draw()?;
            self.touch(seat)?.cards.push(card);
            self.proof_positions.insert(seat, vec![position]);
        }
        self.betting = false;
        self.side_pending.clear();
        Ok(())
    }
    fn place_side_bet(&mut self, seat: usize, action: Move) -> Result<(), String> {
        if !self.can_act(seat) {
            return Err("not_your_turn".into());
        }
        match action {
            Move::Bet { target, amount } => {
                if target >= 4
                    || target == self.banker
                    || amount == 0
                    || (seat < 4 && target != seat)
                    || amount > self.opening_stacks.get(&seat).copied().unwrap_or(0)
                {
                    return Err("invalid_side_bet".into());
                }
                let seq = self.seq;
                let p = self.player_mut(seat)?;
                if seat < 4 && (amount < p.bet || amount % p.bet != 0) {
                    return Err("invalid_side_bet".into());
                }
                p.bet = amount;
                p.changed_at = seq;
                if seat >= 4 {
                    self.side_bets.push(SideBet {
                        seat,
                        target,
                        amount,
                    });
                }
            }
            Move::Stand => {}
            Move::Hit | Move::Bury => return Err("invalid_action".into()),
        }
        self.side_pending.retain(|s| *s != seat);
        // The closing bet deals in the same action, so its cards carry this
        // action's stamp.
        if self.side_pending.is_empty() {
            self.deal_main_cards().map_err(str::to_owned)?;
        }
        self.seq += 1;
        Ok(())
    }
    pub fn close_betting(&mut self) -> Result<(), String> {
        if self.betting {
            self.deal_main_cards().map_err(str::to_owned)?;
            self.seq += 1;
        }
        Ok(())
    }
    pub fn reveal_next(&mut self) -> Result<(), String> {
        if !self.revealing || self.finished {
            return Err("invalid_action".into());
        }
        // The next seat in deal order turns its hand face up.
        let (seq, next) = (self.seq, self.deal_order.get(self.revealed).copied());
        if let Some(p) = self.players.iter_mut().find(|p| Some(p.seat) == next) {
            p.changed_at = seq;
        }
        self.revealed += 1;
        self.seq += 1;
        self.skip_open_reveals()
    }
    /// Seats already shown face-up need no reveal step of their own.
    fn skip_open_reveals(&mut self) -> Result<(), String> {
        while self
            .deal_order
            .get(self.revealed)
            .is_some_and(|s| self.open_early(*s))
        {
            self.revealed += 1;
        }
        if self.revealed == self.deal_order.len() {
            self.settle_four()?;
            self.revealing = false;
        }
        Ok(())
    }
    /// A turn that ended on ten-and-a-half or five dragons shows the whole hand
    /// at once, and so does a challenger's bust; a busted banker stays hidden.
    pub fn open_early(&self, seat: usize) -> bool {
        seat < 4
            && self.players.iter().any(|p| {
                p.seat == seat
                    && p.done
                    && (table_rank(&p.cards) == 2 || (seat != self.banker && score(&p.cards) > 21))
            })
    }
    pub fn is_revealed(&self, seat: usize) -> bool {
        self.open_early(seat)
            || self
                .deal_order
                .iter()
                .take(self.revealed)
                .any(|s| *s == seat)
    }
    fn settle_four(&mut self) -> Result<(), String> {
        let banker = self
            .players
            .iter()
            .find(|p| p.seat == self.banker)
            .ok_or("invalid_hand")?;
        let br = table_rank(&banker.cards);
        let bs = compare_score(&banker.cards);
        for p in self
            .players
            .iter_mut()
            .filter(|p| p.seat < 4 && p.seat != self.banker)
        {
            let pr = table_rank(&p.cards);
            // 庄家大半点: equal totals, including two ten-and-a-half or five-dragon
            // hands, go to the banker; any higher total beats the banker.
            let won = pr > 0 && (br == 0 || compare_score(&p.cards) > bs);
            p.expected_delta = if won {
                p.bet as i64 * if pr >= 2 { 2 } else { 1 }
            } else {
                -(p.bet as i64)
            };
        }
        for bet in &self.side_bets {
            let target = self
                .players
                .iter()
                .find(|p| p.seat == bet.target)
                .ok_or("invalid_hand")?;
            let expected = if target.expected_delta > 0 {
                bet.amount as i64 * if table_rank(&target.cards) >= 2 { 2 } else { 1 }
            } else {
                -(bet.amount as i64)
            };
            let side = self
                .players
                .iter_mut()
                .find(|p| p.seat == bet.seat)
                .ok_or("invalid_hand")?;
            side.expected_delta = expected;
        }
        let banker_seat = self.banker;
        let opening = *self
            .opening_stacks
            .get(&banker_seat)
            .ok_or("invalid_hand")? as i64;
        let mut remaining = opening;
        // Resolve each main challenger in deal order, then side bets in confirmation order.
        // Once the bank is empty, later losses are void too; future receipts cannot refill it.
        let order: Vec<_> = self
            .deal_order
            .iter()
            .copied()
            .filter(|s| *s != banker_seat)
            .chain(self.side_bets.iter().map(|b| b.seat))
            .collect();
        for seat in order {
            let p = self
                .players
                .iter_mut()
                .find(|p| p.seat == seat)
                .ok_or("invalid_hand")?;
            if remaining == 0 {
                p.voided = true;
                p.delta = 0;
            } else {
                p.delta = if p.expected_delta > 0 {
                    p.expected_delta.min(remaining)
                } else {
                    p.expected_delta
                };
                remaining -= p.delta;
            }
        }
        let banker = self
            .players
            .iter_mut()
            .find(|p| p.seat == banker_seat)
            .ok_or("invalid_hand")?;
        banker.delta = remaining - opening;
        banker.expected_delta = banker.delta;
        self.finished = true;
        Ok(())
    }
    /// Decisions use only the acting seat's cards, never opponents or deck order.
    pub fn bot_move(&self, seat: usize) -> Move {
        let Some(p) = self.players.iter().find(|p| p.seat == seat) else {
            return Move::Stand;
        };
        let threshold = if seat == self.banker { 15 } else { 16 };
        if score(&p.cards) < threshold {
            Move::Hit
        } else {
            Move::Stand
        }
    }
}
