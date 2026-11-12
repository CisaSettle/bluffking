//! Shared 押大小 rooms. Same shape as baccarat: open rooms live in memory on the
//! table owner, a dealer loop settles due rounds, and an ordered per-room
//! writer commits each change (`crate::casino_room`). A 金豆 room settles into
//! the shared bean wallet, a 积分 room into its own points, under the stake
//! limits and payout of `crate::sicbo`.
//! AI co-bettors (`crate::casino_bots`) never touch wallets.
pub(crate) mod store;

pub use store::SicboRoom;
