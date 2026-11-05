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
    /// Singapore release day of a recently shipped game.
    pub launched: Option<&'static str>,
    pub record_code: Option<i16>,
    /// `/api/games/{id}/…` resources, named after `handlers::game_api` routes.
    pub api: &'static [&'static str],
}

include!(concat!(env!("OUT_DIR"), "/game_registry.rs"));

/// Poker side game with its own record rows; not a hub entry.
pub const SQUID: i16 = 4;
/// Platform-wide 金豆 wallet shared by public tables and lottery games. Not a
/// game: never listed in `ALL`, never produces game records.
pub const BEANS: i16 = 10;
/// Retired lottery wallet identity, retained for historical receipts.
pub const LOTTERY: i16 = 23;

/// Identities outside the registry, kept with their wire names.
const EXTRA_NAMES: [(i16, &str); 3] = [(SQUID, "squid"), (BEANS, "beans"), (LOTTERY, "lottery")];

/// Games that write account record rows: every registry `recordCode` plus squid.
pub fn all() -> impl Iterator<Item = i16> {
    GAMES.iter().filter_map(|game| game.record_code).chain([SQUID])
}

/// Wire name used by `/api/records` and the client game catalog.
pub fn name(code: i16) -> &'static str {
    GAMES
        .iter()
        .find(|game| game.record_code == Some(code))
        .map(|game| game.id)
        .or_else(|| EXTRA_NAMES.iter().find(|(c, _)| *c == code).map(|(_, n)| *n))
        .unwrap_or("unknown")
}

pub fn from_name(name: &str) -> Option<i16> {
    all().find(|code| self::name(*code) == name)
}

/// The registry row for a hub game id.
pub fn game(id: &str) -> Option<&'static Game> {
    GAMES.iter().find(|game| game.id == id)
}

/// The hub lists a game under 新游戏 for this many days after `launched`.
pub const NEW_GAME_WINDOW_DAYS: i64 = 30;

const fn str_eq(a: &str, b: &str) -> bool {
    let (a, b) = (a.as_bytes(), b.as_bytes());
    if a.len() != b.len() {
        return false;
    }
    let mut i = 0;
    while i < a.len() {
        if a[i] != b[i] {
            return false;
        }
        i += 1;
    }
    true
}

const fn contains(list: &[&str], item: &str) -> bool {
    let mut i = 0;
    while i < list.len() {
        if str_eq(list[i], item) {
            return true;
        }
        i += 1;
    }
    false
}

/// Compile-time check used by `handlers::game_api`: the games a resource
/// dispatches are exactly the registry games that list it in `api`.
pub const fn api_matches(resource: &str, dispatched: &[&str]) -> bool {
    let mut i = 0;
    while i < dispatched.len() {
        let mut known = false;
        let mut g = 0;
        while g < GAMES.len() {
            if str_eq(GAMES[g].id, dispatched[i]) {
                known = true;
            }
            g += 1;
        }
        if !known {
            return false;
        }
        i += 1;
    }
    let mut g = 0;
    while g < GAMES.len() {
        if contains(GAMES[g].api, resource) != contains(dispatched, GAMES[g].id) {
            return false;
        }
        g += 1;
    }
    true
}

/// Every resource named in a registry `api` list has a `game_api` route.
pub const fn api_resources_known(routes: &[&str]) -> bool {
    let mut g = 0;
    while g < GAMES.len() {
        let mut r = 0;
        while r < GAMES[g].api.len() {
            if !contains(routes, GAMES[g].api[r]) {
                return false;
            }
            r += 1;
        }
        g += 1;
    }
    true
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
    PATHS.iter().map(String::as_str).find(|path| path.strip_prefix("/ws/") == Some(id))
}
