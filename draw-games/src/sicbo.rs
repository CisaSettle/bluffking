//! Solo three-dice game. The shared gold-bean wallet row serializes wagers;
//! the ledger receipt is committed together with the balance and record.
use crate::db_writer::PgPool;
use crate::handlers::respond::error_code;
use crate::{app_state::AppState, auth::middleware::AuthUser, wallet};
use axum::{
    extract::{rejection::JsonRejection, Query, State},
    http::StatusCode,
    response::{IntoResponse, Response},
    Json,
};
pub use draw_games::sicbo::{wins, Side, MAX_STAKE, MIN_STAKE};
use serde::{Deserialize, Serialize};
use serde_json::json;
use sqlx::Row;
use uuid::Uuid;

pub const POLICY: wallet::Policy = crate::beans::POLICY;

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

/// A jackpot pays 5 × the highest bean payout this stake can win: a winning
/// small/big returns the stake plus an equal win (2 × stake), so the bonus is
/// 10 × stake (1,000 beans on a 100-bean wager). `None` only on overflow.
pub(crate) fn jackpot_bonus(stake: i64) -> Option<i64> {
    stake
        .checked_mul(2)?
        .checked_mul(5)
        .filter(|bonus| *bonus > 0)
}

/// Credits a jackpot as a separate bean `bonus` row in the caller's settlement
/// transaction, before the play receipt, so that receipt's `balance_after`
/// includes it while its `delta` stays the verifiable rules payout. The caller
/// holds the account and wallet locks; a missing wallet credits nothing.
pub(crate) async fn credit_jackpot(
    conn: &mut crate::db_writer::PgConnection,
    user: Uuid,
    bonus: i64,
    details: serde_json::Value,
) -> sqlx::Result<()> {
    sqlx::query(
        "WITH updated AS (UPDATE wallets SET balance=balance+$3,updated_at=now() WHERE user_id=$1 AND game=$2 RETURNING balance) \
         INSERT INTO wallet_ledger(user_id,game,kind,delta,balance_after,details) \
         SELECT $1,$2,'bonus',$3,balance,$4 FROM updated",
    )
    .bind(user)
    .bind(POLICY.game)
    .bind(bonus)
    .bind(details)
    .execute(conn.writer())
    .await?;
    Ok(())
}

#[derive(Debug, Clone, Deserialize, Serialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct Bet {
    pub request_id: Uuid,
    pub side: Side,
    pub stake: i64,
    #[serde(default)]
    pub expected_balance: Option<i64>,
    pub session_id: Uuid,
    pub fairness: crate::draw_fairness::Challenge,
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
    pub fairness: serde_json::Value,
    /// Legacy: the Max membership grant of a jackpot settled before the bean jackpot.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_reward: Option<serde_json::Value>,
    /// Bean jackpot credited with this roll (a separate `bonus` ledger row).
    #[serde(default)]
    pub jackpot_bonus: i64,
}
#[derive(Debug, Serialize)]
pub struct BetResult {
    pub receipt: Receipt,
    pub balance_after: i64,
    pub balance: i64,
    pub delta: i64,
}

#[derive(Debug)]
pub enum Error {
    Invalid,
    Conflict,
    Balance,
    BalanceChanged,
    /// Seated in a shared sicbo room: solo rolls and refills wait until the
    /// player leaves it, so a pending room wager is always covered.
    InRoom,
    BeansInUse,
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
            Self::BalanceChanged => (StatusCode::CONFLICT, "balance_changed"),
            Self::Balance => (StatusCode::BAD_REQUEST, "insufficient_balance"),
            Self::InRoom => (StatusCode::CONFLICT, "in_room"),
            Self::BeansInUse => (StatusCode::CONFLICT, "beans_in_use"),
            Self::Db(e) => {
                tracing::error!(error=%e, "sicbo storage failed");
                (StatusCode::INTERNAL_SERVER_ERROR, "internal_error")
            }
        };
        error_code(status, code)
    }
}
/// The derived receipt, else a solo receipt written before derivation under
/// the raw request id (a room round's receipt under that id is not this
/// request's). Both are read in one statement, so a retry spanning the
/// release replays instead of debiting again.
async fn saved(
    conn: &mut crate::db_writer::PgConnection,
    user: Uuid,
    bet: &Bet,
    balance: i64,
) -> Result<Option<BetResult>, Error> {
    let derived = record_id(user, bet.request_id);
    let rows = sqlx::query("SELECT record_id, details, balance_after, delta FROM wallet_ledger WHERE game=$1 AND user_id=$2 AND record_id=$3 \
        UNION ALL SELECT record_id, details, balance_after, delta FROM wallet_ledger WHERE game=$1 AND user_id=$2 AND record_id=$4")
        .bind(POLICY.game).bind(user).bind(derived).bind(bet.request_id).fetch_all(conn).await?;
    let pick = |id: Uuid| {
        rows.iter()
            .find(|row| row.get::<Uuid, _>("record_id") == id)
    };
    let row = pick(derived).or_else(|| {
        pick(bet.request_id).filter(|row| {
            let details: serde_json::Value = row.get("details");
            details["game"] == "sicbo" && details.get("room_code").is_none()
        })
    });
    let Some(row) = row else {
        return Ok(None);
    };
    let details: serde_json::Value = row.get("details");
    if details["game"] != "sicbo" || details["session_id"] != json!(bet.session_id) {
        return Err(Error::Conflict);
    }
    let receipt: Receipt =
        serde_json::from_value(details).map_err(|e| sqlx::Error::Protocol(e.to_string()))?;
    if receipt.side != bet.side || receipt.stake != bet.stake {
        return Err(Error::Conflict);
    }
    {
        let (challenge, proof) = (&bet.fairness, &receipt.fairness);
        let reveal = &proof["reveal"];
        if reveal["draw_id"] != json!(challenge.offer_id)
            || reveal["commitment"] != json!(challenge.commitment)
            || reveal["client_seed"] != json!(challenge.client_seed)
        {
            return Err(Error::Conflict);
        }
    }
    Ok(Some(BetResult {
        receipt,
        balance,
        balance_after: row.get("balance_after"),
        delta: row.get("delta"),
    }))
}
/// The receipt (ledger and game record) id of a solo roll; see
/// [`crate::beans::solo_record_id`].
fn record_id(user: Uuid, request: Uuid) -> Uuid {
    crate::beans::solo_record_id("sicbo", user, request)
}
pub async fn play(pool: &PgPool, user: Uuid, bet: &Bet) -> Result<BetResult, Error> {
    if Some(bet.stake) != crate::beans::solo_stake(MIN_STAKE, bet.expected_balance)
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
    let _beans_mutation = crate::beans_read_memory::mutation(pool, [user]);
    wallet::ensure(pool, POLICY, user).await?;
    let mut tx = pool.begin().await?;
    crate::db::lock_active_users(&mut tx, &[user]).await?;
    let balance = wallet::lock(&mut tx, POLICY.game, user)
        .await?
        .ok_or(sqlx::Error::RowNotFound)?;
    // Lookup MUST follow the wallet lock, including retries from another tab.
    if let Some(result) = saved(&mut tx, user, bet, balance).await? {
        let committed = tx.commit().await;
        crate::beans_read_memory::invalidate_user(pool, user);
        committed?;
        return Ok(result);
    }
    // A room settlement uses its round id as the receipt key. Solo receipts
    // are derived ids, and a solo request may still not reuse the id of a
    // round open in any room (fails closed).
    if round_in_play {
        return Err(Error::Conflict);
    }
    let available = crate::beans::available_on(pool, &mut tx, user, balance, None).await?;
    if bet
        .expected_balance
        .is_some_and(|expected| expected != available)
    {
        return Err(Error::BalanceChanged);
    }
    if bet.stake > available {
        return Err(Error::Balance);
    }
    let challenge = &bet.fairness;
    let reveal = crate::draw_fairness::claim(&mut tx, user, "sicbo", bet.request_id, challenge)
        .await
        .map_err(|error| match error {
            crate::draw_fairness::ClaimError::Invalid => Error::Invalid,
            crate::draw_fairness::ClaimError::Conflict => Error::Conflict,
            crate::draw_fairness::ClaimError::Db(error) => Error::Db(error),
        })?;
    let dice = draw_games::sicbo::draw(&reveal).map_err(|_| Error::Invalid)?;
    let won = wins(dice, bet.side);
    let delta = draw_games::sicbo::delta(dice, bet.side, bet.stake);
    let jackpot_bonus = if won && wins_jackpot(&reveal, user) {
        let bonus = jackpot_bonus(bet.stake).ok_or(Error::Invalid)?;
        credit_jackpot(
            &mut tx,
            user,
            bonus,
            json!({"game":"sicbo","purpose":"sicbo_jackpot","request_id":bet.request_id,
                "record_id":record_id(user, bet.request_id),"stake":bet.stake}),
        )
        .await?;
        bonus
    } else {
        0
    };
    let receipt = Receipt {
        request_id: bet.request_id,
        side: bet.side,
        stake: bet.stake,
        dice,
        total: dice.iter().sum(),
        triple: dice[0] == dice[1] && dice[1] == dice[2],
        won,
        fairness: draw_games::sicbo::payout_proof(&reveal, dice, bet.side, bet.stake),
        max_reward: None,
        jackpot_bonus,
    };
    let record = record_id(user, bet.request_id);
    let play = wallet::Play {
        record_id: record,
        delta,
        completes_hand: true,
        won,
        room_code: None,
        revision: None,
        details: Some({
            let mut details = json!(receipt);
            details["session_id"] = json!(bet.session_id);
            details["game"] = json!("sicbo");
            details["unit"] = json!("beans");
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
            crate::db::lock_active_users(&mut tx, &[user]).await?;
            let balance = wallet::lock(&mut tx, POLICY.game, user)
                .await?
                .ok_or(sqlx::Error::RowNotFound)?;
            let result = saved(&mut tx, user, bet, balance)
                .await?
                .ok_or(sqlx::Error::RowNotFound)?;
            let committed = tx.commit().await;
            crate::beans_read_memory::invalidate_user(pool, user);
            committed?;
            return Ok(result);
        }
        Err(e) => return Err(e.into()),
    };
    crate::game_records::record(
        &mut tx,
        crate::game_catalog::SICBO,
        record,
        Some(bet.session_id),
        chrono::Utc::now(),
        &[(user, delta)],
    )
    .await?;
    let committed = tx.commit().await;
    crate::beans_read_memory::invalidate_user(pool, user);
    committed?;
    wallet::project_session_balance(POLICY.game, user, balance_after);
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
    let fairness_offer = crate::draw_fairness::offer(&state.pool, auth.user_id, "sicbo").await?;
    Ok(Json(
        json!({ "wallet":wallet, "room":room, "min_stake":MIN_STAKE, "max_stake":MAX_STAKE,
            "fairness_offer":fairness_offer }),
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

pub type LedgerQuery = crate::pagination::Query;
pub async fn ledger(
    State(state): State<AppState>,
    auth: AuthUser,
    Query(query): Query<LedgerQuery>,
) -> Result<Json<serde_json::Value>, Error> {
    // Preserve old independent-chip receipts alongside this game's new bean
    // receipts, without leaking unrelated shared-wallet games into dice history.
    let entries: Vec<serde_json::Value> = sqlx::query_scalar(
        "SELECT to_jsonb(l) FROM (SELECT id,kind,record_id,room_code,revision,delta,
         balance_after,COALESCE(details,'{}'::jsonb) || jsonb_build_object('unit', CASE WHEN game=$2 THEN 'practice' ELSE 'beans' END) AS details,
         hand_completed,has_bots,created_at FROM wallet_ledger
         WHERE user_id=$1 AND kind='play' AND (game=$2 OR (game=$3 AND details->>'game'='sicbo'))
         ORDER BY id DESC LIMIT $4 OFFSET $5) l",
    )
    .bind(auth.user_id)
    .bind(crate::game_catalog::SICBO)
    .bind(POLICY.game)
    .bind(query.limit())
    .bind(query.offset())
    .fetch_all(&state.pool)
    .await?;
    Ok(Json(json!({"entries":entries})))
}

#[derive(Deserialize)]
pub struct SessionRequest {
    pub session_id: Uuid,
}
/// Sessions group receipts only. Opening or reloading never resets the wallet.
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
