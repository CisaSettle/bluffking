//! Bounded per-room ordered write-behind. Accepted actions and settlement
//! events are retained in FIFO order until their database transaction commits.
//! A process crash can lose snapshots not yet committed. This is not a WAL.
use std::{
    future::Future,
    sync::{
        atomic::{AtomicU64, Ordering},
        Arc, Mutex, OnceLock, Weak,
    },
    time::Duration,
};

use futures::FutureExt;
use tokio::sync::watch;

static QUIESCING: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);
pub fn begin_shutdown() {
    QUIESCING.store(true, Ordering::Release);
}
pub(crate) fn accepting() -> bool {
    !QUIESCING.load(Ordering::Acquire)
}

// Retain membership until its leave snapshot commits. This prevents another
// room from overtaking a pending leave while the database still owns the seat.
fn memberships() -> &'static Mutex<std::collections::HashMap<(&'static str, uuid::Uuid), String>> {
    static MEMBERS: OnceLock<Mutex<std::collections::HashMap<(&'static str, uuid::Uuid), String>>> =
        OnceLock::new();
    MEMBERS.get_or_init(Default::default)
}
pub(crate) fn reserved_room(game: &'static str, user: uuid::Uuid) -> Option<String> {
    memberships().lock().unwrap().get(&(game, user)).cloned()
}
/// Everyone a room of `game` holds a reservation for.
pub(crate) fn reserved_members(game: &'static str, code: &str) -> Vec<uuid::Uuid> {
    memberships()
        .lock()
        .unwrap()
        .iter()
        .filter(|((g, _), c)| *g == game && c.as_str() == code)
        .map(|((_, user), _)| *user)
        .collect()
}
pub(crate) fn reserve_members(
    game: &'static str,
    code: &str,
    users: &[uuid::Uuid],
) -> Result<Vec<uuid::Uuid>, String> {
    let mut members = memberships().lock().unwrap();
    if users
        .iter()
        .any(|u| members.get(&(game, *u)).is_some_and(|c| c != code))
    {
        return Err("in_other_room".into());
    }
    let added = users
        .iter()
        .copied()
        .filter(|u| !members.contains_key(&(game, *u)))
        .collect::<Vec<_>>();
    for user in &added {
        members.insert((game, *user), code.to_owned());
    }
    Ok(added)
}
/// Frees every reservation a room holds, including users only named by
/// snapshots a fenced writer abandoned.
pub(crate) fn release_room(game: &'static str, code: &str) {
    memberships()
        .lock()
        .unwrap()
        .retain(|(g, _), c| *g != game || c != code);
}
pub(crate) fn release_members(game: &'static str, code: &str, users: &[uuid::Uuid]) {
    let mut members = memberships().lock().unwrap();
    for user in users {
        if members.get(&(game, *user)).is_some_and(|c| c == code) {
            members.remove(&(game, *user));
        }
    }
}

#[derive(Default)]
struct DrainState {
    queued: AtomicU64,
    completed: AtomicU64,
}

fn writers() -> &'static Mutex<Vec<Weak<DrainState>>> {
    static WRITERS: OnceLock<Mutex<Vec<Weak<DrainState>>>> = OnceLock::new();
    WRITERS.get_or_init(|| Mutex::new(Vec::new()))
}

/// Called after HTTP/WS connections drain. Flush every snapshot already accepted
/// before shutdown, including workers whose actor has just dropped its sender.
pub async fn drain_all(timeout: Duration) -> bool {
    let targets: Vec<_> = writers()
        .lock()
        .unwrap()
        .iter()
        .filter_map(Weak::upgrade)
        .map(|state| {
            let sequence = state.queued.load(Ordering::Acquire);
            (state, sequence)
        })
        .collect();
    tokio::time::timeout(timeout, async {
        loop {
            let mut complete = true;
            for (state, sequence) in &targets {
                if state.completed.load(Ordering::Acquire) < *sequence {
                    complete = false;
                }
            }
            if complete {
                return;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .is_ok()
}

/// Marks a persist error that no retry can fix: another owner has written the
/// row. The worker stops and abandons its queue instead of retrying forever.
const FENCED: &str = "fenced: ";
pub(crate) fn fenced(error: &str) -> String {
    format!("{FENCED}{error}")
}

#[derive(Clone)]
struct Progress<T> {
    sequence: u64,
    durable: T,
    error: Option<String>,
}

pub struct Ordered<T> {
    pending: tokio::sync::mpsc::Sender<(u64, T)>,
    progress: watch::Receiver<Progress<T>>,
    drain_state: Arc<DrainState>,
    sequence: u64,
}

/// Folds a queued item into the one about to be written, or hands it back when
/// it must be written on its own.
type Merge<T> = Box<dyn FnMut(&mut T, T) -> Option<T> + Send>;

impl<T: Clone + Send + Sync + 'static> Ordered<T> {
    pub fn new<F, Fut>(initial: T, persist: F) -> Self
    where
        F: FnMut(T, T) -> Fut + Send + 'static,
        Fut: Future<Output = Result<T, String>> + Send,
    {
        Self::spawn(initial, None, persist)
    }

    /// Like [`Self::new`], but items already queued behind the head are folded
    /// into it with `merge` before each write, so a slow database writes the
    /// newest state once instead of every intermediate one. Completion still
    /// reports the last folded sequence, so `drain`/`durable` keep their meaning.
    pub fn new_coalescing<M, F, Fut>(initial: T, merge: M, persist: F) -> Self
    where
        M: FnMut(&mut T, T) -> Option<T> + Send + 'static,
        F: FnMut(T, T) -> Fut + Send + 'static,
        Fut: Future<Output = Result<T, String>> + Send,
    {
        Self::spawn(initial, Some(Box::new(merge)), persist)
    }

    fn spawn<F, Fut>(initial: T, mut merge: Option<Merge<T>>, mut persist: F) -> Self
    where
        F: FnMut(T, T) -> Fut + Send + 'static,
        Fut: Future<Output = Result<T, String>> + Send,
    {
        let (pending, mut rx) = tokio::sync::mpsc::channel::<(u64, T)>(64);
        let (done, progress) = watch::channel(Progress {
            sequence: 0,
            durable: initial.clone(),
            error: None,
        });
        let drain_state = Arc::new(DrainState::default());
        {
            let mut registry = writers().lock().unwrap();
            registry.retain(|e| e.strong_count() > 0);
            registry.push(Arc::downgrade(&drain_state));
        }
        let worker = drain_state.clone();
        tokio::spawn(async move {
            let mut durable = initial;
            let mut carry: Option<(u64, T)> = None;
            loop {
                let (mut sequence, mut next) = match carry.take() {
                    Some(item) => item,
                    None => match rx.recv().await {
                        Some(item) => item,
                        None => break,
                    },
                };
                if let Some(merge) = merge.as_mut() {
                    while let Ok((queued, item)) = rx.try_recv() {
                        match merge(&mut next, item) {
                            None => sequence = queued,
                            Some(item) => {
                                carry = Some((queued, item));
                                break;
                            }
                        }
                    }
                }
                let mut attempt = 0u32;
                loop {
                    let result = std::panic::AssertUnwindSafe(async {
                        persist(durable.clone(), next.clone()).await
                    })
                    .catch_unwind()
                    .await
                    .unwrap_or_else(|_| Err("room persistence worker panicked".into()));
                    match result {
                        Ok(saved) => {
                            durable = saved;
                            worker.completed.store(sequence, Ordering::Release);
                            done.send_replace(Progress {
                                sequence,
                                durable: durable.clone(),
                                error: None,
                            });
                            break;
                        }
                        Err(error) if error.starts_with(FENCED) => {
                            // Nothing queued here can become durable. Report the
                            // abandoned tail as settled so shutdown does not wait.
                            tracing::warn!(%error, sequence, "ordered_room_writer_fenced");
                            worker.completed.store(u64::MAX, Ordering::Release);
                            done.send_modify(|p| p.error = Some(error));
                            return;
                        }
                        Err(error) => {
                            attempt = attempt.saturating_add(1);
                            if attempt.is_power_of_two() {
                                tracing::error!(%error,attempt,sequence,"ordered_room_write_retry");
                            }
                            done.send_modify(|p| p.error = Some(error.clone()));
                            tokio::time::sleep(Duration::from_millis(
                                (25u64 << attempt.min(5)).min(1000),
                            ))
                            .await;
                        }
                    }
                }
            }
        });
        Self {
            pending,
            progress,
            drain_state,
            sequence: 0,
        }
    }
    /// The worker stopped for good (fenced); nothing enqueued will be written.
    pub fn is_closed(&self) -> bool {
        self.pending.is_closed()
    }
    /// Admission check before changing actor state. A failed database pauses new
    /// commands while the retained head item retries; it never drops the queue.
    pub fn health(&self) -> Result<(), String> {
        if self.pending.is_closed() {
            return Err("room_writer_closed".into());
        }
        if self.pending.capacity() == 0 {
            return Err("room_writer_full".into());
        }
        if self.progress.borrow().error.is_some() {
            return Err("persistence_unavailable".into());
        }
        Ok(())
    }
    /// Number of snapshots accepted by this writer, including its durable prefix.
    pub fn accepted_count(&self) -> u64 {
        self.sequence
    }

    pub fn enqueue(&mut self, next: T) -> Result<(), String> {
        let sequence = self.sequence + 1;
        self.pending
            .try_send((sequence, next))
            .map_err(|_| "room_writer_full_or_closed".to_string())?;
        self.sequence = sequence;
        self.drain_state.queued.store(sequence, Ordering::Release);
        Ok(())
    }
    /// Resolves once everything enqueued so far is durable. Unlike [`drain`],
    /// it borrows nothing, so the caller can release its room lock first.
    ///
    /// [`drain`]: Self::drain
    pub fn durable(&self) -> impl Future<Output = Result<(), String>> + Send + 'static
    where
        T: Send + Sync,
    {
        let target = self.sequence;
        let mut progress = self.progress.clone();
        async move {
            loop {
                if progress.borrow().sequence >= target {
                    return Ok(());
                }
                progress
                    .changed()
                    .await
                    .map_err(|_| "room_writer_closed".to_string())?;
            }
        }
    }
    pub async fn drain(&mut self) -> Result<T, String> {
        loop {
            if self.progress.borrow().sequence >= self.sequence {
                return Ok(self.progress.borrow().durable.clone());
            }
            self.progress
                .changed()
                .await
                .map_err(|_| "room_writer_closed".to_string())?;
        }
    }
}
