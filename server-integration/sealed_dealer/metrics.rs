//! Dealer-local counters. Reading them never touches hands, tickets or cards.
use super::{internal_authorized, WorkerState};
use axum::{
    extract::{MatchedPath, Request, State},
    http::{HeaderMap, HeaderValue, StatusCode},
    middleware::Next,
    response::{IntoResponse, Response},
};

pub(super) const HTTP_ROUTES: &[(&str, &str)] = &[
    ("GET", "/internal/readyz"),
    ("POST", "/internal/hands"),
    ("POST", "/internal/commits"),
    ("POST", "/internal/hands/:hand_id/bots/:seat"),
    ("GET", "/internal/hands/:hand_id/deliveries/:seat"),
    ("POST", "/internal/hands/:hand_id/board"),
    ("POST", "/internal/hands/:hand_id/showdown"),
    ("POST", "/internal/hands/:hand_id/settle"),
    ("DELETE", "/internal/hands/:hand_id"),
    ("GET", "/internal/performance"),
    ("GET", "/healthz"),
    ("GET", "/ws"),
];

pub(super) async fn measure(request: Request, next: Next) -> Response {
    let started = std::time::Instant::now();
    let route = request.extensions().get::<MatchedPath>();
    let observation = crate::server_performance::begin_http(
        request.method().as_str(),
        route.map_or("<unregistered>", MatchedPath::as_str),
        started,
    );
    let mut response = next.run(request).await;
    let elapsed = started.elapsed();
    observation.finish(elapsed);
    if let Ok(value) =
        HeaderValue::from_str(&format!("app;dur={:.3}", elapsed.as_secs_f64() * 1000.0))
    {
        response.headers_mut().insert("server-timing", value);
    }
    response
}

pub(super) async fn snapshot(State(state): State<WorkerState>, headers: HeaderMap) -> Response {
    if !internal_authorized(&headers, &state) {
        return StatusCode::UNAUTHORIZED.into_response();
    }
    static READER: tokio::sync::Semaphore = tokio::sync::Semaphore::const_new(1);
    let Ok(permit) = READER.try_acquire() else {
        return StatusCode::SERVICE_UNAVAILABLE.into_response();
    };
    match tokio::task::spawn_blocking(move || {
        let _permit = permit;
        serde_json::to_vec(&crate::server_performance::snapshot())
    })
    .await
    {
        Ok(Ok(body)) => (
            [
                ("cache-control", "no-store"),
                ("content-type", "application/json"),
            ],
            body,
        )
            .into_response(),
        _ => StatusCode::SERVICE_UNAVAILABLE.into_response(),
    }
}
