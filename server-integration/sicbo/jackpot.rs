//! Deterministic eligibility and payout for the Big & Small bonus prize.
use uuid::Uuid;

/// A separate purpose-bound stream preserves a room prize across checkpoint recovery.
/// The draw proof and the base bean payout use their original entropy stream.
/// The stream label predates the bean jackpot (it once chose a Max membership
/// prize); it stays unchanged so a recovered round keeps its original decision.
pub(crate) fn wins_jackpot(reveal: &draw_games::proof::Reveal, user: Uuid) -> bool {
    let seed = draw_games::proof::sha256_fields(&[
        b"bluffking-sicbo-max-prize-v1",
        reveal.commitment.game.as_bytes(),
        reveal.commitment.draw_id.as_bytes(),
        reveal.server_seed.as_bytes(),
        reveal.client_seed.as_bytes(),
        user.as_bytes(),
    ]);
    draw_games::proof::Entropy::new(seed).below(10_000) == 0
}

/// The stake a jackpot multiplies: one bean, whatever the wager.
const JACKPOT_STAKE: i64 = 1;

/// A jackpot pays 5 × the highest bean payout a 1-bean stake can win: a
/// winning small/big returns the stake plus an equal win (2 × stake), so the
/// bonus is 10 × [`JACKPOT_STAKE`]. Larger stakes (up to 梭哈) draw the same
/// 10-bean jackpot as a 1-bean one.
pub(crate) fn jackpot_bonus(stake: i64) -> Option<i64> {
    stake
        .min(JACKPOT_STAKE)
        .checked_mul(2)?
        .checked_mul(5)
        .filter(|bonus| *bonus > 0)
}
