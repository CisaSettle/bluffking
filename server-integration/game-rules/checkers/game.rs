use super::rules::{Board, Mode, Move};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

#[derive(Clone, Serialize, Deserialize)]
pub struct Match {
    pub id: Uuid,
    pub board: Board,
    pub mode: Mode,
    pub turn: usize,
    pub revision: u64,
    pub moves: Vec<Move>,
    pub resigned: Vec<bool>,
    pub finished: bool,
    pub winner: Option<usize>,
    pub reason: String,
    /// The seat whose turn the last move ended (runtime only): its legal list
    /// emptied, so the move event carries its private body.
    #[serde(skip)]
    pub passed: Option<usize>,
}
impl Match {
    pub fn new(players: usize, mode: Mode, first: usize) -> Option<Self> {
        Some(Self {
            id: Uuid::now_v7(),
            board: Board::new(players)?,
            mode,
            turn: first % players,
            revision: 0,
            moves: vec![],
            resigned: vec![false; players],
            finished: false,
            winner: None,
            reason: String::new(),
            passed: None,
        })
    }
    fn next(&mut self) {
        self.passed = Some(self.turn);
        for n in 1..=self.resigned.len() {
            let seat = (self.turn + n) % self.resigned.len();
            if !self.resigned[seat] {
                self.turn = seat;
                break;
            }
        }
    }
    pub fn play(&mut self, seat: usize, from: usize, to: usize) -> Result<(), &'static str> {
        if self.finished {
            return Err("match_over");
        }
        if seat != self.turn {
            return Err("not_your_turn");
        }
        self.moves.push(self.board.play(seat, from, to, self.mode)?);
        self.revision += 1;
        if self.board.won_with_resigned(seat, &self.resigned) {
            self.finished = true;
            self.winner = Some(seat);
            self.reason = "home".into();
        } else if self.moves.len() >= 3000 {
            self.finished = true;
            self.reason = "move_limit".into();
        } else {
            self.next();
        }
        Ok(())
    }
    pub fn resign(&mut self, seat: usize) {
        if self.finished || seat >= self.resigned.len() || self.resigned[seat] {
            return;
        }
        self.resigned[seat] = true;
        self.revision += 1;
        // Retain marbles as bridges; an abandoned seat can never block a turn.
        let active: Vec<_> = (0..self.resigned.len())
            .filter(|i| !self.resigned[*i])
            .collect();
        if active.len() <= 1 {
            self.finished = true;
            self.winner = active.first().copied();
            self.reason = "resign".into();
        } else if let Some(winner) = active
            .iter()
            .copied()
            .find(|s| self.board.won_with_resigned(*s, &self.resigned))
        {
            self.finished = true;
            self.winner = Some(winner);
            self.reason = "home".into();
        } else if self.turn == seat {
            self.next();
        } else {
            // Nobody's turn ended: no seat's legal list changed.
            self.passed = None;
        }
    }
    pub fn automatic(&mut self) {
        if self.finished {
            return;
        }
        if let Some(m) =
            self.board
                .bot_move_lookahead(self.turn, self.mode, &self.moves, &self.resigned)
        {
            let _ = self.play(self.turn, m.from, m.to);
        } else {
            self.resign(self.turn);
        }
    }
}
