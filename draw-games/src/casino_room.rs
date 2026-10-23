//! Shared plumbing for the dealer rooms, 百家乐 and 押大小.
//!
//! A room lives in this process's memory while it is open, like every other
//! live table: polls, wagers and settlement change the in-memory state and
//! answer at once, and an ordered per-room writer ([`crate::write_behind`])
//! commits each accepted change — a settled round's wallets, ledger, records
//! and round row together with the room snapshot — in one transaction, in
//! order, retrying until it lands. Seated players' wallets are the in-memory
//! projection ([`crate::wallet::activate`]). Joining and leaving stay durable
//! commit points: the membership row is the seat contract that refills, solo
//! 押大小 and account cleanup read, and a leave commits only after that room's
//! earlier writes have. The host table-owner lock (`crate::runtime_owner`)
//! routes every room request to one process, which recovers all rooms from
//! their rows before serving and drains its writers before exiting.
//!
//! The games differ only in rules, wagers, views and round receipts, which
//! implement [`DealerRoom`]; storage, membership, HTTP and the dealer loop
//! live here once.

use crate::game_hub::standings::{Finish, RoomStandings};
use crate::handlers::respond::error_code;
use std::collections::{BTreeMap, HashMap};
use std::future::Future;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use axum::extract::{Path, State};
use axum::http::{request::Parts, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::Json;
use chrono::Utc;
use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use sqlx::{PgConnection, PgPool, Row};
use uuid::Uuid;

use crate::app_state::AppState;
use crate::auth::middleware::AuthUser;
use crate::game_hub::handlers::csrf;
use crate::game_hub::spectators::{self, Spectators};
use crate::game_hub::RoomError;
use crate::room_clock::{IdleWatch, RoomClock};
use crate::wallet;
use crate::write_behind::Ordered;

#[derive(Debug)]
pub enum Error {
    /// A refusal the client understands (`room_full`, `bet_closed`, …).
    Code(&'static str),
    Db(sqlx::Error),
    /// The room's writer cannot accept another change right now.
    Unavailable,
}
impl From<sqlx::Error> for Error {
    fn from(e: sqlx::Error) -> Self {
        Self::Db(e)
    }
}
pub type Result<T> = std::result::Result<T, Error>;

/// One dealer-room game: pure rules over its in-memory state, plus the SQL
/// that makes a settled round durable.
pub trait DealerRoom: Send + Sync + Sized + 'static {
    /// Label for storage-error logs.
    const LABEL: &'static str;
    /// Label for dealer-loop logs; also the room-code lease game.
    const DEALER: &'static str;
    /// Seats per room, humans and AI co-bettors together.
    const MAX_MEMBERS: usize;
    const POLICY: wallet::Policy;
    /// The `POST …/bet` body.
    type Bet: DeserializeOwned + Send;
    type State: Clone + Serialize + DeserializeOwned + Send + Sync + 'static;
    /// Everything the writer needs to make one settled round durable.
    type Round: Clone + Send + Sync + 'static;

    fn new_state(now: i64, bots: usize) -> Self::State;
    /// The id of the round currently open (its settlement receipt key).
    fn round_id(state: &Self::State) -> Uuid;
    fn bots(state: &Self::State) -> usize;
    fn deadline_ms(state: &Self::State) -> i64;
    fn settled(state: &Self::State) -> bool;
    fn shift_release_clocks(state: &mut Self::State, elapsed_ms: i64);
    /// The committed-change counter kept inside the snapshot; 0 for rows
    /// written before rooms moved into memory.
    fn throws(state: &Self::State) -> &crate::game_hub::throws::RoomThrows;
    fn throws_mut(state: &mut Self::State) -> &mut crate::game_hub::throws::RoomThrows;
    fn revision(state: &Self::State) -> i64;
    fn set_revision(state: &mut Self::State, revision: i64);
    /// The room's session length and countdown, kept in the snapshot.
    fn clock(state: &Self::State) -> &RoomClock;
    fn clock_mut(state: &mut Self::State) -> &mut RoomClock;
    /// Settles a due round in memory: `None` when nothing was due.
    fn settle(code: &str, state: &mut Self::State, now: i64) -> Option<Self::Round>;
    /// Per-bettor chip deltas of a settled round.
    fn deltas(round: &Self::Round) -> Vec<(Uuid, i64)>;
    /// Opens the next round once the result has been shown; true if it did.
    fn advance(state: &mut Self::State, now: i64) -> bool;
    /// The player has a wager on a round that has not settled yet.
    fn bet_pending(state: &Self::State, uid: Uuid) -> bool;
    fn open_bets(state: &Self::State) -> bool;
    /// Applies a wager; `Ok(false)` for an exact retry that changes nothing.
    fn bet(
        state: &mut Self::State,
        uid: Uuid,
        bet: Self::Bet,
        balance: i64,
        now: i64,
    ) -> Result<bool>;
    fn view(
        code: &str,
        state: &Self::State,
        uid: Uuid,
        balance: i64,
        members: &[(Uuid, String)],
        now: i64,
    ) -> Value;
    /// Writes a settled round inside the snapshot transaction. Bettors whose
    /// account (and wallet) has since been deleted are skipped.
    fn persist_round(
        conn: &mut PgConnection,
        code: &str,
        round: &Self::Round,
    ) -> impl Future<Output = sqlx::Result<()>> + Send;
    fn registry() -> &'static Registry<Self>;
    /// The room leaderboard kept in the snapshot. Humans are added here as
    /// rounds are accepted; the game's `settle` adds its AI co-bettors.
    fn standings(state: &Self::State) -> &RoomStandings;
    fn standings_mut(state: &mut Self::State) -> &mut RoomStandings;
    /// 允许朋友观战, kept in the snapshot; rows written before the option
    /// carry none and read as allowed.
    fn allow_spectators(state: &Self::State) -> bool;
    fn set_allow_spectators(state: &mut Self::State, allow: bool);
    /// Top-level [`DealerRoom::view`] fields that belong to the viewer's own
    /// seat (wallet, own wager, own result). A spectator frame nulls them.
    const PRIVATE_FIELDS: &'static [&'static str];
}

/// AI seats the 真人 lobby offers at most; a room with more was opened in the
/// 人机 practice mode, which nobody watches (like Hold'em practice).
const FRIEND_ROOM_MAX_BOTS: usize = 3;

/// Whether friends may watch this room: the host's choice on a friend room.
fn spectatable<R: DealerRoom>(state: &R::State) -> bool {
    R::allow_spectators(state) && R::bots(state) <= FRIEND_ROOM_MAX_BOTS
}

/// `room_not_found` is 404, any other refusal 409, storage trouble 503.
pub fn response<R: DealerRoom>(result: Result<Value>) -> Response {
    match result {
        Ok(value) => Json(value).into_response(),
        Err(Error::Code(code)) => error_code(
            if code == "room_not_found" {
                StatusCode::NOT_FOUND
            } else {
                StatusCode::CONFLICT
            },
            code,
        ),
        Err(Error::Db(e)) => {
            tracing::warn!(error=%e, "{} storage error", R::LABEL);
            unavailable()
        }
        Err(Error::Unavailable) => unavailable(),
    }
}
fn unavailable() -> Response {
    error_code(StatusCode::SERVICE_UNAVAILABLE, "storage_unavailable")
}

// ── In-memory rooms ─────────────────────────────────────────────────────────

struct Member {
    name: String,
    /// Seat reserved while the membership row is being written, or whose
    /// commit outcome is unknown (reconciled by [`sweep`]).
    joining: bool,
    /// Leave accepted; no new wagers while its commit is in flight.
    leaving: bool,
    since: Instant,
}
impl Member {
    fn new(name: String, joining: bool) -> Self {
        Self {
            name,
            joining,
            leaving: false,
            since: Instant::now(),
        }
    }
}

struct Snapshot<R: DealerRoom> {
    users: Vec<Uuid>,
    revision: i64,
    state: Arc<R::State>,
    round: Option<Arc<R::Round>>,
    buy_in: Option<(Uuid, i64, i64, Uuid)>,
}
impl<R: DealerRoom> Clone for Snapshot<R> {
    fn clone(&self) -> Self {
        Self {
            users: self.users.clone(),
            revision: self.revision,
            state: self.state.clone(),
            round: self.round.clone(),
            buy_in: self.buy_in,
        }
    }
}

struct Room<R: DealerRoom> {
    state: Arc<R::State>,
    members: BTreeMap<Uuid, Member>,
    writer: Ordered<Option<Snapshot<R>>>,
    touched: Instant,
    /// Human inactivity, on the same rules as every other game room.
    idle: IdleWatch,
    /// Removed from the registry; a stale handle must not seat anyone.
    closed: bool,
    /// Live state sockets: `(connection id, player, outgoing frames)`.
    subscribers: Vec<(u64, Uuid, tokio::sync::mpsc::Sender<String>)>,
    /// Read-only sockets of friends watching: never members, never counted as
    /// subscribers, so they deal no round and hold nothing open.
    spectators: Spectators,
    handoff_guard: Option<crate::release_handoff::ActorGuard>,
    release_paused: bool,
}

type Shared<R> = Arc<tokio::sync::Mutex<Room<R>>>;

/// One game's open rooms and seat index.
pub struct Registry<R: DealerRoom> {
    rooms: Mutex<BTreeMap<String, Shared<R>>>,
    seats: Mutex<BTreeMap<Uuid, String>>,
    /// Serializes one player's create/join/leave.
    users: Mutex<BTreeMap<Uuid, Arc<tokio::sync::Mutex<()>>>>,
    recovered: tokio::sync::OnceCell<()>,
}
impl<R: DealerRoom> Default for Registry<R> {
    fn default() -> Self {
        Self::new()
    }
}
impl<R: DealerRoom> Registry<R> {
    pub const fn new() -> Self {
        Self {
            rooms: Mutex::new(BTreeMap::new()),
            seats: Mutex::new(BTreeMap::new()),
            users: Mutex::new(BTreeMap::new()),
            recovered: tokio::sync::OnceCell::const_new(),
        }
    }
    fn room(&self, code: &str) -> Option<Shared<R>> {
        self.rooms.lock().unwrap().get(code).cloned()
    }
    fn all(&self) -> Vec<(String, Shared<R>)> {
        let rooms = self.rooms.lock().unwrap();
        rooms.iter().map(|(c, r)| (c.clone(), r.clone())).collect()
    }
    /// The room the player is seated in (or joining), from memory.
    pub fn seat(&self, uid: Uuid) -> Option<String> {
        self.seats.lock().unwrap().get(&uid).cloned()
    }
    fn user_lock(&self, uid: Uuid) -> Arc<tokio::sync::Mutex<()>> {
        let mut users = self.users.lock().unwrap();
        users.retain(|_, l| Arc::strong_count(l) > 1);
        users.entry(uid).or_default().clone()
    }
    fn release_seat(&self, uid: Uuid, code: &str) {
        let mut seats = self.seats.lock().unwrap();
        if seats.get(&uid).is_some_and(|c| c == code) {
            seats.remove(&uid);
        }
    }
}

/// Bounded, card-free staff projection; clone registry handles before awaiting locks.
pub(crate) async fn admin_activity<R: DealerRoom>() -> Vec<crate::game_hub::activity::RoomActivity>
{
    let mut views = Vec::new();
    for (id, shared) in R::registry().all() {
        let room = shared.lock().await;
        if room.closed {
            continue;
        }
        let humans: Vec<_> = room
            .members
            .iter()
            .filter(|(_, m)| !m.joining && !m.leaving)
            .enumerate()
            .map(|(i, (uid, _))| (*uid, i as u8))
            .collect();
        if humans.is_empty() {
            continue;
        }
        views.push(crate::game_hub::activity::RoomActivity {
            expires_at_ms: R::clock(&room.state).expires_at_ms,
            game: R::DEALER,
            id,
            started: true,
            humans,
            bots: R::bots(&room.state),
        });
    }
    views
}

fn now_ms() -> i64 {
    Utc::now().timestamp_millis()
}

fn writer<R: DealerRoom>(pool: &PgPool, code: &str) -> Ordered<Option<Snapshot<R>>> {
    let (pool, code) = (pool.clone(), code.to_owned());
    Ordered::new(None, move |_durable, next: Option<Snapshot<R>>| {
        let (pool, code) = (pool.clone(), code.clone());
        async move {
            if let Some(snapshot) = &next {
                persist::<R>(&pool, &code, snapshot)
                    .await
                    .map_err(|e| e.to_string())?;
            }
            Ok(next)
        }
    })
}

/// One accepted change, exactly once: the stored revision fences a retry
/// whose first attempt committed but lost its acknowledgement.
async fn persist<R: DealerRoom>(pool: &PgPool, code: &str, snap: &Snapshot<R>) -> sqlx::Result<()> {
    let mut tx = pool.begin().await?;
    // Account-before-room-before-wallet matches deletion and other game writers.
    // Pending settlements include soft-deleted participants until purge claim.
    let mut users = snap.users.clone();
    users.extend(
        snap.round
            .iter()
            .flat_map(|round| R::deltas(round).into_iter().map(|(user, _)| user)),
    );
    users.extend(snap.buy_in.map(|(user, _, _, _)| user));
    users.sort_unstable();
    users.dedup();
    let mut active = Vec::new();
    for user in users {
        match crate::db::lock_game_participants(&mut tx, &[user]).await {
            Ok(()) => active.push(user),
            Err(sqlx::Error::RowNotFound) => {}
            Err(error) => return Err(error),
        }
    }
    let stored: Option<i64> = sqlx::query_scalar(&format!(
        "SELECT revision FROM game_room_checkpoints WHERE code=$1 AND game='{}' FOR UPDATE",
        R::DEALER
    ))
    .bind(code)
    .fetch_optional(&mut *tx)
    .await?;
    // A room row is deleted only after its last leave drained this writer.
    let Some(stored) = stored else {
        return Ok(());
    };
    if stored >= snap.revision {
        return Ok(());
    }
    if let Some((user, before, after, receipt)) =
        snap.buy_in.filter(|(user, _, _, _)| active.contains(user))
    {
        wallet::refill_snapshot_on(
            &mut tx,
            R::POLICY.game,
            user,
            before,
            after,
            code,
            snap.revision,
            receipt,
        )
        .await?;
    }
    if let Some(round) = &snap.round {
        R::persist_round(&mut tx, code, round).await?;
    }
    sqlx::query(&format!(
        "UPDATE game_room_checkpoints SET state=$2,revision=($2->>'revision')::bigint,updated_at=now() WHERE code=$1 AND game='{}'",
        R::DEALER
    ))
    .bind(code)
    .bind(json!(snap.state.as_ref()))
    .execute(&mut *tx)
    .await?;
    tx.commit().await
}

/// Commits `next` as the room's new state: queued for the writer first, so a
/// full or failing writer refuses the change instead of losing it.
fn accept<R: DealerRoom>(
    code: &str,
    room: &mut Room<R>,
    next: R::State,
    round: Option<R::Round>,
) -> Result<()> {
    accept_change(code, room, next, round, None)
}
fn accept_change<R: DealerRoom>(
    code: &str,
    room: &mut Room<R>,
    mut next: R::State,
    round: Option<R::Round>,
    buy_in: Option<(Uuid, i64, i64, Uuid)>,
) -> Result<()> {
    // After shutdown starts the writers drain what they already accepted; a
    // round left unsettled here is still due for the next owner.
    if room.release_paused || !crate::write_behind::accepting() {
        return Err(Error::Unavailable);
    }
    room.writer.health().map_err(|_| Error::Unavailable)?;
    if let Some(round) = &round {
        for (uid, delta) in R::deltas(round) {
            let name = room.members.get(&uid).map_or("", |m| m.name.as_str());
            R::standings_mut(&mut next).finish(Some(uid), name, Finish::from_delta(delta));
        }
    }
    let revision = R::revision(&room.state) + 1;
    R::set_revision(&mut next, revision);
    // The room and ordered writer share immutable snapshots, including retries.
    let next = Arc::new(next);
    let round = round.map(Arc::new);
    room.writer
        .enqueue(Some(Snapshot {
            users: room.members.keys().copied().collect(),
            revision,
            state: next.clone(),
            round: round.clone(),
            buy_in,
        }))
        .map_err(|_| Error::Unavailable)?;
    if let Some(round) = round {
        for (uid, delta) in R::deltas(&round) {
            if let Some(w) = wallet::projected(R::POLICY.game, uid) {
                wallet::project_play(
                    R::POLICY.game,
                    uid,
                    w.balance + delta,
                    delta,
                    true,
                    delta > 0,
                );
            }
        }
    }
    if let Some((user, _, after, _)) = buy_in {
        wallet::project_refill(R::POLICY.game, user, after);
    }
    room.state = next;
    push::<R>(code, room);
    Ok(())
}

/// Sends every seated subscriber its own view. A full queue skips this frame;
/// the next change or the dealer's one-second heartbeat carries a fresh one.
///
/// Spectators share one frame, built only when someone watches; a spectator
/// whose queue is full is dropped at once (`Spectators::send`).
fn push<R: DealerRoom>(code: &str, room: &mut Room<R>) {
    if !room.spectators.is_empty() {
        let frame = spectator_view(code, room, room.spectators.count()).to_string();
        room.spectators.send(&frame);
    }
    if room.subscribers.is_empty() {
        return;
    }
    let seated: Vec<Uuid> = room
        .subscribers
        .iter()
        .map(|(_, uid, _)| *uid)
        .filter(|uid| room.members.get(uid).is_some_and(|m| !m.joining))
        .collect();
    let frames: Vec<(Uuid, String)> = seated
        .into_iter()
        .map(|uid| (uid, view(code, room, uid).to_string()))
        .collect();
    // A player no longer seated loses the socket (dropping the sender ends it).
    room.subscribers
        .retain(|(_, uid, tx)| match frames.iter().find(|(u, _)| u == uid) {
            Some((_, frame)) => !matches!(
                tx.try_send(frame.clone()),
                Err(tokio::sync::mpsc::error::TrySendError::Closed(_))
            ),
            None => false,
        });
}

/// Settles a due round and opens the next one after its result has shown.
fn tick<R: DealerRoom>(code: &str, room: &mut Room<R>, now: i64) -> Result<()> {
    if room.release_paused {
        return Err(Error::Unavailable);
    }
    if now < R::deadline_ms(&room.state) {
        return Ok(());
    }
    let mut next = room.state.as_ref().clone();
    let round = R::settle(code, &mut next, now);
    // A room whose session clock ran out deals no new round; it closes once
    // the last result has shown (`close_idle`).
    let advanced = !R::clock(&next).expired(now) && R::advance(&mut next, now);
    if round.is_none() && !advanced {
        return Ok(());
    }
    accept(code, room, next, round)
}

/// Idle tables settle existing wagers once, then wait for a viewer to reopen.
fn settle_unwatched<R: DealerRoom>(code: &str, room: &mut Room<R>, now: i64) -> Result<()> {
    if R::settled(&room.state) || now < R::deadline_ms(&room.state) {
        return Ok(());
    }
    let mut next = room.state.as_ref().clone();
    if let Some(round) = R::settle(code, &mut next, now) {
        accept(code, room, next, Some(round))?;
    }
    Ok(())
}

fn balance<R: DealerRoom>(uid: Uuid) -> i64 {
    wallet::projected(R::POLICY.game, uid).map_or(0, |w| w.balance)
}

fn view<R: DealerRoom>(code: &str, room: &Room<R>, uid: Uuid) -> Value {
    let members: Vec<(Uuid, String)> = room
        .members
        .iter()
        .filter(|(_, m)| !m.joining)
        .map(|(id, m)| (*id, m.name.clone()))
        .collect();
    let mut out = R::view(
        code,
        &room.state,
        uid,
        balance::<R>(uid),
        &members,
        now_ms(),
    );
    if let Some(members) = out["members"].as_array_mut() {
        let mut bot = 0;
        for member in members {
            member["interaction_key"] = if let Some(id) = member["user_id"].as_str() {
                json!(format!("user:{id}"))
            } else {
                let key = format!("bot:{bot}");
                bot += 1;
                json!(key)
            };
        }
    }
    if let Some(seat) = out["members"].as_array().and_then(|rows| {
        rows.iter()
            .position(|row| row["user_id"].as_str() == Some(&uid.to_string()))
    }) {
        out["throws"] =
            R::throws(&room.state).state(uid, seat, R::round_id(&room.state).to_string());
    }
    // Clients order snapshots from HTTP and the socket by (revision, time).
    if let Some(fields) = out.as_object_mut() {
        fields.insert("revision".into(), json!(R::revision(&room.state)));
        fields.insert("room_clock".into(), R::clock(&room.state).view(now_ms()));
        fields.insert("spectators".into(), json!(room.spectators.count()));
    }
    out
}

/// The frame nobody's seat sees: the public round (cards or dice, phase,
/// countdown, every seat's shown wager and settled result, the road), with
/// no wallet, own wager or throw budget.
fn spectator_view<R: DealerRoom>(code: &str, room: &Room<R>, watching: usize) -> Value {
    let mut out = view(code, room, Uuid::nil());
    if let Some(fields) = out.as_object_mut() {
        for field in R::PRIVATE_FIELDS {
            fields.insert((*field).into(), Value::Null);
        }
        fields.insert("throws".into(), Value::Null);
    }
    spectators::mark(&mut out, watching);
    out
}

/// Best-effort `{t:"spectators", n}` to every socket. A full seat queue just
/// misses it; unlike a state frame it never ends a seat's socket.
fn announce_spectators<R: DealerRoom>(room: &mut Room<R>) {
    let payload = json!({"t": "spectators", "n": room.spectators.count()}).to_string();
    room.spectators.send(&payload);
    for (_, _, out) in &room.subscribers {
        let _ = out.try_send(payload.clone());
    }
}

/// Why `uid` may not watch this room now, if anything.
fn watch_refusal<R: DealerRoom>(
    room: &Room<R>,
    uid: Uuid,
    now: i64,
) -> std::result::Result<(), RoomError> {
    if room.release_paused {
        return Err(RoomError::StorageUnavailable);
    }
    if room.closed || R::clock(&room.state).expired(now) {
        return Err(RoomError::RoomNotFound);
    }
    if !spectatable::<R>(&room.state) {
        return Err(RoomError::SpectateDisabled);
    }
    if room.members.contains_key(&uid) {
        return Err(RoomError::AlreadySeated);
    }
    Ok(())
}

/// `uid` now holds a seat here: their spectator sockets end with
/// `already_seated` (those tabs reopen as the player), and every watcher is
/// checked against the new player's block list off the room lock. Spectator
/// delivery is held from now (before the new seat is ever broadcast) until
/// that answer: a blocked watcher never sees a frame with the new player.
fn took_seat<R: DealerRoom>(
    pool: &PgPool,
    code: &str,
    shared: &Shared<R>,
    room: &mut Room<R>,
    uid: Uuid,
) {
    if room.spectators.seated(uid) {
        announce_spectators(room);
    }
    let watchers = room.spectators.users();
    if watchers.is_empty() {
        return;
    }
    room.spectators.hold();
    let (pool, code, shared) = (pool.clone(), code.to_owned(), shared.clone());
    tokio::spawn(async move {
        let blocked: Vec<Uuid> = match sqlx::query_scalar(
            "SELECT w.id FROM users w LEFT JOIN users p ON p.id = $1 \
             WHERE w.id = ANY($2) \
               AND ($1 = ANY(COALESCE(w.blocked_users, ARRAY[]::uuid[])) \
                    OR w.id = ANY(COALESCE(p.blocked_users, ARRAY[]::uuid[])))",
        )
        .bind(uid)
        .bind(&watchers)
        .fetch_all(&pool)
        .await
        {
            Ok(rows) => rows,
            // Fail closed: an unreadable block list ends every watcher's view.
            Err(error) => {
                tracing::warn!("[{}] spectator block check failed: {error}", R::DEALER);
                watchers
            }
        };
        let mut room = shared.lock().await;
        block_answer(&code, &mut room, &blocked);
    });
}

/// The block lookup answered: refuse the blocked watchers, end the hold and,
/// once nothing holds delivery, resync everyone left with the current frame.
fn block_answer<R: DealerRoom>(code: &str, room: &mut Room<R>, blocked: &[Uuid]) {
    let refused = room
        .spectators
        .refuse(blocked, RoomError::BlockedBySeatedPlayer);
    if !room.spectators.release() {
        return;
    }
    if !room.spectators.is_empty() {
        let frame = spectator_view(code, room, room.spectators.count()).to_string();
        room.spectators.send(&frame);
    }
    if refused {
        announce_spectators(room);
    }
}

fn seated<R: DealerRoom>(room: &Room<R>, uid: Uuid) -> Result<()> {
    if room.closed {
        return Err(Error::Code("room_not_found"));
    }
    match room.members.get(&uid) {
        Some(m) if !m.joining => Ok(()),
        _ => Err(Error::Code("room_not_found")),
    }
}

// ── Room operations ─────────────────────────────────────────────────────────

/// Runs one membership change to completion even if the request is dropped
/// (admission timeouts cancel handlers), one at a time per player.
async fn membership<R, F, T>(uid: Uuid, op: F) -> Result<T>
where
    R: DealerRoom,
    F: Future<Output = Result<T>> + Send + 'static,
    T: Send + 'static,
{
    let request = crate::release_handoff::admit_request(false).ok_or(Error::Unavailable)?;
    let lock = R::registry().user_lock(uid);
    tokio::spawn(async move {
        let _request = request;
        let _serial = lock.lock().await;
        op.await
    })
    .await
    .map_err(|_| Error::Unavailable)?
}

/// The seated wallet, read and locked inside the membership transaction so
/// nothing fallible follows the commit. `None` when it has no wallet.
async fn lock_wallet(
    conn: &mut PgConnection,
    game: i16,
    uid: Uuid,
) -> sqlx::Result<Option<wallet::Wallet>> {
    let row = sqlx::query(
        "SELECT balance, hands_played, hands_won, net_won, refills FROM wallets \
         WHERE user_id=$1 AND game=$2 FOR UPDATE",
    )
    .bind(uid)
    .bind(game)
    .fetch_optional(conn)
    .await?;
    Ok(row.map(|r| wallet::Wallet {
        balance: r.get("balance"),
        hands_played: r.get("hands_played"),
        hands_won: r.get("hands_won"),
        net_won: r.get("net_won"),
        refills: r.get("refills"),
    }))
}

/// A commit whose acknowledgement failed may still have landed: ask the DB.
/// `None` when that cannot be told yet (the sweep reconciles later).
async fn member_row<R: DealerRoom>(pool: &PgPool, uid: Uuid, code: &str) -> Option<bool> {
    for attempt in 0..4u32 {
        let found: sqlx::Result<bool> = sqlx::query_scalar(&format!(
            "SELECT EXISTS(SELECT 1 FROM game_room_checkpoint_members WHERE user_id=$1 AND code=$2 AND game='{}')",
            R::DEALER
        ))
        .bind(uid)
        .bind(code)
        .fetch_one(pool)
        .await;
        if let Ok(found) = found {
            return Some(found);
        }
        tokio::time::sleep(Duration::from_millis(100 << attempt)).await;
    }
    None
}

enum Commit<T> {
    Done(T),
    /// Nothing was committed.
    Refused(Error),
    /// The commit itself failed; it may or may not have landed.
    Unknown(Error),
}

pub async fn open_room<R: DealerRoom>(
    pool: &PgPool,
    uid: Uuid,
    name: String,
    bots: usize,
    duration_secs: u32,
) -> Result<String> {
    open_room_with::<R>(
        pool,
        uid,
        name,
        bots,
        duration_secs,
        spectators::default_allow(),
    )
    .await
}

/// Opens a room; `allow_spectators` is the host's 允许朋友观战.
pub async fn open_room_with<R: DealerRoom>(
    pool: &PgPool,
    uid: Uuid,
    name: String,
    bots: usize,
    duration_secs: u32,
    allow_spectators: bool,
) -> Result<String> {
    let pool = pool.clone();
    membership::<R, _, _>(uid, async move {
        open_room_now::<R>(&pool, uid, name, bots, duration_secs, allow_spectators).await
    })
    .await
}

async fn open_room_now<R: DealerRoom>(
    pool: &PgPool,
    uid: Uuid,
    name: String,
    bots: usize,
    duration_secs: u32,
    allow_spectators: bool,
) -> Result<String> {
    recover::<R>(pool).await?;
    let registry = R::registry();
    let bots = bots
        .min(crate::casino_bots::MAX_BOTS)
        .min(R::MAX_MEMBERS - 1);
    wallet::ensure(pool, R::POLICY, uid).await?;
    if registry.seat(uid).is_some() {
        return Err(Error::Code("in_other_room"));
    }
    let code = crate::room_code::allocate(pool, R::DEALER)
        .await
        .map_err(|e| Error::Code(e.code()))?;
    let handoff_guard =
        crate::release_handoff::register(crate::release_handoff::rooms::id(R::DEALER, &code), true)
            .ok_or(Error::Unavailable)?;
    let now = now_ms();
    let mut state = R::new_state(now, bots);
    R::set_allow_spectators(&mut state, allow_spectators);
    // Rounds deal from the start, so the session clock does too.
    let clock = R::clock_mut(&mut state);
    *clock = RoomClock::new(duration_secs);
    clock.start(now);
    R::set_revision(&mut state, 1);
    let committed = async {
        let mut tx = pool.begin().await.map_err(|e| Commit::Refused(e.into()))?;
        crate::db::lock_active_users(&mut tx, &[uid]).await.map_err(|e| Commit::Refused(e.into()))?;
        // Admission lock: serializes refills and solo play against taking a seat.
        let mut wallet = lock_wallet(&mut tx, R::POLICY.game, uid)
            .await
            .map_err(|e| Commit::Refused(e.into()))?
            .ok_or(Commit::Refused(Error::Code("storage_unavailable")))?;
        // Persist the room and its first member in one database round trip.
        // A membership conflict still refuses admission and rolls back this
        // transaction, including the checkpoint written by the CTE.
        let inserted = sqlx::query(&format!(
            "WITH checkpoint AS (INSERT INTO game_room_checkpoints(game,code,revision,state,creator_user_id) \
             VALUES('{}',$1,($2->>'revision')::bigint,$2,$3) RETURNING game,code) \
             INSERT INTO game_room_checkpoint_members(game,user_id,code,name) \
             SELECT game,$3,code,$4 FROM checkpoint ON CONFLICT(game,user_id) DO NOTHING",
            R::DEALER
        ))
        .bind(&code)
        .bind(json!(state))
        .bind(uid)
        .bind(&name)
        .execute(&mut *tx)
        .await
        .map_err(|e| Commit::Refused(e.into()))?
        .rows_affected();
        if inserted == 0 {
            return Err(Commit::Refused(Error::Code("in_other_room")));
        }
        wallet.balance = wallet::start_session_on(&mut tx, R::POLICY, uid, Some(&code))
            .await
            .map_err(|e| Commit::Refused(e.into()))?;
        tx.commit().await.map_err(|e| Commit::Unknown(e.into()))?;
        Ok(wallet)
    }
    .await;
    let (wallet, joining) = match committed.map_or_else(|e| e, Commit::Done) {
        Commit::Done(w) => (Some(w), false),
        Commit::Refused(e) => return Err(e),
        Commit::Unknown(e) => match member_row::<R>(pool, uid, &code).await {
            Some(false) => return Err(e),
            Some(true) => (wallet::ensure(pool, R::POLICY, uid).await.ok(), false),
            None => (None, true),
        },
    };
    // Without a wallet in hand the seat stays reserved; the sweep finishes it.
    let joining = joining || wallet.is_none();
    if let Some(w) = wallet {
        wallet::activate(R::POLICY.game, uid, &code, w);
    }
    let mut members = BTreeMap::new();
    members.insert(uid, Member::new(name, joining));
    let room = Room {
        state: Arc::new(state),
        members,
        writer: writer::<R>(pool, &code),
        touched: Instant::now(),
        idle: IdleWatch::new(),
        closed: false,
        subscribers: Vec::new(),
        spectators: Spectators::default(),
        handoff_guard: Some(handoff_guard),
        release_paused: false,
    };
    registry.seats.lock().unwrap().insert(uid, code.clone());
    registry
        .rooms
        .lock()
        .unwrap()
        .insert(code.clone(), handoff_room::<R>(pool, &code, room)?);
    if joining {
        return Err(Error::Unavailable);
    }
    Ok(code)
}

pub async fn join_room<R: DealerRoom>(
    pool: &PgPool,
    code: &str,
    uid: Uuid,
    name: String,
) -> Result<()> {
    let (pool, code) = (pool.clone(), code.to_owned());
    membership::<R, _, _>(uid, async move {
        join_room_now::<R>(&pool, &code, uid, name).await
    })
    .await
}

async fn join_room_now<R: DealerRoom>(
    pool: &PgPool,
    code: &str,
    uid: Uuid,
    name: String,
) -> Result<()> {
    recover::<R>(pool).await?;
    let registry = R::registry();
    wallet::ensure(pool, R::POLICY, uid).await?;
    let shared = registry.room(code).ok_or(Error::Code("room_not_found"))?;
    match registry.seat(uid) {
        Some(c) if c == code => {
            // Seated already, or a join whose outcome is still unknown.
            let room = shared.lock().await;
            return match room.members.get(&uid) {
                Some(m) if !m.joining => Ok(()),
                _ => Err(Error::Unavailable),
            };
        }
        Some(_) => return Err(Error::Code("in_other_room")),
        None => {}
    }
    {
        let mut room = shared.lock().await;
        if room.release_paused {
            return Err(Error::Unavailable);
        }
        if room.closed {
            return Err(Error::Code("room_not_found"));
        }
        if room.members.len() + R::bots(&room.state) >= R::MAX_MEMBERS {
            return Err(Error::Code("room_full"));
        }
        room.members.insert(uid, Member::new(name.clone(), true));
        registry.seats.lock().unwrap().insert(uid, code.to_owned());
    }
    let committed = async {
        let mut tx = pool.begin().await.map_err(|e| Commit::Refused(e.into()))?;
        crate::db::lock_active_users(&mut tx, &[uid]).await.map_err(|e| Commit::Refused(e.into()))?;
        sqlx::query("SELECT 1 FROM game_room_checkpoints WHERE game=$1 AND code=$2 FOR KEY SHARE")
            .bind(R::DEALER).bind(code).fetch_one(&mut *tx).await
            .map_err(|e| Commit::Refused(e.into()))?;
        let mut wallet = lock_wallet(&mut tx, R::POLICY.game, uid)
            .await
            .map_err(|e| Commit::Refused(e.into()))?
            .ok_or(Commit::Refused(Error::Code("storage_unavailable")))?;
        let inserted = sqlx::query(&format!(
            "INSERT INTO game_room_checkpoint_members(game,user_id,code,name) VALUES('{}',$1,$2,$3) ON CONFLICT(game,user_id) DO NOTHING",
            R::DEALER
        ))
        .bind(uid)
        .bind(code)
        .bind(&name)
        .execute(&mut *tx)
        .await
        .map_err(|e| Commit::Refused(e.into()))?
        .rows_affected();
        if inserted == 0 {
            return Err(Commit::Refused(Error::Code("in_other_room")));
        }
        wallet.balance = wallet::start_session_on(&mut tx, R::POLICY, uid, Some(code))
            .await
            .map_err(|e| Commit::Refused(e.into()))?;
        tx.commit().await.map_err(|e| Commit::Unknown(e.into()))?;
        Ok(wallet)
    }
    .await;
    let outcome = match committed.map_or_else(|e| e, Commit::Done) {
        Commit::Done(w) => Ok(Some(w)),
        Commit::Refused(e) => Err(e),
        Commit::Unknown(e) => match member_row::<R>(pool, uid, code).await {
            Some(false) => Err(e),
            Some(true) => Ok(wallet::ensure(pool, R::POLICY, uid).await.ok()),
            None => Ok(None),
        },
    };
    let mut room = shared.lock().await;
    match outcome {
        Ok(Some(w)) => {
            wallet::activate(R::POLICY.game, uid, code, w);
            if let Some(m) = room.members.get_mut(&uid) {
                m.joining = false;
            }
            room.idle.activity();
            took_seat(pool, code, &shared, &mut room, uid);
            Ok(())
        }
        // Committed or unknown without a wallet: the seat stays reserved and
        // the sweep reconciles it.
        Ok(None) => Err(Error::Unavailable),
        Err(e) => {
            room.members.remove(&uid);
            drop(room);
            registry.release_seat(uid, code);
            Err(e)
        }
    }
}

pub async fn room_state<R: DealerRoom>(pool: &PgPool, code: &str, uid: Uuid) -> Result<Value> {
    recover::<R>(pool).await?;
    let shared = R::registry()
        .room(code)
        .ok_or(Error::Code("room_not_found"))?;
    let mut room = shared.lock().await;
    seated(&room, uid)?;
    room.touched = Instant::now();
    tick(code, &mut room, now_ms())?;
    Ok(view(code, &room, uid))
}

pub async fn place_bet<R: DealerRoom>(
    pool: &PgPool,
    code: &str,
    uid: Uuid,
    bet: R::Bet,
) -> Result<Value> {
    recover::<R>(pool).await?;
    let shared = R::registry()
        .room(code)
        .ok_or(Error::Code("room_not_found"))?;
    let mut room = shared.lock().await;
    seated(&room, uid)?;
    if room.members.get(&uid).is_some_and(|m| m.leaving) {
        return Err(Error::Code("bet_closed"));
    }
    room.touched = Instant::now();
    let mut next = room.state.as_ref().clone();
    if R::bet(&mut next, uid, bet, balance::<R>(uid), now_ms())? {
        accept(code, &mut room, next, None)?;
        room.idle.activity();
    }
    Ok(view(code, &room, uid))
}

/// Waits until everything the room's writer has accepted so far is durable.
async fn flushed<R: DealerRoom>(shared: &Shared<R>) -> Result<()> {
    let waiter = shared.lock().await.writer.durable();
    waiter.await.map_err(|_| Error::Unavailable)
}

pub async fn leave_room<R: DealerRoom>(pool: &PgPool, uid: Uuid) -> Result<()> {
    let pool = pool.clone();
    membership::<R, _, _>(uid, async move { leave_room_now::<R>(&pool, uid).await }).await
}

async fn leave_room_now<R: DealerRoom>(pool: &PgPool, uid: Uuid) -> Result<()> {
    recover::<R>(pool).await?;
    let registry = R::registry();
    let Some(code) = registry.seat(uid) else {
        return Ok(());
    };
    let Some(shared) = registry.room(&code) else {
        registry.release_seat(uid, &code);
        return Ok(());
    };
    {
        let mut room = shared.lock().await;
        tick(&code, &mut room, now_ms())?;
        if R::bet_pending(&room.state, uid) {
            return Err(Error::Code("bet_pending"));
        }
        match room.members.get_mut(&uid) {
            Some(m) if !m.joining && !m.leaving => {
                m.leaving = true;
                m.since = Instant::now();
            }
            // A join or leave whose outcome is still being reconciled.
            Some(_) => return Err(Error::Unavailable),
            None => {
                drop(room);
                registry.release_seat(uid, &code);
                return Ok(());
            }
        }
    }
    // The wallet leaves memory only after every settlement it saw is durable.
    let deleted = async {
        if let Err(e) = flushed(&shared).await {
            return Commit::Refused(e);
        }
        let result = sqlx::query(&format!(
            "DELETE FROM game_room_checkpoint_members WHERE user_id=$1 AND code=$2 AND game='{}'",
            R::DEALER
        ))
        .bind(uid)
        .bind(&code)
        .execute(pool)
        .await;
        match result {
            Ok(_) => Commit::Done(()),
            Err(e) => match member_row::<R>(pool, uid, &code).await {
                Some(false) => Commit::Done(()),
                Some(true) => Commit::Refused(Error::Db(e)),
                None => Commit::Unknown(Error::Db(e)),
            },
        }
    }
    .await;
    let empty = {
        let mut room = shared.lock().await;
        match deleted {
            Commit::Done(()) => {}
            // Still seated for certain: betting reopens.
            Commit::Refused(e) => {
                if let Some(m) = room.members.get_mut(&uid) {
                    m.leaving = false;
                }
                return Err(e);
            }
            // Maybe gone: stay blocked until the sweep asks the DB.
            Commit::Unknown(_) => return Err(Error::Unavailable),
        }
        room.members.remove(&uid);
        empty_and_closed(&mut room)
    };
    wallet::retire(R::POLICY.game, uid, &code);
    registry.release_seat(uid, &code);
    if empty {
        drop_room::<R>(pool, &code, &shared).await?;
    }
    Ok(())
}

/// A room fenced for closing: its members, and their membership locks so no
/// join or leave interleaves until the seats are released.
struct Closing {
    members: Vec<Uuid>,
    /// The open round when fenced. A fenced room opens no new round, and every
    /// room starts with a fresh id, so it tells this room's row from a later
    /// room given the same code.
    round: Uuid,
    _held: Vec<tokio::sync::OwnedMutexGuard<()>>,
}

/// Whether `round` is the open round of any live room. Solo 押大小 refuses it
/// as a request id: the room settlement reuses it as its receipt key.
pub async fn round_in_play<R: DealerRoom>(pool: &PgPool, round: Uuid) -> Result<bool> {
    recover::<R>(pool).await?;
    for (_, shared) in R::registry().all() {
        if R::round_id(&shared.lock().await.state) == round {
            return Ok(true);
        }
    }
    Ok(false)
}

/// Registers a state socket for a seated player and sends its first view.
async fn subscribe<R: DealerRoom>(
    code: &str,
    uid: Uuid,
    id: u64,
    out: tokio::sync::mpsc::Sender<String>,
) -> std::result::Result<(), String> {
    let refused = || json!({"t":"error","code":"room_not_found"}).to_string();
    let shared = R::registry().room(code).ok_or_else(refused)?;
    let mut room = shared.lock().await;
    if seated(&room, uid).is_err() {
        return Err(refused());
    }
    // A seat never also watches the same room.
    if room.spectators.seated(uid) {
        announce_spectators(&mut room);
    }
    let first = view(code, &room, uid).to_string();
    out.try_send(first).map_err(|_| refused())?;
    room.subscribers.push((id, uid, out));
    Ok(())
}

async fn unsubscribe<R: DealerRoom>(code: &str, id: u64) {
    if let Some(shared) = R::registry().room(code) {
        shared.lock().await.subscribers.retain(|(c, _, _)| *c != id);
    }
}

/// Admits a spectator socket: registered, not blocked by or blocking any
/// member (two-sided), the room open and spectatable, the viewer not seated
/// here, within the per-room and per-person caps. Refusals are error frames.
/// Admission touches no membership, wallet, presence or idle clock.
async fn watch<R: DealerRoom>(
    pool: &PgPool,
    code: &str,
    uid: Uuid,
    id: u64,
    out: tokio::sync::mpsc::Sender<String>,
) -> std::result::Result<(), String> {
    let refused = |code: RoomError| json!({"t":"error","code":code}).to_string();
    let shared = R::registry()
        .room(code)
        .ok_or_else(|| refused(RoomError::RoomNotFound))?;
    let mut roster: Vec<Uuid> = {
        let room = shared.lock().await;
        watch_refusal::<R>(&room, uid, now_ms()).map_err(refused)?;
        room.members.keys().copied().collect()
    };
    // The block lookup runs off the room lock; a member who sat down during
    // it is looked up again before admitting.
    for _ in 0..3 {
        match crate::game_hub::handlers::spectator_admission(pool, uid, &roster).await {
            Ok(None) => {}
            Ok(Some(code)) => return Err(json!({"t":"error","code":code}).to_string()),
            Err(error) => {
                tracing::error!("[{}] spectator admission lookup failed: {error}", R::DEALER);
                return Err(refused(RoomError::StorageUnavailable));
            }
        }
        let mut room = shared.lock().await;
        watch_refusal::<R>(&room, uid, now_ms()).map_err(refused)?;
        let now: Vec<Uuid> = room.members.keys().copied().collect();
        if now.iter().any(|m| !roster.contains(m)) {
            roster = now;
            continue;
        }
        let watching =
            room.spectators.count() + usize::from(!room.spectators.users().contains(&uid));
        let frame = spectator_view(code, &room, watching).to_string();
        let count = json!({"t": "spectators", "n": watching}).to_string();
        room.spectators
            .admit(uid, id, out, count, frame)
            .map_err(refused)?;
        announce_spectators(&mut room);
        return Ok(());
    }
    Err(refused(RoomError::StorageUnavailable))
}

/// A spectator socket ended.
async fn unwatch<R: DealerRoom>(code: &str, id: u64) {
    if let Some(shared) = R::registry().room(code) {
        let mut room = shared.lock().await;
        if room.spectators.detach(id) {
            announce_spectators(&mut room);
        }
    }
}

#[derive(Deserialize)]
pub struct SocketQuery {
    room: String,
    client_source: Option<String>,
    /// `1`: a read-only spectator socket, never a seat.
    #[serde(default)]
    watch: Option<String>,
}

/// `/ws/{game}?room=` — the table's state pushed on every change and once a
/// second, instead of the client polling `GET …/rooms/{code}`. Wagers, joins
/// and leaves stay HTTP.
pub async fn socket<R: DealerRoom>(
    State(s): State<AppState>,
    auth: AuthUser,
    axum::extract::Query(q): axum::extract::Query<SocketQuery>,
    headers: axum::http::HeaderMap,
    connect_info: Option<axum::extract::ConnectInfo<std::net::SocketAddr>>,
    ws: axum::extract::ws::WebSocketUpgrade,
) -> Response {
    if !crate::game_socket::origin_allowed(&headers) {
        return (StatusCode::FORBIDDEN, "forbidden origin").into_response();
    }
    if let Err(e) = recover::<R>(&s.pool).await {
        return response::<R>(Err(Error::Db(e)));
    }
    // Watch intent always takes the spectator path; its refusals (including
    // `already_seated`) arrive as error frames on the upgraded socket.
    let watching = q.watch.as_deref() == Some("1");
    if !watching {
        let seated_here = match R::registry().room(&q.room) {
            Some(shared) => seated(&*shared.lock().await, auth.user_id).is_ok(),
            None => false,
        };
        if !seated_here {
            return response::<R>(Err(Error::Code("room_not_found")));
        }
    }
    let Some(socket_auth) = crate::game_socket::SocketAuth::capture(
        &s,
        &headers,
        connect_info.map(|ci| ci.0.ip()),
        auth.user_id,
    )
    .await
    else {
        return StatusCode::UNAUTHORIZED.into_response();
    };
    let socket_auth = socket_auth.with_source(
        R::DEALER,
        &q.room,
        auth.user_id,
        crate::client_source::from_request(&headers, q.client_source.as_deref()).as_deref(),
    );
    // ADR-105 global socket ceiling, as on /ws. A player already holds a
    // seat, so that socket is a returning one; a spectator holds nothing and
    // arrives new. The guard lives until the socket ends.
    let arrival = if watching {
        crate::admission::Arrival::New
    } else {
        crate::admission::Arrival::Returning
    };
    let admission = match crate::admission::ws_admission().try_admit_kind(arrival) {
        Ok(guard) => guard,
        Err(rejection) => {
            tracing::warn!(
                "[ws_admission_rejected] user_id={} reason={} live={}",
                auth.user_id,
                rejection.code(),
                crate::admission::ws_admission().live()
            );
            return crate::admission::ws_refusal(
                rejection,
                crate::admission::ws_admission().retry_after_sec(),
            );
        }
    };
    static IDS: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(1);
    let id = IDS.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    let (code, uid) = (q.room, auth.user_id);
    if watching {
        let pool = s.pool.clone();
        return ws
            .max_message_size(1024)
            .on_upgrade(move |socket| async move {
                let _admission = admission;
                let (code_ref, pool_ref) = (&code, &pool);
                // Read-only: a keepalive is answered, everything else ignored.
                let attached = crate::game_socket::serve::<Value, _, _, _>(
                    socket,
                    socket_auth,
                    8,
                    |out| watch::<R>(pool_ref, code_ref, uid, id, out),
                    |msg| async move {
                        if msg["t"] == "ping" {
                            if let Some(shared) = R::registry().room(code_ref) {
                                shared.lock().await.spectators.pong(id);
                            }
                        }
                        true
                    },
                )
                .await;
                if attached {
                    unwatch::<R>(&code, id).await;
                }
            });
    }
    ws.max_message_size(1024)
        .on_upgrade(move |socket| async move {
            let _admission = admission;
            let code_ref = &code;
            crate::game_socket::serve::<Value, _, _, _>(
                socket,
                socket_auth,
                8,
                |out| subscribe::<R>(code_ref, uid, id, out),
                |msg| handle_social::<R>(code_ref, uid, id, msg),
            )
            .await;
            unsubscribe::<R>(&code, id).await;
        })
}

// Uses the room owner lock and ordered checkpoint writer, just like a wager.
async fn handle_social<R: DealerRoom>(code: &str, uid: Uuid, connection: u64, msg: Value) -> bool {
    let operation = if msg["t"] == "throw_item" {
        crate::server_performance::WsOperation::Social
    } else {
        crate::server_performance::WsOperation::Rejected
    };
    let _timing = crate::request_timing::WsTimer::with_operation(
        crate::request_timing::ws_received(),
        R::LABEL,
        operation,
    );
    if msg["t"] != "throw_item" {
        return true;
    }
    let Some(shared) = R::registry().room(code) else {
        return false;
    };
    let mut room = shared.lock().await;
    if seated(&room, uid).is_err()
        || !room
            .subscribers
            .iter()
            .any(|(c, u, _)| *c == connection && *u == uid)
    {
        return false;
    }
    let result = (|| -> Result<String> {
        if room.members.get(&uid).is_some_and(|m| m.leaving) || room.release_paused {
            return Err(Error::Unavailable);
        }
        if msg["revision"].as_i64() != Some(R::revision(&room.state)) {
            return Err(Error::Code("throw_invalid_target"));
        }
        let target = msg["target_seat"]
            .as_u64()
            .and_then(|v| usize::try_from(v).ok())
            .ok_or(Error::Code("throw_invalid_target"))?;
        let item: crate::protocol::ThrowItem = serde_json::from_value(msg["item"].clone())
            .map_err(|_| Error::Code("throw_invalid_target"))?;
        let snapshot = view::<R>(code, &room, uid);
        let from = snapshot["throws"]["seat"]
            .as_u64()
            .ok_or(Error::Code("throw_invalid_target"))? as usize;
        let target_key = msg["target_key"]
            .as_str()
            .ok_or(Error::Code("throw_invalid_target"))?;
        let occupied = snapshot["members"].as_array().is_some_and(|m| {
            m.get(target)
                .is_some_and(|row| row["interaction_key"].as_str() == Some(target_key))
        });
        let mut next = (*room.state).clone();
        let round = R::round_id(&next).to_string();
        let payload = R::throws_mut(&mut next)
            .throw(uid, from, target, occupied, item, round)
            .map_err(Error::Code)?;
        accept::<R>(code, &mut room, next, None)?;
        Ok(payload)
    })();
    match result {
        Ok(payload) => {
            room.subscribers.retain(|(_, _, out)| {
                !matches!(
                    out.try_send(payload.clone()),
                    Err(tokio::sync::mpsc::error::TrySendError::Closed(_))
                )
            });
            room.spectators.send(&payload);
        }
        Err(err) => {
            let code = match err {
                Error::Code(code) => code,
                _ => "throw_unavailable",
            };
            if let Some((_, _, out)) = room.subscribers.iter().find(|(c, _, _)| *c == connection) {
                let _ = out.try_send(json!({"t":"error","code":code}).to_string());
            }
        }
    }
    true
}

// ── HTTP ────────────────────────────────────────────────────────────────────

/// `{}` (or no body) opens a human room; `bots` seats that many AI co-bettors.
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CreateRequest {
    #[serde(default)]
    pub bots: u8,
    /// Session length; omitted means one hour (`crate::room_clock`).
    #[serde(default)]
    pub duration_secs: Option<u32>,
    /// 允许朋友观战; omitted means allowed.
    #[serde(default = "spectators::default_allow")]
    pub allow_spectators: bool,
}
impl Default for CreateRequest {
    fn default() -> Self {
        Self {
            bots: 0,
            duration_secs: None,
            allow_spectators: spectators::default_allow(),
        }
    }
}

pub async fn create<R: DealerRoom>(
    State(s): State<AppState>,
    auth: AuthUser,
    parts: Parts,
    body: Option<Json<CreateRequest>>,
) -> Response {
    if let Some(r) = csrf(&parts) {
        return r;
    }
    // A dealer room seats any number of players by code, so bots can never
    // close it to other people: it is account-only.
    if let Some(r) = crate::auth::guest::reject_guest_unless(&s.pool, auth.user_id, false).await {
        return r;
    }
    let Json(req) = body.unwrap_or_default();
    let duration_secs =
        match crate::room_clock::resolve_duration(&s.config.app_env, req.duration_secs) {
            Ok(secs) => secs,
            Err(resp) => return resp,
        };
    let name = crate::game_hub::display_name(&s.pool, auth.user_id).await;
    response::<R>(
        open_room_with::<R>(
            &s.pool,
            auth.user_id,
            name,
            usize::from(req.bots),
            duration_secs,
            req.allow_spectators,
        )
        .await
        .map(|code| json!({"code":code})),
    )
}

pub async fn join<R: DealerRoom>(
    State(s): State<AppState>,
    auth: AuthUser,
    Path(code): Path<String>,
    parts: Parts,
) -> Response {
    if let Some(r) = csrf(&parts) {
        return r;
    }
    if let Some(r) = crate::auth::guest::reject_guest_unless(&s.pool, auth.user_id, false).await {
        return r;
    }
    let name = crate::game_hub::display_name(&s.pool, auth.user_id).await;
    response::<R>(
        join_room::<R>(&s.pool, &code, auth.user_id, name)
            .await
            .map(|()| json!({"code":code})),
    )
}

pub async fn state<R: DealerRoom>(
    State(s): State<AppState>,
    auth: AuthUser,
    Path(code): Path<String>,
    headers: axum::http::HeaderMap,
) -> Response {
    let result = room_state::<R>(&s.pool, &code, auth.user_id).await;
    if result.is_ok() {
        let source = crate::client_source::from_request(&headers, None);
        crate::client_source::observe_http(R::DEALER, &code, auth.user_id, source.as_deref());
    }
    response::<R>(result)
}

/// `GET /api/<game>/rooms/:code/standings` — the room leaderboard, members only.
pub async fn standings<R: DealerRoom>(
    State(s): State<AppState>,
    auth: AuthUser,
    Path(code): Path<String>,
) -> Response {
    let result = async {
        recover::<R>(&s.pool).await?;
        let shared = R::registry()
            .room(&code)
            .ok_or(Error::Code("room_not_found"))?;
        let room = shared.lock().await;
        seated(&room, auth.user_id)?;
        Ok(R::standings(&room.state).records(auth.user_id, true))
    }
    .await;
    response::<R>(result)
}

pub async fn bet<R: DealerRoom>(
    State(s): State<AppState>,
    auth: AuthUser,
    Path(code): Path<String>,
    parts: Parts,
    Json(req): Json<R::Bet>,
) -> Response {
    if let Some(r) = csrf(&parts) {
        return r;
    }
    response::<R>(place_bet::<R>(&s.pool, &code, auth.user_id, req).await)
}

pub async fn leave<R: DealerRoom>(
    State(s): State<AppState>,
    auth: AuthUser,
    parts: Parts,
) -> Response {
    if let Some(r) = csrf(&parts) {
        return r;
    }
    response::<R>(
        leave_room::<R>(&s.pool, auth.user_id)
            .await
            .map(|()| json!({"ok":true})),
    )
}

/// Settles every due round; the next round opens when someone looks at the
/// table, so an abandoned table stops dealing.
async fn settle_due<R: DealerRoom>(now: i64) {
    for (code, shared) in R::registry().all() {
        let mut room = shared.lock().await;
        // A closing room holds no human wager and must write nothing more:
        // its final flush is what the closure's row fence relies on.
        if room.closed || room.release_paused {
            continue;
        }
        if room.subscribers.is_empty() {
            // No seat watching: settle a due round, but open no new one.
            // Spectators never keep a table dealing; they still get the
            // one-second frame so their view stays live.
            if let Err(error) = settle_unwatched(&code, &mut room, now) {
                tracing::warn!(?error, %code, "{} round settlement deferred", R::DEALER);
            }
            push::<R>(&code, &mut room);
            continue;
        }
        // Watched tables run on the dealer's clock (nobody polls them), and the
        // one-second push carries countdowns and bot wagers as they show.
        if let Err(error) = tick(&code, &mut room, now) {
            tracing::warn!(?error, %code, "{} round deferred", R::DEALER);
        }
        push::<R>(&code, &mut room);
    }
}

/// Settles due rounds from memory once a second, forever, on the table owner
/// only; rooms reconcile with their membership rows every minute.
pub async fn run_dealer<R: DealerRoom>(pool: PgPool) {
    crate::runtime_owner::wait_active().await;
    while let Err(error) = recover::<R>(&pool).await {
        tracing::error!(%error, "{} room recovery failed; retrying", R::DEALER);
        tokio::time::sleep(Duration::from_secs(1)).await;
    }
    let mut tick_every = tokio::time::interval(Duration::from_secs(1));
    tick_every.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    let mut last_sweep = Instant::now();
    loop {
        tick_every.tick().await;
        let Some(_request) = crate::release_handoff::admit_request(false) else {
            continue;
        };
        settle_due::<R>(now_ms()).await;
        close_idle::<R>(&pool, now_ms()).await;
        if last_sweep.elapsed() >= Duration::from_secs(60) {
            last_sweep = Instant::now();
            if let Err(error) = sweep::<R>(&pool).await {
                tracing::warn!(?error, "{} idle room sweep failed", R::DEALER);
            }
        }
    }
}

#[derive(Deserialize)]
pub struct BuyInRequest {
    pub units: i64,
    pub revision: i64,
}

pub async fn buy_in<R: DealerRoom>(
    State(s): State<AppState>,
    auth: AuthUser,
    Path(code): Path<String>,
    parts: Parts,
    Json(req): Json<BuyInRequest>,
) -> Response {
    if let Some(r) = csrf(&parts) {
        return r;
    }
    let result = async {
        recover::<R>(&s.pool).await?;
        let shared = R::registry()
            .room(&code)
            .ok_or(Error::Code("room_not_found"))?;
        let mut room = shared.lock().await;
        add_chips::<R>(&code, &mut room, auth.user_id, &req)?;
        Ok(view::<R>(&code, &room, auth.user_id))
    }
    .await;
    response::<R>(result)
}

fn add_chips<R: DealerRoom>(
    code: &str,
    room: &mut Room<R>,
    user: Uuid,
    req: &BuyInRequest,
) -> Result<()> {
    seated(room, user)?;
    if R::clock(&room.state).expired(now_ms()) || room.members.get(&user).is_some_and(|m| m.leaving)
    {
        return Err(Error::Code("room_expired"));
    }
    if !(100..=1000).contains(&req.units) {
        return Err(Error::Code("invalid_options"));
    }
    if R::revision(&room.state) != req.revision {
        return Err(Error::Code("stale_revision"));
    }
    let before = balance::<R>(user);
    // Dealer games use ten chips as one base unit, matching their chip increment.
    let after = before
        .checked_add(req.units * 10)
        .filter(|n| *n <= 10_000)
        .ok_or(Error::Code("buy_in_limit"))?;
    let next = room.state.as_ref().clone();
    accept_change::<R>(
        code,
        room,
        next,
        None,
        Some((user, before, after, Uuid::new_v4())),
    )?;
    Ok(())
}

pub(crate) async fn release_snapshot(
    pool: &PgPool,
    game: &str,
    code: &str,
) -> sqlx::Result<(i64, Value)> {
    if !matches!(game, "baccarat" | "sicbo") {
        return Err(sqlx::Error::RowNotFound);
    }
    sqlx::query_as("SELECT revision,state FROM game_room_checkpoints WHERE code=$1 AND game=$2")
        .bind(code)
        .bind(game)
        .fetch_one(pool)
        .await
}
pub(crate) async fn release_members(
    pool: &PgPool,
    game: &str,
    code: &str,
) -> sqlx::Result<Vec<Uuid>> {
    if !matches!(game, "baccarat" | "sicbo") {
        return Err(sqlx::Error::RowNotFound);
    }
    sqlx::query_scalar("SELECT user_id FROM game_room_checkpoint_members WHERE code=$1 AND game=$2")
        .bind(code)
        .bind(game)
        .fetch_all(pool)
        .await
}

#[path = "casino_room_lifecycle.rs"]
mod lifecycle;
pub use lifecycle::recover;
use lifecycle::{close_idle, drop_room, empty_and_closed, handoff_room, sweep};
