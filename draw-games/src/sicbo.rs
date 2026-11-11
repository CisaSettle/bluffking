//! Solo three-dice game. The shared gold-bean wallet row serializes wagers;
//! the [`crate::bean_ledger`] posting commits together with the game record.
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
/// This request's committed roll, read under the wallet lock.
async fn saved(
    conn: &mut crate::db_writer::PgConnection,
    user: Uuid,
    bet: &Bet,
    balance: i64,
) -> Result<Option<BetResult>, Error> {
    let row = sqlx::query(
        "SELECT details, balance_after, delta FROM wallet_ledger WHERE game=$1 AND user_id=$2 AND record_id=$3",
    )
    .bind(POLICY.game)
    .bind(user)
    .bind(record_id(user, bet.request_id))
    .fetch_optional(conn)
    .await?;
    let Some(row) = row else {
        return Ok(None);
    };
    let details = crate::bean_ledger::receipt(&row.get::<serde_json::Value, _>("details"));
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
    use crate::bean_ledger::{self, Entry, Kind, Posting};
    if Some(bet.stake) != crate::beans::solo_stake(MIN_STAKE, bet.expected_balance)
        || bet.request_id.is_nil()
        || bet.session_id.is_nil()
    {
        return Err(Error::Invalid);
    }
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
    let record = record_id(user, bet.request_id);
    let jackpot_bonus = if won && wins_jackpot(&reveal, user) {
        let bonus = jackpot_bonus(bet.stake).ok_or(Error::Invalid)?;
        credit_jackpot(
            &mut tx,
            user,
            record,
            bonus,
            json!({"request_id":bet.request_id,"record_id":record,"stake":bet.stake}),
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
        jackpot_bonus,
    };
    let mut meta = json!(receipt);
    meta["session_id"] = json!(bet.session_id);
    let posting = Posting {
        game: "sicbo",
        kind: Kind::Solo,
        record_id: record,
        room_code: None,
        session: Some(bet.session_id),
        has_bots: false,
        meta,
        entries: vec![Entry {
            user,
            delta,
            completes_hand: true,
            won,
            meta: None,
        }],
    };
    let balance_after = bean_ledger::settle_one(&mut tx, &posting).await?;
    crate::game_records::record_beans(
        &mut tx,
        crate::game_catalog::SICBO,
        record,
        Some(bet.session_id),
        false,
        &[(
            user,
            delta,
            bean_ledger::receipt(&bean_ledger::details(&posting, &posting.entries[0])),
        )],
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
    let fairness_offer = crate::draw_fairness::offer(&state.pool, auth.user_id, "sicbo").await?;
    let mut body = json!({ "wallet":wallet, "min_stake":MIN_STAKE, "max_stake":MAX_STAKE,
        "fairness_offer":fairness_offer });
    crate::casino_room::rooms_fields::<crate::sicbo_room::SicboRoom>(auth.user_id, &mut body);
    Ok(Json(body))
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
    // This game's own plays (solo rolls and room rounds).
    let entries: Vec<serde_json::Value> = sqlx::query_scalar(&format!(
        "SELECT to_jsonb(l) FROM (SELECT id,kind,record_id,room_code,revision,delta,
         balance_after,{} AS details,
         hand_completed,has_bots,created_at FROM wallet_ledger
         WHERE user_id=$1 AND kind='play' AND game=$2 AND details->>'game'='sicbo'
         ORDER BY id DESC LIMIT $3 OFFSET $4) l",
        crate::bean_ledger::RECEIPT_SQL
    ))
    .bind(auth.user_id)
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
