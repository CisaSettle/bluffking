use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::sync::Arc;
use uuid::Uuid;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Color {
    Red,
    Yellow,
    Green,
    Blue,
}
const COLORS: [Color; 4] = [Color::Red, Color::Yellow, Color::Green, Color::Blue];

fn color(card: u8) -> Option<Color> {
    (card < 100).then(|| COLORS[card as usize / 25])
}
// Numbers 0..9; skip=10, reverse=11, draw-two=12, wild=13, wild-four=14.
fn rank(card: u8) -> u8 {
    if card >= 104 {
        14
    } else if card >= 100 {
        13
    } else {
        let n = card % 25;
        if n == 0 {
            0
        } else {
            1 + (n - 1) / 2
        }
    }
}
fn points(card: u8) -> u32 {
    match rank(card) {
        n @ 0..=9 => n as u32,
        10..=12 => 20,
        _ => 50,
    }
}
/// A card as every client payload shows it.
#[derive(Serialize)]
pub(super) struct CardView {
    id: u8,
    color: Option<Color>,
    kind: &'static str,
    value: Option<u8>,
}
impl CardView {
    fn of(card: u8) -> Self {
        let r = rank(card);
        Self {
            id: card,
            color: color(card),
            kind: match r {
                0..=9 => "number",
                10 => "skip",
                11 => "reverse",
                12 => "draw2",
                13 => "wild",
                _ => "wild_draw4",
            },
            value: (r <= 9).then_some(r),
        }
    }
}
fn card_view(card: u8) -> Value {
    json!(CardView::of(card))
}

/// The public part of an ordinary move: every shared `match` field a play,
/// draw, pass or resignation that does not end the match can change. The
/// match id and `card_proof` (a new shuffle epoch is only ever dealt on the
/// entropy path), `finished`, `winner`, `reason` and `points` are fixed
/// until the match ends.
#[derive(Serialize)]
pub(super) struct MoveEvent<'a> {
    revision: u64,
    turn: usize,
    resigned: &'a [bool],
    counts: Vec<usize>,
    color: Color,
    direction: i8,
    top: Option<CardView>,
}

/// Which viewers' private `match` fields the last move changed (runtime
/// only: the move event's private bodies follow it).
#[derive(Clone, Copy, Default)]
struct Touched {
    /// A play: the top card is in every viewer's `card_proof_views`.
    everyone: bool,
    /// Seats whose hand, drawn card or actions changed.
    seats: u8,
    /// A resignation cancelled a draw awaiting entropy: every frame's
    /// `player_entropy` changes, so it is a boundary, not a move event.
    entropy: bool,
}
impl Touched {
    fn seats(seats: &[usize]) -> Self {
        Self {
            seats: seats.iter().fold(0, |bits, s| bits | (1 << s)),
            ..Self::default()
        }
    }
}

type DeferredDraw = (usize, Option<(u8, Option<Color>)>);

/// This full representation is a private checkpoint, never a client payload.
#[derive(Clone, Serialize, Deserialize)]
pub struct Match {
    pub proofs: std::collections::BTreeMap<usize, ProofEpoch>,
    proof_origins: Arc<std::collections::BTreeMap<u8, (usize, usize)>>,
    proof_positions: Vec<usize>,
    next_proof: Option<crate::card_proofs::Deal>,
    pub pending_entropy: Option<crate::zjh::fairness::Pending>,
    proof_owners: std::collections::BTreeMap<usize, Option<Uuid>>,
    deferred_draw: Option<DeferredDraw>,
    pub id: Uuid,
    pub revision: u64,
    pub(crate) turn_sequence: u64,
    pub turn: usize,
    pub finished: bool,
    pub winner: Option<usize>,
    pub resigned: Vec<bool>,
    pub reason: String,
    pub points: u32,
    hands: Vec<Vec<u8>>,
    deck: Vec<u8>,
    discard: Vec<u8>,
    current_color: Color,
    direction: i8,
    has_drawn: bool,
    drawn: Option<u8>,
    stalled: usize,
    #[serde(skip)]
    touched: Touched,
}
impl Match {
    pub fn new(
        players: usize,
        starting_seat: usize,
        proof: crate::card_proofs::Deal,
        proof_owners: std::collections::BTreeMap<usize, Option<Uuid>>,
    ) -> Option<Self> {
        if !(2..=6).contains(&players) {
            return None;
        }
        let id = proof.secret.context.hand_id;
        let mut deck: Vec<u8> = proof.cards.iter().map(|c| *c as u8).collect();
        let mut epoch = ProofEpoch {
            deal: Arc::new(proof),
            public: Vec::new(),
            grants: vec![Vec::new(); players],
        };
        let mut proof_origins = std::collections::BTreeMap::new();
        let mut proof_positions: Vec<usize> = (0..108).collect();
        let mut hands = vec![vec![]; players];
        for _ in 0..7 {
            for (seat, hand) in hands.iter_mut().enumerate() {
                let card = deck.pop()?;
                let position = proof_positions.pop()?;
                hand.push(card);
                epoch.grants[seat].push(position);
                proof_origins.insert(card, (0, position));
            }
        }
        let index = deck.iter().position(|c| rank(*c) < 10)?;
        let first = deck.remove(index);
        let position = proof_positions.remove(index);
        epoch.public.push(position);
        proof_origins.insert(first, (0, position));
        Some(Self {
            id,
            proofs: [(0, epoch)].into_iter().collect(),
            proof_origins: Arc::new(proof_origins),
            proof_positions,
            next_proof: None,
            pending_entropy: None,
            proof_owners,
            deferred_draw: None,
            revision: 0,
            turn_sequence: 0,
            turn: starting_seat % players,
            finished: false,
            winner: None,
            resigned: vec![false; players],
            reason: String::new(),
            points: 0,
            hands,
            deck,
            discard: vec![first],
            current_color: color(first)?,
            direction: 1,
            has_drawn: false,
            drawn: None,
            stalled: 0,
            touched: Touched::default(),
        })
    }
    /// Retire only epochs no physical card still references. The current deck
    /// and at most 108 physical origins bound resident secrets to 109 epochs.
    /// The caller commits these receipts atomically with the pruned checkpoint.
    pub fn archive_epochs(&mut self) -> Vec<ProofEpoch> {
        if self.proofs.len() <= 1 {
            // The current shoe must keep its receipt, including before any draw.
            return Vec::new();
        }
        let current = self.proofs.keys().next_back().copied();
        let active: std::collections::BTreeSet<_> =
            self.proof_origins.values().map(|(e, _)| *e).collect();
        let retired: Vec<_> = self
            .proofs
            .keys()
            .copied()
            .filter(|e| Some(*e) != current && !active.contains(e))
            .collect();
        retired
            .into_iter()
            .filter_map(|e| self.proofs.remove(&e))
            .collect()
    }
    fn active(&self) -> usize {
        self.resigned.iter().filter(|r| !**r).count()
    }
    fn check_turn(&self, seat: usize) -> Result<(), &'static str> {
        if self.deferred_draw.is_some() {
            return Err("entropy_pending");
        }
        if self.finished {
            return Err("match_over");
        }
        if seat >= self.hands.len() || self.resigned[seat] || seat != self.turn {
            return Err("not_your_turn");
        }
        Ok(())
    }
    fn next(&mut self) {
        self.turn_sequence = self.turn_sequence.saturating_add(1);
        for _ in 0..self.hands.len() {
            self.turn = (self.turn as isize + self.direction as isize)
                .rem_euclid(self.hands.len() as isize) as usize;
            if !self.resigned[self.turn] {
                break;
            }
        }
        self.has_drawn = false;
        self.drawn = None;
    }
    fn playable_in_hand(&self, hand: &[u8], card: u8) -> bool {
        if self.has_drawn && self.drawn != Some(card) {
            return false;
        }
        if rank(card) == 14 {
            // Server enforcement replaces bluff/challenge mechanics, including after drawing.
            return !hand.iter().any(|c| color(*c) == Some(self.current_color));
        }
        card >= 100
            || color(card) == Some(self.current_color)
            || rank(card) == rank(*self.discard.last().unwrap())
    }
    fn prepare_draw(&mut self, count: usize, new_top: bool) -> Result<(), &'static str> {
        if self.deck.len() >= count {
            return Ok(());
        }
        if let Some(pending) = &self.pending_entropy {
            let Some(deal) = pending.finish_cards() else {
                return Err("entropy_pending");
            };
            self.next_proof = Some(deal);
            self.pending_entropy = None;
            return Ok(());
        }
        let end = self.discard.len().saturating_sub(usize::from(!new_top));
        if end == 0 {
            return Ok(());
        }
        let mut subset: Vec<u16> = self.discard[..end].iter().map(|c| u16::from(*c)).collect();
        subset.sort_unstable();
        let previous = &self
            .proofs
            .values()
            .next_back()
            .ok_or("proof_unavailable")?
            .deal
            .commitment;
        let context = mental_poker::card_proof::Context {
            game: mental_poker::card_proof::Game::Colorcards,
            hand_id: Uuid::now_v7(),
            seats: (0..self.hands.len() as u8).collect(),
            roster: previous.context.roster.clone(),
            epoch: previous
                .context
                .epoch
                .checked_add(1)
                .ok_or("proof_epoch_limit")?,
            previous: Some(previous.commitment.clone()),
            subset: Some(subset),
        };
        let deal = crate::card_proofs::take_context(context).ok_or("entropy_pending")?;
        let expected = self
            .proof_owners
            .iter()
            .filter_map(|(s, u)| u.filter(|_| !self.resigned[*s]).map(|_| *s as u8))
            .collect();
        self.pending_entropy = Some(crate::zjh::fairness::Pending::for_cards(
            deal,
            self.proof_owners.clone(),
            expected,
        ));
        Err("entropy_pending")
    }
    fn take(&mut self, seat: usize) -> Option<u8> {
        if self.deck.is_empty() && self.discard.len() > 1 {
            let top = self.discard.pop()?;
            let proof = self.next_proof.take()?;
            self.deck = proof.cards.iter().map(|c| *c as u8).collect();
            self.proof_positions = (0..self.deck.len()).collect();
            self.proofs.insert(
                proof.secret.context.epoch as usize,
                ProofEpoch {
                    deal: Arc::new(proof),
                    public: Vec::new(),
                    grants: vec![Vec::new(); self.hands.len()],
                },
            );
            self.discard.clear();
            self.discard.push(top);
        }
        let card = self.deck.pop()?;
        if let Some((&epoch_id, epoch)) = self.proofs.iter_mut().next_back() {
            let position = self.proof_positions.pop()?;
            epoch.grants[seat].push(position);
            Arc::make_mut(&mut self.proof_origins).insert(card, (epoch_id, position));
        }
        Some(card)
    }
    fn finish(&mut self, winner: Option<usize>, reason: &str) {
        self.finished = true;
        self.winner = winner;
        self.reason = reason.into();
        self.has_drawn = false;
        self.drawn = None;
        self.points = winner.map_or(0, |w| {
            self.hands
                .iter()
                .enumerate()
                .filter(|(s, _)| *s != w)
                .flat_map(|(_, h)| h)
                .map(|c| points(*c))
                .sum()
        });
    }
    pub fn play(
        &mut self,
        seat: usize,
        card: u8,
        chosen: Option<Color>,
    ) -> Result<(), &'static str> {
        self.check_turn(seat)?;
        let hand = &self.hands[seat];
        if !hand.contains(&card) || !self.playable_in_hand(hand, card) {
            return Err("illegal_card");
        }
        let new_color = if card >= 100 {
            chosen.ok_or("choose_color")?
        } else {
            if chosen.is_some() {
                return Err("unexpected_color");
            }
            color(card).unwrap()
        };
        let draw_count = match rank(card) {
            12 => 2,
            14 => 4,
            _ => 0,
        };
        if let Err(e) = self.prepare_draw(draw_count, true) {
            if e == "entropy_pending" {
                self.deferred_draw = Some((seat, Some((card, chosen))));
            }
            return Err(e);
        }
        let index = self.hands[seat].iter().position(|c| *c == card).unwrap();
        self.hands[seat].remove(index);
        self.discard.push(card);
        if let Some(&(epoch, position)) = self.proof_origins.get(&card) {
            if let Some(proof) = self.proofs.get_mut(&epoch) {
                proof.public.push(position);
            }
        }
        self.current_color = new_color;
        self.stalled = 0;
        let r = rank(card);
        if r == 11 {
            self.direction = -self.direction;
        }
        self.next();
        if r == 12 || r == 14 {
            for _ in 0..if r == 12 { 2 } else { 4 } {
                if let Some(c) = self.take(self.turn) {
                    self.hands[self.turn].push(c);
                }
            }
            self.next();
        } else if r == 10 || (r == 11 && self.active() == 2) {
            self.next();
        }
        // A final action card's penalty is included in the round score.
        if self.hands[seat].is_empty() {
            self.finish(Some(seat), "empty_hand");
        }
        self.touched = Touched {
            everyone: true,
            ..Touched::default()
        };
        self.revision += 1;
        Ok(())
    }
    pub fn draw(&mut self, seat: usize) -> Result<(), &'static str> {
        self.check_turn(seat)?;
        if self.has_drawn {
            return Err("already_drew");
        }
        if let Err(e) = self.prepare_draw(1, false) {
            if e == "entropy_pending" {
                self.deferred_draw = Some((seat, None));
            }
            return Err(e);
        }
        self.drawn = self.take(seat);
        if let Some(c) = self.drawn {
            self.hands[seat].push(c);
            self.stalled = 0;
        } else {
            self.stalled += 1;
        }
        self.has_drawn = true;
        self.touched = Touched::seats(&[seat]);
        self.revision += 1;
        Ok(())
    }
    pub fn pass(&mut self, seat: usize) -> Result<(), &'static str> {
        self.check_turn(seat)?;
        if !self.has_drawn {
            return Err("draw_first");
        }
        self.next();
        if self.stalled >= self.active() {
            self.finish(None, "blocked");
        }
        self.touched = Touched::seats(&[seat, self.turn]);
        self.revision += 1;
        Ok(())
    }
    pub fn entropy_waiting(&self) -> bool {
        self.deferred_draw.is_some() && !self.finished
    }
    pub fn advance_entropy(&mut self) {
        if let Some(pending) = &mut self.pending_entropy {
            pending.advance();
        }
        if self
            .pending_entropy
            .as_ref()
            .is_some_and(|p| p.phase != crate::zjh::fairness::Phase::Ready)
        {
            return;
        }
        if let Some((seat, play)) = self.deferred_draw.take() {
            if let Some((card, color)) = play {
                let _ = self.play(seat, card, color);
            } else {
                let _ = self.draw(seat);
            }
        }
    }
    pub fn resign(&mut self, seat: usize) {
        if self.finished || seat >= self.resigned.len() || self.resigned[seat] {
            return;
        }
        let cancelled = self.pending_entropy.is_some() || self.deferred_draw.is_some();
        self.pending_entropy = None;
        self.deferred_draw = None;
        self.next_proof = None;
        self.resigned[seat] = true;
        self.stalled = 0;
        if self.active() <= 1 {
            self.finish(self.resigned.iter().position(|r| !*r), "resign");
        } else if self.turn == seat {
            self.next();
        }
        self.touched = Touched {
            entropy: cancelled,
            ..Touched::seats(&[seat, self.turn])
        };
        self.revision += 1;
    }
    pub fn abandon(&mut self) {
        if self.finished {
            return;
        }
        self.finish(None, "abandoned");
        self.stalled = 0;
        self.revision += 1;
    }
    pub fn automatic(&mut self) {
        if self.check_turn(self.turn).is_err() {
            return;
        }
        let seat = self.turn;
        let candidate = self.hands[seat]
            .iter()
            .copied()
            .filter(|c| self.playable_in_hand(&self.hands[seat], *c))
            .max_by_key(|c| (u8::from(*c < 100), points(*c)));
        if let Some(c) = candidate {
            let chosen = (c >= 100).then(|| {
                *COLORS
                    .iter()
                    .max_by_key(|col| {
                        self.hands[seat]
                            .iter()
                            .filter(|c| color(**c) == Some(**col))
                            .count()
                    })
                    .unwrap()
            });
            let _ = self.play(seat, c, chosen);
        } else if self.has_drawn {
            let _ = self.pass(seat);
        } else {
            let _ = self.draw(seat);
        }
    }
    pub fn view(&self, seat: usize) -> Value {
        self.projection(Some(seat))
    }
    /// What a spectator sees: the top card and colour, direction, whose turn,
    /// every hand's card count, points and the result; never a hand, the
    /// deck, the drawn card or legal plays.
    pub fn public_view(&self) -> Value {
        self.projection(None)
    }
    fn projection(&self, viewer: Option<usize>) -> Value {
        let mut view = self.shared_view();
        view.as_object_mut()
            .unwrap()
            .extend(self.viewer_fields(viewer));
        view
    }

    pub(super) fn shared_view(&self) -> Value {
        let mut view = json!({"id":self.id,"card_proof":self.proofs.values().next_back().map(|p| &p.deal.commitment),"revision":self.revision,"turn":self.turn,
            "finished":self.finished,"winner":self.winner,"reason":self.reason,"points":self.points,
            "resigned":self.resigned,"counts":self.hands.iter().map(Vec::len).collect::<Vec<_>>(),
            "color":self.current_color,"direction":self.direction});
        view["top"] = self.discard.last().copied().map_or(Value::Null, card_view);
        view
    }

    /// The shared body of an ordinary move; `None` when the move was a
    /// boundary for every frame (a cancelled entropy draw).
    pub(super) fn move_event(&self) -> Option<MoveEvent<'_>> {
        (!self.touched.entropy).then(|| MoveEvent {
            revision: self.revision,
            turn: self.turn,
            resigned: &self.resigned,
            counts: self.hands.iter().map(Vec::len).collect(),
            color: self.current_color,
            direction: self.direction,
            top: self.discard.last().copied().map(CardView::of),
        })
    }

    /// Whether the last move changed this viewer's private `match` fields
    /// (`None` = spectators, who see only the top card's proof view).
    pub(super) fn move_touched(&self, viewer: Option<usize>) -> bool {
        self.touched.everyone || viewer.is_some_and(|s| self.touched.seats & (1 << s) != 0)
    }

    pub(super) fn viewer_fields(&self, viewer: Option<usize>) -> serde_json::Map<String, Value> {
        let hand = viewer
            .and_then(|seat| self.hands.get(seat))
            .map(Vec::as_slice)
            .unwrap_or_default();
        let acting = viewer.is_some_and(|seat| self.check_turn(seat).is_ok());
        let legal = if acting {
            hand.iter()
                .copied()
                .filter(|&card| self.playable_in_hand(hand, card))
                .collect::<Vec<_>>()
        } else {
            Vec::new()
        };
        let mut visible = std::collections::BTreeMap::<usize, Vec<u16>>::new();
        for card in hand.iter().chain(self.discard.last()) {
            if let Some(&(epoch, _)) = self.proof_origins.get(card) {
                visible.entry(epoch).or_default().push(u16::from(*card));
            }
        }
        let proof_views: Vec<_> = visible
            .into_iter()
            .filter_map(|(epoch, cards)| {
                self.proofs
                    .get(&epoch)
                    .map(|p| json!({"commitment":p.deal.commitment,"cards":cards}))
            })
            .collect();
        let mut fields = serde_json::Map::new();
        fields.insert("legal".into(), json!(legal));
        fields.insert("can_draw".into(), json!(acting && !self.has_drawn));
        fields.insert("can_pass".into(), json!(acting && self.has_drawn));
        fields.insert(
            "drawn".into(),
            json!(if acting { self.drawn } else { None }),
        );
        fields.insert("card_proof_views".into(), Value::Array(proof_views));
        fields.insert(
            "hand".into(),
            Value::Array(hand.iter().copied().map(card_view).collect()),
        );
        fields
    }
    pub fn valid(&self) -> bool {
        let n = self.hands.len();
        if !(2..=6).contains(&n)
            || self.resigned.len() != n
            || self.turn >= n
            || ![-1, 1].contains(&self.direction)
            || self.discard.is_empty()
            || self.stalled > self.active()
            || self.winner.is_some_and(|w| w >= n || self.resigned[w])
        {
            return false;
        }
        let mut seen = [false; 108];
        for c in self
            .hands
            .iter()
            .flatten()
            .chain(&self.deck)
            .chain(&self.discard)
        {
            if *c >= 108 || seen[*c as usize] {
                return false;
            }
            seen[*c as usize] = true;
        }
        if seen.iter().any(|s| !s) {
            return false;
        }
        let top = *self.discard.last().unwrap();
        if color(top).is_some_and(|c| c != self.current_color)
            || (!self.has_drawn && self.drawn.is_some())
            || self
                .drawn
                .is_some_and(|c| !self.hands[self.turn].contains(&c))
        {
            return false;
        }
        if !self.finished {
            return self.active() >= 2
                && !self.resigned[self.turn]
                && self.winner.is_none()
                && self.reason.is_empty()
                && self.points == 0
                && self.hands.iter().all(|h| !h.is_empty())
                && (!self.has_drawn
                    || self.drawn.is_some()
                    || (self.deck.is_empty() && self.discard.len() == 1));
        }
        if self.has_drawn
            || self.drawn.is_some()
            || self.hands.iter().enumerate().any(|(seat, h)| {
                h.is_empty() && (self.reason != "empty_hand" || self.winner != Some(seat))
            })
        {
            return false;
        }
        let expected = self.winner.map_or(0, |w| {
            self.hands
                .iter()
                .enumerate()
                .filter(|(s, _)| *s != w)
                .flat_map(|(_, h)| h)
                .map(|c| points(*c))
                .sum()
        });
        self.points == expected
            && match self.reason.as_str() {
                "empty_hand" => {
                    self.active() >= 2 && self.winner.is_some_and(|w| self.hands[w].is_empty())
                }
                "resign" => self.active() == 1 && self.winner.is_some(),
                "abandoned" => self.winner.is_none() && self.stalled == 0,
                "blocked" => {
                    self.active() >= 2
                        && self.winner.is_none()
                        && self.stalled >= self.active()
                        && self.deck.is_empty()
                        && self.discard.len() == 1
                }
                _ => false,
            }
    }
}

/// Private state for each shuffle epoch. Recycled cards keep their physical id,
/// while an origin records exactly which ciphertext supplied each observed draw.
#[derive(Clone, Serialize, Deserialize)]
pub struct ProofEpoch {
    pub deal: Arc<crate::card_proofs::Deal>,
    pub public: Vec<usize>,
    pub grants: Vec<Vec<usize>>,
}
#[cfg(test)]
pub(crate) mod colorcards_frame_tests {
    use super::*;
    use mental_poker::card_proof::{Commitment, Context, Game, Secret, SCHEME};

    // Supply the real constructor a fixed physical deck so the next draw and
    // the seats' proof grants are reproducible without starting a shuffle worker.
    pub(crate) fn fixture(red_in_hand: bool, next_draw: u16) -> Match {
        let host = [if red_in_hand { 1 } else { 31 }, 26, 53, 75, 100, 104, 60];
        let guest = [2, 5, 12, 34, 58, 78, 103];
        let deal_order: Vec<u16> = host
            .into_iter()
            .zip(guest)
            .flat_map(|(h, g)| [h, g])
            .collect();
        let mut cards = vec![0]; // Public red zero.
        cards.extend((1..108).filter(|c| *c != next_draw && !deal_order.contains(c)));
        cards.push(next_draw);
        cards.extend(deal_order.into_iter().rev());
        let context = Context {
            game: Game::Colorcards,
            hand_id: Uuid::from_u128(1),
            seats: vec![0, 1],
            roster: "0".repeat(64),
            epoch: 0,
            previous: None,
            subset: None,
        };
        let deal = crate::card_proofs::Deal {
            secret: Secret {
                context: context.clone(),
                master: [0; 32],
                client_seeds: Default::default(),
            },
            commitment: Commitment {
                scheme: SCHEME.into(),
                context,
                commitment: "0".repeat(64),
            },
            cards,
            commitments: None,
        };
        let m = Match::new(2, 0, deal, Default::default()).unwrap();
        assert!(m.valid());
        m
    }

    fn check_plays(m: &Match, expected: &[u8]) {
        assert_eq!(m.view(0)["legal"], json!(expected));
        for card in 0..108 {
            let mut copy = m.clone();
            let chosen = (card >= 100).then_some(Color::Blue);
            assert_eq!(
                copy.play(0, card, chosen).is_ok(),
                expected.contains(&card),
                "card {card}"
            );
        }
    }

    #[test]
    fn drawn_card_must_be_held_visible_to_owner_and_legal_for_its_color() {
        let mut m = fixture(true, 6);
        check_plays(&m, &[1, 75, 100]);
        assert_eq!(m.play(0, 2, None), Err("illegal_card")); // Guest's matching red card.
        assert_eq!(m.play(1, 2, None), Err("not_your_turn"));
        assert_eq!(m.play(99, 2, None), Err("not_your_turn"));
        assert_eq!(m.pass(0), Err("draw_first"));
        assert_eq!(m.view(0)["can_draw"], true);
        m.draw(0).unwrap();
        assert_eq!(m.draw(0), Err("already_drew"));
        assert_eq!(m.view(0)["drawn"], 6);
        assert_eq!(m.view(0)["can_pass"], true);
        check_plays(&m, &[6]);
        for view in [m.view(1), m.public_view()] {
            assert_eq!(view["legal"], json!([]));
            assert_eq!(view["can_draw"], false);
            assert_eq!(view["can_pass"], false);
            assert!(view["drawn"].is_null());
            let cards = view["card_proof_views"][0]["cards"].as_array().unwrap();
            assert!(!cards.contains(&json!(6)));
        }
        assert_eq!(m.public_view()["hand"], json!([]));
        m.pass(0).unwrap();
        assert_eq!(m.turn, 1);
        assert_eq!(m.view(0)["legal"], json!([]));
    }

    #[test]
    fn later_draw_and_play_preserve_frozen_proof_grants_and_origins() {
        let mut live = fixture(true, 6);
        let frozen = live.clone();
        let checkpoint = json!(frozen);
        assert!(Arc::ptr_eq(&frozen.proof_origins, &live.proof_origins));
        live.draw(0).unwrap();
        assert!(!Arc::ptr_eq(&frozen.proof_origins, &live.proof_origins));
        assert!(!frozen.proof_origins.contains_key(&6));
        let (epoch, position) = live.proof_origins[&6];
        assert!(Arc::ptr_eq(
            &frozen.proofs[&epoch].deal,
            &live.proofs[&epoch].deal
        ));
        assert!(live.proofs[&epoch].grants[0].contains(&position));
        live.play(0, 6, None).unwrap();
        assert!(live.proofs[&epoch].public.contains(&position));
        assert_eq!(json!(frozen), checkpoint);
        let restored: Match = serde_json::from_value(checkpoint).unwrap();
        assert_eq!(restored.view(0), frozen.view(0));
        assert_eq!(restored.public_view(), frozen.public_view());
    }

    #[test]
    fn wild_four_requires_no_matching_color_even_when_drawn() {
        let mut blocked = fixture(true, 105);
        blocked.draw(0).unwrap();
        check_plays(&blocked, &[]);
        assert_eq!(blocked.play(0, 105, Some(Color::Blue)), Err("illegal_card"));
        assert_eq!(blocked.view(0)["can_pass"], true);

        let mut allowed = fixture(false, 105);
        check_plays(&allowed, &[75, 100, 104]);
        allowed.draw(0).unwrap();
        check_plays(&allowed, &[105]);
        assert_eq!(allowed.play(0, 105, None), Err("choose_color"));
        let before = allowed.hands[1].len();
        allowed.play(0, 105, Some(Color::Green)).unwrap();
        assert_eq!(allowed.hands[1].len(), before + 4);
        assert_eq!(allowed.current_color, Color::Green);
        assert_eq!(allowed.turn, 0);
        assert_eq!(allowed.view(0)["can_draw"], true);
        assert!(allowed.valid());
    }
}
