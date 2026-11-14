//! Stable game identities shared by storage, records and discovery.
//!
//! Every hub game comes from the one registry,
//! `client-game/src/lib/gameRegistry.json` (also read by the H5, website and
//! E2E runners); `build.rs` turns it into `GAMES` plus one `pub const <ID>: i16`
//! per game with a persisted `recordCode`. Persisted codes are never renumbered
//! or reused; squid is a poker side game and the wallets below are not games.

/// One registry row, in hub order.
#[derive(Debug, Clone, Copy)]
pub struct Game {
    pub id: &'static str,
    pub zh: &'static str,
    pub en: &'static str,
    pub category: &'static str,
    /// `room` games have a table route; `solo` games play on their lobby page.
    pub kind: &'static str,
    /// Listed in the 金豆专区 tab.
    pub beans: bool,
    /// Settles through the lottery center.
    pub lottery: bool,
    /// Its rooms can be published to a club's room list.
    pub club_rooms: bool,
    /// A sign-up invited from one of its live rooms credits the room host
    /// (`handlers::registration_referral`).
    pub room_referral: bool,
    /// A room game that also has a no-table route reporting `solo_game_play`.
    pub solo_play: bool,
    /// Singapore release day of a recently shipped game.
    pub launched: Option<&'static str>,
    pub record_code: Option<i16>,
    /// `/api/games/{id}/…` resources, named after the `crate::games::Api` fields.
    pub api: &'static [&'static str],
}

include!(concat!(env!("OUT_DIR"), "/game_registry.rs"));

/// Poker side game with its own record rows; not a hub entry.
pub const SQUID: i16 = 4;
/// Platform-wide 金豆 wallet shared by public tables and lottery games. Not a
/// game: never listed in `ALL`, never produces game records.
pub const BEANS: i16 = 10;

/// Identities outside the registry, kept with their wire names.
const EXTRA_NAMES: [(i16, &str); 2] = [(SQUID, "squid"), (BEANS, "beans")];

/// One row of the database's `public.games` (schema 0132).
pub struct CatalogRow {
    pub slug: &'static str,
    pub code: Option<i16>,
    pub kind: &'static str,
    pub lottery: bool,
    pub solo_play: bool,
}

/// Identities SQL knows beside the hub registry: the poker side game with its
/// own records, tournaments (rated as their own game) and the two bean tools.
const OFF_HUB: [(&str, Option<i16>, &str); 4] = [
    ("squid", Some(SQUID), "side"),
    ("tournament", None, "event"),
    ("call-fold", None, "tool"),
    ("lucky-draw", None, "tool"),
];

/// What `db::initialize` writes to `public.games` on every start: adding a
/// game to the registry is all its SQL lookups need.
pub fn catalog_rows() -> Vec<CatalogRow> {
    GAMES
        .iter()
        .map(|game| CatalogRow {
            slug: game.id,
            code: game.record_code,
            kind: game.kind,
            lottery: game.lottery,
            solo_play: crate::handlers::telemetry::SOLO_GAMES.contains(&game.id),
        })
        .chain(OFF_HUB.iter().map(|&(slug, code, kind)| CatalogRow {
            slug,
            code,
            kind,
            lottery: false,
            solo_play: false,
        }))
        .collect()
}

/// Games that write account record rows: every registry `recordCode` plus squid.
pub fn all() -> impl Iterator<Item = i16> {
    GAMES
        .iter()
        .filter_map(|game| game.record_code)
        .chain([SQUID])
}

/// Wire name used by `/api/records` and the client game catalog.
pub fn name(code: i16) -> &'static str {
    GAMES
        .iter()
        .find(|game| game.record_code == Some(code))
        .map(|game| game.id)
        .or_else(|| {
            EXTRA_NAMES
                .iter()
                .find(|(c, _)| *c == code)
                .map(|(_, n)| *n)
        })
        .unwrap_or("unknown")
}

pub fn from_name(name: &str) -> Option<i16> {
    all().find(|code| self::name(*code) == name)
}

/// The registry row for a hub game id.
pub fn game(id: &str) -> Option<&'static Game> {
    GAMES.iter().find(|game| game.id == id)
}

/// `/ws/<id>` for a room game other than poker (poker's table socket is
/// `/ws`). Those games serve their socket and `/api/games/<id>/rooms…` from
/// the runtime owner. `None` for every other id.
pub fn room_socket(id: &str) -> Option<&'static str> {
    static PATHS: std::sync::LazyLock<Vec<String>> = std::sync::LazyLock::new(|| {
        GAMES
            .iter()
            .filter(|game| game.kind == "room" && game.id != "poker")
            .map(|game| format!("/ws/{}", game.id))
            .collect()
    });
    PATHS
        .iter()
        .map(String::as_str)
        .find(|path| path.strip_prefix("/ws/") == Some(id))
}
