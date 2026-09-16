//! Pot settlement: side-pot construction, showdown assembly, `finish` /
//! `finish_blind*`, and the pure `distribute_pots*` awarding functions.
//!
//! Split out of the old 6k-line `game.rs`. These are `impl GameHand` methods on
//! the very same type — no behaviour change, no public-path change.

use super::*;

impl GameHand {
    /// Produce the final result and emit a [`EngineEvent::HandFinished`] event.
    ///
    /// Plaintext mode only. For blind-mode hands call [`GameHand::finish_blind`]
    /// instead — `finish` would panic on the `assert` below if any non-folded
    /// contender's value were missing.
    ///
    /// # Panics
    ///
    /// Panics if [`is_done()`] is not true. Callers must check [`is_done()`]
    /// before calling. Also panics if called on a blind-mode hand (use
    /// [`GameHand::finish_blind`]).
    pub fn finish(&mut self) -> HandResult {
        assert!(self.phase == Phase::Done, "hand not finished");
        assert!(
            self.mode == HandMode::Plaintext,
            "finish() is plaintext-only; blind-mode hands must use finish_blind()"
        );

        // Players still in (non-folded) with plaintext hole cards. In plaintext
        // mode every non-folded seat holds `Plain` after `start()`.
        let eligible: Vec<(PlayerId, HoleCards)> = self
            .seats
            .iter()
            .filter(|s| !s.folded)
            .filter_map(|s| s.hole_cards().map(|h| (s.player_id, h)))
            .collect();

        // Build a normal contested/lone showdown (plaintext path is unchanged).
        let showdown = self.build_plaintext_showdown(&eligible);
        self.assemble_result(eligible, showdown)
    }

    /// Produce the final result for a **blind-mode** hand (ADR-066 P1).
    ///
    /// Distribution and showdown use **only** injected, verified reveals:
    ///
    /// - **Uncontested (`non-folded contenders <= 1`)** — there is no showdown
    ///   and **no reveal is required**. The lone survivor (if any) takes the pot
    ///   via the card-free single-contender path; the showdown is **empty**.
    ///   `rank_hand` is never called on the lone survivor, so "uncontested =
    ///   zero leak" is structural, not a runtime check.
    /// - **Contested (`>= 2`)** — every non-folded contender MUST have an
    ///   injected [`HoleSlot::Revealed`] (fed by [`GameHand::inject_showdown_reveal`]).
    ///   Ranking uses only those reveals. If any non-folded contender is still
    ///   `Opaque` (or holds any non-`Revealed` slot), this returns
    ///   [`BlindFinishError::MissingReveal`] — it never panics and never invents
    ///   cards.
    ///
    /// # Errors
    ///
    /// Never panics. Returns [`BlindFinishError::NotDone`] if the hand has not
    /// reached `Phase::Done`, [`BlindFinishError::NotBlindMode`] if called on a
    /// plaintext hand (use [`GameHand::finish`]), or
    /// [`BlindFinishError::MissingReveal`] if a contested contender lacks a
    /// verified reveal — in every case the server can VOID the hand rather than
    /// crash the room.
    pub fn finish_blind(&mut self) -> Result<HandResult, BlindFinishError> {
        // Preconditions return typed errors (never panic) so the server can VOID
        // the hand rather than crash the room (ADR-066 — blind mode never panics).
        if self.phase != Phase::Done {
            return Err(BlindFinishError::NotDone);
        }
        if self.mode != HandMode::Blind {
            return Err(BlindFinishError::NotBlindMode);
        }

        let non_folded: Vec<&HandSeat> = self.seats.iter().filter(|s| !s.folded).collect();

        if non_folded.len() <= 1 {
            // Uncontested: structural zero-leak. No reveal needed, no rank_hand.
            // Award the lone survivor (if any) via the card-free path and emit
            // an EMPTY showdown.
            let lone: Option<PlayerId> = non_folded.first().map(|s| s.player_id);
            let result = self.assemble_blind_uncontested_result(lone);
            self.events.push(EngineEvent::HandFinished {
                result: result.clone(),
            });
            return Ok(result);
        }

        // Contested: every non-folded contender must have a verified reveal.
        // The blind contested rank path consumes ONLY injected `Revealed` slots
        // — a stray `Plain` (which blind mode never writes; unreachable in prod)
        // is treated as MISSING, not as a reveal, so ranking can never feed on
        // anything but an externally-verified showdown reveal (ADR-066 §2). This
        // hardens the "ranking uses only injected `Revealed`" invariant against
        // an out-of-protocol `Plain`/`Opaque`/`Empty` seat.
        let mut eligible: Vec<(PlayerId, HoleCards)> = Vec::with_capacity(non_folded.len());
        for seat in &non_folded {
            match seat.hole {
                HoleSlot::Revealed(h) => eligible.push((seat.player_id, h)),
                HoleSlot::Plain(_) | HoleSlot::Opaque | HoleSlot::Empty => {
                    // No verified reveal for a non-folded contender at a contested
                    // showdown. Fail loudly with a typed error; never invent
                    // cards, never panic.
                    return Err(BlindFinishError::MissingReveal {
                        player_id: seat.player_id,
                        seat: seat.seat,
                    });
                }
            }
        }

        let showdown = self.build_plaintext_showdown(&eligible);
        let result = self.assemble_result(eligible, showdown);
        Ok(result)
    }

    /// Produce the final result for a **blind-mode** hand, treating any non-folded
    /// contender that lacks a verified reveal as a **FORFEIT** (ADR-066 §5 T-LIVE
    /// forfeit-not-refund) rather than a hard error.
    ///
    /// This is the contested-showdown sibling of [`GameHand::finish_blind`] for the
    /// server-blind choreography: a contender that withholds, disconnects, or
    /// supplies a value that failed `verify_and_open` (so the server never injected
    /// a `Revealed` slot for it) is **excluded from the eligible set**. Its already-
    /// committed chips stay in the pots (the side pots were absorbed at street
    /// close and are unchanged), so they are awarded to the contenders who DID
    /// reveal — the cardroom "won't table a called hand → muck and lose" rule.
    /// This makes stalling strictly −EV (no stake-return for a non-revealer).
    ///
    /// Difference from [`GameHand::finish_blind`]:
    /// - **Uncontested** (`non-folded <= 1`): identical — structural zero-leak,
    ///   no reveal needed, no `rank_hand`.
    /// - **Contested** (`>= 2`): the eligible set is **only** the non-folded
    ///   contenders with an injected [`HoleSlot::Revealed`]. Non-revealed
    ///   contenders are dropped (forfeit). If **no** contender revealed (the
    ///   blameless quorum-wide failure — there is no honest revealer to award the
    ///   pot to), this returns [`BlindFinishError::MissingReveal`] for the first
    ///   such contender so the server VOIDs the hand with a stake-return rather
    ///   than awarding chips to no one.
    ///
    /// # Errors
    ///
    /// Same typed errors as [`GameHand::finish_blind`]; never panics, never
    /// invents cards.
    pub fn finish_blind_with_forfeits(&mut self) -> Result<HandResult, BlindFinishError> {
        if self.phase != Phase::Done {
            return Err(BlindFinishError::NotDone);
        }
        if self.mode != HandMode::Blind {
            return Err(BlindFinishError::NotBlindMode);
        }

        let non_folded: Vec<&HandSeat> = self.seats.iter().filter(|s| !s.folded).collect();

        if non_folded.len() <= 1 {
            // Uncontested: structural zero-leak, identical to `finish_blind`.
            let lone: Option<PlayerId> = non_folded.first().map(|s| s.player_id);
            let result = self.assemble_blind_uncontested_result(lone);
            self.events.push(EngineEvent::HandFinished {
                result: result.clone(),
            });
            return Ok(result);
        }

        // Contested: eligible = non-folded contenders WITH a verified reveal.
        // Non-revealed contenders forfeit (excluded). Only `Revealed` slots are
        // ranked — a stray `Plain`/`Opaque`/`Empty` is a forfeit, never invented.
        let mut eligible: Vec<(PlayerId, HoleCards)> = Vec::with_capacity(non_folded.len());
        let mut first_unrevealed: Option<(PlayerId, u8)> = None;
        // The non-folded contenders who did NOT reveal — forfeiters. A side pot
        // whose eligible set is entirely forfeiters must NOT be refunded to them
        // (ADR-066 §5 forfeit-not-refund / audit F3); its chips go to the honest
        // revealers. Tracked here so `assemble_result` can distinguish a forfeiter
        // (non-folded, no reveal) from a genuinely folded "no contest" refund.
        let mut forfeiters: HashSet<u64> = HashSet::new();
        for seat in &non_folded {
            match seat.hole {
                HoleSlot::Revealed(h) => eligible.push((seat.player_id, h)),
                HoleSlot::Plain(_) | HoleSlot::Opaque | HoleSlot::Empty => {
                    if first_unrevealed.is_none() {
                        first_unrevealed = Some((seat.player_id, seat.seat));
                    }
                    forfeiters.insert(seat.player_id.inner());
                }
            }
        }

        if eligible.is_empty() {
            // Blameless quorum-wide failure: not a single contender revealed, so
            // there is no honest revealer to award the pot to. Surface
            // `MissingReveal` so the server VOIDs (stake-return), never awards.
            let (player_id, seat) = first_unrevealed.expect("non_folded.len() >= 2 ⇒ some seat");
            return Err(BlindFinishError::MissingReveal { player_id, seat });
        }

        let showdown = self.build_plaintext_showdown(&eligible);
        let result = self.assemble_result_with_forfeits(eligible, showdown, &forfeiters);
        Ok(result)
    }

    /// Resolve a blind-mode hand whose **board cannot be revealed** because a
    /// non-folded contender withheld (or aborted) a required board decryption
    /// share (audit LIVE-F1). The named `withholders` — each an **attributable**
    /// non-folded contender that failed to contribute its board share — are
    /// **force-folded** (so they FORFEIT their already-committed chips), and the
    /// pot is awarded to the remaining honest contenders. This makes withholding a
    /// required board share strictly −EV for the withholder (it can NEVER recover
    /// its all-in stake by stalling the board), closing the abort/withhold→refund
    /// hole the void path left open.
    ///
    /// Forfeit-not-refund: the board is interleaved AFTER betting, so by the
    /// turn/river a contender may be all-in for real chips. A blameless
    /// VOID + stake-return would hand that all-in stake BACK to the withholder
    /// (`+EV` to stall). Instead the withholder's committed chips stay in the pots
    /// and flow to the honest survivors.
    ///
    /// Resolution among survivors (the honest n-of-n residual, ADR-066 §5):
    /// - **One honest survivor**: it takes the whole pot (card-free, uncontested) —
    ///   the realistic 2-contender all-in case.
    /// - **Multiple honest survivors**: the board never came, so the engine cannot
    ///   rank them on merit — it **splits the contested pots equally** among the
    ///   honest survivors (TDA-25 odd-chip placement). They each get their own
    ///   committed chips back plus an equal share of the forfeiter's stake. Full
    ///   *fair* ranking under a withholder needs a (t,n) threshold scheme and is
    ///   deferred (P5/future); an equal chop is the chip-conserving, withholder-
    ///   forfeiting interim that never returns a withholder's matched stake.
    ///
    /// Uncalled-overage layers (`refund_to`) are ALWAYS returned to their owner —
    /// even a withholder's — because an uncalled bet was never matched (no one
    /// could win it), so returning it is not a +EV exploit of the withhold.
    ///
    /// # Errors
    /// - [`BlindFinishError::NotBlindMode`] in plaintext mode.
    /// - [`BlindFinishError::NoSurvivor`] if force-folding every named withholder
    ///   leaves no non-folded contender (coordinator misuse) — the server must
    ///   VOID with a blameless stake-return rather than award chips to no one.
    ///
    /// Never panics, never invents cards (the board is never recovered here — the
    /// award is card-free: a lone survivor or an equal chop).
    pub fn finish_blind_forfeit_board(
        &mut self,
        withholders: &[PlayerId],
    ) -> Result<HandResult, BlindFinishError> {
        if self.mode != HandMode::Blind {
            return Err(BlindFinishError::NotBlindMode);
        }

        // Absorb any still-open betting round into the cumulative pots before we
        // resolve, so no committed chip is stranded (mirrors
        // `close_street_and_advance`). At a `pending_board_street` the next
        // street's round exists with zero new contributions in an all-in run-out,
        // so this typically absorbs 0 — but doing it unconditionally is the
        // chip-safe move regardless of where the withhold landed.
        if let Some(round) = self.betting_round.take() {
            let new_pots = round.side_pots();
            let round_total = new_pots.iter().map(|p| p.amount.0).sum::<u32>();
            self.completed_pot.0 += round_total;
            self.accumulate_pots(new_pots);
        }
        // Capture entitlement before force-folding withholders. Their matched
        // stake is forfeited to honest survivors, not reclassified as a normal
        // folded-player refund.
        let settlement_pots = self.settlement_side_pots();

        // Force-fold each attributable withholder (only a currently non-folded
        // seat — a folded/unknown/duplicate id is ignored, never an error: the
        // coordinator may pass a superset and we forfeit only the live contenders).
        for pid in withholders {
            if let Some(seat) = self
                .seats
                .iter_mut()
                .find(|s| s.player_id == *pid && !s.folded)
            {
                seat.folded = true;
                seat.all_in = false; // a forfeiter is out of the hand
            }
        }

        // The board is unrecoverable → the hand ends now (no more streets).
        self.pending_board_street = None;
        self.set_done();

        let survivors: Vec<PlayerId> = self
            .seats
            .iter()
            .filter(|s| !s.folded)
            .map(|s| s.player_id)
            .collect();
        if survivors.is_empty() {
            // Every contender was force-folded — there is no honest survivor to
            // award the pot to. Surface a typed error so the server VOIDs with a
            // blameless stake-return rather than inventing a winner.
            return Err(BlindFinishError::NoSurvivor);
        }

        let button_order = self.button_relative_order();
        let pot_results =
            distribute_pots_among_survivors(&settlement_pots, &survivors, &button_order);
        // The board was not recovered, so this is a chip settlement without a
        // showdown. Keep the result card-free: `show_order` is the protocol's
        // explicit signal that no cards/rank should be exposed.
        let result = self.finalize_pots_with_show_order(pot_results, Vec::new(), Vec::new());
        self.events.push(EngineEvent::HandFinished {
            result: result.clone(),
        });
        Ok(result)
    }

    /// Build the showdown vector from a non-empty eligible set (shared by the
    /// plaintext `finish` and the contested blind branch). Mirrors the original
    /// `finish` showdown logic exactly so plaintext output is byte-for-byte
    /// unchanged.
    pub(super) fn build_plaintext_showdown(
        &self,
        eligible: &[(PlayerId, HoleCards)],
    ) -> Vec<ShowdownEntry> {
        if eligible.len() > 1 {
            let ranked = rank_players(eligible, &self.board);
            ranked
                .into_iter()
                .map(|(pid, rank)| {
                    let hole = eligible
                        .iter()
                        .find(|(p, _)| *p == pid)
                        .map(|(_, h)| *h)
                        .unwrap(); // safe: pid is from eligible
                    ShowdownEntry {
                        player_id: pid,
                        hole_cards: hole,
                        rank_name: rank.name().to_string(),
                        rank,
                    }
                })
                .collect()
        } else {
            // Single-eligible (fold-around) entry: fill `rank_name` from the
            // computed rank so the wire shape matches the contested branch
            // instead of carrying a real `rank` next to an empty `rank_name`
            // (audit 2026-06-03).
            eligible
                .iter()
                .map(|(pid, hole)| {
                    let rank = crate::eval::rank_hand(hole, &self.board);
                    ShowdownEntry {
                        player_id: *pid,
                        hole_cards: *hole,
                        rank,
                        rank_name: rank.name().to_string(),
                    }
                })
                .collect()
        }
    }

    /// Distribute pots from a plaintext `eligible` set and assemble the final
    /// `HandResult` + chip-conservation check. Shared by `finish` and the
    /// contested blind branch.
    ///
    /// The plaintext (non-blind) path has no forfeiters — every non-folded seat
    /// either reaches showdown with cards or folded — so it delegates with an
    /// empty forfeiter set, preserving its output byte-for-byte.
    pub(super) fn assemble_result(
        &mut self,
        eligible: Vec<(PlayerId, HoleCards)>,
        showdown: Vec<ShowdownEntry>,
    ) -> HandResult {
        self.assemble_result_with_forfeits(eligible, showdown, &HashSet::new())
    }

    /// Forfeit-aware result assembly (ADR-066 §5 / audit F3). Identical to
    /// [`Self::assemble_result`] except it carries the set of **forfeiters** —
    /// non-folded contenders that did NOT reveal — so a side pot whose eligible
    /// set is entirely forfeiters is awarded to the honest revealers rather than
    /// refunded to the forfeiters. An empty `forfeiters` set reproduces the
    /// plaintext path exactly.
    pub(super) fn assemble_result_with_forfeits(
        &mut self,
        eligible: Vec<(PlayerId, HoleCards)>,
        showdown: Vec<ShowdownEntry>,
        forfeiters: &HashSet<u64>,
    ) -> HandResult {
        // TDA Rule 25 odd-chip placement: the remainder of a split pot goes to
        // the tied winner in the earliest position — the first seat left of the
        // button. Build a `PlayerId -> button-relative order` map (0 = first seat
        // clockwise after the button) so `distribute_pots` can place the odd
        // chip deterministically by table position rather than by pot.eligible
        // (action) order (audit 2026-06-03).
        let button_order = self.button_relative_order();

        let settlement_pots = self.settlement_side_pots();
        let pot_results = distribute_pots(
            &settlement_pots,
            &eligible,
            &self.board,
            &button_order,
            forfeiters,
        );

        let result = self.finalize_pots(pot_results, showdown);

        // Emit HandFinished event.
        self.events.push(EngineEvent::HandFinished {
            result: result.clone(),
        });

        result
    }

    /// Assemble a blind-mode **uncontested** result: award the lone survivor (if
    /// any) the entire pot via a card-free single-contender path, with an EMPTY
    /// showdown. No `HoleCards` are ever constructed — uncontested = structural
    /// zero-leak. `rank_hand` is not called.
    pub(super) fn assemble_blind_uncontested_result(&self, lone: Option<PlayerId>) -> HandResult {
        let button_order = self.button_relative_order();
        let settlement_pots = self.settlement_side_pots();
        let pot_results = distribute_pots_uncontested(&settlement_pots, lone, &button_order);
        self.finalize_pots(pot_results, Vec::new())
    }

    /// Shared tail of result assembly: chip-conservation check, `chips_awarded`,
    /// `final_stacks`, `show_order`, and the `HandResult` struct. Does NOT emit
    /// the `HandFinished` event (callers decide whether to emit).
    pub(super) fn finalize_pots(
        &self,
        pot_results: Vec<PotResult>,
        showdown: Vec<ShowdownEntry>,
    ) -> HandResult {
        self.finalize_pots_with_show_order(pot_results, showdown, self.compute_show_order())
    }

    pub(super) fn finalize_pots_with_show_order(
        &self,
        pot_results: Vec<PotResult>,
        showdown: Vec<ShowdownEntry>,
        show_order: Vec<PlayerId>,
    ) -> HandResult {
        // chips_awarded: sum from all pots.
        let mut chips_awarded: HashMap<u64, u32> = self
            .seats
            .iter()
            .map(|s| (s.player_id.inner(), 0u32))
            .collect();
        for pr in &pot_results {
            for w in &pr.winners {
                *chips_awarded.entry(w.player_id.inner()).or_default() += w.amount_won.0;
            }
        }

        // Invariant: every chip in every pot is awarded or refunded — never
        // destroyed. A future regression that drops a pot (e.g. the orphaned
        // side-pot bug fixed 2026-05-29 #5) panics here instead of silently
        // leaking chips in production.
        //
        // backend review F-ENG-7: this is a deliberate always-on failsafe, so
        // it uses `assert_eq!` (active in release) rather than `debug_assert_eq!`
        // (compiled out of `--release`). The invariant is already proven by the
        // engine property tests (`prop_chip_conservation`,
        // `prop_side_pot_multistreet`, `orphaned_side_pot_does_not_destroy_chips`)
        // so it never fires in practice; and per the architecture a panic that
        // kills a single room is strictly preferable to silently corrupting the
        // persisted stacks of a distribution regression. (A graceful Result return
        // would change this fn's signature across many call sites — out of scope.)
        for pot in &pot_results {
            assert_eq!(
                pot.winners
                    .iter()
                    .map(|winner| u64::from(winner.amount_won.0))
                    .sum::<u64>(),
                u64::from(pot.amount.0),
                "every pot must award or refund its full amount"
            );
        }
        let total_awarded = chips_awarded
            .values()
            .map(|amount| u64::from(*amount))
            .sum::<u64>();
        let result_pot_total = pot_results
            .iter()
            .map(|pot| u64::from(pot.amount.0))
            .sum::<u64>();
        let legacy_pot_total = self
            .cumulative_side_pots
            .iter()
            .map(|pot| u64::from(pot.amount.0))
            .sum::<u64>();
        let total_contributed =
            self.seats
                .iter()
                .map(|seat| {
                    u64::from(
                        seat.starting_stack.0.checked_sub(seat.stack.0).expect(
                            "in-hand stack cannot exceed its starting stack before settlement",
                        ),
                    )
                })
                .sum::<u64>();
        assert_eq!(
            legacy_pot_total, total_contributed,
            "street bookkeeping must contain every committed chip"
        );
        assert_eq!(
            result_pot_total, total_contributed,
            "canonical pots must equal the whole hand's committed chips"
        );
        assert_eq!(
            total_awarded, result_pot_total,
            "chip conservation: awards must equal canonical pots"
        );

        // final_stacks = current stack + chips awarded.
        let final_stacks: HashMap<u64, u32> = self
            .seats
            .iter()
            .map(|s| {
                let awarded = *chips_awarded.get(&s.player_id.inner()).unwrap_or(&0);
                (
                    s.player_id.inner(),
                    s.stack
                        .0
                        .checked_add(awarded)
                        .expect("settled stack must fit the public u32 chip range"),
                )
            })
            .collect();
        assert_eq!(
            final_stacks
                .values()
                .map(|stack| u64::from(*stack))
                .sum::<u64>(),
            self.seats
                .iter()
                .map(|seat| u64::from(seat.starting_stack.0))
                .sum::<u64>(),
            "final stacks must equal starting stacks"
        );

        // Folded players' dealt hole cards — plaintext hands ONLY. In blind mode
        // folded seats are `Opaque` (a folded seat can never be `Revealed` —
        // `inject_showdown_reveal` rejects folded players), so filtering on
        // `hole_cards()` already yields an empty map there; gating on
        // `HandMode::Plaintext` makes engine-blind fail-closed doubly structural
        // (no blind result ever carries a folded value). This backs the server's
        // post-hand voluntary-show affordance for folded humans (issue #6).
        let folded_hole_cards: HashMap<u64, HoleCards> = if self.mode == HandMode::Plaintext {
            self.seats
                .iter()
                .filter(|s| s.folded)
                .filter_map(|s| s.hole_cards().map(|h| (s.player_id.inner(), h)))
                .collect()
        } else {
            HashMap::new()
        };

        HandResult {
            deck_seed: self.deck_seed,
            board: self.board.clone(),
            pots: pot_results,
            chips_awarded,
            final_stacks,
            actions: self.actions.clone(),
            showdown,
            show_order,
            folded_hole_cards,
        }
    }

    /// Compute the WSOP / TDA canonical show-order for the current showdown.
    ///
    /// Called from `finish()` after the showdown is built. The returned
    /// `Vec<PlayerId>` lists non-folded players in the order they should
    /// reveal hole cards at showdown. See `HandResult.show_order` for the
    /// per-rule details.
    pub(super) fn compute_show_order(&self) -> Vec<PlayerId> {
        // Non-folded seats are the only candidates.
        let alive_count = self.seats.iter().filter(|s| !s.folded).count();
        if alive_count <= 1 {
            return Vec::new();
        }

        // Did anyone bet/raise on the river?
        // Walk river actions in order; track each player's contribution before
        // their action and flag aggression when the action moved current_bet up
        // (Raise / AllIn that exceeded the running bet) — `actions` carries
        // pot_before/pot_after but not current_bet, so use action-shape +
        // amount > 0 to identify aggression.
        let mut river_current_bet: u32 = 0;
        let mut river_committed: std::collections::HashMap<u64, u32> =
            std::collections::HashMap::new();
        let mut last_river_aggressor: Option<PlayerId> = None;
        for a in &self.actions {
            if a.street != Street::River {
                continue;
            }
            let pid_key = a.player_id.inner();
            let prior = *river_committed.get(&pid_key).unwrap_or(&0);
            let new_total = prior + a.amount.0;
            river_committed.insert(pid_key, new_total);
            match a.action {
                PlayerAction::Raise { .. } | PlayerAction::AllIn
                    if new_total > river_current_bet =>
                {
                    river_current_bet = new_total;
                    last_river_aggressor = Some(a.player_id);
                }
                _ => {}
            }
        }

        // Pick the start seat:
        //   * river aggressor exists → their seat
        //   * else → first non-folded seat clockwise from button (dealer + 1)
        let n = self.seats.len();
        let first_left_of_button = (self.dealer_idx % n + 1) % n;
        let start_idx = if let Some(pid) = last_river_aggressor {
            self.seats
                .iter()
                .position(|s| s.player_id == pid)
                .unwrap_or(first_left_of_button)
        } else {
            // Clockwise from the button — start the search at dealer+1 and walk
            // through non-folded seats.
            let mut idx = first_left_of_button;
            for _ in 0..n {
                if !self.seats[idx].folded {
                    break;
                }
                idx = (idx + 1) % n;
            }
            idx
        };

        // Build the show order by walking clockwise from start_idx and emitting
        // non-folded seats. start_idx may itself be folded only when alive is
        // empty (already returned above), so it is always included.
        let mut order: Vec<PlayerId> = Vec::with_capacity(alive_count);
        let mut idx = start_idx;
        for _ in 0..n {
            if !self.seats[idx].folded {
                order.push(self.seats[idx].player_id);
            }
            idx = (idx + 1) % n;
            if order.len() == alive_count {
                break;
            }
        }
        order
    }

    /// Map each player to its button-relative seat order: 0 for the first seat
    /// clockwise after the button (small blind in 3+ handed), 1 for the next,
    /// and so on. Used for TDA Rule 25 odd-chip placement (the odd chip goes to
    /// the tied winner in the earliest position, i.e. the lowest order).
    pub(super) fn button_relative_order(&self) -> HashMap<u64, usize> {
        let n = self.seats.len();
        let mut order = HashMap::with_capacity(n);
        if n == 0 {
            return order;
        }
        let first_left_of_button = (self.dealer_idx % n + 1) % n;
        for offset in 0..n {
            let idx = (first_left_of_button + offset) % n;
            order.insert(self.seats[idx].player_id.inner(), offset);
        }
        order
    }

    /// The hand-global pot decomposition visible right now.
    pub(super) fn merged_side_pots(&self) -> Vec<SidePot> {
        self.hand_side_pots()
    }

    /// Rebuild canonical pots from each player's total contribution for the
    /// whole hand. Street-local pot fragments are bookkeeping only: real side
    /// pot boundaries arise when total contributions (normally all-in caps)
    /// change the set of players entitled to a layer.
    pub(super) fn hand_side_pots(&self) -> Vec<SidePot> {
        let all: Vec<(PlayerId, u32)> = self
            .seats
            .iter()
            .map(|seat| {
                (
                    seat.player_id,
                    seat.starting_stack
                        .0
                        .checked_sub(seat.stack.0)
                        .expect("in-hand stack cannot exceed its starting stack"),
                )
            })
            .collect();
        let eligible: Vec<(PlayerId, u32)> = self
            .seats
            .iter()
            .filter(|seat| !seat.folded)
            .map(|seat| {
                (
                    seat.player_id,
                    seat.starting_stack
                        .0
                        .checked_sub(seat.stack.0)
                        .expect("in-hand stack cannot exceed its starting stack"),
                )
            })
            .collect();
        if eligible.is_empty() {
            return Vec::new();
        }

        let mut levels: Vec<u32> = all.iter().map(|(_, amount)| *amount).collect();
        levels.sort_unstable();
        levels.dedup();
        let mut previous = 0u32;
        let mut pots: Vec<SidePot> = Vec::new();

        for level in levels.into_iter().filter(|level| *level > 0) {
            let slice = level - previous;
            let amount = all
                .iter()
                .map(|(_, contribution)| contribution.saturating_sub(previous).min(slice))
                .sum::<u32>();
            let pot_eligible: Vec<PlayerId> = eligible
                .iter()
                .filter(|(_, contribution)| *contribution >= level)
                .map(|(pid, _)| *pid)
                .collect();
            let contributors: Vec<PlayerId> = all
                .iter()
                .filter(|(_, contribution)| *contribution > previous)
                .map(|(pid, _)| *pid)
                .collect();

            let (eligible_ids, refund_to) = if pot_eligible.is_empty() {
                (Vec::new(), contributors)
            } else {
                let refund = if pot_eligible.len() == 1 && contributors == pot_eligible {
                    pot_eligible.clone()
                } else {
                    Vec::new()
                };
                (pot_eligible, refund)
            };
            if amount > 0 {
                pots.push(SidePot {
                    cap: Chips(level),
                    amount: Chips(amount),
                    eligible: eligible_ids,
                    refund_to,
                });
            }
            previous = level;
        }

        let mut canonical: Vec<SidePot> = Vec::new();
        for pot in pots {
            if let Some(existing) = canonical.last_mut().filter(|existing| {
                existing.eligible == pot.eligible && existing.refund_to == pot.refund_to
            }) {
                existing.amount.0 += pot.amount.0;
                existing.cap.0 = existing.cap.0.max(pot.cap.0);
            } else {
                canonical.push(pot);
            }
        }
        canonical
    }

    /// Canonical, hand-global pots used for every terminal settlement path.
    pub(super) fn settlement_side_pots(&self) -> Vec<SidePot> {
        self.hand_side_pots()
    }

    /// Accumulate new side pots into the running cumulative list.
    pub(super) fn accumulate_pots(&mut self, new_pots: Vec<SidePot>) {
        if new_pots.is_empty() {
            return;
        }
        self.cumulative_side_pots.extend(new_pots);
    }
}

/// Place odd chips in button-relative order, with player id breaking ties.
pub(super) fn odd_chip_recipients(
    ids: &[PlayerId],
    rem: u32,
    button_order: &HashMap<u64, usize>,
) -> Vec<PlayerId> {
    if rem == 0 {
        return Vec::new();
    }
    let mut sorted = ids.to_vec();
    sorted.sort_by_key(|pid| {
        (
            button_order
                .get(&pid.inner())
                .copied()
                .unwrap_or(usize::MAX),
            pid.inner(),
        )
    });
    sorted.truncate(rem as usize);
    sorted
}

/// Distribute each pot to winner(s).
///
/// `button_order` maps each player to its button-relative seat order (0 = first
/// seat left of the button). A split pot's odd chips are spread one per seat to
/// the tied winners with the lowest order (TDA Rule 25). When `button_order` is
/// missing a player (defensive), that player sorts last.
///
/// `forfeiters` (ADR-066 §5 / audit F3) is the set of non-folded contenders that
/// did NOT reveal at a blind-mode showdown. It is **empty** for the plaintext
/// path (which has no forfeit concept). When a pot has no contender reaching
/// showdown (`contenders.is_empty()`) the refund branch must distinguish two
/// cases by inspecting `pot.eligible`:
/// - every eligible contributor genuinely **folded** (none is a forfeiter): a
///   real "no contest" pot → refund to the eligible contributors (unchanged);
/// - at least one eligible contributor is a **forfeiter**: a forfeiter must
///   never get its stake back, so the layer is awarded to the honest revealers
///   (`eligible_with_cards`), ranked among themselves — forfeit-not-refund.
pub(super) fn distribute_pots(
    pots: &[SidePot],
    eligible_with_cards: &[(PlayerId, HoleCards)],
    board: &BoardCards,
    button_order: &HashMap<u64, usize>,
    forfeiters: &HashSet<u64>,
) -> Vec<PotResult> {
    if pots.is_empty() {
        return vec![];
    }

    pots.iter()
        .enumerate()
        .filter_map(|(idx, pot)| {
            // Filter to non-folded eligible players who have cards.
            let contenders: Vec<(PlayerId, HoleCards)> = pot
                .eligible
                .iter()
                .filter_map(|pid| {
                    eligible_with_cards
                        .iter()
                        .find(|(p, _)| p == pid)
                        .map(|(p, h)| (*p, *h))
                })
                .collect();

            if pot.amount.0 == 0 {
                return None;
            }

            if !pot.refund_to.is_empty() {
                let refund_ids = &pot.refund_to;
                let n = refund_ids.len() as u32;
                let base = pot.amount.0 / n;
                let rem = pot.amount.0 % n;
                let odd_recipients = odd_chip_recipients(refund_ids, rem, button_order);
                let winners = refund_ids
                    .iter()
                    .map(|pid| PotWinner {
                        player_id: *pid,
                        amount_won: Chips(base + if odd_recipients.contains(pid) { 1 } else { 0 }),
                    })
                    .collect();
                return Some(PotResult {
                    index: idx,
                    amount: pot.amount,
                    eligible_player_ids: if pot.eligible.is_empty() {
                        refund_ids.to_vec()
                    } else {
                        pot.eligible.clone()
                    },
                    winners,
                    is_refund: true,
                });
            }

            // No eligible player reached showdown for THIS layer. Two distinct
            // cases (the second is ADR-066 §5 / audit F3, blind mode only):
            //
            // (1) Every player eligible for this pot genuinely **folded** ("no
            //     contest" side pot). Real-poker resolution is to return the
            //     stakes, not destroy them: refund the pot equally to its eligible
            //     contributors (each contributed an equal share of this layer by
            //     side-pot construction). The previous `return None` here silently
            //     destroyed the chips, violating chip conservation (bug-sweep
            //     2026-05-29 finding #5: reachable when a side pot's whole eligible
            //     set folds on a later street, e.g. both deep stacks fold an
            //     uncontested pot to an already-all-in short stack).
            //
            // (2) At least one eligible contributor is a **forfeiter** (non-folded
            //     but withheld/failed its blind showdown reveal). A forfeiter must
            //     NEVER get its stake back (forfeit-not-refund) — that would reward
            //     stalling. The layer is instead awarded to the honest revealers
            //     (`eligible_with_cards`), ranked among themselves, exactly as if
            //     this layer were contested by them. `eligible_with_cards` is
            //     guaranteed non-empty here: a contested blind finish that reaches
            //     distribution has ≥1 honest revealer (zero-revealer contests VOID
            //     before distribution), and the plaintext path has no forfeiters.
            if contenders.is_empty() {
                let has_forfeiter = pot.eligible.iter().any(|p| forfeiters.contains(&p.inner()));
                if has_forfeiter && !eligible_with_cards.is_empty() {
                    // Forfeited layer → award to the honest revealers (ranked),
                    // NOT refunded to the forfeiters.
                    let winners = if eligible_with_cards.len() == 1 {
                        vec![PotWinner {
                            player_id: eligible_with_cards[0].0,
                            amount_won: pot.amount,
                        }]
                    } else {
                        let ranked = rank_players(eligible_with_cards, board);
                        let best = ranked[0].1;
                        let shared: Vec<PlayerId> = ranked
                            .iter()
                            .take_while(|(_, r)| *r == best)
                            .map(|(pid, _)| *pid)
                            .collect();
                        let count = shared.len() as u32;
                        let base = pot.amount.0 / count;
                        let rem = pot.amount.0 % count;
                        let odd_recipients = odd_chip_recipients(&shared, rem, button_order);
                        shared
                            .iter()
                            .map(|pid| PotWinner {
                                player_id: *pid,
                                amount_won: Chips(
                                    base + if odd_recipients.contains(pid) { 1 } else { 0 },
                                ),
                            })
                            .collect()
                    };
                    return Some(PotResult {
                        index: idx,
                        amount: pot.amount,
                        // The award is on merit (a forfeit loss), not a refund —
                        // the eligible-id list stays the layer's original set.
                        eligible_player_ids: pot.eligible.clone(),
                        winners,
                        is_refund: false,
                    });
                }

                // Refund target: the layer's eligible contributors. Explicit
                // uncalled-overage refunds are handled by `refund_to` above.
                let refund_ids: &[PlayerId] = &pot.eligible;
                let n = refund_ids.len() as u32;
                if n == 0 {
                    return None;
                }
                let base = pot.amount.0 / n;
                let rem = pot.amount.0 % n;
                let odd_recipients = odd_chip_recipients(refund_ids, rem, button_order);
                let winners = refund_ids
                    .iter()
                    .map(|pid| PotWinner {
                        player_id: *pid,
                        amount_won: Chips(base + if odd_recipients.contains(pid) { 1 } else { 0 }),
                    })
                    .collect();
                return Some(PotResult {
                    index: idx,
                    amount: pot.amount,
                    eligible_player_ids: refund_ids.to_vec(),
                    winners,
                    is_refund: true,
                });
            }

            let pot_eligible_ids = pot.eligible.clone();

            let winners = if contenders.len() == 1 {
                vec![PotWinner {
                    player_id: contenders[0].0,
                    amount_won: pot.amount,
                }]
            } else {
                let ranked = rank_players(&contenders, board);
                let best = ranked[0].1;
                let shared: Vec<PlayerId> = ranked
                    .iter()
                    .take_while(|(_, r)| *r == best)
                    .map(|(pid, _)| *pid)
                    .collect();
                let count = shared.len() as u32;
                let base = pot.amount.0 / count;
                let rem = pot.amount.0 % count;
                // TDA Rule 25: the odd chip goes to the tied winner in the
                // earliest position (first seat left of the button), NOT to
                // whoever is first in action order (audit 2026-06-03).
                let odd_recipients = odd_chip_recipients(&shared, rem, button_order);
                shared
                    .iter()
                    .map(|pid| PotWinner {
                        player_id: *pid,
                        amount_won: Chips(base + if odd_recipients.contains(pid) { 1 } else { 0 }),
                    })
                    .collect()
            };

            Some(PotResult {
                index: idx,
                amount: pot.amount,
                eligible_player_ids: pot_eligible_ids,
                winners,
                is_refund: false,
            })
        })
        .collect()
}

/// Card-free pot distribution for a blind-mode **uncontested** hand
/// (ADR-066 §2 step 6). No `HoleCards` exist; the winner is decided purely by
/// "who is the lone non-folded survivor" — there is no ranking and no card is
/// ever read. This is the structural zero-leak path: it is impossible for a
/// hole value to influence (or even reach) the outcome.
///
/// - `lone == Some(pid)`: that player is the single non-folded survivor and wins
///   each pot it is eligible for; pots it is not eligible for (a higher all-in
///   side-pot layer it never matched) are refunded to their eligible
///   contributors, exactly like the `contenders.is_empty()` branch of
///   [`distribute_pots`].
/// - `lone == None`: every player folded (degenerate); every pot is refunded to
///   its eligible contributors.
///
/// Chip conservation holds: each pot is fully awarded or fully refunded.
pub(super) fn distribute_pots_uncontested(
    pots: &[SidePot],
    lone: Option<PlayerId>,
    button_order: &HashMap<u64, usize>,
) -> Vec<PotResult> {
    if pots.is_empty() {
        return vec![];
    }

    let refund = |idx: usize, pot: &SidePot| -> Option<PotResult> {
        // Prefer an explicit `refund_to` (uncalled overage); else the eligible
        // contributors of the layer.
        let refund_ids: &[PlayerId] = if !pot.refund_to.is_empty() {
            &pot.refund_to
        } else {
            &pot.eligible
        };
        let n = refund_ids.len() as u32;
        if n == 0 {
            return None;
        }
        let base = pot.amount.0 / n;
        let rem = pot.amount.0 % n;
        let odd_recipients = odd_chip_recipients(refund_ids, rem, button_order);
        let winners = refund_ids
            .iter()
            .map(|pid| PotWinner {
                player_id: *pid,
                amount_won: Chips(base + if odd_recipients.contains(pid) { 1 } else { 0 }),
            })
            .collect();
        Some(PotResult {
            index: idx,
            amount: pot.amount,
            eligible_player_ids: if pot.eligible.is_empty() {
                refund_ids.to_vec()
            } else {
                pot.eligible.clone()
            },
            winners,
            is_refund: true,
        })
    };

    pots.iter()
        .enumerate()
        .filter_map(|(idx, pot)| {
            if pot.amount.0 == 0 {
                return None;
            }
            // Explicit uncalled-overage layers always refund.
            if !pot.refund_to.is_empty() {
                return refund(idx, pot);
            }
            match lone {
                // The lone survivor wins every pot it is eligible for; otherwise
                // (a side-pot layer it never matched) the layer is refunded.
                Some(pid) if pot.eligible.contains(&pid) => Some(PotResult {
                    index: idx,
                    amount: pot.amount,
                    eligible_player_ids: pot.eligible.clone(),
                    winners: vec![PotWinner {
                        player_id: pid,
                        amount_won: pot.amount,
                    }],
                    is_refund: false,
                }),
                _ => refund(idx, pot),
            }
        })
        .collect()
}

/// Distribute pots when the board is **unrevealable** because a contender
/// withheld a required board share (audit LIVE-F1, `finish_blind_forfeit_board`).
/// The `withholders` have already been force-folded; `survivors` is the set of
/// remaining non-folded contenders. Each contested pot is split **equally** among
/// the survivors that are eligible for it (TDA-25 odd-chip placement to the
/// earliest button-relative seats). The withholder's committed chips therefore
/// flow to the honest survivors — never refunded (forfeit-not-refund).
///
/// Uncalled-overage layers (`refund_to`) ALWAYS return to their owner (an
/// uncalled bet was never matched, so no one could win it — returning it is not a
/// +EV exploit of the withhold). A contested pot whose eligible set contains NO
/// survivor (every eligible contributor for THAT layer was force-folded) is
/// awarded to the honest overall `survivors` — NOT refunded to its eligible set
/// (audit F5). Refunding it would hand the withholders their matched side-pot
/// stake back, making a withheld side-pot board share +EV; the only refund path
/// is `refund_to` (uncalled overage). The all-withholders whole-hand case (no
/// overall survivor) is rejected upstream by the caller's `NoSurvivor` guard.
pub(super) fn distribute_pots_among_survivors(
    pots: &[SidePot],
    survivors: &[PlayerId],
    button_order: &HashMap<u64, usize>,
) -> Vec<PotResult> {
    if pots.is_empty() {
        return vec![];
    }

    // Split a pot's `amount` equally among `recipients` (>=1), with `is_refund`
    // controlling the flag on the resulting `PotResult`.
    let split = |idx: usize,
                 pot: &SidePot,
                 recipients: &[PlayerId],
                 is_refund: bool|
     -> Option<PotResult> {
        let n = recipients.len() as u32;
        if n == 0 {
            return None;
        }
        let base = pot.amount.0 / n;
        let rem = pot.amount.0 % n;
        let odd = odd_chip_recipients(recipients, rem, button_order);
        let winners = recipients
            .iter()
            .map(|pid| PotWinner {
                player_id: *pid,
                amount_won: Chips(base + if odd.contains(pid) { 1 } else { 0 }),
            })
            .collect();
        Some(PotResult {
            index: idx,
            amount: pot.amount,
            eligible_player_ids: if pot.eligible.is_empty() {
                recipients.to_vec()
            } else {
                pot.eligible.clone()
            },
            winners,
            is_refund,
        })
    };

    pots.iter()
        .enumerate()
        .filter_map(|(idx, pot)| {
            if pot.amount.0 == 0 {
                return None;
            }
            // Explicit uncalled-overage layers always refund to their owner.
            if !pot.refund_to.is_empty() {
                return split(idx, pot, &pot.refund_to, true);
            }
            // Contested layer: split equally among the survivors eligible for it.
            let eligible_survivors: Vec<PlayerId> = if pot.eligible.is_empty() {
                survivors.to_vec()
            } else {
                pot.eligible
                    .iter()
                    .copied()
                    .filter(|pid| survivors.contains(pid))
                    .collect()
            };
            if eligible_survivors.is_empty() {
                // No survivor is eligible for THIS layer — every eligible
                // contributor was force-folded (audit LIVE-F1 / F5). Refunding
                // the layer to `pot.eligible` here would hand the withholders
                // their matched side-pot stake back, making a withheld board
                // share +EV (the F5 exploit: an honest main-pot survivor keeps
                // the hand out of `NoSurvivor`, but a withholder-only side pot
                // leaked the refund). Forfeit-not-refund: award the layer to the
                // honest overall survivors instead. (`refund_to` uncalled-overage
                // is the ONLY refund path; the all-withholders whole-hand case is
                // still caught upstream by the `NoSurvivor` guard.)
                if !survivors.is_empty() {
                    return split(idx, pot, survivors, false);
                }
                // Defensive: no overall survivor reached this distributor (the
                // caller's `NoSurvivor` guard should have rejected first). Refund
                // to the eligible set so no chip is destroyed rather than panic.
                return split(idx, pot, &pot.eligible, true);
            }
            split(idx, pot, &eligible_survivors, false)
        })
        .collect()
}
