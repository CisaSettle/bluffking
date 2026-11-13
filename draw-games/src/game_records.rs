//! Cross-game record repository: transactional writes and bounded, user-scoped reads.
//! Replay payloads and game rules belong to their game modules.
use crate::db_writer::{PgConnection, PgPool};
use crate::game_catalog as game;
use chrono::{DateTime, Utc};
use serde::Serialize;
use serde_json::Value;
use sqlx::Row;
use uuid::Uuid;

/// Match only affirmative historical evidence; absent legacy metadata stays unknown.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq, Hash, serde::Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Opponent {
    #[default]
    All,
    Bot,
    Human,
}
impl Opponent {
    pub fn predicate(self, expression: &str) -> String {
        match self {
            Self::All => "TRUE".into(),
            Self::Bot => format!("({expression}) IS TRUE"),
            Self::Human => format!("({expression}) IS FALSE"),
        }
    }

    /// The stored `has_bots` value this filter selects; `None` selects all.
    pub fn has_bots(self) -> Option<bool> {
        match self {
            Self::All => None,
            Self::Bot => Some(true),
            Self::Human => Some(false),
        }
    }
}

#[derive(Debug, Serialize)]
pub struct GameSummary {
    pub game: &'static str,
    pub played: i64,
    pub net: Option<i64>,
    pub last_played_at: DateTime<Utc>,
}

#[derive(Debug, Serialize)]
pub struct RecordEntry {
    pub game: &'static str,
    pub record_id: Uuid,
    pub session_id: Option<Uuid>,
    pub played_at: DateTime<Utc>,
    pub delta: i64,
    pub detail: Value,
}

#[derive(Debug, Serialize)]
pub struct RecordsResponse {
    pub summary: Vec<GameSummary>,
    pub entries: Vec<RecordEntry>,
    /// Pass back as `before`, `before_id`, `before_game` to fetch the next
    /// page; `null` at the end.
    pub next_cursor: Option<RecordCursor>,
}

#[derive(Debug, Serialize)]
pub struct RecordCursor {
    pub before: DateTime<Utc>,
    pub before_id: Uuid,
    pub before_game: &'static str,
}

/// Writes one `game_records` row per user for a settled record.
pub async fn record(
    conn: &mut PgConnection,
    game: i16,
    record_id: Uuid,
    session_id: Option<Uuid>,
    played_at: DateTime<Utc>,
    has_bots: bool,
    deltas: &[(Uuid, i64)],
) -> sqlx::Result<()> {
    if deltas.is_empty() {
        return Ok(());
    }
    let users: Vec<Uuid> = deltas.iter().map(|d| d.0).collect();
    let amounts: Vec<i64> = deltas.iter().map(|d| d.1).collect();
    crate::db::lock_game_participants(conn, &users).await?;
    let inserted = sqlx::query(
        "INSERT INTO game_records (user_id, game, record_id, session_id, played_at, delta, has_bots) \
         SELECT t.user_id, $1, $2, $3, $4, t.delta, $7 \
         FROM unnest($5::uuid[], $6::bigint[]) AS t(user_id, delta) ORDER BY t.user_id \
         ON CONFLICT (user_id, game, record_id) DO NOTHING",
    )
    .bind(game)
    .bind(record_id)
    .bind(session_id)
    .bind(played_at)
    .bind(&users)
    .bind(&amounts)
    .bind(has_bots)
    .execute(conn.writer())
    .await?
    .rows_affected();
    // ON CONFLICT DO NOTHING: a replay that inserted nothing counts nothing.
    if inserted > 0 {
        crate::stats_writer::send(crate::stats_writer::StatsEvent::Record(game, record_id));
    }
    Ok(())
}

/// [`record`] for a round that gives every participant a record of their own:
/// `rows` are `(user, record_id, delta)`, written in one statement. The caller
/// already holds these participants' locks ([`crate::db::lock_game_participants`]).
pub async fn record_each(
    conn: &mut PgConnection,
    game: i16,
    session_id: Option<Uuid>,
    played_at: DateTime<Utc>,
    has_bots: bool,
    rows: &[(Uuid, Uuid, i64)],
) -> sqlx::Result<()> {
    if rows.is_empty() {
        return Ok(());
    }
    let users: Vec<Uuid> = rows.iter().map(|r| r.0).collect();
    let records: Vec<Uuid> = rows.iter().map(|r| r.1).collect();
    let amounts: Vec<i64> = rows.iter().map(|r| r.2).collect();
    let inserted: Vec<Uuid> = sqlx::query_scalar(
        "INSERT INTO game_records (user_id, game, record_id, session_id, played_at, delta, has_bots) \
         SELECT t.user_id, $1, t.record_id, $2, $3, t.delta, $4 \
         FROM unnest($5::uuid[], $6::uuid[], $7::bigint[]) WITH ORDINALITY AS t(user_id, record_id, delta, n) \
         ORDER BY t.n \
         ON CONFLICT (user_id, game, record_id) DO NOTHING RETURNING record_id",
    )
    .bind(game)
    .bind(session_id)
    .bind(played_at)
    .bind(has_bots)
    .bind(&users)
    .bind(&records)
    .bind(&amounts)
    .fetch_all(conn.writer())
    .await?;
    // ON CONFLICT DO NOTHING: only a record that was inserted counts.
    for record_id in inserted {
        crate::stats_writer::send(crate::stats_writer::StatsEvent::Record(game, record_id));
    }
    Ok(())
}

/// [`record`] for a gold-bean round whose record-centre detail lives on the
/// record itself: `rows` are `(user, delta, detail)`, the detail being the
/// game's own receipt (`unit = "beans"`), so no reader joins the ledger.
pub async fn record_beans(
    conn: &mut PgConnection,
    game: i16,
    record_id: Uuid,
    session_id: Option<Uuid>,
    has_bots: bool,
    rows: &[(Uuid, i64, serde_json::Value)],
) -> sqlx::Result<()> {
    if rows.is_empty() {
        return Ok(());
    }
    let users: Vec<Uuid> = rows.iter().map(|r| r.0).collect();
    let amounts: Vec<i64> = rows.iter().map(|r| r.1).collect();
    let details: Vec<String> = rows.iter().map(|r| r.2.to_string()).collect();
    let inserted = sqlx::query(
        "INSERT INTO game_records (user_id, game, record_id, session_id, played_at, delta, detail, has_bots) \
         SELECT t.user_id, $1, $2, $3, now(), t.delta, t.detail::jsonb, $4 \
         FROM unnest($5::uuid[], $6::bigint[], $7::text[]) AS t(user_id, delta, detail) ORDER BY t.user_id \
         ON CONFLICT (user_id, game, record_id) DO NOTHING",
    )
    .bind(game)
    .bind(record_id)
    .bind(session_id)
    .bind(has_bots)
    .bind(&users)
    .bind(&amounts)
    .bind(&details)
    .execute(conn.writer())
    .await?
    .rows_affected();
    if inserted > 0 {
        crate::stats_writer::send(crate::stats_writer::StatsEvent::Record(game, record_id));
    }
    Ok(())
}

/// Session groups use immutable lifetime ids; records without one are independent rounds.
// Each argument is one history filter; callers pass them positionally.
#[allow(clippy::too_many_arguments)]
pub async fn load_history_page_filtered(
    pool: &PgPool,
    user_id: Uuid,
    game_code: Option<i16>,
    limit: i64,
    before: Option<(DateTime<Utc>, Uuid, i16)>,
    sessions: bool,
    session_id: Option<Uuid>,
    opponent: Opponent,
) -> sqlx::Result<RecordsResponse> {
    use crate::request_timing::{http_phase, HttpPhase};
    let prepare = http_phase(HttpPhase::Processing).operation("records_query_prepare");
    // Group only this player's indexed records; hydrate details after paging.
    // `game_records.has_bots` holds the per-game opponent evidence (schema
    // 0006, maintained with the record and its sources); NULL is unknown and
    // never matches a filter. Static fragments keep the opponent index usable.
    let opponent_filter = match opponent.has_bots() {
        None => "TRUE",
        Some(true) => "r.has_bots = true",
        Some(false) => "r.has_bots = false",
    };
    let mut query = sqlx::QueryBuilder::<sqlx::Postgres>::new(format!(
        "WITH filtered_records AS NOT MATERIALIZED (SELECT r.* FROM game_records r \
         WHERE r.user_id=$1 AND {opponent_filter}), page AS MATERIALIZED ("
    ));
    if sessions {
        // A record without a session is its own group; its session_id stays
        // NULL so callers never treat a record id as a session. 鱿鱼 rounds
        // belong to their poker room's 局: they join its net, not its hands.
        let (poker, squid) = (game::POKER, game::SQUID);
        query.push(format!(
            "SELECT game, group_id AS record_id, CASE WHEN grouped_records > 0 THEN group_id END AS session_id, \
             played_at, delta, CASE WHEN hands > 0 THEN hands ELSE records END AS hands, started_at FROM (SELECT user_id, \
             CASE WHEN game={squid} AND session_id IS NOT NULL THEN {poker}::smallint ELSE game END AS game, \
             COALESCE(session_id,record_id) AS group_id, \
             max(played_at) AS played_at, min(played_at) AS started_at, \
             sum(delta)::bigint AS delta, count(*) AS records, count(*) FILTER (WHERE game<>{squid}) AS hands, \
             count(session_id) AS grouped_records FROM filtered_records \
             WHERE user_id=$1 GROUP BY user_id,2,COALESCE(session_id,record_id)) grouped WHERE user_id = ",
        ));
        query.push_bind(user_id);
        if let Some(game_code) = game_code {
            query.push(" AND game = ").push_bind(game_code);
        }
        if let Some(id) = session_id {
            query.push(" AND group_id = ").push_bind(id);
        }
    } else if let Some(id) = session_id {
        query.push(
            "SELECT r.game, r.record_id, r.session_id, r.played_at, r.delta \
             FROM filtered_records r WHERE r.user_id = ",
        );
        query
            .push_bind(user_id)
            .push(" AND COALESCE(r.session_id, r.record_id) = ")
            .push_bind(id);
        if let Some(game_code) = game_code {
            query.push(" AND r.game = ").push_bind(game_code);
        }
    } else {
        query.push(
            "SELECT game, record_id, session_id, played_at, delta FROM filtered_records WHERE user_id = ",
        );
        query.push_bind(user_id);
        if let Some(game_code) = game_code {
            query.push(" AND game = ").push_bind(game_code);
        }
    }
    let (at, id_col, game_col) = if sessions || session_id.is_none() {
        (
            "played_at",
            if sessions { "group_id" } else { "record_id" },
            "game",
        )
    } else {
        ("r.played_at", "r.record_id", "r.game")
    };
    if let Some((before, id, code)) = before {
        query
            .push(format!(" AND ({at}, {id_col}, {game_col}) < ("))
            .push_bind(before)
            .push(", ")
            .push_bind(id)
            .push(", ")
            .push_bind(code)
            .push(")");
    }
    query
        .push(format!(
            " ORDER BY {at} DESC, {id_col} DESC, {game_col} DESC LIMIT "
        ))
        .push_bind(limit + 1);
    // Return totals and the bounded page in one snapshot and one round trip.
    // Tagged rows avoid JSON encoding of ordinary SQL fields and keep totals
    // available even when the requested page is empty.
    query.push(
        ") SELECT 0::smallint AS row_kind, t.game AS summary_order, t.game, \
         NULL::uuid AS record_id, NULL::uuid AS session_id, \
         t.last_played_at AS played_at, NULL::bigint AS delta, NULL::jsonb AS detail, \
         t.played, t.net FROM (SELECT game, max(played_at) AS last_played_at, count(*) AS played, \
         CASE WHEN game IN (",
    );
    // Card-game points stay in their room, so those games have no account net.
    query.push(card_codes());
    query.push(
        ") THEN NULL::bigint ELSE sum(delta)::bigint END AS net FROM filtered_records WHERE user_id=$1 GROUP BY game) t \
         UNION ALL SELECT 1::smallint, NULL::smallint, page.game, page.record_id, \
         page.session_id, page.played_at, page.delta, ",
    );
    if sessions {
        // The room games whose session is a points or a gold-bean room (card,
        // board and dealer rooms); the bean receipt tells which.
        let unit_codes = crate::games::code_list(|entry| {
            entry.table() || entry.family == crate::games::Family::Dealer
        });
        // A 掼蛋 match is one record holding all of its deals.
        let guandan = game::GUANDAN;
        let beans = game::BEANS;
        query.push(format!("jsonb_build_object('hands', CASE WHEN page.game={guandan} THEN COALESCE((SELECT jsonb_array_length(r.detail->'results') FROM filtered_records r WHERE r.game={guandan} AND r.user_id=$1 AND r.record_id=page.record_id),page.hands) ELSE page.hands END, 'started_at', page.started_at, \
             'unit', CASE WHEN page.game IN (")).push(unit_codes).push(format!(") THEN (SELECT CASE WHEN EXISTS (SELECT 1 FROM wallet_ledger l WHERE l.game={beans} AND l.record_id=f.record_id AND l.user_id=$1 \
             AND l.kind='play') \
             THEN 'beans' ELSE 'practice' END FROM (SELECT r.record_id FROM game_records r WHERE r.user_id=$1 AND r.game=page.game \
             AND COALESCE(r.session_id,r.record_id)=page.record_id LIMIT 1) f) END)"));
    } else {
        // Hydrated below by a statement planned only for this page's games.
        query.push("NULL::jsonb");
    }
    query.push(
        ", NULL::bigint, NULL::bigint FROM page \
         ORDER BY row_kind, summary_order, played_at DESC, record_id DESC, game DESC",
    );
    // The page and its detail statement are sequential: one pooled connection
    // serves both instead of an acquire (and idle-connection ping) each.
    drop(prepare);
    let acquire = http_phase(HttpPhase::DbPool).operation("records_pool_acquire");
    let mut conn = pool.acquire().await?;
    drop(acquire);
    let page = http_phase(HttpPhase::DbQuery).operation("records_page_query");
    let rows = query.build().fetch_all(&mut *conn).await?;
    drop(page);
    let decode = http_phase(HttpPhase::Processing).operation("records_page_decode");
    let summary = rows
        .iter()
        .filter(|r| r.get::<i16, _>("row_kind") == 0)
        .map(|r| GameSummary {
            game: game::name(r.get("game")),
            played: r.get("played"),
            net: r.get("net"),
            last_played_at: r.get("played_at"),
        })
        .collect();

    let page_rows: Vec<_> = rows
        .iter()
        .filter(|r| r.get::<i16, _>("row_kind") == 1)
        .collect();
    let has_more = page_rows.len() as i64 > limit;
    let page_rows: Vec<_> = page_rows.into_iter().take(limit as usize).collect();
    let mut details = if sessions {
        drop(decode);
        std::collections::HashMap::new()
    } else {
        let keys: Vec<(i16, Uuid)> = page_rows
            .iter()
            .map(|r| (r.get("game"), r.get("record_id")))
            .collect();
        drop(decode);
        load_page_details(&mut conn, user_id, &keys).await?
    };
    let _assemble = http_phase(HttpPhase::Processing).operation("records_response_assemble");
    let entries: Vec<RecordEntry> = page_rows
        .into_iter()
        .map(|r| {
            let code: i16 = r.get("game");
            let record_id: Uuid = r.get("record_id");
            let detail = if sessions {
                r.get::<Option<Value>, _>("detail")
            } else {
                details.remove(&(code, record_id)).flatten()
            };
            RecordEntry {
                game: game::name(code),
                record_id,
                session_id: r.get("session_id"),
                played_at: r.get("played_at"),
                delta: r.get("delta"),
                detail: detail.unwrap_or(Value::Null),
            }
        })
        .collect();
    let next_cursor = if has_more {
        entries.last().map(|entry| RecordCursor {
            before: entry.played_at,
            before_id: entry.record_id,
            before_game: entry.game,
        })
    } else {
        None
    };
    Ok(RecordsResponse {
        summary,
        entries,
        next_cursor,
    })
}

fn card_codes() -> String {
    crate::games::code_list(|entry| entry.family == crate::games::Family::Card)
}

/// Every game without its own [`crate::games::Entry::record_detail`] keeps
/// its detail, unit included, on the record itself (match results, 百家乐
/// hands and room rounds, 押大小 rolls).
const RECORD_DETAIL: &str = r#"(SELECT r.detail FROM game_records r WHERE r.user_id=$1 AND r.game=page.game AND r.record_id=page.record_id)"#;

/// 鱿鱼 rounds are a poker side game, not a registry entry.
const SQUID_DETAIL: &str = r#"(SELECT jsonb_build_object(
    'round_number', r.round_number, 'mode', r.mode, 'calculation', r.calculation,
    'bb_size', r.bb_size, 'total_amount', abs(r.delta), 'lost', r.delta<0)
    FROM squid_player_rounds r WHERE r.id=page.record_id AND r.user_id=$1)"#;

/// The detail expression of one record code; `None` for an unknown code.
fn detail_expression(code: i16) -> Option<&'static str> {
    if code == game::SQUID {
        return Some(SQUID_DETAIL);
    }
    crate::games::by_code(code).map(|entry| entry.record_detail.unwrap_or(RECORD_DETAIL))
}

/// Details for one bounded page, in a second statement whose CASE carries only
/// the arms for games on this page. Planning every game's correlated lookup
/// for every request cost more than executing the page (prod 2026-10-30:
/// 1.4 ms warm / 6.8 ms cold plan against ~1.8 ms execution).
async fn load_page_details(
    conn: &mut crate::db_writer::PgConnection,
    user_id: Uuid,
    keys: &[(i16, Uuid)],
) -> sqlx::Result<std::collections::HashMap<(i16, Uuid), Option<Value>>> {
    use crate::request_timing::{http_phase, HttpPhase};
    let prepare = http_phase(HttpPhase::Processing).operation("records_details_prepare");
    // One CASE arm per distinct expression on this page (all card games share one).
    let mut arms: Vec<(Vec<i16>, &str)> = Vec::new();
    for &(code, _) in keys {
        let Some(expression) = detail_expression(code) else {
            continue;
        };
        match arms.iter_mut().find(|(_, e)| *e == expression) {
            Some((codes, _)) if codes.contains(&code) => {}
            Some((codes, _)) => codes.push(code),
            None => arms.push((vec![code], expression)),
        }
    }
    if arms.is_empty() {
        return Ok(std::collections::HashMap::new());
    }
    let mut sql = String::from("SELECT page.game, page.record_id, COALESCE(CASE");
    for (games, expression) in arms {
        let games: Vec<String> = games.iter().map(i16::to_string).collect();
        sql.push_str(&format!(
            " WHEN page.game IN ({}) THEN {expression}",
            games.join(",")
        ));
    }
    sql.push_str(
        " ELSE NULL END, '{}'::jsonb) || CASE WHEN EXISTS(SELECT 1 FROM wallet_ledger l WHERE l.game=$4 AND l.record_id=page.record_id AND l.user_id=$1 AND l.kind='play') OR EXISTS(SELECT 1 FROM game_records kept WHERE kept.user_id=$1 AND kept.game=page.game AND kept.record_id=page.record_id AND kept.detail->>'unit'='beans') THEN jsonb_build_object('unit','beans') ELSE '{}'::jsonb END AS detail FROM unnest($2::smallint[], $3::uuid[]) AS page(game, record_id)",
    );
    let (games, ids): (Vec<i16>, Vec<Uuid>) = keys.iter().copied().unzip();
    drop(prepare);
    let query = http_phase(HttpPhase::DbQuery).operation("records_details_query");
    let rows = sqlx::query(&sql)
        .bind(user_id)
        .bind(games)
        .bind(ids)
        .bind(game::BEANS)
        .fetch_all(conn)
        .await?;
    drop(query);
    let _decode = http_phase(HttpPhase::Processing).operation("records_details_decode");
    Ok(rows
        .into_iter()
        .map(|row| {
            (
                (row.get("game"), row.get("record_id")),
                row.get::<Option<Value>, _>("detail"),
            )
        })
        .collect())
}
