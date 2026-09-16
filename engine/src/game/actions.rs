//! Player actions: the pure `validate_action` / `preview_action_transition`
//! preflight pair, `apply_action`, and the current-street action-history views.
//!
//! Split out of the old 6k-line `game.rs`. These are `impl GameHand` methods on
//! the very same type — no behaviour change, no public-path change.

use super::*;

impl GameHand {
    /// Apply an action from the current player.
    ///
    /// This is the pure preflight companion to [`Self::apply_action`]. It runs
    /// the complete betting-round legality check on a clone and therefore does
    /// not move chips, allocate an action sequence, append history, or emit an
    /// event. A single-owner coordinator can use it to finish every fallible
    /// protocol/chain check before the real mutation.
    pub fn validate_action(
        &self,
        player_id: PlayerId,
        action: &PlayerAction,
    ) -> Result<(), ActionError> {
        self.preview_action_transition(player_id, action)
            .map(|_| ())
    }

    /// A cheap speculative copy of this hand for
    /// [`Self::preview_action_transition`] / [`Self::validate_action`].
    ///
    /// Identical to `self.clone()` in every field the betting / street
    /// machinery READS, but the three append-only history buffers
    /// (`actions`, `events`, `last_action`) start empty instead of being
    /// deep-copied: nothing reachable from `apply_action` (including
    /// `close_street_and_advance`, which only appends and then patches the
    /// `StreetRevealed` it just pushed itself) reads them back, and
    /// `preview_action_transition` never returns a field derived from them.
    /// `finish()` / `compute_show_order()` DO read `actions`, but the preview
    /// path never reaches them — the previewed hand is dropped immediately.
    ///
    /// This is the hot engine-blind pre-action check: the old full clone copied
    /// a growing `Vec<ActionRecord>`, a growing `Vec<EngineEvent>` (each with
    /// its own heap payloads) and a `HashMap` on every single action.
    pub(super) fn preview_clone(&self) -> Self {
        Self {
            seats: self.seats.clone(),
            dealer_idx: self.dealer_idx,
            big_blind: self.big_blind,
            small_blind: self.small_blind,
            forced_straddle: self.forced_straddle,
            preflop_bring_in: self.preflop_bring_in,
            mode: self.mode,
            deck: self.deck.clone(),
            deck_seed: self.deck_seed,
            board: self.board.clone(),
            phase: self.phase.clone(),
            betting_round: self.betting_round.clone(),
            completed_pot: self.completed_pot,
            cumulative_side_pots: self.cumulative_side_pots.clone(),
            // Append-only history: intentionally NOT copied (see above).
            actions: Vec::new(),
            action_seq: self.action_seq,
            last_action: HashMap::new(),
            events: Vec::new(),
            pending_board_street: self.pending_board_street,
            finished_street: self.finished_street,
        }
    }

    /// Preview the exact public pre/post betting state without mutating the hand.
    pub fn preview_action_transition(
        &self,
        player_id: PlayerId,
        action: &PlayerAction,
    ) -> Result<ActionTransitionPreview, ActionError> {
        match &self.phase {
            Phase::NotStarted => return Err(ActionError::HandNotStarted),
            Phase::Done => return Err(ActionError::HandFinished),
            Phase::Betting(_) => {}
        }
        if !self.seats.iter().any(|seat| seat.player_id == player_id) {
            return Err(ActionError::NotInHand);
        }
        // Pre-action values read straight off the live round — no clone, no
        // speculative `apply_action` on a second copy of it (the post-action
        // round-closed flag now comes back from the single speculative apply
        // below, which computes the exact same `round.is_done()`).
        let round_before = self
            .betting_round
            .as_ref()
            .ok_or(ActionError::HandFinished)?;
        let before = round_before
            .player_states()
            .iter()
            .find(|player| player.player_id == player_id)
            .ok_or(ActionError::NotInHand)?;
        let stack_before = before.stack;
        let committed_before = before.contributed;
        let current_bet_before = round_before.current_bet();
        let min_raise_to_before = round_before
            .current_player_can_raise()
            .then(|| round_before.min_raise_to());

        // `round_closed_after` intentionally describes the round in which the
        // action was taken. All other post fields below come from the fully
        // stabilized hand (which may already contain the next street's round).
        let mut hand_after = self.preview_clone();
        let (after_snapshot, round_closed_after) =
            hand_after.apply_action_tracked(player_id, action.clone())?;
        let after = after_snapshot
            .players
            .iter()
            .find(|player| player.player_id == player_id)
            .expect("previewed player remains in hand snapshot");
        let next_actor = after_snapshot.current_actor;
        let next_actor_can_raise_after = next_actor.is_some()
            && hand_after
                .betting_round_ref()
                .is_some_and(BettingRound::current_player_can_raise);
        let betting_terminal_after = hand_after.is_done()
            || hand_after
                .betting_round_ref()
                .is_none_or(BettingRound::is_done);
        Ok(ActionTransitionPreview {
            stack_before,
            committed_before,
            current_bet_before,
            min_raise_to_before,
            stack_after: after.stack,
            committed_after: after.committed_this_street,
            current_bet_after: after_snapshot.current_bet,
            min_raise_to_after: after_snapshot.min_raise_to,
            all_in_after: after.all_in,
            round_closed_after,
            next_actor,
            next_actor_can_raise_after,
            street_after: after_snapshot.street,
            betting_terminal_after,
            hand_done_after: hand_after.is_done(),
        })
    }

    /// Apply an action from the current player.
    pub fn apply_action(
        &mut self,
        player_id: PlayerId,
        action: PlayerAction,
    ) -> Result<GameSnapshot, ActionError> {
        self.apply_action_tracked(player_id, action)
            .map(|(snapshot, _round_closed)| snapshot)
    }

    /// [`Self::apply_action`] plus the `round.is_done()` flag observed for the
    /// round the action was taken in — i.e. BEFORE any street close swaps in
    /// the next street's betting round. Only `preview_action_transition` needs
    /// it; the public wrapper above drops it so the API is unchanged.
    pub(super) fn apply_action_tracked(
        &mut self,
        player_id: PlayerId,
        action: PlayerAction,
    ) -> Result<(GameSnapshot, bool), ActionError> {
        match &self.phase {
            Phase::NotStarted => return Err(ActionError::HandNotStarted),
            Phase::Done => return Err(ActionError::HandFinished),
            Phase::Betting(_) => {}
        }

        let current_street = self.current_street();
        let action_seq = self.action_seq;

        // Capture pre-action values.
        let stack_before = self
            .seats
            .iter()
            .find(|s| s.player_id == player_id)
            .map(|s| s.stack)
            .ok_or(ActionError::NotInHand)?;

        let pot_before = {
            let round_pot = self
                .betting_round
                .as_ref()
                .map(|r| r.pot_total())
                .unwrap_or(Chips::ZERO);
            Chips(self.completed_pot.0 + round_pot.0)
        };

        // Normalize a zero-chip `Call` (nothing to call) to `Check` for the
        // recorded/emitted label: it is the functionally-equivalent legal action
        // (0 chips move either way, same state transition), and recording it as a
        // `call` would pollute action history / coach inputs with an illegal
        // zero-chip call (C6 review). The applied state transition is unchanged.
        let recorded_action = {
            let to_call = self
                .betting_round
                .as_ref()
                .map(|r| r.to_call(player_id))
                .unwrap_or(Chips::ZERO);
            if matches!(action, PlayerAction::Call) && to_call.0 == 0 {
                PlayerAction::Check
            } else {
                action.clone()
            }
        };

        // Apply action.
        let round = self
            .betting_round
            .as_mut()
            .ok_or(ActionError::HandFinished)?;
        let chips_moved = round.apply_action(player_id, &action)?;

        let stack_after = Chips(stack_before.0.saturating_sub(chips_moved.0));
        let pot_after = Chips(pot_before.0 + chips_moved.0);
        let round_done = round.is_done();
        let player_folded = round.player_folded(player_id);
        let player_all_in = round.player_all_in(player_id);

        // Sync seat state.
        for seat in &mut self.seats {
            if seat.player_id == player_id {
                seat.stack = stack_after;
                seat.folded = player_folded;
                seat.all_in = player_all_in;
            }
        }

        // Record the action (zero-chip Call already relabelled to Check above).
        self.actions.push(ActionRecord {
            seq: action_seq,
            street: current_street,
            player_id,
            action: recorded_action.clone(),
            amount: chips_moved,
            stack_before,
            stack_after,
            pot_before,
            pot_after,
        });
        self.action_seq = self.action_seq.saturating_add(1); // saturating (U33)
        self.last_action
            .insert(player_id.inner(), recorded_action.clone());

        // Emit ActionApplied event.
        // SAFETY: player_id was validated above by the stack_before lookup (ok_or NotInHand?),
        // so a seat must exist here. Using expect instead of unwrap_or to surface any
        // future refactor that breaks this invariant loudly.
        let acting_seat = self
            .seats
            .iter()
            .find(|s| s.player_id == player_id)
            .map(|s| s.seat)
            .expect("player_id was validated above; seat must exist");
        // Compute post-action betting state for the ActionApplied event.
        // These fields let the server stream live betting state to clients
        // without waiting for a full Snapshot round-trip.
        // NOTE: if round_done is true the street is about to close; fields are
        // None / 0 for that final event, which is correct — no one can act.
        let action_current_bet = self
            .betting_round
            .as_ref()
            .map(|r| r.current_bet().0 as u64)
            .unwrap_or(0);
        // Suppress min_raise_to when the next actor cannot legally raise —
        // i.e. after a non-reopening (sub-minimum) all-in they may only call or
        // fold (TDA Rule 6). Advertising a min_raise_to there implied a raise
        // the engine would then reject (audit 2026-06-03).
        let action_min_raise_to = if round_done {
            None
        } else {
            self.betting_round.as_ref().and_then(|r| {
                if r.current_player_can_raise() {
                    Some(r.min_raise_to().0 as u64)
                } else {
                    None
                }
            })
        };
        let action_next_actor_seat = if round_done {
            None
        } else {
            self.betting_round.as_ref().and_then(|r| {
                r.current_player().and_then(|pid| {
                    self.seats
                        .iter()
                        .find(|s| s.player_id == pid)
                        .map(|s| s.seat)
                })
            })
        };
        self.events.push(EngineEvent::ActionApplied {
            seat: acting_seat,
            action: recorded_action,
            contributed: chips_moved.0 as u64,
            current_bet: action_current_bet,
            min_raise_to: action_min_raise_to,
            next_actor_seat: action_next_actor_seat,
            // ADR-079 (F2): the engine seq captured at function entry (line ~750),
            // identical to the recorded `ActionRecord.seq` for this action.
            action_seq,
        });

        // Emit PotUpdated if chips moved (fold/check contribute 0).
        if chips_moved.0 > 0 {
            let current_round_pot = self
                .betting_round
                .as_ref()
                .map(|r| r.pot_total())
                .unwrap_or(Chips::ZERO);
            let total_pot = self.completed_pot.0 as u64 + current_round_pot.0 as u64;
            let side_pots = self.merged_side_pots();
            // Snapshot all-in state at emit time so the translate layer can apply
            // the has_all_in guard without needing access to full engine state.
            let has_all_in = self.seats.iter().any(|s| s.all_in);
            self.events.push(EngineEvent::PotUpdated {
                pot: total_pot,
                side_pots,
                has_all_in,
            });
        }

        if round_done {
            return self
                .close_street_and_advance()
                .map(|snapshot| (snapshot, true));
        }

        Ok((self.snapshot(), false))
    }

    /// Returns all actions recorded on the **current street** as `(player_id, action)` pairs.
    ///
    /// Blind posts (`PlayerAction::Blind { .. }`) are excluded — they are not
    /// voluntary betting actions and must not be treated as aggression when
    /// building bot `DecisionContext.street_actions`.
    ///
    /// This is a raw compatibility view. `AllIn` and call-shaped `Raise` tokens
    /// are semantically ambiguous, so strategy, coach, and aggressor consumers
    /// must use [`Self::current_street_action_facts`] (or its strategy
    /// projection) instead of inferring aggression from this method.
    pub fn current_street_actions(&self) -> Vec<(PlayerId, PlayerAction)> {
        let current = self.current_street();
        self.actions
            .iter()
            .filter(|rec| {
                rec.street == current && !matches!(rec.action, PlayerAction::Blind { .. })
            })
            .map(|rec| (rec.player_id, rec.action.clone()))
            .collect()
    }

    /// Current-street actions with authoritative wager semantics.
    ///
    /// An `AllIn` token alone is ambiguous.  This projection distinguishes an
    /// all-in call/under-call, a short all-in raise, and a full raise by
    /// replaying only public commitment totals.  Preflop starts from the
    /// nominal bring-in even when the posted blind was short; later streets
    /// start from zero.  `last_full_raise` changes only after a full wager,
    /// exactly like [`BettingRound`].
    ///
    /// The preflop baseline is [`Self::preflop_bring_in`], NOT `big_blind`
    /// (discovery-r1 BUG-175). On a live-straddle table those differ, and
    /// seeding the baseline from `big_blind` OVER-reports aggression, because
    /// every commitment is measured against a bar that sits one straddle too
    /// low. With a 2 BB straddle (bring-in 40, big blind 20):
    ///
    /// * An all-in for exactly 40 merely CALLS the straddle, but `40 > 20` read
    ///   as `increased_bet`, and its phantom 20 delta cleared the equally
    ///   phantom 20 increment, so it was recorded as a FULL RAISE — inventing a
    ///   raise level for the bots and the coach out of a call.
    /// * An all-in strictly between the big blind and the straddle (say 30) is
    ///   an UNDER-call, yet `30 > 20` flagged it `increased_bet` too, so
    ///   [`StreetActionFact::is_call_like`] stopped counting it as a caller.
    ///
    /// Both disappear once the projection starts from the same opening wager
    /// `BettingRound::new_preflop` was handed.
    pub fn current_street_action_facts(&self) -> Vec<StreetActionFact> {
        self.street_action_facts(self.current_street())
    }

    /// The seat-agnostic identity of the hand's current **aggressor** — the
    /// player who made the most recent wager increase (bet / raise / short
    /// all-in raise) so far, on ANY street: the current street if it has seen
    /// aggression, otherwise the latest earlier street that did. Blind posts
    /// never count. `None` for a hand with no wager increase yet (limped pot
    /// checked down so far).
    ///
    /// This is the poker "aggressor" that survives street boundaries — the
    /// preflop raiser who c-bets the flop, the flop bettor who barrels the
    /// turn — and is what ADR-043 §3.4.2's `is_aggressor` (rules R3 c-bet /
    /// R6 semi-bluff barrel) means. It differs from
    /// [`Self::current_street_action_facts`]-derived aggression, which resets at
    /// every street: whenever hero can check nobody has raised THIS street yet,
    /// so a current-street-only reading made R3/R6 unreachable on the live
    /// coach path (discovery-r1 BUG-236) and every checked-to / first-to-act
    /// spot was graded as a Check regardless of strength.
    pub fn last_aggressor_player_id(&self) -> Option<PlayerId> {
        const ORDER: [Street; 4] = [Street::Preflop, Street::Flop, Street::Turn, Street::River];
        let current = self.current_street();
        let dealt = ORDER.iter().position(|s| *s == current).unwrap_or(0);
        ORDER[..=dealt].iter().rev().copied().find_map(|street| {
            self.street_action_facts(street)
                .into_iter()
                .rev()
                .find(|fact| fact.increased_bet)
                .map(|fact| fact.player_id)
        })
    }

    /// [`Self::current_street_action_facts`] for an arbitrary `street` of this
    /// hand (empty when that street has not been dealt yet). Same replay
    /// semantics: preflop starts from the nominal bring-in, later streets from
    /// zero, blinds are commitment but never aggression.
    pub fn street_action_facts(&self, street: Street) -> Vec<StreetActionFact> {
        let current_street = street;
        let preflop = current_street == Street::Preflop;
        // Preflop the opening wager IS the minimum raise increment. Postflop the
        // increment reverts to the big blind, matching `BettingRound::new`
        // (which is constructed with `current_bet = 0, big_blind`) — a straddle
        // never raises the postflop increment.
        let bring_in = self.preflop_bring_in.0.max(self.big_blind.0);
        let mut current_bet = if preflop { bring_in } else { 0 };
        let mut last_full_raise = if preflop { bring_in } else { self.big_blind.0 };
        let mut committed_by_player: HashMap<u64, u32> = HashMap::new();
        let mut facts = Vec::new();

        for record in self
            .actions
            .iter()
            .filter(|record| record.street == current_street)
        {
            let committed = committed_by_player
                .entry(record.player_id.inner())
                .or_default();
            *committed = committed
                .checked_add(record.amount.0)
                .expect("one player's street commitment cannot exceed u32");

            if matches!(record.action, PlayerAction::Blind { .. }) {
                continue;
            }

            let aggressive_shape = matches!(
                record.action,
                PlayerAction::Raise { .. } | PlayerAction::AllIn
            );
            let increased_bet = aggressive_shape && *committed > current_bet;
            let raise_delta = (*committed).saturating_sub(current_bet);
            let full_raise = increased_bet && raise_delta >= last_full_raise;

            if increased_bet {
                current_bet = *committed;
            }
            if full_raise {
                last_full_raise = raise_delta;
            }

            facts.push(StreetActionFact {
                player_id: record.player_id,
                action: record.action.clone(),
                increased_bet,
                full_raise,
            });
        }

        facts
    }

    /// Strategy-facing current-street history.
    ///
    /// Full raises retain their `Raise` / `AllIn` shape.  A short raise or an
    /// all-in call becomes `Call`, while its real aggressor identity remains
    /// available from [`Self::current_street_action_facts`].
    pub fn current_street_strategy_actions(&self) -> Vec<(PlayerId, PlayerAction)> {
        self.current_street_action_facts()
            .into_iter()
            .map(|fact| (fact.player_id, fact.strategy_action()))
            .collect()
    }

    /// The sequence number that the next successfully-applied action will
    /// receive. Blinds consume sequence numbers 0 and 1, so the first
    /// voluntary action of a normal hand is 2.
    ///
    /// This is intentionally read-only: the engine remains the sole allocator
    /// of action sequence numbers. The engine-blind coordinator uses the value
    /// to validate a client's pre-signed action claim *before* mutating betting
    /// state, then `apply_action` records the same value atomically.
    pub fn next_action_seq(&self) -> u16 {
        self.action_seq
    }
}
