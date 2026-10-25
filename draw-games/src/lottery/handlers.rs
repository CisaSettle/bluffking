//! Authenticated lottery HTTP endpoints; rule failures have stable wire codes.
use super::{scratch, store, Game};
use crate::{app_state::AppState, auth::middleware::AuthUser, handlers::respond::error_code};
use axum::{
    extract::{rejection::JsonRejection, Path, Query, State},
    http::StatusCode,
    response::{IntoResponse, Response},
    Json,
};
use serde::Deserialize;
use serde_json::{json, Value};
use uuid::Uuid;

#[derive(Debug)]
pub enum Error {
    InvalidEntry,
    SalesClosed,
    TooManyTickets,
    InsufficientBalance,
    RequestConflict,
    DailyAlreadyClaimed,
    BeansInUse,
    TicketNotFound,
    AlreadySettled,
    FairnessRequired,
    Proof(String),
    Db(sqlx::Error),
}
impl From<sqlx::Error> for Error {
    fn from(e: sqlx::Error) -> Self {
        Self::Db(e)
    }
}
impl IntoResponse for Error {
    fn into_response(self) -> Response {
        let (status, code) = match self {
            Self::InvalidEntry => (StatusCode::BAD_REQUEST, "invalid_entry"),
            Self::SalesClosed => (StatusCode::CONFLICT, "sales_closed"),
            Self::TooManyTickets => (StatusCode::CONFLICT, "too_many_tickets"),
            Self::InsufficientBalance => (StatusCode::CONFLICT, "insufficient_balance"),
            Self::RequestConflict => (StatusCode::CONFLICT, "request_conflict"),
            Self::DailyAlreadyClaimed => (StatusCode::CONFLICT, "daily_already_claimed"),
            Self::BeansInUse => (StatusCode::CONFLICT, "beans_in_use"),
            Self::TicketNotFound => (StatusCode::NOT_FOUND, "ticket_not_found"),
            Self::AlreadySettled => (StatusCode::CONFLICT, "already_settled"),
            Self::FairnessRequired => (StatusCode::CONFLICT, "fairness_required"),
            Self::Proof(reason) => {
                tracing::warn!(%reason, "lottery proof unavailable");
                (StatusCode::SERVICE_UNAVAILABLE, "proof_unavailable")
            }
            Self::Db(e) => {
                tracing::error!(error=%e, "lottery storage failed");
                (StatusCode::SERVICE_UNAVAILABLE, "storage_unavailable")
            }
        };
        error_code(status, code)
    }
}
fn game(name: &str) -> Result<Game, Error> {
    Game::parse(name).ok_or(Error::InvalidEntry)
}
pub async fn view(
    State(state): State<AppState>,
    auth: AuthUser,
    Path(name): Path<String>,
) -> Result<Json<Value>, Error> {
    Ok(Json(
        store::view(&state.pool, auth.user_id, game(&name)?).await?,
    ))
}
#[derive(Default, Deserialize)]
pub struct BuyQuery {
    #[serde(default)]
    receipt_only: bool,
}
pub async fn buy(
    State(state): State<AppState>,
    auth: AuthUser,
    Path(name): Path<String>,
    Query(query): Query<BuyQuery>,
    body: Result<Json<Value>, JsonRejection>,
) -> Result<Json<Value>, Error> {
    let game = game(&name)?;
    let Json(body) = body.map_err(|_| Error::InvalidEntry)?;
    let purchase = if game == Game::Scratch {
        let body = serde_json::from_value(body).map_err(|_| Error::InvalidEntry)?;
        scratch::buy(&state.pool, auth.user_id, &body).await?
    } else {
        let body = serde_json::from_value(body).map_err(|_| Error::InvalidEntry)?;
        store::buy_numbers(&state.pool, auth.user_id, game, &body).await?
    };
    if query.receipt_only && game != Game::Scratch {
        // The transaction has committed. Hall crowd/feed/history reads must not
        // delay its receipt or turn their failure into a failed purchase.
        // A replay's balance_after is historical; read the current wallet once.
        let balance = store::ensure_wallet(&state.pool, auth.user_id).await?;
        return Ok(Json(json!({
            "purchased": purchase.purchased, "charged": purchase.charged,
            "balance_before": purchase.balance_before, "balance_after": purchase.balance_after,
            "balance": balance,
        })));
    }
    let view = store::view(&state.pool, auth.user_id, game).await?;
    Ok(Json(
        json!({"view":view,"purchased":purchase.purchased,"charged":purchase.charged,"balance_before":purchase.balance_before,"balance_after":purchase.balance_after}),
    ))
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Reveal {
    pub ticket_id: Uuid,
    pub index: Option<usize>,
}
pub async fn reveal(
    State(state): State<AppState>,
    auth: AuthUser,
    body: Result<Json<Reveal>, JsonRejection>,
) -> Result<Json<Value>, Error> {
    let Json(body) = body.map_err(|_| Error::InvalidEntry)?;
    let (ticket, balance) =
        scratch::reveal(&state.pool, auth.user_id, body.ticket_id, body.index).await?;
    Ok(Json(json!({"ticket":ticket,"balance":balance})))
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Refill {
    request_id: Uuid,
}
pub async fn refill(
    State(state): State<AppState>,
    auth: AuthUser,
    body: Result<Json<Refill>, JsonRejection>,
) -> Result<Json<Value>, Error> {
    let Json(body) = body.map_err(|_| Error::InvalidEntry)?;
    Ok(Json(
        json!({"granted": super::REFILL, "balance":store::refill(&state.pool, auth.user_id, body.request_id).await?}),
    ))
}
#[derive(Deserialize)]
pub struct Listing {
    limit: Option<i64>,
    offset: Option<i64>,
}
pub async fn draws(
    State(state): State<AppState>,
    _auth: AuthUser,
    Path(name): Path<String>,
    Query(q): Query<Listing>,
) -> Result<Json<Value>, Error> {
    Ok(Json(
        json!({"draws":store::draws(&state.pool, game(&name)?, crate::pagination::limit(q.limit).min(100), crate::pagination::offset(q.offset)).await?}),
    ))
}
#[derive(Deserialize)]
pub struct TicketListing {
    limit: Option<i64>,
    offset: Option<i64>,
    #[serde(default)]
    status: store::TicketFilter,
}
pub async fn tickets(
    State(state): State<AppState>,
    auth: AuthUser,
    Path(name): Path<String>,
    Query(q): Query<TicketListing>,
) -> Result<Json<Value>, Error> {
    let limit = crate::pagination::limit(q.limit).min(100);
    let offset = crate::pagination::offset(q.offset);
    let mut tickets = store::tickets(
        &state.pool,
        auth.user_id,
        game(&name)?,
        limit + 1,
        offset,
        q.status,
    )
    .await?;
    let has_more = tickets.len() > limit as usize;
    tickets.truncate(limit as usize);
    let next_offset = has_more.then_some(offset + tickets.len() as i64);
    Ok(Json(
        json!({ "tickets": tickets, "has_more": has_more, "next_offset": next_offset }),
    ))
}

/// Public draw evidence: the announced plan before the draw, and the exact
/// verified drand pulse plus reproducible numbers afterwards.
pub async fn proof(
    State(state): State<AppState>,
    Path((name, period)): Path<(String, i64)>,
) -> Result<Json<Value>, Error> {
    Ok(Json(store::proof(&state.pool, game(&name)?, period).await?))
}
