//! Server-owned claw cabinet. A grab's price and the prize of its drawn
//! outcome settle together in its one play, so no grab is ever left paid but
//! unsettled. The cabinet only reveals that outcome after the claw animation
//! (`ready_at_ms`); until then replies show the balance without the prize and
//! the receipt reports the grab as pending. Revealing moves no beans.
use chrono::Utc;
use rand::Rng;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use uuid::Uuid;

use super::{Outcome, Refusal, SoloGame, Stake};

const COST: i64 = 1;
const PRIZE: i64 = 2;
/// The 0.01% jackpot catch pays 5 × the normal prize.
const JACKPOT_PRIZE: i64 = 5 * PRIZE;
const RADIUS: f64 = 0.28;
// One server-owned chance for every toy and every eligible aim.
const CATCH_PERCENT: u32 = 30;
const NS: Uuid = Uuid::from_u128(0x80ec3f41_e44e_4da4_87b8_43b44f638534);
const POSITIONS: [(f64, f64); 6] = [
    (-1.7, -1.1),
    (0.0, -1.1),
    (1.7, -1.1),
    (-0.65, 0.4),
    (0.6, 0.4),
    (1.85, 0.4),
];

#[derive(Clone, Copy, Deserialize, Serialize, PartialEq)]
#[serde(deny_unknown_fields)]
struct Aim {
    x: f64,
    z: f64,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Play {
    aim: Aim,
}

/// The `detail` of a grab's ledger row.
#[derive(Clone, Deserialize, Serialize)]
struct Stored {
    request_id: Uuid,
    aim: Aim,
    caught: Option<usize>,
    catch_percent: u32,
    /// When the cabinet reveals the outcome.
    ready_at_ms: i64,
    jackpot: bool,
    /// Beans the catch paid, already in the posting's delta.
    payout: i64,
    attempts: i64,
    /// Toys collected so far, this grab's catch included.
    collection: [i64; 3],
}

fn stored(meta: &Value) -> Option<Stored> {
    let mut meta = meta.clone();
    // The engine adds `request_id` after the draw.
    if meta["request_id"].is_null() {
        meta["request_id"] = json!(Uuid::nil());
    }
    serde_json::from_value(meta).ok()
}

fn layout() -> Value {
    json!(POSITIONS
        .iter()
        .enumerate()
        .map(|(id, (x, z))| json!({"id":id,"position":[x,0.6,z],"rotation":[0,0,0,1]}))
        .collect::<Vec<_>>())
}

pub struct Claw;

impl SoloGame for Claw {
    const ID: &'static str = "claw";
    const OWNER_CHANGED: &'static str = "account_changed";
    const REQUEST_CONFLICT: &'static str = "request_conflict";
    const NOT_FOUND: &'static str = "grab_not_found";
    type Play = Play;

    fn record_id(user: Uuid, request: Uuid) -> Uuid {
        Uuid::new_v5(&NS, format!("{user}:{request}:grab").as_bytes())
    }
    fn stake(play: &Play) -> Result<Stake, Refusal> {
        let Aim { x, z } = play.aim;
        if !x.is_finite() || !z.is_finite() || x.abs() > 2.1 || z.abs() > 1.65 {
            return Err(Refusal::invalid("invalid_aim"));
        }
        Ok(Stake::of(COST))
    }
    fn same_play(play: &Play, meta: &Value) -> bool {
        stored(meta).is_some_and(|s| s.aim == play.aim)
    }
    fn draw(
        play: &Play,
        cost: i64,
        previous: Option<&Value>,
        rng: &mut dyn rand::RngCore,
    ) -> Result<Outcome, Refusal> {
        let previous = previous.and_then(stored);
        let now = Utc::now().timestamp_millis();
        if previous.as_ref().is_some_and(|p| now < p.ready_at_ms) {
            return Err(Refusal::conflict("grab_pending"));
        }
        let caught = POSITIONS
            .iter()
            .position(|(x, z)| (play.aim.x - x).hypot(play.aim.z - z) <= RADIUS)
            .filter(|_| rng.gen_ratio(CATCH_PERCENT, 100));
        let jackpot = caught.is_some() && rng.gen_range(0..10_000u32) == 0;
        let overflow = || Refusal::new(axum::http::StatusCode::CONFLICT, "claw_overflow");
        let mut collection = previous.as_ref().map_or([0; 3], |p| p.collection);
        if let Some(toy) = caught {
            collection[toy % 3] = collection[toy % 3].checked_add(1).ok_or_else(overflow)?;
        }
        let payout = match (caught, jackpot) {
            (None, _) => 0,
            (Some(_), true) => JACKPOT_PRIZE,
            (Some(_), false) => PRIZE,
        };
        let attempts = previous
            .as_ref()
            .map_or(0, |p| p.attempts)
            .checked_add(1)
            .ok_or_else(overflow)?;
        let entry = json!({
            "aim": play.aim,
            "caught": caught,
            "catch_percent": CATCH_PERCENT,
            "ready_at_ms": now + if caught.is_some() { 7500 } else { 6000 },
            "jackpot": jackpot,
            "payout": payout,
            "attempts": attempts,
            "collection": collection,
        });
        debug_assert_eq!(cost, COST);
        Ok(Outcome {
            payout,
            won: caught.is_some(),
            meta: entry,
        })
    }
    /// The grab as the cabinet shows it now; `balance` is the wallet balance.
    fn reply(meta: &Value, balance: i64) -> Value {
        let Some(s) = stored(meta) else {
            return json!({"request_id": meta["request_id"], "balance": balance});
        };
        let ready_in_ms = (s.ready_at_ms - Utc::now().timestamp_millis()).max(0);
        let completed = ready_in_ms == 0;
        let mut collection = s.collection;
        if let Some(toy) = s.caught.filter(|_| !completed) {
            collection[toy % 3] -= 1;
        }
        json!({"request_id":s.request_id,"aim":s.aim,"caught":s.caught,
            "completed":completed,"ready_in_ms":ready_in_ms,
            "cost":COST,"payout":if completed {s.payout} else {0},
            "reward":if !completed || s.caught.is_none() {"none"} else if s.jackpot {"jackpot"} else {"beans"},
            "balance":if completed {balance} else {balance - s.payout},
            "attempts":s.attempts,"collection":collection})
    }
    fn profile() -> Value {
        json!({"cost":COST,"prize":PRIZE,"catch_radius":RADIUS,"catch_percent":CATCH_PERCENT,"layout":layout()})
    }
}
