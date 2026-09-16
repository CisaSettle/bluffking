//! Betting round state machine and side-pot calculation.
//!
//! [`BettingRound`] models one street of betting. Multiple consecutive rounds
//! (preflop → flop → turn → river) combine to form a full hand.

use crate::action::{ActionError, PlayerAction};
use crate::player::{Chips, PlayerId};
use serde::{Deserialize, Serialize};

/// A side pot or main pot with eligible players.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SidePot {
    /// Total chips contributed per eligible player up to this cap.
    pub cap: Chips,
    /// Total chips in this pot.
    pub amount: Chips,
    /// Players eligible to win this pot.
    pub eligible: Vec<PlayerId>,
    /// Players to refund this layer to when it is uncalled overage rather than
    /// a contestable pot. Covers both a folder's overage above every live
    /// contribution and a deepest all-in stack's unmatched top layer.
    #[serde(default)]
    pub refund_to: Vec<PlayerId>,
}

/// State of a single player within a betting round.
///
/// All fields are `pub` for read-only access from `game.rs`; mutation only
/// occurs through [`BettingRound::apply_action`].
#[derive(Debug, Clone)]
pub struct PlayerState {
    /// Player identity.
    pub player_id: PlayerId,
    /// Current stack (decremented as chips are contributed).
    pub stack: Chips,
    /// Total chips contributed this round.
    pub contributed: Chips,
    /// Whether the player has folded.
    pub folded: bool,
    /// Whether the player is all-in.
    pub all_in: bool,
    /// Whether the player has taken a voluntary action this street since the
    /// last *full* raise reopened the betting.
    ///
    /// Reset to `false` for every still-active player (except the aggressor)
    /// whenever a full-size raise/bet reopens action. Used to enforce the NL
    /// "action not reopened" rule (TDA Rule 6): a player who has already acted
    /// and is now only facing a sub-minimum (non-reopening) all-in may call or
    /// fold, but may **not** re-raise.
    pub has_acted: bool,
    /// `current_bet` level at the moment this player last took an action.
    ///
    /// Lets us honour TDA Rule 43: even when `has_acted` is still set (no single
    /// wager reopened the betting), if *cumulative* short all-ins since this
    /// player last acted now total at least a full raise, action IS reopened for
    /// them and they may re-raise. Without this, a 100 → 150 → 220 sequence of
    /// short all-ins would wrongly deny the opener a legal re-raise when action
    /// returns facing a 120 increase over the 100 full raise.
    pub acted_at_bet: Chips,
}

impl PlayerState {
    fn new(player_id: PlayerId, stack: Chips) -> Self {
        Self {
            player_id,
            stack,
            contributed: Chips::ZERO,
            folded: false,
            all_in: false,
            has_acted: false,
            acted_at_bet: Chips::ZERO,
        }
    }
}

/// A single street of betting.
///
/// Closing condition: the round is done when
/// - no active (non-folded, non-all-in) player can act, OR
/// - only one non-folded player remains, OR
/// - all active players have matched `current_bet` AND `actions_remaining == 0`.
///
/// `actions_remaining` is managed as follows:
/// - On construction: set to `n_can_act` (everyone needs to act once).
/// - On raise/new-bet: reset to `n_can_act - 1` (the raiser already acted;
///   everyone else needs to respond).
/// - Each non-aggression action (check/call/fold) decrements by 1.
/// - If a player goes all-in for less than a full raise, it does NOT reopen.
#[derive(Debug, Clone)]
pub struct BettingRound {
    /// Players in action order.
    players: Vec<PlayerState>,
    /// Index of the current actor.
    current: usize,
    /// Current bet level this street.
    current_bet: Chips,
    /// Minimum raise delta.
    last_raise_amount: Chips,
    /// Total pot contributions this round.
    pot_total: Chips,
    /// True once no more actions are needed.
    done: bool,
    /// How many more actions are required before the round closes (see struct doc).
    actions_remaining: usize,
    /// Number of players who can still act.
    n_can_act: usize,
}

impl BettingRound {
    /// Create a new betting round.
    ///
    /// `players` is ordered by action order (not seat order).
    /// `first_actor` is the index of the first player to act.
    /// `current_bet` is any existing bet this street (0 for post-flop).
    /// `big_blind` is the minimum raise increment.
    pub fn new(
        players: Vec<(PlayerId, Chips)>,
        first_actor: usize,
        current_bet: Chips,
        big_blind: Chips,
    ) -> Self {
        let n = players.len();
        if n == 0 {
            return Self::empty(current_bet, big_blind);
        }

        let player_states: Vec<PlayerState> = players
            .into_iter()
            .map(|(id, stack)| {
                let mut ps = PlayerState::new(id, stack);
                if ps.stack.0 == 0 {
                    ps.all_in = true;
                }
                ps
            })
            .collect();

        let n_can_act = player_states
            .iter()
            .filter(|p| !p.all_in && !p.folded)
            .count();

        let mut round = Self {
            players: player_states,
            current: first_actor % n,
            current_bet,
            last_raise_amount: big_blind,
            pot_total: Chips::ZERO,
            done: false,
            actions_remaining: n_can_act,
            n_can_act,
        };

        round.seek_active();
        round.check_done();
        round
    }

    /// Create a preflop betting round with blinds pre-posted.
    pub fn new_preflop(
        players: Vec<(PlayerId, Chips)>, // stacks BEFORE blind deduction
        first_actor: usize,
        blinds: &[(PlayerId, Chips)], // (player_id, amount_posted)
        big_blind: Chips,
    ) -> Self {
        let n = players.len();
        if n == 0 {
            return Self::empty(Chips::ZERO, big_blind);
        }

        let mut player_states: Vec<PlayerState> = players
            .into_iter()
            .map(|(id, stack)| PlayerState::new(id, stack))
            .collect();

        let mut current_bet = Chips::ZERO;
        let mut total_posted = Chips::ZERO;

        for (blind_id, blind_amount) in blinds {
            if let Some(ps) = player_states.iter_mut().find(|p| p.player_id == *blind_id) {
                let actual = blind_amount.0.min(ps.stack.0);
                ps.stack.0 -= actual;
                ps.contributed.0 += actual;
                total_posted.0 += actual;
                if ps.stack.0 == 0 {
                    ps.all_in = true;
                }
                if ps.contributed.0 > current_bet.0 {
                    current_bet = Chips(ps.contributed.0);
                }
            }
        }

        let current_bet = Chips(current_bet.0.max(big_blind.0));
        let n_can_act = player_states
            .iter()
            .filter(|p| !p.all_in && !p.folded)
            .count();

        // Preflop: the BB is the last blind poster. After posting, everyone
        // except the BB must act (the BB can check/raise when action returns).
        // So actions_remaining = n_can_act (UTG through BB all get one action).
        let mut round = Self {
            players: player_states,
            current: first_actor % n,
            current_bet,
            last_raise_amount: big_blind,
            pot_total: total_posted,
            done: false,
            actions_remaining: n_can_act,
            n_can_act,
        };

        round.seek_active();
        round.check_done();
        round
    }

    fn empty(current_bet: Chips, big_blind: Chips) -> Self {
        Self {
            players: vec![],
            current: 0,
            current_bet,
            last_raise_amount: big_blind,
            pot_total: Chips::ZERO,
            done: true,
            actions_remaining: 0,
            n_can_act: 0,
        }
    }

    /// The current actor's `PlayerId`, or `None` if the round is done.
    pub fn current_player(&self) -> Option<PlayerId> {
        if self.done || self.players.is_empty() {
            return None;
        }
        Some(self.players[self.current].player_id)
    }

    /// Whether the round is complete.
    pub fn is_done(&self) -> bool {
        self.done
    }

    /// Current bet level.
    pub fn current_bet(&self) -> Chips {
        self.current_bet
    }

    /// Minimum amount to raise to this street.
    pub fn min_raise_to(&self) -> Chips {
        Chips(self.current_bet.0.saturating_add(self.last_raise_amount.0))
    }

    /// Total chips in the pot from this street.
    pub fn pot_total(&self) -> Chips {
        self.pot_total
    }

    /// Whether the current actor is allowed to raise/re-raise this turn.
    ///
    /// `false` when the actor has already acted this street and the betting was
    /// NOT reopened by a full raise (TDA Rule 6: facing only a sub-minimum,
    /// non-reopening all-in they may call or fold but not re-raise). Used by the
    /// engine to avoid advertising a `min_raise_to` the actor cannot legally
    /// use (audit 2026-06-03). Returns `false` when the round is done.
    pub fn current_player_can_raise(&self) -> bool {
        if self.done || self.players.is_empty() {
            return false;
        }
        self.player_can_raise(self.current)
    }

    /// Whether `idx` may make an aggressive wager in the current state.
    ///
    /// Raise rights alone are insufficient: poker has no meaningful wager when
    /// nobody with chips remains able to respond. In particular, the sole live
    /// stack facing an all-in opponent may fold or call the outstanding amount,
    /// but may not create an unmatched overage that would immediately be
    /// refunded as a synthetic side pot.
    fn player_can_raise(&self, idx: usize) -> bool {
        let Some(ps) = self.players.get(idx) else {
            return false;
        };
        if ps.folded || ps.all_in {
            return false;
        }
        // The actor must be able to put at least one chip beyond a call.
        if ps.contributed.0.saturating_add(ps.stack.0) <= self.current_bet.0 {
            return false;
        }
        // At least one other live stack must remain capable of responding.
        if !self
            .players
            .iter()
            .enumerate()
            .any(|(other_idx, other)| other_idx != idx && !other.folded && !other.all_in)
        {
            return false;
        }
        // A player who already acted may only re-raise if a full raise cleared
        // their `has_acted` flag, OR cumulative short all-ins since they last
        // acted now total at least a full raise (TDA Rule 43).
        !ps.has_acted || self.action_reopened_for(ps)
    }

    /// Whether betting is reopened for a player who has already acted because the
    /// bet has risen by at least a full raise since their last action.
    ///
    /// Implements TDA Rule 43: a single sub-minimum all-in does not reopen
    /// action, but multiple short all-ins that *sum* to a full raise do. The
    /// increase since the player last acted is `current_bet - acted_at_bet`; if
    /// that meets or exceeds the last full-raise increment, they regain the right
    /// to re-raise even though no single wager cleared their `has_acted` flag.
    fn action_reopened_for(&self, ps: &PlayerState) -> bool {
        self.last_raise_amount.0 > 0
            && self.current_bet.0.saturating_sub(ps.acted_at_bet.0) >= self.last_raise_amount.0
    }

    /// Apply an action for `player_id` (must be the current actor).
    pub fn apply_action(
        &mut self,
        player_id: PlayerId,
        action: &PlayerAction,
    ) -> Result<Chips, ActionError> {
        if self.done {
            return Err(ActionError::HandFinished);
        }
        if self.players.is_empty() {
            return Err(ActionError::HandFinished);
        }

        let idx = self.current;
        if self.players[idx].player_id != player_id {
            return Err(ActionError::NotYourTurn);
        }

        let ps = &self.players[idx];
        if ps.folded {
            return Err(ActionError::AlreadyFolded);
        }
        if ps.all_in {
            return Err(ActionError::InvalidAction);
        }

        // A wager above the current bet is an aggressive action regardless of
        // whether it arrived as Raise(to=...) or AllIn. It is illegal unless the
        // actor has raise rights, chips beyond the call, and another live stack
        // that can respond. Exact/partial all-in calls remain legal.
        let is_aggressive_attempt = match action {
            PlayerAction::Raise { amount } => amount.0 > self.current_bet.0,
            PlayerAction::AllIn => ps.contributed.0.saturating_add(ps.stack.0) > self.current_bet.0,
            _ => false,
        };
        if is_aggressive_attempt && !self.player_can_raise(idx) {
            return Err(ActionError::InvalidAction);
        }

        // NL "action not reopened" rule (TDA Rule 6): a player who has already
        // acted this street and whose action was reopened ONLY by a sub-minimum
        // (non-reopening) all-in may call or fold, but may NOT re-raise.
        // `has_acted` is cleared whenever a full raise legally reopens the
        // betting, so this guard only fires for the illegal re-raise case and
        // never blocks a legitimate 3-bet/4-bet.
        if ps.has_acted && !self.action_reopened_for(ps) {
            let is_raise_attempt = match action {
                PlayerAction::Raise { amount: raise_to } => {
                    // Some clients/replay producers encode a short all-in call
                    // as `Raise(to=table_bet)` rather than `AllIn`.  It is not
                    // an aggressive raise when the player is committing their
                    // entire remaining stack and still ends at or below the
                    // current bet; TDA Rule 6 allows that call even though the
                    // action was not reopened.
                    let to_commit = raise_to.0.saturating_sub(ps.contributed.0);
                    !(to_commit == ps.stack.0 && raise_to.0 <= self.current_bet.0)
                }
                // An all-in that would commit MORE than the current bet is an
                // aggressive re-raise; an all-in that only calls (or under-calls)
                // is allowed.
                PlayerAction::AllIn => ps.contributed.0 + ps.stack.0 > self.current_bet.0,
                _ => false,
            };
            if is_raise_attempt {
                return Err(ActionError::InvalidAction);
            }
        }

        let stack = ps.stack;
        let contributed = ps.contributed;
        let to_call = self.current_bet.0.saturating_sub(contributed.0);
        let is_aggression;

        let chips_moved = match action {
            PlayerAction::Fold => {
                self.players[idx].folded = true;
                self.n_can_act = self.n_can_act.saturating_sub(1);
                is_aggression = false;
                Chips::ZERO
            }

            PlayerAction::Check => {
                if to_call > 0 {
                    return Err(ActionError::InvalidAction);
                }
                is_aggression = false;
                Chips::ZERO
            }

            PlayerAction::Call => {
                // A `Call` with nothing to call (`to_call == 0`) is functionally a
                // Check: 0 chips move and the state transition is identical. We
                // accept it (a client sending Call in a checkable spot must not
                // error) and let the recording layer relabel it as Check
                // (see `BettingRound::to_call` / `GameHand::apply_action`), so it
                // never pollutes action history as an illegal zero-chip call.
                let actual = to_call.min(stack.0);
                self.players[idx].contributed.0 += actual;
                self.players[idx].stack.0 -= actual;
                self.pot_total.0 += actual;
                self.mark_all_in_if_broke(idx);
                is_aggression = false;
                Chips(actual)
            }

            PlayerAction::Raise { amount: raise_to } => {
                let raise_total = raise_to.0;
                let min_raise = self.current_bet.0.saturating_add(self.last_raise_amount.0);
                let to_commit = raise_total.saturating_sub(contributed.0);

                // Affordability check first: cannot commit more than the player's
                // remaining stack. This is an insufficient-funds failure, NOT a
                // min-raise violation — a Raise can be well ABOVE the minimum yet
                // still cost more chips than the player holds. Reporting
                // `BelowMinRaise` here mislabelled the failure (audit 2026-06-03);
                // `InvalidAction` is the honest "this action is illegal" code.
                if to_commit > stack.0 {
                    return Err(ActionError::InvalidAction);
                }

                // NL rule (U-01 fix): an under-min raise is only legal when the
                // player is going all-in. Otherwise reject.
                //
                // When it IS an all-in under-min, it does NOT reopen action and
                // must NOT update `last_raise_amount` (the min-raise tracker).
                // `current_bet` still rises so subsequent callers must match it.
                let raise_delta = raise_total.saturating_sub(self.current_bet.0);
                let is_all_in_with_action = to_commit == stack.0 && stack.0 > 0;

                if raise_total < min_raise {
                    if !is_all_in_with_action {
                        return Err(ActionError::BelowMinRaise);
                    }
                    // Under-min all-in via Raise: the bet may rise but the
                    // tracker stays. CRITICAL: never *lower* `current_bet` — a
                    // short all-in for LESS than the current bet is an
                    // all-in-for-less (call semantics) and must leave
                    // `current_bet` untouched so downstream `to_call` stays
                    // correct for everyone. Mirrors the `AllIn` branch guard
                    // below (`if total_committed > self.current_bet.0`).
                    if raise_total > self.current_bet.0 {
                        self.current_bet = Chips(raise_total);
                    }
                    is_aggression = false;
                } else {
                    // Full (or larger) raise: update both tracker and current bet.
                    self.last_raise_amount = Chips(raise_delta);
                    self.current_bet = Chips(raise_total);
                    is_aggression = true;
                }

                self.players[idx].contributed.0 += to_commit;
                self.players[idx].stack.0 -= to_commit;
                self.pot_total.0 += to_commit;
                self.mark_all_in_if_broke(idx);
                Chips(to_commit)
            }

            PlayerAction::AllIn => {
                let all_in_amount = stack.0;
                let total_committed = contributed.0 + all_in_amount;
                if total_committed > self.current_bet.0 {
                    let raise_delta = total_committed - self.current_bet.0;
                    // Only a full raise (>= last_raise_amount) reopens action.
                    if raise_delta >= self.last_raise_amount.0 {
                        self.last_raise_amount = Chips(raise_delta);
                        is_aggression = true;
                    } else {
                        is_aggression = false;
                    }
                    self.current_bet = Chips(total_committed);
                } else {
                    is_aggression = false;
                }
                self.players[idx].contributed.0 += all_in_amount;
                self.players[idx].stack.0 = 0;
                self.players[idx].all_in = true;
                self.n_can_act = self.n_can_act.saturating_sub(1);
                self.pot_total.0 += all_in_amount;
                Chips(all_in_amount)
            }

            PlayerAction::Blind { .. } => {
                // Blinds/antes are posted internally during hand setup
                // (`new_preflop` seeds the preflop round; `GameHand::start_hand`
                // synthesizes the Blind `ActionRecord`s directly) — they NEVER
                // flow through the voluntary-action path. Accepting a `Blind`
                // here would bypass every betting rule (min-raise, call legality,
                // the no-reopen guard), and the variant is wire-deserializable, so
                // an integrator feeding client input into `apply_action` would
                // inherit that bypass. Reject it as an illegal in-turn action.
                return Err(ActionError::InvalidAction);
            }
        };

        if is_aggression {
            // Raise/new-bet: the aggressor already acted. All OTHER active players must respond.
            // n_can_act may have decreased if the aggressor went all-in.
            self.actions_remaining = self.n_can_act;
            // The aggressor is not in n_can_act if they went all-in.
            // If they're still active, subtract 1 (they already acted).
            let aggressor_still_active = !self.players[idx].all_in;
            if aggressor_still_active {
                self.actions_remaining = self.actions_remaining.saturating_sub(1);
            }
            // A full raise reopens the betting: every other still-active player
            // must respond again with full rights (clear their has_acted flag).
            for (i, p) in self.players.iter_mut().enumerate() {
                if i != idx && !p.folded && !p.all_in {
                    p.has_acted = false;
                }
            }
        } else {
            // Non-aggressive action (check/call/fold) — and crucially a
            // sub-minimum all-in, which does NOT reopen action: do not clear
            // anyone's has_acted flag.
            self.actions_remaining = self.actions_remaining.saturating_sub(1);
        }

        // The current actor has now acted this street. Record the bet level they
        // acted against so a later cumulative full raise (TDA Rule 43) can reopen
        // their action even without a single reopening wager.
        self.players[idx].has_acted = true;
        self.players[idx].acted_at_bet = self.current_bet;

        self.advance();
        self.check_done();

        Ok(chips_moved)
    }

    /// Compute side pots from contributions this round.
    pub fn side_pots(&self) -> Vec<SidePot> {
        let eligible: Vec<(PlayerId, u32)> = self
            .players
            .iter()
            .filter(|p| !p.folded)
            .map(|p| (p.player_id, p.contributed.0))
            .collect();

        // (player_id, contribution) for EVERY player, folded included — needed
        // both to size each layer's `amount` and to identify the contributors of
        // a folder's uncalled-overage slice (the top layer that sits above every
        // eligible player's contribution).
        let all_with_contrib: Vec<(PlayerId, u32)> = self
            .players
            .iter()
            .map(|p| (p.player_id, p.contributed.0))
            .collect();

        if eligible.is_empty() {
            return vec![];
        }

        // Derive levels from ALL contributions (folded included), not just the
        // eligible (non-folded) ones. A folder who put in strictly more than the
        // top eligible contribution must still form its own top layer, otherwise
        // the slice between the top eligible level and the folder's contribution
        // is allocated to no pot and those chips are silently destroyed.
        let mut levels: Vec<u32> = all_with_contrib.iter().map(|(_, c)| *c).collect();
        levels.sort_unstable();
        levels.dedup();

        if levels.is_empty() || levels == [0] {
            return vec![];
        }

        let mut pots = Vec::new();
        let mut prev = 0u32;

        for &level in &levels {
            if level == 0 {
                prev = level;
                continue;
            }
            let slice = level - prev;

            let amount: u32 = all_with_contrib
                .iter()
                .map(|(_, c)| if *c <= prev { 0 } else { slice.min(c - prev) })
                .sum();

            let pot_eligible: Vec<PlayerId> = eligible
                .iter()
                .filter(|(_, c)| *c >= level)
                .map(|(pid, _)| *pid)
                .collect();

            if amount == 0 {
                prev = level;
                continue;
            }

            if !pot_eligible.is_empty() {
                let contributors: Vec<PlayerId> = all_with_contrib
                    .iter()
                    .filter(|(_, c)| *c > prev)
                    .map(|(pid, _)| *pid)
                    .collect();
                let refund_to = if pot_eligible.len() == 1 && contributors == pot_eligible {
                    pot_eligible.clone()
                } else {
                    Vec::new()
                };
                pots.push(SidePot {
                    cap: Chips(level),
                    amount: Chips(amount),
                    eligible: pot_eligible,
                    refund_to,
                });
            } else {
                // No live (non-folded) player reached this level — this slice is
                // a folder's uncalled overage. Refund it to its actual
                // contributor(s): everyone (folded) who put chips into this slice
                // (contribution > prev). distribute_pots returns it to them
                // rather than dropping the chips.
                let refund_to: Vec<PlayerId> = all_with_contrib
                    .iter()
                    .filter(|(_, c)| *c > prev)
                    .map(|(pid, _)| *pid)
                    .collect();
                if !refund_to.is_empty() {
                    pots.push(SidePot {
                        cap: Chips(level),
                        amount: Chips(amount),
                        eligible: Vec::new(),
                        refund_to,
                    });
                }
            }

            prev = level;
        }

        pots
    }

    // ---- Public accessors ----

    /// Contribution of a player this round.
    pub fn player_contributed(&self, player_id: PlayerId) -> Chips {
        self.players
            .iter()
            .find(|p| p.player_id == player_id)
            .map(|p| p.contributed)
            .unwrap_or(Chips::ZERO)
    }

    /// Chips a player must add to match the current bet (0 when already even).
    ///
    /// Used by the action-recording layer to normalize a zero-chip `Call`
    /// (`to_call == 0`) to the `Check` label it is functionally equivalent to,
    /// so action history / coach inputs never carry an illegal zero-chip call.
    pub fn to_call(&self, player_id: PlayerId) -> Chips {
        Chips(
            self.current_bet
                .0
                .saturating_sub(self.player_contributed(player_id).0),
        )
    }

    /// Whether a player has folded.
    pub fn player_folded(&self, player_id: PlayerId) -> bool {
        self.players
            .iter()
            .find(|p| p.player_id == player_id)
            .map(|p| p.folded)
            .unwrap_or(false)
    }

    /// Whether a player is all-in.
    pub fn player_all_in(&self, player_id: PlayerId) -> bool {
        self.players
            .iter()
            .find(|p| p.player_id == player_id)
            .map(|p| p.all_in)
            .unwrap_or(false)
    }

    /// All player states (for snapshotting).
    pub fn player_states(&self) -> &[PlayerState] {
        &self.players
    }

    // ---------------------------------------------------------------------------
    // Private helpers
    // ---------------------------------------------------------------------------

    /// Flag a player all-in (and drop them from `n_can_act`) if a contribution
    /// just emptied their stack. No-op when the stack is non-zero.
    fn mark_all_in_if_broke(&mut self, idx: usize) {
        if self.players[idx].stack.0 == 0 {
            self.players[idx].all_in = true;
            self.n_can_act = self.n_can_act.saturating_sub(1);
        }
    }

    /// Move `current` forward to the next player who can act.
    fn advance(&mut self) {
        let n = self.players.len();
        if n == 0 {
            self.done = true;
            return;
        }
        for _ in 0..n {
            self.current = (self.current + 1) % n;
            let p = &self.players[self.current];
            if !p.folded && !p.all_in {
                return;
            }
        }
        self.done = true;
    }

    /// Skip from the initial `current` to the first active player.
    fn seek_active(&mut self) {
        let n = self.players.len();
        if n == 0 {
            self.done = true;
            return;
        }
        let start = self.current;
        for i in 0..n {
            let idx = (start + i) % n;
            if !self.players[idx].folded && !self.players[idx].all_in {
                self.current = idx;
                return;
            }
        }
        self.done = true;
    }

    /// Check whether the round should close.
    fn check_done(&mut self) {
        if self.n_can_act == 0 {
            self.done = true;
            return;
        }

        // All-but-one all-in: exactly ONE player can still act and they owe
        // nothing (the bet is already matched — e.g. a freshly opened post-flop
        // street with current_bet == 0). There is no betting decision left, so
        // close the round and let the engine run the remaining board out to
        // showdown instead of prompting the lone chip-holder. Betting into
        // all-in opponents only builds an uncontested side pot that is returned,
        // so there is nothing to decide. The round STAYS open when that lone
        // live player still owes a call to a larger all-in (`lone_can_call`),
        // since fold-vs-call is a real decision.
        //
        // Bug-fix 2026-05-29: heads-up, villain shoves all-in pre-flop and hero
        // calls; the flop opened and prompted hero to act (Fold/Raise/Check)
        // instead of running the board out. n_can_act was 1 but none of the
        // branches below fired, leaving the round open.
        if self.n_can_act == 1 {
            // Find the lone player who can still act.
            let lone = self
                .players
                .iter()
                .find(|p| !p.folded && !p.all_in)
                .map(|p| (p.player_id, p.contributed.0));
            if let Some((lone_id, lone_contrib)) = lone {
                // The lone live player faces a real fold-or-call decision ONLY if
                // some OTHER non-folded player has actually committed MORE chips
                // than they have. Compare against the largest ACTUAL opponent
                // contribution — NOT `self.current_bet`, which `new_preflop`
                // floors to a full big blind even when the big blind is all-in
                // for LESS than a full blind. Comparing against that floor invents
                // a phantom "owed call": a player who has already fully covered
                // every live opponent (e.g. HU, SB posted 10, BB all-in for 8) was
                // forced to fold-or-call a bet nobody actually made, forfeiting
                // already-matched chips. Closing here lets the uncalled overage be
                // refunded and the board run out to showdown, which is correct NL
                // behaviour (dual-AI audit 2026-07-10, finding C11).
                let max_opp_contrib = self
                    .players
                    .iter()
                    .filter(|p| p.player_id != lone_id && !p.folded)
                    .map(|p| p.contributed.0)
                    .max()
                    .unwrap_or(0);
                if lone_contrib >= max_opp_contrib {
                    self.done = true;
                    return;
                }
                // A short big blind floors the opening `current_bet` to the
                // nominal blind while multiple live stacks can still enter for
                // a full bring-in. Once folds leave a single live stack facing
                // only that short all-in, however, its real decision is against
                // the opponent's ACTUAL contribution. Lower the effective bet
                // now so Call commits only the difference (e.g. SB 5 versus BB
                // all-in 8 calls 3, not the phantom nominal 20).
                if self.current_bet.0 > max_opp_contrib {
                    self.current_bet = Chips(max_opp_contrib);
                }
            }
        }

        let not_folded = self.players.iter().filter(|p| !p.folded).count();
        if not_folded <= 1 {
            self.done = true;
            return;
        }

        // All active players must be matched on the bet AND no actions remaining.
        let all_matched = self
            .players
            .iter()
            .all(|p| p.folded || p.all_in || p.contributed.0 == self.current_bet.0);

        if all_matched && self.actions_remaining == 0 {
            self.done = true;
        }
    }
}

#[cfg(test)]
mod tests;
