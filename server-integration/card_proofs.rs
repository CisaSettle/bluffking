//! Shared card-proof preparation and private receipt handling. Encryption and
//! proof generation run on bounded blocking workers, never in a room actor or
//! wallet transaction. Only commitments are included in live state frames.
use mental_poker::card_proof::{self as proof, Commitment, Context, Game, LockedDeck, Secret};
use rand::{rngs::OsRng, RngCore};
use serde::{Deserialize, Serialize};
use std::{
    collections::{BTreeMap, HashMap, VecDeque},
    sync::{Mutex, OnceLock},
};
use uuid::Uuid;

struct Prepared {
    master: [u8; 32],
    deck: LockedDeck,
}
#[derive(Default)]
struct Queue {
    pending: bool,
    decks: VecDeque<Prepared>,
}
/// Ready decks kept per game, so tables of one game that deal together do not
/// wait for each other's preparation.
const READY_DECKS: usize = 2;
fn queues() -> &'static Mutex<HashMap<Game, Queue>> {
    static QUEUES: OnceLock<Mutex<HashMap<Game, Queue>>> = OnceLock::new();
    QUEUES.get_or_init(Mutex::default)
}
/// At most one preparation and two user-requested proofs consume CPU at once.
static PREPARATION: tokio::sync::Semaphore = tokio::sync::Semaphore::const_new(1);
static PROOFS: tokio::sync::Semaphore = tokio::sync::Semaphore::const_new(2);

#[derive(Clone, Serialize, Deserialize)]
pub struct Deal {
    pub secret: Secret,
    pub commitment: Commitment,
    /// Native physical identities in forward deck order; never sent to clients.
    pub cards: Vec<u16>,
    #[serde(default)]
    pub commitments: Option<BTreeMap<u8, String>>,
}

/// A miss asks the existing room scheduler to retry. No random/shuffle fallback.
pub fn take(game: Game, players: Vec<(u8, Option<Uuid>)>) -> Option<Deal> {
    let hand_id = Uuid::now_v7();
    let identity: Vec<u8> = players
        .iter()
        .flat_map(|(seat, user)| {
            let mut bytes = vec![*seat, u8::from(user.is_some())];
            bytes.extend_from_slice(user.unwrap_or_default().as_bytes());
            bytes
        })
        .collect();
    let roster = hex::encode(mental_poker::hash::ds_hash(
        "cards:roster:v1",
        &[hand_id.as_bytes(), &identity],
    ));
    take_context(Context {
        game,
        hand_id,
        seats: players.iter().map(|(seat, _)| *seat).collect(),
        roster,
        epoch: 0,
        previous: None,
        subset: None,
    })
}
/// One preparation at a time per process (PREPARATION); keeps preparing until
/// the game holds READY_DECKS decks, then clears `pending`.
async fn refill(game: Game) {
    loop {
        let Ok(permit) = PREPARATION.acquire().await else {
            return;
        };
        let prepared = tokio::task::spawn_blocking(move || {
            let mut master = [0; 32];
            OsRng.fill_bytes(&mut master);
            let context = Context {
                game,
                hand_id: Uuid::now_v7(),
                seats: vec![0],
                roster: "0".repeat(64),
                epoch: 0,
                previous: None,
                subset: None,
            };
            LockedDeck::new(context, &master)
                .ok()
                .map(|deck| Prepared { master, deck })
        })
        .await;
        drop(permit);
        let Ok(mut queues) = queues().lock() else {
            return;
        };
        let queue = queues.entry(game).or_default();
        if let Ok(Some(deck)) = prepared {
            queue.decks.push_back(deck);
            if queue.decks.len() < READY_DECKS {
                continue;
            }
        }
        // Full, or a failed preparation: the next take starts a new refill.
        queue.pending = false;
        return;
    }
}
pub fn take_context(context: Context) -> Option<Deal> {
    let game = context.game;
    context.validate().ok()?;
    let mut map = queues().lock().ok()?;
    let queue = map.entry(game).or_default();
    let prepared = queue.decks.pop_front();
    if !queue.pending && queue.decks.len() < READY_DECKS {
        queue.pending = true;
        tokio::spawn(refill(game));
    }
    drop(map);
    let mut prepared = prepared?;
    prepared.deck.rebind(context.clone()).ok()?;
    let client_seeds = BTreeMap::new();
    Some(Deal {
        commitments: None,
        cards: prepared.deck.cards(&client_seeds).ok()?,
        commitment: prepared.deck.commitment(),
        secret: Secret {
            context,
            master: prepared.master,
            client_seeds,
        },
    })
}

/// Private authorization persisted atomically with the hand. These grants are
/// derived from original participant identities and the game's disclosure rules.
#[derive(Clone, Serialize, Deserialize)]
pub struct Receipt {
    pub scheme: String,
    pub secret: Secret,
    pub commitment: Commitment,
    pub public: Vec<usize>,
    pub grants: BTreeMap<Uuid, Vec<usize>>,
    #[serde(default)]
    pub commitments: Option<BTreeMap<u8, String>>,
}
impl Receipt {
    pub fn new(deal: &Deal, public: Vec<usize>, grants: BTreeMap<Uuid, Vec<usize>>) -> Self {
        Self {
            scheme: proof::SCHEME.into(),
            secret: deal.secret.clone(),
            commitment: deal.commitment.clone(),
            public,
            grants,
            commitments: deal.commitments.clone(),
        }
    }
    pub fn link(&self) -> serde_json::Value {
        serde_json::json!({"commitment": self.commitment, "url": format!("/api/history/cards-fairness?game={}&id={}", self.secret.context.game.id(), self.secret.context.hand_id)})
    }
}

#[derive(Deserialize)]
pub struct Query {
    game: String,
    id: Uuid,
}

pub async fn handler(
    axum::extract::State(state): axum::extract::State<crate::app_state::AppState>,
    auth: crate::auth::middleware::AuthUser,
    axum::extract::Query(query): axum::extract::Query<Query>,
) -> axum::response::Response {
    use axum::{http::StatusCode, response::IntoResponse, Json};
    let decision = crate::rate_limit::charge_account(crate::rate_limit::Principal::account(
        crate::rate_limit::AccountAction::HandFairness,
        auth.user_id.as_bytes(),
    ));
    if !decision.allowed {
        return crate::rate_limit::account_refusal(&decision);
    }
    let Some(game) = Game::parse(&query.game) else {
        return StatusCode::BAD_REQUEST.into_response();
    };
    let Ok(permit) = PROOFS.try_acquire() else {
        return crate::admission::refusal(
            StatusCode::TOO_MANY_REQUESTS,
            "verifier_busy",
            2,
            "The card proof verifier is busy. Please retry shortly.",
        );
    };
    let stored: Result<Option<serde_json::Value>, _> = sqlx::query_scalar(
        "SELECT receipt FROM card_proof_receipts WHERE game=$1 AND id=$2 AND $3=ANY(participants)",
    )
    .bind(game.id())
    .bind(query.id)
    .bind(auth.user_id)
    .fetch_optional(&state.pool)
    .await;
    let receipt: Receipt = match stored {
        Ok(Some(value)) => match serde_json::from_value(value) {
            Ok(value) => value,
            Err(_) => return StatusCode::NOT_FOUND.into_response(),
        },
        Ok(None) => return StatusCode::NOT_FOUND.into_response(),
        Err(error) => {
            tracing::error!(%error, "card proof receipt read failed");
            return StatusCode::SERVICE_UNAVAILABLE.into_response();
        }
    };
    if receipt.scheme != proof::SCHEME
        || receipt.secret.context.game != game
        || receipt.secret.context.hand_id != query.id
    {
        return StatusCode::NOT_FOUND.into_response();
    }
    let Some(own) = receipt.grants.get(&auth.user_id) else {
        return StatusCode::NOT_FOUND.into_response();
    };
    let positions: Vec<_> = receipt.public.iter().chain(own).copied().collect();
    // The permit lives inside the worker: cancelling the HTTP request cannot
    // release capacity while its uncancellable CPU work is still running.
    let result = tokio::task::spawn_blocking(move || {
        let _permit = permit;
        let deck = LockedDeck::new(receipt.secret.context, &receipt.secret.master)?;
        if deck.commitment() != receipt.commitment {
            return Err(proof::Error::Commitment);
        }
        let mut bundle = deck.bundle(&receipt.secret.client_seeds, &positions, &mut OsRng)?;
        bundle.commitments = receipt.commitments;
        Ok(bundle)
    })
    .await;
    match result {
        Ok(Ok(bundle)) => (
            [(axum::http::header::CACHE_CONTROL, "private, no-store")],
            Json(bundle),
        )
            .into_response(),
        _ => StatusCode::INTERNAL_SERVER_ERROR.into_response(),
    }
}

/// The caller's checkpoint/settlement transaction makes the proof durable at
/// the same boundary as the cards and wallet. Conflicts never replace a seed.
pub async fn save_on(
    conn: &mut crate::db_writer::PgConnection,
    record: Uuid,
    receipt: &Receipt,
) -> sqlx::Result<()> {
    let participants: Vec<_> = receipt.grants.keys().copied().collect();
    if participants.is_empty() {
        return Ok(());
    }
    let value = serde_json::to_value(receipt).map_err(|e| sqlx::Error::Encode(Box::new(e)))?;
    let inserted = sqlx::query("INSERT INTO card_proof_receipts(id,game,record_id,participants,receipt) VALUES($1,$2,$3,$4,$5) ON CONFLICT(id) DO NOTHING")
        .bind(receipt.secret.context.hand_id).bind(receipt.secret.context.game.id()).bind(record)
        .bind(&participants).bind(&value).execute(conn.writer()).await?.rows_affected();
    if inserted == 0 {
        let identical: bool = sqlx::query_scalar("SELECT game=$2 AND record_id=$3 AND participants=$4 AND receipt-'public'=($5::jsonb)-'public' AND (receipt->'public') @> ($5::jsonb->'public') FROM card_proof_receipts WHERE id=$1")
            .bind(receipt.secret.context.hand_id).bind(receipt.secret.context.game.id()).bind(record)
            .bind(&participants).bind(&value).fetch_one(conn.writer()).await?;
        if !identical {
            return Err(sqlx::Error::Protocol("card_proof_receipt_conflict".into()));
        }
    }
    Ok(())
}
/// Only the room's validated ShowCards transition calls this. Keep the seed,
/// commitment, participants and private grants immutable; public openings grow.
pub async fn disclose_on(
    conn: &mut crate::db_writer::PgConnection,
    receipt: &Receipt,
) -> sqlx::Result<()> {
    let value: serde_json::Value =
        sqlx::query_scalar("SELECT receipt FROM card_proof_receipts WHERE id=$1")
            .bind(receipt.secret.context.hand_id)
            .fetch_one(conn.writer())
            .await?;
    let mut stored: Receipt =
        serde_json::from_value(value).map_err(|e| sqlx::Error::Decode(Box::new(e)))?;
    if stored.commitment != receipt.commitment
        || serde_json::to_value(&stored.secret).ok() != serde_json::to_value(&receipt.secret).ok()
        || receipt
            .public
            .iter()
            .any(|p| *p >= receipt.secret.context.size())
    {
        return Err(sqlx::Error::Protocol("card_proof_receipt_conflict".into()));
    }
    stored.public.extend(receipt.public.iter().copied());
    stored.public.sort_unstable();
    stored.public.dedup();
    let value = serde_json::to_value(stored).map_err(|e| sqlx::Error::Encode(Box::new(e)))?;
    sqlx::query("UPDATE card_proof_receipts SET receipt=$2 WHERE id=$1")
        .bind(receipt.secret.context.hand_id)
        .bind(value)
        .execute(conn.writer())
        .await?;
    Ok(())
}
pub async fn save_value_on(
    conn: &mut crate::db_writer::PgConnection,
    record: Uuid,
    value: Option<&serde_json::Value>,
) -> sqlx::Result<()> {
    let Some(value) = value.filter(|v| v["scheme"] == proof::SCHEME) else {
        return Ok(());
    };
    let receipt: Receipt =
        serde_json::from_value(value.clone()).map_err(|e| sqlx::Error::Decode(Box::new(e)))?;
    save_on(conn, record, &receipt).await
}

impl std::fmt::Debug for Deal {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Deal")
            .field("commitment", &self.commitment)
            .finish_non_exhaustive()
    }
}
impl std::fmt::Debug for Receipt {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Receipt")
            .field("commitment", &self.commitment)
            .finish_non_exhaustive()
    }
}

#[derive(Deserialize)]
pub struct ListQuery {
    game: String,
}
pub async fn list(
    axum::extract::State(state): axum::extract::State<crate::app_state::AppState>,
    auth: crate::auth::middleware::AuthUser,
    axum::extract::Query(query): axum::extract::Query<ListQuery>,
) -> axum::response::Response {
    use axum::{http::StatusCode, response::IntoResponse, Json};
    let Some(game) = Game::parse(&query.game) else {
        return StatusCode::BAD_REQUEST.into_response();
    };
    let rows: Result<Vec<serde_json::Value>, _> = sqlx::query_scalar(
        "SELECT jsonb_build_object('commitment',receipt->'commitment','record_id',record_id,'created_at',created_at) FROM card_proof_receipts WHERE game=$1 AND participants @> ARRAY[$2]::uuid[] ORDER BY created_at DESC,id DESC LIMIT 30",
    ).bind(game.id()).bind(auth.user_id).fetch_all(&state.pool).await;
    match rows {
        Ok(rows) => Json(serde_json::json!({"hands":rows})).into_response(),
        Err(_) => StatusCode::SERVICE_UNAVAILABLE.into_response(),
    }
}

/// Historical/game APIs get only the public link, never a serialized Secret.
pub fn public_value(value: Option<&serde_json::Value>) -> Option<serde_json::Value> {
    let value = value?;
    if value["scheme"] != proof::SCHEME {
        return Some(value.clone());
    }
    serde_json::from_value::<Receipt>(value.clone())
        .ok()
        .map(|r| {
            let mut link = r.link();
            link["kind"] = serde_json::json!("cards");
            link
        })
}
