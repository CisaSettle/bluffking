//! `POST /api/tools/poker/solve` — PUBLIC, no-login REAL postflop GTO solver.
//!
//! This is the heaviest of the free poker-tools: it runs a genuine per-spot
//! Discounted-CFR equilibrium solve over the unabstracted 2-player postflop game
//! tree (the open-source `postflop-solver` crate, wrapped by `gto-solver` behind
//! engine types per ADR-012). Unlike the equity / outs / pot-odds / analyze
//! tools (Monte-Carlo or heuristic), this computes an actual approximate Nash
//! equilibrium with a MEASURED exploitability we report back.
//!
//! ## HONESTY (the #1 rule)
//! The result is labeled `method: "cfr_equilibrium"` and the response carries
//! the bet sizes used, the iteration cap, and the ACHIEVED exploitability (both
//! absolute and as a % of pot). It is GTO *for the inputs given* — the
//! equilibrium is only as meaningful as the assumed ranges + the (deliberately
//! limited, disclosed) bet-size set + the stopping point. The client badge MUST
//! disclose all three; it must NEVER show a bare "GTO". This is categorically
//! distinct from the engine's honestly-labeled `equity_heuristic` /
//! `preflop_chart` — those are NOT equilibrium solves; this one is.
//!
//! ## COMPLIANCE — zero playable-hand surface
//! Pure analysis of a described spot: no chips, no game state, no dealing-and-
//! resolving a hand the user plays, no persistence, no win/loss loop. Equity is
//! an abstract pot-share %; EV is in pot units (the tree's chip scale). NO money
//! / wager / cash / chips-won wording.
//!
//! ## Auth + rate limit
//! PUBLIC (no `AuthUser`). Per-IP rate-limited at the router layer via
//! `RateLimitKind::PostflopSolve` (15/window — far TIGHTER than the 120/window
//! `PokerTool` MC tools, because each solve is hundreds of MB → ~1.5 GB + seconds
//! of CPU).
//!
//! ## DoS / OOM defenses (layered)
//! 1. **Pre-allocation memory gate** (`gto-solver` `SolveLimits::max_memory_bytes`
//!    = 1.5 GB): the solve is REJECTED with an honest 400 before allocating if the
//!    estimated tree exceeds the cap. Memory is dominated by range-width ×
//!    bet-size-count, so the public tier caps the FLOP to ONE bet size.
//! 2. **Hard wall-clock timeout** (`SOLVE_TIMEOUT`): bounds CALLER LATENCY — the
//!    handler returns `solve_timeout` (503) promptly. NOTE (F3): upstream
//!    `solve()` is a synchronous, non-cancellable loop, so the `spawn_blocking`
//!    task keeps running and HOLDS its concurrency permit until it finishes
//!    naturally; the timeout does NOT free solver capacity. The solve still
//!    terminates (bounded by the iteration cap + memory gate), so this is
//!    finite, but slow in-budget solves can keep permits busy and make new
//!    callers see `solver_busy` until they complete. A cancellable /
//!    out-of-process worker would be the structural fix (deferred).
//! 3. **Global concurrency semaphore** (`SOLVE_PERMITS`): at most N solves run at
//!    once box-wide, so N simultaneous flop solves can't stack 1.5 GB × N and OOM
//!    the shared container.
//! 4. **Iteration cap + exploitability target** in `SolveLimits`.
//! 5. **Solved-spot cache**: a SipHash of the normalized request → the result, so
//!    repeated canonical spots are served from memory (the GB solve is a one-time
//!    cost).
//! 6. **Per-IP rate limit** (above) bounds request volume from one source.
//!
//! ## Errors
//! Invalid input (bad cards / board / ranges / sizing / pot) → 400 in the
//! poker-tools `{"error":..,"reason":..}` dialect. A solve too large for the
//! memory cap → 400 `solve_too_large` (honest: "this spot is too big for the free
//! tier"). A timeout → 503 `solve_timeout`. A join/internal error → 500.

use std::collections::hash_map::DefaultHasher;
use std::hash::Hasher;
use std::net::{IpAddr, SocketAddr};
use std::sync::{Arc, OnceLock};
use std::time::Duration;

use axum::{
    extract::{rejection::JsonRejection, ConnectInfo},
    http::StatusCode,
    response::IntoResponse,
    Json,
};
use dashmap::DashMap;
use serde::{Deserialize, Serialize};
use tokio::sync::{OwnedSemaphorePermit, Semaphore};

use engine::card::{parse_card, Card};
use engine::hand::HoleCards;
use gto_solver::{
    hero_strategy, solve_spot, HandStrategy, Player, SolveError, SolveLimits, SolveOutput,
    SolveRequest, SolveStreet,
};

/// Hard wall-clock timeout for one solve. The design measured a 1-bet-size flop
/// at single-digit seconds; 12 s leaves headroom for a busy box while bounding a
/// pathological case (the iteration cap + memory gate already bound the common
/// path).
const SOLVE_TIMEOUT: Duration = Duration::from_secs(12);

/// Process-global timeout override in MILLISECONDS (F5 test hook). `0` (the
/// default) means "use the `SOLVE_TIMEOUT` const". An integration test can set a
/// tiny value via [`test_set_solve_timeout_ms`] so a real solve deterministically
/// trips the wall-clock-timeout branch (`solve_timeout` 503) without waiting the
/// full 12 s. F6 (codex LOW): this mutable global + its setter exist ONLY behind
/// `cfg(any(test, feature = "test-support"))`, so a default/release `cargo build`
/// of the binary carries NEITHER the override state NOR a way to set it — the
/// handler reads it only through [`effective_solve_timeout`], which is the const
/// in a non-test build.
#[cfg(any(test, feature = "test-support"))]
static SOLVE_TIMEOUT_OVERRIDE_MS: std::sync::atomic::AtomicU64 =
    std::sync::atomic::AtomicU64::new(0);

/// The wall-clock timeout the handler actually applies. In a test / `test-support`
/// build it honours the [`SOLVE_TIMEOUT_OVERRIDE_MS`] override when non-zero (F5
/// tests); in a default/release build (no override symbol exists) it is always the
/// `SOLVE_TIMEOUT` const (F6).
#[cfg(any(test, feature = "test-support"))]
fn effective_solve_timeout() -> Duration {
    let ms = SOLVE_TIMEOUT_OVERRIDE_MS.load(std::sync::atomic::Ordering::Relaxed);
    if ms == 0 {
        SOLVE_TIMEOUT
    } else {
        Duration::from_millis(ms)
    }
}

/// Release/default build: no override exists, so the wall-clock timeout is always
/// the `SOLVE_TIMEOUT` const (F6 — the mutable override is test-support-only).
#[cfg(not(any(test, feature = "test-support")))]
fn effective_solve_timeout() -> Duration {
    SOLVE_TIMEOUT
}

/// Set the per-solve wall-clock timeout override in ms (F5 test hook); `0`
/// restores the `SOLVE_TIMEOUT` const. F6: gated behind
/// `cfg(any(test, feature = "test-support"))` so it does NOT compile into the
/// release binary; the integration tests enable the `test-support` feature (a
/// separate-crate integration test can't reach a `#[cfg(test)]`-only item).
#[cfg(any(test, feature = "test-support"))]
pub fn test_set_solve_timeout_ms(ms: u64) {
    SOLVE_TIMEOUT_OVERRIDE_MS.store(ms, std::sync::atomic::Ordering::Relaxed);
}

/// Box-wide max concurrent solves. Each flop solve can hold ~1.5 GB, so a small
/// number prevents N concurrent flops from stacking memory and OOM-killing the
/// shared H5+backend container. Turn/river solves are tiny but share the gate
/// (simplicity > a per-street pool).
const DEFAULT_MAX_CONCURRENT_SOLVES: usize = 2;

/// Box-wide concurrency cap for postflop solves (cached first-read). Default
/// [`DEFAULT_MAX_CONCURRENT_SOLVES`] (2); override at runtime with
/// `SOLVER_MAX_CONCURRENT` (parsed, clamped ≥1). Each solve pre-allocates up to
/// [`PUBLIC_MAX_MEMORY_BYTES`] (1 GB), so 2 concurrent ≈ 2 GB peak — too much for
/// the 2c/4GB prod host, which sets `SOLVER_MAX_CONCURRENT=1` so at most one 1 GB
/// solve runs at a time. Cached so the value can't drift mid-process.
fn max_concurrent_solves() -> usize {
    static N: OnceLock<usize> = OnceLock::new();
    *N.get_or_init(|| {
        std::env::var("SOLVER_MAX_CONCURRENT")
            .ok()
            .and_then(|s| s.trim().parse::<usize>().ok())
            .map(|n| n.max(1))
            .unwrap_or(DEFAULT_MAX_CONCURRENT_SOLVES)
    })
}

/// Max in-flight solves a SINGLE source IP may hold at once (F1). The global
/// `MAX_CONCURRENT_SOLVES` semaphore caps box-wide concurrency, but without a
/// per-IP guard ONE IP could grab BOTH global permits — and because the solve is
/// non-cancellable (F3), a wall-clock timeout does NOT free them. A single source
/// could then keep both global permits busy back-to-back and make the public
/// solver return `solver_busy` to everyone else, all within its 15/window rate
/// bucket. Capping each IP to ONE concurrent solve means a second IP always has a
/// free global permit. Set strictly BELOW `MAX_CONCURRENT_SOLVES` so the per-IP
/// guard can never alone monopolize global capacity.
const MAX_INFLIGHT_PER_IP: usize = 1;

/// The PUBLIC-tier memory cap. 1.0 GB — below the default 1.5 GB so a single
/// flop with one bet size fits comfortably while two bet sizes (7 GB+) are
/// rejected. Conservative for a shared box.
const PUBLIC_MAX_MEMORY_BYTES: u64 = 1_000_000_000;

/// Iteration cap (safety net — flop converges to <1% pot in <100 iters).
const PUBLIC_MAX_ITERATIONS: u32 = 800;

/// Stop once exploitability reaches this fraction of the pot (0.5%).
const PUBLIC_TARGET_EXPLOIT_PCT: f32 = 0.005;

/// Global concurrency semaphore (lazy). `Arc` so a clone can be moved into the
/// async permit-acquire without borrowing a `'static` reference awkwardly.
fn solve_semaphore() -> Arc<Semaphore> {
    static SEM: OnceLock<Arc<Semaphore>> = OnceLock::new();
    SEM.get_or_init(|| Arc::new(Semaphore::new(max_concurrent_solves())))
        .clone()
}

/// Per-IP in-flight registry (F1): `IpAddr → Semaphore(MAX_INFLIGHT_PER_IP)`. A
/// solve from a given source IP must hold one of THAT IP's permits for its whole
/// duration, in ADDITION to a global permit. This bounds how much of the scarce
/// (memory-gated, non-cancellable) global pool any single source can occupy, so
/// one IP can never sit on every global permit and deny the public solver to
/// everyone else. DashMap = concurrent, no global lock; entries persist (one tiny
/// `Arc<Semaphore>` per distinct IP that ever solved — the working set is small,
/// and stale entries are reclaimed lazily, see `acquire_ip_inflight`).
fn ip_inflight_registry() -> &'static DashMap<IpAddr, Arc<Semaphore>> {
    static REG: OnceLock<DashMap<IpAddr, Arc<Semaphore>>> = OnceLock::new();
    REG.get_or_init(DashMap::new)
}

/// RAII guard for a held per-IP in-flight permit. Dropping it releases the permit
/// AND, when it was the last holder, removes the IP's now-idle registry entry so
/// the map cannot grow unbounded across many distinct source IPs. The
/// `OwnedSemaphorePermit` is kept alive for the guard's lifetime (drop order:
/// `_permit` drops first, freeing the slot, then `cleanup` runs).
struct IpInflightGuard {
    ip: IpAddr,
    // The owned permit keeps the IP's semaphore alive for the guard's lifetime;
    // it is dropped (releasing the slot) AFTER the `Drop::drop` body below runs.
    _permit: OwnedSemaphorePermit,
}

impl Drop for IpInflightGuard {
    fn drop(&mut self) {
        // Field drop order: the `Drop::drop` BODY runs first, THEN the struct's
        // fields (`_permit`, `sem`) drop. So inside this body our own permit is
        // still held → `available_permits()` is 0 while we are the sole holder.
        // `+1` accounts for the permit we are about to return. When the slot will
        // be fully idle after we return it, reclaim the registry entry so the map
        // can't grow unbounded across many distinct source IPs. `remove_if`
        // re-checks the predicate under the shard lock, serialized against
        // `acquire_ip_inflight`'s `entry()`, so we never evict an entry another
        // solve is actively using (worst case a racing acquirer transiently
        // re-creates the entry — harmless: that IP simply gets a fresh slot).
        let reg = ip_inflight_registry();
        reg.remove_if(&self.ip, |_, sem| {
            sem.available_permits() + 1 >= MAX_INFLIGHT_PER_IP
        });
    }
}

/// Try to reserve one in-flight slot for `ip` (F1). Returns `Some(guard)` when
/// granted; `None` when this IP already has `MAX_INFLIGHT_PER_IP` solves in
/// flight (→ the handler returns `solver_busy`, same as a full global pool). The
/// guard releases the slot (and reclaims the entry when idle) on drop.
fn acquire_ip_inflight(ip: IpAddr) -> Option<IpInflightGuard> {
    let reg = ip_inflight_registry();
    let sem = reg
        .entry(ip)
        .or_insert_with(|| Arc::new(Semaphore::new(MAX_INFLIGHT_PER_IP)))
        .clone();
    match sem.try_acquire_owned() {
        Ok(permit) => Some(IpInflightGuard {
            ip,
            _permit: permit,
        }),
        Err(_) => None,
    }
}

// ---------------------------------------------------------------------------
// Test-only capacity hooks (F5) — F6: gated out of release builds
// ---------------------------------------------------------------------------
// The `solver_busy` 503 (global-pool-full and per-IP-full) and the permit-hold
// semantics are otherwise only reachable by racing two real GB-scale solves,
// which is flaky and slow. These hooks let an integration test DETERMINISTICALLY
// drain the SAME process-global guards the handler uses, so it can assert the 503
// paths against the live handler without timing races. They expose no new behavior
// — only the ability to hold the existing guards.
//
// F6 (codex LOW): the whole block is gated behind
// `cfg(any(test, feature = "test-support"))`, so a default/release `cargo build`
// of the binary contains NONE of these `test_*` symbols. The integration tests
// (a separate crate that can't reach `#[cfg(test)]`-only items) enable the
// crate's `test-support` feature via the dev-dependency self-reference in
// `Cargo.toml`. `#[doc(hidden)]` additionally keeps them out of docs.

/// Drain the global solve semaphore to ZERO available permits and return them
/// held in a guard (F5 test hook). While the returned guard is alive the handler's
/// `try_acquire_owned()` on the global semaphore fails, so a fresh solve request
/// hits the global-pool-full `solver_busy` 503 path. Acquires whatever is
/// currently available (a leaked-but-still-running prior test solve may hold
/// some), so the post-condition is "0 available", which is all the 503 path needs.
/// Dropping the guard frees the acquired permits.
#[cfg(any(test, feature = "test-support"))]
#[doc(hidden)]
pub fn test_hold_all_global_permits() -> impl Send {
    let sem = solve_semaphore();
    let mut held = Vec::with_capacity(max_concurrent_solves());
    // Drain every currently-available permit; loop until none remain so the
    // handler is guaranteed to see an empty pool regardless of any in-flight task.
    while let Ok(p) = sem.clone().try_acquire_owned() {
        held.push(p);
    }
    held
}

/// Reserve the per-IP in-flight slot for `ip` and return the guard (F5 test
/// hook). While held, a solve request from the SAME `ip` hits the per-IP-full
/// `solver_busy` 503 path. Returns `None` if the slot is already taken.
#[cfg(any(test, feature = "test-support"))]
#[doc(hidden)]
pub fn test_hold_ip_inflight(ip: IpAddr) -> Option<impl Send> {
    acquire_ip_inflight(ip)
}

/// ASYNC drain of the global solve semaphore (F5 test hook): acquire ALL
/// `MAX_CONCURRENT_SOLVES` permits, AWAITING (not failing) if a solve is still
/// in flight, so it blocks until every running solve has returned its permit.
/// The timeout test uses this to wait out its leaked (non-cancellable) solve
/// before releasing the serialization lock, so a following capacity test never
/// inherits a busy permit. Dropping the returned guard frees the permits.
#[cfg(any(test, feature = "test-support"))]
#[doc(hidden)]
pub async fn test_drain_global_permits() -> impl Send {
    let sem = solve_semaphore();
    let mut held = Vec::with_capacity(max_concurrent_solves());
    for _ in 0..max_concurrent_solves() {
        // `acquire_owned` awaits until a permit is free — this is what makes the
        // drain BLOCK on any in-flight solve rather than skip it.
        if let Ok(p) = sem.clone().acquire_owned().await {
            held.push(p);
        }
    }
    held
}

/// A cached SOLVED SPOT — the HERO-INDEPENDENT equilibrium (F3). Keyed by
/// [`spot_key`] (NOT [`response_key`]), so two requests that differ ONLY in the
/// hero hand share ONE cached solve: the expensive equilibrium is computed once,
/// and each hero's specific response is derived CHEAPLY from `hands` via
/// [`hero_strategy`]. Caching under `response_key` (the old behavior) folded the
/// hero into the key, so changing only the hero forced a full GB-scale re-solve —
/// the cache then defeated its own DoS mitigation. The stored `response` is the
/// hero-independent body (`hero == None`); `hands` is the solving player's full
/// per-hand strategy table the hero row is selected from.
#[derive(Clone)]
struct CachedSpot {
    /// The hero-INDEPENDENT response body (everything but the hero row). Cloned
    /// per request and the hero row is filled in cheaply on the way out.
    response: SolveResponse,
    /// The solving player's full per-hand strategy table — the hero-independent
    /// source any specific hero's [`HandStrategyDto`] is derived from.
    hands: Vec<HandStrategy>,
    /// The estimated heap+inline footprint of THIS entry in bytes (F7), computed
    /// once at insert via [`cached_spot_bytes`]. Stored so eviction can decrement
    /// the global byte accountant by the exact amount this entry contributed —
    /// without re-estimating a value that has since been removed from the map.
    bytes: usize,
}

/// Estimated in-memory footprint of a `Vec<f32>` action-frequency row: the heap
/// buffer (`len × 4`) plus the `Vec`'s own 24-byte (ptr+len+cap) inline header.
fn freq_vec_bytes(freqs: &[f32]) -> usize {
    std::mem::size_of_val(freqs) + std::mem::size_of::<Vec<f32>>()
}

/// Estimate the bytes a [`CachedSpot`] occupies (F7). The dominant term is
/// `hands: Vec<HandStrategy>` — for a wide early-street spot this is the full
/// per-hand table (hundreds to ~1000+ hands, each with a small per-action
/// frequency vector), so a single entry can be ~135 KB. We sum the real heap
/// content (each hand's `hand` string + its `frequencies` buffer) plus the inline
/// struct sizes, then add the (small, bounded) hero-independent response body. The
/// estimate is deliberately a slight OVER-count (we charge full `Vec`/`String`
/// headers per element) so the byte budget errs on the side of holding LESS, never
/// more, than the cap. Used by the byte-budgeted cache so total memory is bounded
/// by BYTES, not by a coarse entry count.
fn cached_spot_bytes(spot: &CachedSpot) -> usize {
    let mut total = std::mem::size_of::<CachedSpot>();
    // The per-hand strategy table — the heavy part.
    total += spot.hands.capacity() * std::mem::size_of::<HandStrategy>();
    for h in &spot.hands {
        total += h.hand.capacity();
        total += freq_vec_bytes(&h.frequencies);
    }
    // The hero-independent response body: only its variable-length parts matter
    // (the fixed scalar fields are already in `size_of::<CachedSpot>()` above).
    let r = &spot.response;
    total += r.method.capacity()
        + r.solving_player.capacity()
        + r.bet_sizes.capacity()
        + r.raise_sizes.capacity()
        + r.source_url.as_ref().map_or(0, |s| s.capacity());
    total += r.actions.capacity() * std::mem::size_of::<ActionFreqDto>();
    for a in &r.actions {
        total += a.action.capacity();
    }
    // The cached body never carries a hero row (it is hero-independent), so no
    // `r.hero` term — it is always `None` here.
    total
}

/// Solved-spot cache: hero-INDEPENDENT [`spot_key`] → [`CachedSpot`]. Bounded by
/// BYTES via [`MAX_CACHE_BYTES`] (F7), NOT by a coarse entry count: a single wide
/// early-street `CachedSpot` holds the full per-hand strategy table (~135 KB), so
/// an entry-count cap of 2048 would have allowed ~275 MB worst-case — the comment
/// claimed "a few MB". The byte budget + FIFO eviction (see [`cache_insert`])
/// keeps total tracked memory under the cap regardless of per-entry size.
/// F3: keyed by `spot_key` so a hero-only change reuses the solved equilibrium.
/// DashMap = concurrent, no global lock.
fn solve_cache() -> &'static DashMap<u64, CachedSpot> {
    static CACHE: OnceLock<DashMap<u64, CachedSpot>> = OnceLock::new();
    CACHE.get_or_init(DashMap::new)
}

/// FIFO insertion-order queue of cache keys (F7), used to pick the eviction
/// victim. A `Mutex<VecDeque>` serializes the small insert/evict bookkeeping; the
/// expensive cache READS still go lock-free through the `DashMap`. (Simple FIFO,
/// not true LRU — the canonical public working set is small and repeats, so the
/// eviction policy rarely bites; FIFO bounds memory without per-read recency
/// tracking.)
fn cache_order() -> &'static std::sync::Mutex<std::collections::VecDeque<u64>> {
    static ORDER: OnceLock<std::sync::Mutex<std::collections::VecDeque<u64>>> = OnceLock::new();
    ORDER.get_or_init(|| std::sync::Mutex::new(std::collections::VecDeque::new()))
}

/// Running total of estimated cache bytes (F7). Kept in lock-step with the map by
/// [`cache_insert`] (which holds `cache_order()`'s mutex while it inserts/evicts),
/// so it is the single source of truth for "are we over budget".
fn cache_bytes() -> &'static std::sync::atomic::AtomicUsize {
    static BYTES: OnceLock<std::sync::atomic::AtomicUsize> = OnceLock::new();
    BYTES.get_or_init(|| std::sync::atomic::AtomicUsize::new(0))
}

/// Byte budget for the solved-spot cache (F7). 96 MB — generous enough for a
/// healthy hit-rate on the canonical public working set (hundreds of distinct
/// ~135 KB wide-spot entries, or many more small turn/river entries) while bounding
/// the cache's contribution to the shared H5+backend container's RSS far below the
/// old entry-count bound's ~275 MB worst case. Memory is bounded by BYTES, not by
/// entry count.
const MAX_CACHE_BYTES: usize = 96 * 1024 * 1024;

/// Insert a solved spot into the byte-budgeted cache, evicting oldest entries
/// (FIFO) until the running total stays at or below [`MAX_CACHE_BYTES`] (F7).
///
/// The `cache_order()` mutex serializes all insert/evict bookkeeping so the
/// `DashMap`, the FIFO order queue and the `cache_bytes()` accountant move
/// together — a concurrent insert can't race the byte total past the cap. A single
/// entry larger than the whole budget is simply not cached (it would force an
/// empty cache); that never happens for a public-tier solve, whose per-hand table
/// is far under 96 MB. Returns the entry's estimated byte cost (for tests/metrics).
fn cache_insert(key: u64, spot: CachedSpot) -> usize {
    let bytes = cached_spot_bytes(&spot);
    let cache = solve_cache();
    let order = cache_order();
    let accountant = cache_bytes();

    // Hold the order lock for the whole insert+evict so the byte total can't be
    // raced past the budget by a concurrent insert.
    let mut q = match order.lock() {
        Ok(g) => g,
        // A poisoned mutex means a prior holder panicked mid-bookkeeping; recover
        // the guard and proceed (the accountant may be slightly off, but we never
        // want a poisoned lock to disable the cache or panic the handler).
        Err(poisoned) => poisoned.into_inner(),
    };

    // An entry that alone exceeds the whole budget is not worth caching (it would
    // evict everything and still be over). Skip it; the solve still returns.
    if bytes > MAX_CACHE_BYTES {
        return bytes;
    }

    // If this key already has an entry (a benign re-solve race), drop the old one
    // from the accountant first so we don't double-count.
    if let Some((_, old)) = cache.remove(&key) {
        accountant.fetch_sub(old.bytes, std::sync::atomic::Ordering::Relaxed);
        q.retain(|k| *k != key);
    }

    // Evict oldest (FIFO) until the NEW entry fits under the budget.
    while accountant.load(std::sync::atomic::Ordering::Relaxed) + bytes > MAX_CACHE_BYTES {
        match q.pop_front() {
            Some(victim) => {
                if let Some((_, removed)) = cache.remove(&victim) {
                    accountant.fetch_sub(removed.bytes, std::sync::atomic::Ordering::Relaxed);
                }
            }
            // Queue empty but still over budget — only possible if the accountant
            // drifted; nothing left to evict, so stop (the `bytes <= budget` guard
            // above guarantees the single new entry fits once the map is empty).
            None => break,
        }
    }

    let mut spot = spot;
    spot.bytes = bytes;
    cache.insert(key, spot);
    q.push_back(key);
    accountant.fetch_add(bytes, std::sync::atomic::Ordering::Relaxed);
    bytes
}

/// The AGPL §13 Corresponding-Source URL for the deployed solver version (F4).
/// Set once by `poker_tools::routes()` when (and only when) it mounts the route —
/// the route does not mount without a valid `POSTFLOP_SOLVER_SOURCE_URL`, so when
/// the solve handler runs this is always populated. Echoed in every solve
/// response + served by `solve_source_handler` so the §13 offer structurally
/// reaches the network user.
fn source_url_cell() -> &'static OnceLock<String> {
    static URL: OnceLock<String> = OnceLock::new();
    &URL
}

/// Record the §13 source URL (F4). Idempotent: `OnceLock` ignores a second set,
/// which is fine — the route is mounted once at boot with a fixed URL.
pub fn set_source_url(url: String) {
    let _ = source_url_cell().set(url);
}

/// The configured §13 source URL, if the route was mounted with one.
fn source_url() -> Option<String> {
    source_url_cell().get().cloned()
}

/// The configured §13 source URL, exposed for the rate-limit layer (F2). The
/// `PostflopSolve` 429 is produced by `RateLimitLayer` BEFORE `solve_handler`
/// runs, so the layer cannot reuse `error_body`; it reads the offer through this
/// accessor instead, keeping the single `OnceLock` as the source of truth.
pub fn configured_source_url() -> Option<String> {
    source_url()
}

/// Max characters in EITHER range string (F2). The upstream range grammar parses
/// comma-separated tokens; a near-body-limit (2 MB) comma-heavy string would burn
/// worker CPU in `validate_request` (parsed SYNCHRONOUSLY on the async worker,
/// BEFORE the solve permit) without ever consuming a solve permit — an asymmetry
/// with the bet/raise sizing 64-char cap that IS already present. A real range
/// (e.g. `"22+,A2s+,K9s+,QTs+,JTs,A8o+,KJo+"`) is well under this; the cap rejects
/// absurd input outright and keeps the parse cheap. 512 chars comfortably fits the
/// widest hand-written range while bounding the attack surface.
const MAX_RANGE_LEN: usize = 512;

/// Max comma-separated tokens in EITHER range string (F2). A pathological
/// `","`-only body could pack ~256 empty tokens under `MAX_RANGE_LEN`; bound the
/// token count too so the parser's per-token work is bounded regardless of the
/// raw byte length. The 169 distinct starting hands plus suit/rank-run shorthands
/// never need more than this many tokens in practice.
const MAX_RANGE_TOKENS: usize = 128;

// ===========================================================================
// Wire DTOs
// ===========================================================================

/// `POST /api/tools/poker/solve` request body.
#[derive(Debug, Clone, Deserialize)]
pub struct SolveRequestDto {
    /// `"flop" | "turn" | "river"`.
    pub street: String,
    /// Community cards: 3 (flop) / 4 (turn) / 5 (river), e.g. `["Td","9d","6h"]`.
    pub board: Vec<String>,
    /// OOP range string (upstream grammar), e.g. `"66+,A8s+,AJo+"`.
    pub oop_range: String,
    /// IP range string (upstream grammar).
    pub ip_range: String,
    /// Starting pot in chips (> 0). EV/exploitability use this scale.
    pub starting_pot: i32,
    /// Effective stack in chips (> 0).
    pub effective_stack: i32,
    /// `"oop" | "ip"` — which player's strategy to view (the player to act).
    pub solving_player: String,
    /// Bet sizes (upstream grammar). The PUBLIC tier accepts only ONE flop size;
    /// see validation. Defaults to `"50%"` when omitted.
    #[serde(default)]
    pub bet_sizes: Option<String>,
    /// Raise sizes (upstream grammar). Defaults to `"2.5x"` when omitted.
    #[serde(default)]
    pub raise_sizes: Option<String>,
    /// Optional hero hand (2 cards) whose specific strategy to extract.
    #[serde(default)]
    pub hero: Option<Vec<String>>,
}

/// One action + its equilibrium frequency, on the wire.
#[derive(Debug, Clone, Serialize, PartialEq)]
pub struct ActionFreqDto {
    pub action: String,
    pub amount: Option<i32>,
    pub frequency: f32,
}

/// Hero's per-hand strategy, on the wire.
#[derive(Debug, Clone, Serialize, PartialEq)]
pub struct HandStrategyDto {
    pub hand: String,
    pub frequencies: Vec<f32>,
    pub equity: f32,
    pub ev: f32,
}

/// `POST /api/tools/poker/solve` response body. Carries the full honesty
/// disclosure (method + bet sizes + iterations + achieved exploitability).
#[derive(Debug, Clone, Serialize, PartialEq)]
pub struct SolveResponse {
    /// Honesty badge — always `"cfr_equilibrium"`. The client MUST render this as
    /// a qualified label (with the disclosures below), never a bare "GTO".
    pub method: String,
    /// Which player the strategy view is for: `"oop" | "ip"`.
    pub solving_player: String,
    /// Achieved exploitability in the tree's chip scale (ACHIEVED, not a fixed
    /// guarantee — CFR-with-rayon is not bit-reproducible across thread counts).
    pub exploitability: f32,
    /// Achieved exploitability as a % of the starting pot (the disclosable
    /// "how close to equilibrium" figure).
    pub exploitability_pct_of_pot: f32,
    /// Available root actions + the solving player's range-average equilibrium
    /// frequency for each.
    pub actions: Vec<ActionFreqDto>,
    /// Range-average equity of the solving player at the root, 0..1.
    pub range_equity: f32,
    /// Range-average EV of the solving player at the root, in chips (pot scale).
    pub range_ev: f32,
    /// Hero's specific hand strategy, when a hero hand in the player's range was
    /// supplied.
    pub hero: Option<HandStrategyDto>,
    /// Bet sizes the tree used (echoed for the honesty badge).
    pub bet_sizes: String,
    /// Raise sizes the tree used (echoed for the honesty badge).
    pub raise_sizes: String,
    /// Iteration cap the solve ran under (echoed for the honesty badge).
    pub max_iterations: u32,
    /// `true` when this response was served from the solved-spot cache.
    pub cached: bool,
    /// AGPL §13 Corresponding-Source link for the exact deployed solver version
    /// (F4). The route only mounts with a configured source URL, so this is
    /// always present when the endpoint is reachable; the UI MUST render it as a
    /// visible "Source code (AGPL-3.0)" link (the network-user offer).
    pub source_url: Option<String>,
}

// ===========================================================================
// Helpers
// ===========================================================================

/// Build the JSON body for an error response, ALWAYS carrying the AGPL §13
/// `source_url` offer (N1). The §13 Corresponding-Source offer must reach the
/// network user even when they only ever hit errors (or are a direct API
/// consumer that never gets a successful `SolveResponse`) — so EVERY handler-
/// emitted response (400 / 503 / 500) echoes the configured source link, not
/// just the success body (`to_response_no_hero`). `source_url()` is `Some`
/// whenever the route is mounted
/// (it does not mount without a configured URL), so this is populated in
/// practice; serialized as `null` only in a focused unit test that bypasses the
/// router mount.
fn error_body(code: &str, reason: Option<&str>) -> serde_json::Value {
    let mut body = serde_json::json!({ "error": code, "source_url": source_url() });
    if let Some(r) = reason {
        body["reason"] = serde_json::Value::String(r.to_string());
    }
    body
}

/// An error response at `status` carrying the code, optional reason, and the
/// §13 source offer (N1).
fn error_response(
    status: StatusCode,
    code: &str,
    reason: Option<&str>,
) -> axum::response::Response {
    (status, Json(error_body(code, reason))).into_response()
}

/// Shared 400 helper: `{"error": <code>, "reason": <detail>, "source_url": ..}`
/// (the poker-tools error dialect — same as the sibling tools so the client maps
/// it uniformly — plus the always-present AGPL §13 source offer, N1).
fn bad_request(code: &str, reason: impl Into<String>) -> axum::response::Response {
    error_response(StatusCode::BAD_REQUEST, code, Some(&reason.into()))
}

/// Parsed board: `(flop, turn, river)`. `turn`/`river` are `None` on earlier
/// streets. The error is the poker-tools `(code, reason)` pair.
type ParsedBoard = ([Card; 3], Option<Card>, Option<Card>);

/// Parse + validate a board string list into `(flop, turn, river)` Cards for the
/// given street. Enforces the count↔street invariant + card uniqueness.
fn parse_board(street: SolveStreet, board: &[String]) -> Result<ParsedBoard, (String, String)> {
    let want = match street {
        SolveStreet::Flop => 3,
        SolveStreet::Turn => 4,
        SolveStreet::River => 5,
    };
    if board.len() != want {
        return Err((
            "invalid_board".into(),
            format!(
                "{} solve needs exactly {want} board cards, got {}",
                street_str(street),
                board.len()
            ),
        ));
    }
    let mut cards = Vec::with_capacity(board.len());
    for s in board {
        match parse_card(s) {
            Some(c) => cards.push(c),
            // N4 (codex LOW): CONSTANT reason — do not reflect the raw card token
            // back to the caller. The UI localizes by error CODE.
            None => {
                return Err((
                    "invalid_card".into(),
                    "a board card is not a valid card (use e.g. \"Td\", \"9d\", \"6h\")".into(),
                ))
            }
        }
    }
    // Uniqueness across the whole board.
    for (i, a) in cards.iter().enumerate() {
        for b in &cards[i + 1..] {
            if a == b {
                return Err((
                    "duplicate_card".into(),
                    "a board card appears more than once".into(),
                ));
            }
        }
    }
    let flop = [cards[0], cards[1], cards[2]];
    let turn = cards.get(3).copied();
    let river = cards.get(4).copied();
    Ok((flop, turn, river))
}

fn street_str(s: SolveStreet) -> &'static str {
    match s {
        SolveStreet::Flop => "flop",
        SolveStreet::Turn => "turn",
        SolveStreet::River => "river",
    }
}

/// Count the bet-size buckets in a `"33%,75%"`-style string (comma-separated,
/// ignoring empty tokens). Used to enforce the public-tier FLOP = 1 bet size cap.
fn count_bet_sizes(s: &str) -> usize {
    s.split(',').filter(|t| !t.trim().is_empty()).count()
}

/// Count comma-separated tokens in a range string, INCLUDING empty ones (F2).
/// Unlike `count_bet_sizes` we count empties too, because the DoS vector is the
/// raw number of split segments the parser walks — a `",,,,…"` body of empty
/// tokens still costs per-token work even though each is blank.
fn count_range_tokens(s: &str) -> usize {
    s.split(',').count()
}

/// Canonicalize a range string to a normal form so EQUIVALENT spots collapse to
/// ONE cache key (F3): case-fold, strip ALL whitespace, drop empty tokens, then
/// SORT the comma-separated tokens. The CFR equilibrium is invariant under token
/// reordering and whitespace/case, so `"AA,KK"`, `" kk , aa "`, and `"KK,AA"` are
/// the SAME spot and must hit the same cached solve instead of re-running the
/// GB-scale solve. (This is a cache-efficiency canonicalization, NOT range
/// validation — the upstream grammar still validates the original string.)
fn canonical_range(range: &str) -> String {
    let mut toks: Vec<String> = range
        .split(',')
        .map(|t| {
            t.split_whitespace()
                .collect::<String>()
                .to_ascii_lowercase()
        })
        .filter(|t| !t.is_empty())
        .collect();
    toks.sort();
    toks.join(",")
}

/// The HERO-INDEPENDENT equilibrium key (F3). The CFR equilibrium is a function
/// of (street, board, both canonicalized ranges, stakes, bet/raise sizing,
/// solving player) ONLY — the optional `hero` hand merely SELECTS which hand's
/// strategy to read OUT of the already-solved equilibrium; it does not change the
/// equilibrium itself. Splitting this out from [`response_key`] documents that
/// invariant and gives reordered/whitespace range variants a single key.
fn spot_key(req: &SolveRequest) -> u64 {
    let mut h = DefaultHasher::new();
    match req.street {
        SolveStreet::Flop => h.write_u8(0),
        SolveStreet::Turn => h.write_u8(1),
        SolveStreet::River => h.write_u8(2),
    }
    for c in &req.flop {
        h.write(c.to_string().to_ascii_uppercase().as_bytes());
        h.write_u8(0);
    }
    if let Some(t) = req.turn {
        h.write(t.to_string().to_ascii_uppercase().as_bytes());
    }
    h.write_u8(0);
    if let Some(r) = req.river {
        h.write(r.to_string().to_ascii_uppercase().as_bytes());
    }
    h.write_u8(0);
    // Ranges: canonical (sorted, whitespace-stripped, case-folded tokens) so
    // reordered/whitespace/case variants of the SAME range collapse to one key.
    h.write(canonical_range(&req.oop_range).as_bytes());
    h.write_u8(0);
    h.write(canonical_range(&req.ip_range).as_bytes());
    h.write_u8(0);
    h.write_i32(req.starting_pot);
    h.write_i32(req.effective_stack);
    h.write(
        req.bet_sizes
            .to_ascii_lowercase()
            .replace(' ', "")
            .as_bytes(),
    );
    h.write_u8(0);
    h.write(
        req.raise_sizes
            .to_ascii_lowercase()
            .replace(' ', "")
            .as_bytes(),
    );
    h.write_u8(0);
    match req.solving_player {
        Player::Oop => h.write_u8(0),
        Player::Ip => h.write_u8(1),
    }
    h.finish()
}

/// A hero-FOLDING cache key, retained only to document/verify the hero
/// distinction (F3). The handler no longer caches by this: it caches the
/// hero-INDEPENDENT [`spot_key`] and derives the hero row cheaply, so a hero-only
/// change reuses the solved equilibrium. The tests still assert that folding in
/// the hero would key responses apart (and that `spot_key` does NOT), proving the
/// hero is the ONLY thing the per-request response adds over the cached spot.
/// `#[cfg(test)]` because nothing in the production path keys by it anymore.
#[cfg(test)]
fn response_key(req: &SolveRequest) -> u64 {
    let mut h = DefaultHasher::new();
    h.write_u64(spot_key(req));
    if let Some(hero) = req.hero {
        h.write_u8(1);
        // Order-independent: hash the sorted token pair.
        let mut toks = [hero.card1.to_string(), hero.card2.to_string()];
        toks.sort();
        for t in &toks {
            h.write(t.to_ascii_uppercase().as_bytes());
            h.write_u8(0);
        }
    } else {
        h.write_u8(0xFF);
    }
    h.finish()
}

fn player_str(p: Player) -> &'static str {
    match p {
        Player::Oop => "oop",
        Player::Ip => "ip",
    }
}

/// The source IP for the per-IP in-flight guard (F1). Uses the TCP peer from
/// axum's `ConnectInfo`. We deliberately key on the DIRECT peer (not
/// `X-Forwarded-For`): the per-IP guard is a self-DoS / capacity-fairness control
/// layered UNDER the `PostflopSolve` rate limit (which already does the
/// proxy-aware client-IP extraction). Behind a trusted proxy every request shares
/// the edge peer, which only makes this guard STRICTER (more conservative), never
/// looser, so it cannot be bypassed by forging headers. A missing `ConnectInfo`
/// (focused unit test without `into_make_service_with_connect_info`) falls back to
/// a loopback sentinel so those tests share one bucket — acceptable, since the
/// real per-IP behavior is covered by the integration tests + the unit tests for
/// the guard itself.
fn client_ip(connect_info: Option<&ConnectInfo<SocketAddr>>) -> IpAddr {
    connect_info
        .map(|ci| ci.0.ip())
        .unwrap_or(IpAddr::V4(std::net::Ipv4Addr::LOCALHOST))
}

/// Convert a `gto_solver::HandStrategy` to the wire `HandStrategyDto`.
fn hand_strategy_to_dto(h: HandStrategy) -> HandStrategyDto {
    HandStrategyDto {
        hand: h.hand,
        frequencies: h.frequencies,
        equity: h.equity,
        ev: h.ev,
    }
}

/// Project a `SolveOutput` (+ the echoed params) onto a HERO-INDEPENDENT wire
/// `SolveResponse` (F3): the hero row is left `None` here and is attached
/// separately so the cached body can be reused for any hero. `cached` is set by
/// the caller (always `false` at solve time).
fn to_response_no_hero(
    out: &SolveOutput,
    bet_sizes: &str,
    raise_sizes: &str,
    cached: bool,
) -> SolveResponse {
    SolveResponse {
        method: "cfr_equilibrium".into(),
        solving_player: player_str(out.solving_player).into(),
        exploitability: out.exploitability,
        exploitability_pct_of_pot: out.exploitability_pct_of_pot,
        actions: out
            .actions
            .iter()
            .map(|a| ActionFreqDto {
                action: a.action.clone(),
                amount: a.amount,
                frequency: a.frequency,
            })
            .collect(),
        range_equity: out.range_equity,
        range_ev: out.range_ev,
        // Hero is attached by the caller (derived from `out.hands` per request).
        hero: None,
        bet_sizes: bet_sizes.into(),
        raise_sizes: raise_sizes.into(),
        // The wire field is `max_iterations` — it carries the iteration CAP the
        // solve ran under (F5: the upstream solver never returns the actual
        // count), which the crate now names `iteration_cap` to match.
        max_iterations: out.cost.iteration_cap,
        cached,
        // F4: the §13 source link is echoed in every response so the UI can
        // render the network-user "Source code (AGPL-3.0)" offer.
        source_url: source_url(),
    }
}

/// Build the per-request `SolveResponse` from a [`CachedSpot`] and the optional
/// hero hand (F3). The hero-independent body is cloned from the cache and the
/// hero row (if any) is derived CHEAPLY from the cached per-hand table via
/// [`hero_strategy`] — no re-solve. `cached` flags whether this was a cache hit.
fn response_from_cached_spot(
    spot: &CachedSpot,
    hero: Option<HoleCards>,
    cached: bool,
) -> SolveResponse {
    let mut resp = spot.response.clone();
    resp.cached = cached;
    resp.hero = hero
        .and_then(|h| hero_strategy(&spot.hands, h))
        .map(hand_strategy_to_dto);
    resp
}

/// Map a `gto_solver::SolveError` to the right HTTP response. All variants are
/// caller-input errors except `Internal` (500).
///
/// F4 (codex MED — reflected attacker input): the `reason` strings here are
/// CONSTANTS. The crate's `InvalidRange { detail }` / `InvalidBetSize(_)`
/// variants embed the UPSTREAM parser message, which in turn interpolates the
/// caller's raw range / bet-size token (e.g. upstream `"invalid range: '<raw>'"`).
/// Reflecting that `detail`/`m` verbatim into a public response is an XSS / token-
/// echo vector (the same class codex r2 F1 proved on the sibling `analyze`
/// handler). So we NEVER interpolate any upstream detail or request input into the
/// public `reason` — the UI localizes by error CODE, so a constant advisory
/// reason is sufficient. The `InvalidRange.player` field is itself a CONSTANT
/// (`"oop"`/`"ip"`, set by the crate, never caller-derived), so it is safe to keep
/// for which-range disambiguation; only `detail` is dropped.
fn map_solve_error(e: SolveError) -> axum::response::Response {
    match e {
        // Board errors carry only crate-internal, non-reflected messages, but we
        // keep them CONSTANT here too for uniformity with the reflected-input
        // defense (the caller's raw card token is already rejected with a constant
        // reason in `parse_board`).
        SolveError::InvalidBoard(_) => {
            bad_request("invalid_board", "the board is not a valid set of cards")
        }
        SolveError::StreetMismatch(_) => bad_request(
            "invalid_board",
            "the board does not match the requested street",
        ),
        // F4: NEVER echo the upstream `detail` (it interpolates the raw range
        // token). `player` is a crate-set constant, safe to disambiguate which
        // range failed; the reason is otherwise constant.
        SolveError::InvalidRange { player, .. } => {
            bad_request("invalid_range", format!("{player} range is invalid"))
        }
        // F4: NEVER echo the upstream bet-size parse message (it interpolates the
        // raw sizing token). Constant reason; the UI localizes by code.
        SolveError::InvalidBetSize(_) => {
            bad_request("invalid_bet_size", "bet/raise sizing is invalid")
        }
        SolveError::InvalidStakes(m) => bad_request("invalid_stakes", m),
        // F1: the requested view player is not the one to act at the root. The
        // handler ALSO guards this up front (see `solving_player` validation),
        // so this is a defensive backstop; constant reason (no token echo).
        SolveError::PlayerNotAtRoot { .. } => bad_request(
            "invalid_player",
            "this view is only available for the player to act at the root",
        ),
        // Honest: this spot is too big for the free tier's memory budget.
        SolveError::TooLarge { estimated, cap } => bad_request(
            "solve_too_large",
            format!(
                "this spot is too large for the free solver (estimated {} MB, limit {} MB) — \
                 try a narrower range or a single bet size",
                estimated / 1_000_000,
                cap / 1_000_000
            ),
        ),
        SolveError::Internal(m) => {
            tracing::error!("gto_solve internal solver error: {m}");
            // N1: the §13 source offer rides on the error too (no reason — never
            // leak the internal solver message).
            error_response(StatusCode::INTERNAL_SERVER_ERROR, "internal_error", None)
        }
    }
}

// ===========================================================================
// Handler
// ===========================================================================

/// `POST /api/tools/poker/solve` — run a real per-spot CFR equilibrium.
///
/// PUBLIC, no-login. Validates everything, checks the solved-spot cache, then
/// (on a miss) acquires a global concurrency permit and runs the solve inside
/// `spawn_blocking` under a hard wall-clock timeout. The pre-allocation memory
/// gate inside `gto-solver` rejects oversized spots before allocating.
pub async fn solve_handler(
    // F1: the source IP gates the per-IP in-flight cap. Optional so a router that
    // mounts the handler WITHOUT `into_make_service_with_connect_info`
    // (e.g. a focused unit test) still works — a missing peer falls back to a
    // shared loopback sentinel (see `client_ip`). Production wires ConnectInfo via
    // `axum::serve(.., app.into_make_service_with_connect_info::<SocketAddr>())`.
    connect_info: Option<ConnectInfo<SocketAddr>>,
    body: Result<Json<SolveRequestDto>, JsonRejection>,
) -> impl IntoResponse {
    // Malformed JSON → 400 in the poker-tools dialect (constant reason, never
    // echo attacker input — same reflected-input defense as the analyze tool).
    let Json(dto) = match body {
        Ok(json) => json,
        Err(_rejection) => return bad_request("invalid_input", "malformed request body"),
    };

    // --- street ---
    let street = match dto.street.to_ascii_lowercase().as_str() {
        "flop" => SolveStreet::Flop,
        "turn" => SolveStreet::Turn,
        "river" => SolveStreet::River,
        // N4 (codex LOW): CONSTANT reason — never reflect the raw `street` token
        // back to the caller. JSON-only + the UI localizes by error CODE, so the
        // reason is advisory; keeping it constant matches the sibling
        // `analyze_handler` (codex r2 F1 proved `<script>` reflection there).
        _ => {
            return bad_request(
                "invalid_street",
                "street must be 'flop', 'turn', or 'river'",
            )
        }
    };

    // --- board ---
    let (flop, turn, river) = match parse_board(street, &dto.board) {
        Ok(b) => b,
        Err((code, reason)) => return bad_request(&code, reason),
    };

    // --- solving player ---
    let solving_player = match dto.solving_player.to_ascii_lowercase().as_str() {
        "oop" => Player::Oop,
        "ip" => Player::Ip,
        // N4 (codex LOW): CONSTANT reason — do not echo the raw token.
        _ => return bad_request("invalid_player", "solving_player must be 'oop' or 'ip'"),
    };

    // --- hero (optional, exactly 2 cards) ---
    let hero = match &dto.hero {
        None => None,
        Some(cards) => {
            if cards.len() != 2 {
                return bad_request(
                    "invalid_hero",
                    format!("hero must be exactly 2 cards, got {}", cards.len()),
                );
            }
            // N4 (codex LOW): CONSTANT reason — do not reflect the raw hero card
            // token. The UI localizes by error CODE; this matches the sibling
            // analyze handler's reflected-input defense.
            let c0 = match parse_card(&cards[0]) {
                Some(c) => c,
                None => return bad_request("invalid_card", "a hero card is not a valid card"),
            };
            let c1 = match parse_card(&cards[1]) {
                Some(c) => c,
                None => return bad_request("invalid_card", "a hero card is not a valid card"),
            };
            if c0 == c1 {
                return bad_request("duplicate_card", "hero's two cards are identical");
            }
            // F11 (codex LOW): a hero card that duplicates a board card is a
            // physically-impossible request. Without this check it degrades
            // silently to "hero not found" (hero:null) because the blocked combo
            // is absent from the solver's private cards. Reject it as the clean
            // 400 it is. `board_cards()` collects flop+turn+river.
            for board_card in [Some(flop[0]), Some(flop[1]), Some(flop[2]), turn, river]
                .into_iter()
                .flatten()
            {
                if c0 == board_card || c1 == board_card {
                    return bad_request("duplicate_card", "a hero card also appears on the board");
                }
            }
            Some(HoleCards::new(c0, c1))
        }
    };

    // --- bet/raise sizing (PUBLIC-tier cap: FLOP = exactly ONE bet size) ---
    let bet_sizes = dto.bet_sizes.clone().unwrap_or_else(|| "50%".to_string());
    let raise_sizes = dto
        .raise_sizes
        .clone()
        .unwrap_or_else(|| "2.5x".to_string());
    if bet_sizes.trim().is_empty() {
        return bad_request("invalid_bet_size", "bet_sizes must not be empty");
    }
    // F2 (codex HIGH): bound the raw token-string length BEFORE building the
    // tree. `BetSizeOptions::try_from` splits BOTH bet_sizes and raise_sizes on
    // commas, so a pathological `"2.5x,3x,4x,..."` would expand the raise
    // branches and cost CPU building a huge `ActionTree` before the memory gate
    // fires. A short length cap makes the count check below cheap and rejects
    // absurd input outright (a legitimate request is a handful of chars).
    const MAX_SIZE_TOKENS_LEN: usize = 64;
    if bet_sizes.len() > MAX_SIZE_TOKENS_LEN || raise_sizes.len() > MAX_SIZE_TOKENS_LEN {
        return bad_request(
            "invalid_bet_size",
            "bet/raise sizing string is too long — use a single simple size (e.g. \"50%\" / \"2.5x\")",
        );
    }
    // The OOM blow-up is range-width × (bet-size-count × raise-size-count). The
    // flop tree is the expensive one, so the free tier hard-caps the flop to a
    // single bet/raise size. Turn/river trees are tiny, so up to 2 is allowed
    // there. F2: raise_sizes is now counted+capped the SAME way as bet_sizes —
    // previously only bet_sizes was guarded, so a wide `raise_sizes` could still
    // expand the tree (an inconsistency + a residual CPU-DoS dimension).
    let max_sizes = match street {
        SolveStreet::Flop => 1,
        SolveStreet::Turn | SolveStreet::River => 2,
    };
    if count_bet_sizes(&bet_sizes) > max_sizes {
        return bad_request(
            "too_many_bet_sizes",
            format!(
                "the free solver allows at most {max_sizes} bet size(s) on the {} — \
                 a wider tree exceeds the memory budget",
                street_str(street)
            ),
        );
    }
    if count_bet_sizes(&raise_sizes) > max_sizes {
        return bad_request(
            "too_many_raise_sizes",
            format!(
                "the free solver allows at most {max_sizes} raise size(s) on the {} — \
                 a wider tree exceeds the memory budget",
                street_str(street)
            ),
        );
    }

    // --- range length cap (F2) — BEFORE the synchronous parse in
    //     validate_request, so a near-body-limit comma-heavy range can't burn
    //     worker CPU without consuming a solve permit. Mirrors the bet/raise
    //     sizing length cap above. Checked on the RAW request strings (the cache
    //     key + validate_request both consume `dto.oop_range`/`dto.ip_range`). ---
    for (label, range) in [("oop", &dto.oop_range), ("ip", &dto.ip_range)] {
        if range.len() > MAX_RANGE_LEN {
            return bad_request(
                "invalid_range",
                format!(
                    "{label} range is too long ({} chars, limit {MAX_RANGE_LEN}) — \
                     use a normal range like \"22+,A2s+,KTo+\"",
                    range.len()
                ),
            );
        }
        if count_range_tokens(range) > MAX_RANGE_TOKENS {
            return bad_request(
                "invalid_range",
                format!(
                    "{label} range has too many tokens (limit {MAX_RANGE_TOKENS}) — \
                     use a normal range like \"22+,A2s+,KTo+\""
                ),
            );
        }
    }

    // --- assemble the engine-typed request ---
    let req = SolveRequest {
        street,
        flop,
        turn,
        river,
        oop_range: dto.oop_range.clone(),
        ip_range: dto.ip_range.clone(),
        starting_pot: dto.starting_pot,
        effective_stack: dto.effective_stack,
        bet_sizes: bet_sizes.clone(),
        raise_sizes: raise_sizes.clone(),
        hero,
        solving_player,
        limits: SolveLimits {
            max_memory_bytes: PUBLIC_MAX_MEMORY_BYTES,
            max_iterations: PUBLIC_MAX_ITERATIONS,
            target_exploitability_pct_of_pot: PUBLIC_TARGET_EXPLOIT_PCT,
        },
    };

    // --- cheap, solve-free validation BEFORE the permit (F3 hardening) ---
    // Reject malformed input (bad board/range/stakes/sizing, or an IP-at-root
    // view) here so a flood of invalid requests can't each grab a scarce solve
    // permit and starve real solves. `solve_spot` re-runs the same checks as the
    // single source of truth, so this is a fast-fail, not the only guard.
    if let Err(e) = gto_solver::validate_request(&req) {
        return map_solve_error(e);
    }

    // --- cache lookup (the GB solve is a one-time cost per canonical spot) ---
    // F3: keyed on the HERO-INDEPENDENT `spot_key` so a request that differs ONLY
    // in the hero hand reuses the already-solved equilibrium — the hero row is
    // derived cheaply from the cached per-hand table. (Previously keyed by
    // `response_key`, which folded in the hero, so changing only the hero forced a
    // full GB-scale re-solve and the cache DEFEATED its own DoS mitigation.)
    // Whitespace/case/token-order range variants already collapse via
    // `canonical_range` inside `spot_key`.
    let key = spot_key(&req);
    if let Some(cached) = solve_cache().get(&key) {
        let resp = response_from_cached_spot(&cached, req.hero, true);
        return Json(resp).into_response();
    }

    // --- per-IP in-flight guard (F1) — BEFORE the global permit so one source
    //     cannot occupy every scarce (non-cancellable) global solve permit and
    //     starve the public solver. Held for the whole solve via the RAII guard;
    //     released (and the registry entry reclaimed) on drop. ---
    let client_ip = client_ip(connect_info.as_ref());
    let ip_guard = match acquire_ip_inflight(client_ip) {
        Some(g) => g,
        None => {
            // This IP already has a solve in flight. Honest 503 — same shape as a
            // full global pool; the rate limit + this guard bound the blast radius.
            // N1: carries the §13 source offer too.
            return error_response(
                StatusCode::SERVICE_UNAVAILABLE,
                "solver_busy",
                Some("you already have a solve in progress — please wait for it to finish"),
            );
        }
    };

    // --- global concurrency permit (cap box-wide concurrent solves) ---
    let sem = solve_semaphore();
    let permit = match sem.try_acquire_owned() {
        Ok(p) => p,
        Err(_) => {
            // All solver permits busy. Honest 503 — try again shortly. We do NOT
            // block waiting (that would let requests pile up and amplify load).
            // `ip_guard` drops here, freeing this IP's in-flight slot.
            // N1: carries the §13 source offer too.
            return error_response(
                StatusCode::SERVICE_UNAVAILABLE,
                "solver_busy",
                Some("the free solver is at capacity right now — please retry in a moment"),
            );
        }
    };

    // --- solve on the blocking pool under a hard timeout ---
    let req_for_solve = req.clone();
    let solve_fut = tokio::task::spawn_blocking(move || {
        // Hold BOTH the global permit AND the per-IP in-flight guard for the whole
        // solve, then drop them. F1+F3: moving `ip_guard` INTO the blocking task
        // (not the handler scope) means the per-IP slot is released only when the
        // non-cancellable solve actually FINISHES — not when a caller-side timeout
        // returns early. So a timed-out solve still counts against its IP's
        // in-flight cap until it truly completes, exactly like the global permit.
        let _permit = permit;
        let _ip_guard = ip_guard;
        solve_spot(&req_for_solve)
    });

    let timeout = effective_solve_timeout();
    let result = match tokio::time::timeout(timeout, solve_fut).await {
        Ok(Ok(solve_result)) => solve_result,
        Ok(Err(join_err)) => {
            tracing::error!("gto_solve spawn_blocking join error: {join_err}");
            // N1: carries the §13 source offer too.
            return error_response(StatusCode::INTERNAL_SERVER_ERROR, "internal_error", None);
        }
        Err(_elapsed) => {
            // Timed out from the CALLER's perspective only. F3 (codex HIGH):
            // upstream `solve()` is a synchronous, non-cancellable
            // `for t in 0..max_iterations` loop with no interrupt hook, and the
            // spawn_blocking closure holds its concurrency permit (`let _permit =
            // permit`) until it finishes NATURALLY. So this timeout bounds caller
            // LATENCY, NOT solver CAPACITY: the blocking task keeps running and
            // its permit stays consumed until the solve completes on its own (it
            // DOES terminate — bounded by the iteration cap + memory gate, so
            // this is finite, not "forever"). With MAX_CONCURRENT_SOLVES permits,
            // a few slow in-budget solves can keep all permits while new callers
            // get `solver_busy` (503). The per-IP rate limit + memory gate +
            // iteration cap bound the blast radius; a fully cancellable /
            // out-of-process worker would be the structural fix (deferred).
            tracing::warn!("gto_solve exceeded {timeout:?} — returning solve_timeout");
            // N1: carries the §13 source offer too.
            return error_response(
                StatusCode::SERVICE_UNAVAILABLE,
                "solve_timeout",
                Some("this spot took too long to solve on the free tier — try a narrower range"),
            );
        }
    };

    let out = match result {
        Ok(out) => out,
        Err(e) => return map_solve_error(e),
    };

    // F3: build the HERO-INDEPENDENT cached body + per-hand table, then cache it
    // under the hero-independent `spot_key`. The per-request response (with THIS
    // hero's row) is derived from that cached spot — so a later request that
    // differs ONLY in the hero hand re-derives its row from the cache instead of
    // re-solving the GB-scale equilibrium.
    let spot = CachedSpot {
        response: to_response_no_hero(&out, &bet_sizes, &raise_sizes, false),
        hands: out.hands,
        // Filled in by `cache_insert` from `cached_spot_bytes`; 0 until then.
        bytes: 0,
    };
    let resp = response_from_cached_spot(&spot, req.hero, false);

    // Cache the solved spot under a BYTE budget (F7): `cache_insert` estimates the
    // entry's footprint and evicts oldest entries (FIFO) so total tracked memory
    // stays under `MAX_CACHE_BYTES` regardless of per-entry size — a wide spot's
    // ~135 KB per-hand table can no longer let a 2048-entry cap reach ~275 MB.
    cache_insert(key, spot);

    Json(resp).into_response()
}

/// `GET /api/tools/poker/solve/source` — the AGPL §13 network-user offer (F4).
///
/// A login-free endpoint returning the Corresponding-Source link for the EXACT
/// deployed AGPL postflop-solver version. This is the structural §13 offer: the
/// route only mounts when `POSTFLOP_SOLVER_SOURCE_URL` is configured, so when this
/// handler is reachable the URL is always present. Returns
/// `{"label": "Source code (AGPL-3.0)", "url": <url>}` — the label is the locked
/// ADR-072 §3 string so the UI renders a consistent, visible link.
pub async fn solve_source_handler() -> impl IntoResponse {
    match source_url() {
        Some(url) => Json(serde_json::json!({
            "label": "Source code (AGPL-3.0)",
            "url": url,
        }))
        .into_response(),
        // Defensive: the route does not mount without a URL, so this is
        // unreachable in practice. Fail honestly rather than 500.
        None => (
            StatusCode::SERVICE_UNAVAILABLE,
            Json(serde_json::json!({
                "error": "source_unavailable",
                "reason": "the source link is not configured"
            })),
        )
            .into_response(),
    }
}

// ===========================================================================
// Tests (pure request/response shaping + helpers — no DB, no solve)
// ===========================================================================

#[cfg(test)]
mod tests {
    use super::*;
    use engine::card::{Rank, Suit};

    #[test]
    fn count_bet_sizes_handles_commas_and_blanks() {
        assert_eq!(count_bet_sizes("50%"), 1);
        assert_eq!(count_bet_sizes("33%,75%"), 2);
        assert_eq!(count_bet_sizes("33%, 75%, "), 2);
        assert_eq!(count_bet_sizes(""), 0);
    }

    #[test]
    fn parse_board_enforces_count_for_street() {
        // flop needs 3
        assert!(parse_board(SolveStreet::Flop, &["Td".into(), "9d".into()]).is_err());
        assert!(parse_board(SolveStreet::Flop, &["Td".into(), "9d".into(), "6h".into()]).is_ok());
        // turn needs 4
        assert!(parse_board(SolveStreet::Turn, &["Td".into(), "9d".into(), "6h".into()]).is_err());
        // river needs 5
        assert!(parse_board(
            SolveStreet::River,
            &[
                "Td".into(),
                "9d".into(),
                "6h".into(),
                "Qc".into(),
                "2s".into()
            ]
        )
        .is_ok());
    }

    #[test]
    fn parse_board_rejects_duplicate_and_bad_cards() {
        assert!(matches!(
            parse_board(SolveStreet::Flop, &["Td".into(), "Td".into(), "6h".into()]),
            Err((code, _)) if code == "duplicate_card"
        ));
        assert!(matches!(
            parse_board(SolveStreet::Flop, &["XX".into(), "9d".into(), "6h".into()]),
            Err((code, _)) if code == "invalid_card"
        ));
    }

    fn sample_req() -> SolveRequest {
        SolveRequest {
            street: SolveStreet::Flop,
            flop: [
                Card::new(Rank::Ten, Suit::Diamonds),
                Card::new(Rank::Nine, Suit::Diamonds),
                Card::new(Rank::Six, Suit::Hearts),
            ],
            turn: None,
            river: None,
            oop_range: "AA,KK".into(),
            ip_range: "JJ,TT".into(),
            starting_pot: 60,
            effective_stack: 100,
            bet_sizes: "50%".into(),
            raise_sizes: "2.5x".into(),
            hero: None,
            solving_player: Player::Oop,
            limits: SolveLimits::default(),
        }
    }

    #[test]
    fn response_key_is_stable_and_case_insensitive() {
        let a = sample_req();
        let mut b = sample_req();
        b.oop_range = "aa,kk".into(); // case + (no) whitespace equivalent
        b.bet_sizes = "50%".into();
        assert_eq!(
            response_key(&a),
            response_key(&b),
            "equivalent spots → same key"
        );
    }

    // F3: equivalent ranges that differ only by token ORDER / whitespace must
    // canonicalize to the SAME spot/response key so the GB solve is reused
    // instead of re-run.
    #[test]
    fn response_key_is_order_and_whitespace_insensitive_on_ranges() {
        let a = sample_req();
        let mut b = sample_req();
        b.oop_range = " KK , AA ".into(); // reordered + whitespace
        b.ip_range = "TT,JJ".into(); // reordered
        assert_eq!(
            spot_key(&a),
            spot_key(&b),
            "reordered/whitespace range variants → same hero-independent spot key"
        );
        assert_eq!(
            response_key(&a),
            response_key(&b),
            "reordered/whitespace range variants → same response key"
        );
    }

    // F3: the spot key is HERO-INDEPENDENT (the equilibrium does not depend on the
    // hero hand), while the response key DOES fold in the hero (the response body
    // carries that hand's strategy).
    #[test]
    fn spot_key_ignores_hero_but_response_key_does_not() {
        let a = sample_req();
        let mut withhero = sample_req();
        withhero.hero = Some(HoleCards::new(
            Card::new(Rank::Ace, Suit::Spades),
            Card::new(Rank::Ace, Suit::Hearts),
        ));
        assert_eq!(
            spot_key(&a),
            spot_key(&withhero),
            "the equilibrium spot key must be hero-independent"
        );
        assert_ne!(
            response_key(&a),
            response_key(&withhero),
            "the response key must distinguish a hero-bearing response"
        );
    }

    #[test]
    fn response_key_changes_with_board() {
        let a = sample_req();
        let mut b = sample_req();
        b.flop[2] = Card::new(Rank::Seven, Suit::Hearts);
        assert_ne!(response_key(&a), response_key(&b));
    }

    #[test]
    fn response_key_changes_with_solving_player_and_hero() {
        let a = sample_req();
        let mut b = sample_req();
        b.solving_player = Player::Ip;
        assert_ne!(response_key(&a), response_key(&b));

        let mut c = sample_req();
        c.hero = Some(HoleCards::new(
            Card::new(Rank::Ace, Suit::Spades),
            Card::new(Rank::Ace, Suit::Hearts),
        ));
        assert_ne!(response_key(&a), response_key(&c));
    }

    #[test]
    fn canonical_range_normalizes_order_case_and_whitespace() {
        assert_eq!(canonical_range(" KK , AA "), canonical_range("aa,kk"));
        assert_eq!(canonical_range("AKs,AA"), "aa,aks");
        // empty tokens are dropped.
        assert_eq!(canonical_range("AA,,KK,"), "aa,kk");
        assert_eq!(canonical_range(""), "");
    }

    #[test]
    fn count_range_tokens_counts_segments_including_empties() {
        // The DoS vector is split-segment count, so empties COUNT (unlike
        // count_bet_sizes which ignores them).
        assert_eq!(count_range_tokens("AA,KK,QQ"), 3);
        assert_eq!(count_range_tokens(",,,"), 4);
        assert_eq!(count_range_tokens(""), 1);
    }

    // F1: a single IP can hold at most MAX_INFLIGHT_PER_IP concurrent solves; a
    // second acquire from the SAME ip is refused while the first guard is held,
    // and a DIFFERENT ip is unaffected.
    #[test]
    fn per_ip_inflight_caps_one_concurrent_solve_per_ip() {
        let ip_a = IpAddr::V4(std::net::Ipv4Addr::new(10, 0, 0, 1));
        let ip_b = IpAddr::V4(std::net::Ipv4Addr::new(10, 0, 0, 2));
        let g1 = acquire_ip_inflight(ip_a).expect("first solve for ip_a is granted");
        assert!(
            acquire_ip_inflight(ip_a).is_none(),
            "a second concurrent solve from the same IP must be refused"
        );
        // A different IP always has a free slot.
        let g_b = acquire_ip_inflight(ip_b).expect("a different IP is unaffected");
        drop(g1);
        // Once the first guard drops, the IP can solve again.
        let g2 = acquire_ip_inflight(ip_a).expect("after the first solve ends the IP can solve");
        drop(g2);
        drop(g_b);
        // The registry reclaims now-idle entries on drop (no unbounded growth).
        assert!(
            !ip_inflight_registry().contains_key(&ip_a),
            "an idle IP's registry entry is reclaimed on drop"
        );
    }

    #[test]
    fn client_ip_uses_peer_or_loopback_fallback() {
        let peer = ConnectInfo(SocketAddr::from(([203, 0, 113, 7], 4444)));
        assert_eq!(
            client_ip(Some(&peer)),
            IpAddr::V4(std::net::Ipv4Addr::new(203, 0, 113, 7))
        );
        assert_eq!(
            client_ip(None),
            IpAddr::V4(std::net::Ipv4Addr::LOCALHOST),
            "a missing ConnectInfo falls back to the loopback sentinel"
        );
    }

    #[test]
    fn dto_deserializes_with_optional_fields_defaulted() {
        let dto: SolveRequestDto = serde_json::from_value(serde_json::json!({
            "street": "flop",
            "board": ["Td", "9d", "6h"],
            "oop_range": "AA,KK",
            "ip_range": "JJ,TT",
            "starting_pot": 60,
            "effective_stack": 100,
            "solving_player": "oop"
        }))
        .unwrap();
        assert!(dto.bet_sizes.is_none());
        assert!(dto.raise_sizes.is_none());
        assert!(dto.hero.is_none());
    }

    // F7: build a synthetic CachedSpot whose per-hand table is `hands` rows wide,
    // each carrying a `freqs`-long frequency vector — large enough that a handful
    // of entries would blow a coarse entry-count cap but still be "a few MB".
    fn synthetic_spot(hands: usize, freqs: usize) -> CachedSpot {
        let mut table = Vec::with_capacity(hands);
        for i in 0..hands {
            table.push(HandStrategy {
                hand: format!("h{i:08}"),
                frequencies: vec![0.25_f32; freqs],
                equity: 0.5,
                ev: 1.0,
            });
        }
        CachedSpot {
            response: SolveResponse {
                method: "cfr_equilibrium".into(),
                solving_player: "oop".into(),
                exploitability: 0.1,
                exploitability_pct_of_pot: 0.1,
                actions: Vec::new(),
                range_equity: 0.5,
                range_ev: 1.0,
                hero: None,
                bet_sizes: "50%".into(),
                raise_sizes: "2.5x".into(),
                max_iterations: PUBLIC_MAX_ITERATIONS,
                cached: false,
                source_url: None,
            },
            hands: table,
            bytes: 0,
        }
    }

    // F7: the solved-spot cache is bounded by BYTES, not entry count. Insert many
    // large DISTINCT spots whose combined estimated footprint FAR exceeds the byte
    // budget and assert the tracked total never exceeds `MAX_CACHE_BYTES`, that the
    // map is kept under the budget (entries evicted), and that the accountant stays
    // in lock-step with the map's real estimated size.
    #[test]
    fn cache_is_bounded_by_bytes_not_entry_count() {
        // Each entry: ~4000 hands × 8 freqs ≈ a few hundred KB — comparable to a
        // wide early-street per-hand table. Inserting far more than fit in the
        // budget proves eviction keeps total bytes capped.
        let per_entry = cached_spot_bytes(&synthetic_spot(4_000, 8));
        assert!(per_entry > 0, "a non-empty spot must estimate > 0 bytes");
        // Enough entries that their SUM is several × the budget.
        let n = (MAX_CACHE_BYTES / per_entry) * 4 + 16;

        // Use a key range disjoint from any other test so this is self-contained
        // against the process-global cache (other unit tests don't touch it).
        const BASE: u64 = 0xF7F7_0000_0000_0000;
        for i in 0..(n as u64) {
            cache_insert(BASE + i, synthetic_spot(4_000, 8));
            let tracked = cache_bytes().load(std::sync::atomic::Ordering::Relaxed);
            assert!(
                tracked <= MAX_CACHE_BYTES,
                "after insert #{i} the tracked cache bytes ({tracked}) must stay \
                 within the byte budget ({MAX_CACHE_BYTES})"
            );
        }

        // The cache must have evicted: it cannot hold all `n` huge entries.
        let entries = solve_cache()
            .iter()
            .filter(|kv| (*kv.key() & 0xFFFF_0000_0000_0000) == 0xF7F7_0000_0000_0000)
            .count();
        assert!(
            entries < n,
            "the byte budget must have evicted entries (held {entries} of {n})"
        );

        // The accountant must equal the SUM of the (recorded) bytes of the entries
        // actually in the map — proof it stayed in lock-step through eviction.
        let real: usize = solve_cache().iter().map(|kv| kv.value().bytes).sum();
        let tracked = cache_bytes().load(std::sync::atomic::Ordering::Relaxed);
        assert_eq!(
            tracked, real,
            "the byte accountant must match the map's recorded entry bytes"
        );

        // A single entry larger than the WHOLE budget is simply not cached (it
        // would force an empty cache), and it does not corrupt the accountant.
        let before = cache_bytes().load(std::sync::atomic::Ordering::Relaxed);
        let oversized = synthetic_spot(MAX_CACHE_BYTES / 8 + 1_000, 16);
        assert!(
            cached_spot_bytes(&oversized) > MAX_CACHE_BYTES,
            "the oversized spot must exceed the whole budget for this assertion"
        );
        cache_insert(0xF7F7_FFFF_FFFF_FFFF, oversized);
        assert!(
            !solve_cache().contains_key(&0xF7F7_FFFF_FFFF_FFFF),
            "an entry larger than the whole budget must not be cached"
        );
        assert_eq!(
            cache_bytes().load(std::sync::atomic::Ordering::Relaxed),
            before,
            "rejecting an oversized entry must not change the byte accountant"
        );

        // Clean up our keys so we leave the process-global cache as we found it.
        let keys: Vec<u64> = solve_cache()
            .iter()
            .map(|kv| *kv.key())
            .filter(|k| (*k & 0xFFFF_0000_0000_0000) == 0xF7F7_0000_0000_0000)
            .collect();
        for k in keys {
            if let Some((_, removed)) = solve_cache().remove(&k) {
                cache_bytes().fetch_sub(removed.bytes, std::sync::atomic::Ordering::Relaxed);
            }
        }
        if let Ok(mut q) = cache_order().lock() {
            q.retain(|k| (*k & 0xFFFF_0000_0000_0000) != 0xF7F7_0000_0000_0000);
        }
    }
}
