//! 彩票中心 (lottery center): one server-authoritative hall per lottery game.
//!
//! Everybody buys from the same house; there are no rooms, roles or bots.
//! The number games (双色球 `ssq`, TOTO `toto`, 快乐8 `happy8`, 福彩3D
//! `fucai3d`) sell tickets for wall-clock periods and a draw loop in every
//! backend draws each due period exactly once (database-arbitrated) and
//! settles it one user per transaction. 刮刮乐 `scratch` cards are generated
//! at purchase and revealed spot by spot. One shared practice wallet
//! (`game_catalog::BEANS`, grant 10,000) backs all five games; every ledger
//! record id is derived here, so retries never debit or credit twice.
//!
//! Module map: [`rules`] entry validation, stakes and prizes; [`store`]
//! purchases, views, refills; [`scratch`] cards; [`draw_loop`] draws,
//! settlement and retention; [`handlers`] HTTP; [`legacy_scratch`] the
//! account-purge transform for pre-switch scratch room checkpoints.

pub mod draw_loop;
pub mod handlers;
pub mod legacy_scratch;
pub mod rules;
pub mod scratch;
pub mod store;

use chrono::{DateTime, FixedOffset, TimeZone, Utc};
use uuid::Uuid;

pub const POLICY: crate::wallet::Policy = crate::beans::POLICY;
/// Namespace of every derived ledger record id (v5 UUIDs).
pub const LOTTERY_NS: Uuid = Uuid::from_u128(0x3f1b_5a7c_9d2e_4b86_a5c0_7e1d_2f3a_4b5c);
/// Extra daily grant shared with every beans game.
pub const REFILL: i64 = crate::beans::DAILY_REFILL_TO;
/// 封盘: sales close this long before the draw.
pub const CLOSE_MS: i64 = 5_000;
/// Period labels and report days use Singapore time.
pub const SGT_OFFSET_SECS: i32 = 8 * 60 * 60;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Game {
    Ssq,
    Toto,
    Happy8,
    Fucai3d,
    Scratch,
}

impl Game {
    pub const ALL: [Game; 5] = [
        Game::Ssq,
        Game::Toto,
        Game::Happy8,
        Game::Fucai3d,
        Game::Scratch,
    ];
    /// The four scheduled number games, in draw-loop order.
    pub const DRAWN: [Game; 4] = [Game::Happy8, Game::Fucai3d, Game::Ssq, Game::Toto];

    pub fn parse(name: &str) -> Option<Game> {
        Some(match name {
            "ssq" => Game::Ssq,
            "toto" => Game::Toto,
            "happy8" => Game::Happy8,
            "fucai3d" => Game::Fucai3d,
            "scratch" => Game::Scratch,
            _ => return None,
        })
    }
    pub fn as_str(self) -> &'static str {
        match self {
            Game::Ssq => "ssq",
            Game::Toto => "toto",
            Game::Happy8 => "happy8",
            Game::Fucai3d => "fucai3d",
            Game::Scratch => "scratch",
        }
    }
    /// Chips per 注 (scratch: the smallest card).
    pub fn unit(self) -> i64 {
        match self {
            Game::Ssq | Game::Toto => 10,
            Game::Happy8 => 4,
            Game::Fucai3d => 2,
            Game::Scratch => 10,
        }
    }
    pub fn schedule(self) -> Option<Schedule> {
        Some(match self {
            Game::Ssq => Schedule {
                interval_ms: 300_000,
                offset_ms: 0,
            },
            Game::Toto => Schedule {
                interval_ms: 300_000,
                offset_ms: 150_000,
            },
            Game::Happy8 => Schedule {
                interval_ms: 120_000,
                offset_ms: 0,
            },
            Game::Fucai3d => Schedule {
                interval_ms: 180_000,
                offset_ms: 0,
            },
            Game::Scratch => return None,
        })
    }
}

/// Wall-clock aligned periods: `period = floor((now − offset) / interval)`.
#[derive(Debug, Clone, Copy)]
pub struct Schedule {
    pub interval_ms: i64,
    pub offset_ms: i64,
}

#[derive(Debug, Clone)]
pub struct Period {
    pub id: i64,
    pub opens_ms: i64,
    pub closes_ms: i64,
    pub draw_ms: i64,
    pub label: String,
}

impl Schedule {
    /// The period selling at `now_ms` (its draw is the next boundary).
    pub fn period_at(&self, now_ms: i64) -> Period {
        self.period((now_ms - self.offset_ms).div_euclid(self.interval_ms))
    }
    pub fn period(&self, id: i64) -> Period {
        let opens_ms = id * self.interval_ms + self.offset_ms;
        let draw_ms = opens_ms + self.interval_ms;
        Period {
            id,
            opens_ms,
            closes_ms: draw_ms - CLOSE_MS,
            draw_ms,
            label: self.label(draw_ms),
        }
    }
    /// `YYYYMMDD-NNN`: the SGT day of the draw and the draw's 1-based index
    /// within that day.
    pub fn label(&self, draw_ms: i64) -> String {
        let sgt = sgt();
        let at = sgt
            .timestamp_millis_opt(draw_ms)
            .single()
            .unwrap_or_else(|| {
                sgt.timestamp_millis_opt(0)
                    .single()
                    .expect("epoch is representable")
            });
        let day_start_ms = sgt
            .from_local_datetime(&at.date_naive().and_hms_opt(0, 0, 0).expect("midnight"))
            .single()
            .map_or(draw_ms, |d| d.timestamp_millis());
        // Draws happen at j·interval + offset (j ≥ 1); count the day's draws
        // up to this one.
        let j = (draw_ms - self.offset_ms).div_euclid(self.interval_ms);
        let first_j =
            (day_start_ms - self.offset_ms + self.interval_ms - 1).div_euclid(self.interval_ms);
        let index = (j - first_j + 1).max(1);
        format!("{}-{:03}", at.format("%Y%m%d"), index)
    }
}

pub fn sgt() -> FixedOffset {
    FixedOffset::east_opt(SGT_OFFSET_SECS).expect("Singapore UTC offset is valid")
}

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
