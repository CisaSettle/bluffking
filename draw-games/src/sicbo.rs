//! Solo three-dice practice game. The wallet row serializes bets and refills;
//! the ledger receipt is committed together with the balance and record.
use crate::handlers::respond::error_code;
use crate::{app_state::AppState, auth::middleware::AuthUser, wallet};
use axum::{
    extract::{rejection::JsonRejection, Query, State},
    http::StatusCode,
    response::{IntoResponse, Response},
    Json,
};
use rand::{rngs::OsRng, Rng};
use serde::{Deserialize, Serialize};
use serde_json::json;
use sqlx::{PgPool, Row};
use uuid::Uuid;

pub const POLICY: wallet::Policy = wallet::Policy {
    game: crate::game_catalog::SICBO,
    grant: 1_000,
};
pub const MIN_STAKE: i64 = 10;
pub const MAX_STAKE: i64 = 1_000;

#[derive(Debug, Clone, Copy, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum Side {
    Small,
    Big,
}
#[derive(Debug, Clone, Deserialize, Serialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct Bet {
    pub request_id: Uuid,
    pub side: Side,
    pub stake: i64,
    pub session_id: Uuid,
}
#[derive(Debug, Clone, Deserialize, Serialize, PartialEq, Eq)]
pub struct Receipt {
    pub request_id: Uuid,
    pub side: Side,
    pub stake: i64,
    pub dice: [u8; 3],
    pub total: u8,
    pub triple: bool,
    pub won: bool,
}
#[derive(Debug, Serialize)]
pub struct BetResult {
    pub receipt: Receipt,
    pub balance_after: i64,
    pub balance: i64,
    pub delta: i64,
}

pub fn wins(dice: [u8; 3], side: Side) -> bool {
    if dice[0] == dice[1] && dice[1] == dice[2] {
        return false;
    }
    let total: u8 = dice.iter().sum();
    match side {
        Side::Small => (4..=10).contains(&total),
        Side::Big => (11..=17).contains(&total),
    }
}
#[derive(Debug)]
pub enum Error {
    Invalid,
    Conflict,
    Balance,
    /// Seated in a shared sicbo room: solo rolls and refills wait until the
    /// player leaves it, so a pending room wager is always covered.
    InRoom,
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
            Self::Invalid => (StatusCode::BAD_REQUEST, "invalid_bet"),
            Self::Conflict => (StatusCode::CONFLICT, "request_conflict"),
            Self::Balance => (StatusCode::BAD_REQUEST, "insufficient_balance"),
            Self::InRoom => (StatusCode::CONFLICT, "in_room"),
            Self::Db(e) => {
                tracing::error!(error=%e, "sicbo storage failed");
                (StatusCode::INTERNAL_SERVER_ERROR, "internal_error")
            }
        };
        error_code(status, code)
    }
}
async fn saved(
    conn: &mut sqlx::PgConnection,
    user: Uuid,
    bet: &Bet,
    balance: i64,
) -> Result<Option<BetResult>, Error> {
    let row = sqlx::query("SELECT details, balance_after, delta FROM wallet_ledger WHERE game=$1 AND user_id=$2 AND record_id=$3")
        .bind(POLICY.game).bind(user).bind(bet.request_id).fetch_optional(conn).await?;
    let Some(row) = row else {
        return Ok(None);
    };
    let details: serde_json::Value = row.get("details");
    if details["session_id"] != json!(bet.session_id) {
        return Err(Error::Conflict);
    }
    let receipt: Receipt =
        serde_json::from_value(details).map_err(|e| sqlx::Error::Protocol(e.to_string()))?;
    if receipt.side != bet.side || receipt.stake != bet.stake {
        return Err(Error::Conflict);
    }
    Ok(Some(BetResult {
        receipt,
        balance,
        balance_after: row.get("balance_after"),
        delta: row.get("delta"),
    }))
}
pub async fn play(pool: &PgPool, user: Uuid, bet: &Bet) -> Result<BetResult, Error> {
    if !(MIN_STAKE..=MAX_STAKE).contains(&bet.stake)
        || bet.request_id.is_nil()
        || bet.session_id.is_nil()
    {
        return Err(Error::Invalid);
    }
    // Recovery and room inspection may need pool connections or room locks.
    // Finish them before holding the wallet transaction so concurrent retries
    // cannot exhaust the pool while waiting for recovery or the room sweep.
    let round_in_play =
        crate::casino_room::round_in_play::<crate::sicbo_room::SicboRoom>(pool, bet.request_id)
            .await
            .unwrap_or(true);
    wallet::ensure(pool, POLICY, user).await?;
    let mut tx = pool.begin().await?;
    let balance = wallet::lock(&mut tx, POLICY.game, user)
        .await?
        .ok_or(sqlx::Error::RowNotFound)?;
    // Lookup MUST follow the wallet lock, including retries from another tab.
    if let Some(result) = saved(&mut tx, user, bet, balance).await? {
        tx.commit().await?;
        return Ok(result);
    }
    if crate::sicbo_room::seated(&mut tx, user).await? {
        return Err(Error::InRoom);
    }
    // A room settlement uses its round id as the receipt key; a solo roll may
    // not take the id of a round still open in any room (fails closed).
    if round_in_play {
        return Err(Error::Conflict);
    }
    if current_session(&mut tx, user).await?.as_deref() != Some(&bet.session_id.to_string()) {
        return Err(Error::Conflict);
    }
    if bet.stake > balance {
        return Err(Error::Balance);
    }
    let dice = std::array::from_fn(|_| OsRng.gen_range(1..=6));
    let won = wins(dice, bet.side);
    let delta = if won { bet.stake } else { -bet.stake };
    let receipt = Receipt {
        request_id: bet.request_id,
        side: bet.side,
        stake: bet.stake,
        dice,
        total: dice.iter().sum(),
        triple: dice[0] == dice[1] && dice[1] == dice[2],
        won,
    };
    let play = wallet::Play {
        record_id: bet.request_id,
        delta,
        completes_hand: true,
        won,
        room_code: None,
        revision: None,
        details: Some({
            let mut details = json!(receipt);
            details["session_id"] = json!(bet.session_id);
            details
        }),
        hand_completed: Some(true),
        has_bots: Some(false),
    };
    let settled = wallet::settle(&mut tx, POLICY.game, user, &play).await;
    let balance_after = match settled {
        Ok(balance) => balance,
        Err(e)
            if e.as_database_error()
                .is_some_and(|e| e.is_unique_violation()) =>
        {
            tx.rollback().await?;
            let mut tx = pool.begin().await?;
            let balance = wallet::lock(&mut tx, POLICY.game, user)
                .await?
                .ok_or(sqlx::Error::RowNotFound)?;
            let result = saved(&mut tx, user, bet, balance)
                .await?
                .ok_or(sqlx::Error::RowNotFound)?;
            tx.commit().await?;
            return Ok(result);
        }
        Err(e) => return Err(e.into()),
    };
    crate::game_records::record(
        &mut tx,
        POLICY.game,
        bet.request_id,
        Some(bet.session_id),
        chrono::Utc::now(),
        &[(user, delta)],
    )
    .await?;
    tx.commit().await?;
    Ok(BetResult {
        receipt,
        balance_after,
        balance: balance_after,
        delta,
    })
}
pub async fn profile(
    State(state): State<AppState>,
    auth: AuthUser,
) -> Result<Json<serde_json::Value>, Error> {
    let wallet = wallet::ensure(&state.pool, POLICY, auth.user_id).await?;
    crate::casino_room::recover::<crate::sicbo_room::SicboRoom>(&state.pool).await?;
    let room = crate::sicbo_room::current_room(auth.user_id);
    Ok(Json(
        json!({ "wallet":wallet, "room":room, "min_stake":MIN_STAKE, "max_stake":MAX_STAKE }),
    ))
}
pub async fn bet(
    State(state): State<AppState>,
    auth: AuthUser,
    body: Result<Json<Bet>, JsonRejection>,
) -> Result<Json<BetResult>, Error> {
    let Json(bet) = body.map_err(|_| Error::Invalid)?;
    Ok(Json(play(&state.pool, auth.user_id, &bet).await?))
}

#[derive(Deserialize)]
pub struct LedgerQuery {
    limit: Option<i64>,
}
pub async fn ledger(
    State(state): State<AppState>,
    auth: AuthUser,
    Query(query): Query<LedgerQuery>,
) -> Result<Json<serde_json::Value>, Error> {
    let entries = wallet::ledger(
        &state.pool,
        POLICY.game,
        auth.user_id,
        query.limit.unwrap_or(30).clamp(1, 100),
    )
    .await?;
    Ok(Json(json!({"entries":entries})))
}

#[derive(Deserialize)]
pub struct SessionRequest {
    pub session_id: Uuid,
}
#[derive(Deserialize)]
pub struct BuyInRequest {
    pub session_id: Uuid,
    pub request_id: Uuid,
    pub units: i64,
}

async fn current_session(
    conn: &mut sqlx::PgConnection,
    user: Uuid,
) -> sqlx::Result<Option<String>> {
    Ok(sqlx::query_scalar::<_, Option<String>>("SELECT room_code FROM wallet_ledger WHERE game=$1 AND user_id=$2 AND kind='grant' ORDER BY id DESC LIMIT 1")
        .bind(POLICY.game).bind(user).fetch_optional(conn).await?.flatten())
}
/// A session is represented by its grant receipt. Reload retries cannot reset it,
/// and starting another room or solo session fences the previous one.
pub async fn open_session(pool: &PgPool, user: Uuid, session: Uuid) -> Result<i64, Error> {
    if session.is_nil() {
        return Err(Error::Invalid);
    }
    crate::casino_room::recover::<crate::sicbo_room::SicboRoom>(pool).await?;
    wallet::ensure(pool, POLICY, user).await?;
    let mut tx = pool.begin().await?;
    let balance = wallet::lock(&mut tx, POLICY.game, user)
        .await?
        .ok_or(sqlx::Error::RowNotFound)?;
    if crate::sicbo_room::seated(&mut tx, user).await? {
        return Err(Error::InRoom);
    }
    let code = session.to_string();
    if current_session(&mut tx, user).await?.as_deref() == Some(&code) {
        return Ok(balance);
    }
    let existed: bool = sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM wallet_ledger WHERE game=$1 AND user_id=$2 AND kind='grant' AND room_code=$3)")
        .bind(POLICY.game).bind(user).bind(&code).fetch_one(&mut *tx).await?;
    if existed {
        return Err(Error::Conflict);
    }
    let balance = wallet::start_session_on(&mut tx, POLICY, user, Some(&code)).await?;
    tx.commit().await?;
    Ok(balance)
}
pub async fn session(
    State(s): State<AppState>,
    auth: AuthUser,
    Json(req): Json<SessionRequest>,
) -> Result<Json<serde_json::Value>, Error> {
    Ok(Json(
        json!({"balance":open_session(&s.pool, auth.user_id, req.session_id).await?}),
    ))
}
pub async fn add_chips(pool: &PgPool, user: Uuid, req: &BuyInRequest) -> Result<i64, Error> {
    if !(100..=1000).contains(&req.units) || req.request_id.is_nil() {
        return Err(Error::Invalid);
    }
    let mut tx = pool.begin().await?;
    let before = wallet::lock(&mut tx, POLICY.game, user)
        .await?
        .ok_or(sqlx::Error::RowNotFound)?;
    let code = req.session_id.to_string();
    if current_session(&mut tx, user).await?.as_deref() != Some(&code)
        || crate::sicbo_room::seated(&mut tx, user).await?
    {
        return Err(Error::Conflict);
    }
    let prior: Option<i64> = sqlx::query_scalar("SELECT delta FROM wallet_ledger WHERE game=$1 AND user_id=$2 AND kind='refill' AND room_code=$3 AND details->>'buy_in_id'=$4")
        .bind(POLICY.game).bind(user).bind(&code).bind(req.request_id.to_string()).fetch_optional(&mut *tx).await?;
    if let Some(delta) = prior {
        return if delta == req.units * MIN_STAKE {
            Ok(before)
        } else {
            Err(Error::Conflict)
        };
    }
    let after = before
        .checked_add(req.units * MIN_STAKE)
        .filter(|n| *n <= 1000 * MIN_STAKE)
        .ok_or(Error::Balance)?;
    wallet::refill_snapshot_on(
        &mut tx,
        POLICY.game,
        user,
        before,
        after,
        &code,
        0,
        req.request_id,
    )
    .await?;
    tx.commit().await?;
    Ok(after)
}
pub async fn buy_in(
    State(s): State<AppState>,
    auth: AuthUser,
    Json(req): Json<BuyInRequest>,
) -> Result<Json<serde_json::Value>, Error> {
    Ok(Json(
        json!({"balance":add_chips(&s.pool, auth.user_id, &req).await?}),
    ))
}
