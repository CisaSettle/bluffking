//! Deterministic board-game rules used by the server and the offline verifier.
pub mod checkers;
pub mod gomoku;
mod serialization;
pub mod xiangqi;

use serde::{Deserialize, Serialize};
use serde_json::{json, Value};

#[derive(Deserialize, Serialize)]
#[serde(tag = "game", rename_all = "snake_case")]
pub enum Replay {
    Gomoku {
        plays: Vec<[u8; 2]>,
        board: String,
        winner: Option<gomoku::Stone>,
        reason: String,
    },
    Xiangqi {
        plays: Vec<[u8; 2]>,
        fen: String,
        winner: Option<xiangqi::Side>,
        reason: String,
    },
    Checkers {
        players: usize,
        first: usize,
        mode: checkers::Mode,
        plays: Vec<[usize; 2]>,
        /// Each entry records a seat resigning after a given count of moves.
        resignations: Vec<[usize; 2]>,
        board: Vec<Option<usize>>,
        winner: Option<usize>,
        reason: String,
    },
}

fn outcome<T: Eq + Copy>(
    computed: Option<(Option<T>, &str)>,
    winner: Option<T>,
    reason: &str,
    external_allowed: bool,
) -> Result<&'static str, String> {
    match computed {
        Some((actual_winner, actual_reason))
            if actual_winner == winner && actual_reason == reason =>
        {
            Ok("rules")
        }
        None if external_allowed => Ok("reported_event"),
        _ => Err("Claimed board result differs from the replay".into()),
    }
}

pub fn verify(proof: &Value) -> Result<Value, String> {
    if proof["protocol"] != "board-replay-v1" {
        return Err("Unsupported board replay protocol".into());
    }
    let replay: Replay =
        serde_json::from_value(proof["replay"].clone()).map_err(|_| "Invalid board replay")?;
    let (game, moves, result_scope) = match replay {
        Replay::Gomoku {
            plays,
            board,
            winner,
            reason,
        } => {
            if plays.len() > gomoku::CELLS {
                return Err("Too many Gomoku moves".into());
            }
            let mut position = gomoku::Board::new();
            let mut stone = gomoku::Stone::Black;
            let mut ended = None;
            for [x, y] in &plays {
                if ended.is_some() {
                    return Err("Move after the game finished".into());
                }
                let point = gomoku::Point::new(*x, *y);
                position
                    .place(point, stone)
                    .map_err(|_| "Illegal Gomoku move")?;
                if position.winning_line(point).is_some() {
                    ended = Some((Some(stone), "five"));
                } else if position.is_full() {
                    ended = Some((None, "board_full"));
                }
                stone = stone.other();
            }
            if position.to_wire() != board {
                return Err("Final Gomoku board differs from the replay".into());
            }
            let reported = match reason.as_str() {
                "resign" | "abandon" => winner.is_some(),
                "agreement" => winner.is_none(),
                _ => false,
            };
            (
                "gomoku",
                plays.len(),
                outcome(ended, winner, &reason, reported)?,
            )
        }
        Replay::Xiangqi {
            plays,
            fen,
            winner,
            reason,
        } => {
            // A legal game with this capture and quiet-ply limit is finite.
            if plays.len() > 4_000 {
                return Err("Too many Xiangqi moves".into());
            }
            let mut game = xiangqi::Game::new();
            for &[from, to] in &plays {
                if usize::from(from) >= xiangqi::SQUARES
                    || usize::from(to) >= xiangqi::SQUARES
                    || !game.play(xiangqi::Move::new(from, to))
                {
                    return Err("Illegal Xiangqi move".into());
                }
            }
            if game.board.fen() != fen {
                return Err("Final Xiangqi board differs from the replay".into());
            }
            let ended = game.outcome.map(|value| match value {
                xiangqi::Outcome::Win { winner, reason } => (Some(winner), reason),
                xiangqi::Outcome::Draw { reason } => (None, reason),
            });
            let reported = match reason.as_str() {
                "resign" | "timeout" | "abandon" => winner.is_some(),
                "agreement" => winner.is_none(),
                _ => false,
            };
            (
                "xiangqi",
                plays.len(),
                outcome(ended, winner, &reason, reported)?,
            )
        }
        Replay::Checkers {
            players,
            first,
            mode,
            plays,
            resignations,
            board,
            winner,
            reason,
        } => {
            if first >= players || plays.len() > 3_000 || resignations.len() > players {
                return Err("Invalid Chinese Checkers replay limits".into());
            }
            let mut position = checkers::Board::new(players).ok_or("Invalid player count")?;
            let mut turn = first;
            let mut resigned = vec![false; players];
            let mut ended = None;
            let mut resignations = resignations.into_iter().peekable();
            for count in 0..=plays.len() {
                while let Some([at, seat]) = resignations.peek().copied().filter(|r| r[0] <= count)
                {
                    resignations.next();
                    if ended.is_some() || at != count || seat >= players || resigned[seat] {
                        return Err("Invalid Chinese Checkers resignation".into());
                    }
                    resigned[seat] = true;
                    let mut active = (0..players).filter(|&s| !resigned[s]);
                    let first_active = active.next();
                    if active.next().is_none() {
                        ended = Some((first_active, "resign"));
                    } else if let Some(winner) = (0..players)
                        .find(|&s| !resigned[s] && position.won_with_resigned(s, &resigned))
                    {
                        ended = Some((Some(winner), "home"));
                    } else if turn == seat {
                        turn = checkers::next_active(turn, &resigned);
                    }
                }
                let Some(&[from, to]) = plays.get(count) else {
                    break;
                };
                if ended.is_some() {
                    return Err("Move after the game finished".into());
                }
                position
                    .play(turn, from, to, mode)
                    .map_err(|_| "Illegal Chinese Checkers move")?;
                if position.won_with_resigned(turn, &resigned) {
                    ended = Some((Some(turn), "home"));
                } else if count + 1 == 3_000 {
                    ended = Some((None, "move_limit"));
                } else {
                    turn = checkers::next_active(turn, &resigned);
                }
            }
            if resignations.next().is_some() || position.cells != board {
                return Err("Final Chinese Checkers board differs from the replay".into());
            }
            let reported = matches!(reason.as_str(), "abandoned" | "idle") && winner.is_none();
            (
                "checkers",
                plays.len(),
                outcome(ended, winner, &reason, reported)?,
            )
        }
    };
    Ok(
        json!({ "kind": "board-replay", "game": game, "moves": moves, "result_scope": result_scope }),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn gomoku_replays_win_and_rejects_a_changed_move() {
        let plays = [
            [0, 0],
            [0, 1],
            [1, 0],
            [1, 1],
            [2, 0],
            [2, 1],
            [3, 0],
            [3, 1],
            [4, 0],
        ];
        let mut proof = json!({
            "protocol": "board-replay-v1",
            "replay": {
                "game": "gomoku", "plays": plays,
                "board": format!("bbbbb..........wwww...........{}", ".".repeat(195)),
                "winner": "black", "reason": "five",
            },
        });
        assert_eq!(verify(&proof).unwrap()["result_scope"], "rules");
        proof["replay"]["plays"][8] = json!([5, 0]);
        assert!(verify(&proof).is_err());
    }

    #[test]
    fn xiangqi_replays_moves_and_marks_reported_end() {
        let mut actual = xiangqi::Game::new();
        assert!(actual.play(xiangqi::Move::new(54, 45)));
        let mut proof = json!({
            "protocol": "board-replay-v1",
            "replay": {
                "game": "xiangqi", "plays": [[54, 45]],
                "fen": actual.board.fen(), "winner": "black", "reason": "resign",
            },
        });
        assert_eq!(verify(&proof).unwrap()["result_scope"], "reported_event");
        proof["replay"]["plays"][0] = json!([90, 0]);
        assert!(verify(&proof).is_err());
    }

    #[test]
    fn checkers_replays_resignation_and_rejects_the_wrong_winner() {
        let mut actual = checkers::Board::new(2).unwrap();
        let legal = actual.all_moves(1, checkers::Mode::Super).remove(0);
        actual
            .play(1, legal.from, legal.to, checkers::Mode::Super)
            .unwrap();
        let mut proof = json!({
            "protocol": "board-replay-v1",
            "replay": {
                "game": "checkers", "players": 2, "mode": "super", "first": 1,
                "plays": [[legal.from, legal.to]], "resignations": [[1, 1]],
                "board": actual.cells, "winner": 0, "reason": "resign",
            },
        });
        assert_eq!(verify(&proof).unwrap()["result_scope"], "rules");
        proof["replay"]["winner"] = json!(1);
        assert!(verify(&proof).is_err());
    }
}
