//! Shared 押大小 rooms. Open rooms live in memory on the
//! table owner, a dealer loop settles due rounds, and an ordered per-room
//! writer commits each change (`crate::casino_room`). Wagers settle into the
//! solo game's sicbo wallet under its stake limits and payout (`crate::sicbo`).
//! AI co-bettors (`crate::casino_bots`) never touch wallets.
mod store;

pub use store::{current_room, seated, SicboRoom};
