//! Independent four-player Guiyang state machine. No I/O; wall and hidden hands
//! never enter a player's wire view. Settlement is one additive, zero-sum ledger.
use super::rules::{self, Meld, MeldKind, Score, Tile, CHICKEN};
use crate::game_hub::board::BoardMatchSeat;
use crate::mahjong::event::{self, Moved, SeatTile};
use crate::mahjong::wall::WallDraws;
use crate::mahjong::{
    copies, remove_tiles, ClaimWindow, Decided, Resolution, TileMatch, TileSeat, Transfer,
};
#[cfg(test)]
use rand::{seq::SliceRandom, Rng};
use serde::{Deserialize, Serialize};
use serde_json::{json, value::RawValue, Value};
use uuid::Uuid;
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "t", rename_all = "snake_case")]
pub enum Action {
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
    passed_hu: bool,
    passed_pung: Vec<Tile>,
}
impl TileSeat for Seat {
    fn hand(&mut self) -> (&mut Vec<Tile>, &mut Vec<Meld>, &mut Vec<Tile>) {
        (&mut self.tiles, &mut self.melds, &mut self.discards)
    }
}
crate::mahjong::claim_phase!(4);
pub type Winner = crate::mahjong::Winner<Score>;
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Charging {
    pub from: usize,
    pub claimed_by: Option<usize>,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Match {
    pub id: Uuid,
    pub revision: u64,
    pub seats: [Seat; 4],
    wall: Vec<Tile>,
    #[serde(default)]
    pub(crate) wall_draws: WallDraws,
    pub phase: Phase,
    pub dealer: usize,
    pub next_dealer: usize,
    pub finished: bool,
    pub winners: Vec<Winner>,
    pub points: [i64; 4],
    pub transfers: Vec<Transfer>,
    pub resigned: [bool; 4],
    pub reason: String,
    pub flip: Option<Tile>,
    pub chicken: Option<Tile>,
    pub charging: Option<Charging>,
    pub last: Option<(usize, Tile)>,
    win_from: Option<usize>,
    /// The last ordinary move's record for its event; never checkpointed.
    #[serde(skip)]
    pub(crate) moved: Option<Moved<usize>>,
}
impl Match {
    #[cfg(test)]
    pub fn new(players: usize, dealer: usize) -> Option<Self> {
        if players != 4 || dealer >= 4 {
            return None;
        }
        Some(Self::deal(dealer, &mut rand::thread_rng()))
    }
    #[cfg(test)]
    fn deal(dealer: usize, rng: &mut impl Rng) -> Self {
        let mut wall: Vec<Tile> = (0..27).flat_map(|t| [t; 4]).collect();
        wall.shuffle(rng);
        Self::from_wall(Uuid::new_v4(), dealer, wall)
    }
    pub(crate) fn from_wall(id: Uuid, dealer: usize, mut wall: Vec<Tile>) -> Self {
        let mut wall_draws = WallDraws::default();
        let mut seats: [Seat; 4] = Default::default();
        for (seat, s) in seats.iter_mut().enumerate() {
            s.tiles = wall_draws.initial(&mut wall, seat);
        }
        let tile = wall_draws.draw(&mut wall, Some(dealer), false).unwrap();
        seats[dealer].tiles.push(tile);
        seats[dealer].tiles.sort_unstable();
        Self {
            id,
            revision: 0,
            seats,
            wall,
            wall_draws,
            phase: Phase::Turn {
                seat: dealer,
                drawn: Some(tile),
                kong: false,
            },
            dealer,
            next_dealer: dealer,
            finished: false,
            winners: vec![],
            points: [0; 4],
            transfers: vec![],
            resigned: [false; 4],
            reason: String::new(),
            flip: None,
            chicken: None,
            charging: None,
            last: None,
            win_from: None,
            moved: None,
        }
    }
    fn score_for(
        &self,
        seat: usize,
        tile: Tile,
        from: Option<usize>,
        kong: bool,
        rob: bool,
    ) -> Option<Score> {
        let s = self.seats.get(seat)?;
        let mut hand = s.tiles.clone();
        if from.is_some() {
            if s.passed_hu {
                return None;
            }
            hand.push(tile);
        }
        let mut score = rules::score(&hand, &s.melds)?;
        if from.is_some()
            && score.points == 1
            && !kong
            && !rob
            && !s.melds.iter().any(|m| m.kind != MeldKind::Pung)
        {
            return None;
        }
        if from.is_none() || rob {
            score.points += 1;
        }
        if kong && !rob {
            score.points += 1;
        }
        Some(score)
    }
    fn claims_for(
        &self,
        seat: usize,
        from: usize,
        tile: Tile,
        rob: bool,
        kong: bool,
    ) -> Vec<Action> {
        if seat == from {
            return vec![];
        }
        let mut a = vec![];
        let s = &self.seats[seat];
        if self.score_for(seat, tile, Some(from), kong, rob).is_some() {
            a.push(Action::Hu);
        }
        if !rob {
            let n = copies(&s.tiles, tile);
            if n >= 2 && !s.passed_pung.contains(&tile) {
                a.push(Action::Pung);
            }
            if n == 3 && !self.wall.is_empty() {
                a.push(Action::Kong { tile });
            }
        }
        if !a.is_empty() {
            a.push(Action::Pass);
        }
        a
    }
    /// Tiles left in the wall (public).
    pub fn remaining(&self) -> usize {
        self.wall.len()
    }
    pub fn legal(&self, seat: usize) -> Vec<Action> {
        if seat >= 4 || self.finished {
            return vec![];
        }
        match &self.phase {
            Phase::End => vec![],
            Phase::Claims(w) => {
                if w.choices[seat].is_none() {
                    self.claims_for(seat, w.from, w.tile, w.rob, w.kong)
                } else {
                    vec![]
                }
            }
            Phase::Turn {
                seat: turn,
                drawn,
                kong,
            } => {
                if *turn != seat {
                    return vec![];
                }
                let s = &self.seats[seat];
                let mut a = vec![];
                if drawn.is_some_and(|t| self.score_for(seat, t, None, *kong, false).is_some()) {
                    a.push(Action::Hu);
                }
                let mut tiles = s.tiles.clone();
                tiles.sort_unstable();
                tiles.dedup();
                for tile in tiles {
                    a.push(Action::Discard { tile });
                    if drawn.is_some()
                        && !self.wall.is_empty()
                        && (copies(&s.tiles, tile) == 4
                            || s.melds
                                .iter()
                                .any(|m| m.tile == tile && m.kind == MeldKind::Pung))
                    {
                        a.push(Action::Kong { tile });
                    }
                }
                a
            }
        }
    }
    pub fn decision_seats(&self) -> Vec<usize> {
        (0..4).filter(|&s| !self.legal(s).is_empty()).collect()
    }
    /// The house bot's (and auto-play's) choice for `seat`, from its own
    /// tiles and public information only; see `bot.rs`.
    pub fn suggested(&self, seat: usize) -> Option<Action> {
        super::bot::decide(self, seat)
    }
    pub fn automatic(&mut self, seat: usize) {
        if let Some(a) = self.suggested(seat) {
            let _ = self.act(seat, a);
        }
    }
    /// Plays one move; a refused move changes nothing.
    pub fn act(&mut self, seat: usize, action: Action) -> Result<(), &'static str> {
        crate::mahjong::all_or_nothing(self, |game| game.apply(seat, action))
    }
    fn apply(&mut self, seat: usize, action: Action) -> Result<(), &'static str> {
        let legal = if let Action::Discard { tile } = &action {
            if self.finished
                || !matches!(self.phase, Phase::Turn { seat: turn, .. } if turn == seat)
                || !self.seats.get(seat).is_some_and(|s| s.tiles.contains(tile))
            {
                return Err("illegal_action");
            }
            Vec::new()
        } else {
            let legal = self.legal(seat);
            if !legal.contains(&action) {
                return Err("illegal_action");
            }
            legal
        };
        match self.phase.clone() {
            Phase::Turn { drawn, kong, .. } => match action {
                Action::Hu => self.finish_win(
                    vec![seat],
                    drawn.ok_or("illegal_action")?,
                    None,
                    kong,
                    false,
                ),
                Action::Discard { tile } => {
                    remove_tiles(&mut self.seats[seat].tiles, tile, 1)?;
                    self.seats[seat].discards.push(tile);
                    if tile == CHICKEN && self.charging.is_none() {
                        self.charging = Some(Charging {
                            from: seat,
                            claimed_by: None,
                        });
                    }
                    self.last = Some((seat, tile));
                    self.claims(seat, tile, false, kong)?;
                }
                Action::Kong { tile } => {
                    if copies(&self.seats[seat].tiles, tile) == 4 {
                        self.kong(seat, tile, seat, MeldKind::ConcealedKong)?;
                    } else {
                        self.claims(seat, tile, true, kong)?;
                    }
                }
                _ => return Err("illegal_action"),
            },
            Phase::Claims(ClaimWindow { tile, .. }) => {
                if action != Action::Hu && legal.contains(&Action::Hu) {
                    self.seats[seat].passed_hu = true;
                }
                if action == Action::Pass && legal.contains(&Action::Pung) {
                    self.seats[seat].passed_pung.push(tile);
                }
                if let Phase::Claims(w) = &mut self.phase {
                    w.choices[seat] = Some(action);
                }
                self.resolve()?;
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
        let (s, exposed) = match resolution {
            Resolution::Win(winners) => {
                if rob {
                    self.seats[from].robbed(tile)?;
                }
                self.finish_win(winners, tile, Some(from), kong, rob);
                return Ok(());
            }
            Resolution::KongStands => return self.kong(from, tile, from, MeldKind::AddedKong),
            Resolution::Pass => {
                self.draw((from + 1) % 4, false);
                return Ok(());
            }
            Resolution::Claim { seat, kong } => (seat, kong),
        };
        // Only the first physical chicken discard can become liability.
        if tile == CHICKEN
            && self
                .seats
                .iter()
                .map(|s| s.discards.iter().filter(|&&t| t == CHICKEN).count())
                .sum::<usize>()
                == 1
        {
            if let Some(c) = &mut self.charging {
                if c.from == from && c.claimed_by.is_none() {
                    c.claimed_by = Some(s);
                }
            }
        }
        if exposed {
            // The pile gives the tile up before the replacement draw.
            self.seats[from].discards.pop();
            self.kong(s, tile, from, MeldKind::ExposedKong)
        } else {
            self.seats[s].pung(tile, from)?;
            self.seats[from].discards.pop();
            self.phase = Phase::after_pung(s);
            Ok(())
        }
    }
    fn kong(
        &mut self,
        seat: usize,
        tile: Tile,
        from: usize,
        kind: MeldKind,
    ) -> Result<(), &'static str> {
        self.seats[seat].kong_meld(tile, from, kind)?;
        // Beans settle only at the end, after tenpai and hot-discard checks.
        self.draw(seat, true);
        Ok(())
    }
    fn draw(&mut self, seat: usize, kong: bool) {
        let tile = self.wall_draws.draw(&mut self.wall, Some(seat), kong);
        if let Some(tile) = tile {
            let s = &mut self.seats[seat];
            s.tiles.push(tile);
            s.tiles.sort_unstable();
            s.passed_hu = false;
            s.passed_pung.clear();
            self.phase = Phase::Turn {
                seat,
                drawn: Some(tile),
                kong,
            };
        } else {
            self.finish_draw();
        }
    }
    fn pay(&mut self, from: usize, to: usize, amount: i64, kind: &str) {
        if from == to || amount <= 0 {
            return;
        }
        self.points[from] -= amount;
        self.points[to] += amount;
        self.transfers.push(Transfer {
            from,
            to,
            amount,
            kind: kind.into(),
        });
    }
    fn ready(&self) -> [bool; 4] {
        std::array::from_fn(|i| {
            self.winners.iter().any(|w| w.seat == i)
                || !rules::waits(&self.seats[i].tiles, &self.seats[i].melds).is_empty()
        })
    }
    fn beans(&mut self, ready: [bool; 4], burn: Option<usize>) {
        for (i, eligible) in ready.into_iter().enumerate() {
            if !eligible || burn == Some(i) {
                continue;
            }
            for m in self.seats[i].melds.clone() {
                match m.kind {
                    MeldKind::Pung => {}
                    MeldKind::ExposedKong => self.pay(burn.unwrap_or(m.from), i, 1, "exposed_kong"),
                    MeldKind::AddedKong | MeldKind::ConcealedKong => {
                        for j in 0..4 {
                            if i != j {
                                self.pay(
                                    burn.unwrap_or(j),
                                    i,
                                    if m.kind == MeldKind::AddedKong { 3 } else { 2 },
                                    if m.kind == MeldKind::AddedKong {
                                        "added_kong"
                                    } else {
                                        "concealed_kong"
                                    },
                                );
                            }
                        }
                    }
                }
            }
        }
    }
    fn owned_discard_chickens(&self, seat: usize) -> i64 {
        let count = self.seats[seat]
            .discards
            .iter()
            .filter(|&&t| t == CHICKEN)
            .count() as i64;
        let won =
            self.win_from == Some(seat) && self.winners.first().is_some_and(|w| w.tile == CHICKEN);
        count - i64::from(won)
    }
    fn charging_bonus(&self, seat: usize) -> bool {
        self.charging
            .as_ref()
            .is_some_and(|c| c.from == seat && c.claimed_by.is_none())
            && self.owned_discard_chickens(seat) > 0
    }
    fn chickens(&mut self, ready: [bool; 4], burn: Option<usize>) {
        let gold = if self.chicken == Some(CHICKEN) { 2 } else { 1 };
        for i in 0..4 {
            let s = &self.seats[i];
            let count = |t: Tile| -> i64 {
                s.tiles.iter().filter(|&&x| x == t).count() as i64
                    + s.melds
                        .iter()
                        .filter(|m| m.tile == t)
                        .map(|m| if m.kind == MeldKind::Pung { 3 } else { 4 })
                        .sum::<i64>()
            };
            let mut chicken = count(CHICKEN) * gold;
            if let Some(t) = self.chicken.filter(|&t| t != CHICKEN) {
                chicken += count(t);
            }
            // Ordinary discarded chickens remain with their discarder. Flip
            // chickens count only in retained hands/melds (no full-house option).
            chicken += self.owned_discard_chickens(i) * gold;
            if self.charging_bonus(i) {
                chicken += 2 * gold;
            }
            // A claimed winning discard belongs to the first winner for chicken
            // points; keep the physical tile in the discard pile for conservation.
            if self.win_from.is_some() {
                if let Some(w) = self.winners.first() {
                    let value = if w.tile == CHICKEN {
                        gold
                    } else if Some(w.tile) == self.chicken {
                        1
                    } else {
                        0
                    };
                    if i == w.seat {
                        chicken += value;
                    }
                }
            }
            if ready[i] && burn != Some(i) {
                for j in 0..4 {
                    if j != i {
                        self.pay(burn.unwrap_or(j), i, chicken, "chicken");
                    }
                }
            } else if !ready[i] {
                // Unready exposed chickens are payable to every ready player.
                let visible = self.owned_discard_chickens(i)
                    + s.melds
                        .iter()
                        .filter(|m| m.tile == CHICKEN && m.kind != MeldKind::ConcealedKong)
                        .map(|m| if m.kind == MeldKind::Pung { 3 } else { 4 })
                        .sum::<i64>();
                let charging = if self.charging_bonus(i) { 2 } else { 0 };
                for (j, &r) in ready.iter().enumerate() {
                    if r {
                        self.pay(i, j, (visible + charging) * gold, "unready_chicken");
                    }
                }
            }
        }
        if let Some(c) = self.charging.clone() {
            if let Some(to) = c.claimed_by {
                if ready[to] {
                    self.pay(c.from, to, 2 * gold, "responsibility");
                }
            }
        }
    }
    fn finish_win(
        &mut self,
        seats: Vec<usize>,
        tile: Tile,
        from: Option<usize>,
        kong: bool,
        rob: bool,
    ) {
        for &seat in &seats {
            if let Some(score) = self.score_for(seat, tile, from, kong, rob) {
                if let Some(payer) = from {
                    self.pay(
                        payer,
                        seat,
                        score.points * if rob { 3 } else { 1 },
                        if rob { "rob_kong" } else { "discard_win" },
                    );
                } else {
                    for j in 0..4 {
                        self.pay(j, seat, score.points, "self_draw");
                    }
                }
                self.winners.push(Winner {
                    seat,
                    tile,
                    score,
                    self_draw: from.is_none(),
                });
            }
        }
        self.win_from = from;
        self.flip = self.wall_draws.draw(&mut self.wall, None, false);
        self.chicken = self.flip.map(rules::next_chicken);
        let ready = self.ready();
        let burn = if kong && !rob { from } else { None };
        self.beans(ready, burn);
        self.chickens(ready, burn);
        self.next_dealer = if seats.len() > 1 {
            from.unwrap_or(self.dealer)
        } else {
            seats[0]
        };
        self.finished = true;
        self.phase = Phase::End;
        self.reason = if rob {
            "rob_kong"
        } else if from.is_none() {
            "self_draw"
        } else {
            "discard_win"
        }
        .into();
    }
    fn finish_draw(&mut self) {
        let waits: Vec<_> = self
            .seats
            .iter()
            .map(|s| rules::waits(&s.tiles, &s.melds))
            .collect();
        let ready = std::array::from_fn(|s| !waits[s].is_empty());
        for (i, seat_waits) in waits.iter().enumerate() {
            if let Some(points) = seat_waits.iter().map(|(_, s)| s.points).max() {
                for (j, &r) in ready.iter().enumerate() {
                    if !r {
                        self.pay(j, i, points, "draw_ready");
                    }
                }
            }
        }
        self.beans(ready, None); // No flip/chicken settlement on an exhausted wall.
        self.finished = true;
        self.phase = Phase::End;
        self.reason = "wall_exhausted".into();
    }
    pub fn resign(&mut self, seat: usize) {
        if seat < 4 {
            self.resigned[seat] = true;
        }
    }
    pub fn abandon(&mut self) {
        if !self.finished {
            self.finished = true;
            self.phase = Phase::End;
            self.reason = "abandoned".into();
            self.revision += 1;
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
            Phase::Turn { seat, .. } => ("turn", Some(*seat), None),
            Phase::Claims(w) => (
                if w.rob { "rob_kong" } else { "claims" },
                None,
                Some(SeatTile {
                    seat: w.from,
                    tile: w.tile,
                }),
            ),
            Phase::End => ("end", None, None),
        }
    }

    /// What a spectator sees: every discard, open meld, tile count, the turn,
    /// the flip/chicken tiles and points; never a concealed hand, a concealed
    /// kong, the drawn tile, the wall or legal moves until the hand is over.
    pub fn public_view(&self) -> Value {
        let (phase, turn, discard) = self.phase_view();
        let mut view = json!({"id":self.id,"revision":self.revision,"phase":phase,"turn":turn,"drawn":null,"discard":discard,"last":self.last,"remaining":self.wall.len(),"dealer":self.dealer,"next_dealer":self.next_dealer,"finished":self.finished,"legal":[],"winners":self.winners,"points":self.points,"transfers":self.transfers,"flip":self.flip,"chicken":self.chicken,"charging":self.charging,"reason":self.reason});
        view["seats"] = event::public_rows(self);
        view
    }

    /// The public body of the ordinary move applied to revision `before`:
    /// every public match field it can change. `winners`, `flip`, `chicken`,
    /// `next_dealer`, `finished` and `reason` only change when the match ends,
    /// which is a boundary (full frames); `dealer` never does.
    pub fn move_event(&self, before: u64) -> Option<Box<RawValue>> {
        let (moved, seats) = event::moved_since(self, before)?;
        let transfers_tail = event::tail(&self.transfers, moved.lists)?;
        let (phase, turn, discard) = self.phase_view();
        serde_json::value::to_raw_value(&MoveEvent {
            revision: self.revision,
            phase,
            turn,
            discard,
            last: self.last,
            remaining: self.wall.len(),
            points: self.points,
            transfers_from: transfers_tail.map(|_| moved.lists),
            transfers: transfers_tail,
            charging: &self.charging,
            seats,
        })
        .ok()
    }

    pub fn move_private(&self, seat: Option<usize>) -> Option<Box<RawValue>> {
        event::move_private(self, seat)
    }
    pub fn valid(&self) -> bool {
        if self.dealer >= 4
            || self.next_dealer >= 4
            || self.finished != matches!(self.phase, Phase::End)
            || self.points.iter().sum::<i64>() != 0
        {
            return false;
        }
        let mut c = [0usize; 27];
        let mut add = |t: Tile, n: usize| {
            if let Some(x) = c.get_mut(t as usize) {
                *x += n;
                true
            } else {
                false
            }
        };
        for &t in &self.wall {
            if !add(t, 1) {
                return false;
            }
        }
        if self.flip.is_some_and(|t| !add(t, 1)) {
            return false;
        }
        for s in &self.seats {
            if s.melds.len() > 4 {
                return false;
            }
            for &t in s.tiles.iter().chain(&s.discards) {
                if !add(t, 1) {
                    return false;
                }
            }
            for m in &s.melds {
                if m.from >= 4 || !add(m.tile, if m.kind == MeldKind::Pung { 3 } else { 4 }) {
                    return false;
                }
            }
        }
        if c.iter().any(|&n| n != 4) {
            return false;
        }
        if self.winners.iter().any(|w| w.seat >= 4 || w.tile >= 27)
            || self
                .charging
                .as_ref()
                .is_some_and(|x| x.from >= 4 || x.claimed_by.is_some_and(|s| s >= 4))
        {
            return false;
        }
        let mut sums = [0; 4];
        for t in &self.transfers {
            if t.from >= 4 || t.to >= 4 || t.from == t.to || t.amount <= 0 {
                return false;
            }
            sums[t.from] -= t.amount;
            sums[t.to] += t.amount;
        }
        if sums != self.points {
            return false;
        }
        match &self.phase {
            Phase::Turn { seat, drawn, .. } => {
                *seat < 4 && drawn.is_none_or(|t| self.seats[*seat].tiles.contains(&t))
            }
            Phase::Claims(w) => w.from < 4 && w.tile < 27,
            Phase::End => true,
        }
    }
}

impl TileMatch for Match {
    crate::mahjong::tile_match_fields!();
    type Lists = usize;
    type Row<'a> = SeatRow<'a>;
    type Claims<'a> = ();
    /// `tiles` only for the seat itself or a finished match.
    fn row(&self, seat: usize, own: bool) -> SeatRow<'_> {
        let player = &self.seats[seat];
        let visible = own || self.finished;
        SeatRow {
            tiles: visible.then_some(&player.tiles),
            count: player.tiles.len(),
            discards: &player.discards,
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
        self.phase.deciding(s)
    }
    fn lists(&self) -> usize {
        self.transfers.len()
    }
}

#[derive(Serialize)]
pub struct SeatRow<'a> {
    tiles: Option<&'a Vec<Tile>>,
    count: usize,
    discards: &'a [Tile],
    melds: Vec<event::MeldView<MeldKind>>,
}

#[derive(Serialize)]
struct MoveEvent<'a> {
    revision: u64,
    phase: &'static str,
    turn: Option<usize>,
    discard: Option<SeatTile>,
    last: Option<(usize, Tile)>,
    remaining: usize,
    points: [i64; 4],
    #[serde(skip_serializing_if = "Option::is_none")]
    transfers_from: Option<usize>,
    #[serde(skip_serializing_if = "Option::is_none")]
    transfers: Option<&'a [Transfer]>,
    charging: &'a Option<Charging>,
    #[serde(flatten)]
    seats: event::SeatRows<SeatRow<'a>>,
}
