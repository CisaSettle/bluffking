use crate::casino_bots::{self, Bot};
use crate::casino_room::{DealerRoom, Registry};
use crate::db_writer::PgConnection;
use crate::sicbo::{Side, MAX_STAKE, MIN_STAKE};
use draw_games::proof::{commit, Reveal};
use rand::{rngs::OsRng, Rng, RngCore};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::collections::{BTreeMap, BTreeSet};
use uuid::Uuid;

const BET_MS: i64 = 8_000;
const BET_LOCK_MS: i64 = 1_000;
// Match SicboTableView/useDiceRoll: reveal, then only the chip stages needed.
const REVEAL_MS: i64 = 2_800;
const COLLECT_MS: i64 = 480;
const PAYOUT_MS: i64 = 620;
/// Seats per room, humans and AI co-bettors together.
const MAX_MEMBERS: usize = 8;
const HISTORY: usize = 60;
/// AI co-bettors stake a plausible 1–50, not the open player limit.
const BOT_MAX_STAKE: i64 = 50;

pub use crate::casino_room::{Error, Result};

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

pub type State = crate::casino_room::RoundState<Wager, Roll, SicboState>;

#[derive(Clone, Serialize, Deserialize)]
pub struct SicboState {
    fairness: Reveal,
    #[serde(default)]
    jackpots: BTreeSet<Uuid>,
    bot_bets: BTreeMap<Uuid, BotBet>,
    /// The seed each bettor's browser sent with their accepted wager. Never
    /// shown before settlement: until the last one arrives nobody, the server
    /// included, can compute the dice.
    #[serde(default)]
    player_seeds: BTreeMap<Uuid, String>,
    /// The settled round's folded-in seeds, sorted, for its proofs.
    #[serde(default)]
    round_seeds: Vec<String>,
}
impl State {
    fn new(now: i64, bots: Vec<Bot>, beans: bool) -> Self {
        let round_id = Uuid::now_v7();
        let mut state = Self::new_round(
            round_id,
            now,
            BET_MS,
            bots,
            beans,
            SicboState {
                fairness: new_fairness(round_id),
                jackpots: BTreeSet::new(),
                bot_bets: BTreeMap::new(),
                player_seeds: BTreeMap::new(),
                round_seeds: Vec::new(),
            },
        );
        state.deal_bot_bets(now);
        state
    }
    fn next(&mut self, now: i64) {
        self.next_round(now, BET_MS);
        self.extra.fairness = new_fairness(self.round_id);
        self.extra.jackpots.clear();
        self.extra.player_seeds.clear();
        self.extra.round_seeds.clear();
        self.deal_bot_bets(now);
    }
    fn deal_bot_bets(&mut self, open: i64) {
        self.extra.bot_bets.clear();
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
            let stake = casino_bots::stake(MIN_STAKE, BOT_MAX_STAKE, bot.chips);
            let wager = Wager { side, stake };
            if wager.valid() {
                let at_ms = casino_bots::reveal_at(open, close);
                self.extra.bot_bets.insert(bot.id, BotBet { wager, at_ms });
            }
        }
    }
}

/// One settled round, as the writer makes it durable.
#[derive(Clone)]
pub struct Round {
    session_id: Uuid,
    round_id: Uuid,
    round_no: i64,
    result: Roll,
    fairness: Reveal,
    player_seeds: Vec<String>,
    has_bots: bool,
    bettors: Vec<(Uuid, Wager, i64)>,
    jackpots: BTreeSet<Uuid>,
    /// A gold-bean room's round settles in the bean ledger.
    beans: bool,
}

impl Round {
    pub(crate) fn participants(&self) -> Vec<Uuid> {
        self.bettors.iter().map(|(user, _, _)| *user).collect()
    }
    /// The bean jackpot owed to `uid` in this round, if any (see
    /// [`crate::sicbo::jackpot_bonus`]).
    fn jackpot(&self, uid: Uuid, wager: Wager) -> Option<i64> {
        self.jackpots
            .contains(&uid)
            .then(|| crate::sicbo::jackpot_bonus(wager.stake))
            .flatten()
    }
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
    /// 32 random bytes (lowercase hex) from the bettor's browser, required
    /// with a wager and folded into this round's dice.
    #[serde(default)]
    client_seed: Option<String>,
    /// `null` cancels the wager.
    bet: Option<Wager>,
}

static REGISTRY: Registry<SicboRoom> = Registry::new();

/// Folds the settled wagers' player seeds into the committed round seed.
fn settle_round(state: &mut State, now: i64) -> Option<Round> {
    if state.result.is_some() || now < state.deadline_ms {
        return None;
    }
    if state.extra.fairness.commitment.game != "sicbo-room"
        || state.extra.fairness.commitment.draw_id != state.round_id.to_string()
    {
        tracing::error!(round_id=%state.round_id, "sicbo room proof identity mismatch");
        return None;
    }
    // Only the seeds of wagers this round settles; a set sorts and dedups.
    let seeds: Vec<String> = state
        .bets
        .keys()
        .filter_map(|uid| state.extra.player_seeds.get(uid).cloned())
        .collect::<BTreeSet<_>>()
        .into_iter()
        .collect();
    let client_seed = match draw_games::sicbo::room_client_seed(&seeds) {
        Ok(seed) => seed,
        Err(error) => {
            tracing::error!(round_id=%state.round_id, %error, "sicbo room player seeds invalid");
            return None;
        }
    };
    state.extra.fairness.client_seed = client_seed;
    state.extra.round_seeds = seeds;
    let reveal = &state.extra.fairness;
    let dice = match draw_games::sicbo::draw(reveal, &state.extra.round_seeds) {
        Ok(dice) => dice,
        Err(error) => {
            tracing::error!(round_id=%state.round_id, %error, "sicbo room proof invalid");
            return None;
        }
    };
    let result = Roll::new(dice);
    let beans = state.wallet_unit.beans();
    let mut bettors = Vec::with_capacity(state.bets.len());
    for (&uid, &wager) in &state.bets {
        let delta = wager.delta(dice);
        state.deltas.insert(uid, delta);
        if beans {
            // The jackpot is a bean prize; a points room has none.
            if delta > 0 && crate::sicbo::wins_jackpot(reveal, uid) {
                state.extra.jackpots.insert(uid);
            }
        } else {
            let stack = state.stack(uid);
            state.stacks.insert(uid, stack + delta);
        }
        bettors.push((uid, wager, delta));
    }
    // Bots settle on their cosmetic stacks only: no wallet, no ledger, and
    // not part of the house result.
    for bot in &mut state.bots {
        if let Some(bet) = state.extra.bot_bets.get(&bot.id) {
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
    let fairness =
        draw_games::sicbo::draw_proof(&state.extra.fairness, dice, &state.extra.round_seeds);
    state
        .history
        .push(json!({"round_id":state.round_id,"round_no":state.round_no,
        "dice":dice,"total":result.total,"triple":result.triple,"fairness":fairness,
        "unit":state.wallet_unit}));
    if state.history.len() > HISTORY {
        state.history.remove(0);
    }
    state.result = Some(result);
    let mut collect = false;
    let mut payout = false;
    for delta in state.deltas.values().chain(state.bot_deltas.values()) {
        collect |= *delta < 0;
        payout |= *delta > 0;
    }
    state.deadline_ms =
        now + REVEAL_MS + if collect { COLLECT_MS } else { 0 } + if payout { PAYOUT_MS } else { 0 };
    Some(Round {
        session_id: state.history_session_id,
        round_id: state.round_id,
        round_no: state.round_no,
        result,
        fairness: state.extra.fairness.clone(),
        player_seeds: state.extra.round_seeds.clone(),
        has_bots: !state.bots.is_empty(),
        bettors,
        jackpots: state.extra.jackpots.clone(),
        beans,
    })
}

impl DealerRoom for SicboRoom {
    const LABEL: &'static str = "sicbo room";
    const DEALER: &'static str = "sicbo";
    const MAX_MEMBERS: usize = MAX_MEMBERS;
    const MIN_STAKE: i64 = MIN_STAKE;
    const REQUIRE_DURABLE_BETS: bool = true;
    const PRIVATE_FIELDS: &'static [&'static str] = &[
        "balance",
        "available",
        "bet",
        "bet_revision",
        "delta",
        "jackpot_bonus",
    ];
    type Bet = BetRequest;
    type Wager = Wager;
    type Outcome = Roll;
    type Extra = SicboState;
    type Round = Round;

    fn new_state(now: i64, bots: usize, beans: bool) -> State {
        State::new(now, casino_bots::roster(bots), beans)
    }
    fn remove_bot(state: &mut State, bot: Uuid) -> bool {
        let before = state.bots.len();
        state.bots.retain(|b| b.id != bot);
        state.extra.bot_bets.remove(&bot);
        state.bot_deltas.remove(&bot);
        state.bots.len() != before
    }
    fn bot_roster(state: &State) -> Vec<(Uuid, String)> {
        state.bots.iter().map(|b| (b.id, b.name.clone())).collect()
    }
    fn shift_release_clocks(state: &mut State, elapsed_ms: i64) {
        state.deadline_ms = state.deadline_ms.saturating_add(elapsed_ms);
        state.clock.expires_at_ms = state
            .clock
            .expires_at_ms
            .map(|at| at.saturating_add(elapsed_ms));
        for bet in state.extra.bot_bets.values_mut() {
            bet.at_ms = bet.at_ms.saturating_add(elapsed_ms);
        }
    }
    fn next_reveal_ms(state: &State, after: i64) -> Option<i64> {
        if state.result.is_some() {
            return None;
        }
        state
            .extra
            .bot_bets
            .values()
            .map(|bet| bet.at_ms)
            .filter(|at| *at > after)
            .min()
    }
    fn settle(_code: &str, state: &mut State, now: i64) -> Option<Round> {
        settle_round(state, now)
    }
    fn deltas(round: &Round) -> Vec<(Uuid, i64)> {
        round.bettors.iter().map(|(u, _, d)| (*u, *d)).collect()
    }
    fn bonus_credits(round: &Round) -> Vec<(Uuid, i64)> {
        round
            .bettors
            .iter()
            .filter_map(|&(uid, wager, _)| Some((uid, round.jackpot(uid, wager)?)))
            .collect()
    }
    fn advance(state: &mut State, now: i64) -> bool {
        if state.result.is_some() && now >= state.deadline_ms {
            state.next(now);
            return true;
        }
        false
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
            .is_some_and(|expected| state.extra.fairness.commitment.commitment.as_str() != expected)
        {
            return Err(Error::Code("fairness_conflict"));
        }
        // Exact retries (same wager and seed) are read-only, including after
        // closing/settlement. A different seed is a new attempt.
        if state.bets.get(&uid).copied() == wager
            && (wager.is_none()
                || state.extra.player_seeds.get(&uid).map(String::as_str)
                    == req.client_seed.as_deref())
        {
            return Ok(false);
        }
        if !state.betting_open(now, BET_LOCK_MS) {
            return Err(Error::Code("bet_closed"));
        }
        if wager.is_some_and(|w| !w.valid()) {
            return Err(Error::Code("invalid_bet"));
        }
        let seed = match wager {
            Some(_) => {
                let reveal = &state.extra.fairness;
                if req.commitment.as_deref() != Some(reveal.commitment.commitment.as_str()) {
                    return Err(Error::Code("fairness_required"));
                }
                Some(
                    req.client_seed
                        .filter(|seed| draw_games::proof::seed_bytes(seed).is_ok())
                        .ok_or(Error::Code("fairness_required"))?,
                )
            }
            None => None,
        };
        let current_revision = state.bet_revisions.get(&uid).copied().unwrap_or(0);
        if req.bet_revision != current_revision {
            return Err(Error::Code("bet_changed"));
        }
        if wager.map_or(0, |w| w.stake) > balance {
            return Err(Error::Code(state.wallet_unit.insufficient()));
        }
        match (wager, seed) {
            (Some(w), Some(seed)) => {
                state.bets.insert(uid, w);
                state.extra.player_seeds.insert(uid, seed);
            }
            (None, _) => {
                state.bets.remove(&uid);
                state.extra.player_seeds.remove(&uid);
            }
            (Some(_), None) => return Err(Error::Code("fairness_required")),
        }
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
        let reserved = if state.result.is_none() {
            bet.map(|w| w.stake).unwrap_or(0)
        } else {
            0
        };
        let result = state.result.map(|roll| {
            let mut result = json!(roll);
            result["fairness"] = {
                let reveal = &state.extra.fairness;
                if let Some((wager, delta)) = bet.zip(state.deltas.get(&uid)) {
                    let mut proof = draw_games::sicbo::payout_proof(
                        reveal,
                        roll.dice,
                        &state.extra.round_seeds,
                        wager.side,
                        wager.stake,
                    );
                    // Assert the actual shown settlement, so verification also
                    // detects a mismatch between that settlement and the rules.
                    proof["delta"] = json!(delta);
                    proof["won"] = json!(*delta > 0);
                    proof
                } else {
                    draw_games::sicbo::draw_proof(reveal, roll.dice, &state.extra.round_seeds)
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
        "unit":state.wallet_unit,
        "phase":if state.result.is_some(){"result"}else{"betting"},"deadline_ms":state.deadline_ms,"server_now_ms":now,
        "bet_close_ms":if state.result.is_none(){Some(state.deadline_ms-BET_LOCK_MS)}else{None},
        "bet_revision":state.bet_revisions.get(&uid).copied().unwrap_or(0),
        "balance":balance,"available":balance-reserved,"bet":bet,"result":result,"delta":state.deltas.get(&uid),
        "jackpot_bonus":state.bets.get(&uid).filter(|_| state.extra.jackpots.contains(&uid))
            .and_then(|wager| crate::sicbo::jackpot_bonus(wager.stake)),
        "fairness_commitment":&state.extra.fairness.commitment,
        "fairness_status":fairness_status,
        "min_stake":MIN_STAKE,"max_stake":MAX_STAKE,"ai":!state.bots.is_empty(),
        "members":members_view(state,members,uid,now),"history":state.history})
    }
    /// One [`crate::bean_ledger`] posting for the round's bettors, then its
    /// jackpot credits, in the checkpoint's transaction.
    async fn persist_round(conn: &mut PgConnection, code: &str, round: &Round) -> sqlx::Result<()> {
        if !round.beans {
            return persist_points_round(conn, code, round).await;
        }
        use crate::bean_ledger::{Entry, Kind, Posting};
        let shared = json!({"request_id":round.round_id,"dice":round.result.dice,
            "total":round.result.total,"triple":round.result.triple,
            "room_code":code,"round_no":round.round_no});
        // Same receipt as a solo roll, so history renders both alike.
        let plays = Posting {
            game: "sicbo",
            kind: Kind::Round,
            record_id: round.round_id,
            room_code: Some(code),
            session: Some(round.session_id),
            has_bots: round.has_bots,
            meta: shared,
            entries: round
                .bettors
                .iter()
                .map(|&(uid, wager, delta)| Entry {
                    user: uid,
                    delta,
                    completes_hand: true,
                    won: delta > 0,
                    meta: Some(json!({"side":wager.side,"stake":wager.stake,"won":delta>0,
                        "jackpot_bonus":round.jackpot(uid, wager).unwrap_or(0),
                        "fairness":draw_games::sicbo::payout_proof(&round.fairness, round.result.dice, &round.player_seeds, wager.side, wager.stake)})),
                })
                .collect(),
        };
        let Some(records) = crate::casino_room::settle_plays(&mut *conn, &plays).await? else {
            return Ok(());
        };
        // Jackpots commit with the round. The play receipts above carry the
        // rules payout only; each bonus is its own idempotent credit.
        for &(uid, wager, _) in &round.bettors {
            if let Some(bonus) = round.jackpot(uid, wager) {
                crate::sicbo::credit_jackpot(
                    &mut *conn,
                    uid,
                    round.round_id,
                    bonus,
                    json!({"request_id":round.round_id,"record_id":round.round_id,
                        "room_code":code,"round_no":round.round_no,"stake":wager.stake}),
                )
                .await?;
            }
        }
        crate::game_records::record_beans(
            &mut *conn,
            crate::game_catalog::SICBO,
            round.round_id,
            Some(round.session_id),
            round.has_bots,
            &records,
        )
        .await?;
        Ok(())
    }
    fn checkpoint_settlement(
        round: std::sync::Arc<Round>,
    ) -> Option<crate::game_checkpoint::Settlement> {
        Some(crate::game_checkpoint::Settlement::Sicbo(round))
    }
    fn registry() -> &'static Registry<Self> {
        &REGISTRY
    }
}

/// A points room's settled round: one practice record per bettor, in the
/// room checkpoint's transaction.
async fn persist_points_round(
    tx: &mut PgConnection,
    code: &str,
    round: &Round,
) -> sqlx::Result<()> {
    let rows: Vec<_> = round.bettors.iter().map(|&(uid, wager, delta)| (uid, delta, json!({"unit":"practice","room_code":code,"round_no":round.round_no,
            "side":wager.side,"stake":wager.stake,"won":delta>0,
            "dice":round.result.dice,"total":round.result.total,"triple":round.result.triple,
            "fairness":draw_games::sicbo::payout_proof(&round.fairness, round.result.dice, &round.player_seeds, wager.side, wager.stake)}))).collect();
    crate::casino_room::record_practice(
        tx,
        crate::game_catalog::SICBO,
        round.round_id,
        round.session_id,
        round.has_bots,
        &rows,
    )
    .await
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
        let bet = state.extra
            .bot_bets
            .get(&bot.id)
            .filter(|b| settled || b.at_ms <= now)
            .map(|b| b.wager);
        json!({"name":bot.name,"user_id":null,"bot_id":bot.id,"me":false,"ai":true,"bet":bet,"chips":bot.chips,
            "delta":if settled {state.bot_deltas.get(&bot.id).copied()} else {None}})
    }));
    out
}
