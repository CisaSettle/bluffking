//! 五子棋 (gomoku) rules — 15×15, free-style.
//!
//! Free-style means the only winning condition is five **or more** of one
//! colour in an unbroken line; there are no forbidden moves (no 三三/四四/长连
//! restriction) and no opening swap. Black always moves first. The board is
//! small enough that the whole position is a plain array — no incremental
//! bitboards, no zobrist hashing.

use serde::{Deserialize, Serialize};

/// Side length of the board. 15×15 is the standard gomoku board.
pub const BOARD: usize = 15;
/// Number of intersections.
pub const CELLS: usize = BOARD * BOARD;
/// Stones in a row needed to win.
pub const WIN_LEN: usize = 5;

/// The four line orientations, as (dx, dy). Each is scanned in both
/// directions, so the opposite four are redundant.
pub const DIRS: [(i32, i32); 4] = [(1, 0), (0, 1), (1, 1), (1, -1)];

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Stone {
    Black,
    White,
}

impl Stone {
    pub fn other(self) -> Stone {
        match self {
            Stone::Black => Stone::White,
            Stone::White => Stone::Black,
        }
    }

    /// Wire/board-string character.
    pub fn ch(self) -> char {
        match self {
            Stone::Black => 'b',
            Stone::White => 'w',
        }
    }
}

/// A point on the board. Both coordinates are 0..BOARD.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct Point {
    pub x: u8,
    pub y: u8,
}

impl Point {
    pub fn new(x: u8, y: u8) -> Point {
        Point { x, y }
    }

    pub fn index(self) -> usize {
        self.y as usize * BOARD + self.x as usize
    }

    pub fn from_index(index: usize) -> Point {
        Point {
            x: (index % BOARD) as u8,
            y: (index / BOARD) as u8,
        }
    }

    pub fn in_bounds(self) -> bool {
        (self.x as usize) < BOARD && (self.y as usize) < BOARD
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MoveError {
    /// Coordinates fall outside the 15×15 grid.
    OutOfBounds,
    /// The intersection already holds a stone.
    Occupied,
}

#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct Board {
    #[serde(with = "super::serialization::fixed_array")]
    cells: [Option<Stone>; CELLS],
    stones: usize,
}

impl Default for Board {
    fn default() -> Board {
        Board::new()
    }
}

impl Board {
    pub fn new() -> Board {
        Board {
            cells: [None; CELLS],
            stones: 0,
        }
    }

    /// Row-major board string; the client renders straight from this.
    pub fn to_wire(&self) -> String {
        self.cells
            .iter()
            .map(|cell| cell.map_or('.', Stone::ch))
            .collect()
    }

    pub fn get(&self, point: Point) -> Option<Stone> {
        if point.in_bounds() {
            self.cells[point.index()]
        } else {
            None
        }
    }

    /// Reads a point given as signed coordinates; out-of-bounds reads as
    /// empty-and-walled, which the line scanners rely on.
    fn at(&self, x: i32, y: i32) -> Option<Option<Stone>> {
        if x < 0 || y < 0 || x >= BOARD as i32 || y >= BOARD as i32 {
            return None;
        }
        Some(self.cells[y as usize * BOARD + x as usize])
    }

    pub fn is_empty(&self) -> bool {
        self.stones == 0
    }

    pub fn is_full(&self) -> bool {
        self.stones == CELLS
    }

    pub fn is_vacant(&self, point: Point) -> bool {
        point.in_bounds() && self.cells[point.index()].is_none()
    }

    /// Places a stone. The caller owns turn order; this only checks the board.
    pub fn place(&mut self, point: Point, stone: Stone) -> Result<(), MoveError> {
        if !point.in_bounds() {
            return Err(MoveError::OutOfBounds);
        }
        if self.cells[point.index()].is_some() {
            return Err(MoveError::Occupied);
        }
        self.cells[point.index()] = Some(stone);
        self.stones += 1;
        Ok(())
    }

    /// Lifts a stone again (used by the bot's search, never by a real move).
    pub fn undo(&mut self, point: Point) {
        if point.in_bounds() && self.cells[point.index()].take().is_some() {
            self.stones -= 1;
        }
    }

    /// If the stone at `point` completes a line of five or more, returns every
    /// point of that line so the client can highlight it. Free-style: an
    /// overline (six or more) wins too.
    pub fn winning_line(&self, point: Point) -> Option<Vec<Point>> {
        let stone = self.get(point)?;
        for (dx, dy) in DIRS {
            let mut line = vec![point];
            for sign in [1, -1] {
                let (sx, sy) = (dx * sign, dy * sign);
                let (mut x, mut y) = (point.x as i32 + sx, point.y as i32 + sy);
                while self.at(x, y) == Some(Some(stone)) {
                    line.push(Point::new(x as u8, y as u8));
                    x += sx;
                    y += sy;
                }
            }
            if line.len() >= WIN_LEN {
                line.sort_by_key(|p| (p.y, p.x));
                return Some(line);
            }
        }
        None
    }

    /// Would placing `stone` at `point` win immediately?
    pub fn wins_at(&mut self, point: Point, stone: Stone) -> bool {
        if !self.is_vacant(point) {
            return false;
        }
        self.cells[point.index()] = Some(stone);
        self.stones += 1;
        let won = self.winning_line(point).is_some();
        self.undo(point);
        won
    }

    /// Empty points within `radius` of an existing stone — the only moves
    /// worth considering. An empty board offers the centre alone.
    pub fn candidates(&self, radius: i32) -> Vec<Point> {
        if self.is_empty() {
            let mid = (BOARD / 2) as u8;
            return vec![Point::new(mid, mid)];
        }
        let mut out = Vec::new();
        for index in 0..CELLS {
            if self.cells[index].is_some() {
                continue;
            }
            let point = Point::from_index(index);
            if self.has_neighbour(point, radius) {
                out.push(point);
            }
        }
        out
    }

    fn has_neighbour(&self, point: Point, radius: i32) -> bool {
        for dy in -radius..=radius {
            for dx in -radius..=radius {
                if dx == 0 && dy == 0 {
                    continue;
                }
                if matches!(
                    self.at(point.x as i32 + dx, point.y as i32 + dy),
                    Some(Some(_))
                ) {
                    return true;
                }
            }
        }
        false
    }

    /// Length of the run through `point` along `dir` plus how many of its two
    /// ends are open. `point` must already hold `stone`.
    pub fn run(&self, point: Point, stone: Stone, dir: (i32, i32)) -> Run {
        let (dx, dy) = dir;
        let mut len = 1;
        let mut open_ends = 0;
        // A gap one step past the run with a friendly stone behind it (a
        // "broken" shape, e.g. ●●.●) is worth nearly as much as a solid run,
        // so it is counted separately.
        let mut gap_ext = 0;
        for sign in [1, -1] {
            let (sx, sy) = (dx * sign, dy * sign);
            let (mut x, mut y) = (point.x as i32 + sx, point.y as i32 + sy);
            while self.at(x, y) == Some(Some(stone)) {
                len += 1;
                x += sx;
                y += sy;
            }
            if self.at(x, y) == Some(None) {
                open_ends += 1;
                let mut behind = 0;
                let (mut bx, mut by) = (x + sx, y + sy);
                while self.at(bx, by) == Some(Some(stone)) {
                    behind += 1;
                    bx += sx;
                    by += sy;
                }
                gap_ext = gap_ext.max(behind);
            }
        }
        Run {
            len,
            open_ends,
            gap_ext,
        }
    }
}

/// One line through a just-placed stone, as seen by the evaluator.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Run {
    /// Stones in the unbroken run, including the placed one.
    pub len: usize,
    /// How many of the run's two ends are empty (0, 1 or 2).
    pub open_ends: usize,
    /// Friendly stones sitting just past an open end, across a one-point gap.
    pub gap_ext: usize,
}
