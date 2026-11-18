use crate::casino_bots::{self, Bot};
use crate::casino_room::{DealerRoom, Registry};
use crate::db_writer::PgConnection;
use draw_games::proof::{commit, Reveal};
use draw_games::yuxiaxie::{Bets, Symbol, MAX_STAKE, MIN_STAKE};
use rand::{rngs::OsRng, seq::SliceRandom, RngCore};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::collections::BTreeMap;
use uuid::Uuid;

const BET_MS: i64 = 8_000;
const BET_LOCK_MS: i64 = 1_000;
// Match YuxiaxieTableView: reveal, then only the chip stages needed.
const REVEAL_MS: i64 = 2_800;
const COLLECT_MS: i64 = 480;
const PAYOUT_MS: i64 = 620;
/// Seats per room, humans and AI co-bettors together.
const MAX_MEMBERS: usize = 8;
const HISTORY: usize = 60;
/// AI co-bettors stake a plausible 1–50 per symbol, not the open player limit.
const BOT_MAX_STAKE: i64 = 50;
/// The proof game for a room round (empty client seed: one draw per table).
const PROOF_GAME: &str = "yuxiaxie-room";

pub use crate::casino_room::{Error, Result};

/// One player's whole wager for a round: a stake per chosen symbol.
#[derive(Debug, Clone, Deserialize, Serialize, PartialEq, Eq)]
#[serde(transparent)]
pub struct Wager(pub Bets);
impl Wager {
    fn total(&self) -> Option<i64> {
        draw_games::yuxiaxie::total(&self.0)
    }
    fn delta(&self, dice: [u8; 3]) -> i64 {
        draw_games::yuxiaxie::delta(dice, &self.0)
    }
    fn stake(&self) -> i64 {
        self.0.values().sum()
    }
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
pub struct Roll {
    pub dice: [u8; 3],
}

/// Kept only in the room checkpoint until settlement. Views select the public
/// commitment explicitly, never serialize this pending reveal.
fn new_fairness(round_id: Uuid) -> Reveal {
    let mut seed = [0u8; 32];
    OsRng.fill_bytes(&mut seed);
    let server_seed = hex::encode(seed);
    let commitment = commit(PROOF_GAME, &round_id.to_string(), &server_seed)
        .expect("generated room seed and UUID are valid commitment inputs");
    Reveal {
        commitment,
        server_seed,
        client_seed: String::new(),
    }
}

#[derive(Clone, Serialize, Deserialize)]
struct BotBet {
    wager: Wager,
    /// When the wager shows on the table; always before betting closes.
    at_ms: i64,
}

pub type State = crate::casino_room::RoundState<Wager, Roll, YuxiaxieState>;

#[derive(Clone, Serialize, Deserialize)]
pub struct YuxiaxieState {
    fairness: Reveal,
    bot_bets: BTreeMap<Uuid, BotBet>,
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
            YuxiaxieState {
                fairness: new_fairness(round_id),
                bot_bets: BTreeMap::new(),
            },
        );
        state.deal_bot_bets(now);
        state
    }
    fn next(&mut self, now: i64) {
        self.next_round(now, BET_MS);
        self.extra.fairness = new_fairness(self.round_id);
        self.deal_bot_bets(now);
    }
    /// Each AI co-bettor backs one or two symbols, like a casual player.
    fn deal_bot_bets(&mut self, open: i64) {
        self.extra.bot_bets.clear();
        let close = self.deadline_ms - BET_LOCK_MS;
        for bot in &self.bots {
            if casino_bots::skips() {
                continue;
            }
            let picks = if OsRng.next_u32() % 3 == 0 { 2 } else { 1 };
            let budget = (bot.chips / picks as i64).max(MIN_STAKE);
            let bets: Bets = Symbol::ALL
                .choose_multiple(&mut OsRng, picks)
                .map(|symbol| (*symbol, casino_bots::stake(MIN_STAKE, BOT_MAX_STAKE, budget)))
                .collect();
            let wager = Wager(bets);
            if wager.total().is_some() {
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
    has_bots: bool,
    bettors: Vec<(Uuid, Wager, i64)>,
    /// A gold-bean room's round settles in the bean ledger.
    beans: bool,
}

impl Round {
    pub(crate) fn participants(&self) -> Vec<Uuid> {
        self.bettors.iter().map(|(user, _, _)| *user).collect()
    }
    fn payout_proof(&self, wager: &Wager) -> Value {
        draw_games::yuxiaxie::payout_proof(&self.fairness, self.result.dice, &wager.0)
    }
}

/// The 鱼虾蟹 room store behind the shared dealer-room plumbing.
pub struct YuxiaxieRoom;

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

static REGISTRY: Registry<YuxiaxieRoom> = Registry::new();

/// Replays the committed round seed.
fn settle_round(state: &mut State, now: i64) -> Option<Round> {
    if state.result.is_some() || now < state.deadline_ms {
        return None;
    }
    let reveal = &state.extra.fairness;
    if reveal.commitment.game != PROOF_GAME
        || reveal.commitment.draw_id != state.round_id.to_string()
    {
        tracing::error!(round_id=%state.round_id, "yuxiaxie room proof identity mismatch");
        return None;
    }
    let dice = match draw_games::yuxiaxie::draw(reveal) {
        Ok(dice) => dice,
        Err(error) => {
            tracing::error!(round_id=%state.round_id, %error, "yuxiaxie room proof invalid");
            return None;
        }
    };
    let result = Roll { dice };
    let beans = state.wallet_unit.beans();
    let mut bettors = Vec::with_capacity(state.bets.len());
    for (&uid, wager) in &state.bets {
        let delta = wager.delta(dice);
        state.deltas.insert(uid, delta);
        if !beans {
            let stack = state.stack(uid);
            state.stacks.insert(uid, stack + delta);
        }
        bettors.push((uid, wager.clone(), delta));
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
    let fairness = draw_games::yuxiaxie::draw_proof(&state.extra.fairness, dice);
    state.history.push(json!({"round_id":state.round_id,"round_no":state.round_no,
        "dice":dice,"fairness":fairness,"unit":state.wallet_unit}));
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
        has_bots: !state.bots.is_empty(),
        bettors,
        beans,
    })
}

impl DealerRoom for YuxiaxieRoom {
    const LABEL: &'static str = "yuxiaxie room";
    const DEALER: &'static str = "yuxiaxie";
    const MAX_MEMBERS: usize = MAX_MEMBERS;
    const MIN_STAKE: i64 = MIN_STAKE;
    const REQUIRE_DURABLE_BETS: bool = true;
    const PRIVATE_FIELDS: &'static [&'static str] =
        &["balance", "available", "bet", "bet_revision", "delta"];
    type Bet = BetRequest;
    type Wager = Wager;
    type Outcome = Roll;
    type Extra = YuxiaxieState;
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
        // Exact retries are read-only, including after closing/settlement.
        if state.bets.get(&uid) == wager.as_ref() {
            return Ok(false);
        }
        if !state.betting_open(now, BET_LOCK_MS) {
            return Err(Error::Code("bet_closed"));
        }
        let total = match &wager {
            Some(w) => w.total().ok_or(Error::Code("invalid_bet"))?,
            None => 0,
        };
        if wager.is_some() {
            let reveal = &state.extra.fairness;
            if req.commitment.as_deref() != Some(reveal.commitment.commitment.as_str()) {
                return Err(Error::Code("fairness_required"));
            }
        }
        let current_revision = state.bet_revisions.get(&uid).copied().unwrap_or(0);
        if req.bet_revision != current_revision {
            return Err(Error::Code("bet_changed"));
        }
        if total > balance {
            return Err(Error::Code(state.wallet_unit.insufficient()));
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
        let reserved = if state.result.is_none() {
            bet.map(Wager::stake).unwrap_or(0)
        } else {
            0
        };
        let result = state.result.map(|roll| {
            let mut result = json!(roll);
            result["fairness"] = {
                let reveal = &state.extra.fairness;
                if let Some((wager, delta)) = bet.zip(state.deltas.get(&uid)) {
                    let mut proof =
                        draw_games::yuxiaxie::payout_proof(reveal, roll.dice, &wager.0);
                    // Assert the actual shown settlement, so verification also
                    // detects a mismatch between that settlement and the rules.
                    proof["delta"] = json!(delta);
                    proof["won"] = json!(*delta > 0);
                    proof
                } else {
                    draw_games::yuxiaxie::draw_proof(reveal, roll.dice)
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
        "fairness_commitment":&state.extra.fairness.commitment,
        "fairness_status":fairness_status,
        "min_stake":MIN_STAKE,"max_stake":MAX_STAKE,"ai":!state.bots.is_empty(),
        "members":members_view(state,members,uid,now),"history":state.history})
    }
    /// One [`crate::bean_ledger`] posting for the round's bettors, in the
    /// checkpoint's transaction.
    async fn persist_round(conn: &mut PgConnection, code: &str, round: &Round) -> sqlx::Result<()> {
        if !round.beans {
            return persist_points_round(conn, code, round).await;
        }
        use crate::bean_ledger::{Entry, Kind, Posting};
        let shared = json!({"request_id":round.round_id,"dice":round.result.dice,
            "room_code":code,"round_no":round.round_no});
        let plays = Posting {
            game: "yuxiaxie",
            kind: Kind::Round,
            record_id: round.round_id,
            room_code: Some(code),
            session: Some(round.session_id),
            has_bots: round.has_bots,
            meta: shared,
            entries: round
                .bettors
                .iter()
                .map(|(uid, wager, delta)| Entry {
                    user: *uid,
                    delta: *delta,
                    completes_hand: true,
                    won: *delta > 0,
                    meta: Some(json!({"bets":wager,"stake":wager.stake(),"won":*delta>0,
                        "fairness":round.payout_proof(wager)})),
                })
                .collect(),
        };
        let Some(records) = crate::casino_room::settle_plays(&mut *conn, &plays).await? else {
            return Ok(());
        };
        crate::game_records::record_beans(
            &mut *conn,
            crate::game_catalog::YUXIAXIE,
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
        Some(crate::game_checkpoint::Settlement::Yuxiaxie(round))
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
    let rows: Vec<_> = round
        .bettors
        .iter()
        .map(|(uid, wager, delta)| {
            (
                *uid,
                *delta,
                json!({"unit":"practice","room_code":code,"round_no":round.round_no,
                    "bets":wager,"stake":wager.stake(),"won":*delta>0,"dice":round.result.dice,
                    "fairness":round.payout_proof(wager)}),
            )
        })
        .collect();
    crate::casino_room::record_practice(
        tx,
        crate::game_catalog::YUXIAXIE,
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
        let bet = state
            .extra
            .bot_bets
            .get(&bot.id)
            .filter(|b| settled || b.at_ms <= now)
            .map(|b| &b.wager);
        json!({"name":bot.name,"user_id":null,"bot_id":bot.id,"me":false,"ai":true,"bet":bet,"chips":bot.chips,
            "delta":if settled {state.bot_deltas.get(&bot.id).copied()} else {None}})
    }));
    out
}
