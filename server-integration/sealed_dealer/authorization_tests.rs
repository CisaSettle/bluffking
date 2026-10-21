//! Security acceptance uses the actual router without another runtime or DB.
use super::*;
use axum::body::{to_bytes, Body};
use tower::ServiceExt;

const TOKEN: &str = "authorization-test-token";

fn state_with_key(key: [u8; 32]) -> WorkerState {
    WorkerState {
        game_verifier: Arc::new(authorization::verifier(&key).unwrap()),
        internal_token: Arc::from(TOKEN),
        hands: Arc::new(RwLock::new(HashMap::new())),
        commits: Arc::new(RwLock::new(HashMap::new())),
        hole_connections: Arc::new(Semaphore::new(MAX_HOLE_CONNECTIONS)),
    }
}

async fn fixture() -> (WorkerState, Uuid, CreateHandRequest, CreateHandResponse) {
    let state = state_with_key(authorization::game_authority().public_key());
    let id = Uuid::new_v4();
    let (hand, _) = SealedHand::new(id, vec![0, 3]).unwrap();
    let response = hand.create_response();
    state.hands.write().await.insert(id, hand);
    let request = CreateHandRequest {
        hand_id: id,
        seats: vec![0, 3],
        bot_seats: vec![],
        client_seeds: BTreeMap::new(),
        seed_commit: None,
        test_seed: None,
    };
    (state, id, request, response)
}

fn request(method: &str, path: &str, body: serde_json::Value, signed: bool) -> reqwest::Request {
    let mut request = reqwest::Client::new()
        .request(
            method.parse().unwrap(),
            format!("http://dealer.invalid{path}"),
        )
        .header(INTERNAL_TOKEN_HEADER, TOKEN)
        .json(&body)
        .build()
        .unwrap();
    if signed {
        authorization::sign(&mut request).unwrap();
    }
    request
}

async fn send(state: &WorkerState, request: reqwest::Request) -> axum::response::Response {
    let mut builder = axum::http::Request::builder()
        .method(request.method().as_str())
        .uri(request.url().path());
    for (key, value) in request.headers() {
        builder = builder.header(key.as_str(), value.as_bytes());
    }
    let body = request
        .body()
        .and_then(|body| body.as_bytes())
        .unwrap_or_default();
    worker_router(state.clone())
        .oneshot(builder.body(Body::from(body.to_vec())).unwrap())
        .await
        .unwrap()
}

async fn fingerprint(
    state: &WorkerState,
    id: Uuid,
) -> (DealerStage, Vec<u8>, String, Vec<(String, u8)>) {
    let hands = state.hands.read().await;
    let hand = hands.get(&id).unwrap();
    let mut tickets: Vec<_> = hand.tickets.iter().map(|(t, s)| (t.clone(), *s)).collect();
    tickets.sort_unstable();
    (
        hand.stage,
        hand.deck.clone(),
        hand.commitment.clone(),
        tickets,
    )
}

#[tokio::test]
async fn internal_token_alone_cannot_retrieve_or_advance_live_secrets() {
    let (state, id, creation, deal) = fixture().await;
    let before = fingerprint(&state, id).await;
    let operations = [
        (
            "POST",
            format!("/internal/hands/{id}/settle"),
            serde_json::json!({"commitment":deal.commitment}),
        ),
        (
            "POST",
            format!("/internal/hands/{id}/board"),
            serde_json::to_value(RevealBoardRequest {
                street: engine::Street::Flop,
            })
            .unwrap(),
        ),
        (
            "POST",
            format!("/internal/hands/{id}/showdown"),
            serde_json::json!({"seats":[0,3]}),
        ),
        (
            "POST",
            "/internal/hands".into(),
            serde_json::to_value(creation).unwrap(),
        ),
        (
            "POST",
            format!("/internal/hands/{id}/bots/0"),
            serde_json::json!({}),
        ),
        (
            "GET",
            format!("/internal/hands/{id}/deliveries/0"),
            serde_json::json!({}),
        ),
        (
            "DELETE",
            format!("/internal/hands/{id}"),
            serde_json::json!({}),
        ),
    ];
    for (method, path, body) in operations {
        let response = send(&state, request(method, &path, body, false)).await;
        assert_eq!(
            response.status(),
            StatusCode::UNAUTHORIZED,
            "{method} {path}"
        );
        let bytes = to_bytes(response.into_body(), 4096).await.unwrap();
        assert!(
            bytes.is_empty(),
            "unauthorized response exposes no cards, seeds or tickets"
        );
        assert_eq!(
            fingerprint(&state, id).await,
            before,
            "{method} {path} mutated the hand"
        );
    }
}

#[tokio::test]
async fn signatures_bind_key_method_path_and_body_without_mutating_live_hand() {
    let (state, id, _, deal) = fixture().await;
    let path = format!("/internal/hands/{id}/settle");
    let body = serde_json::json!({"commitment":deal.commitment});
    let before = fingerprint(&state, id).await;

    let mut invalid = request("POST", &path, body.clone(), true);
    invalid
        .headers_mut()
        .insert("x-sealed-game-signature", "00".repeat(64).parse().unwrap());
    assert_eq!(
        send(&state, invalid).await.status(),
        StatusCode::UNAUTHORIZED
    );

    let mut changed_body = request("POST", &path, body.clone(), true);
    *changed_body.body_mut() = Some(
        serde_json::to_vec(&serde_json::json!({"commitment":"forged"}))
            .unwrap()
            .into(),
    );
    assert_eq!(
        send(&state, changed_body).await.status(),
        StatusCode::UNAUTHORIZED
    );

    let mut changed_path = request("POST", &path, body.clone(), true);
    changed_path
        .url_mut()
        .set_path(&format!("/internal/hands/{id}/showdown"));
    assert_eq!(
        send(&state, changed_path).await.status(),
        StatusCode::UNAUTHORIZED
    );

    let mut changed_method = request("POST", &path, body.clone(), true);
    *changed_method.method_mut() = reqwest::Method::DELETE;
    // Use a path with both POST and DELETE handlers so routing cannot mask the
    // method-signature assertion with a 405 response.
    changed_method.url_mut().set_path("/internal/hands");
    authorization::sign(&mut changed_method).unwrap();
    *changed_method.method_mut() = reqwest::Method::POST;
    assert_eq!(
        send(&state, changed_method).await.status(),
        StatusCode::UNAUTHORIZED
    );

    let wrong_provider = mental_poker::crypto_real::Ed25519SignatureProvider::new()
        .with_signer("other", &[0x93; 32]);
    let wrong_key: [u8; 32] = hex::decode(wrong_provider.verifying_key_hex("other").unwrap())
        .unwrap()
        .try_into()
        .unwrap();
    let wrong_state = state_with_key(wrong_key);
    assert_eq!(
        send(&wrong_state, request("POST", &path, body, true))
            .await
            .status(),
        StatusCode::UNAUTHORIZED
    );
    assert_eq!(fingerprint(&state, id).await, before);
}

#[tokio::test]
async fn completion_is_bound_to_deck_commitment_and_retries_return_same_deck() {
    let (state, id, _, deal) = fixture().await;
    let path = format!("/internal/hands/{id}/settle");
    let before = fingerprint(&state, id).await;
    assert_eq!(
        send(
            &state,
            request(
                "POST",
                &path,
                serde_json::json!({"commitment":"another-hand"}),
                true
            )
        )
        .await
        .status(),
        StatusCode::CONFLICT
    );
    assert_eq!(fingerprint(&state, id).await, before);

    // Obtain completion from a real engine fold-around before issuing the
    // trusted game process command. The dealer does not accept bearer-token
    // assertions that the same state transition happened.
    let mut finished = blind_heads_up();
    finished
        .apply_action(engine::PlayerId(1), engine::PlayerAction::Fold)
        .unwrap();
    assert_eq!(finished.finish_blind().unwrap().board.count(), 0);

    let mut decks = Vec::new();
    for _ in 0..2 {
        let response = send(
            &state,
            request(
                "POST",
                &path,
                serde_json::json!({"commitment":deal.commitment}),
                true,
            ),
        )
        .await;
        assert_eq!(response.status(), StatusCode::OK);
        let bytes = to_bytes(response.into_body(), 16384).await.unwrap();
        let deck: FinalDeckResponse = serde_json::from_slice(&bytes).unwrap();
        validate_final_deck(id, &deal.commitment, &deck).unwrap();
        decks.push(serde_json::to_value(deck).unwrap());
    }
    assert_eq!(decks[0], decks[1]);
    assert_eq!(
        state.hands.read().await.get(&id).unwrap().stage,
        DealerStage::Settled
    );
}

fn blind_heads_up() -> engine::GameHand {
    let mut hand = engine::GameHand::new_blind(
        vec![
            (engine::PlayerId(1), engine::Chips(100), 0),
            (engine::PlayerId(2), engine::Chips(100), 3),
        ],
        0,
        engine::Chips(20),
        engine::Chips(10),
        [0; 32],
    );
    hand.start().unwrap();
    hand
}

#[tokio::test]
async fn active_or_unresolved_hand_cannot_produce_completion_request() {
    // This invalid URL has no HTTP server. Engine precondition errors must be
    // returned before transport, rather than a DNS/connection/retry error.
    let client = SealedDealerClient::new("not a URL".to_owned(), TOKEN.to_owned());
    let mut active = blind_heads_up();
    let before = active.snapshot();
    let error = client
        .finish_and_settle(Uuid::new_v4(), "commit", &mut active)
        .await
        .unwrap_err();
    assert!(error.contains("NotDone"), "{error}");
    assert_eq!(active.snapshot().pot, before.pot);
    assert_eq!(active.snapshot().current_actor, before.current_actor);

    let mut contested = blind_heads_up();
    contested
        .apply_action(engine::PlayerId(1), engine::PlayerAction::AllIn)
        .unwrap();
    contested
        .apply_action(engine::PlayerId(2), engine::PlayerAction::Call)
        .unwrap();
    let cards: Vec<_> = (0..5)
        .map(|id| mental_poker::card_id::id_to_card(id).unwrap())
        .collect();
    let mut offset = 0;
    while let Some(street) = contested.pending_board_street() {
        let count = if street == engine::Street::Flop { 3 } else { 1 };
        contested
            .inject_board_for_street(&cards[offset..offset + count])
            .unwrap();
        offset += count;
    }
    assert!(contested.is_done());
    let error = client
        .finish_and_settle(Uuid::new_v4(), "commit", &mut contested)
        .await
        .unwrap_err();
    assert!(error.contains("MissingReveal"), "{error}");
}

#[test]
fn legitimate_preflop_fold_win_completes_without_showdown_or_board() {
    let mut hand = blind_heads_up();
    hand.apply_action(engine::PlayerId(1), engine::PlayerAction::Fold)
        .unwrap();
    let result = hand.finish_blind().unwrap();
    assert_eq!(result.board.count(), 0);
    assert!(result.showdown.is_empty());
    assert_eq!(result.final_stacks.values().sum::<u32>(), 200);
}
