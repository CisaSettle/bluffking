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

    /// Number of players who still need to act this street, **excluding** the
    /// current actor.
    ///
    /// Used by the bot module (ADR-024 §3) to build `DecisionContext.players_to_act_after_me`.
    /// Returns 0 when the round is done or the current actor is the last to act.
    pub fn players_yet_to_act(&self) -> usize {
        if self.done {
            return 0;
        }
        // Derive the count from live state rather than `actions_remaining`: a
        // player still owes an action if they are active (non-folded, non-all-in)
        // and either still owe a call (`contributed < current_bet`) or have not
        // acted since the last reopen (`!has_acted`). Exclude the current actor.
        //
        // `actions_remaining` alone is wrong after a non-reopening (sub-minimum)
        // all-in that RAISED `current_bet`: it is decremented to 0 even though
        // the already-acted players now owe the new difference and must still
        // call or fold (audit 2026-06-03). The round correctly stays open via
        // the `all_matched` guard in `check_done`; this reporting must agree.
        self.players
            .iter()
            .enumerate()
            .filter(|(i, p)| {
                *i != self.current
                    && !p.folded
                    && !p.all_in
                    && (p.contributed.0 < self.current_bet.0 || !p.has_acted)
            })
            .count()
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
        let ps = &self.players[self.current];
        if ps.folded || ps.all_in {
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

        // NL "action not reopened" rule (TDA Rule 6): a player who has already
        // acted this street and whose action was reopened ONLY by a sub-minimum
        // (non-reopening) all-in may call or fold, but may NOT re-raise.
        // `has_acted` is cleared whenever a full raise legally reopens the
        // betting, so this guard only fires for the illegal re-raise case and
        // never blocks a legitimate 3-bet/4-bet.
        if ps.has_acted && !self.action_reopened_for(ps) {
            let is_raise_attempt = match action {
                PlayerAction::Raise { .. } => true,
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

    /// Non-folded players.
    pub fn eligible_players(&self) -> Vec<PlayerId> {
        self.players
            .iter()
            .filter(|p| !p.folded)
            .map(|p| p.player_id)
            .collect()
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
            let lone_can_call = self
                .players
                .iter()
                .any(|p| !p.folded && !p.all_in && p.contributed.0 < self.current_bet.0);
            if !lone_can_call {
                self.done = true;
                return;
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
mod tests {
    use super::*;
    use crate::action::PlayerAction;

    fn pid(n: u64) -> PlayerId {
        PlayerId::new(n)
    }

    fn c(n: u32) -> Chips {
        Chips(n)
    }

    fn two_players(s1: u32, s2: u32) -> Vec<(PlayerId, Chips)> {
        vec![(pid(1), c(s1)), (pid(2), c(s2))]
    }

    fn three_players(s1: u32, s2: u32, s3: u32) -> Vec<(PlayerId, Chips)> {
        vec![(pid(1), c(s1)), (pid(2), c(s2)), (pid(3), c(s3))]
    }

    // U05 (dual-AI OSS review): a `Blind` is never a voluntary in-turn action —
    // blinds post internally during setup. Accepting it here would bypass every
    // betting rule, and the variant is wire-deserializable.
    #[test]
    fn blind_action_is_rejected_as_voluntary_action() {
        use crate::action::BlindKind;
        let mut round = BettingRound::new(two_players(1000, 1000), 0, c(100), c(20));
        // Under-min "blind" post that previously slipped through unvalidated.
        let err = round
            .apply_action(
                pid(1),
                &PlayerAction::Blind {
                    kind: BlindKind::Small,
                    amount: c(1),
                },
            )
            .unwrap_err();
        assert_eq!(err, ActionError::InvalidAction);
        // State untouched: it is still P1's turn, nothing contributed.
        assert_eq!(round.current_player(), Some(pid(1)));
        assert_eq!(round.pot_total(), c(0));
    }

    // U09 (dual-AI OSS review) / TDA Rule 43: two short all-ins that SUM to at
    // least a full raise reopen the betting for a player who has already acted.
    #[test]
    fn cumulative_short_all_ins_reopen_action() {
        // P1 opens to 100 (full raise, last_raise_amount = 100).
        let mut round = BettingRound::new(three_players(5000, 150, 220), 0, c(0), c(20));
        round
            .apply_action(pid(1), &PlayerAction::Raise { amount: c(100) })
            .unwrap();
        // P2 all-in 150 (+50, short) and P3 all-in 220 (+70, short): neither alone
        // reopens, but together they raise the bet 120 over the 100 full raise.
        round.apply_action(pid(2), &PlayerAction::AllIn).unwrap();
        round.apply_action(pid(3), &PlayerAction::AllIn).unwrap();
        // Action returns to P1 facing a cumulative full raise → may re-raise.
        assert_eq!(round.current_player(), Some(pid(1)));
        assert!(
            round.current_player_can_raise(),
            "cumulative short all-ins totalling a full raise must reopen P1's action"
        );
        round
            .apply_action(pid(1), &PlayerAction::Raise { amount: c(400) })
            .expect("re-raise must be legal when action is reopened cumulatively");
        assert_eq!(round.current_bet(), c(400));
    }

    // Negative control: a SINGLE sub-minimum all-in must NOT reopen action.
    #[test]
    fn single_short_all_in_does_not_reopen_action() {
        let mut round = BettingRound::new(three_players(5000, 130, 5000), 0, c(0), c(20));
        round
            .apply_action(pid(1), &PlayerAction::Raise { amount: c(100) })
            .unwrap();
        round.apply_action(pid(2), &PlayerAction::AllIn).unwrap(); // +30, short
                                                                   // Action to P3 (fresh) then back to P1: P1 already acted, faces only a 30
                                                                   // increase over the 100 full raise → cannot re-raise.
        round.apply_action(pid(3), &PlayerAction::Call).unwrap();
        assert_eq!(round.current_player(), Some(pid(1)));
        assert!(
            !round.current_player_can_raise(),
            "a single 30-chip short all-in must not reopen P1's action"
        );
        assert_eq!(
            round
                .apply_action(pid(1), &PlayerAction::Raise { amount: c(300) })
                .unwrap_err(),
            ActionError::InvalidAction
        );
    }

    #[test]
    fn fold_check_standard() {
        let mut round = BettingRound::new(two_players(1000, 1000), 0, c(0), c(20));
        round.apply_action(pid(1), &PlayerAction::Fold).unwrap();
        assert!(
            round.is_done(),
            "round should be done after last player folds"
        );
        assert!(round.player_folded(pid(1)));
    }

    #[test]
    fn check_check_closes_round() {
        let mut round = BettingRound::new(two_players(1000, 1000), 0, c(0), c(20));
        assert_eq!(round.current_player(), Some(pid(1)));
        round.apply_action(pid(1), &PlayerAction::Check).unwrap();
        assert_eq!(round.current_player(), Some(pid(2)));
        round.apply_action(pid(2), &PlayerAction::Check).unwrap();
        assert!(round.is_done());
    }

    #[test]
    fn call_basic() {
        let mut round = BettingRound::new(two_players(1000, 1000), 0, c(20), c(20));
        let moved = round.apply_action(pid(1), &PlayerAction::Call).unwrap();
        assert_eq!(moved, c(20));
    }

    #[test]
    fn raise_and_call() {
        let mut round = BettingRound::new(two_players(1000, 1000), 0, c(0), c(20));
        round
            .apply_action(pid(1), &PlayerAction::Raise { amount: c(60) })
            .unwrap();
        assert_eq!(round.current_bet(), c(60));
        let moved = round.apply_action(pid(2), &PlayerAction::Call).unwrap();
        assert_eq!(moved, c(60));
        assert!(round.is_done(), "round should be done after call of raise");
        assert_eq!(round.pot_total(), c(120));
    }

    #[test]
    fn raise_below_minimum_rejected() {
        let mut round = BettingRound::new(two_players(1000, 1000), 0, c(20), c(20));
        // current_bet=20, min_raise = 20+20=40. Raise to 30 is invalid.
        let err = round
            .apply_action(pid(1), &PlayerAction::Raise { amount: c(30) })
            .unwrap_err();
        assert_eq!(err, ActionError::BelowMinRaise);
    }

    /// Regression test: postflop bet 40, opponent raise-to 55 must be rejected.
    ///
    /// Scenario (Bug 1 repro):
    ///   - New postflop street: current_bet=0, big_blind=10 → last_raise_amount=10
    ///   - P1 bets 40 (Raise{amount:40}): 40 >= 10 → OK, last_raise_amount becomes 40
    ///   - P2 raises to 55: min_raise = 40+40=80, 55 < 80 → must return BelowMinRaise
    #[test]
    fn postflop_bet_40_reraise_to_55_rejected() {
        // New postflop street: current_bet=0, big_blind=10
        let mut round = BettingRound::new(two_players(1000, 1000), 0, c(0), c(10));
        assert_eq!(round.current_player(), Some(pid(1)));

        // P1 bets 40 (valid: 40 >= big_blind=10, last_raise_amount becomes 40)
        round
            .apply_action(pid(1), &PlayerAction::Raise { amount: c(40) })
            .unwrap();
        assert_eq!(round.current_bet(), c(40));
        assert_eq!(round.min_raise_to(), c(80)); // 40+40=80

        // P2 attempts raise to 55 — must be rejected (55 < 80)
        let err = round
            .apply_action(pid(2), &PlayerAction::Raise { amount: c(55) })
            .unwrap_err();
        assert_eq!(
            err,
            ActionError::BelowMinRaise,
            "raise to 55 must be rejected when min raise to is 80"
        );
    }

    /// Regression (audit 2026-06-03): a Raise whose total exceeds the player's
    /// stack is an affordability failure, not a min-raise violation. A player
    /// with a 200 stack facing current_bet 50 who raises to 1000 (well ABOVE the
    /// minimum) must be rejected with `InvalidAction`, not the misleading
    /// `BelowMinRaise`.
    #[test]
    fn over_the_top_unaffordable_raise_is_invalid_not_below_min() {
        // p1 stack 200, p2 stack 1000. current_bet 50, bb 20 → min_raise_to 70.
        let mut round = BettingRound::new(two_players(200, 1000), 0, c(50), c(20));
        let err = round
            .apply_action(pid(1), &PlayerAction::Raise { amount: c(1000) })
            .unwrap_err();
        assert_eq!(
            err,
            ActionError::InvalidAction,
            "unaffordable over-the-top raise must report InvalidAction, not BelowMinRaise"
        );
        // State untouched after the rejection.
        assert_eq!(round.current_bet(), c(50));
    }

    #[test]
    fn not_your_turn_rejected() {
        let mut round = BettingRound::new(two_players(1000, 1000), 0, c(0), c(20));
        let err = round
            .apply_action(pid(2), &PlayerAction::Check)
            .unwrap_err();
        assert_eq!(err, ActionError::NotYourTurn);
    }

    #[test]
    fn action_after_round_done_rejected() {
        let mut round = BettingRound::new(two_players(1000, 1000), 0, c(0), c(20));
        round.apply_action(pid(1), &PlayerAction::Check).unwrap();
        round.apply_action(pid(2), &PlayerAction::Check).unwrap();
        assert!(round.is_done());
        let err = round
            .apply_action(pid(1), &PlayerAction::Check)
            .unwrap_err();
        assert_eq!(err, ActionError::HandFinished);
    }

    #[test]
    fn all_in_heads_up_equal_stacks() {
        let mut round = BettingRound::new(two_players(1000, 1000), 0, c(0), c(20));
        round.apply_action(pid(1), &PlayerAction::AllIn).unwrap();
        round.apply_action(pid(2), &PlayerAction::AllIn).unwrap();
        assert!(round.is_done());
        assert!(round.player_all_in(pid(1)));
        assert!(round.player_all_in(pid(2)));
        assert_eq!(round.pot_total(), c(2000));
    }

    #[test]
    fn all_in_heads_up_unequal_stacks() {
        let mut round = BettingRound::new(two_players(500, 1000), 0, c(0), c(20));
        round.apply_action(pid(1), &PlayerAction::AllIn).unwrap();
        round.apply_action(pid(2), &PlayerAction::Call).unwrap();
        assert!(round.is_done());

        let pots = round.side_pots();
        let total: u32 = pots.iter().map(|p| p.amount.0).sum();
        assert_eq!(total, 1000); // p1 puts in 500, p2 calls 500
    }

    #[test]
    fn three_player_all_in_different_stacks() {
        let mut round = BettingRound::new(three_players(300, 600, 1000), 0, c(0), c(20));
        round.apply_action(pid(1), &PlayerAction::AllIn).unwrap();
        round.apply_action(pid(2), &PlayerAction::AllIn).unwrap();
        round.apply_action(pid(3), &PlayerAction::AllIn).unwrap();
        assert!(round.is_done());

        let pots = round.side_pots();
        let total: u32 = pots.iter().map(|p| p.amount.0).sum();
        assert_eq!(total, 1900);
    }

    /// REGRESSION (2026-05-29): a freshly-opened street with exactly ONE player
    /// who can act (everyone else all-in) and nothing owed must close
    /// immediately — there is no betting decision, so the board runs out. A
    /// stack-0 player is marked all-in by `BettingRound::new`.
    #[test]
    fn all_but_one_all_in_closes_round_when_nothing_owed() {
        // pid(1) deep (492), pid(2) all-in (stack 0). Post-flop: current_bet 0.
        let round = BettingRound::new(vec![(pid(1), c(492)), (pid(2), c(0))], 0, c(0), c(20));
        assert!(
            round.player_all_in(pid(2)),
            "a stack-0 player must be flagged all-in at construction"
        );
        assert!(
            round.is_done(),
            "round with one live stack owing nothing must close (auto-runout)"
        );
        assert!(
            round.current_player().is_none(),
            "no actor when the round is already done"
        );
    }

    /// NEGATIVE companion to the above: when the lone live player still OWES a
    /// call to a larger all-in, fold-vs-call is a real decision, so the round
    /// must stay open and expose them as the actor.
    #[test]
    fn lone_live_player_owing_a_call_stays_open() {
        // pid(1) short (50) shoves; pid(2) deep (1000) now owes a 50 call.
        let mut round = BettingRound::new(two_players(50, 1000), 0, c(0), c(20));
        round.apply_action(pid(1), &PlayerAction::AllIn).unwrap();
        assert!(
            !round.is_done(),
            "round must stay open while the lone live stack still owes a call"
        );
        assert_eq!(
            round.current_player(),
            Some(pid(2)),
            "the owing live player is still the actor"
        );
        // Once they call, nothing is owed and the round closes.
        round.apply_action(pid(2), &PlayerAction::Call).unwrap();
        assert!(
            round.is_done(),
            "round closes once the lone live stack matches the all-in"
        );
    }

    #[test]
    fn check_when_bet_outstanding_rejected() {
        let mut round = BettingRound::new(two_players(1000, 1000), 0, c(20), c(20));
        let err = round
            .apply_action(pid(1), &PlayerAction::Check)
            .unwrap_err();
        assert_eq!(err, ActionError::InvalidAction);
    }

    #[test]
    fn fold_after_fold_rejected() {
        let mut round = BettingRound::new(three_players(1000, 1000, 1000), 0, c(0), c(20));
        round.apply_action(pid(1), &PlayerAction::Fold).unwrap();
        round.apply_action(pid(2), &PlayerAction::Fold).unwrap();
        assert!(round.is_done());
        let err = round
            .apply_action(pid(3), &PlayerAction::Check)
            .unwrap_err();
        assert_eq!(err, ActionError::HandFinished);
    }

    /// U-01 regression: an under-min raise must NOT update `last_raise_amount`,
    /// so the next legal min re-raise stays anchored to the prior full increment.
    ///
    /// Scenario (engine-level, blinds-stripped to focus on the round):
    ///   - current_bet=$30 (someone opened to $30 above a $10 BB → last_raise_amount=$20)
    ///   - villain goes all-in for $40 (delta=$10 < $20 → under-min)
    ///   - Expected: current_bet=$40, last_raise_amount UNCHANGED at $20,
    ///     so `min_raise_to = $60`. The user-reported bug had this collapse to
    ///     $50 (= 40 + 10), which would mean `last_raise_amount` was wrongly
    ///     overwritten with the under-min delta.
    #[test]
    fn under_min_raise_does_not_update_min_reraise_tracker() {
        // Build a round that mirrors the live state after a $30 open above a $10 BB.
        // last_raise_amount must equal $20 (the prior raise increment); we set it
        // by constructing the round with current_bet=$10 (BB), big_blind=$10, then
        // having the first actor raise to $30 — same path the live engine takes.
        let mut round =
            BettingRound::new(vec![(pid(1), c(1000)), (pid(2), c(40))], 0, c(10), c(10));

        // Hero raises to $30 (delta=$20, full raise).
        round
            .apply_action(pid(1), &PlayerAction::Raise { amount: c(30) })
            .unwrap();
        assert_eq!(round.current_bet(), c(30));
        assert_eq!(
            round.min_raise_to(),
            c(50),
            "after $30 open the min re-raise is $30+$20=$50"
        );

        // Villain all-in for $40 total (their entire stack). Delta=$10 < $20 → under-min.
        round.apply_action(pid(2), &PlayerAction::AllIn).unwrap();

        assert_eq!(
            round.current_bet(),
            c(40),
            "under-min all-in still raises the current bet to $40"
        );
        assert_eq!(
            round.min_raise_to(),
            c(60),
            "U-01: under-min all-in must NOT update last_raise_amount; min_raise_to \
             must remain $40+$20=$60, NOT collapse to $40+$10=$50"
        );
    }

    /// U-01 regression: an under-min `Raise` (non-all-in) must be rejected,
    /// and the engine's state must be untouched (current_bet and
    /// last_raise_amount stay at their pre-action values).
    #[test]
    fn under_min_raise_rejected_leaves_state_intact() {
        let mut round = BettingRound::new(two_players(1000, 1000), 0, c(10), c(10));

        // Hero raises to $30 (delta=$20).
        round
            .apply_action(pid(1), &PlayerAction::Raise { amount: c(30) })
            .unwrap();
        assert_eq!(round.min_raise_to(), c(50));

        // Villain attempts Raise{$40}: 40 < min_raise=50, NOT an all-in
        // (villain has $1000 stack), must reject.
        let err = round
            .apply_action(pid(2), &PlayerAction::Raise { amount: c(40) })
            .unwrap_err();
        assert_eq!(err, ActionError::BelowMinRaise);

        // State must be unchanged after rejection.
        assert_eq!(round.current_bet(), c(30));
        assert_eq!(
            round.min_raise_to(),
            c(50),
            "rejected under-min raise must not mutate last_raise_amount"
        );
    }

    /// U-01: an under-min all-in submitted via `Raise{amount}` (e.g. short-stack
    /// shoving via the raise button) must be treated the same as an `AllIn`
    /// under-min: current_bet rises, but last_raise_amount stays.
    #[test]
    fn under_min_raise_as_all_in_does_not_update_tracker() {
        // Villain stack = $40 exactly.
        let mut round =
            BettingRound::new(vec![(pid(1), c(1000)), (pid(2), c(40))], 0, c(10), c(10));
        round
            .apply_action(pid(1), &PlayerAction::Raise { amount: c(30) })
            .unwrap();
        assert_eq!(round.min_raise_to(), c(50));

        // Villain "raises" to $40 — this is below min ($50) but it's their
        // entire remaining stack ($40 - $0 contributed = $40 = stack).
        // Engine must accept (legal under-min all-in) but NOT update tracker.
        round
            .apply_action(pid(2), &PlayerAction::Raise { amount: c(40) })
            .unwrap();

        assert!(round.player_all_in(pid(2)));
        assert_eq!(round.current_bet(), c(40));
        assert_eq!(
            round.min_raise_to(),
            c(60),
            "U-01: under-min Raise that is also an all-in must NOT update \
             last_raise_amount; min_raise_to must stay at $40+$20=$60"
        );
    }

    #[test]
    fn side_pots_three_way_correct_eligibility() {
        let mut round = BettingRound::new(three_players(300, 600, 1000), 0, c(0), c(20));
        round.apply_action(pid(1), &PlayerAction::AllIn).unwrap(); // 300
        round.apply_action(pid(2), &PlayerAction::AllIn).unwrap(); // 600
        round.apply_action(pid(3), &PlayerAction::Call).unwrap(); // 600
        assert!(round.is_done());

        let pots = round.side_pots();
        let total: u32 = pots.iter().map(|p| p.amount.0).sum();
        assert_eq!(total, 1500); // 300*3 + 300*2

        let first = &pots[0];
        assert_eq!(first.eligible.len(), 3);

        let second = &pots[1];
        assert_eq!(second.eligible.len(), 2);
        assert!(!second.eligible.contains(&pid(1)));
    }

    // ----------------------------------------------------------------------
    // Regression: short all-in via the Raise action must never LOWER the
    // current bet (P0 — 2026-05-29 prod bug hunt). A player raising "to" an
    // amount below the current bet while shoving their whole (short) stack is
    // an all-in-for-less and must be treated like AllIn: commit the stack,
    // mark all-in, but leave current_bet (and therefore everyone's to_call)
    // untouched.
    // ----------------------------------------------------------------------
    #[test]
    fn short_all_in_via_raise_does_not_lower_current_bet() {
        // p1 1000, p2 only 30 chips, p3 1000. BB = 20.
        let mut round = BettingRound::new(three_players(1000, 30, 1000), 0, c(0), c(20));
        // p1 raises to 100.
        round
            .apply_action(pid(1), &PlayerAction::Raise { amount: c(100) })
            .unwrap();
        assert_eq!(round.current_bet().0, 100);
        // p2 (stack 30) "raises to 30" — a short all-in for LESS than the bet.
        round
            .apply_action(pid(2), &PlayerAction::Raise { amount: c(30) })
            .unwrap();
        // current_bet MUST stay at 100, not drop to 30.
        assert_eq!(
            round.current_bet().0,
            100,
            "short all-in via raise must not lower current_bet"
        );
        assert!(round.player_all_in(pid(2)), "p2 should be all-in");
        // p3 still owes the full 100, not 30.
        assert_eq!(round.current_player(), Some(pid(3)));
        round.apply_action(pid(3), &PlayerAction::Call).unwrap();
        assert_eq!(round.player_contributed(pid(3)).0, 100);
        // Round closes: p1 already matched 100 and the short all-in did not
        // reopen action, so p1 is NOT re-prompted.
        assert!(
            round.is_done(),
            "round should close without re-prompting p1"
        );
        // Chip conservation: 100 + 30 + 100 = 230 in the pot this street.
        assert_eq!(round.pot_total().0, 230);
    }

    // ----------------------------------------------------------------------
    // Regression: a player who already acted may not re-raise after a
    // non-reopening (sub-minimum) all-in (P2 — TDA Rule 6, 2026-05-29 hunt).
    // ----------------------------------------------------------------------
    #[test]
    fn non_reopening_all_in_blocks_reraise_from_acted_player() {
        // Post-flop 3-way, all deep except p3 who is short.
        // p1 1000, p2 1000, p3 130. current_bet starts 0, BB 20.
        let mut round = BettingRound::new(three_players(1000, 1000, 130), 0, c(0), c(100));
        // p1 bets 100 (full).
        round
            .apply_action(pid(1), &PlayerAction::Raise { amount: c(100) })
            .unwrap();
        // p2 calls 100.
        round.apply_action(pid(2), &PlayerAction::Call).unwrap();
        // p3 all-in for 130 — delta 30 < min-raise 100 => does NOT reopen.
        round.apply_action(pid(3), &PlayerAction::AllIn).unwrap();
        assert_eq!(round.current_bet().0, 130);
        // Action back to p1, who already acted. p1 owes 30 — may call or fold,
        // but a re-raise is illegal (action not reopened).
        assert_eq!(round.current_player(), Some(pid(1)));
        let illegal = round.apply_action(pid(1), &PlayerAction::Raise { amount: c(300) });
        assert!(
            matches!(illegal, Err(ActionError::InvalidAction)),
            "acted player may not re-raise a non-reopening all-in, got {illegal:?}"
        );
        // But p1 may legally call the extra 30.
        round.apply_action(pid(1), &PlayerAction::Call).unwrap();
        assert_eq!(round.player_contributed(pid(1)).0, 130);
    }

    /// Regression (audit 2026-06-03): `current_player_can_raise()` must report
    /// `false` for an already-acted player facing only a non-reopening all-in,
    /// so the engine does not advertise a `min_raise_to` it would then reject.
    #[test]
    fn current_player_cannot_raise_after_non_reopening_all_in() {
        let mut round = BettingRound::new(three_players(1000, 1000, 130), 0, c(0), c(100));
        round
            .apply_action(pid(1), &PlayerAction::Raise { amount: c(100) })
            .unwrap();
        round.apply_action(pid(2), &PlayerAction::Call).unwrap();
        round.apply_action(pid(3), &PlayerAction::AllIn).unwrap();
        // Action back to p1 (already acted, facing a non-reopening all-in).
        assert_eq!(round.current_player(), Some(pid(1)));
        assert!(
            !round.current_player_can_raise(),
            "p1 may only call/fold here — must not be told it can raise"
        );
    }

    /// A player who has NOT yet acted (fresh street) can raise.
    #[test]
    fn current_player_can_raise_on_fresh_street() {
        let round = BettingRound::new(two_players(1000, 1000), 0, c(0), c(20));
        assert!(
            round.current_player_can_raise(),
            "first actor on a fresh street may raise"
        );
    }

    /// Regression (audit 2026-06-03): after a non-reopening (sub-minimum) all-in
    /// that RAISES `current_bet`, `players_yet_to_act()` must still count the
    /// already-acted players who now owe the new difference. Previously it read
    /// `actions_remaining` (driven to 0 by the non-aggression decrement) and
    /// reported 0 while two players still owed a call.
    #[test]
    fn players_yet_to_act_counts_owers_after_non_reopening_all_in() {
        // Postflop 3-way, current_bet 0, bb 100. P1 bets 100, P2 calls 100,
        // P3 short all-in to 150 (delta 50 < 100 min → non-reopening).
        let mut round = BettingRound::new(three_players(1000, 1000, 150), 0, c(0), c(100));
        round
            .apply_action(pid(1), &PlayerAction::Raise { amount: c(100) })
            .unwrap();
        round.apply_action(pid(2), &PlayerAction::Call).unwrap();
        round.apply_action(pid(3), &PlayerAction::AllIn).unwrap();
        assert_eq!(round.current_bet().0, 150, "all-in raised the bet to 150");
        // Round stays open; action is back on P1 who owes 50.
        assert!(
            !round.is_done(),
            "P1 and P2 still owe 50 — round must stay open"
        );
        assert_eq!(round.current_player(), Some(pid(1)));
        // P1 is the current actor (excluded); P2 still owes 50 and is not all-in
        // → players_yet_to_act must be 1, not 0.
        assert_eq!(
            round.players_yet_to_act(),
            1,
            "P2 still owes a call after the non-reopening all-in"
        );
    }

    // A FULL raise must still reopen action for a player who already acted.
    #[test]
    fn full_raise_reopens_action_for_acted_player() {
        let mut round = BettingRound::new(three_players(1000, 1000, 1000), 0, c(0), c(20));
        round
            .apply_action(pid(1), &PlayerAction::Raise { amount: c(100) })
            .unwrap(); // p1 opens
        round.apply_action(pid(2), &PlayerAction::Call).unwrap(); // p2 calls
                                                                  // p3 makes a FULL raise to 300 (delta 200 >= 100) — reopens.
        round
            .apply_action(pid(3), &PlayerAction::Raise { amount: c(300) })
            .unwrap();
        // p1 (already acted) is reopened and MAY re-raise legally.
        assert_eq!(round.current_player(), Some(pid(1)));
        let ok = round.apply_action(pid(1), &PlayerAction::Raise { amount: c(700) });
        assert!(ok.is_ok(), "full raise must reopen action, got {ok:?}");
    }
}
