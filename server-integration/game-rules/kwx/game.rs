//! Pure, serializable three-seat state machine; a hand deals in two or three
//! of the seats. No clocks, sockets or database.
use super::rules::{self, Meld, MeldKind, RuleOptions, Score, Tile, WinContext};
use crate::mahjong::wall::WallDraws;
use crate::mahjong::{
    claim_phase, copies, remove_tiles, ClaimWindow, Resolution, TileSeat, Transfer,
};
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "t", rename_all = "snake_case")]
pub enum Action {
    Discard { tile: Tile },
    Ready { tile: Tile },
    Pung,
    Kong { tile: Tile },
    Hu,
    Pass,
}
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct Seat {
    pub tiles: Vec<Tile>,
    pub melds: Vec<Meld>,
    pub discards: Vec<Tile>,
    pub ready: bool,
    pub waits: Vec<Tile>,
    #[serde(default)]
    pub exposed: Option<Vec<Tile>>,
    pub passed_hu: bool,
}
impl TileSeat for Seat {
    fn hand(&mut self) -> (&mut Vec<Tile>, &mut Vec<Meld>, &mut Vec<Tile>) {
        (&mut self.tiles, &mut self.melds, &mut self.discards)
    }
}
claim_phase!(3);
pub type Winner = crate::mahjong::Winner<Score>;
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Game {
    pub seats: [Seat; 3],
    pub wall: Vec<Tile>,
    #[serde(default)]
    pub(crate) wall_draws: WallDraws,
    pub phase: Phase,
    pub dealer: usize,
    pub next_dealer: usize,
    pub base: i64,
    pub transfers: Vec<Transfer>,
    pub winners: Vec<Winner>,
    pub deltas: [i64; 3],
    pub last: Option<(usize, Tile)>,
    pub rules: RuleOptions,
    pub kong_chain: u8,
    pub horses: Vec<Tile>,
    pub stacks: Option<[i64; 3]>,
    /// Seats dealt into this hand.
    pub active: [bool; 3],
    /// Every declined discard win this hand, as (passer, discarder). Passing a
    /// winning tile is rare honest play and a core collusion tell, so the
    /// collusion tracker reads it after the hand ends.
    pub passed_wins: Vec<(usize, usize)>,
}
impl Game {
    pub(crate) fn from_wall(
        dealer: usize,
        base: i64,
        rules: RuleOptions,
        active: [bool; 3],
        mut wall: Vec<Tile>,
    ) -> Self {
        let mut wall_draws = WallDraws::default();
        let mut seats: [Seat; 3] = Default::default();
        for (seat, s) in seats.iter_mut().enumerate().filter(|(i, _)| active[*i]) {
            s.tiles = wall_draws.initial(&mut wall, seat);
        }
        let tile = wall_draws
            .draw(&mut wall, Some(dealer), false)
            .expect("84 tiles minus at most 39");
        seats[dealer].tiles.push(tile);
        seats[dealer].tiles.sort_unstable();
        Self {
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
            base,
            transfers: vec![],
            winners: vec![],
            deltas: [0; 3],
            last: None,
            rules,
            kong_chain: 0,
            horses: vec![],
            stacks: None,
            active,
            passed_wins: vec![],
        }
    }
    pub fn ended(&self) -> bool {
        matches!(self.phase, Phase::End)
    }
    fn score_for(
        &self,
        seat: usize,
        tile: Tile,
        from: Option<usize>,
        kong: bool,
        rob: bool,
    ) -> Option<Score> {
        let s = &self.seats[seat];
        let mut tiles = s.tiles.clone();
        if from.is_some() {
            if s.passed_hu {
                return None;
            }
            tiles.push(tile);
        }
        let mut score = rules::score_with_options(
            &tiles,
            &s.melds,
            WinContext {
                tile,
                kong_draw: from.is_none() && kong,
                kong_discard: from.is_some() && kong && !rob,
                rob_kong: rob,
                last_tile: from.is_none() && self.wall.is_empty(),
            },
            self.rules.wild,
        )?;
        let chain = self.kong_chain.saturating_add(u8::from(rob && kong));
        if self.rules.kong_chain && (kong || rob) && chain > 1 {
            let factor = 1u32 << chain.saturating_sub(1).min(12);
            score.raw = score.raw.saturating_mul(factor);
            // Hand-type multipliers keep the existing Xiangyang cap.
            score.multiplier = score.raw.min(
                if score.patterns.iter().any(|p| p == "double_dragon_pairs") {
                    16
                } else {
                    8
                },
            );
            score.patterns.push("kong_chain".into());
        }
        if from.is_some_and(|f| score.multiplier == 1 && !s.ready && !self.seats[f].ready) {
            return None;
        }
        Some(score)
    }
    fn ready_kong(&self, seat: usize, tile: Tile, from: usize, kind: MeldKind) -> bool {
        let s = &self.seats[seat];
        if !s.ready {
            return true;
        }
        let mut tiles = s.tiles.clone();
        let mut melds = s.melds.clone();
        let count = match kind {
            MeldKind::AddedKong => 1,
            MeldKind::ConcealedKong => 4,
            _ => 3,
        };
        for _ in 0..count {
            let Some(i) = tiles.iter().position(|&t| t == tile) else {
                return false;
            };
            tiles.remove(i);
        }
        if kind == MeldKind::AddedKong {
            let Some(m) = melds
                .iter_mut()
                .find(|m| m.tile == tile && m.kind == MeldKind::Pung)
            else {
                return false;
            };
            m.kind = kind;
        } else {
            melds.push(Meld { tile, kind, from });
        }
        rules::waits_with_options(&tiles, &melds, self.rules.wild) == s.waits
    }
    fn claim_actions(
        &self,
        seat: usize,
        from: usize,
        tile: Tile,
        rob: bool,
        kong: bool,
    ) -> Vec<Action> {
        if seat == from || !self.active[seat] {
            return vec![];
        }
        let mut a = vec![];
        let s = &self.seats[seat];
        if self.score_for(seat, tile, Some(from), kong, rob).is_some() {
            a.push(Action::Hu);
        }
        if !rob && !self.wall.is_empty() && !(self.rules.wild && tile == 18) {
            let n = s.tiles.iter().filter(|&&t| t == tile).count();
            if n >= 2 && !s.ready {
                a.push(Action::Pung);
            }
            if n == 3 && self.ready_kong(seat, tile, from, MeldKind::ExposedKong) {
                a.push(Action::Kong { tile });
            }
        }
        if !a.is_empty() {
            a.push(Action::Pass);
        }
        a
    }
    fn can_discard(&self, seat: usize, tile: Tile) -> bool {
        let Phase::Turn {
            seat: turn, drawn, ..
        } = self.phase
        else {
            return false;
        };
        self.active.get(seat) == Some(&true)
            && turn == seat
            && (!self.seats[seat].ready || drawn == Some(tile))
            && self.seats[seat].tiles.contains(&tile)
    }

    pub fn legal(&self, seat: usize) -> Vec<Action> {
        if seat >= 3 || !self.active[seat] {
            return vec![];
        }
        match &self.phase {
            Phase::End => vec![],
            Phase::Claims(w) => {
                if w.choices[seat].is_some() {
                    vec![]
                } else {
                    self.claim_actions(seat, w.from, w.tile, w.rob, w.kong)
                }
            }
            Phase::Turn {
                seat: turn,
                drawn,
                kong,
            } => {
                if seat != *turn {
                    return vec![];
                }
                let s = &self.seats[seat];
                let mut a = vec![];
                if drawn.is_some_and(|t| self.score_for(seat, t, None, *kong, false).is_some()) {
                    a.push(Action::Hu);
                }
                let mut unique = s.tiles.clone();
                unique.sort_unstable();
                unique.dedup();
                for tile in unique {
                    if self.can_discard(seat, tile) {
                        a.push(Action::Discard { tile });
                        if !s.ready
                            && rules::can_declare_ready(&s.tiles, &s.melds, self.rules.wild, tile)
                        {
                            a.push(Action::Ready { tile });
                        }
                    }
                    if drawn.is_some() && !self.wall.is_empty() && !(self.rules.wild && tile == 18)
                    {
                        let n = s.tiles.iter().filter(|&&t| t == tile).count();
                        let kind = if n == 4 {
                            Some(MeldKind::ConcealedKong)
                        } else if s
                            .melds
                            .iter()
                            .any(|m| m.tile == tile && m.kind == MeldKind::Pung)
                            && *drawn == Some(tile)
                        {
                            Some(MeldKind::AddedKong)
                        } else {
                            None
                        };
                        if kind.is_some_and(|k| self.ready_kong(seat, tile, seat, k)) {
                            a.push(Action::Kong { tile });
                        }
                    }
                }
                a
            }
        }
    }
    /// The house bot's (and auto-play's) choice for `seat`, from its own
    /// tiles and public information only; see `bot.rs`.
    pub fn automatic(&self, seat: usize) -> Option<Action> {
        super::bot::decide(self, seat)
    }
    /// Plays one move; a refused move changes nothing.
    pub fn act(&mut self, seat: usize, action: Action) -> Result<(), &'static str> {
        crate::mahjong::all_or_nothing(self, |game| game.apply(seat, action))
    }
    fn apply(&mut self, seat: usize, action: Action) -> Result<(), &'static str> {
        let legal = match action {
            Action::Discard { tile } => self.can_discard(seat, tile),
            _ => self.legal(seat).contains(&action),
        };
        if !legal {
            return Err("illegal_action");
        }
        match self.phase.clone() {
            Phase::Turn { drawn, kong, .. } => match action {
                Action::Hu => {
                    self.finish_win(
                        vec![seat],
                        drawn.ok_or("illegal_action")?,
                        None,
                        kong,
                        false,
                    );
                }
                Action::Discard { tile } | Action::Ready { tile } => {
                    let ready = matches!(action, Action::Ready { .. });
                    let s = &mut self.seats[seat];
                    remove_tiles(&mut s.tiles, tile, 1)?;
                    s.discards.push(tile);
                    if ready {
                        s.ready = true;
                        s.exposed =
                            Some(rules::listening_tiles(&s.tiles, &s.melds, self.rules.wild));
                        s.waits = rules::waits_with_options(&s.tiles, &s.melds, self.rules.wild);
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
            Phase::Claims(w) => {
                if action != Action::Hu
                    && self
                        .score_for(seat, w.tile, Some(w.from), w.kong, w.rob)
                        .is_some()
                {
                    self.seats[seat].passed_hu = true;
                    self.passed_wins.push((seat, w.from));
                }
                if let Phase::Claims(w) = &mut self.phase {
                    w.choices[seat] = Some(action);
                }
                self.resolve_claims()?;
            }
            Phase::End => return Err("illegal_action"),
        }
        Ok(())
    }
    fn claims(
        &mut self,
        from: usize,
        tile: Tile,
        rob: bool,
        kong: bool,
    ) -> Result<(), &'static str> {
        let window = ClaimWindow::open(from, tile, rob, kong, |s| {
            !self.claim_actions(s, from, tile, rob, kong).is_empty()
        });
        self.phase = Phase::Claims(window);
        self.resolve_claims()
    }
    fn resolve_claims(&mut self) -> Result<(), &'static str> {
        let Some(d) = self.phase.decided() else {
            return Ok(());
        };
        match d.resolution {
            Resolution::Win(winners) => {
                if d.rob {
                    self.seats[d.from].robbed(d.tile)?;
                }
                self.finish_win(winners, d.tile, Some(d.from), d.kong, d.rob);
            }
            Resolution::KongStands => self.kong(d.from, d.tile, d.from, MeldKind::AddedKong)?,
            Resolution::Claim { seat, kong: true } => {
                // The pile gives the tile up before the replacement draw.
                self.seats[d.from].discards.pop();
                self.kong(seat, d.tile, d.from, MeldKind::ExposedKong)?;
            }
            Resolution::Claim { seat, kong: false } => {
                self.seats[seat].pung(d.tile, d.from)?;
                self.seats[d.from].discards.pop();
                self.kong_chain = 0;
                self.phase = Phase::after_pung(seat);
            }
            Resolution::Pass => self.draw(self.next_active(d.from), false),
        }
        Ok(())
    }
    fn next_active(&self, from: usize) -> usize {
        (1..3)
            .map(|n| (from + n) % 3)
            .find(|&s| self.active[s])
            .unwrap_or(from)
    }
    fn pay(&mut self, from: usize, to: usize, units: i64, kind: &str) {
        let requested = units.saturating_mul(self.base);
        let amount = self.stacks.map_or(requested, |stacks| {
            requested.min((stacks[from] + self.deltas[from]).max(0))
        });
        if amount == 0 {
            return;
        }
        self.deltas[from] -= amount;
        self.deltas[to] += amount;
        self.transfers.push(Transfer {
            from,
            to,
            amount,
            kind: kind.to_owned(),
        });
    }
    fn kong(
        &mut self,
        seat: usize,
        tile: Tile,
        from: usize,
        kind: MeldKind,
    ) -> Result<(), &'static str> {
        self.seats[seat].kong_meld(tile, from, kind)?;
        self.kong_chain = self.kong_chain.saturating_add(1);
        let factor = if self.rules.kong_chain {
            1i64 << self.kong_chain.saturating_sub(1).min(12)
        } else {
            1
        };
        if kind == MeldKind::ExposedKong {
            self.pay(from, seat, 2 * factor, "exposed_kong");
        } else {
            for other in 0..3 {
                if other != seat && self.active[other] {
                    self.pay(
                        other,
                        seat,
                        if kind == MeldKind::AddedKong { 1 } else { 2 } * factor,
                        if kind == MeldKind::AddedKong {
                            "added_kong"
                        } else {
                            "concealed_kong"
                        },
                    );
                }
            }
        }
        self.draw(seat, true);
        Ok(())
    }
    fn draw(&mut self, seat: usize, kong: bool) {
        if !kong {
            self.kong_chain = 0;
        }
        let tile = self.wall_draws.draw(&mut self.wall, Some(seat), kong);
        if let Some(tile) = tile {
            self.seats[seat].tiles.push(tile);
            self.seats[seat].tiles.sort_unstable();
            self.seats[seat].passed_hu = false;
            self.phase = Phase::Turn {
                seat,
                drawn: Some(tile),
                kong,
            };
        } else {
            for a in 0..3 {
                for b in 0..3 {
                    if self.active[a]
                        && self.active[b]
                        && self.seats[a].ready
                        && !self.seats[b].ready
                    {
                        self.pay(a, b, 1, "draw_ready");
                    }
                }
            }
            let ready: Vec<_> = (0..3).filter(|&s| self.seats[s].ready).collect();
            if ready.len() == 1 {
                self.next_dealer = ready[0];
            }
            self.phase = Phase::End;
        }
    }
    fn finish_win(
        &mut self,
        winners: Vec<usize>,
        tile: Tile,
        from: Option<usize>,
        kong: bool,
        rob: bool,
    ) {
        let scores: Vec<_> = winners
            .iter()
            .map(|&seat| self.score_for(seat, tile, from, kong, rob))
            .collect();
        // One shared, real wall tile per self-drawn win (full-point horse).
        // An exhausted wall awards no horse bonus.
        let horse = if self.rules.horse && from.is_none() {
            self.wall_draws
                .draw(&mut self.wall, None, false)
                .map(|horse| {
                    self.horses.push(horse);
                    if horse < 18 {
                        i64::from(horse % 9 + 1)
                    } else {
                        10
                    }
                })
        } else {
            None
        };
        for (&seat, score) in winners.iter().zip(scores) {
            // The action was validated before state transition. Passing is reset
            // only on a draw; a winner never passes their own accepted Hu.
            if let Some(score) = score {
                let win_ready = if self.seats[seat].ready { 2 } else { 1 };
                for loser in 0..3 {
                    if loser == seat || !self.active[loser] || from.is_some_and(|f| f != loser) {
                        continue;
                    }
                    let lose_ready = if self.seats[loser].ready { 2 } else { 1 };
                    let bao = if from.is_some() && self.seats[seat].ready {
                        2
                    } else {
                        1
                    };
                    self.pay(
                        loser,
                        seat,
                        i64::from(score.multiplier) * win_ready * lose_ready * bao,
                        if from.is_none() {
                            "self_draw"
                        } else if rob {
                            "rob_kong"
                        } else {
                            "discard_win"
                        },
                    );
                    if self.rules.piao > 0 {
                        self.pay(loser, seat, i64::from(self.rules.piao), "piao");
                    }
                    if let Some(points) = horse {
                        self.pay(loser, seat, points, "horse");
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
        self.next_dealer = if winners.len() > 1 {
            from.unwrap_or(self.dealer)
        } else {
            winners[0]
        };
        self.phase = Phase::End;
    }
}
