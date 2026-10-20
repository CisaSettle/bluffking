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

pub(crate) mod diagnostics;
mod metrics;

use crate::auth::constant_time::constant_time_eq;
use std::{
    collections::{BTreeMap, HashMap},
    net::SocketAddr,
    sync::{Arc, OnceLock},
    time::Duration,
};

use axum::{
    extract::{
        ws::{Message, WebSocket, WebSocketUpgrade},
        Path, RawQuery, State,
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
/// A socket with no VALID ticket for this long closes; the client reopens it
/// on the next hand. Long enough to span a typical wait between hands: a fresh
/// cross-border handshake is the fragile step (prod 2026-10-14), not an
/// established socket.
const HOLE_IDLE_TIMEOUT: Duration = Duration::from_secs(300);
/// Keeps NAT/proxy state of an idle socket alive. Never extends the deadline.
const HOLE_KEEPALIVE: Duration = Duration::from_secs(30);
/// Unknown tickets tolerated per socket before it is closed.
const HOLE_MAX_MISSES: u8 = 3;

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct SeatTicket {
    pub seat: u8,
    pub ticket: String,
}

/// Phase one of a verifiable deal (ADR-064 on the sealed dealer): the worker
/// draws `server_seed` and publishes only its hash before any player entropy
/// exists, so it cannot pick a deck after seeing the client seeds.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct CommitHandRequest {
    pub hand_id: Uuid,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct CommitHandResponse {
    pub seed_commit: [u8; 32],
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct CreateHandRequest {
    pub hand_id: Uuid,
    pub seats: Vec<u8>,
    #[serde(default)]
    pub bot_seats: Vec<u8>,
    /// Fresh per-hand player seeds collected after the commit, by seat. When a
    /// commit exists the deck is `Deck::new(from_seed_bytes(derive_deck_seed(
    /// server_seed, client_seeds, hand_id)))`, reproducible by players.
    #[serde(default)]
    pub client_seeds: BTreeMap<u8, [u8; 32]>,
    /// The commitment the game server obtained for this hand. When set, the
    /// worker must still hold that exact pending commit: a worker restarted
    /// between commit and deal has lost it and refuses the deal up front
    /// (`hand_not_committed`) instead of dealing an uncommitted deck that
    /// only fails verification at settlement, after a whole hand was played.
    #[serde(default)]
    pub seed_commit: Option<[u8; 32]>,
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
    /// Committed seed, released only at settlement. `None` for an uncommitted
    /// (test-pinned) deal.
    #[serde(default)]
    pub server_seed: Option<[u8; 32]>,
    #[serde(default)]
    pub client_seeds: BTreeMap<u8, [u8; 32]>,
}

fn final_deck(hand: &SealedHand) -> FinalDeckResponse {
    FinalDeckResponse {
        cards: hand.deck.clone(),
        seed: hand.seed,
        nonce: hand.nonce,
        server_seed: hand.server_seed,
        client_seeds: hand.client_seeds.clone(),
    }
}

/// Canonical card ids of the deck a seed shuffles to.
pub fn deck_ids(seed: [u8; 32]) -> Vec<u8> {
    Deck::new(&mut PokerRng::from_seed_bytes(seed))
        .cards()
        .iter()
        .copied()
        .map(mental_poker::card_id::card_to_id)
        .collect()
}

/// Settlement guard: every card the engine settled with (board and showdown
/// holes) must sit at its position in the final deck. `seats` is the sorted
/// deal order. A worker that returned different cards during play than the
/// deck it settles with fails here, before the result is accepted.
pub fn check_settled_cards(
    seats: &[u8],
    deck: &[u8],
    result: &engine::game::HandResult,
    seat_of: impl Fn(engine::PlayerId) -> Option<u8>,
) -> Result<(), String> {
    let n = seats.len();
    let id = mental_poker::card_id::card_to_id;
    for (i, card) in result.board.all_cards().into_iter().enumerate() {
        if deck.get(2 * n + i) != Some(&id(card)) {
            return Err("settled board does not match the sealed deck".into());
        }
    }
    for entry in &result.showdown {
        let p = seat_of(entry.player_id)
            .and_then(|seat| seats.iter().position(|s| *s == seat))
            .ok_or("showdown seat was not dealt")?;
        let mut want = [deck[p], deck[n + p]];
        let mut got = entry.hole_cards.as_array().map(id);
        want.sort_unstable();
        got.sort_unstable();
        if want != got {
            return Err("settled showdown cards do not match the sealed deck".into());
        }
    }
    Ok(())
}

/// The material players check after a verifiable hand. Validates the commit,
/// the seed derivation and the deck before anything is revealed or persisted.
pub fn verified_reveal(
    hand_id: Uuid,
    seed_commit: [u8; 32],
    deck: &FinalDeckResponse,
) -> Result<crate::pf_dealing::PfSecrets, String> {
    use crate::pf_dealing::{derive_deck_seed, seed_commit as commit_of, PfSecrets};
    let server_seed = deck.server_seed.ok_or("sealed deal was not committed")?;
    if commit_of(&server_seed) != seed_commit {
        return Err("sealed server seed does not match its commit".into());
    }
    let deck_seed = derive_deck_seed(&server_seed, &deck.client_seeds, hand_id);
    if deck_seed != deck.seed || deck_ids(deck_seed) != deck.cards {
        return Err("sealed deck does not follow from the revealed seeds".into());
    }
    Ok(PfSecrets {
        hand_id,
        server_seed,
        seed_commit,
        client_seeds: deck.client_seeds.clone(),
        deck_seed,
        dealt_seats: Vec::new(),
        withheld_seats: Vec::new(),
    })
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
    // Malformed optional diagnostics never reject an otherwise valid ticket.
    #[serde(default, deserialize_with = "diagnostics::deserialize_trace")]
    diagnostic_trace: Option<Uuid>,
}

#[derive(Serialize)]
struct ObservedHoleDelivery<'a> {
    #[serde(flatten)]
    delivery: &'a HoleDelivery,
    #[serde(skip_serializing_if = "Option::is_none")]
    diagnostic_trace: Option<Uuid>,
    /// Receipt of the text frame through lookup, before serialization/send.
    /// Does not include the handshake, network, send/backpressure or receipt ack.
    server_work_ms: f64,
    server_lookup_ms: f64,
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
    server_seed: Option<[u8; 32]>,
    client_seeds: BTreeMap<u8, [u8; 32]>,
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

    fn new_committed(
        hand_id: Uuid,
        seats: Vec<u8>,
        server_seed: [u8; 32],
        client_seeds: BTreeMap<u8, [u8; 32]>,
    ) -> Result<(Self, Vec<SeatTicket>), &'static str> {
        let deck_seed = crate::pf_dealing::derive_deck_seed(&server_seed, &client_seeds, hand_id);
        let (mut hand, tickets) =
            Self::new_with_rng(hand_id, seats, PokerRng::from_seed_bytes(deck_seed))?;
        hand.server_seed = Some(server_seed);
        hand.client_seeds = client_seeds;
        Ok((hand, tickets))
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
                server_seed: None,
                client_seeds: BTreeMap::new(),
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

/// A worker-held server seed and when it was drawn.
type PendingCommit = ([u8; 32], tokio::time::Instant);

/// Commits older than this without a deal are dropped (the game server voided
/// or crashed between the two phases).
const COMMIT_TTL: Duration = Duration::from_secs(120);

#[derive(Clone)]
struct WorkerState {
    internal_token: Arc<str>,
    hands: Arc<RwLock<HashMap<Uuid, SealedHand>>>,
    commits: Arc<RwLock<HashMap<Uuid, PendingCommit>>>,
    hole_connections: Arc<Semaphore>,
}

fn internal_authorized(headers: &HeaderMap, state: &WorkerState) -> bool {
    headers
        .get(INTERNAL_TOKEN_HEADER)
        .and_then(|value| value.to_str().ok())
        .is_some_and(|value| constant_time_eq(value.as_bytes(), state.internal_token.as_bytes()))
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
        if existing.seats != seats
            || existing.bot_seats != bot_seats
            || existing.client_seeds != request.client_seeds
            || request.seed_commit.is_some_and(|expected| {
                existing
                    .server_seed
                    .is_none_or(|seed| crate::pf_dealing::seed_commit(&seed) != expected)
            })
        {
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

    // A committed hand must be dealt from its committed seed. Without a commit
    // there is no player entropy to mix, so seeds are refused rather than
    // silently ignored.
    let committed = state
        .commits
        .write()
        .await
        .remove(&request.hand_id)
        .map(|(seed, _)| seed);
    if committed.is_none() && !request.client_seeds.is_empty() {
        return (
            StatusCode::CONFLICT,
            Json(serde_json::json!({"error":"hand_not_committed"})),
        )
            .into_response();
    }
    // The game server expects a committed deal: the pending commit must be the
    // one it announced to the players, not missing (worker restarted after the
    // commit) and not another hand's.
    if let Some(expected) = request.seed_commit {
        if committed.is_none_or(|seed| crate::pf_dealing::seed_commit(&seed) != expected) {
            return (
                StatusCode::CONFLICT,
                Json(serde_json::json!({"error":"hand_not_committed"})),
            )
                .into_response();
        }
    }
    if request
        .client_seeds
        .keys()
        .any(|seat| !seats.contains(seat) || bot_seats.contains(seat))
    {
        return StatusCode::BAD_REQUEST.into_response();
    }
    let fresh = |seats| match committed {
        Some(server_seed) => SealedHand::new_committed(
            request.hand_id,
            seats,
            server_seed,
            request.client_seeds.clone(),
        ),
        None => SealedHand::new(request.hand_id, seats),
    };
    #[cfg(any(test, feature = "test-support"))]
    let created = match request.test_seed {
        Some(seed) => SealedHand::new_with_rng(request.hand_id, seats, PokerRng::from_seed(seed)),
        None => fresh(seats),
    };
    #[cfg(not(any(test, feature = "test-support")))]
    let created = fresh(seats);
    let Ok((mut hand, _)) = created else {
        return StatusCode::BAD_REQUEST.into_response();
    };
    hand.bot_seats = bot_seats;
    let response = hand.create_response();
    hands.insert(request.hand_id, hand);
    (StatusCode::CREATED, Json(response)).into_response()
}

async fn commit_hand(
    State(state): State<WorkerState>,
    headers: HeaderMap,
    Json(request): Json<CommitHandRequest>,
) -> impl IntoResponse {
    if !internal_authorized(&headers, &state) {
        return StatusCode::UNAUTHORIZED.into_response();
    }
    if state.hands.read().await.contains_key(&request.hand_id) {
        return (
            StatusCode::CONFLICT,
            Json(serde_json::json!({"error":"hand_exists"})),
        )
            .into_response();
    }
    let now = tokio::time::Instant::now();
    let mut commits = state.commits.write().await;
    commits.retain(|_, (_, at)| now.duration_since(*at) < COMMIT_TTL);
    // A lost response is replayed with the same commit, never a new seed.
    let (server_seed, _) = *commits
        .entry(request.hand_id)
        .or_insert_with(|| (crate::pf_dealing::os_random_32(), now));
    Json(CommitHandResponse {
        seed_commit: crate::pf_dealing::seed_commit(&server_seed),
    })
    .into_response()
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
    state.commits.write().await.remove(&hand_id);
    StatusCode::NO_CONTENT
}

async fn hole_ws(
    State(state): State<WorkerState>,
    RawQuery(query): RawQuery,
    ws: WebSocketUpgrade,
) -> impl IntoResponse {
    let trace = diagnostics::query_trace(query.as_deref());
    let connection = Uuid::new_v4();
    let upgrade_started = std::time::Instant::now();
    diagnostics::record(connection, trace, "upgrade_requested", 0.0, 0.0);
    let Ok(permit) = state.hole_connections.clone().try_acquire_owned() else {
        diagnostics::record(connection, trace, "capacity_rejected", 0.0, 0.0);
        return StatusCode::SERVICE_UNAVAILABLE.into_response();
    };
    ws.max_message_size(512)
        .max_frame_size(512)
        .on_failed_upgrade(move |_| {
            let elapsed = elapsed_ms(upgrade_started);
            diagnostics::record(connection, trace, "upgrade_failed", elapsed, elapsed);
        })
        .on_upgrade(move |socket| async move {
            let _permit = permit;
            // Same-handler monotonic duration; excludes earlier proxy/TLS work.
            let elapsed = elapsed_ms(upgrade_started);
            diagnostics::record(connection, trace, "upgrade_complete", elapsed, elapsed);
            deliver_holes(socket, state, connection, trace).await;
        })
        .into_response()
}

/// One socket serves every hand of a seated player: each text frame is a
/// ticket, answered with that seat's hole cards (or an error frame). Opening a
/// fresh connection per hand cost a TCP + TLS + WebSocket handshake before
/// every deal.
async fn deliver_holes(
    mut socket: WebSocket,
    state: WorkerState,
    connection: Uuid,
    mut trace: Option<Uuid>,
) {
    let opened = std::time::Instant::now();
    let mut close_reason = "peer_closed";
    let mut tickets_seen = 0u64;
    let mut misses = 0u8;
    let mut closing_timer = None;
    // Absolute deadline: only a valid ticket extends it, never control frames,
    // unknown tickets or keepalives. A frame that was already buffered is not
    // processed after expiry, and every write is bounded by it too.
    let mut deadline = tokio::time::Instant::now() + HOLE_DELIVERY_TIMEOUT;
    let mut keepalive =
        tokio::time::interval_at(tokio::time::Instant::now() + HOLE_KEEPALIVE, HOLE_KEEPALIVE);
    let expired = |deadline| tokio::time::Instant::now() >= deadline;
    loop {
        let raw = tokio::select! {
            _ = keepalive.tick() => {
                if expired(deadline)
                    || !matches!(
                        tokio::time::timeout_at(deadline, socket.send(Message::Ping(Vec::new()))).await,
                        Ok(Ok(()))
                    )
                {
                    close_reason = "keepalive_write_failed";
                    break;
                }
                continue;
            }
            frame = tokio::time::timeout_at(deadline, socket.recv()) => match frame {
                Ok(Some(Ok(Message::Text(raw)))) => Some(raw),
                Ok(Some(Ok(Message::Binary(_)))) => None,
                Ok(Some(Err(_))) => { close_reason = "receive_error"; break; },
                _ if expired(deadline) => { close_reason = "ticket_deadline"; break; },
                Ok(Some(Ok(Message::Ping(_) | Message::Pong(_)))) => continue,
                _ => break,
            },
        };
        // No timer spans socket.recv(): client/keepalive waiting is excluded.
        let received = std::time::Instant::now();
        let (auth, timing) = ticket_observation(raw.as_deref(), expired(deadline), received);
        let Some(auth) = auth else {
            close_reason = "invalid_ticket_frame";
            closing_timer = Some(timing);
            break;
        };
        trace = auth.diagnostic_trace;
        tickets_seen = tickets_seen.saturating_add(1);
        if tickets_seen == 1 {
            diagnostics::record(
                connection,
                trace,
                "first_ticket_received",
                elapsed_ms(opened),
                0.0,
            );
        }
        diagnostics::record(
            connection,
            trace,
            "ticket_received",
            elapsed_ms(received),
            0.0,
        );
        let lookup_started = std::time::Instant::now();
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
        let lookup_ms = elapsed_ms(lookup_started);
        let Some(delivery) = delivery else {
            diagnostics::record(
                connection,
                trace,
                "unknown_ticket",
                elapsed_ms(received),
                lookup_ms,
            );
            misses += 1;
            if misses >= HOLE_MAX_MISSES
                || expired(deadline)
                || !matches!(
                    tokio::time::timeout_at(
                        deadline,
                        socket.send(Message::Text(r#"{"error":"unknown_ticket"}"#.into())),
                    )
                    .await,
                    Ok(Ok(()))
                )
            {
                close_reason = "unknown_ticket_limit_or_write_failed";
                closing_timer = Some(timing);
                break;
            }
            continue;
        };
        diagnostics::record(
            connection,
            trace,
            "lookup_complete",
            elapsed_ms(received),
            lookup_ms,
        );
        misses = 0;
        deadline = tokio::time::Instant::now() + HOLE_IDLE_TIMEOUT;
        let observed = ObservedHoleDelivery {
            delivery: &delivery,
            diagnostic_trace: trace,
            server_work_ms: elapsed_ms(received),
            server_lookup_ms: lookup_ms,
        };
        let Ok(json) = serde_json::to_string(&observed) else {
            close_reason = "serialize_failed";
            closing_timer = Some(timing);
            break;
        };
        let send_started = std::time::Instant::now();
        let sent = tokio::time::timeout_at(deadline, socket.send(Message::Text(json))).await;
        let outcome = match sent {
            Ok(Ok(())) => "send_complete",
            Ok(Err(_)) => "send_error",
            Err(_) => "send_deadline",
        };
        diagnostics::record(
            connection,
            trace,
            outcome,
            elapsed_ms(received),
            elapsed_ms(send_started),
        );
        if outcome != "send_complete" {
            close_reason = outcome;
            closing_timer = Some(timing);
            break;
        }
        complete_delivery(&state, &delivery, timing).await;
    }
    diagnostics::record(connection, trace, close_reason, elapsed_ms(opened), 0.0);
    let _ = tokio::time::timeout(Duration::from_secs(1), socket.close()).await;
    drop(closing_timer);
}

fn elapsed_ms(started: std::time::Instant) -> f64 {
    started.elapsed().as_secs_f64() * 1000.0
}

fn ticket_observation(
    raw: Option<&str>,
    expired: bool,
    received: std::time::Instant,
) -> (Option<TicketAuth>, crate::request_timing::WsTimer) {
    let auth = if expired {
        None
    } else {
        raw.and_then(|raw| serde_json::from_str(raw).ok())
    };
    let operation = if auth.is_some() {
        crate::server_performance::WsOperation::Crypto
    } else {
        crate::server_performance::WsOperation::Rejected
    };
    (
        auth,
        crate::request_timing::WsTimer::with_operation(
            Some(received),
            "/sealed-dealer/ws",
            operation,
        ),
    )
}

async fn complete_delivery(
    state: &WorkerState,
    delivery: &HoleDelivery,
    _timing: crate::request_timing::WsTimer,
) {
    // This flag is the authoritative answer to delivery_received. Keep the
    // same receipt timer through its lock wait and commit, also on cancellation.
    if let Some(hand) = state.hands.write().await.get_mut(&delivery.hand_id) {
        hand.delivered_seats.insert(delivery.seat);
    }
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
    // Initialize optional evidence infrastructure before accepting connections.
    diagnostics::record(Uuid::nil(), None, "worker_ready", 0.0, 0.0);
    let state = WorkerState {
        internal_token: Arc::from(internal_token),
        hands: Arc::new(RwLock::new(HashMap::new())),
        commits: Arc::new(RwLock::new(HashMap::new())),
        hole_connections: Arc::new(Semaphore::new(MAX_HOLE_CONNECTIONS)),
    };
    if let Err(error) =
        crate::server_performance::initialize(metrics::HTTP_ROUTES, &["/sealed-dealer/ws"])
    {
        tracing::error!(%error, "dealer performance catalog initialization failed");
    }
    let app = worker_router(state);
    let listener = tokio::net::TcpListener::bind(addr).await?;
    axum::serve(listener, app).await
}

fn worker_router(state: WorkerState) -> Router {
    Router::new()
        .route("/internal/readyz", get(internal_ready))
        .route("/internal/performance", get(metrics::snapshot))
        .route("/internal/hands", post(create_hand))
        .route("/internal/commits", post(commit_hand))
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
        .layer(axum::middleware::from_fn(metrics::measure))
        .with_state(state)
}

#[derive(Clone)]
pub struct SealedDealerClient {
    base_url: Arc<str>,
    internal_token: Arc<str>,
    http: reqwest::Client,
    metrics_http: reqwest::Client,
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
            metrics_http: reqwest::Client::builder()
                .timeout(Duration::from_secs(2))
                .redirect(reqwest::redirect::Policy::none())
                .no_proxy()
                .build()
                .expect("sealed dealer metrics client"),
        }
    }

    /// Optional admin read: one bounded request, no business retries or secret
    /// payloads. Preserve the worker's PID/instance rather than merging counters.
    pub(crate) async fn performance_snapshot(&self) -> Result<Vec<u8>, ()> {
        let read = async {
            let mut response = self
                .request(
                    self.metrics_http
                        .get(format!("{}/internal/performance", self.base_url)),
                )
                .send()
                .await
                .map_err(|_| ())?;
            if response.status() != reqwest::StatusCode::OK {
                return Err(());
            }
            const LIMIT: usize = 512 * 1024;
            let mut body = Vec::new();
            while let Some(chunk) = response.chunk().await.map_err(|_| ())? {
                if chunk.len() > LIMIT - body.len() {
                    return Err(());
                }
                body.extend_from_slice(&chunk);
            }
            // Only the authenticated dealer endpoint can supply this schema.
            let value: serde_json::Value = serde_json::from_slice(&body).map_err(|_| ())?;
            if value["process_id"].as_u64().is_none()
                || value["process_instance_id"].as_str().is_none()
                || value["routes"].as_array().is_none()
            {
                return Err(());
            }
            Ok(body)
        };
        tokio::time::timeout(Duration::from_secs(2), read)
            .await
            .map_err(|_| ())?
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

    pub async fn commit_hand(&self, hand_id: Uuid) -> Result<[u8; 32], String> {
        let response: CommitHandResponse = self
            .send_with_retry(
                self.request(
                    self.http
                        .post(format!("{}/internal/commits", self.base_url)),
                )
                .json(&CommitHandRequest { hand_id }),
            )
            .await?
            .json()
            .await
            .map_err(|e| e.to_string())?;
        Ok(response.seed_commit)
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

/// The outcome of the pre-deal seed exchange for a committed hand.
#[derive(Debug, Clone)]
pub struct SeedCommitInput {
    pub seed_commit: [u8; 32],
    pub client_seeds: BTreeMap<u8, [u8; 32]>,
    pub withheld_seats: Vec<u8>,
}

/// Per-hand capabilities. This never contains the undealt deck or human holes.
pub struct ActiveHand {
    pub deal: CreateHandResponse,
    /// The commit players saw and the exchange outcome; `None` for a pinned
    /// test deck, which is never revealed.
    commit: Option<SeedCommitInput>,
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
        commit: Option<SeedCommitInput>,
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
            commit,
        )
        .await
    }

    /// Phase one: obtain the dealer's seed commitment for `hand_id`.
    pub async fn commit(hand_id: Uuid) -> Result<[u8; 32], String> {
        #[cfg(any(test, feature = "test-support"))]
        ensure_test_worker().await?;
        client()?.commit_hand(hand_id).await
    }

    #[allow(clippy::too_many_arguments)]
    async fn create_with(
        hand_id: Uuid,
        hand: &engine::GameHand,
        bots: &std::collections::HashSet<u8>,
        users: &HashMap<Uuid, u8>,
        client: SealedDealerClient,
        endpoint: String,
        _test_seed: Option<u64>,
        commit: Option<SeedCommitInput>,
    ) -> Result<Self, String> {
        #[cfg(any(test, feature = "test-support"))]
        let pinned = _test_seed.is_some();
        #[cfg(not(any(test, feature = "test-support")))]
        let pinned = false;
        let commit = commit.filter(|_| !pinned);
        let client_seeds = commit
            .as_ref()
            .map(|c| c.client_seeds.clone())
            .unwrap_or_default();
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
                client_seeds,
                seed_commit: commit.as_ref().map(|c| c.seed_commit),
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
            commit,
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
            if users.get(&uid) != Some(&seat)
                || crate::runtime_checkpoint::replaced_during_hand(seat)
            {
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

    /// Settle the hand. Also returns the verified commit-reveal secrets that
    /// players use to recompute the deck (`None` for a pinned test deck).
    pub async fn finish(
        &self,
        hand: &mut engine::GameHand,
    ) -> Result<
        (
            engine::game::HandResult,
            Option<crate::pf_dealing::PfSecrets>,
        ),
        String,
    > {
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
        check_settled_cards(&self.seats, &deck.cards, &result, |pid| {
            hand.seats()
                .iter()
                .find(|s| s.player_id == pid)
                .map(|s| s.seat)
        })?;
        let pf = match &self.commit {
            Some(commit) => {
                let mut pf = verified_reveal(self.hand_id, commit.seed_commit, &deck)?;
                if pf.client_seeds != commit.client_seeds {
                    return Err("sealed dealer mixed different player seeds".into());
                }
                pf.dealt_seats = self.seats.clone();
                pf.withheld_seats = commit.withheld_seats.clone();
                Some(pf)
            }
            None => None,
        };
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
        Ok((result, pf))
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
/// A fixture pinned this session's next deck: it is dealt without the
/// commit-reveal exchange and never revealed.
#[cfg(any(test, feature = "test-support"))]
pub fn next_deck_is_pinned(session_id: Uuid) -> bool {
    TEST_HAND_SEEDS
        .get_or_init(Default::default)
        .lock()
        .unwrap()
        .contains_key(&session_id)
}

#[cfg(not(any(test, feature = "test-support")))]
pub fn next_deck_is_pinned(_session_id: Uuid) -> bool {
    false
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

#[cfg(test)]
mod tests;

#[cfg(test)]
mod metrics_tests;
