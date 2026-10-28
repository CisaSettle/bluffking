//! Pending lottery wallet effects use the same admission/publication authority
//! as poker. The immutable ledger identity reconciles refreshes after lost ACKs.
use super::*;

#[derive(Clone, Debug, PartialEq, serde::Serialize, serde::Deserialize)]
pub(crate) struct PendingEffect {
    pub balance: i64,
    pub net: i64,
    pub played: i32,
    pub won: i32,
    pub ledger_delta: i64,
    pub day: Option<NaiveDate>,
}
impl PendingEffect {
    pub(crate) fn apply(&self, wallet: &mut Wallet) -> sqlx::Result<()> {
        wallet.balance = wallet
            .balance
            .checked_add(self.balance)
            .filter(|v| *v >= 0)
            .ok_or_else(unavailable)?;
        wallet.net_won = wallet
            .net_won
            .checked_add(self.net)
            .ok_or_else(unavailable)?;
        wallet.hands_played = wallet
            .hands_played
            .checked_add(self.played)
            .ok_or_else(unavailable)?;
        wallet.hands_won = wallet
            .hands_won
            .checked_add(self.won)
            .ok_or_else(unavailable)?;
        Ok(())
    }
}

pub(crate) fn accept_effect(
    pool: &PgPool,
    owner: Uuid,
    record: Uuid,
    effect: PendingEffect,
    spend: i64,
    expected: i64,
    enqueue: impl FnOnce() -> sqlx::Result<()>,
) -> sqlx::Result<i64> {
    let state = state(pool);
    let mut inner = state.inner.lock().unwrap_or_else(|e| e.into_inner());
    if !inner.ready || inner.active_users.contains_key(&owner) {
        return Err(unavailable());
    }
    let user = inner
        .users
        .get(&owner)
        .filter(|u| u.available && !u.retired)
        .ok_or_else(unavailable)?;
    if let Some(old) = user.effects.get(&record) {
        if old != &effect {
            return Err(unavailable());
        }
        return Ok(user.wallet.as_ref().ok_or_else(unavailable)?.balance);
    }
    // Admission publishes seats while holding this same bean authority lock.
    let directory = crate::live_directory::memory_snapshot_for_user(pool, owner)?;
    let reserved = reservations::reserved(&inner, &directory, owner, false, None, None)?;
    let mut wallet = user.wallet.clone().ok_or_else(unavailable)?;
    if wallet.balance != expected {
        return Err(unavailable());
    }
    if effect.day.is_some_and(|day| user.days.contains(&day)) {
        return Err(unavailable());
    }
    if spend < 0 || wallet.balance.saturating_sub(reserved) < spend {
        return Err(sqlx::Error::Protocol("lottery_insufficient_balance".into()));
    }
    if inner.count >= MAX_ROWS || inner.bytes.saturating_add(256) > MAX_BYTES {
        return Err(unavailable());
    }
    effect.apply(&mut wallet)?;
    let hold = if user.effect_hold.is_none() {
        crate::local_runtime::hold_sicbo(owner).map_err(sqlx::Error::Io)?
    } else {
        None
    };
    enqueue()?;
    if let Some(key) = inner.users.get(&owner).and_then(|u| rank(owner, u)) {
        inner.ranks.remove(&key);
    }
    inner.epoch = inner.epoch.wrapping_add(1);
    let generation = inner.epoch;
    let balance = wallet.balance;
    let user = inner.users.get_mut(&owner).ok_or_else(unavailable)?;
    user.wallet = Some(wallet);
    user.effects.insert(record, effect.clone());
    if hold.is_some() {
        user.effect_hold = hold;
    }
    let new_day = effect.day.is_some_and(|day| user.days.insert(day));
    user.bytes += 256;
    user.generation = generation;
    inner.count += 1 + usize::from(new_day);
    inner.bytes += 256;
    if let Some(key) = inner.users.get(&owner).and_then(|u| rank(owner, u)) {
        inner.ranks.insert(key);
    }
    state.published.notify_waiters();
    Ok(balance)
}

pub(crate) fn effect_committed(pool: &PgPool, owner: Uuid, record: Uuid) {
    let state = state(pool);
    let mut inner = state.inner.lock().unwrap_or_else(|e| e.into_inner());
    inner.epoch = inner.epoch.wrapping_add(1);
    let generation = inner.epoch;
    if let Some(user) = inner.users.get_mut(&owner) {
        if user.effects.remove(&record).is_some() {
            user.bytes -= 256;
            user.generation = generation;
            if user.effects.is_empty() {
                user.effect_hold = None;
            }
            inner.count -= 1;
            inner.bytes -= 256;
        }
    }
    state.published.notify_waiters();
}

/// The lottery writer fenced before this effect committed, so SQL never saw
/// it. Drop it and reload the wallet from SQL with the remaining effects.
pub(crate) fn effect_abandoned(pool: &PgPool, owner: Uuid, record: Uuid) {
    let state = state(pool);
    let mut inner = state.inner.lock().unwrap_or_else(|e| e.into_inner());
    if let Some(user) = inner.users.get_mut(&owner) {
        if user.effects.remove(&record).is_some() {
            user.bytes -= 256;
            if user.effects.is_empty() {
                user.effect_hold = None;
            }
            inner.count -= 1;
            inner.bytes -= 256;
        }
    }
    invalidate_locked(&mut inner, owner);
    drop(inner);
    state.wake.notify_one();
    state.published.notify_waiters();
}

pub(crate) fn daily_claimed(pool: &PgPool, owner: Uuid, day: NaiveDate) -> bool {
    state(pool)
        .inner
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .users
        .get(&owner)
        .is_some_and(|u| u.days.contains(&day))
}

pub(crate) fn settlement_balance(pool: &PgPool, owner: Uuid) -> sqlx::Result<i64> {
    let state = state(pool);
    let inner = state.inner.lock().unwrap_or_else(|e| e.into_inner());
    if !inner.ready || inner.active_users.contains_key(&owner) {
        return Err(unavailable());
    }
    inner
        .users
        .get(&owner)
        .filter(|u| u.available && !u.retired)
        .and_then(|u| u.wallet.as_ref())
        .map(|w| w.balance)
        .ok_or_else(unavailable)
}
