//! Paid casual entries and server-authoritative rewards. Local scores never
//! mint beans.
//!
//! Call/Fold and Lucky Draw settle price and prize in the entry play. A 2048
//! entry posts its price and issues the board seed; solving that board posts
//! the reward as a follow-up credit (`casual_reward`) keyed by the same
//! request. Rule for an unfinished 2048 entry: the price is final (it bought
//! the board; a refund would be a free retry), and the reward stays claimable
//! with a valid replay of that board, exactly once, with no deadline.
use rand::Rng;
use serde::Deserialize;
use serde_json::{json, Value};
use uuid::Uuid;

use super::{Keep, Outcome, Refusal, SoloGame, Stake};

/// The rare top result pays this multiple of the game's highest bean prize.
const JACKPOT_MULTIPLE: i64 = 5;
/// A winning Call returns twice its price. Call is drawn 49% of the time, so
/// the house keeps 2% (RTP 98%); there is no jackpot on Call / Fold.
const CALL_PRIZE: i64 = 2;
const CALL_PERCENT: u32 = 49;
/// Lucky Draw costs 2 so its smallest non-zero prize is a whole bean.
const DRAW_TOP_PRIZE: i64 = 20;
/// Reaching 2048 refunds the 1-bean entry. The board seed is client-visible
/// and any 2048 bot wins most boards, so a prize above the entry would be a
/// bean faucet. The rare draw pays the same refund, so RTP never exceeds 100%.
const PUZZLE_PRIZE: i64 = 1;
/// The rare 2048 reward replaces the normal one.
const PUZZLE_JACKPOT_PRIZE: i64 = 1;
const NS: Uuid = Uuid::from_u128(0x5c737949_c677_4992_a296_349e3f68c568);

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Play {
    /// The price the client showed; a changed price is refused.
    cost: i64,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Replay {
    actions: Vec<String>,
}

fn not_found() -> Refusal {
    Refusal::new(axum::http::StatusCode::NOT_FOUND, "entry_not_found")
}

/// Entry ids are shared by the three games: one request is one entry.
fn record(user: Uuid, request: Uuid) -> Uuid {
    Uuid::new_v5(&NS, format!("{user}:{request}").as_bytes())
}

fn stake(play: &Play, cost: i64) -> Result<Stake, Refusal> {
    if play.cost != cost {
        return Err(Refusal::conflict("price_changed"));
    }
    Ok(Stake::of(cost))
}

fn entry(cost: i64, extra: Value, payout: i64, reward: Value) -> Outcome {
    let mut meta = json!({"purpose": "casual_entry", "cost": cost, "reward": reward});
    if let (Value::Object(meta), Value::Object(extra)) = (&mut meta, extra) {
        meta.extend(extra);
    }
    Outcome::paid(payout, cost, meta)
}

fn reply(meta: &Value, balance: i64) -> Value {
    json!({"request_id": meta["request_id"], "cost": meta["cost"], "balance": balance,
        "face": meta["face"], "seed": meta["seed"], "reward": meta["reward"]})
}

macro_rules! casual {
    ($name:ident, $id:literal, $cost:expr) => {
        pub struct $name;
        impl $name {
            const COST: i64 = $cost;
        }
        impl SoloGame for $name {
            const ID: &'static str = $id;
            const KEEP: Keep = Keep::Every;
            const OWNER_CHANGED: &'static str = "account_changed";
            const REQUEST_CONFLICT: &'static str = "request_conflict";
            const NOT_FOUND: &'static str = "entry_not_found";
            type Play = Play;
            fn record_id(user: Uuid, request: Uuid) -> Uuid {
                record(user, request)
            }
            fn stake(play: &Play) -> Result<Stake, Refusal> {
                stake(play, Self::COST)
            }
            fn same_play(play: &Play, meta: &Value) -> bool {
                meta["cost"] == json!(play.cost)
            }
            fn draw(
                _: &Play,
                cost: i64,
                _: Option<&Value>,
                rng: &mut dyn rand::RngCore,
            ) -> Result<Outcome, Refusal> {
                Ok(Self::outcome(cost, rng))
            }
            fn reply(meta: &Value, balance: i64) -> Value {
                reply(meta, balance)
            }
            fn profile() -> Value {
                json!({"cost": Self::COST})
            }
            fn follow_up(entry: &Value, body: Value) -> Result<(i64, Value), Refusal> {
                Self::reward(entry, body)
            }
        }
    };
}

casual!(CallFold, "call-fold", 1);
casual!(LuckyDraw, "lucky-draw", 2);
casual!(Puzzle2048, "2048", 1);

impl CallFold {
    fn outcome(cost: i64, rng: &mut dyn rand::RngCore) -> Outcome {
        let call = rng.gen_ratio(CALL_PERCENT, 100);
        let payout = if call { CALL_PRIZE } else { 0 };
        entry(
            cost,
            json!({"face": if call { "call" } else { "fold" }}),
            payout,
            json!({"beans": payout, "jackpot": false}),
        )
    }
    fn reward(_: &Value, _: Value) -> Result<(i64, Value), Refusal> {
        Err(not_found())
    }
}

impl LuckyDraw {
    fn outcome(cost: i64, rng: &mut dyn rand::RngCore) -> Outcome {
        // Every paid draw has the same disclosed prize distribution.
        let pick = rng.gen_range(0..10_000u32);
        let payout = match pick {
            0..=2999 => 0,
            3000..=6999 => 1,
            7000..=8999 => 2,
            9000..=9899 => 4,
            9900..=9998 => DRAW_TOP_PRIZE,
            _ => JACKPOT_MULTIPLE * DRAW_TOP_PRIZE,
        };
        let jackpot = pick == 9999;
        entry(
            cost,
            json!({}),
            payout,
            json!({"beans": payout, "jackpot": jackpot}),
        )
    }
    fn reward(_: &Value, _: Value) -> Result<(i64, Value), Refusal> {
        Err(not_found())
    }
}

impl Puzzle2048 {
    fn outcome(cost: i64, rng: &mut dyn rand::RngCore) -> Outcome {
        entry(cost, json!({"seed": rng.gen::<u32>()}), 0, Value::Null)
    }
    /// A valid replay of the entry's board reaching 2048 earns the reward.
    fn reward(entry: &Value, body: Value) -> Result<(i64, Value), Refusal> {
        let replay: Replay =
            serde_json::from_value(body).map_err(|_| Refusal::invalid("invalid_replay"))?;
        if replay.actions.len() > 8192 {
            return Err(Refusal::invalid("invalid_replay"));
        }
        let seed = entry["seed"]
            .as_u64()
            .and_then(|seed| u32::try_from(seed).ok())
            .ok_or(Refusal::conflict("request_id_conflict"))?;
        if !crate::puzzle2048::completed(seed, &replay.actions) {
            return Err(Refusal::new(
                axum::http::StatusCode::UNPROCESSABLE_ENTITY,
                "target_not_reached",
            ));
        }
        // The rare draw only counts as a jackpot while it pays more than the refund.
        let jackpot =
            PUZZLE_JACKPOT_PRIZE > PUZZLE_PRIZE && rand::rngs::OsRng.gen_range(0..10_000u32) == 0;
        let payout = if jackpot {
            PUZZLE_JACKPOT_PRIZE
        } else {
            PUZZLE_PRIZE
        };
        Ok((payout, json!({"beans": payout, "jackpot": jackpot})))
    }
}
