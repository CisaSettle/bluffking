//! Numeric liabilities share the bean authority's admission/publication mutex.
//! Wallet balances include poker stacks; only unencumbered beans may be spent.
use super::*;
use crate::runtime_checkpoint::RuntimeCheckpoint;
use chrono::{DateTime, Utc};

#[derive(Default)]
pub(super) struct Reservations {
    poker: HashMap<(Uuid, Uuid, DateTime<Utc>), (u64, i64)>,
    /// Hold'em beans a table has let go of in memory whose checkpoint write is
    /// still queued, by `(session, user)`: `(checkpoint revision, amount)`. A
    /// crash before that write commits restores them to the table, so
    /// [`reserved`] keeps counting them until [`poker_checkpoint_durable`].
    /// This is the one rule that makes a table operation depend on the
    /// database: the actor never waits, the released beans do. Amounts add
    /// up (cancel, re-stage, cancel counts both), which can over-reserve for
    /// the few milliseconds until the writer commits; it never under-reserves,
    /// also when a settlement moved the stack in between.
    releasing: HashMap<(Uuid, Uuid), Vec<(u64, i64)>>,
    /// Newest checkpoint revision accepted per hold'em session, so accepting
    /// the same checkpoint again counts no release twice.
    accepted: HashMap<Uuid, u64>,
    /// Open wagers of gold-bean dealer rooms (百家乐, 押大小), by room.
    dealer: HashMap<DealerKey, (u64, HashMap<Uuid, i64>)>,
    dealer_locks: HashMap<(DealerKey, Uuid), std::fs::File>,
    /// Each room's checkpoint seats as [`reserved`] reads them, parsed once
    /// per checkpoint revision. A room's checkpoint only ever advances to a
    /// higher revision (directory overlay and the guarded durable write), so
    /// one revision always names the same seats. Read under the bean mutex.
    seats: std::cell::RefCell<HashMap<Uuid, CheckpointSeats>>,
    /// Owner 2026-11-12 — squid charges posted past a wallet's free beans
    /// (「金豆不够 金豆就是负数」), by user. The reconcile tolerates this much
    /// over-reservation until the wallet backs its seats again.
    squid_unbacked: HashMap<Uuid, i64>,
}

/// A dealer room: `(game, code)`. Codes are drawn per game.
pub(super) type DealerKey = (String, String);

fn dealer_key(game: &str, code: &str) -> DealerKey {
    (game.to_owned(), code.to_owned())
}

/// Bound on cached rooms; past it the cache restarts empty.
const MAX_CACHED_CHECKPOINTS: usize = 16_384;

/// Occupied seats of one runtime checkpoint: `(user, joined_at)` to the seat's
/// stack plus pending chips (the first seat in seat order when repeated).
struct CheckpointSeats {
    revision: u64,
    seats: HashMap<(Uuid, DateTime<Utc>), i64>,
}

impl CheckpointSeats {
    fn parse(value: &Value) -> sqlx::Result<Self> {
        let checkpoint = serde_json::from_value::<RuntimeCheckpoint>(value.clone())
            .map_err(|e| sqlx::Error::Protocol(e.to_string()))?;
        let mut seats = HashMap::new();
        for seat in checkpoint.seats.values() {
            if let Some(owner) = &seat.occupant {
                seats.entry((owner.user_id, owner.joined_at)).or_insert(
                    seat.stack
                        .saturating_add(seat.pending_chips)
                        .min(i64::MAX as u64) as i64,
                );
            }
        }
        Ok(Self {
            revision: checkpoint.revision,
            seats,
        })
    }
}

impl Reservations {
    /// Accounts holding a hold'em or dealer-room reservation.
    pub(super) fn holders(&self) -> impl Iterator<Item = Uuid> + '_ {
        self.poker.keys().map(|(_, user, _)| *user).chain(
            self.dealer
                .values()
                .flat_map(|(_, bets)| bets.keys().copied()),
        )
    }
    /// `(revision, seat amount)` of `user`'s seat generation in `room`'s
    /// checkpoint, parsing the checkpoint only when its revision is new.
    fn checkpoint_seat(
        &self,
        room: Uuid,
        checkpoint: &Value,
        user: Uuid,
        joined_at: DateTime<Utc>,
    ) -> sqlx::Result<(u64, Option<i64>)> {
        let mut cache = self.seats.borrow_mut();
        let revision = checkpoint.get("revision").and_then(Value::as_u64);
        let cached = cache
            .get(&room)
            .is_some_and(|seats| Some(seats.revision) == revision);
        if !cached {
            let parsed = CheckpointSeats::parse(checkpoint)?;
            if cache.len() >= MAX_CACHED_CHECKPOINTS && !cache.contains_key(&room) {
                cache.clear();
            }
            cache.insert(room, parsed);
        }
        let seats = &cache[&room];
        Ok((seats.revision, seats.seats.get(&(user, joined_at)).copied()))
    }
    pub(super) fn remove_user(&mut self, user: Uuid) {
        self.poker.retain(|(_, owner, _), _| *owner != user);
        self.releasing.retain(|(_, owner), _| *owner != user);
        self.squid_unbacked.remove(&user);
    }
    /// The over-reservation a squid charge past free beans explains.
    pub(super) fn squid_unbacked(&self, user: Uuid) -> i64 {
        self.squid_unbacked.get(&user).copied().unwrap_or(0)
    }
    pub(super) fn squid_unbacked_users(&self) -> Vec<Uuid> {
        self.squid_unbacked.keys().copied().collect()
    }
    /// Shrinks the tolerance to the wallet's real deficit, dropping it at 0,
    /// so an old squid charge never masks a later over-reservation.
    pub(super) fn ratchet_squid_unbacked(&mut self, user: Uuid, deficit: i64) {
        if let Some(entry) = self.squid_unbacked.get_mut(&user) {
            *entry = (*entry).min(deficit.max(0));
            if *entry == 0 {
                self.squid_unbacked.remove(&user);
            }
        }
    }
    pub(super) fn note_squid_unbacked(&mut self, user: Uuid, amount: i64) {
        if amount > 0 {
            let entry = self.squid_unbacked.entry(user).or_default();
            *entry = entry.saturating_add(amount);
        }
    }
    /// Accept `checkpoint` as the table's seats. `published` is the room's
    /// previously accepted checkpoint and `deltas` the wallet deltas accepted
    /// with this one (a settlement moves a stack and its wallet together).
    /// Whatever a seat that stays lets go of beyond its wallet delta (a
    /// cancelled top-up) is a release: it stays reserved under this
    /// checkpoint's revision until that revision is durable. A seat that
    /// leaves is released at once, as before (`bean_ledger` "narrow residue").
    pub(super) fn checkpoint(
        &mut self,
        session: Uuid,
        checkpoint: &RuntimeCheckpoint,
        published: Option<&Value>,
        deltas: &[(Uuid, i64)],
    ) {
        let mut next: HashMap<(Uuid, DateTime<Utc>), i64> = HashMap::new();
        for seat in checkpoint.seats.values() {
            if let Some(owner) = &seat.occupant {
                next.entry((owner.user_id, owner.joined_at)).or_insert(
                    seat.stack
                        .saturating_add(seat.pending_chips)
                        .min(i64::MAX as u64) as i64,
                );
            }
        }
        // Every seat generation reserved until now: held entries, else the
        // amount [`reserved`] reads from the published checkpoint.
        let mut held: HashMap<(Uuid, DateTime<Utc>), (u64, i64)> = HashMap::new();
        let newest = self.accepted.entry(session).or_default();
        let first = *newest < checkpoint.revision;
        *newest = (*newest).max(checkpoint.revision);
        if let Some(Ok(seats)) = published.filter(|_| first).map(CheckpointSeats::parse) {
            for (key, amount) in seats.seats {
                held.insert(key, (seats.revision, amount));
            }
        }
        for ((room, user, joined), (revision, amount)) in &self.poker {
            if first
                && *room == session
                && held
                    .get(&(*user, *joined))
                    .is_none_or(|(shown, _)| revision >= shown)
            {
                held.insert((*user, *joined), (*revision, *amount));
            }
        }
        for ((user, joined), (revision, amount)) in held {
            // A reservation made for a later revision is not lowered here.
            if revision > checkpoint.revision {
                continue;
            }
            let delta = deltas
                .iter()
                .find(|(owner, _)| *owner == user)
                .map_or(0, |(_, delta)| *delta);
            let Some(kept) = next.get(&(user, joined)).copied() else {
                continue;
            };
            let released = amount.saturating_add(delta).saturating_sub(kept);
            if released > 0 {
                self.releasing
                    .entry((session, user))
                    .or_default()
                    .push((checkpoint.revision, released));
            }
        }
        self.poker.retain(|(room, user, joined), (revision, _)| {
            *room != session
                || *revision > checkpoint.revision
                || next.contains_key(&(*user, *joined))
        });
        for ((user, joined), amount) in next {
            let key = (session, user, joined);
            if self
                .poker
                .get(&key)
                .is_none_or(|(revision, _)| *revision <= checkpoint.revision)
            {
                self.poker.insert(key, (checkpoint.revision, amount));
            }
        }
    }

    pub(super) fn dealer_commit(&mut self, game: &str, code: &str, snapshot: &Value) {
        let key = dealer_key(game, code);
        let revision = snapshot["revision"].as_u64().unwrap_or(0);
        if self
            .dealer
            .get(&key)
            .is_some_and(|(latest, _)| *latest > revision)
        {
            return;
        }
        let bets = dealer_bets(game, snapshot);
        self.dealer_locks
            .retain(|(room, user), _| *room != key || bets.contains_key(user));
        if bets.is_empty() {
            self.dealer.remove(&key);
        } else {
            self.dealer.insert(key, (revision, bets));
        }
    }
}

/// Each bettor's open stake in a gold-bean dealer room's snapshot: 押大小's
/// one `stake`, 百家乐's three betting areas together. A points room or a
/// settled round holds nothing.
fn dealer_bets(game: &str, snapshot: &Value) -> HashMap<Uuid, i64> {
    if snapshot["wallet_unit"] != "beans" || !snapshot["result"].is_null() {
        return HashMap::new();
    }
    let stake = |bet: &Value| -> Option<i64> {
        match game {
            "sicbo" => bet["stake"].as_i64(),
            "baccarat" => ["player", "banker", "tie"]
                .iter()
                .try_fold(0i64, |sum, side| sum.checked_add(bet[*side].as_i64()?)),
            _ => None,
        }
    };
    snapshot["bets"]
        .as_object()
        .into_iter()
        .flatten()
        .filter_map(|(id, bet)| Some((id.parse().ok()?, stake(bet)?.max(0))))
        .collect()
}

/// Until the corresponding snapshot commits, retain the larger old/new wager.
/// A cancellation acknowledged in memory cannot release crash-recoverable debt.
pub fn dealer_accepted<T>(
    pool: &PgPool,
    game: &str,
    code: &str,
    snapshot: &Value,
    enqueue: impl FnOnce() -> Result<T, crate::casino_room::Error>,
) -> Result<T, crate::casino_room::Error> {
    let state = state(pool);
    let mut inner = state.inner.lock().unwrap_or_else(|e| e.into_inner());
    let key = dealer_key(game, code);
    let bets = dealer_bets(game, snapshot);
    let mut locks = Vec::new();
    for user in bets.keys() {
        if !inner
            .reservations
            .dealer_locks
            .contains_key(&(key.clone(), *user))
        {
            if let Some(lock) = crate::local_runtime::hold_dealer(*user)
                .map_err(|_| crate::casino_room::Error::Unavailable)?
            {
                locks.push(((key.clone(), *user), lock));
            }
        }
    }
    let result = enqueue()?;
    inner.reservations.dealer_locks.extend(locks);
    let entry = inner.reservations.dealer.entry(key).or_default();
    entry.0 = snapshot["revision"].as_u64().unwrap_or(entry.0);
    for (user, stake) in bets {
        let amount = entry.1.entry(user).or_default();
        *amount = (*amount).max(stake);
    }
    Ok(result)
}

pub fn dealer_committed(pool: &PgPool, game: &str, code: &str, snapshot: &Value) {
    state(pool)
        .inner
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .reservations
        .dealer_commit(game, code, snapshot);
}

pub fn sync_poker_checkpoint(pool: &PgPool, session: Uuid, checkpoint: &RuntimeCheckpoint) {
    let state = state(pool);
    let mut inner = state.inner.lock().unwrap_or_else(|e| e.into_inner());
    let Ok(directory) = crate::live_directory::memory_snapshot(pool) else {
        return;
    };
    if !directory
        .sessions
        .get(&session)
        .is_some_and(|room| is_beans(room) && room.ended_at.is_none())
    {
        return;
    }
    let published = directory
        .sessions
        .get(&session)
        .and_then(|room| room.runtime_checkpoint.as_ref());
    inner
        .reservations
        .checkpoint(session, checkpoint, published, &[]);
}

/// The table's ordered writer committed checkpoint `revision`: beans released
/// by it and by every earlier checkpoint of the same FIFO are free again.
/// A flush accepts its checkpoint before queuing the write, and a hand or
/// squid settlement records under the bean mutex before queuing, so no
/// release is recorded after its own commit was reported here. A departure
/// records after queuing; its checkpoint releases nothing for seats that stay.
pub fn poker_checkpoint_durable(pool: &PgPool, session: Uuid, revision: u64) {
    let state = state(pool);
    let mut inner = state.inner.lock().unwrap_or_else(|e| e.into_inner());
    inner.reservations.releasing.retain(|(room, _), released| {
        if *room == session {
            released.retain(|(accepted, _)| *accepted > revision);
        }
        !released.is_empty()
    });
}

/// The accepted checkpoint of an open gold-bean hold'em room.
pub(super) fn published_poker_checkpoint(pool: &PgPool, session: Uuid) -> Option<Option<Value>> {
    let directory = crate::live_directory::memory_snapshot(pool).ok()?;
    directory
        .sessions
        .get(&session)
        .filter(|room| is_beans(room) && room.ended_at.is_none())
        .map(|room| room.runtime_checkpoint.clone())
}

/// Called after directory closure and after dropping its publication lock.
pub(crate) fn release_poker_session(pool: &PgPool, session: Uuid) {
    let state = state(pool);
    let mut inner = state.inner.lock().unwrap_or_else(|e| e.into_inner());
    inner
        .reservations
        .poker
        .retain(|(room, _, _), _| *room != session);
    inner
        .reservations
        .releasing
        .retain(|(room, _), _| *room != session);
    inner.reservations.accepted.remove(&session);
    inner.reservations.seats.get_mut().remove(&session);
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
    excluding_dealer: Option<(&str, &str)>,
) -> sqlx::Result<i64> {
    if crate::local_runtime::dealer_held(user).map_err(sqlx::Error::Io)?
        && !inner
            .users
            .get(&user)
            .is_some_and(|u| !u.effects.is_empty())
        && !inner
            .reservations
            .dealer
            .values()
            .any(|(_, bets)| bets.contains_key(&user))
    {
        return Err(unavailable());
    }
    if inner
        .reservations
        .dealer
        .iter()
        .any(|((game, code), (_, bets))| {
            bets.contains_key(&user)
                && crate::local_runtime::owned_by_another_backend(
                    crate::release_handoff::rooms::id(game, code),
                )
        })
    {
        return Err(unavailable());
    }
    let (mut poker, fixed_departure_losses) = crate::beans::holds::amounts(user);
    let mut seats: HashMap<Uuid, i64> = HashMap::new();
    for (room, member) in directory.memberships_for(user) {
        if !is_beans(room) || room.ended_at.is_some() {
            continue;
        }
        let amount = if member.left_at.is_none() {
            let (revision, seat) = match &room.runtime_checkpoint {
                Some(value) => {
                    inner
                        .reservations
                        .checkpoint_seat(room.id, value, user, member.joined_at)?
                }
                None => (0, None),
            };
            // Admission publishes the restored stack before WS binding creates
            // its checkpoint seat. Reserve that full amount during this gap.
            let admitted_stack = member
                .standing_chips
                .as_ref()
                .map(|value| crate::beans::reentry_chips(Some(value)))
                .transpose()
                .map_err(|code| sqlx::Error::Protocol(code.into()))?
                .flatten()
                .map_or(i64::from(room.starting_stack), |saved| {
                    (saved.stack + saved.pending_chips) as i64
                });
            let base = seat.unwrap_or(admitted_stack);
            inner
                .reservations
                .poker
                .get(&(room.id, user, member.joined_at))
                .filter(|(rev, _)| *rev >= revision)
                .map_or(base, |(_, amount)| *amount)
        } else {
            // Departure releases the wallet reservation, but retains the
            // required reentry stack. Admission checks it against available
            // beans again. Unsettled hand holds are included in `poker` above.
            0
        };
        let entry = seats.entry(room.id).or_default();
        *entry = (*entry).max(amount);
    }
    // Beans a table released whose write has not committed: a crash returns
    // them to that table, so they stay reserved there until it does.
    for ((session, owner), released) in &inner.reservations.releasing {
        if *owner != user {
            continue;
        }
        let releasing = released
            .iter()
            .try_fold(0i64, |sum, (_, amount)| sum.checked_add(*amount))
            .ok_or_else(unavailable)?;
        let entry = seats.entry(*session).or_default();
        *entry = entry.checked_add(releasing).ok_or_else(unavailable)?;
    }
    for (session, amount) in seats {
        let entry = poker.entry(session).or_default();
        *entry = (*entry).max(amount);
    }
    let mut total = inner.cards.reserved(user, None)?;
    // Owed by departed occupancies, never part of the excluded seat's stack.
    total = total
        .checked_add(fixed_departure_losses)
        .ok_or_else(unavailable)?;
    for (session, amount) in poker {
        if Some(session) != excluding_poker {
            total = total.checked_add(amount).ok_or_else(unavailable)?;
        }
    }
    for ((game, code), (_, bets)) in &inner.reservations.dealer {
        if Some((game.as_str(), code.as_str())) != excluding_dealer {
            total = total
                .checked_add(bets.get(&user).copied().unwrap_or(0))
                .ok_or_else(unavailable)?;
        }
    }
    // The memory balance already includes accepted winnings whose settlement
    // has not committed. A crash would void them, so they may only back the
    // stack of the table that produced them: a card room keeps them in its own
    // reservation until its checkpoint commits (`accept_card_snapshot`), and a
    // hold'em table while the winner still holds the seat its accepted
    // checkpoint reserved. Otherwise (the winner left) they stay reserved here.
    if !sql_balance {
        if let Some(owner) = inner.users.get(&user) {
            for (hand, effect) in &owner.poker {
                if effect.delta <= 0 || inner.cards.carries(*hand) {
                    continue;
                }
                let seated = inner
                    .poker
                    .get(hand)
                    .and_then(|receipt| receipt.room)
                    .is_some_and(|session| {
                        directory.memberships_for(user).any(|(room, member)| {
                            room.id == session
                                && room.ended_at.is_none()
                                && member.left_at.is_none()
                                && inner.reservations.poker.contains_key(&(
                                    session,
                                    user,
                                    member.joined_at,
                                ))
                        })
                    });
                if !seated {
                    total = total.checked_add(effect.delta).ok_or_else(unavailable)?;
                }
            }
            // A lottery prize has no table: until its ledger row commits it
            // backs nothing (no purchase, seat, stake, gift or fee).
            for effect in owner.effects.values() {
                total = total
                    .checked_add(effect.balance()?.max(0))
                    .ok_or_else(unavailable)?;
            }
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
                .checked_add(effect.balance()?.saturating_neg().max(0))
                .ok_or_else(unavailable)?;
        }
        for delta in inner
            .users
            .get(&user)
            .into_iter()
            .flat_map(|u| u.poker.values())
        {
            total = total
                .checked_add(delta.delta.saturating_neg().max(0))
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
    excluding_dealer: Option<(&str, &str)>,
) -> sqlx::Result<i64> {
    let state = { state(pool) };
    let inner = { state.inner.lock().unwrap_or_else(|e| e.into_inner()) };
    available_locked(pool, &inner, user, balance, sql_balance, excluding_dealer)
}

pub(super) fn available_locked(
    pool: &PgPool,
    inner: &Inner,
    user: Uuid,
    balance: i64,
    sql_balance: bool,
    excluding_dealer: Option<(&str, &str)>,
) -> sqlx::Result<i64> {
    if !inner.ready {
        return Err(unavailable());
    }
    let directory = { crate::live_directory::memory_snapshot_for_user(pool, user)? };
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
            inner,
            &directory,
            user,
            sql_balance,
            None,
            excluding_dealer,
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
    if !inner.ready || inner.active_users.contains_key(&user) {
        return Err(unavailable());
    }
    let balance = inner
        .users
        .get(&user)
        .filter(|u| u.available)
        .and_then(|u| u.wallet.as_ref())
        .ok_or_else(unavailable)?
        .balance;
    let directory = crate::live_directory::memory_snapshot_for_user(pool, user)?;
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

/// Owner 2026-11-12 — a squid loser short of the penalty tops up the whole
/// shortfall from the wallet, which may go negative (「金豆不够 金豆就是负数」).
/// Grows this seat's whole-stack reservation by the part of `wanted` the
/// wallet holds beside every reservation (this seat's current one included),
/// so nothing else spends it before the hand commits, and returns that part.
pub fn reserve_poker_top_up(
    pool: &PgPool,
    session: Uuid,
    user: Uuid,
    wanted: u64,
) -> sqlx::Result<u64> {
    ensure(pool, user)?;
    let state = state(pool);
    let mut inner = state.inner.lock().unwrap_or_else(|e| e.into_inner());
    if !inner.ready || inner.active_users.contains_key(&user) {
        return Err(unavailable());
    }
    let balance = inner
        .users
        .get(&user)
        .filter(|u| u.available)
        .and_then(|u| u.wallet.as_ref())
        .ok_or_else(unavailable)?
        .balance;
    let directory = crate::live_directory::memory_snapshot_for_user(pool, user)?;
    let room = directory.sessions.get(&session).ok_or_else(unavailable)?;
    let member = room
        .members
        .iter()
        .find(|m| m.user_id == user && m.left_at.is_none())
        .ok_or_else(unavailable)?;
    let other = reserved(&inner, &directory, user, false, Some(session), None)?;
    let all = reserved(&inner, &directory, user, false, None, None)?;
    // The seat's own stack: `releasing` amounts are counted on their own.
    let releasing = inner
        .reservations
        .releasing
        .get(&(session, user))
        .map_or(0, |released| {
            released.iter().map(|(_, amount)| *amount).sum::<i64>()
        });
    let current = all.saturating_sub(other).saturating_sub(releasing).max(0);
    let free = balance.saturating_sub(all).max(0);
    let wanted = i64::try_from(wanted).unwrap_or(i64::MAX);
    let grant = wanted.min(free);
    inner.reservations.note_squid_unbacked(user, wanted - grant);
    if grant == 0 {
        return Ok(0);
    }
    let revision = room
        .runtime_checkpoint
        .as_ref()
        .and_then(|c| c["revision"].as_u64())
        .unwrap_or(0)
        .saturating_add(1);
    inner.reservations.poker.insert(
        (session, user, member.joined_at),
        (revision, current.saturating_add(grant)),
    );
    Ok(grant as u64)
}

/// Owner 2026-11-12 — a gold-bean squid leaver's extra buy-in is charged in
/// full from the wallet: the part past its free beans now (the wallet may go
/// negative). Record it with [`note_squid_unbacked`] once the leave commits.
pub fn squid_departure_unbacked(pool: &PgPool, user: Uuid, wanted: u64) -> sqlx::Result<i64> {
    let state = state(pool);
    let inner = state.inner.lock().unwrap_or_else(|e| e.into_inner());
    if !inner.ready {
        return Err(unavailable());
    }
    let balance = inner
        .users
        .get(&user)
        .and_then(|u| u.wallet.as_ref())
        .ok_or_else(unavailable)?
        .balance;
    let directory = crate::live_directory::memory_snapshot_for_user(pool, user)?;
    let all = reserved(&inner, &directory, user, false, None, None)?;
    let free = balance.saturating_sub(all).max(0);
    let wanted = i64::try_from(wanted).unwrap_or(i64::MAX);
    Ok(wanted.saturating_sub(free).max(0))
}

/// Tolerates `amount` of over-reservation for a committed squid charge
/// posted past the wallet's free beans (see [`squid_departure_unbacked`]).
pub fn note_squid_unbacked(pool: &PgPool, user: Uuid, amount: i64) {
    let state = state(pool);
    let mut inner = state.inner.lock().unwrap_or_else(|e| e.into_inner());
    inner.reservations.note_squid_unbacked(user, amount);
}
