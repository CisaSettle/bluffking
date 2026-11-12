//! Pending lottery wallet effects use the same admission/publication authority
//! as poker. The immutable ledger identity reconciles refreshes after lost ACKs.
//!
//! An effect is the [`crate::bean_ledger`] posting (and the bonus credit paid
//! beside it) the lottery runtime has accepted in memory and its ordered
//! writer has not committed yet. A debit
//! (ticket purchase) leaves the memory balance at once. A credit (draw prize,
//! scratch prize, jackpot or card bonus) is in the memory balance too, but a
//! crash before its commit voids that acceptance, so until then it is neither
//! shown ([`lottery_read_balance`]) nor spendable anywhere
//! (`reservations::reserved` keeps it reserved).
use super::*;

/// The wallet side of one accepted lottery event: `delta` is exactly what its
/// play row will carry (and what `net_won` moves by), `bonus` the jackpot or
/// card bonus credited beside it as a non-play row.
#[derive(Clone, Debug, PartialEq, serde::Serialize, serde::Deserialize)]
pub(crate) struct PendingEffect {
    pub delta: i64,
    pub bonus: i64,
    pub played: i32,
    pub won: i32,
}
impl PendingEffect {
    /// Net change of the balance.
    pub(crate) fn balance(&self) -> sqlx::Result<i64> {
        self.delta.checked_add(self.bonus).ok_or_else(unavailable)
    }
    pub(crate) fn apply(&self, wallet: &mut Wallet) -> sqlx::Result<()> {
        wallet.balance = wallet
            .balance
            .checked_add(self.balance()?)
            // Admission already proved a debit is covered. A settlement may
            // since have driven the wallet negative; the effect still applies.
            .ok_or_else(unavailable)?;
        wallet.net_won = wallet
            .net_won
            .checked_add(self.delta)
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
    if spend < 0 || (spend > 0 && wallet.balance.saturating_sub(reserved) < spend) {
        return Err(sqlx::Error::Protocol("lottery_insufficient_balance".into()));
    }
    if inner.count >= MAX_ROWS || inner.bytes.saturating_add(256) > MAX_BYTES {
        return Err(unavailable());
    }
    effect.apply(&mut wallet)?;
    let hold = if user.effect_hold.is_none() {
        crate::local_runtime::hold_dealer(owner).map_err(sqlx::Error::Io)?
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
    user.effects.insert(record, effect);
    if hold.is_some() {
        user.effect_hold = hold;
    }
    user.bytes += 256;
    user.generation = generation;
    inner.count += 1;
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

/// Lottery readers hold their committed projection lock before entering here.
/// Keep pending effects in the spend authority, but hide them in the displayed
/// balance until their lottery projection has been published. Other games keep
/// their existing accepted-result semantics. The spendable amount comes from
/// the accepted balance: a pending purchase is already spent, and a pending
/// prize is reserved until it commits.
pub(crate) fn lottery_read_balance(pool: &PgPool, owner: Uuid) -> sqlx::Result<(i64, i64)> {
    let state = state(pool);
    let inner = state.inner.lock().unwrap_or_else(|e| e.into_inner());
    if !inner.ready || inner.active_users.contains_key(&owner) {
        return Err(unavailable());
    }
    let user = inner
        .users
        .get(&owner)
        .filter(|u| u.available && !u.retired)
        .ok_or_else(unavailable)?;
    let pending = user.effects.values().try_fold(0i64, |sum, effect| {
        sum.checked_add(effect.balance()?).ok_or_else(unavailable)
    })?;
    let accepted = user.wallet.as_ref().ok_or_else(unavailable)?.balance;
    let balance = accepted.checked_sub(pending).ok_or_else(unavailable)?;
    let available = reservations::available_locked(pool, &inner, owner, accepted, false, None)?;
    Ok((balance, available))
}
