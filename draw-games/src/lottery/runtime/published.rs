//! Read projection of the committed FIFO prefix. No SQL or command waits.
use super::*;
use std::time::Instant;

const SCRATCH_USERS: usize = 256;
const IDLE: Duration = Duration::from_secs(300);

pub(super) struct Published {
    pub(super) data: Data,
    scratch: HashMap<Uuid, Instant>,
}

impl Published {
    pub(super) fn new(data: Data) -> Self {
        Self {
            data,
            scratch: HashMap::new(),
        }
    }

    pub(super) fn touch(&mut self, user: Uuid) -> bool {
        let Some(seen) = self.scratch.get_mut(&user) else {
            return false;
        };
        *seen = Instant::now();
        true
    }

    fn own_scratch(&self, user: Uuid) -> Vec<Uuid> {
        self.data
            .by_user
            .get(&(user, "scratch".into()))
            .cloned()
            .unwrap_or_default()
    }

    fn remove_cards(&mut self, user: Uuid, ids: &[Uuid]) {
        if ids.is_empty() {
            return;
        }
        let ids: std::collections::HashSet<_> = ids.iter().copied().collect();
        for id in &ids {
            if let Some(card) = self.data.tickets.remove(id) {
                self.data.bytes = self.data.bytes.saturating_sub(resident(&card));
            }
        }
        if let Some(own) = self.data.by_user.get_mut(&(user, "scratch".into())) {
            own.retain(|id| !ids.contains(id));
        }
        if let Some(period) = self.data.by_period.get_mut(&("scratch".into(), 0)) {
            period.retain(|id| !ids.contains(id));
        }
        if let Some(recent) = self.data.recent.get_mut("scratch") {
            recent.retain(|id| !ids.contains(id));
        }
    }

    fn remove_scratch(&mut self, user: Uuid) {
        self.remove_cards(user, &self.own_scratch(user));
        self.data.by_user.remove(&(user, "scratch".into()));
        if let Some(offer) = self.data.offers.remove(&user) {
            self.data.bytes = self.data.bytes.saturating_sub(resident(&offer));
        }
        self.scratch.remove(&user);
    }

    /// Caller holds the command gate and accepted-data lock, after draining
    /// previous scratch work and loading this user's SQL baseline. Other users'
    /// public views survive the command working set's scratch eviction.
    pub(super) fn load_scratch(
        &mut self,
        source: &Data,
        user: Uuid,
        queued: usize,
    ) -> Result<(), Error> {
        let cards: Vec<_> = source
            .by_user
            .get(&(user, "scratch".into()))
            .into_iter()
            .flatten()
            .filter_map(|id| source.tickets.get(id))
            .cloned()
            .collect();
        let offer = source.offers.get(&user).cloned();
        let incoming =
            cards.iter().map(resident).sum::<usize>() + offer.as_ref().map_or(0, resident);
        // Active users are not evicted. A cold caller can retry preparation if
        // every slot is active; a successful purchase never loses its view.
        let now = Instant::now();
        let expired: Vec<_> = self
            .scratch
            .iter()
            .filter(|(id, seen)| **id != user && now.duration_since(**seen) > IDLE)
            .map(|(id, _)| *id)
            .collect();
        for id in expired {
            self.remove_scratch(id);
        }
        let existing = self
            .own_scratch(user)
            .iter()
            .filter_map(|id| self.data.tickets.get(id))
            .map(resident)
            .sum::<usize>()
            + self.data.offers.get(&user).map_or(0, resident);
        if (!self.scratch.contains_key(&user) && self.scratch.len() >= SCRATCH_USERS)
            || self
                .data
                .bytes
                .saturating_sub(existing)
                .saturating_add(incoming)
                .saturating_add(queued)
                > MAX_BYTES
        {
            return Err(unavailable());
        }
        self.remove_scratch(user);
        for card in cards {
            self.data.insert(card);
        }
        if let Some(offer) = offer {
            self.data.bytes = self.data.bytes.saturating_add(resident(&offer));
            self.data.offers.insert(user, offer);
        }
        self.scratch.insert(user, now);
        Ok(())
    }

    pub(super) fn commit(&mut self, event: &Event, grants: &[(Uuid, crate::game_rewards::Grant)]) {
        let mut event = event.clone();
        for (id, grant) in grants {
            if let Some(card) = event.cards.iter_mut().find(|t| t.card.id == *id) {
                card.card.entry["reward_grant"] = json!(grant);
            }
        }
        // Replays belong to the command working set/SQL, not to read memory.
        event.receipt = None;
        self.data.apply(&event);
        self.data.durable = self.data.revision;
        if event.game == "scratch" {
            if let Some(user) = event.owner {
                self.scratch.insert(user, Instant::now());
                let ids = self.own_scratch(user);
                let latest = ids
                    .iter()
                    .filter_map(|id| self.data.tickets.get(id))
                    .max_by_key(|t| (t.card.bought_at, t.card.id))
                    .map(|t| t.card.purchase_id);
                let retired: Vec<_> = ids
                    .into_iter()
                    .filter(|id| {
                        self.data.tickets.get(id).is_some_and(|t| {
                            t.card.settled_at.is_some() && Some(t.card.purchase_id) != latest
                        })
                    })
                    .collect();
                self.remove_cards(user, &retired);
            }
        }
    }

    pub(super) fn remove_user(&mut self, user: Uuid) {
        forget(&mut self.data, user);
        self.scratch.remove(&user);
        self.data.presence.retain(|(_, u), _| *u != user);
        for winners in self.data.winners.values_mut() {
            for winner in winners.iter_mut().filter(|w| w.user == Some(user)) {
                winner.user = None;
                winner.name = crate::display_name::fallback_display_name(user);
            }
        }
        self.data.compact();
    }
}
