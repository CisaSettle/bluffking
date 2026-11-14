//! Shared flop/turn draw facts. Policy-specific outs and made-hand strength stay
//! with the caller. A board-only pattern is never credited to the hero.
use crate::hand::{BoardCards, HoleCards};

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct DrawFeatures {
    pub flush: bool,
    pub open_ended: bool,
    pub gutshot: bool,
}

/// No allocations: bot range evaluation calls this for every possible holding.
/// Straight ranks are 2..A in bits 0..12; the wheel also uses bit 12 as low Ace.
pub fn draws(hole: &HoleCards, board: &BoardCards) -> DrawFeatures {
    if !matches!(board.count(), 3 | 4) {
        return DrawFeatures::default();
    }
    let mut board_bits = 0u16;
    let mut suits = [0u8; 4];
    for card in board
        .flop
        .into_iter()
        .flatten()
        .chain(board.turn)
        .chain(board.river)
    {
        board_bits |= 1 << card.rank as u16;
        suits[card.suit as usize] += 1;
    }
    let hole_bits = (1 << hole.card1.rank as u16) | (1 << hole.card2.rank as u16);
    let hero_bits = hole_bits & !board_bits;
    let bits = board_bits | hole_bits;
    suits[hole.card1.suit as usize] += 1;
    suits[hole.card2.suit as usize] += 1;
    let flush = [hole.card1.suit, hole.card2.suit]
        .into_iter()
        .any(|suit| suits[suit as usize] == 4);
    let mut result = DrawFeatures {
        flush,
        ..DrawFeatures::default()
    };
    const WHEEL: u16 = (1 << 12) | 0b1111;
    if bits & WHEEL == WHEEL || (0..=8).any(|start| bits & (0b11111 << start) == 0b11111 << start) {
        return result; // Already a straight, so there is no straight draw.
    }
    // 2-3-4-5 has two ends too (A and 6). J-Q-K-A has no upper end.
    for start in 0..=8 {
        let run = 0b1111 << start;
        if bits & run == run && hero_bits & run != 0 {
            result.open_ended = true;
            return result;
        }
    }
    result.gutshot = (0..=8)
        .map(|start| 0b11111 << start)
        .chain([WHEEL])
        .any(|window| (bits & window).count_ones() == 4 && hero_bits & window != 0);
    result
}
