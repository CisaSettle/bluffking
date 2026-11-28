use super::{cache, enqueue, ensure, evict, Family, Profile, Slot, CACHE_CAP, POOL};
use crate::bot::OpponentStats;
use std::time::Instant;
use uuid::Uuid;

/// Fold `user`'s stored reads into this seat's reads once they are loaded.
/// Returns whether the seat now owns `user`'s profile (and may be stored).
/// Reads owned by a different user are reset first: a new occupant never
/// inherits someone else's history.
pub fn bind(family: Family, user: Uuid, stats: &mut OpponentStats) -> bool {
    if stats.profile_owner == Some(user) {
        return true;
    }
    if stats.profile_owner.is_some() {
        *stats = OpponentStats::default();
    }
    let now = Instant::now();
    let mut cache = cache();
    ensure(&mut cache, user, family, now);
    let Some(Slot::Ready(Profile::Reads(stored), touched)) = cache.get_mut(&(user, family)) else {
        return false;
    };
    *touched = now;
    *stats = stats.after(stored);
    stats.profile_owner = Some(user);
    true
}

/// Remember an owned seat's reads (after [`bind`] returned true) for the next
/// table: refresh the cache now, write behind.
pub fn store(family: Family, user: Uuid, stats: &OpponentStats) {
    if stats.profile_owner != Some(user) || POOL.get().is_none() {
        return;
    }
    let profile = Profile::Reads(Box::new(stats.stored()));
    let now = Instant::now();
    {
        let mut cache = cache();
        if cache.len() >= CACHE_CAP {
            evict(&mut cache, now);
        }
        cache.insert((user, family), Slot::Ready(profile.clone(), now));
    }
    enqueue(user, family, profile);
}
