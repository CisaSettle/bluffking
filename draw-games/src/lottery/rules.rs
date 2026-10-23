//! Production and offline verification share the same entry and payout rules.
pub use draw_games::lottery::*;
use rand::rngs::OsRng;

// Compatibility only for pre-proof periods already sold at rollout.
/// Unbiased draw of `count` distinct numbers from 1..=max, sorted.
fn sample(max: u8, count: usize) -> Vec<i16> {
    let mut picked: Vec<i16> = rand::seq::index::sample(&mut OsRng, usize::from(max), count)
        .into_iter()
        .map(|i| i as i16 + 1)
        .collect();
    picked.sort_unstable();
    picked
}

/// Winning numbers of one period: `(numbers, scalar bonus, bonus numbers)`.
/// DLT uses two back numbers; existing scalar-bonus games keep their wire shape.
pub fn legacy_draw(game: Game) -> (Vec<i16>, Option<i16>, Vec<i16>) {
    use rand::Rng;
    match game {
        Game::Ssq => (
            sample(SSQ_RED_MAX, 6),
            Some(OsRng.gen_range(1..=i16::from(SSQ_BLUE_MAX))),
            Vec::new(),
        ),
        Game::Dlt => (sample(DLT_FRONT_MAX, 5), None, sample(DLT_BACK_MAX, 2)),
        Game::Toto => {
            let seven: Vec<i16> = rand::seq::index::sample(&mut OsRng, usize::from(TOTO_MAX), 7)
                .into_iter()
                .map(|i| i as i16 + 1)
                .collect();
            let mut winning = seven[..6].to_vec();
            winning.sort_unstable();
            (winning, Some(seven[6]), Vec::new())
        }
        Game::Happy8 => (sample(HAPPY8_MAX, HAPPY8_DRAW), None, Vec::new()),
        Game::Fucai3d => (
            (0..3).map(|_| OsRng.gen_range(0..=9i16)).collect(),
            None,
            Vec::new(),
        ),
        Game::Scratch => (Vec::new(), None, Vec::new()),
    }
}
