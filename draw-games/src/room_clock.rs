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
use std::sync::atomic::{AtomicU64, Ordering};

pub const DEFAULT_DURATION_SECS: u32 = 3_600;

/// Owner 2026-10-29: every room, in every game, bot or human, is reclaimed
/// after [`empty_reclaim_secs`] without human input (a seated player's
/// command, chat or joining); an empty room has none. Only the shared
/// [`crate::room_code::spawn_room_cleanup`] tick reclaims rooms. Owner
/// 2026-11-11: five minutes, production's value (was ten);
/// `ROOM_EMPTY_RECLAIM_SECS`
/// overrides it (the local profile uses a short window for testing).
pub const DEFAULT_EMPTY_RECLAIM_SECS: u64 = 300;
/// Default cadence of the shared cleanup tick, so a room closes between
/// [`empty_reclaim_secs`] and that plus one tick after its last human input.
/// `ROOM_CLEANUP_TICK_SECS` overrides it.
pub const DEFAULT_CLEANUP_TICK_SECS: u64 = 300;
const MIN_EMPTY_RECLAIM_SECS: u64 = 30;
const MAX_EMPTY_RECLAIM_SECS: u64 = 24 * 60 * 60;
const MIN_CLEANUP_TICK_SECS: u64 = 5;

static EMPTY_RECLAIM: AtomicU64 = AtomicU64::new(DEFAULT_EMPTY_RECLAIM_SECS);
static CLEANUP_TICK_SECS: AtomicU64 = AtomicU64::new(DEFAULT_CLEANUP_TICK_SECS);

/// Room idle-reclaim timing, validated once at startup from the profile's
/// environment.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ReclaimTiming {
    pub empty_reclaim_secs: u64,
    pub cleanup_tick_secs: u64,
}

/// Validates `ROOM_EMPTY_RECLAIM_SECS` (30 s to one day; unset or blank =
/// 300) and `ROOM_CLEANUP_TICK_SECS` (5 s up to the reclaim window; unset or
/// blank = 300, or the reclaim window when that is shorter).
pub fn parse_reclaim_timing(
    raw_reclaim: Option<&str>,
    raw_tick: Option<&str>,
) -> Result<ReclaimTiming, String> {
    fn parse(name: &str, raw: Option<&str>) -> Result<Option<u64>, String> {
        raw.map(str::trim)
            .filter(|value| !value.is_empty())
            .map(|value| {
                value.parse::<u64>().map_err(|_| {
                    format!("{name} must be a whole number of seconds (got {value:?})")
                })
            })
            .transpose()
    }
    let reclaim =
        parse("ROOM_EMPTY_RECLAIM_SECS", raw_reclaim)?.unwrap_or(DEFAULT_EMPTY_RECLAIM_SECS);
    if !(MIN_EMPTY_RECLAIM_SECS..=MAX_EMPTY_RECLAIM_SECS).contains(&reclaim) {
        return Err(format!(
            "ROOM_EMPTY_RECLAIM_SECS must be from {MIN_EMPTY_RECLAIM_SECS} to \
             {MAX_EMPTY_RECLAIM_SECS} (got {reclaim})"
        ));
    }
    let tick = parse("ROOM_CLEANUP_TICK_SECS", raw_tick)?
        .unwrap_or_else(|| DEFAULT_CLEANUP_TICK_SECS.min(reclaim));
    if !(MIN_CLEANUP_TICK_SECS..=reclaim).contains(&tick) {
        return Err(format!(
            "ROOM_CLEANUP_TICK_SECS must be from {MIN_CLEANUP_TICK_SECS} to \
             ROOM_EMPTY_RECLAIM_SECS ({reclaim}) (got {tick})"
        ));
    }
    Ok(ReclaimTiming {
        empty_reclaim_secs: reclaim,
        cleanup_tick_secs: tick,
    })
}

/// Installed once at startup from the validated configuration, before any
/// room or the cleanup task starts.
pub fn configure(timing: ReclaimTiming) {
    EMPTY_RECLAIM.store(timing.empty_reclaim_secs, Ordering::Release);
    CLEANUP_TICK_SECS.store(timing.cleanup_tick_secs, Ordering::Release);
}

/// Seconds without human input after which the cleanup tick reclaims a room.
pub fn empty_reclaim_secs() -> u64 {
    EMPTY_RECLAIM.load(Ordering::Acquire)
}

/// Cadence of the shared [`crate::room_code::spawn_room_cleanup`] tick.
pub fn cleanup_tick_interval() -> std::time::Duration {
    std::time::Duration::from_secs(CLEANUP_TICK_SECS.load(Ordering::Acquire))
}

/// Leave the final result visible before an expired room disappears.
const RESULT_DISPLAY_SECS: u64 = 10;

static CLEANUP_TICK: std::sync::LazyLock<tokio::sync::watch::Sender<()>> =
    std::sync::LazyLock::new(|| tokio::sync::watch::channel(()).0);

/// The shared room cleanup tick, for room tasks that hold rooms in memory.
pub fn subscribe_cleanup() -> tokio::sync::watch::Receiver<()> {
    CLEANUP_TICK.subscribe()
}

/// Called only by [`crate::room_code::spawn_room_cleanup`].
pub(crate) fn notify_cleanup() {
    CLEANUP_TICK.send_modify(|_| {});
}

/// A loaded room's inactivity. Kept in memory by the room task (a restart
/// grants a fresh window); rooms nobody is connected to are unloaded and then
/// swept from storage by [`crate::room_code::spawn_room_cleanup`] after
/// [`empty_reclaim_secs`].
#[derive(Debug, Clone, Copy)]
pub struct IdleWatch {
    since: Option<tokio::time::Instant>,
    last_activity: tokio::time::Instant,
    last_human_input_at_ms: Option<i64>,
}

impl Default for IdleWatch {
    fn default() -> Self {
        Self::new()
    }
}

impl IdleWatch {
    /// Opening a room counts as the start of its idleness.
    pub fn new() -> Self {
        let now = tokio::time::Instant::now();
        Self {
            since: Some(now),
            last_activity: now,
            last_human_input_at_ms: None,
        }
    }

    /// Accepted human input or a join, never socket pings or automatic bot
    /// turns.
    pub fn activity(&mut self) {
        self.last_activity = tokio::time::Instant::now();
        self.last_human_input_at_ms = Some(now_ms());
    }

    /// Time since the last human input (or since the room opened or its idle
    /// window was reset), the span [`Self::reclaim_due`] measures.
    pub fn idle_for(&self) -> std::time::Duration {
        self.last_activity.elapsed()
    }

    /// None until a human input is observed in this runtime; loading is not input.
    pub fn last_human_input_at_ms(&self) -> Option<i64> {
        self.last_human_input_at_ms
    }

    /// Resume cleanup's grace window without forgetting an observed human input.
    pub fn reset_idle(&mut self) {
        *self = Self {
            last_human_input_at_ms: self.last_human_input_at_ms,
            ..Self::new()
        };
    }

    /// Fed by the room task on every pass: whether a hand is in progress.
    pub fn observe(&mut self, in_play: bool) {
        if in_play {
            self.since = None;
        } else {
            self.since.get_or_insert_with(tokio::time::Instant::now);
        }
    }

    /// The session clock ran out, the hand finished and its result has shown.
    pub fn expiry_due(&self, in_play: bool, expired: bool) -> bool {
        !in_play
            && expired
            && self
                .since
                .is_some_and(|at| at.elapsed().as_secs() >= RESULT_DISPLAY_SECS)
    }

    /// How long until [`Self::expiry_due`] can hold once the session clock has
    /// run out: `None` while a hand is in progress.
    pub fn expiry_wait(&self, in_play: bool) -> Option<std::time::Duration> {
        let since = self.since.filter(|_| !in_play)?;
        Some(std::time::Duration::from_secs(RESULT_DISPLAY_SECS).saturating_sub(since.elapsed()))
    }

    /// Checked only on the shared cleanup tick. A hand in progress is never
    /// interrupted; bot turns do not extend the human inactivity window.
    pub fn reclaim_due(&self, in_play: bool) -> bool {
        !in_play && self.last_activity.elapsed().as_secs() >= empty_reclaim_secs()
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
