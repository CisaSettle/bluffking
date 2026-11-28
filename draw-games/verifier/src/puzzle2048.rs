//! Bounded replay of the paid 2048 board. Client scores and tiles are not trusted.
fn random(seed: &mut u32) -> f64 {
    *seed = seed.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
    f64::from(*seed) / 4_294_967_296.0
}
fn spawn(board: &mut [u32; 16], seed: &mut u32) {
    let mut empty = [0; 16];
    let mut count = 0;
    for (index, &tile) in board.iter().enumerate() {
        if tile == 0 {
            empty[count] = index;
            count += 1;
        }
    }
    if count == 0 {
        return;
    }
    let at = empty[(random(seed) * count as f64) as usize];
    board[at] = if random(seed) >= 0.9 { 4 } else { 2 };
}
pub fn completed(mut seed: u32, actions: &[String]) -> bool {
    if actions.is_empty() || actions.len() > 8192 {
        return false;
    }
    let mut board = [0u32; 16];
    spawn(&mut board, &mut seed);
    spawn(&mut board, &mut seed);
    let mut previous = None;
    let mut reached = false;
    for action in actions {
        if action == "undo" {
            if let Some(saved) = previous.take() {
                board = saved;
            } else {
                return false;
            }
            continue;
        }
        let (vertical, reverse) = match action.as_str() {
            "up" => (true, false),
            "down" => (true, true),
            "left" => (false, false),
            "right" => (false, true),
            _ => return false,
        };
        let mut next = [0u32; 16];
        for line in 0..4 {
            let index = |slot: usize| {
                let along = if reverse { 3 - slot } else { slot };
                if vertical {
                    along * 4 + line
                } else {
                    line * 4 + along
                }
            };
            let mut values = (0..4)
                .map(|s| board[index(s)])
                .filter(|&v| v > 0)
                .peekable();
            for slot in 0..4 {
                let Some(mut value) = values.next() else {
                    break;
                };
                if values.next_if_eq(&value).is_some() {
                    let Some(doubled) = value.checked_mul(2) else {
                        return false;
                    };
                    value = doubled;
                }
                next[index(slot)] = value;
            }
        }
        if next == board {
            return false;
        }
        previous = Some(board);
        reached |= next.iter().any(|&v| v >= 2048);
        spawn(&mut next, &mut seed);
        board = next;
    }
    reached
}

pub const COST: i64 = 1;
pub const COMPLETION_REFUND: i64 = COST;

pub fn draw_seed(entropy: &mut crate::proof::Entropy) -> u32 {
    entropy.below(1 << 32) as u32
}
