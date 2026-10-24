//! Optional correlation evidence, deliberately separate from card/auth data.
//! A fixed queue and try_send keep log I/O/backpressure off the dealing task.
use std::sync::{
    atomic::{AtomicU64, Ordering},
    mpsc::{sync_channel, SyncSender},
    Mutex, OnceLock,
};
use std::time::{Duration, Instant};
use uuid::Uuid;

const QUEUE_CAPACITY: usize = 256;
const LOGS_PER_SECOND: u32 = 128;
static DROPPED: AtomicU64 = AtomicU64::new(0);
static LOG_DROPPED_TOTAL: AtomicU64 = AtomicU64::new(0);
static RETENTION_DROPPED: AtomicU64 = AtomicU64::new(0);
static SEQUENCE: AtomicU64 = AtomicU64::new(0);
const RECENT_CAPACITY: usize = 8192;
const RETENTION: Duration = Duration::from_secs(120);
const SNAPSHOT_LIMIT: usize = 48;

fn enabled() -> bool {
    static ENABLED: OnceLock<bool> = OnceLock::new();
    *ENABLED.get_or_init(|| {
        std::env::var_os("SEALED_DEALER_DIAGNOSTICS").as_deref() != Some(std::ffi::OsStr::new("0"))
    })
}

fn process_id() -> Uuid {
    static ID: OnceLock<Uuid> = OnceLock::new();
    *ID.get_or_init(Uuid::new_v4)
}

pub(super) fn valid_trace(value: &str) -> Option<Uuid> {
    if value.len() != 36 {
        return None;
    }
    let id = Uuid::parse_str(value).ok()?;
    (id.get_version_num() == 4 && id.to_string() == value).then_some(id)
}

pub(super) fn query_trace(query: Option<&str>) -> Option<Uuid> {
    // Canonical UUIDs need no URL decoding. Reject duplicates and oversized input;
    // malformed optional diagnostics must never reject a WebSocket upgrade.
    let query = query.filter(|query| query.len() <= 256)?;
    let mut values = query
        .split('&')
        .filter_map(|part| part.strip_prefix("diagnostic_trace="));
    let trace = valid_trace(values.next()?)?;
    values.next().is_none().then_some(trace)
}

#[derive(Clone, Copy)]
struct Event {
    sequence: u64,
    source: &'static str,
    connection: Uuid,
    trace: Option<Uuid>,
    phase: &'static str,
    elapsed_ms: f64,
    phase_ms: f64,
    action_delivery_ms: Option<[f64; 11]>,
    encode_cpu_ms: Option<f64>,
}

struct Recent {
    events: [Option<(Instant, Event)>; RECENT_CAPACITY],
    next: usize,
    evicted: u64,
}

// Fixed memory; producers never wait for a reader or perform serialization.
static RECENT: Mutex<Recent> = Mutex::new(Recent {
    events: [None; RECENT_CAPACITY],
    next: 0,
    evicted: 0,
});

fn retain(event: Event) {
    if event.trace.is_none() {
        return;
    }
    let Ok(mut recent) = RECENT.try_lock() else {
        RETENTION_DROPPED.fetch_add(1, Ordering::Relaxed);
        return;
    };
    let now = Instant::now();
    let next = recent.next;
    if recent.events[next].is_some_and(|(at, _)| now.duration_since(at) <= RETENTION) {
        recent.evicted += 1;
    }
    recent.events[next] = Some((now, event));
    recent.next = (next + 1) % RECENT_CAPACITY;
}

/// Only the ingestion worker calls this, after client-field sanitization. The
/// saved copy survives ordinary log rate/queue shedding; missing or partial
/// retention stays explicit. Optional collector failure never gates an action.
pub(crate) fn attach_recent(metadata: &mut serde_json::Value) {
    if metadata
        .get("transport")
        .and_then(serde_json::Value::as_str)
        != Some("ws")
        || metadata
            .get("elapsed_ms")
            .and_then(serde_json::Value::as_u64)
            .unwrap_or(0)
            < 1000
    {
        return;
    }
    let Some(trace) = metadata
        .pointer("/diagnostics/trace_id")
        .and_then(serde_json::Value::as_str)
        .and_then(valid_trace)
    else {
        return;
    };
    let mut snapshot = serde_json::json!({
        "wire": 1,
        "state": "missing_or_expired",
        "process_id": process_id(),
        "log_dropped_total": LOG_DROPPED_TOTAL.load(Ordering::Relaxed),
        "retention_dropped_total": RETENTION_DROPPED.load(Ordering::Relaxed),
        "retention_evicted_total": 0,
        "truncated": false,
        "events": [],
    });
    if !enabled() {
        snapshot["state"] = "disabled".into();
    } else if let Ok(recent) = RECENT.try_lock() {
        let now = Instant::now();
        // Copy a fixed maximum under the lock, then format outside it. A
        // contending producer discards immediately and increments coverage loss.
        let mut selected = [None; SNAPSHOT_LIMIT];
        let mut count = 0;
        for (_, event) in recent.events.iter().flatten().filter(|(at, event)| {
            event.trace == Some(trace) && now.duration_since(*at) <= RETENTION
        }) {
            if count == SNAPSHOT_LIMIT {
                snapshot["truncated"] = true.into();
                break;
            }
            selected[count] = Some(*event);
            count += 1;
        }
        let evicted = recent.evicted;
        drop(recent);
        snapshot["retention_evicted_total"] = evicted.into();
        if count > 0 {
            snapshot["state"] = "retained".into();
        }
        let mut events: Vec<_> = selected.into_iter().flatten().collect();
        events.sort_unstable_by_key(|event| event.sequence);
        snapshot["events"] = events
            .into_iter()
            .map(|event| {
                serde_json::json!({
                    "evidence_seq": event.sequence,
                    "source": event.source,
                    "connection_id": event.connection,
                    "diagnostic_trace": event.trace,
                    "phase": event.phase,
                    "elapsed_ms": event.elapsed_ms,
                    "phase_ms": event.phase_ms,
                    "action_delivery_ms": event.action_delivery_ms,
                    "encode_cpu_ms": event.encode_cpu_ms,
                })
            })
            .collect();
    } else {
        snapshot["state"] = "busy".into();
    }
    metadata["server_ws_evidence"] = snapshot;
}

fn log_dropped() {
    DROPPED.fetch_add(1, Ordering::Relaxed);
    LOG_DROPPED_TOTAL.fetch_add(1, Ordering::Relaxed);
}

fn enqueue(sender: &SyncSender<Event>, event: Event) {
    if sender.try_send(event).is_err() {
        log_dropped();
    }
}

pub(super) fn record(
    connection: Uuid,
    trace: Option<Uuid>,
    phase: &'static str,
    elapsed_ms: f64,
    phase_ms: f64,
) {
    record_event(Event {
        sequence: 0,
        source: "sealed_dealer",
        connection,
        trace,
        phase,
        elapsed_ms,
        phase_ms,
        action_delivery_ms: None,
        encode_cpu_ms: None,
    });
}

pub(crate) fn record_probe(trace: Uuid, phase: &'static str, elapsed_ms: f64) {
    record_event(Event {
        sequence: 0,
        source: "ws_probe",
        connection: Uuid::nil(),
        trace: Some(trace),
        phase,
        elapsed_ms,
        phase_ms: 0.0,
        action_delivery_ms: None,
        encode_cpu_ms: None,
    });
}

/// Same-action evidence shares the bounded nonblocking collector, never probe
/// semantics or an independent input's timing.
pub(crate) fn record_action(trace: Uuid, phase: &'static str, elapsed_ms: f64) {
    record_event(Event {
        sequence: 0,
        source: "ws_action",
        connection: Uuid::nil(),
        trace: Some(trace),
        phase,
        elapsed_ms,
        phase_ms: 0.0,
        action_delivery_ms: None,
        encode_cpu_ms: None,
    });
}

/// One bounded producer event holds fixed monotonic milestones; only the
/// collector thread formats them. -1 means missing, never a measured zero.
pub(crate) fn record_action_delivery(
    trace: Uuid,
    phase: &'static str,
    elapsed_ms: f64,
    milestones: [f64; 11],
    encode_cpu_ms: Option<f64>,
) {
    record_event(Event {
        sequence: 0,
        source: "ws_action",
        connection: Uuid::nil(),
        trace: Some(trace),
        phase,
        elapsed_ms,
        phase_ms: 0.0,
        action_delivery_ms: Some(milestones),
        encode_cpu_ms,
    });
}

fn record_event(mut event: Event) {
    if !enabled() {
        return;
    }
    event.sequence = SEQUENCE.fetch_add(1, Ordering::Relaxed) + 1;
    retain(event);
    static SENDER: OnceLock<Option<SyncSender<Event>>> = OnceLock::new();
    let sender = SENDER.get_or_init(|| {
        let (sender, receiver) = sync_channel::<Event>(QUEUE_CAPACITY);
        // Only this dedicated thread formats or writes diagnostic log records.
        // A slow/unavailable collector can fill the queue; producers discard.
        std::thread::Builder::new()
            .name("dealer-evidence".into())
            .spawn(move || {
                let mut window = std::time::Instant::now();
                let mut emitted = 0;
                while let Ok(event) = receiver.recv() {
                    if window.elapsed().as_secs() >= 1 {
                        window = std::time::Instant::now();
                        emitted = 0;
                    }
                    if emitted >= LOGS_PER_SECOND {
                        log_dropped();
                        continue;
                    }
                    emitted += 1;
                    let dropped = DROPPED.swap(0, Ordering::Relaxed);
                    tracing::info!(
                        target: "sealed_dealer_evidence",
                        source = event.source,
                        evidence_process = %process_id(),
                        evidence_seq = event.sequence,
                        connection_id = %event.connection,
                        diagnostic_trace = %event.trace.map(|id| id.to_string()).unwrap_or_default(),
                        phase = event.phase,
                        elapsed_ms = event.elapsed_ms,
                        phase_ms = event.phase_ms,
                        action_delivery_ms = ?event.action_delivery_ms,
                        encode_cpu_ms = ?event.encode_cpu_ms,
                        evidence_dropped = dropped,
                        "optional socket delivery evidence; flush completion is not a client acknowledgement"
                    );
                }
            })
            .ok()
            .map(|_| sender)
    });
    if let Some(sender) = sender {
        enqueue(sender, event);
    } else {
        log_dropped();
    }
}
