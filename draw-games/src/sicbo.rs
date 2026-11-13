//! 押大小 shared rules for its rooms (`crate::sicbo_room`): stakes, the bean
//! jackpot, the lobby profile and the bean play history.
use crate::handlers::respond::error_code;
use crate::{app_state::AppState, auth::middleware::AuthUser, bean_ledger};
use axum::{
    extract::{Query, State},
    http::StatusCode,
    response::{IntoResponse, Response},
    Json,
};
pub use draw_games::sicbo::{wins, Side, MAX_STAKE, MIN_STAKE};
use serde_json::json;
use uuid::Uuid;

/// A separate purpose-bound stream preserves a room prize across checkpoint recovery.
/// The draw proof and the base bean payout use their original entropy stream.
/// The stream label predates the bean jackpot (it once chose a Max membership
/// prize); it stays unchanged so a recovered round keeps its original decision.
pub(crate) fn wins_jackpot(reveal: &draw_games::proof::Reveal, user: Uuid) -> bool {
    let seed = draw_games::proof::sha256_fields(&[
        b"bluffking-sicbo-max-prize-v1",
        reveal.commitment.game.as_bytes(),
        reveal.commitment.draw_id.as_bytes(),
        reveal.server_seed.as_bytes(),
        reveal.client_seed.as_bytes(),
        user.as_bytes(),
    ]);
    draw_games::proof::Entropy::new(seed).below(10_000) == 0
}

/// The stake a jackpot multiplies: one bean, whatever the wager.
const JACKPOT_STAKE: i64 = 1;

/// A jackpot pays 5 × the highest bean payout a 1-bean stake can win: a
/// winning small/big returns the stake plus an equal win (2 × stake), so the
/// bonus is 10 × [`JACKPOT_STAKE`]. Larger stakes (up to 梭哈) draw the same
/// 10-bean jackpot as a 1-bean one.
pub(crate) fn jackpot_bonus(stake: i64) -> Option<i64> {
    stake
        .min(JACKPOT_STAKE)
        .checked_mul(2)?
        .checked_mul(5)
        .filter(|bonus| *bonus > 0)
}

/// Credits a jackpot as its own bonus row ([`crate::bean_ledger::credit`],
/// idempotent on the play's record id) in the settlement transaction, before
/// the play receipt: that receipt's `balance_after` includes it while its
/// `delta` stays the verifiable rules payout. `false` when the account is
/// erased (nothing to credit).
pub(crate) async fn credit_jackpot(
    conn: &mut crate::db_writer::PgConnection,
    user: Uuid,
    play_record: Uuid,
    bonus: i64,
    meta: serde_json::Value,
) -> sqlx::Result<bool> {
    use crate::bean_ledger::{credit, Amount, Leg, Reason, Transfer, TransferError};
    let leg = Leg {
        game: Some("sicbo"),
        ..Leg::new(
            user,
            Reason::SicboJackpot,
            play_record,
            Amount::Credit(bonus),
            meta,
        )
    };
    match credit(conn, &Transfer::one(leg)).await {
        Ok(_) => Ok(true),
        Err(TransferError::Db(sqlx::Error::RowNotFound)) => Ok(false),
        Err(error) => Err(error.into()),
    }
}

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
        tracing::error!(error=%self.0, "sicbo storage failed");
        error_code(StatusCode::INTERNAL_SERVER_ERROR, "internal_error")
    }
}
pub async fn profile(
    State(state): State<AppState>,
    auth: AuthUser,
) -> Result<Json<serde_json::Value>, Error> {
    let wallet = bean_ledger::ensure(&state.pool, auth.user_id).await?;
    crate::casino_room::recover::<crate::sicbo_room::SicboRoom>(&state.pool).await?;
    let mut body = json!({ "wallet":wallet, "min_stake":MIN_STAKE, "max_stake":MAX_STAKE });
    crate::casino_room::rooms_fields::<crate::sicbo_room::SicboRoom>(auth.user_id, &mut body);
    Ok(Json(body))
}
pub type LedgerQuery = crate::pagination::Query;
pub async fn ledger(
    State(state): State<AppState>,
    auth: AuthUser,
    Query(query): Query<LedgerQuery>,
) -> Result<Json<serde_json::Value>, Error> {
    // This game's own bean plays (room rounds, and solo rolls from before
    // rooms were the only way to play).
    let entries = bean_ledger::game_plays(
        &state.pool,
        auth.user_id,
        "sicbo",
        &["revision", "hand_completed", "has_bots"],
        query,
    )
    .await?;
    Ok(Json(json!({"entries":entries})))
}
