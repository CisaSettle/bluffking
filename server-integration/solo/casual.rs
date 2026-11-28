//! Paid casual entries and server-authoritative rewards. Local scores never
//! mint beans.
//!
//! Call/Fold and Lucky Draw settle price and prize in the entry play. A 2048
//! entry posts its price and issues the board seed; solving that board posts
//! the reward as a follow-up credit (`casual_reward`) keyed by the same
//! request. Rule for an unfinished 2048 entry: the price is final (it bought
//! the board; a refund would be a free retry), and the reward stays claimable
//! with a valid replay of that board, exactly once, with no deadline.
use draw_games::{bean_tools, proof::Entropy, puzzle2048};
use serde::Deserialize;
use serde_json::{json, Value};
use uuid::Uuid;

use super::{Keep, Outcome, Refusal, SoloGame, Stake};

const NS: Uuid = Uuid::from_u128(0x5c737949_c677_4992_a296_349e3f68c568);

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Play {
    /// The price the client showed; a changed price is refused.
    cost: i64,
    roster: Option<bean_tools::Roster>,
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
        "face": meta["face"], "seed": meta["seed"], "reward": meta["reward"],
        "roster": meta["roster"], "index": meta["index"]})
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
            const FAIR_DRAW: bool = true;
            const OWNER_CHANGED: &'static str = "account_changed";
            const REQUEST_CONFLICT: &'static str = "request_conflict";
            const NOT_FOUND: &'static str = "entry_not_found";
            type Play = Play;
            fn record_id(user: Uuid, request: Uuid) -> Uuid {
                record(user, request)
            }
            fn stake(play: &Play) -> Result<Stake, Refusal> {
                if (Self::ID == "lucky-draw") != play.roster.is_some()
                    || play.roster.as_ref().is_some_and(|roster| !roster.is_valid())
                {
                    return Err(Refusal::invalid("invalid_roster"));
                }
                stake(play, Self::COST)
            }
            fn same_play(play: &Play, meta: &Value) -> bool {
                meta["cost"] == json!(play.cost) && meta["roster"] == json!(play.roster)
            }
            fn committed_draw(
                play: &Play,
                cost: i64,
                _: Option<&Value>,
                entropy: &mut Entropy,
            ) -> Result<Outcome, Refusal> {
                let settled = Self::outcome(play, entropy)?;
                let reward = settled["reward"].clone();
                let payout = reward["beans"].as_i64().unwrap_or_default();
                Ok(entry(cost, settled, payout, reward))
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

casual!(CallFold, "call-fold", bean_tools::CALL_FOLD_COST);
casual!(LuckyDraw, "lucky-draw", bean_tools::LUCKY_DRAW_COST);
casual!(Puzzle2048, "2048", puzzle2048::COST);

impl CallFold {
    fn outcome(_: &Play, entropy: &mut Entropy) -> Result<Value, Refusal> {
        Ok(bean_tools::call_fold(entropy))
    }
    fn reward(_: &Value, _: Value) -> Result<(i64, Value), Refusal> {
        Err(not_found())
    }
}

impl LuckyDraw {
    fn outcome(play: &Play, entropy: &mut Entropy) -> Result<Value, Refusal> {
        let roster = play
            .roster
            .as_ref()
            .ok_or_else(|| Refusal::invalid("invalid_roster"))?;
        bean_tools::lucky_draw(roster, entropy).map_err(|_| Refusal::invalid("invalid_roster"))
    }
    fn reward(_: &Value, _: Value) -> Result<(i64, Value), Refusal> {
        Err(not_found())
    }
}

impl Puzzle2048 {
    fn outcome(_: &Play, entropy: &mut Entropy) -> Result<Value, Refusal> {
        Ok(json!({"seed": puzzle2048::draw_seed(entropy)}))
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
        let payout = puzzle2048::COMPLETION_REFUND;
        Ok((payout, json!({"beans": payout, "jackpot": false})))
    }
}
