use crate::casino_bots::{self, Bot};
use crate::casino_room::{DealerRoom, Registry};
use crate::room_clock::RoomClock;
use crate::sicbo::{Side, MAX_STAKE, MIN_STAKE, POLICY};
use crate::wallet;
use chrono::Utc;
use draw_games::proof::{commit, Reveal};
use rand::{rngs::OsRng, Rng, RngCore};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use sqlx::PgConnection;
use std::collections::{BTreeMap, BTreeSet};
use uuid::Uuid;

const BET_MS: i64 = 8_000;
const BET_LOCK_MS: i64 = 1_000;
const RESULT_MS: i64 = 6_000;
/// Seats per room, humans and AI co-bettors together.
const MAX_MEMBERS: usize = 8;
const HISTORY: usize = 60;

pub use crate::casino_room::{Error, Result};

/// Missing checkpoint metadata means a wager accepted before the bean upgrade.
/// Keep the denomination on the round, including after repeated crash recovery.
#[derive(Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
enum WalletUnit {
    #[default]
    Practice,
    Beans,
}
impl WalletUnit {
    fn game(self) -> i16 {
        match self {
            Self::Practice => crate::game_catalog::SICBO,
            Self::Beans => crate::game_catalog::BEANS,
        }
    }
}

/// One player's whole wager for a round: the solo game's sides and limits.
#[derive(Debug, Clone, Copy, Deserialize, Serialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct Wager {
    pub side: Side,
    pub stake: i64,
}
impl Wager {
    fn valid(self) -> bool {
        (MIN_STAKE..=MAX_STAKE).contains(&self.stake)
    }
    /// Even money; a triple loses both sides (`crate::sicbo::wins`).
    fn delta(self, dice: [u8; 3]) -> i64 {
        draw_games::sicbo::delta(dice, self.side, self.stake)
    }
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
pub struct Roll {
    pub dice: [u8; 3],
    pub total: u8,
    pub triple: bool,
}
impl Roll {
    fn new(dice: [u8; 3]) -> Self {
        Self {
            dice,
            total: dice.iter().sum(),
            triple: dice[0] == dice[1] && dice[1] == dice[2],
        }
    }
}

/// Kept only in the room checkpoint until settlement. Views select the public
/// commitment explicitly, never serialize this pending reveal.
fn new_fairness(round_id: Uuid) -> Reveal {
    let mut seed = [0u8; 32];
    OsRng.fill_bytes(&mut seed);
    let server_seed = hex::encode(seed);
    let commitment = commit("sicbo-room", &round_id.to_string(), &server_seed)
        .expect("generated room seed and UUID are valid commitment inputs");
    Reveal {
        commitment,
        server_seed,
        client_seed: String::new(),
    }
}

#[derive(Clone, Copy, Serialize, Deserialize)]
struct BotBet {
    wager: Wager,
    /// When the wager shows on the table; always before betting closes.
    at_ms: i64,
}

#[derive(Clone, Serialize, Deserialize)]
pub struct State {
    #[serde(default)]
    wallet_unit: WalletUnit,
    history_session_id: Uuid,
    throws: crate::game_hub::throws::RoomThrows,
    round_id: Uuid,
    fairness: Reveal,
    round_no: i64,
    deadline_ms: i64,
    bets: BTreeMap<Uuid, Wager>,
    bet_revisions: BTreeMap<Uuid, u64>,
    result: Option<Roll>,
    deltas: BTreeMap<Uuid, i64>,
    #[serde(default)]
    max_rewards: BTreeSet<Uuid>,
    history: Vec<Value>,
    bots: Vec<Bot>,
    bot_bets: BTreeMap<Uuid, BotBet>,
    bot_deltas: BTreeMap<Uuid, i64>,
    /// Committed-change counter (`crate::casino_room`).
    revision: i64,
    /// Session length and countdown.
    clock: RoomClock,
    /// The room leaderboard (输赢排行).
    standings: crate::game_hub::standings::RoomStandings,
    /// 允许朋友观战; defaults to on at creation.
    #[serde(default = "crate::game_hub::spectators::default_allow")]
    allow_spectators: bool,
}
impl State {
    fn new(now: i64, bots: Vec<Bot>) -> Self {
        let round_id = Uuid::now_v7();
        let mut state = Self {
            wallet_unit: WalletUnit::Beans,
            history_session_id: Uuid::now_v7(),
            round_id,
            fairness: new_fairness(round_id),
            round_no: 1,
            deadline_ms: now + BET_MS,
            bets: BTreeMap::new(),
            bet_revisions: BTreeMap::new(),
            result: None,
            deltas: BTreeMap::new(),
            max_rewards: BTreeSet::new(),
            history: Vec::new(),
            bots,
            bot_bets: BTreeMap::new(),
            bot_deltas: BTreeMap::new(),
            throws: Default::default(),
            revision: 0,
            clock: RoomClock::default(),
            standings: Default::default(),
            allow_spectators: crate::game_hub::spectators::default_allow(),
        };
        state.deal_bot_bets(now);
        state
    }
    fn betting_open(&self, now: i64) -> bool {
        self.result.is_none() && now < self.deadline_ms - BET_LOCK_MS
    }
    fn next(&mut self, now: i64) {
        if self.wallet_unit == WalletUnit::Practice {
            // The old session remains in durable records; don't sum its chips
            // into the new bean session or its room leaderboard.
            self.history_session_id = Uuid::now_v7();
            self.standings = Default::default();
        }
        self.wallet_unit = WalletUnit::Beans;
        self.round_id = Uuid::now_v7();
        self.fairness = new_fairness(self.round_id);
        self.round_no += 1;
        self.deadline_ms = now + BET_MS;
        self.bets.clear();
        self.bet_revisions.clear();
        self.result = None;
        self.deltas.clear();
        self.max_rewards.clear();
        self.bot_deltas.clear();
        self.deal_bot_bets(now);
    }
    fn deal_bot_bets(&mut self, open: i64) {
        self.bot_bets.clear();
        let close = self.deadline_ms - BET_LOCK_MS;
        for bot in &self.bots {
            if casino_bots::skips() {
                continue;
            }
            let side = if OsRng.gen_bool(0.5) {
                Side::Small
            } else {
                Side::Big
            };
            let stake = casino_bots::stake(MIN_STAKE, MAX_STAKE, bot.chips);
            let wager = Wager { side, stake };
            if wager.valid() {
                let at_ms = casino_bots::reveal_at(open, close);
                self.bot_bets.insert(bot.id, BotBet { wager, at_ms });
            }
        }
    }
}

/// One settled round, as the writer makes it durable.
#[derive(Clone)]
pub struct Round {
    wallet_unit: WalletUnit,
    session_id: Uuid,
    round_id: Uuid,
    round_no: i64,
    result: Roll,
    fairness: Reveal,
    has_bots: bool,
    bettors: Vec<(Uuid, Wager, i64)>,
    max_rewards: BTreeSet<Uuid>,
}

/// The room the player is seated in, from memory.
pub fn current_room(uid: Uuid) -> Option<String> {
    SicboRoom::registry().seat(uid)
}
/// Whether the player holds a seat in any sicbo room. Solo rolls and refills
/// are refused while seated so a pending room wager can always be covered.
/// The membership row is written before a seat is usable and removed only
/// after the room's settlements commit, so it is exact under the wallet lock.
pub async fn seated(conn: &mut PgConnection, uid: Uuid) -> sqlx::Result<bool> {
    sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM game_room_checkpoint_members WHERE game='sicbo' AND user_id=$1)")
        .bind(uid)
        .fetch_one(conn)
        .await
}

/// The 押大小 room store behind the shared dealer-room plumbing.
pub struct SicboRoom;

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct BetRequest {
    round_id: Uuid,
    bet_revision: u64,
    /// Commitment the bettor received before submitting this wager.
    #[serde(default)]
    commitment: Option<String>,
    /// `null` cancels the wager.
    bet: Option<Wager>,
}

static REGISTRY: Registry<SicboRoom> = Registry::new();

/// Replays the committed round seed.
fn settle_round(state: &mut State, now: i64) -> Option<Round> {
    if state.result.is_some() || now < state.deadline_ms {
        return None;
    }
    let reveal = &state.fairness;
    if reveal.commitment.game != "sicbo-room"
        || reveal.commitment.draw_id != state.round_id.to_string()
    {
        tracing::error!(round_id=%state.round_id, "sicbo room proof identity mismatch");
        return None;
    }
    let dice = match draw_games::sicbo::draw(reveal) {
        Ok(dice) => dice,
        Err(error) => {
            tracing::error!(round_id=%state.round_id, %error, "sicbo room proof invalid");
            return None;
        }
    };
    let result = Roll::new(dice);
    let mut bettors = Vec::with_capacity(state.bets.len());
    for (&uid, &wager) in &state.bets {
        let delta = wager.delta(dice);
        state.deltas.insert(uid, delta);
        if state.wallet_unit == WalletUnit::Beans
            && delta > 0
            && crate::sicbo::wins_max(reveal, uid)
        {
            state.max_rewards.insert(uid);
        }
        bettors.push((uid, wager, delta));
    }
    // Bots settle on their cosmetic stacks only: no wallet, no ledger, and
    // not part of the house result.
    for bot in &mut state.bots {
        if let Some(bet) = state.bot_bets.get(&bot.id) {
            let delta = bet.wager.delta(dice);
            casino_bots::apply(bot, delta, MIN_STAKE * 10);
            state.bot_deltas.insert(bot.id, delta);
            state.standings.finish(
                None,
                &bot.name,
                crate::game_hub::standings::Finish::from_delta(delta),
            );
        }
    }
    let fairness = draw_games::sicbo::draw_proof(&state.fairness, dice);
    state
        .history
        .push(json!({"round_id":state.round_id,"round_no":state.round_no,
        "dice":dice,"total":result.total,"triple":result.triple,"fairness":fairness,
        "unit":state.wallet_unit}));
    if state.history.len() > HISTORY {
        state.history.remove(0);
    }
    state.result = Some(result);
    state.deadline_ms = now + RESULT_MS;
    Some(Round {
        wallet_unit: state.wallet_unit,
        session_id: state.history_session_id,
        round_id: state.round_id,
        round_no: state.round_no,
        result,
        fairness: state.fairness.clone(),
        has_bots: !state.bots.is_empty(),
        bettors,
        max_rewards: state.max_rewards.clone(),
    })
}

impl DealerRoom for SicboRoom {
    const LABEL: &'static str = "sicbo room";
    const DEALER: &'static str = "sicbo";
    const MAX_MEMBERS: usize = MAX_MEMBERS;
    const POLICY: wallet::Policy = POLICY;
    const REQUIRE_DURABLE_BETS: bool = true;
    const PRIVATE_FIELDS: &'static [&'static str] = &[
        "balance",
        "available",
        "bet",
        "bet_revision",
        "delta",
        "max_reward",
    ];
    type Bet = BetRequest;
    type State = State;
    type Round = Round;

    fn new_state(now: i64, bots: usize) -> State {
        State::new(now, casino_bots::roster(bots))
    }
    fn round_id(state: &State) -> Uuid {
        state.round_id
    }
    fn bots(state: &State) -> usize {
        state.bots.len()
    }
    fn shift_release_clocks(state: &mut State, elapsed_ms: i64) {
        state.deadline_ms = state.deadline_ms.saturating_add(elapsed_ms);
        state.clock.expires_at_ms = state
            .clock
            .expires_at_ms
            .map(|at| at.saturating_add(elapsed_ms));
        for bet in state.bot_bets.values_mut() {
            bet.at_ms = bet.at_ms.saturating_add(elapsed_ms);
        }
    }
    fn deadline_ms(state: &State) -> i64 {
        state.deadline_ms
    }
    fn settled(state: &State) -> bool {
        state.result.is_some()
    }
    fn throws(state: &State) -> &crate::game_hub::throws::RoomThrows {
        &state.throws
    }
    fn throws_mut(state: &mut State) -> &mut crate::game_hub::throws::RoomThrows {
        &mut state.throws
    }
    fn revision(state: &State) -> i64 {
        state.revision
    }
    fn set_revision(state: &mut State, revision: i64) {
        state.revision = revision;
    }
    fn clock(state: &State) -> &RoomClock {
        &state.clock
    }
    fn clock_mut(state: &mut State) -> &mut RoomClock {
        &mut state.clock
    }
    fn settle(_code: &str, state: &mut State, now: i64) -> Option<Round> {
        settle_round(state, now)
    }
    fn deltas(round: &Round) -> Vec<(Uuid, i64)> {
        round.bettors.iter().map(|(u, _, d)| (*u, *d)).collect()
    }
    fn projection_deltas(round: &Round) -> Vec<(Uuid, i64)> {
        // Recovery activates the current bean wallet. A legacy practice result
        // remains durable in its original wallet and cannot change that display.
        if round.wallet_unit == WalletUnit::Beans {
            Self::deltas(round)
        } else {
            Vec::new()
        }
    }
    fn uses_current_wallet(state: &State) -> bool {
        state.wallet_unit == WalletUnit::Beans
    }
    fn reward_users(round: &Round) -> Vec<Uuid> {
        round.max_rewards.iter().copied().collect()
    }
    fn advance(state: &mut State, now: i64) -> bool {
        if state.result.is_some() && now >= state.deadline_ms {
            state.next(now);
            return true;
        }
        false
    }
    fn bet_pending(state: &State, uid: Uuid) -> bool {
        state.result.is_none() && state.bets.contains_key(&uid)
    }
    fn open_bets(state: &State) -> bool {
        state.result.is_none() && !state.bets.is_empty()
    }
    fn close_when_all_bet(state: &mut State, seated: &[Uuid], now: i64) -> bool {
        // Includes the final lock second: no wager can join then anyway.
        if state.result.is_some()
            || seated.is_empty()
            || !seated.iter().all(|uid| state.bets.contains_key(uid))
        {
            return false;
        }
        state.deadline_ms = now;
        true
    }
    /// Replace the caller's whole wager atomically; `None` cancels it.
    fn bet(state: &mut State, uid: Uuid, req: BetRequest, balance: i64, now: i64) -> Result<bool> {
        let wager = req.bet;
        if state.round_id != req.round_id {
            return Err(Error::Code("bet_closed"));
        }
        if req
            .commitment
            .as_deref()
            .is_some_and(|expected| state.fairness.commitment.commitment.as_str() != expected)
        {
            return Err(Error::Code("fairness_conflict"));
        }
        // Exact retries are read-only, including after closing/settlement.
        if state.bets.get(&uid).copied() == wager {
            return Ok(false);
        }
        // Old accepted wagers retain their exact receipt/retry semantics, but
        // no new wager may join a legacy round under the new bean UI.
        if state.wallet_unit != WalletUnit::Beans || !state.betting_open(now) {
            return Err(Error::Code("bet_closed"));
        }
        if wager.is_some_and(|w| !w.valid()) {
            return Err(Error::Code("invalid_bet"));
        }
        if wager.is_some() {
            let reveal = &state.fairness;
            if req.commitment.as_deref() != Some(reveal.commitment.commitment.as_str()) {
                return Err(Error::Code("fairness_required"));
            }
        }
        let current_revision = state.bet_revisions.get(&uid).copied().unwrap_or(0);
        if req.bet_revision != current_revision {
            return Err(Error::Code("bet_changed"));
        }
        if wager.map_or(0, |w| w.stake) > balance {
            return Err(Error::Code("insufficient_beans"));
        }
        match wager {
            Some(w) => state.bets.insert(uid, w),
            None => state.bets.remove(&uid),
        };
        state.bet_revisions.insert(uid, current_revision + 1);
        Ok(true)
    }
    fn view(
        code: &str,
        state: &State,
        uid: Uuid,
        balance: i64,
        members: &[(Uuid, String)],
        now: i64,
    ) -> Value {
        let bet = state.bets.get(&uid);
        let reserved = if state.wallet_unit == WalletUnit::Beans && state.result.is_none() {
            bet.map(|w| w.stake).unwrap_or(0)
        } else {
            0
        };
        let result = state.result.map(|roll| {
            let mut result = json!(roll);
            result["fairness"] = {
                let reveal = &state.fairness;
                if let Some((wager, delta)) = bet.zip(state.deltas.get(&uid)) {
                    let mut proof =
                        draw_games::sicbo::payout_proof(reveal, roll.dice, wager.side, wager.stake);
                    // Assert the actual shown settlement, so verification also
                    // detects a mismatch between that settlement and the rules.
                    proof["delta"] = json!(delta);
                    proof["won"] = json!(*delta > 0);
                    proof
                } else {
                    draw_games::sicbo::draw_proof(reveal, roll.dice)
                }
            };
            result
        });
        let fairness_status = if state.result.is_some() {
            "revealed"
        } else {
            "committed"
        };
        json!({"code":code,"round_id":state.round_id,"round_no":state.round_no,
        "unit":state.wallet_unit,"balance_unit":"beans",
        "phase":if state.result.is_some(){"result"}else{"betting"},"deadline_ms":state.deadline_ms,"server_now_ms":now,
        "bet_close_ms":if state.result.is_none(){Some(state.deadline_ms-BET_LOCK_MS)}else{None},
        "bet_revision":state.bet_revisions.get(&uid).copied().unwrap_or(0),
        "balance":balance,"available":balance-reserved,"bet":bet,"result":result,"delta":state.deltas.get(&uid),
        "max_reward":state.max_rewards.contains(&uid),
        "fairness_commitment":&state.fairness.commitment,
        "fairness_status":fairness_status,
        "min_stake":MIN_STAKE,"max_stake":MAX_STAKE,"ai":!state.bots.is_empty(),
        "members":members_view(state,members,uid,now),"history":state.history})
    }
    async fn persist_round(conn: &mut PgConnection, code: &str, round: &Round) -> sqlx::Result<()> {
        let mut records = Vec::new();
        let wallet_game = round.wallet_unit.game();
        // Deterministic UUID order also defines wallet-lock order.
        for &(uid, wager, delta) in &round.bettors {
            match crate::db::lock_game_participants(&mut *conn, &[uid]).await {
                Ok(()) => {}
                Err(sqlx::Error::RowNotFound) => continue,
                Err(error) => return Err(error),
            }
            // A room settlement reuses the round id as its receipt key. A solo
            // roll submitted with that id (only a crafted client can) would
            // collide with the unique play receipt and wedge this room's
            // writer, so that one wager is void instead.
            let taken: bool = sqlx::query_scalar(
                "SELECT EXISTS(SELECT 1 FROM wallet_ledger WHERE game=$2 AND record_id=$3 AND user_id=$1)",
            )
            .bind(uid)
            .bind(wallet_game)
            .bind(round.round_id)
            .fetch_one(&mut *conn)
            .await?;
            if taken {
                tracing::warn!(%uid, %code, "sicbo room wager reused a solo receipt id; voided");
                continue;
            }
            if wallet::lock(&mut *conn, wallet_game, uid).await?.is_none() {
                continue;
            }
            // Keep the membership grant in the same transaction as the original bean payout.
            let max_reward =
                if round.wallet_unit == WalletUnit::Beans && round.max_rewards.contains(&uid) {
                    Some(
                        crate::game_rewards::grant_max_on(&mut *conn, uid, "sicbo", round.round_id)
                            .await?,
                    )
                } else {
                    None
                };
            // Same receipt shape as a solo roll, so history renders both alike.
            let details = json!({"game":"sicbo","unit":round.wallet_unit,
                "request_id":round.round_id,"side":wager.side,"stake":wager.stake,
                "dice":round.result.dice,"total":round.result.total,"triple":round.result.triple,"won":delta>0,
                "room_code":code,"round_no":round.round_no,"max_reward":max_reward,
                "fairness":draw_games::sicbo::payout_proof(&round.fairness, round.result.dice, wager.side, wager.stake)});
            let play = wallet::Play {
                record_id: round.round_id,
                delta,
                completes_hand: true,
                won: delta > 0,
                room_code: Some(code),
                revision: None,
                details: Some(details),
                hand_completed: Some(true),
                has_bots: Some(round.has_bots),
            };
            match wallet::settle(&mut *conn, wallet_game, uid, &play).await {
                Ok(_) => {}
                // Account deletion removes the wallet; its hold vanishes with it.
                Err(sqlx::Error::RowNotFound) => {
                    tracing::warn!(%uid, %code, "sicbo bettor has no wallet at settlement; skipped");
                    continue;
                }
                Err(e) => return Err(e),
            }
            records.push((uid, delta));
        }
        crate::game_records::record(
            &mut *conn,
            crate::game_catalog::SICBO,
            round.round_id,
            Some(round.session_id),
            Utc::now(),
            &records,
        )
        .await?;
        Ok(())
    }
    fn standings(state: &State) -> &crate::game_hub::standings::RoomStandings {
        &state.standings
    }
    fn standings_mut(state: &mut State) -> &mut crate::game_hub::standings::RoomStandings {
        &mut state.standings
    }
    fn registry() -> &'static Registry<Self> {
        &REGISTRY
    }
    fn allow_spectators(state: &State) -> bool {
        state.allow_spectators
    }
    fn set_allow_spectators(state: &mut State, allow: bool) {
        state.allow_spectators = allow;
    }
}

/// Everyone at the table with their confirmed wager, humans first, then AI.
/// A bot's wager shows only once it has "placed" it.
fn members_view(state: &State, members: &[(Uuid, String)], uid: Uuid, now: i64) -> Vec<Value> {
    let settled = state.result.is_some();
    let mut out: Vec<Value> = members
        .iter()
        .map(|(id, name)| {
            json!({"name":name,"user_id":id,"me":*id==uid,"ai":false,"bet":state.bets.get(id),
                "delta":if settled {state.deltas.get(id).copied()} else {None}})
        })
        .collect();
    out.extend(state.bots.iter().map(|bot| {
        let bet = state
            .bot_bets
            .get(&bot.id)
            .filter(|b| settled || b.at_ms <= now)
            .map(|b| b.wager);
        json!({"name":bot.name,"user_id":null,"me":false,"ai":true,"bet":bet,"chips":bot.chips,
            "delta":if settled {state.bot_deltas.get(&bot.id).copied()} else {None}})
    }));
    out
}
