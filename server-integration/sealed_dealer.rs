//! Process-isolated sealed dealer for ordinary all-human Hold'em tables.
//!
//! The game server receives only an opaque hand id, a deck commitment and
//! per-seat bearer tickets when a hand is created. Hole cards travel directly
//! from this worker to the owning browser over the worker's WebSocket endpoint;
//! they never cross the game-server process. Public board cards and contested
//! showdown cards are released only when the authoritative game loop reaches
//! those stages. The full deck can be released after settlement for owner-only
//! history/coach persistence.
//!
//! This is intentionally a single trusted dealer, not Mental Poker: players do
//! not shuffle, exchange key shares or participate in threshold decryption.

use std::{
    collections::HashMap,
    net::SocketAddr,
    sync::{Arc, OnceLock},
    time::Duration,
};

use axum::{
    extract::{
        ws::{Message, WebSocket, WebSocketUpgrade},
        Path, State,
    },
    http::{HeaderMap, StatusCode},
    response::IntoResponse,
    routing::{delete, get, post},
    Json, Router,
};
use base64::{engine::general_purpose::URL_SAFE_NO_PAD, Engine as _};
use engine::{Deck, PokerRng};
use rand::RngCore;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use tokio::sync::{RwLock, Semaphore};
use uuid::Uuid;

const INTERNAL_TOKEN_HEADER: &str = "x-sealed-dealer-token";
const DEALER_REQUEST_ATTEMPTS: usize = 3;
/// A seated human keeps one socket open across hands, so this bounds seated
/// humans on the dealer, not deals per second.
const MAX_HOLE_CONNECTIONS: usize = 8192;
/// A new socket must present its first ticket within this.
const HOLE_DELIVERY_TIMEOUT: Duration = Duration::from_secs(5);
/// A socket with no ticket for this long closes; the client reopens it on the
/// next hand.
const HOLE_IDLE_TIMEOUT: Duration = Duration::from_secs(120);
/// Unknown tickets tolerated per socket before it is closed.
const HOLE_MAX_MISSES: u8 = 3;

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct SeatTicket {
    pub seat: u8,
    pub ticket: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct CreateHandRequest {
    pub hand_id: Uuid,
    pub seats: Vec<u8>,
    #[serde(default)]
    pub bot_seats: Vec<u8>,
    #[cfg(any(test, feature = "test-support"))]
    #[serde(default)]
    pub test_seed: Option<u64>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct CreateHandResponse {
    pub commitment: String,
    pub deck_seed_tag: [u8; 32],
    pub tickets: Vec<SeatTicket>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct RevealBoardRequest {
    pub street: engine::Street,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct RevealCardsResponse {
    pub cards: Vec<u8>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct RevealShowdownRequest {
    pub seats: Vec<u8>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct RevealedSeat {
    pub seat: u8,
    pub cards: [u8; 2],
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct RevealShowdownResponse {
    pub seats: Vec<RevealedSeat>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct FinalDeckResponse {
    pub cards: Vec<u8>,
    pub seed: [u8; 32],
    pub nonce: [u8; 32],
}

fn final_deck(hand: &SealedHand) -> FinalDeckResponse {
    FinalDeckResponse {
        cards: hand.deck.clone(),
        seed: hand.seed,
        nonce: hand.nonce,
    }
}

pub fn validate_final_deck(
    hand_id: Uuid,
    commitment: &str,
    deck: &FinalDeckResponse,
) -> Result<(), String> {
    if deck.cards.len() != 52
        || deck.cards.iter().any(|card| *card >= 52)
        || deck
            .cards
            .iter()
            .copied()
            .collect::<std::collections::HashSet<_>>()
            .len()
            != 52
    {
        return Err("sealed recovery deck is invalid".to_string());
    }
    let mut actual = Sha256::new();
    actual.update(b"bluffking:sealed-deck:v1");
    actual.update(hand_id.as_bytes());
    actual.update(deck.seed);
    actual.update(deck.nonce);
    actual.update(&deck.cards);
    if hex::encode(actual.finalize()) != commitment {
        return Err("sealed recovery commitment mismatch".to_string());
    }
    Ok(())
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct HoleDelivery {
    pub hand_id: Uuid,
    pub seat: u8,
    pub cards: [u8; 2],
    pub commitment: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct TicketAuth {
    ticket: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
enum DealerStage {
    Preflop,
    Flop,
    Turn,
    River,
    Showdown,
    Settled,
}

#[derive(Debug)]
struct SealedHand {
    seats: Vec<u8>,
    deck: Vec<u8>,
    seed: [u8; 32],
    nonce: [u8; 32],
    commitment: String,
    tickets: HashMap<String, u8>,
    bot_seats: Vec<u8>,
    delivered_seats: std::collections::HashSet<u8>,
    stage: DealerStage,
    showdown_seats: Option<Vec<u8>>,
}

impl SealedHand {
    fn create_response(&self) -> CreateHandResponse {
        let mut tickets: Vec<_> = self
            .tickets
            .iter()
            .map(|(ticket, &seat)| SeatTicket {
                seat,
                ticket: ticket.clone(),
            })
            .collect();
        tickets.sort_unstable_by_key(|ticket| ticket.seat);
        CreateHandResponse {
            commitment: self.commitment.clone(),
            deck_seed_tag: Sha256::digest(self.commitment.as_bytes()).into(),
            tickets,
        }
    }

    fn new(hand_id: Uuid, seats: Vec<u8>) -> Result<(Self, Vec<SeatTicket>), &'static str> {
        Self::new_with_rng(hand_id, seats, PokerRng::from_os())
    }
    fn new_with_rng(
        hand_id: Uuid,
        mut seats: Vec<u8>,
        mut rng: PokerRng,
    ) -> Result<(Self, Vec<SeatTicket>), &'static str> {
        seats.sort_unstable();
        seats.dedup();
        if seats.len() < 2 || seats.len() > 9 {
            return Err("sealed dealer requires 2..=9 distinct seats");
        }
        let seed = rng.seed();
        let deck: Vec<u8> = Deck::new(&mut rng)
            .cards()
            .iter()
            .copied()
            .map(mental_poker::card_id::card_to_id)
            .collect();
        let mut nonce = [0u8; 32];
        rand::rngs::OsRng.fill_bytes(&mut nonce);
        let mut commit = Sha256::new();
        commit.update(b"bluffking:sealed-deck:v1");
        commit.update(hand_id.as_bytes());
        commit.update(seed);
        commit.update(nonce);
        commit.update(&deck);
        let commitment = hex::encode(commit.finalize());

        let mut tickets = HashMap::with_capacity(seats.len());
        let mut response_tickets = Vec::with_capacity(seats.len());
        for &seat in &seats {
            let mut raw = [0u8; 32];
            rand::rngs::OsRng.fill_bytes(&mut raw);
            let ticket = URL_SAFE_NO_PAD.encode(raw);
            tickets.insert(ticket.clone(), seat);
            response_tickets.push(SeatTicket { seat, ticket });
        }

        Ok((
            Self {
                seats,
                deck,
                seed,
                nonce,
                commitment,
                tickets,
                bot_seats: Vec::new(),
                delivered_seats: Default::default(),
                stage: DealerStage::Preflop,
                showdown_seats: None,
            },
            response_tickets,
        ))
    }

    fn hole_cards(&self, seat: u8) -> Option<[u8; 2]> {
        let n = self.seats.len();
        let index = self.seats.iter().position(|candidate| *candidate == seat)?;
        Some([*self.deck.get(index)?, *self.deck.get(index + n)?])
    }

    fn reveal_board(&mut self, street: engine::Street) -> Result<Vec<u8>, &'static str> {
        let n = self.seats.len();
        let (want, range) = match street {
            engine::Street::Flop => (DealerStage::Flop, (2 * n)..(2 * n + 3)),
            engine::Street::Turn if self.stage >= DealerStage::Flop => {
                (DealerStage::Turn, (2 * n + 3)..(2 * n + 4))
            }
            engine::Street::River if self.stage >= DealerStage::Turn => {
                (DealerStage::River, (2 * n + 4)..(2 * n + 5))
            }
            _ => return Err("board reveal is out of order"),
        };
        // A response may be lost after the mutation. Retrying a public street
        // returns the same committed cards without regressing the stage.
        self.stage = self.stage.max(want);
        Ok(self.deck[range].to_vec())
    }

    fn reveal_showdown(&mut self, mut seats: Vec<u8>) -> Result<Vec<RevealedSeat>, &'static str> {
        if self.stage < DealerStage::River {
            return Err("showdown is available only after the river");
        }
        seats.sort_unstable();
        seats.dedup();
        if seats.len() < 2 || seats.iter().any(|seat| !self.seats.contains(seat)) {
            return Err("showdown seats must be distinct dealt seats");
        }
        if self
            .showdown_seats
            .as_ref()
            .is_some_and(|previous| previous != &seats)
        {
            return Err("showdown seats cannot change after disclosure");
        }
        self.showdown_seats = Some(seats.clone());
        let mut out = Vec::with_capacity(seats.len());
        for seat in seats {
            let cards = self.hole_cards(seat).ok_or("showdown seat has no cards")?;
            out.push(RevealedSeat { seat, cards });
        }
        self.stage = self.stage.max(DealerStage::Showdown);
        Ok(out)
    }
}

#[derive(Clone)]
struct WorkerState {
    internal_token: Arc<str>,
    hands: Arc<RwLock<HashMap<Uuid, SealedHand>>>,
    hole_connections: Arc<Semaphore>,
}

fn internal_authorized(headers: &HeaderMap, state: &WorkerState) -> bool {
    headers
        .get(INTERNAL_TOKEN_HEADER)
        .and_then(|value| value.to_str().ok())
        .is_some_and(|value| constant_time_eq(value.as_bytes(), state.internal_token.as_bytes()))
}

/// Constant-time byte comparison so response timing can't recover the
/// internal token byte-by-byte.
fn constant_time_eq(a: &[u8], b: &[u8]) -> bool {
    if a.len() != b.len() {
        return false;
    }
    let mut diff = 0u8;
    for (x, y) in a.iter().zip(b.iter()) {
        diff |= x ^ y;
    }
    diff == 0
}

async fn internal_ready(State(state): State<WorkerState>, headers: HeaderMap) -> StatusCode {
    if internal_authorized(&headers, &state) {
        StatusCode::NO_CONTENT
    } else {
        StatusCode::UNAUTHORIZED
    }
}

async fn create_hand(
    State(state): State<WorkerState>,
    headers: HeaderMap,
    Json(request): Json<CreateHandRequest>,
) -> impl IntoResponse {
    if !internal_authorized(&headers, &state) {
        return (
            StatusCode::UNAUTHORIZED,
            Json(serde_json::json!({"error":"unauthorized"})),
        )
            .into_response();
    }
    let mut seats = request.seats;
    seats.sort_unstable();
    seats.dedup();
    let mut bot_seats = request.bot_seats;
    bot_seats.sort_unstable();
    bot_seats.dedup();
    if !(2..=9).contains(&seats.len()) || bot_seats.iter().any(|seat| !seats.contains(seat)) {
        return StatusCode::BAD_REQUEST.into_response();
    }

    // A successful create can lose its HTTP response. Replaying that request
    // must recover the original deal, never replace it or fail with hand_exists.
    // Keep lookup and insertion atomic, including concurrent retries.
    let mut hands = state.hands.write().await;
    if let Some(existing) = hands.get(&request.hand_id) {
        if existing.seats != seats || existing.bot_seats != bot_seats {
            return (
                StatusCode::CONFLICT,
                Json(serde_json::json!({"error":"hand_config_mismatch"})),
            )
                .into_response();
        }
        #[cfg(any(test, feature = "test-support"))]
        if request
            .test_seed
            .is_some_and(|seed| PokerRng::from_seed(seed).seed() != existing.seed)
        {
            return StatusCode::CONFLICT.into_response();
        }
        return (StatusCode::OK, Json(existing.create_response())).into_response();
    }

    #[cfg(any(test, feature = "test-support"))]
    let created = match request.test_seed {
        Some(seed) => SealedHand::new_with_rng(request.hand_id, seats, PokerRng::from_seed(seed)),
        None => SealedHand::new(request.hand_id, seats),
    };
    #[cfg(not(any(test, feature = "test-support")))]
    let created = SealedHand::new(request.hand_id, seats);
    let Ok((mut hand, _)) = created else {
        return StatusCode::BAD_REQUEST.into_response();
    };
    hand.bot_seats = bot_seats;
    let response = hand.create_response();
    hands.insert(request.hand_id, hand);
    (StatusCode::CREATED, Json(response)).into_response()
}

async fn reveal_board(
    State(state): State<WorkerState>,
    Path(hand_id): Path<Uuid>,
    headers: HeaderMap,
    Json(request): Json<RevealBoardRequest>,
) -> impl IntoResponse {
    if !internal_authorized(&headers, &state) {
        return StatusCode::UNAUTHORIZED.into_response();
    }
    let mut hands = state.hands.write().await;
    let Some(hand) = hands.get_mut(&hand_id) else {
        return StatusCode::NOT_FOUND.into_response();
    };
    match hand.reveal_board(request.street) {
        Ok(cards) => Json(RevealCardsResponse { cards }).into_response(),
        Err(reason) => (
            StatusCode::CONFLICT,
            Json(serde_json::json!({"error":reason})),
        )
            .into_response(),
    }
}

async fn reveal_showdown(
    State(state): State<WorkerState>,
    Path(hand_id): Path<Uuid>,
    headers: HeaderMap,
    Json(request): Json<RevealShowdownRequest>,
) -> impl IntoResponse {
    if !internal_authorized(&headers, &state) {
        return StatusCode::UNAUTHORIZED.into_response();
    }
    let mut hands = state.hands.write().await;
    let Some(hand) = hands.get_mut(&hand_id) else {
        return StatusCode::NOT_FOUND.into_response();
    };
    match hand.reveal_showdown(request.seats) {
        Ok(seats) => Json(RevealShowdownResponse { seats }).into_response(),
        Err(reason) => (
            StatusCode::CONFLICT,
            Json(serde_json::json!({"error":reason})),
        )
            .into_response(),
    }
}

async fn settle_hand(
    State(state): State<WorkerState>,
    Path(hand_id): Path<Uuid>,
    headers: HeaderMap,
) -> impl IntoResponse {
    if !internal_authorized(&headers, &state) {
        return StatusCode::UNAUTHORIZED.into_response();
    }
    let mut hands = state.hands.write().await;
    let Some(hand) = hands.get_mut(&hand_id) else {
        return StatusCode::NOT_FOUND.into_response();
    };
    // This internal command is sent only after the authoritative game engine has
    // completed the hand.  It intentionally works for both showdowns and
    // fold-arounds: after settlement the game server may persist each owner's
    // cards for private review/coach use, matching a physical cardroom opening
    // the stub after the pot has been awarded.
    hand.stage = DealerStage::Settled;
    Json(final_deck(hand)).into_response()
}

async fn delete_hand(
    State(state): State<WorkerState>,
    Path(hand_id): Path<Uuid>,
    headers: HeaderMap,
) -> impl IntoResponse {
    if !internal_authorized(&headers, &state) {
        return StatusCode::UNAUTHORIZED;
    }
    state.hands.write().await.remove(&hand_id);
    StatusCode::NO_CONTENT
}

async fn hole_ws(State(state): State<WorkerState>, ws: WebSocketUpgrade) -> impl IntoResponse {
    let Ok(permit) = state.hole_connections.clone().try_acquire_owned() else {
        return StatusCode::SERVICE_UNAVAILABLE.into_response();
    };
    ws.max_message_size(512)
        .max_frame_size(512)
        .on_upgrade(move |socket| async move {
            let _permit = permit;
            deliver_holes(socket, state).await;
        })
        .into_response()
}

/// One socket serves every hand of a seated player: each text frame is a
/// ticket, answered with that seat's hole cards (or an error frame). Opening a
/// fresh connection per hand cost a TCP + TLS + WebSocket handshake before
/// every deal.
async fn deliver_holes(mut socket: WebSocket, state: WorkerState) {
    let mut misses = 0u8;
    // Absolute deadlines: only a ticket extends them, never control frames.
    let mut deadline = tokio::time::Instant::now() + HOLE_DELIVERY_TIMEOUT;
    loop {
        let raw = match tokio::time::timeout_at(deadline, socket.recv()).await {
            Ok(Some(Ok(Message::Text(raw)))) => raw,
            Ok(Some(Ok(Message::Ping(_) | Message::Pong(_)))) => continue,
            _ => break,
        };
        deadline = tokio::time::Instant::now() + HOLE_IDLE_TIMEOUT;
        let Ok(auth) = serde_json::from_str::<TicketAuth>(&raw) else {
            break;
        };
        let delivery = {
            let hands = state.hands.read().await;
            hands.iter().find_map(|(hand_id, hand)| {
                let seat = *hand.tickets.get(&auth.ticket)?;
                let cards = hand.hole_cards(seat)?;
                Some(HoleDelivery {
                    hand_id: *hand_id,
                    seat,
                    cards,
                    commitment: hand.commitment.clone(),
                })
            })
        };
        let Some(delivery) = delivery else {
            misses += 1;
            if misses >= HOLE_MAX_MISSES
                || socket
                    .send(Message::Text(r#"{"error":"unknown_ticket"}"#.into()))
                    .await
                    .is_err()
            {
                break;
            }
            continue;
        };
        misses = 0;
        let Ok(json) = serde_json::to_string(&delivery) else {
            break;
        };
        if socket.send(Message::Text(json)).await.is_err() {
            break;
        }
        if let Some(hand) = state.hands.write().await.get_mut(&delivery.hand_id) {
            hand.delivered_seats.insert(delivery.seat);
        }
    }
    let _ = socket.close().await;
}

async fn delivery_received(
    State(state): State<WorkerState>,
    headers: HeaderMap,
    Path((hand_id, seat)): Path<(Uuid, u8)>,
) -> axum::response::Response {
    if !internal_authorized(&headers, &state) {
        return StatusCode::UNAUTHORIZED.into_response();
    }
    let hands = state.hands.read().await;
    let Some(hand) = hands.get(&hand_id) else {
        return StatusCode::NOT_FOUND.into_response();
    };
    if !hand.seats.contains(&seat) {
        return StatusCode::NOT_FOUND.into_response();
    }
    Json(hand.delivered_seats.contains(&seat)).into_response()
}

// Bots receive only their own cards, never the remaining deck or human holes.
async fn bot_hole(
    State(state): State<WorkerState>,
    headers: HeaderMap,
    Path((hand_id, seat)): Path<(Uuid, u8)>,
) -> axum::response::Response {
    if !internal_authorized(&headers, &state) {
        return StatusCode::UNAUTHORIZED.into_response();
    }
    let hands = state.hands.read().await;
    let Some(hand) = hands.get(&hand_id) else {
        return StatusCode::NOT_FOUND.into_response();
    };
    if !hand.bot_seats.contains(&seat) {
        return StatusCode::FORBIDDEN.into_response();
    }
    match hand.hole_cards(seat) {
        Some(cards) => Json(cards).into_response(),
        None => StatusCode::NOT_FOUND.into_response(),
    }
}

pub async fn run_worker(addr: SocketAddr, internal_token: String) -> std::io::Result<()> {
    let state = WorkerState {
        internal_token: Arc::from(internal_token),
        hands: Arc::new(RwLock::new(HashMap::new())),
        hole_connections: Arc::new(Semaphore::new(MAX_HOLE_CONNECTIONS)),
    };
    let app = Router::new()
        .route("/internal/readyz", get(internal_ready))
        .route("/internal/hands", post(create_hand))
        .route("/internal/hands/:hand_id/bots/:seat", post(bot_hole))
        .route(
            "/internal/hands/:hand_id/deliveries/:seat",
            get(delivery_received),
        )
        .route("/internal/hands/:hand_id/board", post(reveal_board))
        .route("/internal/hands/:hand_id/showdown", post(reveal_showdown))
        .route("/internal/hands/:hand_id/settle", post(settle_hand))
        .route("/internal/hands/:hand_id", delete(delete_hand))
        .route("/healthz", get(|| async { StatusCode::NO_CONTENT }))
        .route("/ws", get(hole_ws))
        .with_state(state);
    let listener = tokio::net::TcpListener::bind(addr).await?;
    axum::serve(listener, app).await
}

#[derive(Clone)]
pub struct SealedDealerClient {
    base_url: Arc<str>,
    internal_token: Arc<str>,
    http: reqwest::Client,
}

impl SealedDealerClient {
    pub fn new(base_url: impl Into<String>, internal_token: impl Into<String>) -> Self {
        Self {
            base_url: Arc::from(base_url.into().trim_end_matches('/')),
            internal_token: Arc::from(internal_token.into()),
            http: reqwest::Client::builder()
                .timeout(Duration::from_secs(2))
                .build()
                .expect("sealed dealer HTTP client"),
        }
    }

    fn request(&self, request: reqwest::RequestBuilder) -> reqwest::RequestBuilder {
        request.header(INTERNAL_TOKEN_HEADER, self.internal_token.as_ref())
    }

    async fn send_with_retry(
        &self,
        request: reqwest::RequestBuilder,
    ) -> Result<reqwest::Response, String> {
        let mut last_error = "sealed dealer request failed".to_string();
        for attempt in 0..DEALER_REQUEST_ATTEMPTS {
            let cloned = request
                .try_clone()
                .ok_or_else(|| "sealed dealer request is not replayable".to_string())?;
            match cloned.send().await {
                Ok(response) if response.status().is_success() => return Ok(response),
                Ok(response)
                    if response.status().is_server_error()
                        && attempt + 1 < DEALER_REQUEST_ATTEMPTS =>
                {
                    last_error = format!("sealed dealer returned {}", response.status());
                }
                Ok(response) => return response.error_for_status().map_err(|e| e.to_string()),
                Err(error) if attempt + 1 < DEALER_REQUEST_ATTEMPTS => {
                    last_error = error.to_string();
                }
                Err(error) => return Err(error.to_string()),
            }
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
        Err(last_error)
    }

    pub async fn create_hand(
        &self,
        request: &CreateHandRequest,
    ) -> Result<CreateHandResponse, String> {
        self.send_with_retry(
            self.request(self.http.post(format!("{}/internal/hands", self.base_url)))
                .json(request),
        )
        .await?
        .json()
        .await
        .map_err(|e| e.to_string())
    }

    pub async fn delivery_received(&self, hand_id: Uuid, seat: u8) -> Result<bool, String> {
        self.send_with_retry(self.request(self.http.get(format!(
            "{}/internal/hands/{hand_id}/deliveries/{seat}",
            self.base_url
        ))))
        .await?
        .json()
        .await
        .map_err(|e| e.to_string())
    }

    pub async fn bot_hole(&self, hand_id: Uuid, seat: u8) -> Result<[u8; 2], String> {
        let response = self
            .send_with_retry(self.request(self.http.post(format!(
                "{}/internal/hands/{hand_id}/bots/{seat}",
                self.base_url
            ))))
            .await?;
        response.json().await.map_err(|e| e.to_string())
    }

    pub async fn reveal_board(
        &self,
        hand_id: Uuid,
        street: engine::Street,
    ) -> Result<Vec<u8>, String> {
        let response: RevealCardsResponse = self
            .send_with_retry(
                self.request(
                    self.http
                        .post(format!("{}/internal/hands/{hand_id}/board", self.base_url)),
                )
                .json(&RevealBoardRequest { street }),
            )
            .await?
            .json()
            .await
            .map_err(|e| e.to_string())?;
        Ok(response.cards)
    }

    pub async fn reveal_showdown(
        &self,
        hand_id: Uuid,
        seats: Vec<u8>,
    ) -> Result<Vec<RevealedSeat>, String> {
        let response: RevealShowdownResponse = self
            .send_with_retry(
                self.request(self.http.post(format!(
                    "{}/internal/hands/{hand_id}/showdown",
                    self.base_url
                )))
                .json(&RevealShowdownRequest { seats }),
            )
            .await?
            .json()
            .await
            .map_err(|e| e.to_string())?;
        Ok(response.seats)
    }

    pub async fn settle(&self, hand_id: Uuid) -> Result<FinalDeckResponse, String> {
        self.send_with_retry(
            self.request(
                self.http
                    .post(format!("{}/internal/hands/{hand_id}/settle", self.base_url)),
            ),
        )
        .await?
        .json()
        .await
        .map_err(|e| e.to_string())
    }

    pub async fn delete_hand(&self, hand_id: Uuid) {
        let _ = self
            .request(
                self.http
                    .delete(format!("{}/internal/hands/{hand_id}", self.base_url)),
            )
            .send()
            .await;
    }
}

static CLIENT: OnceLock<SealedDealerClient> = OnceLock::new();
static PUBLIC_WS_ENDPOINT: OnceLock<Arc<str>> = OnceLock::new();

pub fn client() -> Result<&'static SealedDealerClient, &'static str> {
    CLIENT.get().ok_or("sealed dealer is not configured")
}

pub fn public_ws_endpoint() -> Result<&'static str, &'static str> {
    PUBLIC_WS_ENDPOINT
        .get()
        .map(AsRef::as_ref)
        .ok_or("sealed dealer public websocket is not configured")
}

#[cfg(test)]
pub(crate) fn install_for_test(base_url: String, token: String, endpoint: String) {
    CLIENT
        .set(SealedDealerClient::new(base_url, token))
        .unwrap_or_else(|_| panic!("sealed dealer test client installed once"));
    PUBLIC_WS_ENDPOINT
        .set(Arc::from(endpoint))
        .expect("sealed dealer test endpoint installed once");
}

/// Configure an externally managed dealer, or launch the default sibling
/// process on loopback. The returned child guard must live as long as the main
/// server; dropping it kills only the worker launched by this process.
pub async fn configure_or_launch() -> Result<Option<tokio::process::Child>, String> {
    let public_endpoint = std::env::var("SEALED_DEALER_PUBLIC_WS_ENDPOINT")
        .unwrap_or_else(|_| "/sealed-dealer/ws".to_string());

    if let Ok(base_url) = std::env::var("SEALED_DEALER_URL") {
        let token = std::env::var("SEALED_DEALER_INTERNAL_TOKEN")
            .map_err(|_| "SEALED_DEALER_INTERNAL_TOKEN is required with SEALED_DEALER_URL")?;
        CLIENT
            .set(SealedDealerClient::new(base_url, token))
            .map_err(|_| "sealed dealer configured twice")?;
        PUBLIC_WS_ENDPOINT
            .set(Arc::from(public_endpoint))
            .map_err(|_| "sealed dealer public endpoint configured twice")?;
        return Ok(None);
    }

    let addr: SocketAddr = std::env::var("SEALED_DEALER_ADDR")
        .unwrap_or_else(|_| "127.0.0.1:3012".to_string())
        .parse()
        .map_err(|e| format!("invalid SEALED_DEALER_ADDR: {e}"))?;
    let mut raw_token = [0u8; 32];
    rand::rngs::OsRng.fill_bytes(&mut raw_token);
    let worker_credential = URL_SAFE_NO_PAD.encode(raw_token);
    let executable = std::env::current_exe().map_err(|e| e.to_string())?;
    let mut command = tokio::process::Command::new(executable);
    command
        .arg("--sealed-dealer-worker")
        .env("SEALED_DEALER_ADDR", addr.to_string())
        .env("SEALED_DEALER_INTERNAL_TOKEN", &worker_credential)
        .kill_on_drop(true);
    let mut child = command
        .spawn()
        .map_err(|e| format!("start sealed dealer: {e}"))?;
    let client_addr = if addr.ip().is_unspecified() {
        SocketAddr::from(([127, 0, 0, 1], addr.port()))
    } else {
        addr
    };
    let base_url = format!("http://{client_addr}");
    // A surviving worker from an earlier server can still answer /healthz.
    // Prove this is the worker we launched, using its fresh internal token.
    let health_url = format!("{base_url}/internal/readyz");
    let http = reqwest::Client::builder()
        .timeout(Duration::from_secs(1))
        .build()
        .map_err(|e| e.to_string())?;
    let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
    loop {
        if let Some(status) = child.try_wait().map_err(|e| e.to_string())? {
            return Err(format!(
                "sealed dealer exited before becoming ready at {addr}: {status}"
            ));
        }
        if http
            .get(&health_url)
            .header(INTERNAL_TOKEN_HEADER, &worker_credential)
            .send()
            .await
            .is_ok_and(|response| response.status().is_success())
        {
            if let Some(status) = child.try_wait().map_err(|e| e.to_string())? {
                return Err(format!(
                    "sealed dealer exited before becoming ready at {addr}: {status}"
                ));
            }
            break;
        }
        if tokio::time::Instant::now() >= deadline {
            return Err(format!("sealed dealer did not become ready at {addr}"));
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    CLIENT
        .set(SealedDealerClient::new(base_url, worker_credential))
        .map_err(|_| "sealed dealer configured twice")?;
    PUBLIC_WS_ENDPOINT
        .set(Arc::from(public_endpoint))
        .map_err(|_| "sealed dealer public endpoint configured twice")?;
    Ok(Some(child))
}

#[cfg(test)]
mod tests {
    use super::*;
    use futures::{SinkExt, StreamExt};

    #[test]
    fn sealed_hand_reveals_only_in_stage_order() {
        let hand_id = Uuid::new_v4();
        let (mut hand, tickets) = SealedHand::new(hand_id, vec![0, 1, 2]).expect("valid hand");
        assert_eq!(tickets.len(), 3);
        assert!(hand.reveal_board(engine::Street::Turn).is_err());
        let flop = hand.reveal_board(engine::Street::Flop).expect("flop");
        assert_eq!(hand.reveal_board(engine::Street::Flop).unwrap(), flop);
        assert!(hand.reveal_board(engine::Street::River).is_err());
        let turn = hand.reveal_board(engine::Street::Turn).expect("turn");
        assert_eq!(hand.reveal_board(engine::Street::Turn).unwrap(), turn);
        let river = hand.reveal_board(engine::Street::River).expect("river");
        assert_eq!((flop.len(), turn.len(), river.len()), (3, 1, 1));
        let shown = hand.reveal_showdown(vec![0, 2]).expect("showdown");
        assert_eq!(shown.len(), 2);
        assert_eq!(hand.reveal_showdown(vec![2, 0]).unwrap(), shown);
        assert!(
            hand.reveal_showdown(vec![0, 1]).is_err(),
            "retry cannot add a folded seat"
        );
        assert_eq!(hand.reveal_board(engine::Street::Flop).unwrap(), flop);
        assert_eq!(hand.stage, DealerStage::Showdown);
        let mut all = flop;
        all.extend(turn);
        all.extend(river);
        all.extend(shown.iter().flat_map(|entry| entry.cards));
        all.sort_unstable();
        all.dedup();
        assert_eq!(all.len(), 9, "revealed board and holes must be unique");
    }

    #[test]
    fn sealed_hand_accepts_sparse_seats_and_rejects_duplicates() {
        let id = Uuid::new_v4();
        let (hand, _) = SealedHand::new(id, vec![0, 2]).expect("sparse physical seats");
        assert!(hand.hole_cards(0).is_some());
        assert!(hand.hole_cards(2).is_some());
        assert!(hand.hole_cards(1).is_none());
        assert!(SealedHand::new(id, vec![0, 0]).is_err());
    }

    #[tokio::test]
    async fn worker_delivers_holes_directly_and_opens_deck_only_at_settlement() {
        let probe = std::net::TcpListener::bind("127.0.0.1:0").expect("ephemeral port");
        let addr = probe.local_addr().expect("local addr");
        drop(probe);
        let token = "test-internal-token".to_string();
        let worker = tokio::spawn(run_worker(addr, token.clone()));
        let http = reqwest::Client::new();
        let health = format!("http://{addr}/healthz");
        for _ in 0..50 {
            if http.get(&health).send().await.is_ok() {
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }

        // Public health alone cannot admit a worker with a stale token.
        let ready = format!("http://{addr}/internal/readyz");
        for stale_token in [None, Some("previous-worker-token")] {
            let mut request = http.get(&ready);
            if let Some(stale_token) = stale_token {
                request = request.header(INTERNAL_TOKEN_HEADER, stale_token);
            }
            assert_eq!(
                request.send().await.unwrap().status(),
                StatusCode::UNAUTHORIZED
            );
        }
        assert_eq!(
            http.get(&ready)
                .header(INTERNAL_TOKEN_HEADER, &token)
                .send()
                .await
                .unwrap()
                .status(),
            StatusCode::NO_CONTENT
        );

        let client = SealedDealerClient::new(format!("http://{addr}"), token);
        let (mut oversized, _) = tokio_tungstenite::connect_async(format!("ws://{addr}/ws"))
            .await
            .expect("connect unauthenticated socket");
        oversized
            .send(tokio_tungstenite::tungstenite::Message::Text(
                "x".repeat(513),
            ))
            .await
            .expect("send oversized ticket");
        let reply = tokio::time::timeout(Duration::from_secs(1), oversized.next())
            .await
            .expect("oversized ticket must close promptly");
        assert!(!matches!(
            reply,
            Some(Ok(tokio_tungstenite::tungstenite::Message::Text(_)))
        ));
        let (mut idle, _) = tokio_tungstenite::connect_async(format!("ws://{addr}/ws"))
            .await
            .expect("connect idle unauthenticated socket");
        let closed =
            tokio::time::timeout(HOLE_DELIVERY_TIMEOUT + Duration::from_secs(1), idle.next())
                .await
                .expect("idle connection must release its admission slot");
        assert!(!matches!(
            closed,
            Some(Ok(tokio_tungstenite::tungstenite::Message::Text(_)))
        ));
        let hand_id = Uuid::new_v4();
        let deal = client
            .create_hand(&CreateHandRequest {
                hand_id,
                seats: vec![0, 3],
                bot_seats: Vec::new(),
                #[cfg(any(test, feature = "test-support"))]
                test_seed: None,
            })
            .await
            .expect("create sealed hand");
        let retry_request = CreateHandRequest {
            hand_id,
            seats: vec![3, 0], // Request ordering does not change seat ownership.
            bot_seats: Vec::new(),
            #[cfg(any(test, feature = "test-support"))]
            test_seed: None,
        };
        let (retry_a, retry_b) = tokio::join!(
            client.create_hand(&retry_request),
            client.create_hand(&retry_request),
        );
        assert_eq!(retry_a.unwrap(), deal);
        assert_eq!(retry_b.unwrap(), deal);
        for conflicting in [
            CreateHandRequest {
                seats: vec![0, 2],
                ..retry_request.clone()
            },
            CreateHandRequest {
                bot_seats: vec![0],
                ..retry_request.clone()
            },
        ] {
            assert!(client.create_hand(&conflicting).await.is_err());
        }

        assert!(!client.delivery_received(hand_id, 3).await.unwrap());
        assert!(client.delivery_received(hand_id, 8).await.is_err());
        let ticket = deal
            .tickets
            .iter()
            .find(|ticket| ticket.seat == 3)
            .expect("seat ticket");
        let (mut socket, _) = tokio_tungstenite::connect_async(format!("ws://{addr}/ws"))
            .await
            .expect("connect direct dealer websocket");
        socket
            .send(tokio_tungstenite::tungstenite::Message::Text(
                serde_json::json!({"ticket": ticket.ticket}).to_string(),
            ))
            .await
            .expect("send ticket");
        let raw = socket
            .next()
            .await
            .expect("delivery frame")
            .expect("valid frame")
            .into_text()
            .expect("text frame");
        let delivery: HoleDelivery = serde_json::from_str(&raw).expect("hole delivery");
        assert_eq!((delivery.hand_id, delivery.seat), (hand_id, 3));
        assert_ne!(delivery.cards[0], delivery.cards[1]);
        assert_eq!(delivery.commitment, deal.commitment);
        assert!(
            client.delivery_received(hand_id, 3).await.unwrap(),
            "delivered player cannot demand a refund by reporting failure"
        );
        // The same socket serves the next hand; an unknown ticket gets an
        // error frame, and repeated unknown tickets close the socket.
        let next_hand = Uuid::new_v4();
        let next = client
            .create_hand(&CreateHandRequest {
                hand_id: next_hand,
                seats: vec![0, 3],
                bot_seats: Vec::new(),
                #[cfg(any(test, feature = "test-support"))]
                test_seed: None,
            })
            .await
            .expect("create second sealed hand");
        socket
            .send(tokio_tungstenite::tungstenite::Message::Text(
                serde_json::json!({"ticket": next.tickets.iter().find(|t| t.seat == 3).unwrap().ticket}).to_string(),
            ))
            .await
            .expect("send second ticket on the same socket");
        let raw = socket.next().await.unwrap().unwrap().into_text().unwrap();
        let second: HoleDelivery = serde_json::from_str(&raw).expect("second delivery");
        assert_eq!((second.hand_id, second.seat), (next_hand, 3));
        for n in 0..HOLE_MAX_MISSES {
            socket
                .send(tokio_tungstenite::tungstenite::Message::Text(
                    serde_json::json!({"ticket": "nope"}).to_string(),
                ))
                .await
                .expect("send unknown ticket");
            let reply = tokio::time::timeout(Duration::from_secs(1), socket.next())
                .await
                .expect("unknown ticket answered promptly");
            if n + 1 < HOLE_MAX_MISSES {
                let text = reply.unwrap().unwrap().into_text().unwrap();
                assert!(text.contains("unknown_ticket"));
            } else {
                assert!(!matches!(
                    reply,
                    Some(Ok(tokio_tungstenite::tungstenite::Message::Text(_)))
                ));
            }
        }
        client.delete_hand(next_hand).await;

        assert!(client
            .reveal_board(hand_id, engine::Street::Turn)
            .await
            .is_err());
        assert_eq!(
            client
                .reveal_board(hand_id, engine::Street::Flop)
                .await
                .expect("flop")
                .len(),
            3
        );
        // Even a delayed replay after the flop preserves delivery and street
        // state; the following turn would fail if creation reset this hand.
        assert_eq!(client.create_hand(&retry_request).await.unwrap(), deal);
        assert!(client.delivery_received(hand_id, 3).await.unwrap());
        client
            .reveal_board(hand_id, engine::Street::Turn)
            .await
            .expect("turn");
        client
            .reveal_board(hand_id, engine::Street::River)
            .await
            .expect("river");
        assert_eq!(
            client
                .reveal_showdown(hand_id, vec![0, 3])
                .await
                .expect("showdown")
                .len(),
            2
        );
        let opened = client.settle(hand_id).await.expect("settled deck");
        assert_eq!(opened.cards.len(), 52);
        let mut commitment = Sha256::new();
        commitment.update(b"bluffking:sealed-deck:v1");
        commitment.update(hand_id.as_bytes());
        commitment.update(opened.seed);
        commitment.update(opened.nonce);
        commitment.update(&opened.cards);
        assert_eq!(hex::encode(commitment.finalize()), deal.commitment);

        // Bot tables use opaque engine slots too. Only the bot's own cards can be fetched.
        let bot_hand_id = Uuid::new_v4();
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
        let active = ActiveHand::create_with(
            bot_hand_id,
            &hand,
            &std::collections::HashSet::from([3]),
            &HashMap::new(),
            client.clone(),
            format!("ws://{addr}/ws"),
            Some(3),
        )
        .await
        .unwrap();
        assert!(hand.seats().iter().all(|s| s.hole_cards().is_none()));
        assert_eq!(active.bot_holes.len(), 1);
        assert!(
            client.bot_hole(bot_hand_id, 0).await.is_err(),
            "human holes cannot be obtained through bot API"
        );
        let wire = serde_json::to_value(&active.deal).unwrap();
        assert!(wire.get("recovery").is_none());
        assert!(wire.get("cards").is_none());
        hand.apply_action(engine::PlayerId(1), engine::PlayerAction::AllIn)
            .unwrap();
        hand.apply_action(engine::PlayerId(2), engine::PlayerAction::Call)
            .unwrap();
        active.advance(&mut hand).await.unwrap();
        let result = active.finish(&mut hand).await.unwrap();
        assert_eq!(result.final_stacks.values().sum::<u32>(), 200);
        assert_eq!(result.board.count(), 5);
        assert_eq!(completed_holes(&result).len(), 2);
        active.delete().await;

        worker.abort();
    }

    /// A mid-hand joiner can bind a seat a fill bot is playing, or a seat a dealt
    /// human vacated. Neither seat's ticket may reach them: redeeming it would
    /// expose a live opponent's hole cards.
    #[tokio::test]
    async fn tickets_go_only_to_users_dealt_into_the_hand() {
        let mut hand = engine::GameHand::new_blind(
            vec![
                (engine::PlayerId(1), engine::Chips(100), 0),
                (engine::PlayerId(2), engine::Chips(100), 1),
                (engine::PlayerId(104), engine::Chips(100), 3),
            ],
            0,
            engine::Chips(20),
            engine::Chips(10),
            [0; 32],
        );
        hand.start().unwrap();
        let (alice, bob, carol, mallory) = (
            Uuid::new_v4(),
            Uuid::new_v4(),
            Uuid::new_v4(),
            Uuid::new_v4(),
        );
        let active = ActiveHand::create(
            Uuid::new_v4(),
            Uuid::new_v4(),
            &hand,
            &std::collections::HashSet::from([3]),
            &HashMap::from([(alice, 0), (bob, 1)]),
        )
        .await
        .expect("sealed hand");

        // Bob left mid-hand; Carol took his seat and Mallory took the bot seat.
        let live = HashMap::from([(alice, 0), (carol, 1), (mallory, 3)]);
        let mut receivers = HashMap::new();
        let mut senders = HashMap::new();
        for (conn_id, uid) in [alice, carol, mallory].into_iter().enumerate() {
            let (tx, rx) = tokio::sync::mpsc::channel(4);
            senders.insert(
                uid,
                crate::session_registry::WsSender {
                    conn_id: conn_id as u64,
                    tx,
                },
            );
            receivers.insert(uid, rx);
        }
        let conns = tokio::sync::Mutex::new(senders);
        active.sync_tickets(&live, &conns).await;

        let mut tickets_for = |uid: Uuid| {
            let rx = receivers.get_mut(&uid).unwrap();
            let mut seats = Vec::new();
            while let Ok(frame) = rx.try_recv() {
                let crate::outbound::Outbound::Text(json) = frame else {
                    continue;
                };
                let value: serde_json::Value = serde_json::from_str(&json).unwrap();
                if value["kind"] == "sealed_deal_ticket" {
                    seats.push(value["data"]["seat"].as_u64().unwrap());
                }
            }
            seats
        };
        assert_eq!(tickets_for(alice), vec![0]);
        assert!(
            tickets_for(carol).is_empty(),
            "replacement got Bob's ticket"
        );
        assert!(
            tickets_for(mallory).is_empty(),
            "joiner got the bot's ticket"
        );
        active.delete().await;
    }
}

/// Per-hand capabilities. This never contains the undealt deck or human holes.
pub struct ActiveHand {
    pub deal: CreateHandResponse,
    pub bot_holes: HashMap<u8, engine::HoleCards>,
    pub endpoint: String,
    pub hand_id: Uuid,
    sent_connections: tokio::sync::Mutex<HashMap<Uuid, u64>>,
    seats: Vec<u8>,
    /// Human user -> seat for the users actually dealt into this hand, taken
    /// when the hand is created. A mid-hand joiner who binds a bot-filled or
    /// vacated seat is absent here, so it never receives that seat's ticket.
    dealt_users: HashMap<Uuid, u8>,
    client: SealedDealerClient,
}

impl ActiveHand {
    pub async fn create(
        session_id: Uuid,
        hand_id: Uuid,
        hand: &engine::GameHand,
        bots: &std::collections::HashSet<u8>,
        users: &HashMap<Uuid, u8>,
    ) -> Result<Self, String> {
        #[cfg(any(test, feature = "test-support"))]
        ensure_test_worker().await?;
        #[cfg(any(test, feature = "test-support"))]
        let test_seed = take_test_seed(session_id);
        #[cfg(not(any(test, feature = "test-support")))]
        let test_seed = {
            let _ = session_id;
            None
        };
        Self::create_with(
            hand_id,
            hand,
            bots,
            users,
            client()?.clone(),
            public_ws_endpoint()?.to_string(),
            test_seed,
        )
        .await
    }

    async fn create_with(
        hand_id: Uuid,
        hand: &engine::GameHand,
        bots: &std::collections::HashSet<u8>,
        users: &HashMap<Uuid, u8>,
        client: SealedDealerClient,
        endpoint: String,
        _test_seed: Option<u64>,
    ) -> Result<Self, String> {
        let mut seats: Vec<u8> = hand.seats().iter().map(|s| s.seat).collect();
        seats.sort_unstable();
        let dealt_users = users
            .iter()
            .filter(|(_, seat)| seats.contains(seat) && !bots.contains(seat))
            .map(|(&uid, &seat)| (uid, seat))
            .collect();
        let deal = client
            .create_hand(&CreateHandRequest {
                hand_id,
                seats: seats.clone(),
                #[cfg(any(test, feature = "test-support"))]
                test_seed: _test_seed,
                bot_seats: bots.iter().copied().filter(|s| seats.contains(s)).collect(),
            })
            .await?;
        let mut bot_holes = HashMap::new();
        for seat in bots.iter().filter(|s| seats.contains(s)) {
            let cards = match client.bot_hole(hand_id, *seat).await.and_then(decode_hole) {
                Ok(cards) => cards,
                Err(e) => {
                    client.delete_hand(hand_id).await;
                    return Err(e);
                }
            };
            bot_holes.insert(*seat, cards);
        }
        Ok(Self {
            deal,
            bot_holes,
            endpoint,
            hand_id,
            seats,
            dealt_users,
            client,
            sent_connections: Default::default(),
        })
    }

    pub async fn sync_tickets(
        &self,
        users: &HashMap<Uuid, u8>,
        conns: &tokio::sync::Mutex<HashMap<Uuid, crate::session_registry::WsSender>>,
    ) {
        let recipients: Vec<_> = conns
            .lock()
            .await
            .iter()
            .map(|(&uid, c)| (uid, c.conn_id, c.tx.clone()))
            .collect();
        let mut sent = self.sent_connections.lock().await;
        for (uid, conn_id, tx) in recipients {
            if sent.get(&uid) == Some(&conn_id) {
                continue;
            }
            // Only a user dealt into this hand who still holds that seat gets a
            // ticket. The live map also holds mid-hand joiners bound to a
            // bot-filled or vacated seat; that seat's cards are not theirs.
            let Some(&seat) = self.dealt_users.get(&uid) else {
                continue;
            };
            if users.get(&uid) != Some(&seat) || crate::runtime_checkpoint::replaced_during_hand(seat) {
                continue;
            }
            let Some(ticket) = self.deal.tickets.iter().find(|t| t.seat == seat) else {
                continue;
            };
            let msg = crate::protocol::ServerMsg::SealedDealTicket {
                hand_id: self.hand_id,
                seat,
                ticket: ticket.ticket.clone(),
                endpoint: self.endpoint.clone(),
                commitment: self.deal.commitment.clone(),
            };
            if let Ok(json) = serde_json::to_string(&msg) {
                if crate::runtime_checkpoint::try_send(&tx, crate::outbound::Outbound::Text(json))
                    .is_ok()
                {
                    sent.insert(uid, conn_id);
                }
            }
        }
    }

    pub async fn can_void_delivery(&self, seat: u8) -> bool {
        self.seats.contains(&seat)
            && !self.bot_holes.contains_key(&seat)
            && self.client.delivery_received(self.hand_id, seat).await.ok() == Some(false)
    }

    pub async fn sync_solo_ticket(
        &self,
        seat: u8,
        slot: &tokio::sync::Mutex<Option<crate::session_registry::WsSender>>,
    ) {
        let Some((conn_id, tx)) = slot
            .lock()
            .await
            .as_ref()
            .map(|c| (c.conn_id, c.tx.clone()))
        else {
            return;
        };
        let mut sent = self.sent_connections.lock().await;
        if sent.get(&Uuid::nil()) == Some(&conn_id) {
            return;
        }
        let Some(ticket) = self.deal.tickets.iter().find(|t| t.seat == seat) else {
            return;
        };
        let msg = crate::protocol::ServerMsg::SealedDealTicket {
            hand_id: self.hand_id,
            seat,
            ticket: ticket.ticket.clone(),
            endpoint: self.endpoint.clone(),
            commitment: self.deal.commitment.clone(),
        };
        if let Ok(json) = serde_json::to_string(&msg) {
            if crate::runtime_checkpoint::try_send(&tx, crate::outbound::Outbound::Text(json))
                .is_ok()
            {
                sent.insert(Uuid::nil(), conn_id);
            }
        }
    }

    pub async fn advance(&self, hand: &mut engine::GameHand) -> Result<(), String> {
        while let Some(street) = hand.pending_board_street() {
            let ids = self.client.reveal_board(self.hand_id, street).await?;
            let cards: Option<Vec<_>> = ids
                .into_iter()
                .map(mental_poker::card_id::id_to_card)
                .collect();
            let cards = cards.ok_or("invalid sealed board")?;
            hand.inject_board_for_street(&cards)
                .map_err(|e| format!("{e:?}"))?;
        }
        Ok(())
    }

    pub async fn finish(
        &self,
        hand: &mut engine::GameHand,
    ) -> Result<engine::game::HandResult, String> {
        let contenders: Vec<_> = hand
            .seats()
            .iter()
            .filter(|s| !s.folded)
            .map(|s| (s.player_id, s.seat))
            .collect();
        if contenders.len() >= 2 {
            let reveals = self
                .client
                .reveal_showdown(self.hand_id, contenders.iter().map(|(_, s)| *s).collect())
                .await?;
            for reveal in reveals {
                let pid = contenders
                    .iter()
                    .find(|(_, s)| *s == reveal.seat)
                    .ok_or("invalid showdown seat")?
                    .0;
                hand.inject_showdown_reveal(pid, decode_hole(reveal.cards)?)
                    .map_err(|e| format!("{e:?}"))?;
            }
        }
        let mut result = hand.finish_blind().map_err(|e| format!("{e:?}"))?;
        // The hand is over. Only now may private review obtain the completed deck.
        let deck = self.client.settle(self.hand_id).await?;
        validate_final_deck(self.hand_id, &self.deal.commitment, &deck)?;
        result.deck_seed = deck.seed;
        for seat in hand.seats() {
            if result
                .showdown
                .iter()
                .any(|s| s.player_id == seat.player_id)
            {
                continue;
            }
            let i = self
                .seats
                .iter()
                .position(|s| *s == seat.seat)
                .ok_or("missing dealt seat")?;
            result.folded_hole_cards.insert(
                seat.player_id.inner(),
                decode_hole([deck.cards[i], deck.cards[i + self.seats.len()]])?,
            );
        }
        Ok(result)
    }

    pub async fn delete(&self) {
        self.client.delete_hand(self.hand_id).await;
    }
}

fn decode_hole(ids: [u8; 2]) -> Result<engine::HoleCards, String> {
    if ids[0] == ids[1] {
        return Err("duplicate hole card".into());
    }
    let a = mental_poker::card_id::id_to_card(ids[0]).ok_or("invalid hole card")?;
    let b = mental_poker::card_id::id_to_card(ids[1]).ok_or("invalid hole card")?;
    Ok(engine::HoleCards::new(a, b))
}

/// Private review material is assembled only after the hand finishes.
pub fn completed_holes(result: &engine::game::HandResult) -> HashMap<u64, [engine::Card; 2]> {
    let mut holes: HashMap<_, _> = result
        .folded_hole_cards
        .iter()
        .map(|(&pid, h)| (pid, h.as_array()))
        .collect();
    holes.extend(
        result
            .showdown
            .iter()
            .map(|s| (s.player_id.inner(), s.hole_cards.as_array())),
    );
    holes
}

#[cfg(any(test, feature = "test-support"))]
static TEST_HAND_SEEDS: OnceLock<std::sync::Mutex<HashMap<Uuid, u64>>> = OnceLock::new();

#[cfg(any(test, feature = "test-support"))]
pub fn set_next_hand_seed_for_test(session_id: Uuid, seed: u64) {
    TEST_HAND_SEEDS
        .get_or_init(Default::default)
        .lock()
        .unwrap()
        .insert(session_id, seed);
}
#[cfg(any(test, feature = "test-support"))]
fn take_test_seed(session_id: Uuid) -> Option<u64> {
    TEST_HAND_SEEDS
        .get_or_init(Default::default)
        .lock()
        .unwrap()
        .remove(&session_id)
}

/// Actor fixtures still exercise a real isolated dealer; no plaintext fallback.
#[cfg(any(test, feature = "test-support"))]
pub async fn ensure_test_worker() -> Result<(), String> {
    let client = CLIENT.get_or_init(|| {
        let probe = std::net::TcpListener::bind("127.0.0.1:0").expect("test dealer port");
        let addr = probe.local_addr().unwrap();
        drop(probe);
        let token = Uuid::new_v4().to_string();
        let worker_token = token.clone();
        std::thread::spawn(move || {
            tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .unwrap()
                .block_on(run_worker(addr, worker_token))
                .unwrap();
        });
        let _ = PUBLIC_WS_ENDPOINT.set(Arc::from(format!("ws://{addr}/ws")));
        SealedDealerClient::new(format!("http://{addr}"), token)
    });
    for _ in 0..50 {
        if client
            .http
            .get(format!("{}/healthz", client.base_url))
            .send()
            .await
            .is_ok_and(|r| r.status().is_success())
        {
            return Ok(());
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    Err("test sealed dealer did not start".into())
}
