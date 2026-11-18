//! 鱼虾蟹 (Fish Prawn Crab) dealer rooms: the Guangdong three-dice game.
//! Same shape as 押大小 (`crate::sicbo_room`): open rooms live in memory on
//! the table owner, a dealer loop settles due rounds, and an ordered per-room
//! writer commits each change (`crate::casino_room`). A 金豆 room settles into
//! the shared bean wallet, a 积分 room into its own points, under the rules in
//! `draw_games::yuxiaxie`. AI co-bettors (`crate::casino_bots`) never touch
//! wallets.
pub(crate) mod store;

use crate::handlers::respond::error_code;
use crate::{app_state::AppState, auth::middleware::AuthUser, bean_ledger};
use axum::{
    extract::{Query, State},
    http::StatusCode,
    response::{IntoResponse, Response},
    Json,
};
pub use draw_games::yuxiaxie::{Bets, Symbol, MAX_STAKE, MIN_STAKE};
use serde_json::json;
pub use store::YuxiaxieRoom;

/// Storage trouble behind a profile or history read.
#[derive(Debug)]
pub struct Error(sqlx::Error);
impl From<sqlx::Error> for Error {
    fn from(e: sqlx::Error) -> Self {
        Self(e)
    }
}
impl IntoResponse for Error {
    fn into_response(self) -> Response {
        tracing::error!(error=%self.0, "yuxiaxie storage failed");
        error_code(StatusCode::INTERNAL_SERVER_ERROR, "internal_error")
    }
}

pub async fn profile(
    State(state): State<AppState>,
    auth: AuthUser,
) -> Result<Json<serde_json::Value>, Error> {
    let wallet = bean_ledger::ensure(&state.pool, auth.user_id).await?;
    crate::casino_room::recover::<YuxiaxieRoom>(&state.pool).await?;
    let mut body = json!({ "wallet":wallet, "min_stake":MIN_STAKE, "max_stake":MAX_STAKE });
    crate::casino_room::rooms_fields::<YuxiaxieRoom>(auth.user_id, &mut body);
    Ok(Json(body))
}

pub type LedgerQuery = crate::pagination::Query;

/// This game's own bean room rounds.
pub async fn ledger(
    State(state): State<AppState>,
    auth: AuthUser,
    Query(query): Query<LedgerQuery>,
) -> Result<Json<serde_json::Value>, Error> {
    let entries = bean_ledger::game_plays(
        &state.pool,
        auth.user_id,
        "yuxiaxie",
        &["revision", "hand_completed", "has_bots"],
        query,
    )
    .await?;
    Ok(Json(json!({"entries":entries})))
}
