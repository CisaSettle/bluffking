use super::rules::{self, count, remove, Kind, Meld, Score, Seat, Tile};
use crate::game_hub::board::BoardMatchSeat;
use crate::mahjong::event::{self, Moved, TileMatch};
use crate::mahjong::wall::WallDraws;
#[cfg(test)]
use rand::seq::SliceRandom;
use serde::{Deserialize, Serialize};
use serde_json::{json, value::RawValue, Value};
use uuid::Uuid;
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum Action {
    Discard { tile: Tile },
    Knock { tile: Tile },
    Kong { tile: Tile },
    Claim { index: usize },
    Pass,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Claim {
    pub seat: usize,
    pub kind: Kind,
    pub tiles: Vec<Tile>,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Last {
    pub seat: usize,
    pub tile: Tile,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Result {
    pub winners: Vec<usize>,
    pub from: Option<usize>,
    pub scores: Vec<Score>,
    pub delta: [i64; 4],
}
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Phase {
    Turn,
    Claim,
    End,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Match {
    pub id: Uuid,
    pub revision: u64,
    pub seats: [Seat; 4],
    pub wall: Vec<Tile>,
    #[serde(default)]
    pub(crate) wall_draws: WallDraws,
    pub turn: usize,
    pub phase: Phase,
    pub drawn: Option<Tile>,
    pub replacement: bool,
    pub last: Option<Last>,
    pub claims: Vec<Claim>,
    pub result: Option<Result>,
    pub forbidden: Vec<Tile>,
    /// The last ordinary move's record for its event; never checkpointed.
    #[serde(skip)]
    pub(crate) moved: Option<Moved<()>>,
}
impl Match {
    #[cfg(test)]
    pub fn new(players: usize, dealer: usize) -> Option<Self> {
        if players != 4 || dealer >= 4 {
            return None;
        }
        let mut wall: Vec<_> = (0..34).flat_map(|t| [t; 4]).chain(34..42).collect();
        wall.shuffle(&mut rand::thread_rng());
        Some(Self::from_wall(Uuid::new_v4(), dealer, wall))
    }
    pub(crate) fn from_wall(id: Uuid, dealer: usize, wall: Vec<Tile>) -> Self {
        let mut g = Self {
            id,
            revision: 0,
            seats: Default::default(),
            wall,
            wall_draws: WallDraws::default(),
            turn: dealer,
            phase: Phase::Turn,
            drawn: None,
            replacement: false,
            last: None,
            claims: vec![],
            result: None,
            forbidden: vec![],
            moved: None,
        };
        for _ in 0..13 {
            for s in 0..4 {
                g.take(s, false);
            }
        }
        g.draw(dealer, false);
        g
    }
    pub fn finished(&self) -> bool {
        self.phase == Phase::End
    }
    pub fn deltas(&self) -> [i64; 4] {
        self.result.as_ref().map(|r| r.delta).unwrap_or([0; 4])
    }
    pub fn valid(&self) -> bool {
        self.turn < 4
            && self.finished() == self.result.is_some()
            && self.deltas().iter().sum::<i64>() == 0
            && self.wall.iter().all(|&t| t < 42)
            && self.drawn.is_none_or(|t| t < 31)
            && self.last.as_ref().is_none_or(|l| l.seat < 4 && l.tile < 31)
            && (self.phase != Phase::Claim || (!self.claims.is_empty() && self.last.is_some()))
            && self
                .claims
                .iter()
                .all(|c| c.seat < 4 && c.tiles.iter().all(|&t| t < 31))
            && self.seats.iter().all(|s| {
                s.hand
                    .iter()
                    .chain(&s.river)
                    .chain(&s.waits)
                    .all(|&t| t < 31)
                    && s.flowers.iter().all(|&t| (31..42).contains(&t))
                    && s.melds.len() <= 4
                    && s.melds.iter().all(|m| {
                        m.from < 4
                            && m.tiles.len()
                                == if matches!(m.kind, Kind::Chow | Kind::Pung) {
                                    3
                                } else {
                                    4
                                }
                            && m.tiles.iter().all(|&t| t < 31)
                    })
            })
    }
    fn take(&mut self, s: usize, mut replacement: bool) -> Option<Tile> {
        while let Some(t) = self.wall_draws.draw(&mut self.wall, Some(s), !replacement) {
            if t >= 31 {
                self.seats[s].flowers.push(t);
                replacement = true;
                continue;
            }
            self.seats[s].hand.push(t);
            self.seats[s].hand.sort_unstable();
            self.replacement = replacement;
            return Some(t);
        }
        None
    }
    fn draw(&mut self, s: usize, replacement: bool) {
        self.turn = s;
        self.phase = Phase::Turn;
        self.forbidden.clear();
        self.claims.clear();
        self.drawn = self.take(s, replacement);
        if self.drawn.is_none() {
            self.finish(vec![], None, None, false);
            return;
        }
        if self.seats[s].ready && rules::winning(&self.seats[s].hand, self.seats[s].melds.len()) {
            self.finish(vec![s], None, None, false);
        }
    }
    fn finish(&mut self, winners: Vec<usize>, from: Option<usize>, tile: Option<Tile>, rob: bool) {
        let scores: Vec<_> = winners
            .iter()
            .map(|&s| {
                let mut h = self.seats[s].hand.clone();
                if let Some(t) = tile {
                    h.push(t);
                }
                rules::score(
                    &self.seats[s],
                    s,
                    &h,
                    from.is_none() && self.replacement,
                    rob,
                )
            })
            .collect();
        let mut delta = [0; 4];
        for score in &scores {
            for payer in 0..4 {
                if from.map_or(payer != score.seat, |s| payer == s) {
                    delta[payer] -= score.points;
                    delta[score.seat] += score.points;
                }
            }
        }
        self.phase = Phase::End;
        self.claims.clear();
        self.result = Some(Result {
            winners,
            from,
            scores,
            delta,
        });
    }
    fn win_claimants(&self, from: usize, tile: Tile) -> Vec<usize> {
        (1..4)
            .map(|n| (from + n) % 4)
            .filter(|&s| {
                let p = &self.seats[s];
                if !p.ready {
                    return false;
                }
                let mut h = p.hand.clone();
                h.push(tile);
                rules::winning(&h, p.melds.len())
            })
            .collect()
    }
    fn kongs(&self, s: usize) -> Vec<Tile> {
        if self.phase != Phase::Turn
            || self.turn != s
            || self.drawn.is_none()
            || self.wall.is_empty()
        {
            return vec![];
        }
        let p = &self.seats[s];
        let mut tiles = p.hand.clone();
        tiles.dedup();
        tiles
            .into_iter()
            .filter(|&t| {
                let added = if count(&p.hand, t) == 4 {
                    None
                } else {
                    let Some(i) = p
                        .melds
                        .iter()
                        .position(|m| m.kind == Kind::Pung && m.tiles[0] == t)
                    else {
                        return false;
                    };
                    Some(i)
                };
                if !p.ready {
                    return true;
                }
                let mut h = p.hand.clone();
                let mut ms = p.melds.clone();
                if let Some(i) = added {
                    remove(&mut h, t);
                    ms[i].kind = Kind::Kong;
                    ms[i].tiles.push(t);
                } else {
                    h.retain(|&v| v != t);
                    ms.push(Meld {
                        kind: Kind::Concealed,
                        tiles: vec![t; 4],
                        from: s,
                    });
                }
                rules::preserves(p, &h, &ms)
            })
            .collect()
    }
    fn discard_tiles(&self, seat: usize) -> &[Tile] {
        if self.seats[seat].ready {
            self.drawn.as_slice()
        } else {
            &self.seats[seat].hand
        }
    }

    /// The clock only needs to know who can act. A legal discard suffices
    /// even when knocking is available or a ready player's kong takes priority.
    pub(super) fn has_decision(&self, seat: usize) -> bool {
        if seat >= self.seats.len() {
            return false;
        }
        match self.phase {
            Phase::End => false,
            Phase::Claim => self.claims.first().is_some_and(|first| first.seat == seat),
            Phase::Turn => {
                self.turn == seat
                    && (self
                        .discard_tiles(seat)
                        .iter()
                        .any(|tile| !self.forbidden.contains(tile))
                        || !self.kongs(seat).is_empty())
            }
        }
    }

    pub fn legal(&self, s: usize) -> Vec<Action> {
        self.legal_for(s, None)
    }

    // A viewer needs every knock. Validating a submitted action needs the
    // winning-draw search only for the tile in a submitted knock.
    fn legal_for(&self, s: usize, submitted: Option<&Action>) -> Vec<Action> {
        if s >= 4 || self.finished() {
            return vec![];
        }
        if self.phase == Phase::Claim {
            let Some(first) = self.claims.first() else {
                return vec![];
            };
            if first.seat != s {
                return vec![];
            }
            let mut out = if self.seats[s].ready {
                vec![]
            } else {
                vec![Action::Pass]
            };
            out.extend(
                self.claims
                    .iter()
                    .enumerate()
                    .filter(|(_, c)| {
                        c.seat == s && (c.kind == Kind::Chow) == (first.kind == Kind::Chow)
                    })
                    .map(|(index, _)| Action::Claim { index }),
            );
            return out;
        }
        if self.turn != s {
            return vec![];
        }
        let p = &self.seats[s];
        let kongs = self.kongs(s);
        if p.ready && !kongs.is_empty() {
            return kongs
                .into_iter()
                .map(|tile| Action::Kong { tile })
                .collect();
        }
        let mut tiles = self.discard_tiles(s).to_vec();
        tiles.dedup();
        let mut out = vec![];
        for t in tiles {
            if self.forbidden.contains(&t) {
                continue;
            }
            out.push(Action::Discard { tile: t });
            if !p.ready
                && submitted.is_none_or(|action| action == &(Action::Knock { tile: t }))
                && rules::can_knock(&p.hand, &p.melds, t)
            {
                out.push(Action::Knock { tile: t });
            }
        }
        out.extend(kongs.into_iter().map(|tile| Action::Kong { tile }));
        out
    }
    fn collect_claims(&self, from: usize, tile: Tile) -> Vec<Claim> {
        let mut claims = vec![];
        for distance in 1..4 {
            let seat = (from + distance) % 4;
            let p = &self.seats[seat];
            let n = count(&p.hand, tile);
            if n == 3 && !self.wall.is_empty() {
                let h: Vec<_> = p.hand.iter().copied().filter(|&t| t != tile).collect();
                let mut ms = p.melds.clone();
                ms.push(Meld {
                    kind: Kind::Kong,
                    tiles: vec![tile; 4],
                    from,
                });
                if rules::preserves(p, &h, &ms) {
                    claims.push(Claim {
                        seat,
                        kind: Kind::Kong,
                        tiles: vec![tile; 3],
                    });
                }
            }
            if p.ready {
                continue;
            }
            if n >= 2 {
                claims.push(Claim {
                    seat,
                    kind: Kind::Pung,
                    tiles: vec![tile; 2],
                });
            }
            if distance == 1 && tile < 27 {
                for low in tile.saturating_sub(2)..=tile {
                    if low / 9 != tile / 9 || low % 9 > 6 {
                        continue;
                    }
                    let needed: Vec<_> = (low..=low + 2).filter(|&t| t != tile).collect();
                    if needed.iter().all(|t| p.hand.contains(t)) {
                        let mut rest = p.hand.clone();
                        for &t in &needed {
                            remove(&mut rest, t);
                        }
                        let forbidden = rules::chow_forbidden(tile, &needed);
                        if rest.iter().any(|t| !forbidden.contains(t)) {
                            claims.push(Claim {
                                seat,
                                kind: Kind::Chow,
                                tiles: needed,
                            });
                        }
                    }
                }
            }
        }
        claims.sort_by_key(|c| c.kind == Kind::Chow);
        claims
    }
    fn claim(&mut self, c: Claim) {
        let last = self.last.take().unwrap();
        let p = &mut self.seats[c.seat];
        for &t in &c.tiles {
            remove(&mut p.hand, t);
        }
        let mut tiles = c.tiles.clone();
        tiles.push(last.tile);
        tiles.sort_unstable();
        p.melds.push(Meld {
            kind: c.kind.clone(),
            tiles,
            from: last.seat,
        });
        self.seats[last.seat].river.pop();
        self.turn = c.seat;
        self.claims.clear();
        self.phase = Phase::Turn;
        self.drawn = None;
        self.replacement = false;
        self.forbidden = if c.kind == Kind::Chow {
            rules::chow_forbidden(last.tile, &c.tiles)
        } else {
            vec![]
        };
        if c.kind == Kind::Kong {
            self.draw(c.seat, true);
        }
    }
    fn resolve_claims(&mut self) {
        // Every seat decides through the room actor, including bots; no local seat-0 shortcut.
        if self.claims.is_empty() {
            self.draw((self.last.as_ref().unwrap().seat + 1) % 4, false);
        }
    }
    pub fn act(&mut self, s: usize, a: Action) -> std::result::Result<(), &'static str> {
        if !self.legal_for(s, Some(&a)).contains(&a) {
            return Err("illegal_action");
        }
        self.revision += 1;
        let (tile, knock) = match a {
            Action::Pass => {
                self.claims.retain(|c| c.seat != s);
                self.resolve_claims();
                return Ok(());
            }
            Action::Claim { index } => {
                self.claim(self.claims[index].clone());
                return Ok(());
            }
            Action::Kong { tile } => {
                if let Some(i) = self.seats[s]
                    .melds
                    .iter()
                    .position(|m| m.kind == Kind::Pung && m.tiles[0] == tile)
                {
                    let winners = self.win_claimants(s, tile);
                    remove(&mut self.seats[s].hand, tile);
                    if !winners.is_empty() {
                        self.last = Some(Last { seat: s, tile });
                        self.finish(winners, Some(s), Some(tile), true);
                        return Ok(());
                    }
                    self.seats[s].melds[i].kind = Kind::Kong;
                    self.seats[s].melds[i].tiles.push(tile);
                } else {
                    for _ in 0..4 {
                        remove(&mut self.seats[s].hand, tile);
                    }
                    self.seats[s].melds.push(Meld {
                        kind: Kind::Concealed,
                        tiles: vec![tile; 4],
                        from: s,
                    });
                }
                self.draw(s, true);
                return Ok(());
            }
            Action::Discard { tile } => (tile, false),
            Action::Knock { tile } => (tile, true),
        };
        let p = &mut self.seats[s];
        remove(&mut p.hand, tile);
        p.river.push(tile);
        if knock {
            p.ready = true;
            p.waits = rules::waits(&p.hand, &p.melds);
        }
        self.last = Some(Last { seat: s, tile });
        self.drawn = None;
        self.forbidden.clear();
        let winners = self.win_claimants(s, tile);
        if !winners.is_empty() {
            self.finish(winners, Some(s), Some(tile), false);
            return Ok(());
        }
        self.claims = self.collect_claims(s, tile);
        self.phase = Phase::Claim;
        self.resolve_claims();
        Ok(())
    }
    pub fn automatic_action(&self, s: usize) -> Option<Action> {
        let actions = self.legal(s);
        for kind in 0..3 {
            if let Some(a) = actions.iter().find(|a| match kind {
                0 => matches!(a, Action::Kong { .. }),
                1 => matches!(a, Action::Knock { .. }),
                _ => matches!(a, Action::Claim { .. }),
            }) {
                return Some(a.clone());
            }
        }
        let h = &self.seats[s].hand;
        actions
            .iter()
            .filter_map(|a| {
                if let Action::Discard { tile } = a {
                    let t = *tile;
                    let value = count(h, t) * 5
                        + if t < 27 {
                            h.iter()
                                .filter(|&&v| v / 9 == t / 9 && v.abs_diff(t) == 1)
                                .count()
                                * 2
                                + h.iter()
                                    .filter(|&&v| v / 9 == t / 9 && v.abs_diff(t) == 2)
                                    .count()
                        } else {
                            0
                        };
                    Some((value, a))
                } else {
                    None
                }
            })
            .min_by_key(|(v, _)| *v)
            .map(|(_, a)| a.clone())
            .or_else(|| actions.first().cloned())
    }
    pub fn autoplay_to_end(&mut self) {
        // Every discard consumes the finite wall; claims/kongs are also bounded by the tile set.
        for _ in 0..2048 {
            if self.finished() {
                return;
            }
            let s = if self.phase == Phase::Claim {
                self.claims[0].seat
            } else {
                self.turn
            };
            if let Some(a) = self.automatic_action(s) {
                let _ = self.act(s, a);
            }
        }
    }
    pub fn view(&self, seat: usize) -> Value {
        event::view(self, self.public_view(), seat)
    }

    pub fn match_seat(&self, seat: usize) -> Option<BoardMatchSeat> {
        event::match_seat(self, seat)
    }

    /// The seat to act in the public match.
    fn public_turn(&self) -> Option<usize> {
        if self.phase == Phase::Claim {
            self.claims.first().map(|c| c.seat)
        } else {
            Some(self.turn)
        }
    }

    pub fn public_view(&self) -> Value {
        let mut view = json!({"id":self.id,"revision":self.revision,"turn":self.public_turn(),"phase":self.phase,"drawn":null,"remaining":self.wall.len(),"last":self.last,"result":self.result,"finished":self.finished(),"legal":[],"claims":[]});
        view["seats"] = event::public_rows(self);
        view
    }

    /// The public body of the ordinary move applied to revision `before`:
    /// every public match field it can change. `result` and `finished` only
    /// change when the match ends, which is a boundary (full frames).
    pub fn move_event(&self, before: u64) -> Option<Box<RawValue>> {
        let (_, seats) = event::moved_since(self, before)?;
        serde_json::value::to_raw_value(&MoveEvent {
            revision: self.revision,
            turn: self.public_turn(),
            phase: &self.phase,
            remaining: self.wall.len(),
            last: &self.last,
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
    type Lists = ();
    type Row<'a> = SeatRow<'a>;
    type Claims<'a> = Vec<ClaimView<'a>>;
    /// `hand` only for the seat itself or a finished match, `waits` only for
    /// the seat itself.
    fn row(&self, seat: usize, own: bool) -> SeatRow<'_> {
        let player = &self.seats[seat];
        let visible = own || self.finished();
        SeatRow {
            hand: visible.then_some(&player.hand),
            count: player.hand.len(),
            flowers: &player.flowers,
            river: &player.river,
            ready: player.ready,
            waits: own.then_some(&player.waits),
            melds: player
                .melds
                .iter()
                .map(|m| {
                    let open = visible || m.kind != Kind::Concealed;
                    MeldView {
                        kind: &m.kind,
                        from: m.from,
                        tiles: m.tiles.iter().map(|&t| open.then_some(t)).collect(),
                    }
                })
                .collect(),
        }
    }
    fn drawn_tile(&self, seat: usize) -> Option<Tile> {
        if seat == self.turn {
            self.drawn
        } else {
            None
        }
    }
    /// The claim choices among `legal`.
    fn claim_choices<'a>(&'a self, legal: &[Action]) -> Option<Vec<ClaimView<'a>>> {
        let claims = legal.iter().filter_map(|a| match a {
            Action::Claim { index } => {
                let c = &self.claims[*index];
                Some(ClaimView {
                    index: *index,
                    kind: &c.kind,
                    tiles: &c.tiles,
                })
            }
            _ => None,
        });
        Some(claims.collect())
    }
    /// A discard clears `drawn` before the claim phase, so only the turn seat
    /// (turn phase) or the first claimant (claim phase) has any.
    fn may_act(&self, s: usize) -> bool {
        match self.phase {
            Phase::Turn => s == self.turn,
            Phase::Claim => self.claims.first().is_some_and(|c| c.seat == s),
            Phase::End => false,
        }
    }
}

#[derive(Serialize)]
pub struct MeldView<'a> {
    kind: &'a Kind,
    from: usize,
    tiles: Vec<Option<Tile>>,
}

#[derive(Serialize)]
pub struct SeatRow<'a> {
    hand: Option<&'a Vec<Tile>>,
    count: usize,
    flowers: &'a [Tile],
    river: &'a [Tile],
    ready: bool,
    waits: Option<&'a Vec<Tile>>,
    melds: Vec<MeldView<'a>>,
}

#[derive(Serialize)]
pub struct ClaimView<'a> {
    index: usize,
    kind: &'a Kind,
    tiles: &'a [Tile],
}

#[derive(Serialize)]
struct MoveEvent<'a> {
    revision: u64,
    turn: Option<usize>,
    phase: &'a Phase,
    remaining: usize,
    last: &'a Option<Last>,
    #[serde(flatten)]
    seats: event::SeatRows<SeatRow<'a>>,
}

#[cfg(test)]
mod clock_tests {
    use super::*;

    fn check_decisions(m: &Match, expected: &[usize]) {
        for seat in 0..4 {
            assert_eq!(m.has_decision(seat), expected.contains(&seat));
            assert_eq!(m.has_decision(seat), !m.legal(seat).is_empty());
        }
    }

    #[test]
    fn clock_follows_discards_kong_only_and_claim_priority() {
        let mut m = Match {
            id: Uuid::nil(),
            revision: 0,
            seats: Default::default(),
            wall: vec![7],
            wall_draws: Default::default(),
            turn: 2,
            phase: Phase::Turn,
            drawn: Some(3),
            replacement: false,
            last: None,
            claims: vec![],
            result: None,
            forbidden: vec![],
            moved: None,
        };
        m.seats[2].hand = vec![3; 4];
        check_decisions(&m, &[2]);
        m.forbidden.push(3);
        assert_eq!(m.legal(2), vec![Action::Kong { tile: 3 }]);
        check_decisions(&m, &[2]);
        m.wall.clear();
        check_decisions(&m, &[]);

        m.seats[2].ready = true;
        m.forbidden.clear();
        check_decisions(&m, &[2]);
        m.drawn = None;
        check_decisions(&m, &[]);

        m.phase = Phase::Claim;
        m.claims = vec![
            Claim {
                seat: 2,
                kind: Kind::Pung,
                tiles: vec![3, 3],
            },
            Claim {
                seat: 1,
                kind: Kind::Chow,
                tiles: vec![1, 2],
            },
        ];
        check_decisions(&m, &[2]);
        m.claims.remove(0);
        check_decisions(&m, &[1]);
        m.claims.clear();
        check_decisions(&m, &[]);
        m.phase = Phase::End;
        check_decisions(&m, &[]);
    }
}
