//! Optional, authenticated reads of the isolated worker's evidence ring.
//! Never read hands, cards, tickets or a business lock. Fixed limits apply to
//! both ends; ingestion preserves an explicit failure marker instead of
//! substituting the backend's unrelated ring or dropping the client event.
use super::*;
use serde_json::{json, Value};
use std::sync::Mutex;
use std::time::Instant;

const TRACE_LIMIT: usize = 8;
const BODY_LIMIT: usize = 256 * 1024;
const FETCH_BUDGET: Duration = Duration::from_millis(50);

#[derive(Deserialize, serde::Serialize)]
#[serde(deny_unknown_fields)]
pub(super) struct EvidenceRequest {
    traces: Vec<Uuid>,
}

pub(super) async fn snapshot(
    State(state): State<WorkerState>,
    headers: HeaderMap,
    Json(request): Json<EvidenceRequest>,
) -> axum::response::Response {
    if !internal_authorized(&headers, &state) {
        return StatusCode::UNAUTHORIZED.into_response();
    }
    if request.traces.is_empty()
        || request.traces.len() > TRACE_LIMIT
        || request
            .traces
            .iter()
            .any(|trace| trace.get_version_num() != 4)
        || request
            .traces
            .iter()
            .enumerate()
            .any(|(index, trace)| request.traces[..index].contains(trace))
    {
        return StatusCode::BAD_REQUEST.into_response();
    }
    static READER: tokio::sync::Semaphore = tokio::sync::Semaphore::const_new(1);
    let Ok(permit) = READER.try_acquire() else {
        return StatusCode::SERVICE_UNAVAILABLE.into_response();
    };
    match tokio::task::spawn_blocking(move || {
        let _permit = permit;
        request
            .traces
            .into_iter()
            .map(|trace| json!({"trace_id": trace, "snapshot": diagnostics::snapshot(trace)}))
            .collect::<Vec<_>>()
    })
    .await
    {
        Ok(snapshots) => ([("cache-control", "no-store")], Json(snapshots)).into_response(),
        Err(_) => StatusCode::SERVICE_UNAVAILABLE.into_response(),
    }
}

impl SealedDealerClient {
    async fn evidence_snapshots(&self, traces: &[Uuid]) -> Result<Vec<Value>, &'static str> {
        let read = async {
            let mut request = self
                .request(
                    self.metrics_http
                        .post(format!("{}/internal/evidence", self.base_url)),
                )
                .json(&EvidenceRequest {
                    traces: traces.to_vec(),
                })
                .build()
                .map_err(|_| "invalid_request")?;
            authorization::sign(&mut request).map_err(|_| "invalid_request")?;
            let mut response = self
                .metrics_http
                .execute(request)
                .await
                .map_err(|_| "transport_error")?;
            if response.status() != reqwest::StatusCode::OK {
                return Err("worker_unavailable");
            }
            let mut body = Vec::new();
            while let Some(chunk) = response.chunk().await.map_err(|_| "transport_error")? {
                if chunk.len() > BODY_LIMIT - body.len() {
                    return Err("response_limit");
                }
                body.extend_from_slice(&chunk);
            }
            let rows: Vec<Value> = serde_json::from_slice(&body).map_err(|_| "invalid_response")?;
            if rows.len() != traces.len()
                || rows.iter().zip(traces).any(|(row, trace)| {
                    row["trace_id"].as_str() != Some(trace.to_string().as_str())
                        || row["snapshot"]["wire"] != 1
                        || row["snapshot"]["process_id"]
                            .as_str()
                            .and_then(diagnostics::valid_trace)
                            .is_none()
                        || row["snapshot"]["events"].as_array().is_none_or(|events| {
                            events.len() > 48
                                || events.iter().any(|event| {
                                    event["diagnostic_trace"] != row["trace_id"]
                                        || event["source"] != "sealed_dealer"
                                })
                        })
                })
            {
                return Err("invalid_response");
            }
            Ok(rows)
        };
        tokio::time::timeout(FETCH_BUDGET, read)
            .await
            .map_err(|_| "timeout")?
    }
}

fn eligible_trace(metadata: &Value) -> Option<Uuid> {
    if metadata["transport"] != "ws"
        || metadata["endpoint"] != "/sealed-dealer/ws"
        || metadata["operation"] != "request_latency"
        || metadata["trend_only"] == true
        || metadata["elapsed_ms"].as_u64().unwrap_or(0) < 1000
    {
        return None;
    }
    metadata
        .pointer("/diagnostics/trace_id")
        .and_then(Value::as_str)
        .and_then(diagnostics::valid_trace)
}

/// One bounded batch read, at most ten attempts/second, after ordinary writes.
/// Producers and gameplay never await it; contention sheds only evidence.
pub(crate) async fn attach_dealer_evidence(metadata: &mut [&mut Value]) {
    let mut traces = Vec::new();
    for value in metadata.iter_mut() {
        if let Some(trace) = eligible_trace(value) {
            value
                .as_object_mut()
                .expect("sanitized metadata object")
                .remove("server_ws_evidence");
            if !traces.contains(&trace) && traces.len() < TRACE_LIMIT {
                traces.push(trace);
            }
            value["server_ws_evidence_fetch"] =
                json!({"owner":"sealed_dealer", "state":"batch_limit"});
        }
    }
    if traces.is_empty() {
        return;
    }
    static LAST_READ: Mutex<Option<Instant>> = Mutex::new(None);
    let admitted = LAST_READ.try_lock().is_ok_and(|mut last| {
        let now = Instant::now();
        if last.is_some_and(|at| now.duration_since(at) < Duration::from_millis(100)) {
            return false;
        }
        *last = Some(now);
        true
    });
    let result = if !admitted {
        Err("rate_limited")
    } else if let Ok(client) = client() {
        client.evidence_snapshots(&traces).await
    } else {
        Err("not_configured")
    };
    for value in metadata.iter_mut() {
        let Some(trace) = eligible_trace(value) else {
            continue;
        };
        let Some(index) = traces.iter().position(|id| *id == trace) else {
            continue;
        };
        match &result {
            Ok(rows) => {
                value["server_ws_evidence"] = rows[index]["snapshot"].clone();
                value["server_ws_evidence_fetch"] =
                    json!({"owner":"sealed_dealer", "state":"fetched"});
            }
            Err(reason) => {
                value["server_ws_evidence_fetch"] =
                    json!({"owner":"sealed_dealer", "state": reason});
            }
        }
    }
}
