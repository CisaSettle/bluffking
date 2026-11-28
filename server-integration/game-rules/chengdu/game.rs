use super::rules::{self, Meld, MeldKind, Score, Tile};
use crate::game_hub::board::BoardMatchSeat;
use crate::mahjong::event::{self, Moved, SeatTile};
use crate::mahjong::{
    copies, remove_tiles, ClaimWindow, Decided, Resolution, TileMatch, TileSeat, Transfer,
};
use rand::seq::SliceRandom;
use serde::{Deserialize, Serialize};
use serde_json::{json, value::RawValue, Value};
use uuid::Uuid;
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "t", rename_all = "snake_case")]
pub enum Action {
    Missing { suit: u8 },
    Discard { tile: Tile },
    Pung,
    Kong { tile: Tile },
    Hu,
    Pass,
}
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct Seat {
    pub tiles: Vec<Tile>,
    pub melds: Vec<Meld>,
    pub discards: Vec<Tile>,
    pub missing: Option<u8>,
    pub won: bool,
    pub passed_hu: bool,
}
impl TileSeat for Seat {
    fn hand(&mut self) -> (&mut Vec<Tile>, &mut Vec<Meld>, &mut Vec<Tile>) {
        (&mut self.tiles, &mut self.melds, &mut self.discards)
    }
}
impl Seat {
    /// A chosen suit restricts discards only while the hand still contains it.
    fn required_discard_suit(&self) -> Option<u8> {
        self.missing
            .filter(|&suit| self.tiles.iter().any(|&t| t / 9 == suit))
    }
}

crate::mahjong::claim_phase!(4; Missing);
pub type Winner = crate::mahjong::Winner<Score>;
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Match {
    pub id: Uuid,
    pub revision: u64,
    pub seats: [Seat; 4],
    pub wall: Vec<Tile>,
    pub phase: Phase,
    pub dealer: usize,
    pub finished: bool,
    pub deltas: [i64; 4],
    pub transfers: Vec<Transfer>,
    pub winners: Vec<Winner>,
    pub reason: String,
    /// The last ordinary move's record for its event; never checkpointed.
    #[serde(skip)]
    pub(crate) moved: Option<Moved<[usize; 2]>>,
}
impl Match {
    pub fn new(players: usize, dealer: usize) -> Option<Self> {
        if players != 4 || dealer >= 4 {
            return None;
        }
        let mut wall: Vec<_> = (0..27).flat_map(|t| [t; 4]).collect();
        wall.shuffle(&mut rand::thread_rng());
        let mut seats: [Seat; 4] = Default::default();
        for s in &mut seats {
            s.tiles = wall.split_off(wall.len() - 13);
            s.tiles.sort_unstable();
        }
        Some(Self {
            id: Uuid::new_v4(),
            revision: 0,
            seats,
            wall,
            phase: Phase::Missing,
            dealer,
            finished: false,
            deltas: [0; 4],
            transfers: vec![],
            winners: vec![],
            reason: String::new(),
            moved: None,
        })
    }
    pub fn valid(&self) -> bool {
        self.dealer < 4
            && self.deltas.iter().sum::<i64>() == 0
            && self.finished == matches!(self.phase, Phase::End)
            && self
                .seats
                .iter()
                .all(|s| s.tiles.iter().all(|&t| t < 27) && s.missing.is_none_or(|m| m < 3))
            && self.wall.iter().all(|&t| t < 27)
            && match &self.phase {
                Phase::Turn { seat, .. } => *seat < 4,
                Phase::Claims(w) => w.from < 4 && w.tile < 27,
                _ => true,
            }
    }
    fn active(&self, s: usize) -> bool {
        s < 4 && !self.seats[s].won
    }
    fn score_for(
        &self,
        s: usize,
        t: Tile,
        from: Option<usize>,
        kong: bool,
        rob: bool,
    ) -> Option<Score> {
        if !self.active(s) || (from.is_some() && self.seats[s].passed_hu) {
            return None;
        }
        let seat = &self.seats[s];
        let mut tiles = seat.tiles.clone();
        if from.is_some() {
            tiles.push(t)
        }
        let mut score = rules::score(&tiles, &seat.melds, seat.missing?)?;
        if kong || rob {
            score.multiplier = (score.multiplier * 2).min(16);
            score.patterns.push(
                if rob {
                    "rob_kong"
                } else if from.is_none() {
                    "kong_draw"
                } else {
                    "kong_discard"
                }
                .into(),
            );
        }
        Some(score)
    }
    fn claims_for(&self, s: usize, from: usize, t: Tile, rob: bool, kong: bool) -> Vec<Action> {
        if !self.active(s) || s == from {
            return vec![];
        }
        let h = &self.seats[s];
        let mut a = vec![];
        if self.score_for(s, t, Some(from), kong, rob).is_some() {
            a.push(Action::Hu)
        }
        if !rob
            && !self.wall.is_empty()
            && h.missing != Some(t / 9)
            && !h.tiles.iter().any(|t| Some(t / 9) == h.missing)
        {
            let n = copies(&h.tiles, t);
            if n >= 2 {
                a.push(Action::Pung)
            }
            if n == 3 {
                a.push(Action::Kong { tile: t })
            }
        }
        if !a.is_empty() {
            a.push(Action::Pass)
        }
        a
    }
    pub fn legal(&self, s: usize) -> Vec<Action> {
        if !self.active(s) || self.finished {
            return vec![];
        }
        let h = &self.seats[s];
        match &self.phase {
            Phase::Missing => {
                if h.missing.is_none() {
                    (0..3).map(|suit| Action::Missing { suit }).collect()
                } else {
                    vec![]
                }
            }
            Phase::End => vec![],
            Phase::Claims(w) => {
                if w.choices[s].is_none() {
                    self.claims_for(s, w.from, w.tile, w.rob, w.kong)
                } else {
                    vec![]
                }
            }
            Phase::Turn { seat, drawn, kong } => {
                if *seat != s {
                    return vec![];
                }
                let mut a = vec![];
                if drawn.is_some_and(|t| self.score_for(s, t, None, *kong, false).is_some()) {
                    a.push(Action::Hu)
                }
                let required_suit = h.required_discard_suit();
                let mut unique = h.tiles.clone();
                unique.sort_unstable();
                unique.dedup();
                for t in unique {
                    if required_suit.is_none_or(|suit| t / 9 == suit) {
                        a.push(Action::Discard { tile: t })
                    }
                    if required_suit.is_none()
                        && drawn.is_some()
                        && !self.wall.is_empty()
                        && (copies(&h.tiles, t) == 4
                            || h.melds
                                .iter()
                                .any(|m| m.tile == t && m.kind == MeldKind::Pung))
                    {
                        a.push(Action::Kong { tile: t })
                    }
                }
                a
            }
        }
    }
    /// The house bot's (and auto-play's) choice for `s`, from its own tiles
    /// and public information only; see `bot.rs`.
    pub fn automatic_action(&self, s: usize) -> Option<Action> {
        super::bot::decide(self, s)
    }
    /// Plays one move; a refused move changes nothing.
    pub fn act(&mut self, s: usize, a: Action) -> Result<(), &'static str> {
        crate::mahjong::all_or_nothing(self, |game| game.apply(s, a))
    }
    fn apply(&mut self, s: usize, a: Action) -> Result<(), &'static str> {
        let legal = if let Action::Discard { tile } = &a {
            if self.finished
                || !self.active(s)
                || !matches!(self.phase, Phase::Turn { seat, .. } if seat == s)
            {
                return Err("illegal_action");
            }
            let hand = &self.seats[s];
            if !hand.tiles.contains(tile)
                || hand
                    .required_discard_suit()
                    .is_some_and(|suit| tile / 9 != suit)
            {
                return Err("illegal_action");
            }
            Vec::new()
        } else {
            let legal = self.legal(s);
            if !legal.contains(&a) {
                return Err("illegal_action");
            }
            legal
        };
        match self.phase.clone() {
            Phase::Missing => {
                if let Action::Missing { suit } = a {
                    self.seats[s].missing = Some(suit);
                }
                if self.seats.iter().all(|s| s.missing.is_some()) {
                    self.draw(self.dealer, false)
                }
            }
            Phase::Turn { drawn, kong, .. } => match a {
                Action::Hu => self.win(vec![s], drawn.unwrap(), None, kong, false),
                Action::Discard { tile } => {
                    remove_tiles(&mut self.seats[s].tiles, tile, 1)?;
                    self.seats[s].discards.push(tile);
                    self.claims(s, tile, false, kong)?
                }
                Action::Kong { tile } => {
                    if copies(&self.seats[s].tiles, tile) == 4 {
                        self.kong(s, tile, s, MeldKind::ConcealedKong)?
                    } else {
                        self.claims(s, tile, true, false)?
                    }
                }
                _ => return Err("illegal_action"),
            },
            Phase::Claims(_) => {
                if a != Action::Hu && legal.contains(&Action::Hu) {
                    self.seats[s].passed_hu = true
                }
                if let Phase::Claims(w) = &mut self.phase {
                    w.choices[s] = Some(a)
                }
                self.resolve()?
            }
            Phase::End => return Err("illegal_action"),
        }
        self.revision += 1;
        Ok(())
    }
    fn claims(
        &mut self,
        from: usize,
        tile: Tile,
        rob: bool,
        kong: bool,
    ) -> Result<(), &'static str> {
        let can_claim = |s| !self.claims_for(s, from, tile, rob, kong).is_empty();
        self.phase = Phase::Claims(ClaimWindow::open(from, tile, rob, kong, can_claim));
        self.resolve()
    }
    fn resolve(&mut self) -> Result<(), &'static str> {
        let Some(Decided {
            from,
            tile,
            rob,
            kong,
            resolution,
        }) = self.phase.decided()
        else {
            return Ok(());
        };
        match resolution {
            Resolution::Win(wins) => {
                if rob {
                    self.seats[from].robbed(tile)?;
                }
                self.win(wins, tile, Some(from), kong, rob)
            }
            Resolution::KongStands => self.kong(from, tile, from, MeldKind::AddedKong)?,
            Resolution::Claim { seat, kong: true } => {
                // The pile gives the tile up before the replacement draw.
                self.seats[from].discards.pop();
                self.kong(seat, tile, from, MeldKind::ExposedKong)?
            }
            Resolution::Claim { seat, kong: false } => {
                self.seats[seat].pung(tile, from)?;
                self.seats[from].discards.pop();
                self.phase = Phase::after_pung(seat);
            }
            Resolution::Pass => self.draw(self.next(from), false),
        }
        Ok(())
    }
    fn next(&self, from: usize) -> usize {
        (1..=4)
            .map(|n| (from + n) % 4)
            .find(|&s| self.active(s))
            .unwrap_or(from)
    }
    fn pay(&mut self, from: usize, to: usize, amount: i64, kind: &str) {
        self.deltas[from] -= amount;
        self.deltas[to] += amount;
        self.transfers.push(Transfer {
            from,
            to,
            amount,
            kind: kind.into(),
        })
    }
    fn kong(
        &mut self,
        s: usize,
        tile: Tile,
        from: usize,
        kind: MeldKind,
    ) -> Result<(), &'static str> {
        self.seats[s].kong_meld(tile, from, kind)?;
        if kind == MeldKind::ExposedKong {
            self.pay(from, s, 2, "exposed_kong")
        } else {
            for other in 0..4 {
                if other != s && self.active(other) {
                    self.pay(
                        other,
                        s,
                        if kind == MeldKind::AddedKong { 1 } else { 2 },
                        if kind == MeldKind::AddedKong {
                            "added_kong"
                        } else {
                            "concealed_kong"
                        },
                    )
                }
            }
        }
        self.draw(s, true);
        Ok(())
    }
    fn draw(&mut self, s: usize, kong: bool) {
        if let Some(tile) = self.wall.pop() {
            self.seats[s].tiles.push(tile);
            self.seats[s].tiles.sort_unstable();
            self.seats[s].passed_hu = false;
            self.phase = Phase::Turn {
                seat: s,
                drawn: Some(tile),
                kong,
            };
        } else {
            self.exhausted()
        }
    }
    fn win(&mut self, winners: Vec<usize>, tile: Tile, from: Option<usize>, kong: bool, rob: bool) {
        let scores: Vec<_> = winners
            .iter()
            .map(|&s| self.score_for(s, tile, from, kong, rob).expect("legal win"))
            .collect();
        for (&s, score) in winners.iter().zip(scores) {
            for loser in 0..4 {
                if loser != s && self.active(loser) && from.is_none_or(|f| f == loser) {
                    // Fixed profile: self draw adds one point after the hand-type cap.
                    self.pay(
                        loser,
                        s,
                        i64::from(score.multiplier) + i64::from(from.is_none()),
                        if from.is_none() {
                            "self_draw"
                        } else if rob {
                            "rob_kong"
                        } else {
                            "discard_win"
                        },
                    );
                }
            }
            self.seats[s].won = true;
            self.winners.push(Winner {
                seat: s,
                tile,
                score,
                self_draw: from.is_none(),
            });
        }
        if self.seats.iter().filter(|s| !s.won).count() <= 1 {
            self.finish("three_winners")
        } else {
            self.draw(
                self.next(if winners.len() > 1 {
                    from.unwrap()
                } else {
                    winners[0]
                }),
                false,
            )
        }
    }
    fn exhausted(&mut self) {
        let pig: [bool; 4] = std::array::from_fn(|s| {
            self.active(s)
                && self.seats[s]
                    .tiles
                    .iter()
                    .any(|t| Some(t / 9) == self.seats[s].missing)
        });
        let waits: [i64; 4] = std::array::from_fn(|s| {
            if self.active(s) && !pig[s] {
                rules::waits(
                    &self.seats[s].tiles,
                    &self.seats[s].melds,
                    self.seats[s].missing.unwrap(),
                )
                .iter()
                .map(|(_, v)| i64::from(v.multiplier))
                .max()
                .unwrap_or(0)
            } else {
                0
            }
        });
        // Refund actual kong transfers for non-listening, still-active recipients.
        let refunds: Vec<_> = self
            .transfers
            .iter()
            .filter(|t| {
                self.active(t.to)
                    && waits[t.to] == 0
                    && matches!(
                        t.kind.as_str(),
                        "exposed_kong" | "concealed_kong" | "added_kong"
                    )
            })
            .cloned()
            .collect();
        for t in refunds {
            self.pay(t.to, t.from, t.amount, "kong_refund")
        }
        for a in 0..4 {
            if pig[a] {
                for (b, &b_pig) in pig.iter().enumerate() {
                    if a != b && !b_pig {
                        self.pay(a, b, 16, "flower_pig")
                    }
                }
            } else if self.active(a) && waits[a] == 0 {
                for (b, &b_waits) in waits.iter().enumerate() {
                    if b_waits > 0 {
                        self.pay(a, b, b_waits, "not_ready")
                    }
                }
            }
        }
        self.finish("wall_exhausted")
    }
    fn finish(&mut self, reason: &str) {
        self.finished = true;
        self.phase = Phase::End;
        self.reason = reason.into()
    }
    /// Finish unattended hands so leaving cannot evade exhaustion penalties.
    pub fn autoplay_to_end(&mut self) {
        // At most 108 draws, three claims per discard and four missing choices.
        for _ in 0..1024 {
            if self.finished {
                return;
            }
            let Some(seat) = (0..4).find(|&s| !self.legal(s).is_empty()) else {
                return;
            };
            let Some(action) = self.automatic_action(seat) else {
                return;
            };
            if self.act(seat, action).is_err() {
                return;
            }
        }
    }
    pub fn view(&self, seat: usize) -> Value {
        event::view(self, self.public_view(), seat)
    }

    pub fn match_seat(&self, seat: usize) -> Option<BoardMatchSeat> {
        event::match_seat(self, seat)
    }

    /// `(phase, turn, discard)` of the public match.
    fn phase_view(&self) -> (&'static str, Option<usize>, Option<SeatTile>) {
        match &self.phase {
            Phase::Missing => ("missing", None, None),
            Phase::End => ("end", None, None),
            Phase::Turn { seat, .. } => ("turn", Some(*seat), None),
            Phase::Claims(w) => (
                if w.rob { "rob_kong" } else { "claims" },
                None,
                Some(SeatTile {
                    seat: w.from,
                    tile: w.tile,
                }),
            ),
        }
    }

    /// What a spectator sees: discards, open melds, counts, the turn, chosen
    /// missing suits once revealed, winners' hands and settlement; never a
    /// concealed hand or kong, the drawn tile, the wall or legal moves.
    pub fn public_view(&self) -> Value {
        let (phase, turn, discard) = self.phase_view();
        let mut view = json!({"id":self.id,"revision":self.revision,"phase":phase,"turn":turn,"discard":discard,"drawn":null,"finished":self.finished,"legal":[],"remaining":self.wall.len(),"deltas":self.deltas,"transfers":self.transfers,"winners":self.winners,"reason":self.reason});
        view["seats"] = event::public_rows(self);
        view
    }

    /// The public body of the ordinary move applied to revision `before`:
    /// every public match field it can change. `finished` and `reason` only
    /// change when the match ends, which is a boundary (full frames).
    pub fn move_event(&self, before: u64) -> Option<Box<RawValue>> {
        let (moved, seats) = event::moved_since(self, before)?;
        let [transfers, winners] = moved.lists;
        let (phase, turn, discard) = self.phase_view();
        let transfers_tail = event::tail(&self.transfers, transfers)?;
        let winners_tail = event::tail(&self.winners, winners)?;
        serde_json::value::to_raw_value(&MoveEvent {
            revision: self.revision,
            phase,
            turn,
            discard,
            remaining: self.wall.len(),
            deltas: self.deltas,
            transfers_from: transfers_tail.map(|_| transfers),
            transfers: transfers_tail,
            winners_from: winners_tail.map(|_| winners),
            winners: winners_tail,
            seats,
        })
        .ok()
    }

    pub fn move_private(&self, seat: Option<usize>) -> Option<Box<RawValue>> {
        event::move_private(self, seat)
    }
}

impl TileMatch for Match {
    crate::mahjong::tile_match_fields!();
    type Lists = [usize; 2];
    type Row<'a> = SeatRow<'a>;
    type Claims<'a> = ();
    /// `tiles` only for the seat itself, a winner or a finished match; the
    /// missing suit of others only once 定缺 is over.
    fn row(&self, seat: usize, own: bool) -> SeatRow<'_> {
        let player = &self.seats[seat];
        let visible = own || player.won || self.finished;
        SeatRow {
            tiles: visible.then_some(&player.tiles),
            count: player.tiles.len(),
            discards: &player.discards,
            missing: if own || !matches!(self.phase, Phase::Missing) {
                player.missing
            } else {
                None
            },
            chosen: player.missing.is_some(),
            won: player.won,
            melds: event::meld_views(
                player.melds.iter().map(|m| (m.tile, m.kind, m.from)),
                visible,
                MeldKind::ConcealedKong,
            ),
        }
    }
    fn drawn_tile(&self, seat: usize) -> Option<Tile> {
        self.phase.drawn_for(seat)
    }
    fn may_act(&self, s: usize) -> bool {
        match &self.phase {
            Phase::Missing => self.seats[s].missing.is_none(),
            phase => phase.deciding(s),
        }
    }
    fn row_mode(&self) -> bool {
        matches!(self.phase, Phase::Missing)
    }
    fn lists(&self) -> [usize; 2] {
        [self.transfers.len(), self.winners.len()]
    }
}

#[derive(Serialize)]
pub struct SeatRow<'a> {
    tiles: Option<&'a Vec<Tile>>,
    count: usize,
    discards: &'a [Tile],
    missing: Option<u8>,
    chosen: bool,
    won: bool,
    melds: Vec<event::MeldView<MeldKind>>,
}

#[derive(Serialize)]
struct MoveEvent<'a> {
    revision: u64,
    phase: &'static str,
    turn: Option<usize>,
    discard: Option<SeatTile>,
    remaining: usize,
    deltas: [i64; 4],
    #[serde(skip_serializing_if = "Option::is_none")]
    transfers_from: Option<usize>,
    #[serde(skip_serializing_if = "Option::is_none")]
    transfers: Option<&'a [Transfer]>,
    #[serde(skip_serializing_if = "Option::is_none")]
    winners_from: Option<usize>,
    #[serde(skip_serializing_if = "Option::is_none")]
    winners: Option<&'a [Winner]>,
    #[serde(flatten)]
    seats: event::SeatRows<SeatRow<'a>>,
}
