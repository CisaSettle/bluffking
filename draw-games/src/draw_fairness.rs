//! Durable offers for immediate draws. A claimed seed is moved into the game's
//! receipt in the same transaction as the debit, so retries never choose a seed.
use draw_games::proof::{commit, seed_bytes, Commitment, Reveal};
use rand::{rngs::OsRng, RngCore};
use serde::{Deserialize, Serialize};
use sqlx::{PgConnection, PgPool};
use uuid::Uuid;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Challenge {
    pub offer_id: Uuid,
    pub commitment: String,
    pub client_seed: String,
}

#[derive(Debug, thiserror::Error)]
pub enum ClaimError {
    #[error("invalid draw commitment or client seed")]
    Invalid,
    #[error("draw offer was consumed or belongs to another request")]
    Conflict,
    #[error(transparent)]
    Db(#[from] sqlx::Error),
}

fn commitment(game: &str, id: Uuid, seed: &str) -> Result<Commitment, sqlx::Error> {
    commit(game, &id.to_string(), seed).map_err(sqlx::Error::Protocol)
}

/// At most one outstanding offer per user/game. Ordinary polling only reads it.
pub async fn offer(pool: &PgPool, user: Uuid, game: &str) -> Result<Commitment, sqlx::Error> {
    if !matches!(game, "scratch" | "sicbo") {
        return Err(sqlx::Error::Protocol(
            "unsupported immediate draw game".into(),
        ));
    }
    if let Some((id, seed)) = sqlx::query_as::<_, (Uuid, String)>(
        "SELECT id,server_seed FROM draw_commitments WHERE user_id=$1 AND game=$2",
    )
    .bind(user)
    .bind(game)
    .fetch_optional(pool)
    .await?
    {
        return commitment(game, id, &seed);
    }
    let mut tx = pool.begin().await?;
    sqlx::query("SET LOCAL lock_timeout='1s';")
        .execute(&mut *tx)
        .await?;
    crate::db::lock_active_users(&mut tx, &[user]).await?;
    let id = Uuid::new_v4();
    let mut seed = [0; 32];
    OsRng.fill_bytes(&mut seed);
    sqlx::query("INSERT INTO draw_commitments (id,user_id,game,server_seed) VALUES ($1,$2,$3,$4) ON CONFLICT (user_id,game) DO NOTHING")
        .bind(id).bind(user).bind(game).bind(hex::encode(seed)).execute(&mut *tx).await?;
    let (id, seed) = sqlx::query_as::<_, (Uuid, String)>(
        "SELECT id,server_seed FROM draw_commitments WHERE user_id=$1 AND game=$2",
    )
    .bind(user)
    .bind(game)
    .fetch_one(&mut *tx)
    .await?;
    let result = commitment(game, id, &seed)?;
    tx.commit().await?;
    Ok(result)
}

/// Caller owns the account/wallet locks and has checked its idempotent receipt.
/// Deletion and the resulting game receipt must commit in this same transaction.
pub async fn claim(
    conn: &mut PgConnection,
    user: Uuid,
    game: &str,
    _request_id: Uuid,
    challenge: &Challenge,
) -> Result<Reveal, ClaimError> {
    seed_bytes(&challenge.client_seed).map_err(|_| ClaimError::Invalid)?;
    seed_bytes(&challenge.commitment).map_err(|_| ClaimError::Invalid)?;
    let seed: Option<String> = sqlx::query_scalar(
        "SELECT server_seed FROM draw_commitments WHERE id=$1 AND user_id=$2 AND game=$3 FOR UPDATE",
    ).bind(challenge.offer_id).bind(user).bind(game).fetch_optional(&mut *conn).await?;
    let seed = seed.ok_or(ClaimError::Conflict)?;
    let offer = commitment(game, challenge.offer_id, &seed)?;
    if challenge.commitment != offer.commitment {
        return Err(ClaimError::Invalid);
    }
    sqlx::query("DELETE FROM draw_commitments WHERE id=$1")
        .bind(challenge.offer_id)
        .execute(conn)
        .await?;
    Ok(Reveal {
        commitment: offer,
        server_seed: seed,
        client_seed: challenge.client_seed.clone(),
    })
}
