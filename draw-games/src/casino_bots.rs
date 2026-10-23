//! AI co-bettors for the shared casino rooms (百家乐, 押大小).
//!
//! A bot lives only inside its room's JSON state: it has no user row, no
//! wallet and no ledger entries. Its chip stack is cosmetic, so a room with
//! bots settles real players exactly as a room without them. Each round a bot
//! either sits out or picks a wager, which becomes visible at a random moment
//! of the betting window so the table fills up the way a live one does.
use rand::{rngs::OsRng, seq::SliceRandom, Rng};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

/// Most bots a room may be created with; the room's member cap still applies.
pub const MAX_BOTS: usize = 5;
/// Cosmetic stack every bot starts with and returns to once it runs low.
pub const BOT_STACK: i64 = 20_000;

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct Bot {
    /// Room-local id. Never a `users.id`.
    pub id: Uuid,
    pub name: String,
    pub chips: i64,
}

/// The same table-mate names the other hub games (牛牛, 斗地主, …) seat.
const NAMES: [&str; 9] = [
    "阿福", "小美", "老王", "大锤", "翠花", "铁柱", "二丫", "胖虎", "秋香",
];

/// `n` bots (capped at [`MAX_BOTS`]) with distinct names.
pub fn roster(n: usize) -> Vec<Bot> {
    let mut names = NAMES.to_vec();
    names.shuffle(&mut OsRng);
    names
        .into_iter()
        .take(n.min(MAX_BOTS))
        .map(|name| Bot {
            id: Uuid::new_v4(),
            name: name.to_owned(),
            chips: BOT_STACK,
        })
        .collect()
}

/// Applies a settled round to a bot's cosmetic stack, topping it back up when
/// it can no longer cover a typical wager.
pub fn apply(bot: &mut Bot, delta: i64, floor: i64) {
    bot.chips += delta;
    if bot.chips < floor {
        bot.chips = BOT_STACK;
    }
}

/// When during `[open, close)` a bot's wager appears: never in the first
/// second nor the last one, so humans see bets arrive while they decide.
pub fn reveal_at(open: i64, close: i64) -> i64 {
    let lo = open + 1_000;
    let hi = (close - 1_000).max(lo + 1);
    OsRng.gen_range(lo..hi)
}

/// Whether a bot sits this round out (about one round in five).
pub fn skips() -> bool {
    OsRng.gen_bool(0.2)
}

/// A plausible stake: `step`-aligned, weighted toward small amounts, never
/// above `max` nor the bot's own stack.
pub fn stake(step: i64, max: i64, chips: i64) -> i64 {
    const UNITS: [i64; 8] = [1, 2, 5, 5, 10, 10, 20, 50];
    let unit = *UNITS.choose(&mut OsRng).unwrap_or(&1);
    let cap = max.min(chips.max(step)) / step * step;
    (unit * step).min(cap).max(step)
}
