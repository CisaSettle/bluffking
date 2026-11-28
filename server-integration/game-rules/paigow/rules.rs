//! Two-tile 牌九 with the 32 Chinese dominoes.
//!
//! Hand order: 至尊 (丁三 + 二四) > pairs (双天 … 杂五) > 天王 / 地王 (天 or 地
//! with a nine) > 天杠 / 地杠 (天 or 地 with an eight) > points (pip sum mod
//! 10). Equal points compare the stronger single tile. A player beats the
//! banker only with a strictly stronger hand: a hand still tied after that,
//! and any 0-point player hand, go to the banker.
use serde::{Deserialize, Serialize};

/// Pips of deck ids 0..32, strongest group first. Civil group `g` (0..=10:
/// 天 地 人 和 梅 长三 板凳 斧头 红头十 高脚七 零霖六) is ids `2g`, `2g + 1`;
/// the military tiles follow as 杂九 杂九 杂八 杂八 杂七 杂七 杂五 杂五 二四 丁三.
const PIPS: [(u8, u8); 32] = [
    (6, 6),
    (6, 6),
    (1, 1),
    (1, 1),
    (4, 4),
    (4, 4),
    (3, 1),
    (3, 1),
    (5, 5),
    (5, 5),
    (3, 3),
    (3, 3),
    (2, 2),
    (2, 2),
    (6, 5),
    (6, 5),
    (6, 4),
    (6, 4),
    (6, 1),
    (6, 1),
    (5, 1),
    (5, 1),
    (5, 4),
    (6, 3),
    (5, 3),
    (6, 2),
    (4, 3),
    (5, 2),
    (4, 1),
    (3, 2),
    (4, 2),
    (2, 1),
];
pub const DECK: usize = PIPS.len();
/// The highest hand multiplier (至尊).
pub const MAX_MULTIPLIER: u64 = 4;
/// 天 and 地.
const HEAVEN: u8 = 0;
const EARTH: u8 = 1;
/// 二四 and 丁三, which pair only with each other as 至尊.
const SUPREME: u8 = 15;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct Tile(pub u8);
impl Tile {
    pub fn valid(self) -> bool {
        usize::from(self.0) < DECK
    }
    fn pips(self) -> (u8, u8) {
        PIPS[usize::from(self.0)]
    }
    /// Pair group: 0..=10 civil, 11 杂九, 12 杂八, 13 杂七, 14 杂五, 15 至尊.
    pub fn group(self) -> u8 {
        if self.0 < 22 {
            self.0 / 2
        } else {
            11 + (self.0 - 22) / 2
        }
    }
    pub fn points(self) -> u8 {
        let (a, b) = self.pips();
        a + b
    }
    /// Single-tile strength; the 至尊 halves are the weakest singles.
    fn strength(self) -> u8 {
        SUPREME - self.group()
    }
    /// `"<high pips><low pips><copy>"`, e.g. `66a` / `66b` for the two 天.
    pub fn code(self) -> String {
        let (a, b) = self.pips();
        let copy = if self.0 < 22 && self.0 % 2 == 1 {
            'b'
        } else {
            'a'
        };
        format!("{a}{b}{copy}")
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Kind {
    Points,
    Gang,
    Wang,
    Pair,
    Supreme,
}
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct Value {
    pub kind: Kind,
    /// Pip sum mod 10.
    pub points: u8,
    /// Pair: its group. 王 / 杠: 0 天, 1 地. Points: the stronger tile's group.
    pub group: u8,
}
impl Value {
    pub fn multiplier(self) -> u64 {
        match self.kind {
            Kind::Supreme => MAX_MULTIPLIER,
            Kind::Pair => 3,
            Kind::Wang | Kind::Gang => 2,
            Kind::Points => 1,
        }
    }
    fn key(self) -> (u8, u8) {
        match self.kind {
            Kind::Supreme => (4, 0),
            Kind::Pair => (3, SUPREME - self.group),
            Kind::Wang => (2, 1 - self.group),
            Kind::Gang => (1, 1 - self.group),
            Kind::Points => (0, self.points * 16 + (SUPREME - self.group)),
        }
    }
    /// Whether this player's hand beats the banker's.
    pub fn beats_banker(self, banker: Value) -> bool {
        !(self.kind == Kind::Points && self.points == 0) && self.key() > banker.key()
    }
}
pub fn evaluate(tiles: &[Tile; 2]) -> Value {
    let [a, b] = *tiles;
    let points = (a.points() + b.points()) % 10;
    let (high, low) = if a.strength() >= b.strength() {
        (a, b)
    } else {
        (b, a)
    };
    let (kind, group) = if a.group() == b.group() {
        if a.group() == SUPREME {
            (Kind::Supreme, SUPREME)
        } else {
            (Kind::Pair, a.group())
        }
    } else if matches!(high.group(), HEAVEN | EARTH) && low.points() == 9 {
        (Kind::Wang, high.group())
    } else if matches!(high.group(), HEAVEN | EARTH) && low.points() == 8 {
        (Kind::Gang, high.group())
    } else {
        (Kind::Points, high.group())
    };
    Value {
        kind,
        points,
        group,
    }
}
