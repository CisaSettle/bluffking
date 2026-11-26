//! Player-entropy exchange for verifiable Hold'em dealing (v2
//! `pf-shuffle-v2`, `mental_poker::pf2`).
//!
//! This module is the server-side counterpart of `mp_dealing.rs`, but for the
//! sealed-dealer (non-Mental-Poker) branch. Every committed hand:
//!
//! 1. **Commits**: the sealed dealer locks an encrypted, secretly shuffled deck
//!    from a per-hand master seed and publishes only its commitment
//!    (`ServerMsg::SeedCommit`, `scheme = pf2::SCHEME`) before any player seed
//!    exists, so it cannot pick a deck after seeing the player entropy.
//! 2. **Optionally** mixes in client entropy from any *pf-capable* connected
//!    human through the two-round exchange below (anti-grind). The seeds, the
//!    commitment and the hand id derive the public permutation that fixes the
//!    final card order.
//! 3. **Reveals** only the player seeds and the deal roster after the hand
//!    (`ServerMsg::SeedReveal`). The master seed and the engine seed never
//!    leave the server; players verify shown cards with decryption proofs from
//!    the fairness API, so folded and undealt cards stay secret.
//!
//! ## Key difference from `mp_dealing.rs`: the await is NON-FATAL
//!
//! `mp_dealing` aborts the deal on timeout and falls back to `from_os()`.
//! `pf_dealing` NEVER aborts: on timeout (or channel close) it returns whatever
//! client seeds arrived (possibly none) and the caller deals with them. The
//! worst case is a silent demotion from "anti-grind" to "server-commit-only"
//! for that one hand — still fully verifiable. This protects the no-latency
//! guarantee for old/mobile/bot/solo clients.
//!
//! ## Capability gating
//!
//! There is no separate handshake. A web client signals participation simply by
//! sending `ClientCmd::SubmitClientSeedCommit { hand_id: "" }` once per socket.
//! The session task records the sender as
//! pf-capable (additive, per-session) and remembers its latest seed. The
//! bounded await only runs when ≥1 currently-connected pf-capable human is
//! seated; otherwise it is skipped entirely (zero added latency).
//!
//! Pure data in/out — holds no `!Send` value across an `await` (session.rs
//! discipline).

use std::collections::{BTreeMap, HashMap, HashSet, VecDeque};
use std::time::Duration;

use rand::rngs::OsRng;
use rand::RngCore;
use tokio::sync::{mpsc, Mutex};
use tokio::time::Instant;
use tracing::info;
use uuid::Uuid;

use crate::protocol::ClientCmd;
use crate::session_registry::{SessionCtrl, WsSender};

/// Bounded, NON-FATAL per-hand client-seed collection window. Short on purpose: a participating table adds at most this much
/// pre-deal latency, and a slow/missing client only DOWNGRADES anti-grind — it
/// never aborts the deal (unlike a failed mental-poker deal).
pub const PF_SEED_DEADLINE: Duration = Duration::from_millis(1500);

/// Cap on money/seat/squid commands (`ReBuy`/`CancelReBuy`/`ReBuyExplicit`,
/// `SitOut`, `SquidJoinChoice`, `StaySeated`) deferred during one pre-deal or
/// coordinator window. Shared by the seed exchange, the Mental Poker deal and
/// the engine-blind live hand; an overflow is dropped with its timing marked
/// rejected.
pub(crate) const MAX_DEFERRED_MONEY_CMDS: usize = 40;
/// Separate, smaller cap on social frames (chat, reactions, throws) in a window
/// that still parks them (the engine-blind coordinator's). Kept apart so a chat flood — from a player or a rail spectator — is the one
/// that loses frames and can never crowd out another player's rebuy or
/// squid-join answer.
pub(crate) const MAX_DEFERRED_SOCIAL_CMDS: usize = 8;

/// Chat, reaction and throw frames, unwrapped from their timing/connection
/// envelopes.
fn is_social_window_command(cmd: &ClientCmd) -> bool {
    match cmd {
        ClientCmd::ConnectionScoped { command, .. } => is_social_window_command(command),
        ClientCmd::Chat { .. } | ClientCmd::Reaction { .. } | ClientCmd::ThrowItem { .. } => true,
        _ => false,
    }
}

/// Whether `cmd` still fits its own class's cap in `deferred`: control
/// commands against `control_cap`, social frames against
/// [`MAX_DEFERRED_SOCIAL_CMDS`].
pub(crate) fn window_command_fits(
    deferred: &[ClientCmd],
    cmd: &ClientCmd,
    control_cap: usize,
) -> bool {
    let social = is_social_window_command(cmd);
    let same_class = deferred
        .iter()
        .filter(|parked| is_social_window_command(parked) == social)
        .count();
    same_class
        < if social {
            MAX_DEFERRED_SOCIAL_CMDS
        } else {
            control_cap
        }
}

/// Draw a fresh 32-byte server seed from the OS CSPRNG.
///
/// NEVER `thread_rng()` — game randomness must come from `OsRng` (project rule).
pub fn os_random_32() -> [u8; 32] {
    let mut seed = [0u8; 32];
    OsRng.fill_bytes(&mut seed);
    seed
}

/// Connections whose client answers the seed exchange. A client announces it
/// once per socket (`SubmitClientSeedCommit { hand_id: "" }`); the WS task
/// records it here and retires it on exit. Per connection, so a stale tab
/// that never announced is never waited for, and an announcement that arrives
/// mid-hand is not lost.
fn capable_conns() -> &'static dashmap::DashMap<u64, ()> {
    static CONNS: std::sync::OnceLock<dashmap::DashMap<u64, ()>> = std::sync::OnceLock::new();
    CONNS.get_or_init(dashmap::DashMap::new)
}

pub fn mark_capable_connection(conn_id: u64) {
    capable_conns().insert(conn_id, ());
}

pub fn forget_connection(conn_id: u64) {
    capable_conns().remove(&conn_id);
}

pub fn connection_capable(conn_id: u64) -> bool {
    capable_conns().contains_key(&conn_id)
}

/// `ds_hash("pf:client-seed-commit:v1", [client_seed, hand_id, [seat]])` —
/// binds the seed to one hand and seat so a commitment cannot be replayed.
pub fn client_seed_commitment(client_seed: &[u8; 32], hand_id: Uuid, seat: u8) -> [u8; 32] {
    mental_poker::pf2_zjh::seed_commitment(client_seed, hand_id, seat)
}

/// Shared commitment/reveal ledger. Routing identity, clocks and durable phase
/// ownership stay with the caller; the cryptographic acceptance rules are one.
#[derive(Clone, Debug, Default, serde::Serialize, serde::Deserialize)]
pub struct SeedInputs {
    #[serde(deserialize_with = "seat_keyed")]
    pub commitments: BTreeMap<u8, String>,
    #[serde(deserialize_with = "seat_keyed")]
    pub client_seeds: BTreeMap<u8, [u8; 32]>,
}

// Flattened checkpoint fields deserialize through serde content, whose map keys
// remain strings. Parse those keys explicitly while preserving the JSON shape.
fn seat_keyed<'de, D: serde::Deserializer<'de>, T: serde::Deserialize<'de>>(
    deserializer: D,
) -> Result<BTreeMap<u8, T>, D::Error> {
    use serde::Deserialize;
    BTreeMap::<String, T>::deserialize(deserializer)?
        .into_iter()
        .map(|(seat, value)| {
            seat.parse::<u8>()
                .map(|seat| (seat, value))
                .map_err(serde::de::Error::custom)
        })
        .collect()
}

impl SeedInputs {
    pub fn submit(&mut self, hand: Uuid, seat: u8, bytes: [u8; 32], reveal: bool) -> bool {
        if reveal {
            if self.client_seeds.contains_key(&seat)
                || self.commitments.get(&seat)
                    != Some(&hex::encode(client_seed_commitment(&bytes, hand, seat)))
            {
                return false;
            }
            self.client_seeds.insert(seat, bytes);
        } else if let std::collections::btree_map::Entry::Vacant(entry) =
            self.commitments.entry(seat)
        {
            entry.insert(hex::encode(bytes));
        } else {
            return false;
        }
        true
    }

    pub fn complete(
        &self,
        revealing: bool,
        expected: impl Iterator<Item = u8>,
        released: impl Fn(u8) -> bool,
    ) -> bool {
        if revealing {
            self.commitments
                .keys()
                .all(|s| self.client_seeds.contains_key(s) || released(*s))
        } else {
            expected
                .into_iter()
                .all(|s| self.commitments.contains_key(&s))
        }
    }

    pub fn withheld(&self) -> Vec<u8> {
        self.commitments
            .keys()
            .copied()
            .filter(|s| !self.client_seeds.contains_key(s))
            .collect()
    }
}

/// Two-round player-entropy exchange for one sealed hand.
///
/// Round 1 collects seed COMMITMENTS; closing it fixes the roster and every
/// player input before any player seed is disclosed. Round 2 accepts only the
/// seed that opens a seat's commitment. So neither the operator nor a player
/// colluding with it can choose a seed after seeing an honest player's seed:
/// at most a committed seat can withhold its reveal, which is published as
/// `withheld`. First valid message per seat wins in both rounds.
#[derive(Debug)]
pub struct SeedExchange {
    hand_id: Uuid,
    hand_id_str: String,
    seat_of_user: HashMap<Uuid, u8>,
    inputs: SeedInputs,
    /// Seats whose player is leaving during the window: no longer awaited.
    released: HashSet<u8>,
    revealing: bool,
}

impl SeedExchange {
    /// `seat_of_user` = the connected, capable humans dealt into this hand.
    pub fn new(hand_id: Uuid, seat_of_user: HashMap<Uuid, u8>) -> Self {
        Self {
            hand_id,
            hand_id_str: hand_id.to_string(),
            seat_of_user,
            inputs: SeedInputs::default(),
            released: HashSet::new(),
            revealing: false,
        }
    }

    /// Stop waiting for a departing player's seed: their leave folds the seat
    /// out right after the window, so the deal must not wait on them. A commitment already published stays and is
    /// recorded as withheld; later frames from them are rejected.
    pub fn release(&mut self, user: Uuid) {
        if let Some(seat) = self.seat_of_user.remove(&user) {
            self.released.insert(seat);
        }
    }

    /// Nobody can answer: skip both windows.
    pub fn is_empty(&self) -> bool {
        self.seat_of_user.is_empty()
    }

    /// Input validity is independent of round completion: an accepted partial
    /// commitment/reveal is processed even while other players are outstanding.
    pub(crate) fn ingest(&mut self, cmd: &ClientCmd) -> bool {
        match cmd {
            ClientCmd::SubmitClientSeedCommit {
                hand_id,
                seed_hash,
                sender_user_id,
            } if !self.revealing && *hand_id == self.hand_id_str => {
                if let (Some(&seat), Some(hash)) =
                    (self.seat_of_user.get(sender_user_id), decode_32(seed_hash))
                {
                    self.inputs.submit(self.hand_id, seat, hash, false);
                }
            }
            ClientCmd::SubmitClientSeed {
                hand_id,
                client_seed,
                sender_user_id,
            } if self.revealing && *hand_id == self.hand_id_str => {
                if let (Some(&seat), Some(seed)) = (
                    self.seat_of_user.get(sender_user_id),
                    decode_32(client_seed),
                ) {
                    self.inputs.submit(self.hand_id, seat, seed, true);
                }
            }
            _ => {}
        }
        self.round_complete()
    }

    pub fn round_complete(&self) -> bool {
        self.inputs
            .complete(self.revealing, self.seat_of_user.values().copied(), |s| {
                self.released.contains(&s)
            })
    }

    /// Close round 1 and return the commitments to publish.
    pub fn close_commit_round(&mut self) -> BTreeMap<u8, [u8; 32]> {
        self.revealing = true;
        self.inputs
            .commitments
            .iter()
            .map(|(s, value)| (*s, decode_32(value).expect("validated commitment")))
            .collect()
    }

    /// Revealed seeds by seat, and the committed seats that withheld.
    pub fn finish(self) -> (BTreeMap<u8, [u8; 32]>, Vec<u8>) {
        let withheld = self.inputs.withheld();
        (self.inputs.client_seeds, withheld)
    }
}

/// Decode a 64-hex (32-byte) client seed. Returns `None` on malformed input.
pub fn decode_32(hex_str: &str) -> Option<[u8; 32]> {
    let v = hex::decode(hex_str).ok()?;
    if v.len() != 32 {
        return None;
    }
    let mut out = [0u8; 32];
    out.copy_from_slice(&v);
    Some(out)
}

/// The verified per-hand dealing secrets of a committed (v2) hand, kept by the
/// session task across the hand so the `PlayHandOutcome::Done` arm can persist
/// them and emit `ServerMsg::SeedReveal`. Only the player seeds and the roster
/// are ever sent to a client; every other field stays server-side.
#[derive(Debug, Clone)]
pub struct PfSecrets {
    pub variant: engine::PokerVariant,
    /// The same UUID used for `HandStarted` / persist / reveal.
    pub hand_id: Uuid,
    /// 32-byte master seed of the locked deck (`pf2::LockedDeck::new`). Never
    /// published; persisted as `pf2::encode_stored_seed`.
    pub server_seed: [u8; 32],
    /// 32-byte `pf2::LockedDeck::commit`, published before any player seed.
    pub seed_commit: [u8; 32],
    /// seat → 32-byte client seed actually mixed into the public order. May be
    /// empty (no capable player answered).
    pub client_seeds: BTreeMap<u8, [u8; 32]>,
    /// The secret engine/bot seed (`pf2::engine_seed`). Never published; it does
    /// NOT determine the deck.
    pub deck_seed: [u8; 32],
    /// The final deck in deal order (`mental_poker::card_id` ids) =
    /// `LockedDeck::final_card_ids(client_seeds)`. Server-internal: lets
    /// settled-hand consumers (rabbit hunt, review replay) rebuild the hand
    /// without a seed-derived shuffle.
    pub final_deck: Vec<u8>,
    /// The DEALT-SEAT ORDER: `dealt_seats[deal_pos] = seat`, i.e. the
    /// players-vec seat order at hand start (occupied seats only). The engine
    /// deals by deal position, NOT by seat, so the verifier needs this to map a
    /// sparse table's deal positions back to real seats. Set by the caller from
    /// the `players` vec right after the secrets are derived; `Vec::new()` only
    /// for pre-fill paths that have not yet stamped it (never persisted empty for
    /// a real hand).
    pub dealt_seats: Vec<u8>,
    /// Committed seats that did not reveal their seed (excluded from the mix).
    pub withheld_seats: Vec<u8>,
}

impl PfSecrets {
    /// `{"<seat>":"<64-hex>"}` JSON for persistence + reveal. Empty object for a
    /// server-commit-only hand.
    pub fn client_seeds_json(&self) -> serde_json::Value {
        let mut map = serde_json::Map::new();
        for (seat, seed) in &self.client_seeds {
            map.insert(
                seat.to_string(),
                serde_json::Value::String(hex::encode(seed)),
            );
        }
        serde_json::Value::Object(map)
    }

    /// `HashMap<u8, String>` for `ServerMsg::SeedReveal.client_seeds`.
    pub fn client_seeds_hex(&self) -> HashMap<u8, String> {
        self.client_seeds
            .iter()
            .map(|(seat, seed)| (*seat, hex::encode(seed)))
            .collect()
    }
}

/// Commands and lifecycle controls that arrived during an exchange window.
/// The caller re-queues both, in arrival order, ahead of later traffic.
#[derive(Default)]
pub struct WindowDeferred {
    /// Money commands and voluntary sit-outs: replayed at the hand boundary so
    /// chips and seat intents are never lost.
    pub cmds: Vec<ClientCmd>,
    /// Every lifecycle control (`JoinHuman`, `AttachConnection`, … and
    /// `Shutdown`). The collector owns the session's real `control_rx` while it
    /// waits, so dropping one would strand a connection; `Shutdown` also ends
    /// the window at once.
    pub ctrl: Vec<SessionCtrl>,
}

/// How an exchange window treats one lifecycle control.
pub enum WindowCtrl {
    /// Already answered; keep collecting seeds.
    Answered,
    /// Defer in arrival order; keep collecting seeds.
    Defer(SessionCtrl),
    /// Defer, and end the exchange now: the control will end the hand, so no
    /// seed is worth waiting for.
    DeferAndEnd(SessionCtrl),
    /// Defer, and stop waiting for this user's seed: the control removes them
    /// from the hand once the window ends. Others' seeds are still awaited.
    DeferAndRelease(SessionCtrl, Uuid),
}

/// One seed-window step: the window ended (`false` when a shutdown or a
/// hand-ending control ended it), or a social frame arrived.
pub enum ExchangeStep {
    Done(bool),
    /// Chat, reaction or throw, still in its envelope with its ingress timer.
    /// These are session-owned and independent of the deal, so the caller
    /// answers them at once and resumes the same window and deadline instead
    /// of parking them until the deal.
    Social(Box<ClientCmd>),
}

/// Unwrap a connection-scoped seed message (dropping one from a superseded
/// connection), defer money/sit-out commands, hand social frames back to the
/// caller, and feed seeds to `ex`. `Some(Done(true))` = round complete.
async fn ingest_exchange_cmd(
    cmd: ClientCmd,
    ex: &mut SeedExchange,
    conns: &Mutex<HashMap<Uuid, WsSender>>,
    deferred: &mut Vec<ClientCmd>,
) -> Option<ExchangeStep> {
    if is_social_window_command(&cmd) {
        return Some(ExchangeStep::Social(Box::new(cmd)));
    }
    let cmd = match cmd {
        ClientCmd::ConnectionScoped {
            command,
            sender_user_id,
            conn_id,
            received,
        } => match *command {
            command @ (ClientCmd::SubmitClientSeed { .. }
            | ClientCmd::SubmitClientSeedCommit { .. }) => {
                let current = conns
                    .lock()
                    .await
                    .get(&sender_user_id)
                    .is_some_and(|sender| sender.conn_id == conn_id);
                if !current {
                    return None;
                }
                with_sender(command, sender_user_id)
            }
            command @ (ClientCmd::BuyInApprovalStatus { .. }
            | ClientCmd::ReviewBuyIn { .. }
            | ClientCmd::ReBuy { .. }
            | ClientCmd::CancelReBuy { .. }
            | ClientCmd::ReBuyExplicit { .. }
            | ClientCmd::SitOut { .. }
            | ClientCmd::SquidJoinChoice { .. }
            | ClientCmd::StaySeated { .. }) => {
                defer_window_command(
                    ClientCmd::ConnectionScoped {
                        command: Box::new(command),
                        sender_user_id,
                        conn_id,
                        received,
                    },
                    ex.hand_id,
                    deferred,
                );
                return None;
            }
            _ => return None,
        },
        command => command,
    };
    match cmd {
        ClientCmd::SubmitClientSeed { .. } | ClientCmd::SubmitClientSeedCommit { .. } => {
            ex.ingest(&cmd).then_some(ExchangeStep::Done(true))
        }
        other @ (ClientCmd::BuyInApprovalStatus { .. }
        | ClientCmd::ReviewBuyIn { .. }
        | ClientCmd::ReBuy { .. }
        | ClientCmd::CancelReBuy { .. }
        | ClientCmd::ReBuyExplicit { .. }
        | ClientCmd::SitOut { .. }
        | ClientCmd::SquidJoinChoice { .. }
        | ClientCmd::StaySeated { .. }) => {
            // Money and squid-join commands the in-hand loop would honour are
            // parked, in arrival order, for that loop, so no answer is lost.
            defer_window_command(other, ex.hand_id, deferred);
            None
        }
        // Anything else (e.g. an early game action) is not valid before the
        // deal and is dropped, as before.
        _ => None,
    }
}

fn with_sender(cmd: ClientCmd, sender_user_id: Uuid) -> ClientCmd {
    match cmd {
        ClientCmd::SubmitClientSeed {
            hand_id,
            client_seed,
            ..
        } => ClientCmd::SubmitClientSeed {
            hand_id,
            client_seed,
            sender_user_id,
        },
        ClientCmd::SubmitClientSeedCommit {
            hand_id, seed_hash, ..
        } => ClientCmd::SubmitClientSeedCommit {
            hand_id,
            seed_hash,
            sender_user_id,
        },
        other => other,
    }
}

fn defer_window_command(cmd: ClientCmd, hand_id: Uuid, deferred: &mut Vec<ClientCmd>) {
    if window_command_fits(deferred, &cmd, MAX_DEFERRED_MONEY_CMDS) {
        deferred.push(cmd);
    } else {
        info!("[pf_deferred_cmds_capped] hand_id={hand_id} — dropping deferred cmd over its class cap");
    }
}

/// Run ONE exchange round on the session channels until it is complete,
/// `deadline` (the caller's `PF_SEED_DEADLINE` for this round) passes, a
/// shutdown arrives, or a social frame needs answering — then the caller
/// answers it and calls again with the same `deadline`. Non-fatal: the caller
/// proceeds with whatever arrived. Parked survivors in `pending_action` are
/// consumed before fresh `action_rx` traffic, preserving arrival order.
/// `Done(false)` when a shutdown or a hand-ending control ended the window.
///
/// `answer_ctrl` replies at once to a control whose in-hand answer is already
/// fixed; every other control is deferred in arrival order. Without it a leave
/// from a dealt seat waited out the whole window (up to two
/// `PF_SEED_DEADLINE`s) only to be rejected mid-hand.
#[allow(clippy::too_many_arguments)]
pub async fn run_exchange_round(
    ex: &mut SeedExchange,
    conns: &Mutex<HashMap<Uuid, WsSender>>,
    action_rx: &mut mpsc::Receiver<ClientCmd>,
    pending_action: &mut VecDeque<ClientCmd>,
    control_rx: &mut mpsc::Receiver<SessionCtrl>,
    deferred: &mut WindowDeferred,
    deadline: Instant,
    answer_ctrl: &mut (dyn FnMut(SessionCtrl, bool) -> WindowCtrl + Send),
) -> ExchangeStep {
    if ex.round_complete() {
        return ExchangeStep::Done(true);
    }
    loop {
        // The previous result is submitted. A new input keeps its own original
        // receipt time, whether already queued or still awaited on the socket.
        if let Some(cmd) = pending_action.pop_front() {
            if let Some(step) = ingest_exchange_cmd(cmd, ex, conns, &mut deferred.cmds).await {
                return step;
            }
            continue;
        }
        tokio::select! {
            r = tokio::time::timeout_at(deadline, action_rx.recv()) => match r {
                Ok(Some(cmd)) => {
                    if let Some(step) = ingest_exchange_cmd(cmd, ex, conns, &mut deferred.cmds).await {
                        return step;
                    }
                }
                Ok(None) => return ExchangeStep::Done(true),
                Err(_) => {
                    return ExchangeStep::Done(true);
                },
            },
            ctrl = control_rx.recv() => match ctrl {
                Some(SessionCtrl::Shutdown) => {
                    deferred.ctrl.push(SessionCtrl::Shutdown);
                    return ExchangeStep::Done(false);
                }
                Some(other) => match answer_ctrl(other, !deferred.ctrl.iter().any(SessionCtrl::blocks_seed_admission)) {
                    WindowCtrl::Answered => {}
                    WindowCtrl::Defer(other) => {
                        deferred.ctrl.push(other);
                    },
                    WindowCtrl::DeferAndEnd(other) => {
                        deferred.ctrl.push(other);
                        return ExchangeStep::Done(false);
                    }
                    WindowCtrl::DeferAndRelease(other, user) => {
                        deferred.ctrl.push(other);
                        ex.release(user);
                        if ex.round_complete() {
                            return ExchangeStep::Done(true);
                        }
                    }
                },
                None => return ExchangeStep::Done(true),
            }
        }
    }
}
