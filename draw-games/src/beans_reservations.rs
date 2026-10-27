//! Numeric liabilities share the bean authority's admission/publication mutex.
//! Wallet balances include poker stacks; only unencumbered beans may be spent.
use super::*;
use crate::runtime_checkpoint::RuntimeCheckpoint;
use chrono::{DateTime, Utc};

#[derive(Default)]
pub(super) struct Reservations {
    poker: HashMap<(Uuid, Uuid, DateTime<Utc>), (u64, i64)>,
    sicbo: HashMap<String, (u64, HashMap<Uuid, i64>)>,
    sicbo_locks: HashMap<(String, Uuid), std::fs::File>,
}

impl Reservations {
    pub(super) fn checkpoint(&mut self, session: Uuid, checkpoint: &RuntimeCheckpoint) {
        for seat in checkpoint.seats.values() {
            let Some(owner) = &seat.occupant else {
                continue;
            };
            let key = (session, owner.user_id, owner.joined_at);
            let amount = seat
                .stack
                .saturating_add(seat.pending_chips)
                .min(i64::MAX as u64) as i64;
            if self
                .poker
                .get(&key)
                .is_none_or(|(revision, _)| *revision <= checkpoint.revision)
            {
                self.poker.insert(key, (checkpoint.revision, amount));
            }
        }
    }

    pub(super) fn sicbo_commit(&mut self, code: &str, snapshot: &Value) {
        let revision = snapshot["revision"].as_u64().unwrap_or(0);
        if self
            .sicbo
            .get(code)
            .is_some_and(|(latest, _)| *latest > revision)
        {
            return;
        }
        let bets = sicbo_bets(snapshot);
        self.sicbo_locks
            .retain(|(room, user), _| room != code || bets.contains_key(user));
        if bets.is_empty() {
            self.sicbo.remove(code);
        } else {
            self.sicbo.insert(code.into(), (revision, bets));
        }
    }
}

fn sicbo_bets(snapshot: &Value) -> HashMap<Uuid, i64> {
    if snapshot["wallet_unit"] != "beans" || !snapshot["result"].is_null() {
        return HashMap::new();
    }
    snapshot["bets"]
        .as_object()
        .into_iter()
        .flatten()
        .filter_map(|(id, bet)| Some((id.parse().ok()?, bet["stake"].as_i64()?.max(0))))
        .collect()
}

/// Until the corresponding snapshot commits, retain the larger old/new wager.
/// A cancellation acknowledged in memory cannot release crash-recoverable debt.
pub fn sicbo_accepted<T>(
    pool: &PgPool,
    code: &str,
    snapshot: &Value,
    enqueue: impl FnOnce() -> Result<T, crate::casino_room::Error>,
) -> Result<T, crate::casino_room::Error> {
    let state = state(pool);
    let mut inner = state.inner.lock().unwrap_or_else(|e| e.into_inner());
    let bets = sicbo_bets(snapshot);
    let mut locks = Vec::new();
    for user in bets.keys() {
        if !inner
            .reservations
            .sicbo_locks
            .contains_key(&(code.into(), *user))
        {
            if let Some(lock) = crate::local_runtime::hold_sicbo(*user)
                .map_err(|_| crate::casino_room::Error::Unavailable)?
            {
                locks.push(((code.into(), *user), lock));
            }
        }
    }
    let result = enqueue()?;
    inner.reservations.sicbo_locks.extend(locks);
    let entry = inner.reservations.sicbo.entry(code.into()).or_default();
    entry.0 = snapshot["revision"].as_u64().unwrap_or(entry.0);
    for (user, stake) in bets {
        let amount = entry.1.entry(user).or_default();
        *amount = (*amount).max(stake);
    }
    Ok(result)
}

pub fn sicbo_committed(pool: &PgPool, code: &str, snapshot: &Value) {
    state(pool)
        .inner
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .reservations
        .sicbo_commit(code, snapshot);
}

pub fn sync_poker_checkpoint(pool: &PgPool, session: Uuid, checkpoint: &RuntimeCheckpoint) {
    let Ok(directory) = crate::live_directory::memory_snapshot(pool) else {
        return;
    };
    if !directory.sessions.get(&session).is_some_and(is_beans) {
        return;
    }
    state(pool)
        .inner
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .reservations
        .checkpoint(session, checkpoint);
}

fn is_beans(room: &crate::live_directory::LiveSession) -> bool {
    room.session_config
        .as_ref()
        .and_then(|c| c["beans"].as_bool())
        .unwrap_or(false)
}

pub(super) fn reserved(
    inner: &Inner,
    directory: &crate::live_directory::Directory,
    user: Uuid,
    sql_balance: bool,
    excluding_poker: Option<Uuid>,
    excluding_sicbo: Option<&str>,
) -> sqlx::Result<i64> {
    if crate::local_runtime::sicbo_held(user).map_err(sqlx::Error::Io)?
        && !inner
            .users
            .get(&user)
            .is_some_and(|u| !u.effects.is_empty())
        && !inner
            .reservations
            .sicbo
            .values()
            .any(|(_, bets)| bets.contains_key(&user))
    {
        return Err(unavailable());
    }
    if inner.reservations.sicbo.iter().any(|(code, (_, bets))| {
        bets.contains_key(&user)
            && crate::local_runtime::owned_by_another_backend(crate::release_handoff::rooms::id(
                "sicbo", code,
            ))
    }) {
        return Err(unavailable());
    }
    let mut poker = crate::beans::holds::amounts(user);
    for (room, member) in directory.memberships_for(user) {
        if !is_beans(room) || room.ended_at.is_some() {
            continue;
        }
        let amount = if member.left_at.is_none() {
            let checkpoint = room
                .runtime_checkpoint
                .as_ref()
                .map(|value| serde_json::from_value::<RuntimeCheckpoint>(value.clone()))
                .transpose()
                .map_err(|e| sqlx::Error::Protocol(e.to_string()))?;
            let seat = checkpoint.as_ref().and_then(|c| {
                c.seats.values().find(|s| {
                    s.occupant
                        .as_ref()
                        .is_some_and(|o| o.user_id == user && o.joined_at == member.joined_at)
                })
            });
            let base = seat.map_or(i64::from(room.starting_stack), |s| {
                s.stack.saturating_add(s.pending_chips).min(i64::MAX as u64) as i64
            });
            inner
                .reservations
                .poker
                .get(&(room.id, user, member.joined_at))
                .filter(|(rev, _)| *rev >= checkpoint.as_ref().map_or(0, |c| c.revision))
                .map_or(base, |(_, amount)| *amount)
        } else {
            // Bean-table rejoin creates a fresh stack and discards standing
            // chips. Departed seats reserve only actual unsettled hand holds,
            // already included in `poker` above.
            0
        };
        let entry = poker.entry(room.id).or_default();
        *entry = (*entry).max(amount);
    }
    let mut total = 0i64;
    for (session, amount) in poker {
        if Some(session) != excluding_poker {
            total = total.checked_add(amount).ok_or_else(unavailable)?;
        }
    }
    for (code, (_, bets)) in &inner.reservations.sicbo {
        if Some(code.as_str()) != excluding_sicbo {
            total = total
                .checked_add(bets.get(&user).copied().unwrap_or(0))
                .ok_or_else(unavailable)?;
        }
    }
    // Accepted losses are already in the memory wallet, but a SQL debit may
    // lock the older durable balance. Keep these losses covered until COMMIT.
    if sql_balance {
        for effect in inner
            .users
            .get(&user)
            .into_iter()
            .flat_map(|u| u.effects.values())
        {
            total = total
                .checked_add(effect.balance.saturating_neg().max(0))
                .ok_or_else(unavailable)?;
        }
        for delta in inner
            .users
            .get(&user)
            .into_iter()
            .flat_map(|u| u.poker.values())
        {
            total = total
                .checked_add(delta.saturating_neg().max(0))
                .ok_or_else(unavailable)?;
        }
    }
    Ok(total)
}

/// Debit callers hold Mutation before BEGIN and the wallet row through COMMIT.
/// This excludes admission/top-ups while allowing independent games to spend
/// the remainder. Unknown foreign local actors cannot publish numeric stakes.
pub fn available(
    pool: &PgPool,
    user: Uuid,
    balance: i64,
    sql_balance: bool,
    excluding_sicbo: Option<&str>,
) -> sqlx::Result<i64> {
    let state = {
        let _phase = http_phase(HttpPhase::Processing).operation("beans_state_lookup");
        state(pool)
    };
    let inner = {
        let _phase = http_phase(HttpPhase::Processing).operation("beans_available_mutex");
        state.inner.lock().unwrap_or_else(|e| e.into_inner())
    };
    if !inner.ready {
        return Err(unavailable());
    }
    let directory = {
        let _phase = http_phase(HttpPhase::Processing).operation("beans_directory_snapshot");
        crate::live_directory::memory_snapshot(pool)?
    };
    let _phase = http_phase(HttpPhase::Processing).operation("beans_available_compute");
    for (room, member) in directory.memberships_for(user) {
        if is_beans(room)
            && room.ended_at.is_none()
            && member.left_at.is_none()
            && crate::local_runtime::owned_by_another_backend(room.id)
        {
            return Err(unavailable());
        }
    }
    if crate::local_runtime::beans_held(user).map_err(sqlx::Error::Io)?
        && !crate::beans::holds::outstanding(user)
    {
        return Err(unavailable());
    }
    Ok(balance
        .saturating_sub(reserved(
            &inner,
            &directory,
            user,
            sql_balance,
            None,
            excluding_sicbo,
        )?)
        .max(0))
}

/// Reserve a new whole table stack (including pending top-ups) before the
/// actor yields. Validation and reservation share the admission/debit fence.
pub fn reserve_poker_stack(
    pool: &PgPool,
    session: Uuid,
    user: Uuid,
    target: u64,
) -> sqlx::Result<bool> {
    ensure(pool, user)?;
    let state = state(pool);
    let mut inner = state.inner.lock().unwrap_or_else(|e| e.into_inner());
    if !inner.ready || inner.active_writes != 0 {
        return Err(unavailable());
    }
    let balance = inner
        .users
        .get(&user)
        .filter(|u| u.available)
        .and_then(|u| u.wallet.as_ref())
        .ok_or_else(unavailable)?
        .balance;
    let directory = crate::live_directory::memory_snapshot(pool)?;
    let room = directory.sessions.get(&session).ok_or_else(unavailable)?;
    let member = room
        .members
        .iter()
        .find(|m| m.user_id == user && m.left_at.is_none())
        .ok_or_else(unavailable)?;
    let other = reserved(&inner, &directory, user, false, Some(session), None)?;
    let target = i64::try_from(target).map_err(|_| unavailable())?;
    if balance.saturating_sub(other) < target {
        return Ok(false);
    }
    let revision = room
        .runtime_checkpoint
        .as_ref()
        .and_then(|c| c["revision"].as_u64())
        .unwrap_or(0)
        .saturating_add(1);
    inner
        .reservations
        .poker
        .insert((session, user, member.joined_at), (revision, target));
    Ok(true)
}
