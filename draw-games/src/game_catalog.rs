//! Stable game identities shared by storage, records and discovery.
//! Persisted numeric codes are never renumbered; squid is a poker side game.

pub const TEXAS: i16 = 0;
pub const ZJH: i16 = 1;
pub const DDZ: i16 = 2;
pub const KWX: i16 = 3;
pub const SQUID: i16 = 4;
pub const GUANDAN: i16 = 5;
pub const GOMOKU: i16 = 6;
pub const XIANGQI: i16 = 7;
pub const NIUNIU: i16 = 9;
pub const SANGONG: i16 = 12;
pub const BLACKJACK: i16 = 13;
pub const CHECKERS: i16 = 14;
pub const COLORCARDS: i16 = 15;
pub const ZHUOJI: i16 = 21;
pub const CHENGDU: i16 = 20;
pub const SICBO: i16 = 11;
pub const SHORTDECK: i16 = 22;

pub const BACCARAT: i16 = 8;
/// Platform-wide 金豆 wallet shared by public tables and lottery games. Not a
/// game: never listed in `ALL`, never produces game records.
pub const BEANS: i16 = 10;
/// Retired lottery wallet identity, retained for historical receipts.
pub const LOTTERY: i16 = 23;
pub const ALL: [i16; 18] = [
    TEXAS, ZJH, DDZ, KWX, SQUID, GUANDAN, GOMOKU, XIANGQI, BACCARAT, NIUNIU, SICBO, SANGONG,
    BLACKJACK, CHECKERS, COLORCARDS, ZHUOJI, CHENGDU, SHORTDECK,
];

/// Wire name used by `/api/records` and the client game catalog.
pub fn name(code: i16) -> &'static str {
    match code {
        TEXAS => "poker",
        ZJH => "zjh",
        SHORTDECK => "shortdeck",
        DDZ => "ddz",
        KWX => "kwx",
        SQUID => "squid",
        GUANDAN => "guandan",
        GOMOKU => "gomoku",
        CHECKERS => "checkers",
        COLORCARDS => "colorcards",
        ZHUOJI => "zhuoji",
        CHENGDU => "chengdu",
        XIANGQI => "xiangqi",
        BACCARAT => "baccarat",
        SICBO => "sicbo",
        NIUNIU => "niuniu",
        SANGONG => "sangong",
        BLACKJACK => "blackjack",
        BEANS => "beans",
        LOTTERY => "lottery",
        _ => "unknown",
    }
}

pub fn from_name(name: &str) -> Option<i16> {
    ALL.into_iter().find(|code| self::name(*code) == name)
}

/// Discovery order, including games with no account record rows.
pub const HUB_GAMES: [&str; 26] = [
    "poker",
    "shortdeck",
    "baccarat",
    "niuniu",
    "sangong",
    "zjh",
    "ddz",
    "kwx",
    "zhuoji",
    "chengdu",
    "guandan",
    "gomoku",
    "colorcards",
    "checkers",
    "xiangqi",
    "sicbo",
    "blackjack",
    "scratch",
    "claw",
    "slots",
    "toto",
    "ssq",
    "dlt",
    "happy8",
    "fucai3d",
    "2048",
];

/// Hub games in the 金豆专区 tab, in hub order: the client's `BEAN_GAME_IDS`
/// (client-game/src/lib/gameCatalog.ts) minus its two tools, which have no
/// game route. Keep the two lists in step.
pub const BEAN_HUB_GAMES: [&str; 15] = [
    "poker",
    "shortdeck",
    "niuniu",
    "zjh",
    "baccarat",
    "sicbo",
    "scratch",
    "claw",
    "slots",
    "toto",
    "ssq",
    "dlt",
    "happy8",
    "fucai3d",
    "2048",
];

/// Singapore release day of recently shipped hub games, mirroring the client's
/// `GAME_LAUNCH_DATES`; the hub lists a game under 新游戏 for
/// `NEW_GAME_WINDOW_DAYS` days. Add each new game in both places.
pub const GAME_LAUNCH_DATES: [(&str, &str); 5] = [
    ("ssq", "2026-10-16"),
    ("dlt", "2026-10-23"),
    ("happy8", "2026-10-16"),
    ("fucai3d", "2026-10-16"),
    ("shortdeck", "2026-10-21"),
];
pub const NEW_GAME_WINDOW_DAYS: i64 = 30;
