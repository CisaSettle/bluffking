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
pub async fn buy(
    State(state): State<AppState>,
    auth: AuthUser,
    Path(name): Path<String>,
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
}
pub async fn draws(
    State(state): State<AppState>,
    _auth: AuthUser,
    Path(name): Path<String>,
    Query(q): Query<Listing>,
) -> Result<Json<Value>, Error> {
    Ok(Json(
        json!({"draws":store::draws(&state.pool, game(&name)?, q.limit.unwrap_or(30).clamp(1,100)).await?}),
    ))
}
pub async fn tickets(
    State(state): State<AppState>,
    auth: AuthUser,
    Path(name): Path<String>,
    Query(q): Query<Listing>,
) -> Result<Json<Value>, Error> {
    Ok(Json(
        json!({"tickets":store::tickets(&state.pool, auth.user_id, game(&name)?, q.limit.unwrap_or(50).clamp(1,100)).await?}),
    ))
}
