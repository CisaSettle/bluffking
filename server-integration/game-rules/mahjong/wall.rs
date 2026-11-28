//! Position accounting for the wall the game actually consumes. The rules decide
//! who draws and which end supplies replacements; this records those draws.
use mental_poker::card_proof::Game;
use serde::{Deserialize, Serialize};

use super::Tile;
use crate::card_proofs::{Deal, Receipt};
use crate::game_hub::{board::BoardSeat, seats::HubSeat};

#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
pub struct Draw {
    pub position: usize,
    pub face: Tile,
}

#[derive(Clone, Default, Serialize, Deserialize)]
pub struct WallDraws {
    #[serde(default)]
    pub(crate) proof: Option<Deal>,
    front: usize,
    seats: [Vec<Draw>; 4],
    public: Vec<Draw>,
}

impl std::fmt::Debug for WallDraws {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("WallDraws")
            .field("front", &self.front)
            .finish_non_exhaustive()
    }
}

pub fn faces(game: Game, cards: &[u16]) -> Option<Vec<Tile>> {
    if cards.len() != game.size() {
        return None;
    }
    cards.iter().map(|&id| game.tile_face(id)).collect()
}

impl WallDraws {
    pub fn receipt(&self, seats: &[Option<BoardSeat>]) -> Option<Receipt> {
        let deal = self.proof.as_ref()?;
        let grants = seats
            .iter()
            .enumerate()
            .filter_map(|(position, seat)| {
                let id = seat.as_ref()?.user_id()?;
                Some((
                    id,
                    self.seat(position)
                        .iter()
                        .map(|draw| draw.position)
                        .collect(),
                ))
            })
            .collect();
        Some(Receipt::new(
            deal,
            self.public().iter().map(|draw| draw.position).collect(),
            grants,
        ))
    }

    pub fn seat(&self, seat: usize) -> &[Draw] {
        &self.seats[seat]
    }

    pub fn public(&self) -> &[Draw] {
        &self.public
    }

    fn add(&mut self, seat: Option<usize>, draw: Draw) {
        match seat {
            Some(seat) => self.seats[seat].push(draw),
            None => self.public.push(draw),
        }
    }

    pub fn draw(&mut self, wall: &mut Vec<Tile>, seat: Option<usize>, front: bool) -> Option<Tile> {
        let (face, position) = if front {
            if wall.is_empty() {
                return None;
            }
            let position = self.front;
            self.front += 1;
            (wall.remove(0), position)
        } else {
            (wall.pop()?, self.front + wall.len())
        };
        self.add(seat, Draw { position, face });
        Some(face)
    }

    pub fn initial(&mut self, wall: &mut Vec<Tile>, seat: usize) -> Vec<Tile> {
        let mut hand: Vec<_> = (0..13)
            .map(|_| {
                self.draw(wall, Some(seat), false)
                    .expect("initial wall has 13 tiles")
            })
            .collect();
        hand.sort_unstable();
        hand
    }
}
