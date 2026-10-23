//! Session length for the non-poker game rooms (炸金花 / 斗地主 / 卡五星 / 掼蛋 /
//! 五子棋 / 象棋).
//!
//! The host picks a duration at creation (same 1h–8h grid as poker rooms,
//! validated by [`crate::handlers::validate_room_duration_secs`]). The
//! countdown starts with the first deal. Once it has elapsed the hand in
//! progress still finishes normally; the room simply deals no new hand.
//! Timestamps are epoch milliseconds so persisted rooms (kwx, guandan) keep
//! their deadline across restarts inside their existing JSON state.

use serde::{Deserialize, Serialize};
use serde_json::{json, Value};

pub const DEFAULT_DURATION_SECS: u32 = 3_600;

/// A room that has gone this long with no hand/match in progress closes
/// itself (owner 2026-09-25) — the non-poker counterpart of poker's ADR-058
/// one-hour no-start close. It covers a room nobody ever started, one whose
/// session clock ran out, and one everybody stopped playing in.
pub const IDLE_CLOSE_SECS: u64 = 3_600;

/// Human-versus-bot rooms close after ten minutes without human input.
pub const BOT_IDLE_CLOSE_SECS: u64 = 600;

/// Leave the final result visible before an expired room disappears.
const RESULT_DISPLAY_SECS: u64 = 10;

/// How long a loaded room has gone without a hand in progress. Kept in memory
/// by the room task (a restart grants a fresh window); rooms nobody is
/// connected to are unloaded and then swept from storage by
/// [`crate::room_code::spawn_room_cleanup`] on the same threshold.
#[derive(Debug, Clone, Copy)]
pub struct IdleWatch {
    since: Option<tokio::time::Instant>,
    last_activity: tokio::time::Instant,
}

impl Default for IdleWatch {
    fn default() -> Self {
        Self::new()
    }
}

impl IdleWatch {
    /// Opening a room counts as the start of its idleness.
    pub fn new() -> Self {
        Self {
            since: Some(tokio::time::Instant::now()),
            last_activity: tokio::time::Instant::now(),
        }
    }

    /// Accepted human input, never socket pings or automatic bot turns.
    pub fn activity(&mut self) {
        self.last_activity = tokio::time::Instant::now();
    }

    /// Finish the current hand before closing. Expiry does not grant an extra
    /// idle hour, and bot turns do not extend the human inactivity window.
    pub fn should_close(&mut self, in_play: bool, bot_room: bool, expired: bool) -> bool {
        let idle = self.idle_expired(in_play);
        let result_visible = self
            .since
            .is_some_and(|at| at.elapsed().as_secs() >= RESULT_DISPLAY_SECS);
        !in_play
            && ((expired && result_visible)
                || idle
                || (bot_room && self.last_activity.elapsed().as_secs() >= BOT_IDLE_CLOSE_SECS))
    }

    /// Feeds whether a hand is in progress right now; true once the room has
    /// been without one for [`IDLE_CLOSE_SECS`]. A hand in progress is never
    /// interrupted and restarts the window when it ends.
    pub fn idle_expired(&mut self, in_play: bool) -> bool {
        let now = tokio::time::Instant::now();
        if in_play {
            self.since = None;
            return false;
        }
        let since = *self.since.get_or_insert(now);
        now.duration_since(since) >= std::time::Duration::from_secs(IDLE_CLOSE_SECS)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct RoomClock {
    pub duration_secs: u32,
    /// Absolute expiry; `None` until the first hand/match is dealt.
    #[serde(default)]
    pub expires_at_ms: Option<i64>,
}

impl Default for RoomClock {
    fn default() -> Self {
        Self::new(DEFAULT_DURATION_SECS)
    }
}

impl RoomClock {
    pub fn new(duration_secs: u32) -> Self {
        Self {
            duration_secs,
            expires_at_ms: None,
        }
    }

    /// Starts the countdown on the first deal; later deals keep the deadline.
    pub fn start(&mut self, now_ms: i64) {
        if self.expires_at_ms.is_none() {
            self.expires_at_ms = Some(now_ms + i64::from(self.duration_secs) * 1_000);
        }
    }

    pub fn expired(&self, now_ms: i64) -> bool {
        self.expires_at_ms.is_some_and(|at| now_ms >= at)
    }

    pub fn remaining_ms(&self, now_ms: i64) -> Option<u64> {
        self.expires_at_ms
            .map(|at| u64::try_from(at.saturating_sub(now_ms)).unwrap_or(0))
    }

    /// Wire projection shared by all six games. Clients pin `remaining_ms`
    /// to snapshot arrival, like the per-turn timers.
    pub fn view(&self, now_ms: i64) -> Value {
        json!({
            "duration_secs": self.duration_secs,
            "expires_at_ms": self.expires_at_ms,
            "remaining_ms": self.remaining_ms(now_ms),
            "expired": self.expired(now_ms),
        })
    }
}

/// `(turn_ms_left, turn_ms_total)` for a public view. Both are `None` unless
/// `active` (the game has a seat to act); `total` also needs a recorded start.
pub fn turn_clock_ms(
    turn_deadline: Option<tokio::time::Instant>,
    turn_started: Option<tokio::time::Instant>,
    now: tokio::time::Instant,
    active: bool,
) -> (Option<u64>, Option<u64>) {
    let left = turn_deadline
        .filter(|_| active)
        .map(|d| d.saturating_duration_since(now).as_millis() as u64);
    let total = turn_deadline
        .zip(turn_started)
        .filter(|_| active)
        .map(|(d, s)| d.saturating_duration_since(s).as_millis() as u64);
    (left, total)
}

pub fn now_ms() -> i64 {
    chrono::Utc::now().timestamp_millis()
}

/// Validates a create-room request: omitted means one hour, otherwise the
/// poker room grid/limits apply. Returns a ready-to-send 400 on failure.
#[allow(clippy::result_large_err)]
pub fn resolve_duration(
    app_env: &str,
    requested: Option<u32>,
) -> Result<u32, axum::response::Response> {
    use axum::response::IntoResponse;
    crate::handlers::validate_room_duration_secs(
        app_env,
        Some(requested.unwrap_or(DEFAULT_DURATION_SECS)),
    )
    .map_err(|e| {
        (
            axum::http::StatusCode::BAD_REQUEST,
            axum::Json(json!({"error": e.error, "message": e.message})),
        )
            .into_response()
    })
}
