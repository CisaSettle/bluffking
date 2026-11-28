//! Chinese checkers on the 121-hole star. A move is a step or a chain of
//! symmetric jumps; pieces are never captured. The server enumerates paths.
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::sync::OnceLock;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum Mode {
    #[default]
    Classic,
    Super,
}
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct Point {
    pub q: i8,
    pub r: i8,
}
const DIRS: [(i8, i8); 6] = [(1, 0), (0, 1), (-1, 1), (-1, 0), (0, -1), (1, -1)];
impl Point {
    fn offset(self, d: (i8, i8), n: i8) -> Self {
        Self {
            q: self.q + d.0 * n,
            r: self.r + d.1 * n,
        }
    }
    fn rotate(self) -> Self {
        Self {
            q: -self.r,
            r: self.q + self.r,
        }
    }
    fn distance(self, b: Self) -> i32 {
        ((self.q - b.q).abs() + (self.r - b.r).abs() + ((self.q + self.r) - (b.q + b.r)).abs())
            as i32
            / 2
    }
}
pub fn holes() -> Vec<Point> {
    geometry().points.clone()
}
/// The same directional rays and camps serve every position on the 121-hole star.
struct Geometry {
    points: Vec<Point>,
    /// Contiguous holes along each direction in DIRS order, nearest first.
    rays: Vec<[Vec<u8>; 6]>,
    /// Hole indices and points of each camp, in hole order.
    camp_cells: [Vec<usize>; 6],
    camp_points: [Vec<Point>; 6],
}
fn geometry() -> &'static Geometry {
    static GEOMETRY: OnceLock<Geometry> = OnceLock::new();
    GEOMETRY.get_or_init(|| {
        let points = build_holes();
        let mut index = [u8::MAX; 17 * 17];
        let mut camp_cells: [Vec<usize>; 6] = Default::default();
        let mut camp_points: [Vec<Point>; 6] = Default::default();
        for (i, p) in points.iter().enumerate() {
            index[(p.q + 8) as usize * 17 + (p.r + 8) as usize] = i as u8;
            if let Some(c) = camp(*p) {
                camp_cells[c].push(i);
                camp_points[c].push(*p);
            }
        }
        let rays = points
            .iter()
            .map(|p| {
                DIRS.map(|d| {
                    (1..=16)
                        .map_while(|n| hole_index(&index, p.offset(d, n)))
                        .collect()
                })
            })
            .collect();
        Geometry {
            points,
            rays,
            camp_cells,
            camp_points,
        }
    })
}
fn hole_index(index: &[u8; 17 * 17], p: Point) -> Option<u8> {
    if !(-8..=8).contains(&p.q) || !(-8..=8).contains(&p.r) {
        return None;
    }
    let i = index[(p.q + 8) as usize * 17 + (p.r + 8) as usize];
    (i != u8::MAX).then_some(i)
}
fn build_holes() -> Vec<Point> {
    let mut points = Vec::new();
    for r in -8i8..=8 {
        for q in -8i8..=8 {
            let s = -q - r;
            if [q, r, s].iter().all(|v| v.abs() <= 8)
                && [q, r, s].iter().filter(|v| v.abs() <= 4).count() >= 2
            {
                points.push(Point { q, r });
            }
        }
    }
    points
}
pub fn camp(p: Point) -> Option<usize> {
    let mut v = p;
    for i in 0..6 {
        if v.r < -4 {
            return Some(i);
        }
        v = v.rotate();
    }
    None
}
pub fn camps(players: usize) -> Option<Vec<usize>> {
    match players {
        2 => Some(vec![0, 3]),
        3 => Some(vec![0, 2, 4]),
        4 => Some(vec![0, 1, 3, 4]),
        6 => Some(vec![0, 1, 2, 3, 4, 5]),
        _ => None,
    }
}
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Board {
    pub cells: Vec<Option<usize>>,
    pub camps: Vec<usize>,
}
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct Move {
    pub from: usize,
    pub to: usize,
    pub path: Vec<usize>,
}
const HOLES: usize = 121;

pub fn next_active(turn: usize, resigned: &[bool]) -> usize {
    (1..=resigned.len())
        .map(|offset| (turn + offset) % resigned.len())
        .find(|&seat| !resigned[seat])
        .unwrap_or(turn)
}

/// A destination and the jump tree leading up to it. Only a returned move
/// needs its full path; a step has the origin as its immediate predecessor.
struct Route<'a> {
    from: usize,
    via: usize,
    to: usize,
    parents: &'a [u8; HOLES],
}

impl Route<'_> {
    fn into_move(self) -> Move {
        let mut path = Vec::new();
        path.push(self.to);
        let mut cell = self.via;
        path.push(cell);
        while cell != self.from {
            cell = self.parents[cell] as usize;
            path.push(cell);
        }
        path.reverse();
        Move {
            from: self.from,
            to: self.to,
            path,
        }
    }
}
impl Board {
    pub fn new(players: usize) -> Option<Self> {
        let camps = camps(players)?;
        let cells = holes()
            .iter()
            .map(|p| camp(*p).and_then(|c| camps.iter().position(|v| *v == c)))
            .collect();
        Some(Self { cells, camps })
    }
    pub fn won_with_resigned(&self, seat: usize, resigned: &[bool]) -> bool {
        let mut own = false;
        let filled = geometry().camp_cells[(self.camps[seat] + 3) % 6]
            .iter()
            .all(|&i| match self.cells[i] {
                Some(s) if s == seat => {
                    own = true;
                    true
                }
                Some(s) => resigned.get(s) == Some(&true),
                None => false,
            });
        filled && own
    }
    /// Visit steps in direction order, then the first BFS path to each jump
    /// destination. A visitor returning true has found its requested move.
    fn visit_moves(
        &self,
        seat: usize,
        from: usize,
        mode: Mode,
        mut visit: impl FnMut(Route<'_>) -> bool,
    ) {
        if self.cells.get(from) != Some(&Some(seat)) {
            return;
        }
        let rays = &geometry().rays;
        let occupied = |i: u8| i as usize != from && self.cells[i as usize].is_some();
        let mut parents = [u8::MAX; HOLES];
        parents[from] = from as u8;
        for ray in &rays[from] {
            if let Some(&to) = ray.first() {
                if !occupied(to)
                    && visit(Route {
                        from,
                        via: from,
                        to: to as usize,
                        parents: &parents,
                    })
                {
                    return;
                }
            }
        }

        // Only jump destinations enter the tree. Every hole is queued at most once.
        let mut queue = [0u8; HOLES];
        queue[0] = from as u8;
        let (mut read, mut write) = (0, 1);
        let max_distance = match mode {
            Mode::Classic => 1,
            Mode::Super => 8,
        };
        while read < write {
            let current = queue[read] as usize;
            read += 1;
            for ray in &rays[current] {
                // The landing is twice as far as the first marble. All holes
                // beyond that marble through the landing must be empty.
                let limit = (ray.len() / 2).min(max_distance);
                let Some(mid) = ray[..limit].iter().position(|&i| occupied(i)) else {
                    continue;
                };
                let beyond = &ray[mid + 1..2 * (mid + 1)];
                if beyond.iter().any(|&i| occupied(i)) {
                    continue;
                }
                let to = beyond[beyond.len() - 1] as usize;
                if parents[to] != u8::MAX {
                    continue;
                }
                parents[to] = current as u8;
                if visit(Route {
                    from,
                    via: current,
                    to,
                    parents: &parents,
                }) {
                    return;
                }
                queue[write] = to as u8;
                write += 1;
            }
        }
    }
    pub fn moves(&self, seat: usize, from: usize, mode: Mode) -> Vec<Move> {
        let mut moves = Vec::new();
        self.append_moves(seat, from, mode, &mut moves);
        moves
    }
    fn append_moves(&self, seat: usize, from: usize, mode: Mode, moves: &mut Vec<Move>) {
        self.visit_moves(seat, from, mode, |route| {
            moves.push(route.into_move());
            false
        });
    }
    pub fn all_moves(&self, seat: usize, mode: Mode) -> Vec<Move> {
        let mut moves = Vec::new();
        for from in self.pieces(seat) {
            self.append_moves(seat, from, mode, &mut moves);
        }
        moves
    }
    pub fn play(
        &mut self,
        seat: usize,
        from: usize,
        to: usize,
        mode: Mode,
    ) -> Result<Move, &'static str> {
        let mut found = None;
        self.visit_moves(seat, from, mode, |route| {
            if route.to != to {
                return false;
            }
            found = Some(route.into_move());
            true
        });
        let mv = found.ok_or("illegal_move")?;
        self.cells[from] = None;
        self.cells[to] = Some(seat);
        Ok(mv)
    }
    fn pieces(&self, seat: usize) -> impl Iterator<Item = usize> + '_ {
        self.cells
            .iter()
            .enumerate()
            .filter_map(move |(i, owner)| (*owner == Some(seat)).then_some(i))
    }
    /// Positional cost of `seat` with marble `from` moved to `to` (or the
    /// current board when `from == to`): assignment distance plus a straggler
    /// term, so no marble is left behind while the rest arrive.
    fn cost_after(&self, seat: usize, points: &[Point], from: usize, to: usize) -> i32 {
        let goals = &geometry().camp_points[(self.camps[seat] + 3) % 6];
        let positions: Vec<Point> = self
            .pieces(seat)
            .map(|i| points[if i == from { to } else { i }])
            .collect();
        let straggler = positions
            .iter()
            .map(|p| goals.iter().map(|g| p.distance(*g)).min().unwrap_or(0))
            .max()
            .unwrap_or(0);
        assignment_cost(&positions, goals) + 6 * straggler
    }
    /// The best positional improvement `seat` can make in one move on this
    /// board (0 when it has no move).
    fn best_gain(
        &self,
        seat: usize,
        mode: Mode,
        points: &[Point],
        costs: &mut HashMap<(usize, usize, usize), i32>,
    ) -> i32 {
        let now = *costs
            .entry((seat, usize::MAX, usize::MAX))
            .or_insert_with(|| self.cost_after(seat, points, usize::MAX, usize::MAX));
        let mut best = None;
        for from in self.pieces(seat) {
            self.visit_moves(seat, from, mode, |route| {
                let cost = *costs
                    .entry((seat, from, route.to))
                    .or_insert_with(|| self.cost_after(seat, points, from, route.to));
                let gain = now - cost;
                best = Some(best.map_or(gain, |v: i32| v.max(gain)));
                false
            });
        }
        best.unwrap_or(0)
    }
    /// Two-ply house bot: every own move is scored by the position it reaches,
    /// then the eight best are re-scored by the best single reply each active
    /// opponent would have on the resulting board (bridges we build for them
    /// count against the move). `history` breaks repetition as before.
    pub fn bot_move_lookahead(
        &self,
        seat: usize,
        mode: Mode,
        history: &[Move],
        resigned: &[bool],
    ) -> Option<Move> {
        let points = &geometry().points;
        let recent = &history[history.len().saturating_sub(60)..];
        let repetition = |m: &Move| -> i32 {
            let repeats = recent.iter().filter(|old| old.to == m.to).count() as i32;
            let reverse = recent
                .iter()
                .rev()
                .take(self.camps.len())
                .any(|old| old.from == m.to && old.to == m.from);
            repeats * 45 + if reverse { 80 } else { 0 }
        };
        let mut scored: Vec<(i32, Move)> = self
            .all_moves(seat, mode)
            .into_iter()
            .map(|m| {
                (
                    self.cost_after(seat, points, m.from, m.to) * 100 + repetition(&m),
                    m,
                )
            })
            .collect();
        scored.sort_by(|a, b| {
            a.0.cmp(&b.0)
                .then(a.1.from.cmp(&b.1.from))
                .then(a.1.to.cmp(&b.1.to))
        });
        let opponents: Vec<usize> = (0..self.camps.len())
            .filter(|&o| o != seat && resigned.get(o) != Some(&true))
            .collect();
        // Only our marble moves across these candidates. Each opponent's own
        // pieces/goals stay identical, so positional costs for the same reply
        // can be reused. Legal paths still depend on occupancy and are rebuilt
        // for every candidate. Cache lifetime is one decision, bounded by five
        // opponents * ten pieces * 121 destinations (plus five current costs).
        let mut costs = HashMap::new();
        let baseline: Vec<i32> = opponents
            .iter()
            .map(|&o| self.best_gain(o, mode, points, &mut costs))
            .collect();
        scored
            .iter()
            .take(8)
            .map(|(own, m)| {
                let mut after = self.clone();
                after.cells[m.from] = None;
                after.cells[m.to] = Some(seat);
                // Only what our move changes for them: replies that got
                // better than on the current board are bridges we built.
                let enabled: i32 = opponents
                    .iter()
                    .zip(&baseline)
                    .map(|(&o, &before)| {
                        (after.best_gain(o, mode, points, &mut costs) - before).max(0)
                    })
                    .sum();
                (own + enabled * 70, m.clone())
            })
            .min_by(|a, b| {
                a.0.cmp(&b.0)
                    .then(a.1.from.cmp(&b.1.from))
                    .then(a.1.to.cmp(&b.1.to))
            })
            .map(|(_, m)| m)
    }
}

// Minimum-cost one-to-one assignment prevents all marbles targeting the same
// nearest hole, and gives already-arrived marbles a reason to clear deeper goals.
fn assignment_cost(pieces: &[Point], goals: &[Point]) -> i32 {
    // A camp has ten holes and a seat ten marbles; stack buffers cover that,
    // since bot search calls this thousands of times per move. Larger inputs
    // run the same algorithm on heap buffers.
    const STACK: usize = 16;
    let n = pieces.len();
    let m = goals.len();
    if n == 0 {
        return 0;
    }
    if n < STACK && m < STACK {
        let (mut u, mut v, mut minv) = ([0i32; STACK], [0i32; STACK], [0i32; STACK]);
        let (mut p, mut way, mut used) = ([0usize; STACK], [0usize; STACK], [false; STACK]);
        hungarian(
            pieces,
            goals,
            &mut u[..=n],
            &mut v[..=m],
            &mut p[..=m],
            &mut way[..=m],
            &mut minv[..=m],
            &mut used[..=m],
        )
    } else {
        hungarian(
            pieces,
            goals,
            &mut vec![0; n + 1],
            &mut vec![0; m + 1],
            &mut vec![0; m + 1],
            &mut vec![0; m + 1],
            &mut vec![0; m + 1],
            &mut vec![false; m + 1],
        )
    }
}

#[allow(clippy::too_many_arguments)]
fn hungarian(
    pieces: &[Point],
    goals: &[Point],
    u: &mut [i32],
    v: &mut [i32],
    p: &mut [usize],
    way: &mut [usize],
    minv: &mut [i32],
    used: &mut [bool],
) -> i32 {
    let n = pieces.len();
    for i in 1..=n {
        p[0] = i;
        let mut j0 = 0;
        minv.fill(i32::MAX);
        used.fill(false);
        loop {
            used[j0] = true;
            let i0 = p[j0];
            let mut delta = i32::MAX;
            let mut j1 = 0;
            for j in 1..=goals.len() {
                if !used[j] {
                    let d = pieces[i0 - 1].distance(goals[j - 1]);
                    let cur = d * d - u[i0] - v[j];
                    if cur < minv[j] {
                        minv[j] = cur;
                        way[j] = j0;
                    }
                    if minv[j] < delta {
                        delta = minv[j];
                        j1 = j;
                    }
                }
            }
            for j in 0..=goals.len() {
                if used[j] {
                    u[p[j]] += delta;
                    v[j] -= delta;
                } else {
                    minv[j] = minv[j].saturating_sub(delta);
                }
            }
            j0 = j1;
            if p[j0] == 0 {
                break;
            }
        }
        loop {
            let j1 = way[j0];
            p[j0] = p[j1];
            j0 = j1;
            if j0 == 0 {
                break;
            }
        }
    }
    -v[0]
}
