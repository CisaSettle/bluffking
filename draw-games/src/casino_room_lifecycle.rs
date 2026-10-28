use super::*;

/// Loads every open room, its members and their wallets once per process,
/// before the first room request or dealer tick.
pub async fn recover<R: DealerRoom>(pool: &PgPool) -> sqlx::Result<()> {
    let registry = R::registry();
    registry
        .recovered
        .get_or_try_init(|| async {
            let mut after_code: Option<String> = None;
            loop {
                let rooms = sqlx::query(
                    "SELECT code,state FROM game_room_checkpoints WHERE game=$1 \
                     AND ($2::text IS NULL OR code>$2) ORDER BY code LIMIT $3",
                )
                .bind(R::DEALER)
                .bind(after_code.as_deref())
                .bind(crate::pagination::DEFAULT_LIMIT)
                .fetch_all(pool)
                .await?;
                if rooms.is_empty() {
                    break;
                }
                after_code = rooms.last().map(|row| row.get("code"));
                let codes: Vec<String> = rooms.iter().map(|row| row.get("code")).collect();
                let mut by_room: HashMap<String, BTreeMap<Uuid, Member>> = HashMap::new();
                let mut users = Vec::new();
                let mut after_user: Option<Uuid> = None;
                loop {
                    let members = sqlx::query(
                        "SELECT m.user_id,m.code,COALESCE(m.name,'') AS name \
                         FROM game_room_checkpoint_members m JOIN game_room_checkpoints c ON c.game=m.game AND c.code=m.code \
                         WHERE m.game=$1 AND m.code=ANY($2) AND ($3::uuid IS NULL OR m.user_id>$3) \
                         ORDER BY m.user_id LIMIT $4",
                    )
                    .bind(R::DEALER)
                    .bind(&codes)
                    .bind(after_user)
                    .bind(crate::pagination::DEFAULT_LIMIT)
                    .fetch_all(pool)
                    .await?;
                    for row in &members {
                        let uid = row.get("user_id");
                        by_room
                            .entry(row.get("code"))
                            .or_default()
                            .insert(uid, Member::new(row.get("name"), false));
                        users.push(uid);
                        after_user = Some(uid);
                    }
                    if members.len() < crate::pagination::DEFAULT_LIMIT as usize {
                        break;
                    }
                }
                let room_count = rooms.len();
                install::<R>(pool, rooms, by_room)?;
                // At most 100 rooms (eight members each) are retained outside
                // the registry. Activate owned rooms' wallets before admitting
                // any request through the shared recovery guard.
                for uid in users {
                    if let Some(code) = registry.seat(uid) {
                        let w = wallet::ensure(pool, R::POLICY, uid).await?;
                        wallet::activate(R::POLICY.game, uid, &code, w);
                    }
                }
                if room_count < crate::pagination::DEFAULT_LIMIT as usize {
                    break;
                }
            }
            Ok::<(), sqlx::Error>(())
        })
        .await
        .map(|_| ())
}

/// Puts recovered rooms and their seats into memory; no await while the
/// registry locks are held.
pub(super) fn install<R: DealerRoom>(
    pool: &PgPool,
    rooms: Vec<sqlx::postgres::PgRow>,
    mut by_room: HashMap<String, BTreeMap<Uuid, Member>>,
) -> sqlx::Result<()> {
    let registry = R::registry();
    let mut map = registry.rooms.lock().unwrap();
    let mut seats = registry.seats.lock().unwrap();
    for row in rooms {
        let code: String = row.get("code");
        // A later page or wallet load may have failed after earlier rooms
        // were installed. A retry completes recovery without rewinding those
        // rooms or replacing their writer/handoff registrations.
        if map.contains_key(&code) {
            continue;
        }
        if crate::release_handoff::sealed_boot()
            && crate::release_handoff::rooms::restoring(R::DEALER, &code).is_none()
        {
            continue;
        }
        let id = crate::release_handoff::rooms::id(R::DEALER, &code);
        // Local development: a room another local backend drives stays there;
        // it is not a recovery failure for this process.
        if crate::local_runtime::owned_by_another_backend(id) {
            tracing::info!(%code, "{} room owned by another local backend", R::DEALER);
            continue;
        }
        let mut state: R::State = crate::game_checkpoint::decode(row.get("state"))?;
        // Rooms opened before the session clock start it now (default hour).
        R::clock_mut(&mut state).start(now_ms());
        let handoff_guard = crate::release_handoff::register(id, true)
            .ok_or_else(|| sqlx::Error::Protocol("release room registration refused".into()))?;
        let members = by_room.remove(&code).unwrap_or_default();
        for uid in members.keys() {
            seats.insert(*uid, code.clone());
        }
        let state = Arc::new(state);
        let room = Room {
            pool: pool.clone(),
            registry_generation: Some(crate::room_registry::publish_committed(
                pool,
                R::DEALER,
                &code,
            )?),
            state,
            members,
            writer: writer::<R>(pool, &code, None),
            touched: Instant::now(),
            idle: IdleWatch::new(),
            closed: false,
            subscribers: Vec::new(),
            spectators: Spectators::default(),
            handoff_guard: Some(handoff_guard),
            release_paused: false,
        };
        map.insert(code.clone(), handoff_room::<R>(pool, &code, room)?);
    }
    Ok(())
}

// Each room participates in the same barrier as actor-owned games. The weak
// waiter does not keep closed rooms alive; the registration belongs to the room.
pub(super) fn handoff_room<R: DealerRoom>(
    pool: &PgPool,
    code: &str,
    mut room: Room<R>,
) -> sqlx::Result<Shared<R>> {
    let id = crate::release_handoff::rooms::id(R::DEALER, code);
    if room.handoff_guard.is_none() {
        room.handoff_guard =
            Some(crate::release_handoff::register(id, true).ok_or_else(|| {
                sqlx::Error::Protocol("release room registration refused".into())
            })?);
    }
    let restoring = crate::release_handoff::rooms::restoring(R::DEALER, code);
    room.release_paused = restoring.is_some();
    let shared = Arc::new(tokio::sync::Mutex::new(room));
    let weak = Arc::downgrade(&shared);
    let (pool, code) = (pool.clone(), code.to_owned());
    tokio::spawn(async move {
        let mut restoring = restoring;
        loop {
            crate::release_handoff::requested().await;
            let Some(shared) = weak.upgrade() else {
                return;
            };
            let frozen = if let Some(proof) = restoring.as_ref() {
                crate::release_handoff::rooms::load(proof)
            } else {
                let mut room = shared.lock().await;
                let paused_at_ms = now_ms();
                // Flush the current snapshot as well as queued writes (legacy
                // clocks can have been initialized only in memory).
                let current = room.state.as_ref().clone();
                if accept(&code, &mut room, current, None).is_err() {
                    crate::release_handoff::fail("casino freeze snapshot refused");
                }
                room.release_paused = true;
                if room.members.values().any(|m| m.joining || m.leaving) {
                    crate::release_handoff::fail("casino membership unresolved");
                }
                if let Err(error) = room.writer.drain().await {
                    crate::release_handoff::fail(&error);
                }
                let frozen = crate::release_handoff::rooms::Frozen {
                    checkpoint: json!(room.state.as_ref()),
                    members: room.members.keys().copied().collect(),
                    online: room
                        .subscribers
                        .iter()
                        .filter(|(_, _, tx)| !tx.is_closed())
                        .map(|(_, u, _)| *u)
                        .collect(),
                    paused_at_ms,
                    auxiliary: Value::Null,
                };
                if let Err(error) = crate::release_handoff::rooms::prove(
                    &pool,
                    R::DEALER,
                    &code,
                    R::revision(&room.state) as u64,
                    frozen.clone(),
                )
                .await
                {
                    crate::release_handoff::fail(&error);
                }
                Ok(frozen)
            };
            let frozen = match frozen {
                Ok(value) => value,
                Err(error) => {
                    crate::release_handoff::fail(&error);
                    return;
                }
            };
            let revision = R::revision(&shared.lock().await.state) as u64;
            if !crate::release_handoff::park(id, Some(revision)).await {
                return;
            }
            if restoring.take().is_some() {
                let until = tokio::time::Instant::now() + Duration::from_secs(30);
                loop {
                    let room = shared.lock().await;
                    let ready = frozen.online.iter().all(|u| {
                        room.subscribers
                            .iter()
                            .any(|(_, user, tx)| user == u && !tx.is_closed())
                    });
                    drop(room);
                    if ready || tokio::time::Instant::now() >= until {
                        break;
                    }
                    tokio::time::sleep(Duration::from_millis(20)).await;
                }
            }
            loop {
                let mut room = shared.lock().await;
                let mut next: R::State = match serde_json::from_value(frozen.checkpoint.clone()) {
                    Ok(value) => value,
                    Err(error) => {
                        crate::release_handoff::fail(&error.to_string());
                        return;
                    }
                };
                R::shift_release_clocks(&mut next, now_ms().saturating_sub(frozen.paused_at_ms));
                room.release_paused = false;
                if accept(&code, &mut room, next, None).is_ok() && room.writer.drain().await.is_ok()
                {
                    room.touched = Instant::now();
                    room.idle = IdleWatch::new();
                    break;
                }
                room.release_paused = true;
                drop(room);
                // The predecessor must recover after cancel even when the
                // storage failure that caused the cancellation is transient.
                tokio::time::sleep(Duration::from_millis(100)).await;
            }
        }
    });
    Ok(shared)
}

/// Marks an empty room closed under its lock; joins through a stale handle
/// then refuse.
pub(super) fn empty_and_closed<R: DealerRoom>(
    pool: &PgPool,
    code: &str,
    room: &mut Room<R>,
) -> bool {
    if room.members.is_empty() && !R::open_bets(&room.state) {
        room.closed = true;
    }
    if room.closed {
        if let Some(generation) = room.registry_generation {
            crate::room_registry::retire_pending(pool, R::DEALER, code, generation);
        }
        // Spectators get the same terminal frame every lobby room sends.
        room.spectators.close();
    }
    room.closed
}

/// Keep a closed room fenced until its ordered writer and deletion finish.
/// A failed or unknown DELETE is retried by the same close worker, retaining
/// the generation tombstone and code claim throughout recovery.
pub(super) async fn drop_room<R: DealerRoom>(
    pool: &PgPool,
    code: &str,
    shared: &Shared<R>,
) -> Result<()> {
    let room = shared.lock().await;
    let closing = Closing {
        registry_generation: room.registry_generation,
        require_empty: true,
        members: Vec::new(),
        round: R::round_id(&room.state),
        _held: Vec::new(),
    };
    let request = crate::release_handoff::admit_request(true).ok_or(Error::Unavailable)?;
    let (pool, code, shared) = (pool.clone(), code.to_owned(), shared.clone());
    tokio::spawn(async move {
        let _request = request;
        finish_close::<R>(pool, code, shared, closing).await;
    });
    Ok(())
}

/// Reconciles memory with the membership rows, holding the room lock across
/// the read. Joins and leaves whose outcome was left unknown are finished or
/// undone — only when that player's membership lock is free, so an operation
/// still in flight is never touched — and members whose rows were removed
/// underneath (account deletion cascades) leave idle rooms. An emptied room
/// closes.
pub(super) async fn sweep<R: DealerRoom>(pool: &PgPool) -> Result<()> {
    let registry = R::registry();
    for (code, shared) in registry.all() {
        let mut room = shared.lock().await;
        let idle = room.touched.elapsed() > Duration::from_secs(3600);
        // Hold each unsettled player's membership lock for the whole pass.
        let mut held = Vec::new();
        for (uid, m) in &room.members {
            if m.joining || m.leaving {
                if let Ok(guard) = registry.user_lock(*uid).try_lock_owned() {
                    held.push((*uid, guard));
                }
            }
        }
        if room.release_paused || room.closed || (held.is_empty() && !idle) {
            continue;
        }
        let rows: Vec<Uuid> = sqlx::query_scalar(&format!(
            "SELECT user_id FROM game_room_checkpoint_members WHERE code=$1 AND game='{}'",
            R::DEALER
        ))
        .bind(&code)
        .fetch_all(pool)
        .await?;
        let settled_uids: Vec<Uuid> = held.iter().map(|(u, _)| *u).collect();
        let mut gone = Vec::new();
        let mut finish_join = Vec::new();
        for (uid, m) in room.members.iter_mut() {
            let present = rows.contains(uid);
            if settled_uids.contains(uid) {
                match (m.joining, present) {
                    (true, true) => finish_join.push(*uid),
                    (true, false) | (false, false) => gone.push(*uid),
                    // A leave whose delete did not land: seated again.
                    (false, true) => m.leaving = false,
                }
            } else if idle && !m.joining && !m.leaving && !present {
                gone.push(*uid);
            }
        }
        for uid in finish_join {
            let w = wallet::ensure(pool, R::POLICY, uid).await?;
            wallet::activate(R::POLICY.game, uid, &code, w);
            if let Some(m) = room.members.get_mut(&uid) {
                m.joining = false;
            }
            took_seat(pool, &code, &shared, &mut room, uid);
        }
        for uid in &gone {
            room.members.remove(uid);
            wallet::retire(R::POLICY.game, *uid, &code);
            registry.release_seat(*uid, &code);
        }
        // A previously unknown create becomes authoritative only once its
        // durable membership and wallet have both been reconciled.
        if room.registry_generation.is_none()
            && !room.members.is_empty()
            && room
                .members
                .values()
                .all(|member| !member.joining && !member.leaving)
        {
            room.registry_generation = Some(crate::room_registry::publish_committed(
                pool,
                R::DEALER,
                &code,
            )?);
        }
        let empty = empty_and_closed(pool, &code, &mut room);
        drop(room);
        drop(held);
        if empty {
            drop_room::<R>(pool, &code, &shared).await?;
        }
    }
    Ok(())
}

/// Closes rooms nobody plays in, on the rules every other game room follows
/// (`crate::room_clock::IdleWatch`, owner 2026-09-25): ten minutes without a
/// human wager when one player sits with AI co-bettors, an hour otherwise,
/// and once the session clock has run out and the last result has shown. A
/// round holding human wagers always settles first. Every seat is released,
/// so the room code frees with the row.
pub(super) async fn close_idle<R: DealerRoom>(pool: &PgPool, now: i64) {
    for (code, shared) in R::registry().all() {
        if let Some(closing) = begin_close::<R>(pool, &code, &shared, now).await {
            // Off the dealer loop: a stalled writer must not stop other rooms.
            let Some(request) = crate::release_handoff::admit_request(true) else {
                crate::release_handoff::fail("room close raced frozen barrier");
                continue;
            };
            let pool = pool.clone();
            tokio::spawn(async move {
                let _request = request;
                finish_close::<R>(pool, code, shared, closing).await;
            });
        }
    }
}

/// Decides under the room lock and fences the room (`closed`) when it is due.
pub(super) async fn begin_close<R: DealerRoom>(
    pool: &PgPool,
    code: &str,
    shared: &Shared<R>,
    now: i64,
) -> Option<Closing> {
    let registry = R::registry();
    let mut room = shared.lock().await;
    if room.closed || room.release_paused || crate::release_handoff::draining() {
        return None;
    }
    let bot_room = room.members.len() <= 1 && R::bots(&room.state) > 0;
    let expired = R::clock(&room.state).expired(now);
    let in_play = R::open_bets(&room.state);
    if !room.idle.should_close(in_play, bot_room, expired) {
        return None;
    }
    // A join or leave in flight finishes first; the next pass retries.
    let mut held = Vec::new();
    for (uid, m) in &room.members {
        match registry.user_lock(*uid).try_lock_owned() {
            Ok(guard) if !m.joining && !m.leaving => held.push(guard),
            _ => return None,
        }
    }
    room.closed = true;
    if let Some(generation) = room.registry_generation {
        crate::room_registry::retire_pending(pool, R::DEALER, code, generation);
    }
    // Dropping the senders ends the state sockets; clients then read
    // `room_not_found` and return to the lobby.
    room.subscribers.clear();
    room.spectators.close();
    tracing::info!(%code, "{} room closing after inactivity or expiry", R::DEALER);
    Some(Closing {
        registry_generation: room.registry_generation,
        require_empty: false,
        members: room.members.keys().copied().collect(),
        round: R::round_id(&room.state),
        _held: held,
    })
}

/// Makes a fenced room's closure durable, then frees its seats. The room
/// never reopens: the deletes are retried until they commit, so a lost
/// acknowledgement cannot leave a live room without its rows, and each retry
/// touches only this room's row (`Closing::round`), never a successor's.
pub(super) async fn finish_close<R: DealerRoom>(
    pool: PgPool,
    code: String,
    shared: Shared<R>,
    closing: Closing,
) {
    let mut registration = crate::room_registry::mutation(&pool, Some(&code));
    let mut wait = Duration::from_millis(200);
    loop {
        let deleted = async {
            // Every settlement the wallets saw is durable before the seats go.
            flushed(&shared).await?;
            let mut tx = pool.begin().await?;
            let current_round: Option<String> = sqlx::query_scalar(&format!(
                "SELECT state->>'round_id' FROM game_room_checkpoints WHERE code=$1 AND game='{}' FOR UPDATE",
                R::DEALER
            ))
            .bind(&code)
            .fetch_optional(&mut *tx)
            .await?;
            // Gone already: an earlier attempt committed. A different room
            // generation must never be deleted or have its code released.
            let Some(current_round) = current_round else {
                return Ok(());
            };
            if current_round != closing.round.to_string() {
                return Err(Error::Unavailable);
            }
            if closing.require_empty {
                let occupied: bool = sqlx::query_scalar(
                    "SELECT EXISTS(SELECT 1 FROM game_room_checkpoint_members WHERE code=$1 AND game=$2)",
                )
                .bind(&code)
                .bind(R::DEALER)
                .fetch_one(&mut *tx)
                .await?;
                if occupied {
                    return Err(Error::Unavailable);
                }
            }
            for table in ["game_room_checkpoint_members", "game_room_checkpoints"] {
                sqlx::query(&format!("DELETE FROM {table} WHERE code=$1 AND game=$2"))
                    .bind(&code)
                    .bind(R::DEALER)
                    .execute(&mut *tx)
                    .await?;
            }
            registration.commit(tx.commit()).await?;
            Ok::<(), Error>(())
        }
        .await;
        match deleted {
            Ok(()) => break,
            Err(error) => {
                tracing::warn!(?error, %code, "{} room close retrying", R::DEALER);
                tokio::time::sleep(wait).await;
                wait = (wait * 2).min(Duration::from_secs(10));
            }
        }
    }
    let registry = R::registry();
    {
        let mut rooms = registry.rooms.lock().unwrap();
        if rooms.get(&code).is_some_and(|r| Arc::ptr_eq(r, &shared)) {
            rooms.remove(&code);
        }
    }
    for uid in &closing.members {
        wallet::retire(R::POLICY.game, *uid, &code);
        registry.release_seat(*uid, &code);
    }
    if let Some(generation) = closing.registry_generation {
        crate::room_registry::confirm_retired_pending(&pool, R::DEALER, &code, generation).await;
    }
}
