//! Cross-game record repository: transactional writes and bounded, user-scoped reads.
//! Replay payloads and game rules belong to their game modules.
use crate::game_catalog as game;
use chrono::{DateTime, Utc};
use serde::Serialize;
use serde_json::Value;
use sqlx::{PgConnection, PgPool, Row};
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
    /// Pass back as `before` to fetch the next page; `null` at the end.
    pub next_before: Option<DateTime<Utc>>,
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
    deltas: &[(Uuid, i64)],
) -> sqlx::Result<()> {
    if deltas.is_empty() {
        return Ok(());
    }
    let users: Vec<Uuid> = deltas.iter().map(|d| d.0).collect();
    let amounts: Vec<i64> = deltas.iter().map(|d| d.1).collect();
    crate::db::lock_game_participants(conn, &users).await?;
    let inserted = sqlx::query(
        "INSERT INTO game_records (user_id, game, record_id, session_id, played_at, delta) \
         SELECT t.user_id, $1, $2, $3, $4, t.delta \
         FROM unnest($5::uuid[], $6::bigint[]) AS t(user_id, delta) ORDER BY t.user_id \
         ON CONFLICT (user_id, game, record_id) DO NOTHING",
    )
    .bind(game)
    .bind(record_id)
    .bind(session_id)
    .bind(played_at)
    .bind(&users)
    .bind(&amounts)
    .execute(conn)
    .await?
    .rows_affected();
    // ON CONFLICT DO NOTHING: a replay that inserted nothing counts nothing.
    if inserted > 0 {
        crate::stats_writer::send(crate::stats_writer::StatsEvent::Record(game, record_id));
    }
    Ok(())
}

pub async fn load(
    pool: &PgPool,
    user_id: Uuid,
    game_code: Option<i16>,
    limit: i64,
    before: Option<DateTime<Utc>>,
) -> sqlx::Result<RecordsResponse> {
    load_page(pool, user_id, game_code, limit, before, None, None).await
}

pub async fn load_page(
    pool: &PgPool,
    user_id: Uuid,
    game_code: Option<i16>,
    limit: i64,
    before: Option<DateTime<Utc>>,
    before_id: Option<Uuid>,
    before_game: Option<i16>,
) -> sqlx::Result<RecordsResponse> {
    load_history_page(
        pool,
        user_id,
        game_code,
        limit,
        before,
        before_id,
        before_game,
        false,
        None,
    )
    .await
}

/// Session groups use immutable lifetime ids; records without one are independent rounds.
// Each argument is one history filter; callers pass them positionally.
#[allow(clippy::too_many_arguments)]
pub async fn load_history_page(
    pool: &PgPool,
    user_id: Uuid,
    game_code: Option<i16>,
    limit: i64,
    before: Option<DateTime<Utc>>,
    before_id: Option<Uuid>,
    before_game: Option<i16>,
    sessions: bool,
    session_id: Option<Uuid>,
) -> sqlx::Result<RecordsResponse> {
    load_history_page_filtered(
        pool,
        user_id,
        game_code,
        limit,
        before,
        before_id,
        before_game,
        sessions,
        session_id,
        Opponent::All,
    )
    .await
}

#[allow(clippy::too_many_arguments)]
pub async fn load_history_page_filtered(
    pool: &PgPool,
    user_id: Uuid,
    game_code: Option<i16>,
    limit: i64,
    before: Option<DateTime<Utc>>,
    before_id: Option<Uuid>,
    before_game: Option<i16>,
    sessions: bool,
    session_id: Option<Uuid>,
    opponent: Opponent,
) -> sqlx::Result<RecordsResponse> {
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
        query.push(
            "SELECT game, group_id AS record_id, group_id AS session_id, played_at, delta, \
             records AS hands, started_at FROM (SELECT user_id, game, COALESCE(session_id,record_id) AS group_id, \
             max(played_at) AS played_at, min(played_at) AS started_at, \
             sum(delta)::bigint AS delta, count(*) AS records FROM filtered_records \
             WHERE user_id=$1 GROUP BY user_id,game,COALESCE(session_id,record_id)) grouped WHERE user_id = ",
        );
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
    if let Some(before) = before {
        if let (Some(id), Some(code)) = (before_id, before_game) {
            query
                .push(format!(" AND ({at}, {id_col}, {game_col}) < ("))
                .push_bind(before)
                .push(", ")
                .push_bind(id)
                .push(", ")
                .push_bind(code)
                .push(")");
        } else {
            query.push(format!(" AND {at} < ")).push_bind(before);
        }
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
         CASE WHEN game IN (1,9,22) THEN NULL::bigint ELSE sum(delta)::bigint END AS net FROM filtered_records WHERE user_id=$1 GROUP BY game) t \
         UNION ALL SELECT 1::smallint, NULL::smallint, page.game, page.record_id, \
         page.session_id, page.played_at, page.delta, ",
    );
    if sessions {
        // A baccarat session is all practice-chip room rounds or all solo
        // gold-bean hands; its first record tells which.
        query.push("jsonb_build_object('hands', CASE WHEN page.game=5 THEN COALESCE((SELECT jsonb_array_length(r.detail->'results') FROM filtered_records r WHERE r.game=5 AND r.user_id=$1 AND r.record_id=page.record_id),page.hands) ELSE page.hands END, 'started_at', page.started_at, \
             'unit', CASE WHEN page.game IN (1,8,9,22) THEN (SELECT CASE WHEN EXISTS (SELECT 1 FROM wallet_ledger l WHERE l.game=10 AND l.record_id=f.record_id AND l.user_id=$1 \
             AND l.kind='play' AND (l.details->>'unit'='beans' OR l.details->>'game'='baccarat')) \
             THEN 'beans' ELSE 'practice' END FROM (SELECT r.record_id FROM game_records r WHERE r.user_id=$1 AND r.game=page.game \
             AND COALESCE(r.session_id,r.record_id)=page.session_id LIMIT 1) f) END)");
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
    let mut conn = pool.acquire().await?;
    let rows = query.build().fetch_all(&mut *conn).await?;
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
        std::collections::HashMap::new()
    } else {
        let keys: Vec<(i16, Uuid)> = page_rows
            .iter()
            .map(|r| (r.get("game"), r.get("record_id")))
            .collect();
        load_page_details(&mut conn, user_id, &keys).await?
    };
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
    let next_before = if has_more {
        entries.last().map(|e| e.played_at)
    } else {
        None
    };

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
        next_before,
        next_cursor,
    })
}

/// Detail expression per game for one page row `page(game, record_id)`;
/// `$1` is the viewer. Each arm retains the original caller filter; missing
/// legacy detail stays null. Ledger record ids can appear more than once, so
/// choose the latest entry deterministically instead of relying on an
/// unordered HashMap overwrite.
const DETAIL_ARMS: &[(&[i16], &str)] = &[
    (
        &[1, 2, 9, 12, 13, 22],
        r#"(SELECT jsonb_build_object(
    'room_code', h.room_code, 'pot', h.pot, 'ante', h.ante, 'showdown', h.showdown,
    'unit', CASE WHEN EXISTS(SELECT 1 FROM wallet_ledger l WHERE l.game=10 AND l.user_id=$1 AND l.record_id=h.id AND l.details->>'unit'='beans') THEN 'beans' ELSE 'practice' END,
    'players', jsonb_array_length(h.players),
    'cards', (SELECT p->'cards' FROM jsonb_array_elements(h.players) p WHERE p->>'user_id'=$1::text LIMIT 1),
    'has_bots', (SELECT bool_or(p->'user_id'='null'::jsonb) FROM jsonb_array_elements(h.players) p))
    FROM game_rounds h WHERE h.game=page.game AND h.id=page.record_id)"#,
    ),
    (
        &[0],
        r#"(SELECT jsonb_build_object(
    'bb_size', h.bb_size, 'bot_count', h.bot_count,
    'position', p.position, 'rank_name', p.rank_name, 'engine_blind', h.engine_blind)
    FROM hands h JOIN hand_players p ON p.hand_id=h.id AND p.user_id=$1
    WHERE h.id=page.record_id)"#,
    ),
    (
        &[3],
        r#"(SELECT jsonb_build_object(
    'room_code', l.room_code, 'revision', l.revision, 'has_bots', l.has_bots)
    FROM wallet_ledger l WHERE l.game=3 AND l.user_id=$1 AND l.record_id=page.record_id ORDER BY l.id DESC LIMIT 1)"#,
    ),
    (
        &[4],
        r#"(SELECT jsonb_build_object(
    'round_number', r.round_number, 'mode', r.mode, 'calculation', r.calculation,
    'bb_size', r.bb_size, 'total_amount', abs(r.delta), 'lost', r.delta<0)
    FROM squid_player_rounds r WHERE r.id=page.record_id AND r.user_id=$1)"#,
    ),
    (
        &[5],
        r#"(SELECT jsonb_build_object(
    'code', r.detail->'code', 'team', r.detail->'team', 'winner', r.detail->'winner',
    'deals', r.detail->'deals', 'results', r.detail->'results', 'levels', r.detail->'levels', 'has_bots', r.detail->'has_bots',
    'won', (r.detail->'winner')=(r.detail->'team'))
    FROM (SELECT * FROM game_records WHERE game=5) r WHERE r.user_id=$1 AND r.record_id=page.record_id)"#,
    ),
    (
        &[6],
        r#"(SELECT jsonb_build_object(
    'code', r.detail->'code', 'stone', r.detail->'stone', 'outcome', r.detail->'outcome',
    'reason', r.detail->'reason', 'moves', r.detail->'moves', 'opponent', r.detail->'opponent',
    'level', r.detail->'level', 'has_bots', r.detail->'has_bots')
    FROM (SELECT * FROM game_records WHERE game=6) r WHERE r.user_id=$1 AND r.record_id=page.record_id)"#,
    ),
    (
        &[20],
        r#"(SELECT r.detail FROM game_records r WHERE r.game=20 AND r.user_id=$1 AND r.record_id=page.record_id)"#,
    ),
    (
        &[15],
        r#"(SELECT r.detail FROM game_records r WHERE r.game=15 AND r.user_id=$1 AND r.record_id=page.record_id)"#,
    ),
    (
        &[21],
        r#"(SELECT r.detail FROM game_records r WHERE r.game=21 AND r.user_id=$1 AND r.record_id=page.record_id)"#,
    ),
    (
        &[14],
        r#"(SELECT r.detail FROM (SELECT * FROM game_records WHERE game=14) r WHERE r.user_id=$1 AND r.record_id=page.record_id)"#,
    ),
    (
        &[7],
        r#"(SELECT jsonb_build_object(
    'room_code', m.room_code, 'stake', m.stake, 'reason', m.reason,
    'moves', jsonb_array_length(m.moves),
    'side', CASE WHEN m.red_user_id=$1 THEN 'red' ELSE 'black' END,
    'result', CASE WHEN m.winner IS NULL THEN 'draw'
                  WHEN (m.winner='red' AND m.red_user_id=$1) OR (m.winner='black' AND m.black_user_id=$1) THEN 'win' ELSE 'loss' END,
    'has_bots', (m.red_user_id IS NULL OR m.black_user_id IS NULL))
    FROM (SELECT id,room_code,started_at,ended_at,(detail->>'stake')::bigint AS stake,(detail->>'red_user_id')::uuid AS red_user_id,(detail->>'black_user_id')::uuid AS black_user_id,detail->>'red_name' AS red_name,detail->>'black_name' AS black_name,detail->>'winner' AS winner,detail->>'reason' AS reason,detail->'moves' AS moves FROM game_rounds WHERE game=7) m WHERE m.id=page.record_id)"#,
    ),
    (
        &[8],
        r#"(SELECT COALESCE(details,'{}'::jsonb) || jsonb_build_object('unit', CASE WHEN game=8 THEN 'practice' ELSE 'beans' END)
    FROM wallet_ledger WHERE (game=8 OR (game=10 AND kind='play' AND details->>'game'='baccarat')) AND user_id=$1 AND record_id=page.record_id ORDER BY id DESC LIMIT 1)"#,
    ),
    (
        &[11],
        r#"(SELECT COALESCE(details,'{}'::jsonb) || jsonb_build_object('unit', CASE WHEN game=11 THEN 'practice' ELSE 'beans' END)
    FROM wallet_ledger WHERE (game=11 OR (game=10 AND details->>'game'='sicbo')) AND kind='play' AND user_id=$1 AND record_id=page.record_id ORDER BY id ASC LIMIT 1)"#,
    ),
];

/// Details for one bounded page, in a second statement whose CASE carries only
/// the arms for games on this page. Planning every game's correlated lookup
/// for every request cost more than executing the page (prod 2026-10-30:
/// 1.4 ms warm / 6.8 ms cold plan against ~1.8 ms execution).
async fn load_page_details(
    conn: &mut sqlx::PgConnection,
    user_id: Uuid,
    keys: &[(i16, Uuid)],
) -> sqlx::Result<std::collections::HashMap<(i16, Uuid), Option<Value>>> {
    let arms: Vec<_> = DETAIL_ARMS
        .iter()
        .filter(|(games, _)| keys.iter().any(|(code, _)| games.contains(code)))
        .collect();
    if arms.is_empty() {
        return Ok(std::collections::HashMap::new());
    }
    let mut sql = String::from("SELECT page.game, page.record_id, CASE");
    for (games, expression) in arms {
        let games: Vec<String> = games.iter().map(i16::to_string).collect();
        sql.push_str(&format!(
            " WHEN page.game IN ({}) THEN {expression}",
            games.join(",")
        ));
    }
    sql.push_str(
        " ELSE NULL END AS detail FROM unnest($2::smallint[], $3::uuid[]) AS page(game, record_id)",
    );
    let (games, ids): (Vec<i16>, Vec<Uuid>) = keys.iter().copied().unzip();
    let rows = sqlx::query(&sql)
        .bind(user_id)
        .bind(games)
        .bind(ids)
        .fetch_all(conn)
        .await?;
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
