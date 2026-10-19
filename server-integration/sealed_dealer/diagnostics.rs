//! Optional correlation evidence, deliberately separate from card/auth data.
//! A fixed queue and try_send keep log I/O/backpressure off the dealing task.
use serde::Deserialize;
use std::sync::{
    atomic::{AtomicU64, Ordering},
    mpsc::{sync_channel, SyncSender},
    OnceLock,
};
use uuid::Uuid;

const QUEUE_CAPACITY: usize = 256;
const LOGS_PER_SECOND: u32 = 128;
static DROPPED: AtomicU64 = AtomicU64::new(0);

pub(super) fn valid_trace(value: &str) -> Option<Uuid> {
    if value.len() != 36 {
        return None;
    }
    let id = Uuid::parse_str(value).ok()?;
    (id.get_version_num() == 4 && id.to_string() == value).then_some(id)
}

pub(crate) fn deserialize_trace<'de, D: serde::Deserializer<'de>>(
    deserializer: D,
) -> Result<Option<Uuid>, D::Error> {
    let value = serde_json::Value::deserialize(deserializer)?;
    Ok(value.as_str().and_then(valid_trace))
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
    source: &'static str,
    connection: Uuid,
    trace: Option<Uuid>,
    phase: &'static str,
    elapsed_ms: f64,
    phase_ms: f64,
}

fn enqueue(sender: &SyncSender<Event>, event: Event) {
    if sender.try_send(event).is_err() {
        DROPPED.fetch_add(1, Ordering::Relaxed);
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
        source: "sealed_dealer",
        connection,
        trace,
        phase,
        elapsed_ms,
        phase_ms,
    });
}

pub(crate) fn record_probe(trace: Uuid, phase: &'static str, elapsed_ms: f64) {
    record_event(Event {
        source: "ws_probe",
        connection: Uuid::nil(),
        trace: Some(trace),
        phase,
        elapsed_ms,
        phase_ms: 0.0,
    });
}

fn record_event(event: Event) {
    static SENDER: OnceLock<Option<SyncSender<Event>>> = OnceLock::new();
    let sender = SENDER.get_or_init(|| {
        if std::env::var_os("SEALED_DEALER_DIAGNOSTICS").as_deref() == Some(std::ffi::OsStr::new("0")) {
            return None;
        }
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
                        DROPPED.fetch_add(1, Ordering::Relaxed);
                        continue;
                    }
                    emitted += 1;
                    let dropped = DROPPED.swap(0, Ordering::Relaxed);
                    tracing::info!(
                        target: "sealed_dealer_evidence",
                        source = event.source,
                        connection_id = %event.connection,
                        diagnostic_trace = %event.trace.map(|id| id.to_string()).unwrap_or_default(),
                        phase = event.phase,
                        elapsed_ms = event.elapsed_ms,
                        phase_ms = event.phase_ms,
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
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn accepts_only_canonical_random_v4_trace() {
        let trace = "2cdbb614-2b4d-42c9-bfdc-e071f7d236a1";
        assert!(valid_trace(trace).is_some());
        for value in [
            "secret-ticket",
            "\nlog-forgery",
            "00000000-0000-0000-0000-000000000000",
            "2CDBB614-2B4D-42C9-BFDC-E071F7D236A1",
        ] {
            assert!(valid_trace(value).is_none());
        }
        assert_eq!(
            query_trace(Some(&format!("diagnostic_trace={trace}"))),
            valid_trace(trace)
        );
        assert!(query_trace(Some(&format!(
            "diagnostic_trace={trace}&diagnostic_trace={trace}"
        )))
        .is_none());
    }

    #[test]
    fn full_or_disconnected_evidence_queue_discards_without_waiting() {
        let (sender, receiver) = sync_channel(1);
        let event = Event {
            source: "test",
            connection: Uuid::nil(),
            trace: None,
            phase: "test",
            elapsed_ms: 0.0,
            phase_ms: 0.0,
        };
        enqueue(&sender, event);
        enqueue(&sender, event);
        assert!(receiver.try_recv().is_ok());
        assert!(receiver.try_recv().is_err());
        drop(receiver);
        enqueue(&sender, event);
    }
}
