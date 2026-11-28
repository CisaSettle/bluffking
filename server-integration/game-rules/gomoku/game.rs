//! One 五子棋 match as a pure state machine: the room actor owns it, drives it
//! with [`Match::play`] and reads the result back out. Nothing here touches the
//! database, the clock or the sockets.

use chrono::{DateTime, Utc};
use serde::Serialize;
use uuid::Uuid;

use super::rules::{Board, MoveError, Point, Stone};

/// One placed stone, in order.
#[derive(Debug, Clone, Copy, Serialize, serde::Deserialize)]
pub struct Play {
    /// 1-based move number, as shown on the stone.
    pub n: u32,
    pub seat: usize,
    pub stone: Stone,
    pub x: u8,
    pub y: u8,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum EndReason {
    /// Five or more in a row.
    Five,
    /// The loser gave up.
    Resign,
    /// The loser left, or stayed away past the abandon timeout.
    Abandon,
    /// Every intersection is filled with no line of five.
    BoardFull,
    /// Both players accepted a draw offer.
    Agreement,
}

#[derive(Debug, Clone, Serialize, serde::Deserialize)]
pub struct Outcome {
    /// `None` is a draw; the board filled up.
    pub winner_seat: Option<usize>,
    pub winner_stone: Option<Stone>,
    pub reason: EndReason,
    /// The five (or more) stones that won, for the client to highlight.
    pub line: Vec<Point>,
    pub ended_at: DateTime<Utc>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum PlayError {
    /// The match already has an outcome.
    MatchOver,
    NotYourTurn,
    OutOfBounds,
    Occupied,
}

#[derive(Clone, serde::Serialize, serde::Deserialize)]
pub struct Match {
    pub id: Uuid,
    pub board: Board,
    pub moves: Vec<Play>,
    /// Which seat holds black. Seats swap colours between matches so the
    /// first-move advantage alternates.
    pub black_seat: usize,
    pub turn: Stone,
    pub outcome: Option<Outcome>,
    pub started_at: DateTime<Utc>,
}

impl Match {
    pub fn new(black_seat: usize) -> Match {
        Match {
            id: Uuid::now_v7(),
            board: Board::new(),
            moves: Vec::new(),
            black_seat,
            turn: Stone::Black,
            outcome: None,
            started_at: Utc::now(),
        }
    }

    pub fn seat_of(&self, stone: Stone) -> usize {
        match stone {
            Stone::Black => self.black_seat,
            Stone::White => 1 - self.black_seat,
        }
    }

    pub fn stone_of(&self, seat: usize) -> Stone {
        if seat == self.black_seat {
            Stone::Black
        } else {
            Stone::White
        }
    }

    pub fn finished(&self) -> bool {
        self.outcome.is_some()
    }

    /// The seat to move, or `None` once the match is over.
    pub fn to_act_seat(&self) -> Option<usize> {
        (!self.finished()).then(|| self.seat_of(self.turn))
    }

    pub fn last(&self) -> Option<&Play> {
        self.moves.last()
    }

    pub fn play(&mut self, seat: usize, point: Point) -> Result<(), PlayError> {
        if self.finished() {
            return Err(PlayError::MatchOver);
        }
        if seat != self.seat_of(self.turn) {
            return Err(PlayError::NotYourTurn);
        }
        let stone = self.turn;
        self.board.place(point, stone).map_err(|e| match e {
            MoveError::OutOfBounds => PlayError::OutOfBounds,
            MoveError::Occupied => PlayError::Occupied,
        })?;
        self.moves.push(Play {
            n: self.moves.len() as u32 + 1,
            seat,
            stone,
            x: point.x,
            y: point.y,
        });
        if let Some(line) = self.board.winning_line(point) {
            self.outcome = Some(Outcome {
                winner_seat: Some(seat),
                winner_stone: Some(stone),
                reason: EndReason::Five,
                line,
                ended_at: Utc::now(),
            });
        } else if self.board.is_full() {
            self.outcome = Some(Outcome {
                winner_seat: None,
                winner_stone: None,
                reason: EndReason::BoardFull,
                line: Vec::new(),
                ended_at: Utc::now(),
            });
        } else {
            self.turn = stone.other();
        }
        Ok(())
    }

    /// Ends the match against `seat` — resignation, or the seat abandoning the
    /// room. A match already decided keeps its original outcome.
    pub fn concede(&mut self, seat: usize, reason: EndReason) {
        if self.finished() {
            return;
        }
        let winner = 1 - seat;
        self.outcome = Some(Outcome {
            winner_seat: Some(winner),
            winner_stone: Some(self.stone_of(winner)),
            reason,
            line: Vec::new(),
            ended_at: Utc::now(),
        });
    }

    /// Ends the match drawn by mutual agreement (a human-vs-human draw offer
    /// that the opponent accepted). A decided match keeps its outcome.
    pub fn agree_draw(&mut self) {
        if self.finished() {
            return;
        }
        self.outcome = Some(Outcome {
            winner_seat: None,
            winner_stone: None,
            reason: EndReason::Agreement,
            line: Vec::new(),
            ended_at: Utc::now(),
        });
    }

    /// Takes back the last `count` stones. Used only by the take-back button in
    /// a room where one human plays the house bot, so the human can retract
    /// their move together with the bot's reply.
    pub fn take_back(&mut self, count: usize) -> bool {
        if self.finished() || count == 0 || count > self.moves.len() {
            return false;
        }
        for _ in 0..count {
            let play = self.moves.pop().expect("count <= moves.len()");
            self.board.undo(Point::new(play.x, play.y));
            self.turn = play.stone;
        }
        true
    }

    /// `[[x, y], …]` in move order; the colour follows from the index, because
    /// black always moves first.
    pub fn moves_wire(&self) -> Vec<[u8; 2]> {
        self.moves.iter().map(|p| [p.x, p.y]).collect()
    }
}
