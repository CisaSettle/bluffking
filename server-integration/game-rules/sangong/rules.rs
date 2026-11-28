//! Three-card Sangong: big triples > small triples > face cards > points.
//! Points ties compare face count, then highest rank and suit (S > H > C > D).
use serde::{Deserialize, Serialize};

pub use crate::banker::card::Card;

fn points(card: Card) -> u8 {
    if card.rank >= 10 {
        0
    } else {
        card.rank
    }
}
/// Rank, then suit S > H > C > D.
fn key(card: Card) -> (u8, u8) {
    (card.rank, 3 - card.suit)
}
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Kind {
    Points,
    Mixed,
    Small,
    Big,
}
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct Value {
    pub kind: Kind,
    pub points: u8,
    pub faces: u8,
    pub highest: Card,
}
impl Value {
    pub fn multiplier(self) -> u64 {
        match self.kind {
            Kind::Big => 9,
            Kind::Small => 7,
            Kind::Mixed => 5,
            Kind::Points if self.points >= 8 => 3,
            Kind::Points => 1,
        }
    }
    pub fn key(self) -> (u8, u8, u8, (u8, u8)) {
        let rank = match self.kind {
            Kind::Points => 0,
            Kind::Mixed => 1,
            Kind::Small => 2,
            Kind::Big => 3,
        };
        (
            rank,
            if rank == 0 { self.points } else { 0 },
            if rank == 0 { self.faces } else { 0 },
            key(self.highest),
        )
    }
}
pub fn evaluate(cards: &[Card; 3]) -> Value {
    let faces = cards.iter().filter(|c| c.rank >= 11).count() as u8;
    let triple = cards.iter().all(|c| c.rank == cards[0].rank);
    Value {
        kind: if triple && faces == 3 {
            Kind::Big
        } else if triple {
            Kind::Small
        } else if faces == 3 {
            Kind::Mixed
        } else {
            Kind::Points
        },
        points: cards.iter().map(|&c| points(c)).sum::<u8>() % 10,
        faces,
        highest: *cards.iter().max_by_key(|&&c| key(c)).expect("three cards"),
    }
}
