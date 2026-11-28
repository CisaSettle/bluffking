//! 中国象棋 rules: board, move generation, legality, end conditions.
//!
//! Geometry: 9 files × 10 ranks, square index = `rank * 9 + file`.
//! Rank 0 is Black's back rank (drawn at the top), rank 9 is Red's back rank,
//! so Red's pawns move toward lower ranks. The river lies between ranks 4 and
//! 5; the palaces are files 3..=5 on ranks 0..=2 (Black) and 7..=9 (Red).
//!
//! End conditions implemented here:
//!   * checkmate (绝杀) and stalemate (困毙) — both lose for the side to move;
//!   * 60 full moves (120 plies) without a capture → draw (自然限着);
//!   * threefold repetition → perpetual check (长将) loses for the side that
//!     checked on every one of its moves inside the repetition window;
//!     otherwise the repetition is a draw.

use std::sync::OnceLock;

use serde::{Deserialize, Serialize};

pub const FILES: i8 = 9;
pub const RANKS: i8 = 10;
pub const SQUARES: usize = 90;
/// Full moves without a capture that end the game in a draw (自然限着 60 回合).
pub const DRAW_PLIES: u32 = 120;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Side {
    Red,
    Black,
}

impl Side {
    pub const fn other(self) -> Self {
        match self {
            Side::Red => Side::Black,
            Side::Black => Side::Red,
        }
    }
    /// Direction a pawn of this side advances in, in ranks.
    const fn forward(self) -> i8 {
        match self {
            Side::Red => -1,
            Side::Black => 1,
        }
    }
    const fn index(self) -> usize {
        match self {
            Side::Red => 0,
            Side::Black => 1,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, serde::Serialize, serde::Deserialize)]
pub enum Kind {
    King,
    Advisor,
    Elephant,
    Horse,
    Rook,
    Cannon,
    Pawn,
}

impl Kind {
    const ALL: [Kind; 7] = [
        Kind::King,
        Kind::Advisor,
        Kind::Elephant,
        Kind::Horse,
        Kind::Rook,
        Kind::Cannon,
        Kind::Pawn,
    ];
    const fn index(self) -> usize {
        match self {
            Kind::King => 0,
            Kind::Advisor => 1,
            Kind::Elephant => 2,
            Kind::Horse => 3,
            Kind::Rook => 4,
            Kind::Cannon => 5,
            Kind::Pawn => 6,
        }
    }
    /// FEN letter, uppercase. Black pieces are the lowercase form.
    const fn letter(self) -> char {
        match self {
            Kind::King => 'K',
            Kind::Advisor => 'A',
            Kind::Elephant => 'B',
            Kind::Horse => 'N',
            Kind::Rook => 'R',
            Kind::Cannon => 'C',
            Kind::Pawn => 'P',
        }
    }
    fn from_letter(c: char) -> Option<Kind> {
        Kind::ALL
            .into_iter()
            .find(|k| k.letter() == c.to_ascii_uppercase())
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, serde::Serialize, serde::Deserialize)]
pub struct Piece {
    pub side: Side,
    pub kind: Kind,
}

impl Piece {
    fn letter(self) -> char {
        let c = self.kind.letter();
        match self.side {
            Side::Red => c,
            Side::Black => c.to_ascii_lowercase(),
        }
    }
    /// The Chinese glyph, which differs by side for 5 of the 7 kinds.
    pub const fn glyph(self) -> &'static str {
        match (self.side, self.kind) {
            (Side::Red, Kind::King) => "帅",
            (Side::Red, Kind::Advisor) => "仕",
            (Side::Red, Kind::Elephant) => "相",
            (Side::Red, Kind::Horse) => "马",
            (Side::Red, Kind::Rook) => "车",
            (Side::Red, Kind::Cannon) => "炮",
            (Side::Red, Kind::Pawn) => "兵",
            (Side::Black, Kind::King) => "将",
            (Side::Black, Kind::Advisor) => "士",
            (Side::Black, Kind::Elephant) => "象",
            (Side::Black, Kind::Horse) => "馬",
            (Side::Black, Kind::Rook) => "車",
            (Side::Black, Kind::Cannon) => "砲",
            (Side::Black, Kind::Pawn) => "卒",
        }
    }
    /// Glyph used on a score sheet. Black's traditional board forms (車馬砲)
    /// are written in their simplified shape there; the Arabic file numbers
    /// already tell the two sides apart.
    pub const fn notation_glyph(self) -> &'static str {
        match (self.side, self.kind) {
            (Side::Black, Kind::Horse) => "马",
            (Side::Black, Kind::Rook) => "车",
            (Side::Black, Kind::Cannon) => "炮",
            _ => self.glyph(),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct Move {
    pub from: u8,
    pub to: u8,
}

impl Move {
    pub const fn new(from: u8, to: u8) -> Self {
        Move { from, to }
    }
    /// ICCS coordinate notation, e.g. `h2e2`: file letter a..i from Red's left,
    /// rank digit 0..9 counted from Red's back rank.
    pub fn iccs(self) -> String {
        fn one(sq: u8) -> (char, char) {
            let f = (b'a' + (sq % 9)) as char;
            let r = char::from_digit(u32::from(9 - sq / 9), 10).unwrap_or('0');
            (f, r)
        }
        let (ff, fr) = one(self.from);
        let (tf, tr) = one(self.to);
        format!("{ff}{fr}{tf}{tr}")
    }
}

pub const fn file_of(sq: u8) -> i8 {
    (sq % 9) as i8
}
pub const fn rank_of(sq: u8) -> i8 {
    (sq / 9) as i8
}
fn square(file: i8, rank: i8) -> Option<u8> {
    ((0..FILES).contains(&file) && (0..RANKS).contains(&rank)).then(|| (rank * FILES + file) as u8)
}
fn in_palace(side: Side, file: i8, rank: i8) -> bool {
    let ranks = match side {
        Side::Red => 7..=9,
        Side::Black => 0..=2,
    };
    (3..=5).contains(&file) && ranks.contains(&rank)
}
/// Elephants may not cross the river; pawns change their move set when they do.
fn own_half(side: Side, rank: i8) -> bool {
    match side {
        Side::Red => rank >= 5,
        Side::Black => rank <= 4,
    }
}

const HORSE_STEPS: [(i8, i8, i8, i8); 8] = [
    // (file delta, rank delta, leg file, leg rank)
    (1, 2, 0, 1),
    (-1, 2, 0, 1),
    (1, -2, 0, -1),
    (-1, -2, 0, -1),
    (2, 1, 1, 0),
    (2, -1, 1, 0),
    (-2, 1, -1, 0),
    (-2, -1, -1, 0),
];
const ELEPHANT_STEPS: [(i8, i8); 4] = [(2, 2), (2, -2), (-2, 2), (-2, -2)];
const ADVISOR_STEPS: [(i8, i8); 4] = [(1, 1), (1, -1), (-1, 1), (-1, -1)];
const ORTHOGONAL: [(i8, i8); 4] = [(0, 1), (0, -1), (1, 0), (-1, 0)];

/// Zobrist keys: fixed seed, so a position's key is stable across processes and
/// can be compared inside a stored game log.
struct Zobrist {
    pieces: [[[u64; SQUARES]; 7]; 2],
    black_to_move: u64,
}

fn zobrist() -> &'static Zobrist {
    static Z: OnceLock<Zobrist> = OnceLock::new();
    Z.get_or_init(|| {
        let mut state: u64 = 0x9E37_79B9_7F4A_7C15;
        let mut next = move || {
            state = state.wrapping_add(0x9E37_79B9_7F4A_7C15);
            let mut z = state;
            z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
            z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
            z ^ (z >> 31)
        };
        let mut pieces = [[[0u64; SQUARES]; 7]; 2];
        for side in &mut pieces {
            for kind in side.iter_mut() {
                for sq in kind.iter_mut() {
                    *sq = next();
                }
            }
        }
        Zobrist {
            pieces,
            black_to_move: next(),
        }
    })
}

#[derive(Debug, Clone, Copy)]
pub struct Undo {
    mv: Move,
    captured: Option<Piece>,
    key: u64,
}

#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct Board {
    #[serde(with = "super::serialization::fixed_array")]
    squares: [Option<Piece>; SQUARES],
    pub turn: Side,
    key: u64,
}

impl Default for Board {
    fn default() -> Self {
        Board::start()
    }
}

impl Board {
    pub fn empty() -> Self {
        let mut b = Board {
            squares: [None; SQUARES],
            turn: Side::Red,
            key: 0,
        };
        b.key = b.compute_key();
        b
    }

    /// The opening position; Red moves first.
    pub fn start() -> Self {
        Self::from_fen("rnbakabnr/9/1c5c1/p1p1p1p1p/9/9/P1P1P1P1P/1C5C1/9/RNBAKABNR w")
            .expect("valid start position")
    }

    pub fn piece_at(&self, sq: u8) -> Option<Piece> {
        self.squares[sq as usize]
    }

    pub fn key(&self) -> u64 {
        self.key
    }

    fn compute_key(&self) -> u64 {
        let z = zobrist();
        let mut key = 0;
        for (sq, piece) in self.squares.iter().enumerate() {
            if let Some(p) = piece {
                key ^= z.pieces[p.side.index()][p.kind.index()][sq];
            }
        }
        if self.turn == Side::Black {
            key ^= z.black_to_move;
        }
        key
    }

    fn set(&mut self, sq: u8, piece: Option<Piece>) {
        let z = zobrist();
        if let Some(p) = self.squares[sq as usize] {
            self.key ^= z.pieces[p.side.index()][p.kind.index()][sq as usize];
        }
        if let Some(p) = piece {
            self.key ^= z.pieces[p.side.index()][p.kind.index()][sq as usize];
        }
        self.squares[sq as usize] = piece;
    }

    /// Standard Xiangqi FEN: ranks from Black's back rank down, uppercase Red,
    /// then `w`/`b` for the side to move.
    pub fn fen(&self) -> String {
        let mut out = String::with_capacity(90);
        for rank in 0..RANKS {
            if rank > 0 {
                out.push('/');
            }
            let mut empty = 0;
            for file in 0..FILES {
                let sq = (rank * FILES + file) as u8;
                match self.squares[sq as usize] {
                    Some(p) => {
                        if empty > 0 {
                            out.push(char::from_digit(empty, 10).unwrap_or('0'));
                            empty = 0;
                        }
                        out.push(p.letter());
                    }
                    None => empty += 1,
                }
            }
            if empty > 0 {
                out.push(char::from_digit(empty, 10).unwrap_or('0'));
            }
        }
        out.push(' ');
        out.push(if self.turn == Side::Red { 'w' } else { 'b' });
        out
    }

    pub fn from_fen(fen: &str) -> Option<Board> {
        let mut parts = fen.split_whitespace();
        let placement = parts.next()?;
        let turn = match parts.next() {
            Some("b") => Side::Black,
            _ => Side::Red,
        };
        let mut board = Board::empty();
        board.turn = turn;
        let mut rank = 0i8;
        for row in placement.split('/') {
            if rank >= RANKS {
                return None;
            }
            let mut file = 0i8;
            for c in row.chars() {
                if let Some(skip) = c.to_digit(10) {
                    file += skip as i8;
                } else {
                    let kind = Kind::from_letter(c)?;
                    let side = if c.is_ascii_uppercase() {
                        Side::Red
                    } else {
                        Side::Black
                    };
                    let sq = square(file, rank)?;
                    board.squares[sq as usize] = Some(Piece { side, kind });
                    file += 1;
                }
            }
            if file != FILES {
                return None;
            }
            rank += 1;
        }
        if rank != RANKS {
            return None;
        }
        board.key = board.compute_key();
        Some(board)
    }

    pub fn king_square(&self, side: Side) -> Option<u8> {
        self.squares
            .iter()
            .position(|p| {
                *p == Some(Piece {
                    side,
                    kind: Kind::King,
                })
            })
            .map(|i| i as u8)
    }

    /// True when the two kings share a file with nothing between them
    /// (将帅照面): the position is illegal for whoever created it.
    pub fn kings_face(&self) -> bool {
        self.kings_face_at(self.king_square(Side::Red), self.king_square(Side::Black))
    }

    fn kings_face_at(&self, first: Option<u8>, second: Option<u8>) -> bool {
        let (Some(a), Some(b)) = (first, second) else {
            return false;
        };
        if file_of(a) != file_of(b) {
            return false;
        }
        let file = file_of(a);
        let (lo, hi) = (rank_of(a).min(rank_of(b)), rank_of(a).max(rank_of(b)));
        (lo + 1..hi).all(|r| square(file, r).is_some_and(|sq| self.squares[sq as usize].is_none()))
    }

    /// Is `sq` attacked by any piece of `by`? Advisors and elephants are
    /// excluded on purpose: neither can leave its own half and palace, so
    /// neither can ever attack the enemy king.
    fn attacks_square(&self, by: Side, sq: u8) -> bool {
        let (file, rank) = (file_of(sq), rank_of(sq));
        // Rooks and cannons along the four rays.
        for (df, dr) in ORTHOGONAL {
            let mut screens = 0;
            let (mut f, mut r) = (file + df, rank + dr);
            while let Some(cur) = square(f, r) {
                if let Some(p) = self.squares[cur as usize] {
                    if p.side == by
                        && matches!((screens, p.kind), (0, Kind::Rook) | (1, Kind::Cannon))
                    {
                        return true;
                    }
                    screens += 1;
                    if screens > 1 {
                        break;
                    }
                }
                f += df;
                r += dr;
            }
        }
        // Horses, checked from the target backwards: the leg square is the one
        // next to the horse, i.e. next to the *origin*.
        for (df, dr, lf, lr) in HORSE_STEPS {
            let Some(origin) = square(file + df, rank + dr) else {
                continue;
            };
            if self.squares[origin as usize]
                != Some(Piece {
                    side: by,
                    kind: Kind::Horse,
                })
            {
                continue;
            }
            // Leg of a horse standing on `origin` moving to `sq`.
            let leg = square(file + df - lf, rank + dr - lr);
            if leg.is_some_and(|l| self.squares[l as usize].is_none()) {
                return true;
            }
        }
        // Pawns: an enemy pawn attacks the squares it can step onto.
        for (df, dr) in [(0, -by.forward()), (1, 0), (-1, 0)] {
            let Some(origin) = square(file + df, rank + dr) else {
                continue;
            };
            if self.squares[origin as usize]
                != Some(Piece {
                    side: by,
                    kind: Kind::Pawn,
                })
            {
                continue;
            }
            // Sideways capture only once the pawn has crossed the river.
            if dr == 0 && own_half(by, rank_of(origin)) {
                continue;
            }
            return true;
        }
        false
    }

    pub fn in_check(&self, side: Side) -> bool {
        self.in_check_at(side, self.king_square(side), self.king_square(side.other()))
    }

    fn in_check_at(&self, side: Side, king: Option<u8>, opponent: Option<u8>) -> bool {
        self.kings_face_at(king, opponent)
            || king.is_some_and(|k| self.attacks_square(side.other(), k))
    }

    /// Pseudo-legal moves for the side to move: geometry only, king safety and
    /// the facing rule are applied by [`Board::legal_moves`].
    pub fn pseudo_moves(&self) -> Vec<Move> {
        let mut out = Vec::with_capacity(48);
        for from in 0..SQUARES as u8 {
            let Some(piece) = self.squares[from as usize] else {
                continue;
            };
            if piece.side != self.turn {
                continue;
            }
            self.pseudo_moves_from(from, piece, &mut out);
        }
        out
    }

    fn pseudo_moves_from(&self, from: u8, piece: Piece, out: &mut Vec<Move>) {
        let (file, rank) = (file_of(from), rank_of(from));
        let side = piece.side;
        let push = |to: u8, out: &mut Vec<Move>| {
            if self.squares[to as usize].is_none_or(|p| p.side != side) {
                out.push(Move::new(from, to));
            }
        };
        match piece.kind {
            Kind::King => {
                for (df, dr) in ORTHOGONAL {
                    let (f, r) = (file + df, rank + dr);
                    if in_palace(side, f, r) {
                        if let Some(to) = square(f, r) {
                            push(to, out);
                        }
                    }
                }
            }
            Kind::Advisor => {
                for (df, dr) in ADVISOR_STEPS {
                    let (f, r) = (file + df, rank + dr);
                    if in_palace(side, f, r) {
                        if let Some(to) = square(f, r) {
                            push(to, out);
                        }
                    }
                }
            }
            Kind::Elephant => {
                for (df, dr) in ELEPHANT_STEPS {
                    let (f, r) = (file + df, rank + dr);
                    if !own_half(side, r) {
                        continue; // 象不过河
                    }
                    let Some(to) = square(f, r) else { continue };
                    // 塞象眼: the midpoint must be empty.
                    let eye = square(file + df / 2, rank + dr / 2);
                    if eye.is_some_and(|e| self.squares[e as usize].is_some()) {
                        continue;
                    }
                    push(to, out);
                }
            }
            Kind::Horse => {
                for (df, dr, lf, lr) in HORSE_STEPS {
                    let Some(to) = square(file + df, rank + dr) else {
                        continue;
                    };
                    // 蹩马腿: the orthogonal square the horse steps over.
                    let leg = square(file + lf, rank + lr);
                    if leg.is_none_or(|l| self.squares[l as usize].is_some()) {
                        continue;
                    }
                    push(to, out);
                }
            }
            Kind::Rook | Kind::Cannon => {
                let cannon = piece.kind == Kind::Cannon;
                for (df, dr) in ORTHOGONAL {
                    let (mut f, mut r) = (file + df, rank + dr);
                    let mut jumped = false;
                    while let Some(to) = square(f, r) {
                        match self.squares[to as usize] {
                            None => {
                                if !jumped {
                                    out.push(Move::new(from, to));
                                }
                            }
                            Some(p) => {
                                if cannon && !jumped {
                                    // 炮: the first piece in the ray is the screen.
                                    jumped = true;
                                } else {
                                    if p.side != side {
                                        out.push(Move::new(from, to));
                                    }
                                    break;
                                }
                            }
                        }
                        f += df;
                        r += dr;
                    }
                }
            }
            Kind::Pawn => {
                let forward = side.forward();
                if let Some(to) = square(file, rank + forward) {
                    push(to, out);
                }
                // Sideways only after crossing the river.
                if !own_half(side, rank) {
                    for df in [-1, 1] {
                        if let Some(to) = square(file + df, rank) {
                            push(to, out);
                        }
                    }
                }
            }
        }
    }

    /// Applies a move without checking legality. Use [`Board::undo`] to revert.
    pub fn apply(&mut self, mv: Move) -> Undo {
        let key = self.key;
        let captured = self.squares[mv.to as usize];
        let piece = self.squares[mv.from as usize];
        self.set(mv.to, piece);
        self.set(mv.from, None);
        self.turn = self.turn.other();
        self.key ^= zobrist().black_to_move;
        Undo { mv, captured, key }
    }

    pub fn undo(&mut self, undo: Undo) {
        let piece = self.squares[undo.mv.to as usize];
        self.squares[undo.mv.from as usize] = piece;
        self.squares[undo.mv.to as usize] = undo.captured;
        self.turn = self.turn.other();
        self.key = undo.key;
    }

    /// Pseudo-legal moves that do not leave the mover in check and do not put
    /// the kings face to face.
    pub fn legal_moves(&self) -> Vec<Move> {
        self.iter_legal_moves().collect()
    }

    fn iter_legal_moves(&self) -> impl Iterator<Item = Move> {
        let side = self.turn;
        let king = self.king_square(side);
        let opponent = self.king_square(side.other());
        let mut board = self.clone();
        self.pseudo_moves().into_iter().filter(move |mv| {
            let undo = board.apply(*mv);
            // Only moving or capturing a king can change its lookup on this trial board.
            let own = if board.piece_at(mv.to).is_some_and(|p| p.kind == Kind::King) {
                board.king_square(side)
            } else {
                king
            };
            let enemy = if undo.captured.is_some_and(|p| p.kind == Kind::King) {
                board.king_square(side.other())
            } else {
                opponent
            };
            let ok = !board.in_check_at(side, own, enemy);
            board.undo(undo);
            ok
        })
    }

    /// Safe on untrusted input: a client-sent square outside the board is
    /// rejected before anything indexes `squares`.
    pub fn is_legal(&self, mv: Move) -> bool {
        if usize::from(mv.from) >= SQUARES || usize::from(mv.to) >= SQUARES {
            return false;
        }
        let Some(piece) = self.piece_at(mv.from).filter(|p| p.side == self.turn) else {
            return false;
        };
        // At most eight squares on the same rank and nine on the same file.
        let mut candidates = Vec::with_capacity((FILES + RANKS - 2) as usize);
        self.pseudo_moves_from(mv.from, piece, &mut candidates);
        if !candidates.contains(&mv) {
            return false;
        }
        let mut board = self.clone();
        board.apply(mv);
        !board.in_check(self.turn)
    }
}

/// Why a game ended. `reason` values are stable wire strings.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum Outcome {
    /// `winner` won; `reason` is one of `checkmate` / `stalemate` /
    /// `perpetual_check` / `resign` / `timeout` / `abandon`.
    Win { winner: Side, reason: &'static str },
    /// `reason` is `move_limit` / `repetition` / `agreement` / `material`.
    Draw { reason: &'static str },
}

impl<'de> serde::Deserialize<'de> for Outcome {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        #[derive(serde::Deserialize)]
        #[serde(tag = "kind", rename_all = "snake_case")]
        enum Stored {
            Win { winner: Side, reason: String },
            Draw { reason: String },
        }
        match Stored::deserialize(deserializer)? {
            Stored::Win { winner, reason } => Ok(Self::Win {
                winner,
                reason: super::serialization::chess_reason_value(&reason)
                    .ok_or_else(|| serde::de::Error::custom("invalid chess outcome reason"))?,
            }),
            Stored::Draw { reason } => Ok(Self::Draw {
                reason: super::serialization::chess_reason_value(&reason)
                    .ok_or_else(|| serde::de::Error::custom("invalid chess outcome reason"))?,
            }),
        }
    }
}

/// One position seen in a game, for repetition and perpetual-check detection.
#[derive(Debug, Clone, Copy, serde::Serialize, serde::Deserialize)]
struct Seen {
    key: u64,
    /// The move that produced this position gave check.
    check: bool,
    /// The side that made that move.
    mover: Side,
}

/// A whole game: the board plus the history the draw and 长将 rules need.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct Game {
    pub board: Board,
    /// Plies since the last capture, for the 自然限着 draw.
    pub quiet_plies: u32,
    pub moves: Vec<MoveRecord>,
    pub outcome: Option<Outcome>,
    seen: Vec<Seen>,
}

#[derive(Debug, Clone, Serialize, serde::Deserialize)]
pub struct MoveRecord {
    pub from: u8,
    pub to: u8,
    /// ICCS coordinates, e.g. `h2e2`.
    pub iccs: String,
    /// Chinese notation, e.g. 炮二平五.
    pub zh: String,
    pub side: Side,
    pub capture: bool,
    pub check: bool,
}

impl Default for Game {
    fn default() -> Self {
        Game::new()
    }
}

impl Game {
    pub fn new() -> Self {
        let board = Board::start();
        let seen = vec![Seen {
            key: board.key(),
            check: false,
            mover: Side::Black,
        }];
        Game {
            board,
            quiet_plies: 0,
            moves: Vec::new(),
            outcome: None,
            seen,
        }
    }

    pub fn turn(&self) -> Side {
        self.board.turn
    }

    pub fn finished(&self) -> bool {
        self.outcome.is_some()
    }

    pub fn legal_moves(&self) -> Vec<Move> {
        if self.finished() {
            return Vec::new();
        }
        self.board.legal_moves()
    }

    /// Plays `mv` if it is legal, then re-evaluates the end conditions.
    /// Returns `false` (and changes nothing) when the move is illegal or the
    /// game is already over.
    pub fn play(&mut self, mv: Move) -> bool {
        if self.finished() || !self.board.is_legal(mv) {
            return false;
        }
        let mover = self.board.turn;
        let zh = notation::chinese(&self.board, mv);
        let captured = self.board.piece_at(mv.to).is_some();
        self.board.apply(mv);
        let check = self.board.in_check(mover.other());
        self.quiet_plies = if captured { 0 } else { self.quiet_plies + 1 };
        self.moves.push(MoveRecord {
            from: mv.from,
            to: mv.to,
            iccs: mv.iccs(),
            zh,
            side: mover,
            capture: captured,
            check,
        });
        self.seen.push(Seen {
            key: self.board.key(),
            check,
            mover,
        });
        // A capture makes every earlier position unreachable, so repetition
        // only ever looks back to the last irreversible move.
        if captured {
            let last = *self.seen.last().expect("just pushed");
            self.seen.clear();
            self.seen.push(last);
        }
        self.outcome = self.detect_outcome();
        true
    }

    /// Ends the game for an out-of-band reason (resign, timeout, agreement).
    pub fn finish(&mut self, outcome: Outcome) {
        if self.outcome.is_none() {
            self.outcome = Some(outcome);
        }
    }

    fn detect_outcome(&self) -> Option<Outcome> {
        let to_move = self.board.turn;
        if self.board.iter_legal_moves().next().is_none() {
            // Both 绝杀 (mate) and 困毙 (stalemate) lose for the side to move.
            let reason = if self.board.in_check(to_move) {
                "checkmate"
            } else {
                "stalemate"
            };
            return Some(Outcome::Win {
                winner: to_move.other(),
                reason,
            });
        }
        if let Some(outcome) = self.repetition_outcome() {
            return Some(outcome);
        }
        if self.quiet_plies >= DRAW_PLIES {
            return Some(Outcome::Draw {
                reason: "move_limit",
            });
        }
        if self.insufficient_material() {
            return Some(Outcome::Draw { reason: "material" });
        }
        None
    }

    /// Threefold repetition. A side that checked on *every* one of its moves
    /// inside the repeated window is perpetually checking (长将) and loses; if
    /// both or neither did, the repetition is a draw.
    fn repetition_outcome(&self) -> Option<Outcome> {
        let current = self.seen.last()?;
        let occurrences: Vec<usize> = self
            .seen
            .iter()
            .enumerate()
            .filter(|(_, s)| s.key == current.key)
            .map(|(i, _)| i)
            .collect();
        if occurrences.len() < 3 {
            return None;
        }
        let window = &self.seen[occurrences[0] + 1..];
        let mut perpetual = Vec::new();
        for side in [Side::Red, Side::Black] {
            let mut moved = false;
            let mut always_check = true;
            for s in window.iter().filter(|s| s.mover == side) {
                moved = true;
                always_check &= s.check;
            }
            if moved && always_check {
                perpetual.push(side);
            }
        }
        Some(match perpetual.as_slice() {
            [side] => Outcome::Win {
                winner: side.other(),
                reason: "perpetual_check",
            },
            _ => Outcome::Draw {
                reason: "repetition",
            },
        })
    }

    /// Kings, advisors and elephants alone can never mate: the game is drawn.
    fn insufficient_material(&self) -> bool {
        (0..SQUARES as u8)
            .filter_map(|sq| self.board.piece_at(sq))
            .all(|p| matches!(p.kind, Kind::King | Kind::Advisor | Kind::Elephant))
    }
}

/// Chinese move notation (炮二平五).
pub mod notation {
    use super::{file_of, rank_of, Board, Kind, Move, Piece, Side, RANKS};

    const RED_DIGITS: [&str; 9] = ["一", "二", "三", "四", "五", "六", "七", "八", "九"];
    const BLACK_DIGITS: [&str; 9] = ["1", "2", "3", "4", "5", "6", "7", "8", "9"];

    /// File label as that side counts files: Red counts 一..九 from its right
    /// (board file 8 down to 0), Black counts 1..9 from its own right
    /// (board file 0 up to 8).
    fn file_label(side: Side, file: i8) -> &'static str {
        match side {
            Side::Red => RED_DIGITS[(8 - file) as usize],
            Side::Black => BLACK_DIGITS[file as usize],
        }
    }

    fn count_label(side: Side, n: i8) -> &'static str {
        let idx = (n.clamp(1, 9) - 1) as usize;
        match side {
            Side::Red => RED_DIGITS[idx],
            Side::Black => BLACK_DIGITS[idx],
        }
    }

    /// Positional prefix when several identical pieces share a file: 前/后 for
    /// two, and 一..五 counted from the front for a file of three or more
    /// (pawns). The plain `前/后/中` triple is used for exactly three.
    fn prefix(board: &Board, piece: Piece, from: u8) -> Option<&'static str> {
        let file = file_of(from);
        let mut same: Vec<u8> = (0..RANKS)
            .filter_map(|r| {
                let sq = (r * 9 + file) as u8;
                (board.piece_at(sq) == Some(piece)).then_some(sq)
            })
            .collect();
        if same.len() < 2 {
            return None;
        }
        // Order from the side's own front (furthest advanced) to its rear.
        same.sort_by_key(|sq| match piece.side {
            Side::Red => rank_of(*sq),
            Side::Black => -rank_of(*sq),
        });
        let idx = same.iter().position(|sq| *sq == from)?;
        Some(match (same.len(), idx) {
            (2, 0) => "前",
            (2, _) => "后",
            (3, 0) => "前",
            (3, 1) => "中",
            (3, _) => "后",
            (_, i) => ["前", "二", "三", "四", "后"][i.min(4)],
        })
    }

    /// Renders `mv` on `board` (the position *before* the move) in Chinese
    /// notation. Returns ICCS coordinates if the origin square is empty.
    pub fn chinese(board: &Board, mv: Move) -> String {
        let Some(piece) = board.piece_at(mv.from) else {
            return mv.iccs();
        };
        let side = piece.side;
        let (from_file, from_rank) = (file_of(mv.from), rank_of(mv.from));
        let (to_file, to_rank) = (file_of(mv.to), rank_of(mv.to));
        let prefix = prefix(board, piece, mv.from);
        // With a 前/后 prefix the piece name replaces the origin file.
        let head = match prefix {
            Some(p) => format!("{p}{}", piece.notation_glyph()),
            None => format!("{}{}", piece.notation_glyph(), file_label(side, from_file)),
        };
        // Advance is toward the opponent: Red's ranks decrease, Black's grow.
        let advance = (from_rank - to_rank) * -side.forward();
        if to_file == from_file {
            let verb = if advance > 0 { "进" } else { "退" };
            return format!("{head}{verb}{}", count_label(side, advance.abs()));
        }
        if advance == 0 {
            return format!("{head}平{}", file_label(side, to_file));
        }
        // Diagonal movers name the destination file, not the rank count.
        let verb = if advance > 0 { "进" } else { "退" };
        let tail = match piece.kind {
            Kind::Horse | Kind::Elephant | Kind::Advisor => file_label(side, to_file),
            _ => count_label(side, advance.abs()),
        };
        format!("{head}{verb}{tail}")
    }
}
