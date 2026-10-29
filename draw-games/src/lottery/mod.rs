//! 彩票中心 (lottery center): one server-authoritative hall per lottery game.
//!
//! Everybody buys from the same house; there are no rooms, roles or bots.
//! The number games (双色球 `ssq`, 大乐透 `dlt`, TOTO `toto`, 快乐8 `happy8`,
//! 福彩3D `fucai3d`) sell tickets for wall-clock periods and a draw loop in every
//! backend draws each due period exactly once (database-arbitrated) and
//! settles it one user per transaction. 刮刮乐 `scratch` cards are generated
//! at purchase and revealed spot by spot. One shared practice wallet
//! (`game_catalog::BEANS`, first-use grant 10,000, daily claim +2,000) backs all
//! six games; every ledger record id is derived here, so retries never debit
//! or credit twice.
//!
//! Module map: [`rules`] entry validation, stakes and prizes; [`store`]
//! purchases, views, refills; [`scratch`] cards; [`draw_loop`] draws,
//! settlement and retention; [`handlers`] HTTP.

pub mod beacon;
pub mod draw_loop;
pub mod handlers;
pub mod rules;
pub mod runtime;
pub mod scratch;
pub mod store;

use chrono::{DateTime, Utc};
use uuid::Uuid;

pub const POLICY: crate::wallet::Policy = crate::beans::POLICY;
/// Namespace of every derived ledger record id (v5 UUIDs).
pub const LOTTERY_NS: Uuid = Uuid::from_u128(0x3f1b_5a7c_9d2e_4b86_a5c0_7e1d_2f3a_4b5c);
/// Extra daily grant shared with every beans game.
pub const REFILL: i64 = crate::beans::DAILY_REFILL_TO;
pub use draw_games::lottery::{sgt, Game, Period};

/// The SGT calendar day of an instant (report day of purchases/settlements).
pub fn sgt_day(at: DateTime<Utc>) -> chrono::NaiveDate {
    at.with_timezone(&sgt()).date_naive()
}

pub fn now_ms() -> i64 {
    Utc::now().timestamp_millis()
}

pub fn record_id(name: &str) -> Uuid {
    Uuid::new_v5(&LOTTERY_NS, name.as_bytes())
}
/// Purchase debit (number games and single scratch cards).
pub fn purchase_id(user: Uuid, request_id: Uuid) -> Uuid {
    record_id(&format!("purchase:{user}:{request_id}"))
}
/// One draw credit per (game, period, user).
pub fn draw_credit_id(game: Game, period: i64, user: Uuid) -> Uuid {
    record_id(&format!("draw:{}:{period}:{user}", game.as_str()))
}
/// Credit of a single scratch card when its last spot opens.
pub fn scratch_credit_id(ticket: Uuid) -> Uuid {
    record_id(&format!("scratch:{ticket}"))
}
/// The one net row of a 5/10-card scratch batch.
pub fn scratch_batch_id(user: Uuid, request_id: Uuid) -> Uuid {
    record_id(&format!("scratch-batch:{user}:{request_id}"))
}
