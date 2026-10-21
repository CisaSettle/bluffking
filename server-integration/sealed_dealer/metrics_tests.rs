use super::*;
use axum::{
    body::{to_bytes, Body},
    http::Request,
};
use std::sync::atomic::{AtomicUsize, Ordering};
use tower::ServiceExt;

fn state() -> WorkerState {
    WorkerState {
        game_verifier: Arc::new(
            authorization::verifier(&authorization::game_authority().public_key()).unwrap(),
        ),
        internal_token: Arc::from("private-dealer-test-token"),
        hands: Arc::new(RwLock::new(HashMap::new())),
        commits: Arc::new(RwLock::new(HashMap::new())),
        hole_connections: Arc::new(Semaphore::new(1)),
    }
}

#[tokio::test]
async fn dealer_metrics_snapshot_auth_and_http_error_timing() {
    let app = worker_router(state());
    for token in [None, Some("wrong-token")] {
        let mut request = Request::builder().uri("/internal/performance");
        if let Some(token) = token {
            request = request.header(INTERNAL_TOKEN_HEADER, token);
        }
        let response = app
            .clone()
            .oneshot(request.body(Body::empty()).unwrap())
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
        assert!(response.headers()["server-timing"]
            .to_str()
            .unwrap()
            .starts_with("app;dur="));
        assert!(to_bytes(response.into_body(), 1024)
            .await
            .unwrap()
            .is_empty());
    }
    let mut signed = reqwest::Client::new()
        .get("http://dealer.invalid/internal/performance")
        .header(INTERNAL_TOKEN_HEADER, "private-dealer-test-token")
        .build()
        .unwrap();
    authorization::sign(&mut signed).unwrap();
    let mut request = Request::builder().uri("/internal/performance");
    for (name, value) in signed.headers() {
        request = request.header(name.as_str(), value.as_bytes());
    }
    let response = app
        .oneshot(request.body(Body::empty()).unwrap())
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(response.headers()["cache-control"], "no-store");
    let bytes = to_bytes(response.into_body(), 512 * 1024).await.unwrap();
    let snapshot: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
    assert_eq!(snapshot["process_id"], std::process::id());
    assert!(!snapshot["process_instance_id"].as_str().unwrap().is_empty());
    assert!(!String::from_utf8_lossy(&bytes).contains("private-dealer-test-token"));
    for row in snapshot["routes"].as_array().unwrap() {
        for private_key in ["ticket", "cards", "hand_id", "seed", "user_id"] {
            assert!(row.get(private_key).is_none());
        }
    }
}

#[tokio::test]
async fn dealer_metrics_delivery_commit_and_cancellation_retain_timer() {
    let state = state();
    let hand_id = Uuid::new_v4();
    let (hand, _) = SealedHand::new(hand_id, vec![0, 1]).unwrap();
    let delivery = HoleDelivery {
        hand_id,
        seat: 0,
        cards: hand.hole_cards(0).unwrap(),
        commitment: hand.commitment.clone(),
    };
    state.hands.write().await.insert(hand_id, hand);
    for cancelled in [true, false] {
        let count = Arc::new(AtomicUsize::new(0));
        let (_, mut timer) = ticket_observation(
            Some(r#"{"ticket":"existing-ticket"}"#),
            false,
            std::time::Instant::now(),
        );
        timer.track_drop(count.clone());
        let lock = state.hands.write().await;
        let mut complete = Box::pin(complete_delivery(&state, &delivery, timer));
        assert!(futures::poll!(complete.as_mut()).is_pending());
        assert_eq!(
            count.load(Ordering::SeqCst),
            0,
            "awaiting delivery commit still owns the receipt timer"
        );
        assert!(!lock[&hand_id].delivered_seats.contains(&0));
        if cancelled {
            drop(complete);
            assert!(
                !lock[&hand_id].delivered_seats.contains(&0),
                "cancellation must not fabricate a committed delivery"
            );
            drop(lock);
        } else {
            drop(lock);
            complete.await;
            assert!(state.hands.read().await[&hand_id]
                .delivered_seats
                .contains(&0));
        }
        assert_eq!(count.load(Ordering::SeqCst), 1);
    }
}

#[test]
fn dealer_metrics_binary_malformed_and_expired_ticket_are_observed_once() {
    for (raw, expired) in [
        (None, false),
        (Some("{"), false),
        (Some(r#"{"ticket":"expired"}"#), true),
    ] {
        let count = Arc::new(AtomicUsize::new(0));
        let (auth, mut timer) = ticket_observation(raw, expired, std::time::Instant::now());
        timer.track_drop(count.clone());
        assert!(auth.is_none());
        assert_eq!(count.load(Ordering::SeqCst), 0);
        drop(timer);
        assert_eq!(count.load(Ordering::SeqCst), 1);
    }
}
