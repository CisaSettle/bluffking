//! Online counter-exploitation shared by table bots (炸金花, Hold'em, Short Deck).
//!
//! A *line* is a public betting pattern a human uses against bots (a blind
//! max raise, a big preflop open, a river overbet ...). Each settled use of a
//! line moves a signed *pressure* in `[-1, 1]`:
//!
//! * the human took the pot uncontested — every bot facing the line folded —
//!   pushes it up by what they won relative to what they risked: the bots
//!   over-fold, so they should continue more against this line;
//! * a bot continued and the human still won pushes it down the same way: the
//!   bots pay off this line, so they should continue less;
//! * a bot continued and the human lost leaves it alone: continuing worked.
//!
//! Every update first decays the pressure toward zero, so the base strategy
//! returns within a few uses once a line stops paying; idle time decays it
//! too, over days for one player and hours for the population. The fixed
//! point is where the line's fold profit balances its showdown profit; the
//! bots map the pressure onto bounded range/price adjustments and never leave
//! their legal, equity-priced decision paths.
//!
//! The same cells also score the bots' own initiative against a human: a bot
//! opening bet every responder folded to counts like an uncontested line
//! (the bots should open more into this human), one a human continued against
//! and beat pushes the cell down (open less without a hand), and one that won
//! when called leaves it alone. A mispriced bluff or stab therefore stops
//! against the player who answers it within a few hands, and returns as the
//! read decays.
//!
//! A *share* cell is a read that is not a betting line: a human's share
//! of the pots that pay a side-game token, against one seat's fair share. A
//! player who enters every pot collects far more tokens than a seat's share
//! while bots fold to the price of entering, at a losing chip result that no
//! betting line shows. Each such pot is `seats - 1` when the human won it and
//! `-1` when someone else did — zero on average at a fair share — and the
//! cell is the decaying average of that: about how many fair shares the human
//! collects above one, over roughly the last hundred pots. It starts at zero,
//! hovers around zero for a player at a fair share and goes negative below.
//! A *rate* cell is the same average over a human's decisions instead of
//! pots ([`observe_rate`]): how often the human takes one action, such as
//! raising after looking at the cards, against a baseline frequency,
//! with a shorter memory.
//!
//! Two scopes: per human (fast; follows one exploiter within a few hands) and
//! per game population (slower; catches the same leak spread across accounts
//! and covers a human the bots have not met). Gold-bean and no-stakes play are
//! separate, so a style faked for free never steers bean tables.
//!
//! Only public information goes in: the human's own public actions and the
//! settled chip result. In-process, O(1) under a short std mutex, never awaits,
//! never touches SQL; it is optional bot state, not a ledger. The reads are
//! handed to [`crate::bot_adapt_store`] as plain snapshots ([`export_dirty`],
//! [`import`]) so they survive a restart; this module itself stays in memory.
use std::collections::{HashMap, HashSet};
use std::sync::{LazyLock, Mutex};
use std::time::{Duration, Instant};
use uuid::Uuid;

/// Games with their own line vocabulary. Lines of different games never mix.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Game {
    Zjh = 0,
    Holdem = 1,
    ShortDeck = 2,
}

impl Game {
    pub(crate) fn from_code(code: u8) -> Option<Self> {
        match code {
            0 => Some(Self::Zjh),
            1 => Some(Self::Holdem),
            2 => Some(Self::ShortDeck),
            _ => None,
        }
    }
}

/// How a human's use of a line ended, from public results only.
#[derive(Debug, Clone, Copy)]
pub enum Outcome {
    /// Every bot facing the line folded; the human took the pot.
    Uncontested,
    /// A bot continued (call, compare, showdown) and the human won.
    ContestedWon,
    /// A bot continued and the human lost (or folded later).
    ContestedLost,
    /// A bot's own opening bet that a human continued against, and the bot
    /// lost the hand: betting into this human costs, so the bots should open
    /// less without a hand. `risked`/`net` are the bot's.
    OpenLost,
}

/// One settled use of `line` by `user` against at least one bot decision.
#[derive(Debug, Clone, Copy)]
pub struct Observation {
    pub game: Game,
    pub beans: bool,
    pub line: u8,
    pub user: Uuid,
    /// Chips the human put at risk in the hand, in big blinds / antes.
    pub risked: f64,
    /// The human's net result of the hand, in the same unit.
    pub net: f64,
    pub outcome: Outcome,
}

#[derive(Clone, Copy)]
struct Scope {
    /// Step per event, as a share of `net / risked` (clamped to `[0, 2]`).
    rate: f64,
    /// Multiplier applied before every update.
    event_decay: f64,
    /// Idle half-life.
    half_life: Duration,
}

/// Steady pressure for a constant signal `x` is `rate * x / (1 - event_decay)`:
/// a human winning 20% of the risk every time saturates in a handful of uses.
/// A player's read fades over days, not hours: time away from the table is
/// weak evidence of a style change. New observations can move the read
/// through `event_decay` within a few hands.
const USER: Scope = Scope {
    rate: 0.15,
    event_decay: 0.97,
    half_life: Duration::from_secs(7 * 24 * 3600),
};
const POPULATION: Scope = Scope {
    rate: 0.04,
    event_decay: 0.99,
    half_life: Duration::from_secs(6 * 3600),
};

#[derive(Clone, Copy)]
struct Cell {
    pressure: f64,
    at: Instant,
}

impl Cell {
    fn decayed(self, scope: Scope, now: Instant) -> f64 {
        let idle = now.saturating_duration_since(self.at).as_secs_f64();
        self.pressure * 0.5f64.powf(idle / scope.half_life.as_secs_f64())
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
struct Key {
    game: Game,
    beans: bool,
    line: u8,
    user: Option<Uuid>,
}

/// Per-human cells beyond this are refused until room is made: faded cells
/// are swept, and [`evict_idle`] hands players who have not played for a
/// while back to their stored rows. Population cells are a few dozen and
/// always kept.
const USER_CAP: usize = 20_000;
const SWEEP_EVERY: Duration = Duration::from_secs(60);

struct State {
    cells: HashMap<Key, Cell>,
    users: usize,
    swept: Option<Instant>,
    /// Owners (`None` = the population) observed since their last export.
    dirty: HashSet<Option<Uuid>>,
}

static STATE: LazyLock<Mutex<State>> = LazyLock::new(|| {
    Mutex::new(State {
        cells: HashMap::new(),
        users: 0,
        swept: None,
        dirty: HashSet::new(),
    })
});

fn state() -> std::sync::MutexGuard<'static, State> {
    STATE.lock().unwrap_or_else(|e| e.into_inner())
}

/// Effective river shove size. Keep cheap shoves, pot-sized wagers and
/// overbets separate when learning how an individual player uses every size.
pub(crate) fn river_shove_size(amount: u64, pot_after_call: u64) -> u8 {
    if amount <= pot_after_call.div_ceil(2) {
        0
    } else if amount <= pot_after_call {
        1
    } else if amount <= pot_after_call.saturating_mul(2) {
        2
    } else {
        3
    }
}

/// Maximum absolute input to a settled betting-line update.
const MAX_LINE_SIGNAL: f64 = 2.0;
/// Largest pressure a single settled wager can produce from a cold player.
pub(crate) const SINGLE_LINE_UPDATE: f64 = USER.rate * MAX_LINE_SIGNAL;

/// The update step for one observation: positive = bots should continue more.
fn signal(o: &Observation) -> f64 {
    if !(o.risked.is_finite() && o.net.is_finite()) || o.risked <= 0.0 {
        return 0.0;
    }
    let x = (o.net / o.risked).clamp(0.0, MAX_LINE_SIGNAL);
    match o.outcome {
        Outcome::Uncontested => x,
        Outcome::ContestedWon => -x,
        Outcome::ContestedLost => 0.0,
        Outcome::OpenLost => -(-o.net / o.risked).clamp(0.0, MAX_LINE_SIGNAL),
    }
}

fn step(cell: &mut Cell, scope: Scope, signal: f64, now: Instant) -> (f64, f64) {
    let before = cell.decayed(scope, now);
    let after = (before * scope.event_decay + scope.rate * signal).clamp(-1.0, 1.0);
    *cell = Cell {
        pressure: after,
        at: now,
    };
    (before, after)
}

/// Record one settled use of a line. Cheap; call from the room actor after
/// settlement.
pub fn observe(o: Observation) {
    let signal = signal(&o);
    let now = Instant::now();
    let mut s = state();
    let population = Key {
        game: o.game,
        beans: o.beans,
        line: o.line,
        user: None,
    };
    let fresh = Cell {
        pressure: 0.0,
        at: now,
    };
    s.dirty.insert(None);
    let (before, after) = step(
        s.cells.entry(population).or_insert(fresh),
        POPULATION,
        signal,
        now,
    );
    // Rare, bounded evidence: only when the population read changes band.
    if band(before) != band(after) {
        tracing::info!(
            game = ?o.game, beans = o.beans, line = o.line,
            before = (before * 100.0).round() / 100.0,
            after = (after * 100.0).round() / 100.0,
            "bot_adapt_population_shift"
        );
    }
    let key = Key {
        user: Some(o.user),
        ..population
    };
    if !s.cells.contains_key(&key) {
        if s.users >= USER_CAP {
            sweep(&mut s, now);
        }
        if s.users >= USER_CAP {
            return;
        }
        s.users += 1;
    }
    // Dirty only once the cell exists: a player refused at the cap must not
    // have a stored row replaced by nothing.
    s.dirty.insert(Some(o.user));
    step(s.cells.entry(key).or_insert(fresh), USER, signal, now);
}

/// Per-event decay of a share cell: a memory of about a hundred pots.
const SHARE_DECAY: f64 = 0.99;
/// The population read weighs a quarter of the per-human step, like the
/// line scopes.
const SHARE_POPULATION: f64 = 0.25;

/// Record one token pot of a `seats`-player side-game hand for a human who
/// was dealt in: `won` = the human was its sole winner. Cheap; call from the
/// room actor after settlement. With a step of `1 - SHARE_DECAY` the cell
/// reads the excess over a fair share in fair shares (twice a fair share
/// settles at 1.0). At a fair share its spread is about 0.2 at nine seats
/// and 0.16 at six.
pub fn observe_share(game: Game, beans: bool, line: u8, user: Uuid, seats: usize, won: bool) {
    if seats < 2 {
        return;
    }
    let signal = if won { (seats - 1) as f64 } else { -1.0 };
    average(game, beans, line, user, signal, SHARE_DECAY);
}

/// Per-event decay of a rate cell: a memory of about thirty decisions, so a
/// player's habit shows within an evening and a change of habit within the
/// next one.
const RATE_DECAY: f64 = 0.97;

/// Record one decision of a human that either took a given action (`hit`)
/// or not, against a baseline frequency of one in `per`: a share cell over
/// decisions instead of pots. It reads the excess over the baseline in units
/// of the baseline (`per = 3`: a human taking it two times in three settles
/// at 1.0, never at -1.0). Cheap; call from the room actor after settlement.
pub fn observe_rate(game: Game, beans: bool, line: u8, user: Uuid, per: usize, hit: bool) {
    if per < 2 {
        return;
    }
    let signal = if hit { (per - 1) as f64 } else { -1.0 };
    average(game, beans, line, user, signal, RATE_DECAY);
}

/// Decaying average of `signal` for a human's cell and, at a quarter of
/// the weight, the population's.
fn average(game: Game, beans: bool, line: u8, user: Uuid, signal: f64, decay: f64) {
    let rate = 1.0 - decay;
    let now = Instant::now();
    let mut s = state();
    let fresh = Cell {
        pressure: 0.0,
        at: now,
    };
    s.dirty.insert(None);
    let step = |key: Key, scope: Scope, rate: f64, s: &mut State| {
        let cell = s.cells.entry(key).or_insert(fresh);
        let before = cell.decayed(scope, now);
        *cell = Cell {
            pressure: (before * decay + rate * signal).clamp(-1.0, 1.0),
            at: now,
        };
    };
    let population = Key {
        game,
        beans,
        line,
        user: None,
    };
    step(population, POPULATION, rate * SHARE_POPULATION, &mut s);
    let key = Key {
        user: Some(user),
        ..population
    };
    if !s.cells.contains_key(&key) {
        if s.users >= USER_CAP {
            sweep(&mut s, now);
        }
        if s.users >= USER_CAP {
            return;
        }
        s.users += 1;
    }
    s.dirty.insert(Some(user));
    step(key, USER, rate, &mut s);
}

fn band(p: f64) -> i8 {
    if p >= 0.5 {
        1
    } else if p <= -0.5 {
        -1
    } else {
        0
    }
}

/// Drop per-human cells whose pressure has decayed away, at most once a
/// minute, so a full table never rescans on every hand.
fn sweep(s: &mut State, now: Instant) {
    if s.swept
        .is_some_and(|at| now.saturating_duration_since(at) < SWEEP_EVERY)
    {
        return;
    }
    s.swept = Some(now);
    s.cells
        .retain(|k, c| k.user.is_none() || c.decayed(USER, now).abs() >= 0.02);
    s.users = s.cells.keys().filter(|k| k.user.is_some()).count();
}

impl State {
    fn read(&self, key: Key, scope: Scope, now: Instant) -> f64 {
        self.cells
            .get(&key)
            .map_or(0.0, |cell| cell.decayed(scope, now))
    }
}

/// The named player's own observations in this game, stakes and line.
pub fn personal_pressure(game: Game, beans: bool, line: u8, user: Uuid) -> f64 {
    state().read(
        Key {
            game,
            beans,
            line,
            user: Some(user),
        },
        USER,
        Instant::now(),
    )
}

/// Signed pressure for a line: the human's own read, topped up by the
/// population read in proportion to how little the human's read says.
/// `user = None` (an unidentified seat) reads the population alone.
pub fn pressure(game: Game, beans: bool, line: u8, user: Option<Uuid>) -> f64 {
    let now = Instant::now();
    let s = state();
    let key = Key {
        game,
        beans,
        line,
        user,
    };
    // A negative population read only half-counts: otherwise one account
    // could prime a line "paid off" with value hands and fresh accounts would
    // bluff it into folding bots before their own reads caught up.
    let population = s.read(Key { user: None, ..key }, POPULATION, now);
    let population = if population < 0.0 {
        population * 0.5
    } else {
        population
    };
    let own = user.map_or(0.0, |_| s.read(key, USER, now));
    (own + population * (1.0 - own.abs())).clamp(-1.0, 1.0)
}

/// One read as stored: the game and line codes, the stakes flag and the
/// pressure at the moment of the snapshot.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Saved {
    pub game: u8,
    pub beans: bool,
    pub line: u8,
    pub pressure: f64,
}

/// Pressure below this has decayed away: not exported, not imported.
const FADED: f64 = 0.02;

fn scope_of(owner: Option<Uuid>) -> Scope {
    if owner.is_some() {
        USER
    } else {
        POPULATION
    }
}

/// Every read of each owner (`None` = the population) observed since its
/// last export, decayed to now; the owners are no longer dirty. One pass
/// over the cells under the mutex, no I/O. An owner whose reads have all
/// faded is returned with none, so its stored reads are cleared too.
/// `ready` filters the owners: one it rejects stays dirty and is skipped.
pub fn export_dirty(ready: impl Fn(Option<Uuid>) -> bool) -> Vec<(Option<Uuid>, Vec<Saved>)> {
    let now = Instant::now();
    let mut s = state();
    let mut out: HashMap<Option<Uuid>, Vec<Saved>> = HashMap::new();
    s.dirty.retain(|owner| {
        if ready(*owner) {
            out.insert(*owner, Vec::new());
            false
        } else {
            true
        }
    });
    if out.is_empty() {
        return Vec::new();
    }
    for (key, cell) in &s.cells {
        let Some(cells) = out.get_mut(&key.user) else {
            continue;
        };
        let pressure = cell.decayed(scope_of(key.user), now);
        if pressure.abs() >= FADED {
            cells.push(Saved {
                game: key.game as u8,
                beans: key.beans,
                line: key.line,
                pressure,
            });
        }
    }
    out.into_iter().collect()
}

/// An export that could not be stored: export the owner again next time.
pub fn mark_dirty(owner: Option<Uuid>) {
    state().dirty.insert(owner);
}

/// Add an owner's stored reads, `idle` after they were saved. The idle
/// half-life applies as if the process had never stopped. A read this process
/// has already observed since it started keeps what it learned and gains the
/// stored pressure (clamped): its events decayed a value that started at zero
/// instead of the stored one, which over the few hands before a load arrives
/// differs by a few percent of the stored pressure. Call once per owner and
/// process. Returns `false`, having changed nothing, when the per-human cap
/// has no room for all of the reads: the caller retries later.
pub fn import(owner: Option<Uuid>, saved: &[Saved], idle: Duration) -> bool {
    let scope = scope_of(owner);
    let fade = 0.5f64.powf(idle.as_secs_f64() / scope.half_life.as_secs_f64());
    let now = Instant::now();
    let mut s = state();
    let reads: Vec<(Key, f64)> = saved
        .iter()
        .filter_map(|read| {
            let game = Game::from_code(read.game)?;
            // Retire the mixed big-bet/raise evidence. It cannot be split
            // truthfully; fresh cells learn each line independently while the
            // existing size-specific reads still supply the initial strategy.
            if game == Game::Holdem && (27..=29).contains(&read.line) {
                return None;
            }
            let stored = read.pressure * fade;
            (stored.is_finite() && stored.abs() >= FADED).then_some((
                Key {
                    game,
                    beans: read.beans,
                    line: read.line,
                    user: owner,
                },
                stored,
            ))
        })
        .collect();
    // All or nothing: a partial import could neither be retried (the reads
    // already added would be added again) nor saved (the row would shrink).
    if owner.is_some() {
        let new = reads
            .iter()
            .filter(|(key, _)| !s.cells.contains_key(key))
            .count();
        if s.users + new > USER_CAP {
            sweep(&mut s, now);
        }
        if s.users + new > USER_CAP {
            return false;
        }
        s.users += new;
    }
    for (key, stored) in reads {
        let before = s
            .cells
            .get(&key)
            .map_or(0.0, |cell| cell.decayed(scope, now));
        s.cells.insert(
            key,
            Cell {
                pressure: (before + stored).clamp(-1.0, 1.0),
                at: now,
            },
        );
    }
    true
}

/// Forget, in this process only, every player with no observation for `idle`
/// and nothing unsaved (not dirty), oldest first beyond that while the
/// per-human cells exceed three quarters of the cap. With the week-long
/// half-life nothing fades on its own in a day, so this is what keeps the
/// cells to the players of the last hours. Returns the players forgotten:
/// their stored rows must be loaded again when they next play. Whole players
/// only, so a reload never meets half of its own reads.
pub fn evict_idle(idle: Duration) -> Vec<Uuid> {
    let now = Instant::now();
    let mut s = state();
    let mut last: HashMap<Uuid, (Instant, usize)> = HashMap::new();
    for (key, cell) in &s.cells {
        if let Some(user) = key.user {
            let entry = last.entry(user).or_insert((cell.at, 0));
            entry.0 = entry.0.max(cell.at);
            entry.1 += 1;
        }
    }
    let mut players: Vec<(Instant, Uuid, usize)> = last
        .into_iter()
        .filter(|(user, _)| !s.dirty.contains(&Some(*user)))
        .map(|(user, (at, cells))| (at, user, cells))
        .collect();
    players.sort_unstable_by_key(|(at, ..)| *at);
    let mut left = s.users;
    let mut gone = HashSet::new();
    for (at, user, cells) in players {
        if now.saturating_duration_since(at) < idle && left <= USER_CAP / 4 * 3 {
            break;
        }
        left = left.saturating_sub(cells);
        gone.insert(user);
    }
    if !gone.is_empty() {
        s.cells
            .retain(|key, _| key.user.is_none_or(|user| !gone.contains(&user)));
        s.users = s.cells.keys().filter(|k| k.user.is_some()).count();
    }
    gone.into_iter().collect()
}
