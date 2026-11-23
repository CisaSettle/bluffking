//! Pot settlement: side-pot construction, showdown assembly, `finish` /
//! `finish_blind*`, and the pure `distribute_pots*` awarding functions.
//!
//! Split out of the old 6k-line `game.rs`. These are `impl GameHand` methods on
//! the very same type — no behaviour change, no public-path change.

use super::*;
use crate::betting::{pot_layers, Contribution, PotPolicy};

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
        let settlement_pots = self.hand_side_pots();

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
        Self::showdown_on(eligible, &self.board)
    }

    /// Rank a non-empty eligible set on `board` (first or second run).
    pub(super) fn showdown_on(
        eligible: &[(PlayerId, HoleCards)],
        board: &BoardCards,
    ) -> Vec<ShowdownEntry> {
        if eligible.len() > 1 {
            let ranked = rank_players(eligible, board);
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
                    let rank = crate::eval::rank_hand(hole, board);
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

        let settlement_pots = self.hand_side_pots();
        let (pot_results, run_twice) = match (self.run_twice_from, self.second_board.as_ref()) {
            // All-in runout dealt twice: each contested pot is halved (the odd
            // chip stays with the first run) and each half is awarded on its
            // own board. Uncalled-overage refunds stay whole in the first run.
            (Some(from_street), Some(second_board)) => {
                let (first_half, second_half) = split_pots_for_two_runs(&settlement_pots);
                let first_pots = distribute_pots(
                    &first_half,
                    &eligible,
                    &self.board,
                    &button_order,
                    forfeiters,
                );
                let second_pots = distribute_pots(
                    &second_half,
                    &eligible,
                    second_board,
                    &button_order,
                    forfeiters,
                );
                let merged = merge_run_pots(&settlement_pots, &first_pots, &second_pots);
                let second_showdown = if eligible.is_empty() {
                    Vec::new()
                } else {
                    Self::showdown_on(&eligible, second_board)
                };
                (
                    merged,
                    Some(RunTwiceResult {
                        from_street,
                        second_board: second_board.clone(),
                        first_pots,
                        second_pots,
                        second_showdown,
                    }),
                )
            }
            _ => (
                distribute_pots(
                    &settlement_pots,
                    &eligible,
                    &self.board,
                    &button_order,
                    forfeiters,
                ),
                None,
            ),
        };

        let mut result = self.finalize_pots(pot_results, showdown);
        result.run_twice = run_twice;

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
        let settlement_pots = self.hand_side_pots();
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
            run_twice: None,
            seven_deuce: Vec::new(),
            premium_hand: Vec::new(),
            bounty_topups: HashMap::new(),
            host_insurance: None,
        }
    }

    /// Apply the room's seven-deuce bounty to a finished hand's `result`, once
    /// every dealt seat's hole cards are known (`showdown` plus
    /// `folded_hole_cards`, which the sealed dealer fills after settlement).
    ///
    /// Requires the flop to have been dealt and a 7-2 offsuit holder to be the
    /// main pot's sole winner; a split main pot never qualifies (when running
    /// twice, the holder must win the main pot on both runs). The winner
    /// collects the full configured amount from every other dealt player. Shortages are explicit automatic
    /// funding in `bounty_topups`: economic net remains zero-sum; `pots` and
    /// `chips_awarded` stay pot-only. The queued `HandFinished` event is
    /// updated to the same result. Idempotent: a result that already carries
    /// bounties is left unchanged. `exempt` players pay nothing (the server
    /// passes players who left the table during the hand, whose result was
    /// already fixed at what they committed).
    pub fn apply_seven_deuce_bounty(
        &mut self,
        result: &mut HandResult,
        exempt: &std::collections::HashSet<PlayerId>,
    ) {
        let amount = self.seven_deuce_bounty.0;
        if amount == 0 || result.board.flop.is_none() || !result.seven_deuce.is_empty() {
            return;
        }
        let Some(main_pot) = result
            .pots
            .iter()
            .filter(|pot| !pot.is_refund)
            .min_by_key(|pot| pot.index)
        else {
            return;
        };
        let hole_cards = |pid: PlayerId| {
            result
                .showdown
                .iter()
                .find(|entry| entry.player_id == pid)
                .map(|entry| entry.hole_cards)
                .or_else(|| result.folded_hole_cards.get(&pid.inner()).copied())
        };
        let [sole] = main_pot.winners.as_slice() else {
            return;
        };
        if sole.amount_won.0 == 0 {
            return;
        }
        let winners: Vec<(PlayerId, HoleCards)> = self
            .seats
            .iter()
            .filter(|seat| !seat.folded && seat.player_id == sole.player_id)
            .filter_map(|seat| hole_cards(seat.player_id).map(|cards| (seat.player_id, cards)))
            .filter(|(_, cards)| is_seven_deuce_offsuit(cards))
            .collect();
        if winners.is_empty() {
            return;
        }
        let bounties = self.collect_bounties(result, amount, winners, exempt);
        result.seven_deuce = bounties;
        for event in self.events.iter_mut().rev() {
            if let EngineEvent::HandFinished { result: queued } = event {
                queued.final_stacks = result.final_stacks.clone();
                queued.seven_deuce = result.seven_deuce.clone();
                queued.bounty_topups = result.bounty_topups.clone();
                break;
            }
        }
    }

    /// Pay a rare-hand main-pot winner once per hand. Shared-board hands and
    /// split main pots do not qualify. Either run can qualify when running twice.
    pub fn apply_premium_hand_bounty(
        &mut self,
        result: &mut HandResult,
        exempt: &HashSet<PlayerId>,
    ) {
        let amount = self.premium_hand_bounty.0;
        if amount == 0 || !result.premium_hand.is_empty() {
            return;
        }
        let qualifies = |board: &BoardCards, pots: &[PotResult], entries: &[ShowdownEntry]| {
            let main = pots
                .iter()
                .filter(|p| !p.is_refund)
                .min_by_key(|p| p.index)?;
            if main.winners.len() != 1 {
                return None;
            }
            let winner = &main.winners[0];
            if winner.amount_won.0 == 0 {
                return None;
            }
            if board.count() < 3 {
                return None;
            }
            let hole_cards = entries
                .iter()
                .find(|e| e.player_id == winner.player_id)
                .map(|e| e.hole_cards)
                .or_else(|| {
                    result
                        .folded_hole_cards
                        .get(&winner.player_id.inner())
                        .copied()
                })?;
            let rank = crate::eval::rank_hand(&hole_cards, board);
            if !matches!(rank, HandRank::StraightFlush(_) | HandRank::FourOfAKind(_)) {
                return None;
            }
            // Board quads never qualify, including a turn fold-around.
            if matches!(rank, HandRank::FourOfAKind(_)) {
                let cards = board.all_cards();
                if cards
                    .iter()
                    .any(|c| cards.iter().filter(|other| other.rank == c.rank).count() == 4)
                {
                    return None;
                }
            }
            // At least one hole card must improve the five-card board.
            if board.count() == 5 {
                let board_rank = crate::eval::rank_board(board);
                if board_rank == rank || matches!(board_rank, HandRank::FourOfAKind(_)) {
                    return None;
                }
            }
            Some((winner.player_id, hole_cards))
        };
        let mut winners = Vec::new();
        if let Some(runs) = &result.run_twice {
            for winner in [
                qualifies(&result.board, &runs.first_pots, &result.showdown),
                qualifies(&runs.second_board, &runs.second_pots, &runs.second_showdown),
            ]
            .into_iter()
            .flatten()
            {
                if !winners.iter().any(|(pid, _)| *pid == winner.0) {
                    winners.push(winner);
                }
            }
        } else if let Some(winner) = qualifies(&result.board, &result.pots, &result.showdown) {
            winners.push(winner);
        }
        result.premium_hand = self.collect_premium_bounties(result, amount, winners, exempt);
        for event in self.events.iter_mut().rev() {
            if let EngineEvent::HandFinished { result: queued } = event {
                queued.final_stacks = result.final_stacks.clone();
                queued.premium_hand = result.premium_hand.clone();
                queued.bounty_topups = result.bounty_topups.clone();
                break;
            }
        }
    }

    /// Qualifying winners do not pay each other. Each other player pays
    /// every qualifying winner in full, with automatic funding if necessary.
    fn collect_premium_bounties(
        &self,
        result: &mut HandResult,
        amount: u32,
        mut winners: Vec<(PlayerId, HoleCards)>,
        exempt: &HashSet<PlayerId>,
    ) -> Vec<HandBounty> {
        if winners.is_empty() {
            return Vec::new();
        }
        winners.sort_by_key(|(pid, _)| self.seats.iter().position(|s| s.player_id == *pid));
        let mut bounties: Vec<_> = winners
            .iter()
            .map(|(winner, hole_cards)| HandBounty {
                winner: *winner,
                hole_cards: *hole_cards,
                amount: Chips(amount),
                payments: Vec::new(),
            })
            .collect();
        for seat in &self.seats {
            let payer = seat.player_id;
            if exempt.contains(&payer) || winners.iter().any(|(pid, _)| *pid == payer) {
                continue;
            }
            for bounty in &mut bounties {
                if let Some(payment) = Self::pay_bounty(result, payer, bounty.winner, amount) {
                    bounty.payments.push(payment);
                }
            }
        }
        bounties
    }

    fn collect_bounties(
        &self,
        result: &mut HandResult,
        amount: u32,
        winners: Vec<(PlayerId, HoleCards)>,
        exempt: &HashSet<PlayerId>,
    ) -> Vec<HandBounty> {
        let mut bounties = Vec::new();
        for (winner, hole_cards) in winners {
            let mut payments = Vec::new();
            for seat in &self.seats {
                let payer = seat.player_id;
                if payer == winner || exempt.contains(&payer) {
                    continue;
                }
                if let Some(payment) = Self::pay_bounty(result, payer, winner, amount) {
                    payments.push(payment);
                }
            }
            bounties.push(HandBounty {
                winner,
                hole_cards,
                amount: Chips(amount),
                payments,
            });
        }
        bounties
    }

    /// Apply a single accepted insurance quote without changing any pot award.
    /// An uncovered host payment is recorded as automatic funding, so the
    /// host's economic loss includes the shortage even with a zero stack.
    pub fn apply_host_insurance(
        &mut self,
        result: &mut HandResult,
        mut insurance: HostInsurance,
    ) -> bool {
        let buyer = insurance.buyer.inner();
        let host = insurance.host.inner();
        let (Some(&buyer_stack), Some(&host_stack)) = (
            result.final_stacks.get(&buyer),
            result.final_stacks.get(&host),
        ) else {
            return false;
        };
        if buyer == host || result.host_insurance.is_some() || insurance.premium == 0 {
            return false;
        }
        let (buyer_end, host_end) = if insurance.payout > 0 {
            let Some(payment) = insurance.payout.checked_sub(insurance.premium) else {
                return false;
            };
            let Some(buyer_end) = buyer_stack
                .checked_add(payment)
                .filter(|&v| i32::try_from(v).is_ok())
            else {
                return false;
            };
            insurance.host_topup = payment.saturating_sub(host_stack);
            (buyer_end, host_stack.saturating_sub(payment))
        } else {
            let (Some(buyer_end), Some(host_end)) = (
                buyer_stack.checked_sub(insurance.premium),
                host_stack
                    .checked_add(insurance.premium)
                    .filter(|&v| i32::try_from(v).is_ok()),
            ) else {
                return false;
            };
            (buyer_end, host_end)
        };
        result.final_stacks.insert(buyer, buyer_end);
        result.final_stacks.insert(host, host_end);
        result.host_insurance = Some(insurance);
        for event in &mut self.events {
            if let EngineEvent::HandFinished { result: queued } = event {
                queued.final_stacks.clone_from(&result.final_stacks);
                queued.host_insurance = Some(insurance);
            }
        }
        true
    }

    /// Shared full payment for 27 and premium-hand bounties; shortages are
    /// explicit automatic funding, never forgiven or silently minted.
    fn pay_bounty(
        result: &mut HandResult,
        payer: PlayerId,
        winner: PlayerId,
        amount: u32,
    ) -> Option<PotWinner> {
        let payer_stack = result
            .final_stacks
            .get(&payer.inner())
            .copied()
            .unwrap_or(0);
        let winner_stack = result
            .final_stacks
            .get(&winner.inner())
            .copied()
            .unwrap_or(0);
        if amount == 0 {
            return None;
        }
        let topup = amount.saturating_sub(payer_stack);
        let winner_stack = winner_stack
            .checked_add(amount)
            .expect("bounty winner stack overflow");
        if topup > 0 {
            *result.bounty_topups.entry(payer.inner()).or_default() += u64::from(topup);
        }
        result
            .final_stacks
            .insert(payer.inner(), payer_stack.saturating_sub(amount));
        result.final_stacks.insert(winner.inner(), winner_stack);
        Some(PotWinner {
            player_id: payer,
            amount_won: Chips(amount),
        })
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

        // Auto-runout streets have no actions. Use the last street actually
        // played, so a called pre-river shove retains first-show priority.
        // A played, checked-through river still starts left of the button.
        let last_betting_street = [Street::River, Street::Turn, Street::Flop, Street::Preflop]
            .into_iter()
            .find(|street| self.actions.iter().any(|action| action.street == *street));
        // Reuse authoritative wager semantics: blind commitments count toward
        // preflop totals, but blinds and all-in calls are never aggression.
        let last_aggressor = last_betting_street.and_then(|street| {
            self.street_action_facts(street)
                .into_iter()
                .rev()
                .find(|fact| fact.increased_bet)
                .map(|fact| fact.player_id)
        });

        // Last played street's aggressor first; otherwise left of the button.
        let n = self.seats.len();
        let first_left_of_button = (self.dealer_idx % n + 1) % n;
        let start_idx = if let Some(pid) = last_aggressor {
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

    /// Rebuild canonical pots from each player's total contribution for the
    /// whole hand. Street-local pot fragments are bookkeeping only: real side
    /// pot boundaries arise when total contributions (normally all-in caps)
    /// change the set of players entitled to a layer.
    pub(super) fn hand_side_pots(&self) -> Vec<SidePot> {
        let contribution = |seat: &HandSeat| {
            let total = seat
                .starting_stack
                .0
                .checked_sub(seat.stack.0)
                .expect("in-hand stack cannot exceed its starting stack");
            let ante = self
                .posted_big_blind_ante
                .filter(|(payer, _)| *payer == seat.player_id)
                .map_or(0, |(_, amount)| amount.0);
            total.saturating_sub(ante)
        };
        let contributions: Vec<_> = self
            .seats
            .iter()
            .map(|seat| Contribution {
                id: seat.player_id,
                amount: contribution(seat),
                live: !seat.folded,
            })
            .collect();
        let eligible: Vec<PlayerId> = contributions
            .iter()
            .filter(|p| p.live)
            .map(|p| p.id)
            .collect();
        if eligible.is_empty() {
            return Vec::new();
        }
        let mut pots: Vec<SidePot> = Vec::new();
        if let Some((_, amount)) = self.posted_big_blind_ante {
            // A table ante is shared dead money, even when another contender
            // is all-in for less than one big blind. Never refund it to its payer.
            pots.push(SidePot {
                cap: Chips::ZERO,
                amount,
                eligible,
                refund_to: Vec::new(),
            });
        }
        pots.extend(
            pot_layers(&contributions, PotPolicy::RefundUnmatched)
                .into_iter()
                .map(SidePot::from),
        );

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

    /// Accumulate new side pots into the running cumulative list.
    pub(super) fn accumulate_pots(&mut self, new_pots: Vec<SidePot>) {
        if new_pots.is_empty() {
            return;
        }
        self.cumulative_side_pots.extend(new_pots);
    }
}

/// Split settlement pots into the two halves of a run-twice runout. Both
/// vectors keep the original order so `PotResult::index` lines up. A pot with
/// an explicit uncalled-overage refund is never contested and stays whole in
/// the first half (its second half is zero and is dropped by
/// [`distribute_pots`]).
pub(super) fn split_pots_for_two_runs(pots: &[SidePot]) -> (Vec<SidePot>, Vec<SidePot>) {
    let mut first = Vec::with_capacity(pots.len());
    let mut second = Vec::with_capacity(pots.len());
    for pot in pots {
        let second_amount = if pot.refund_to.is_empty() {
            pot.amount.0 / 2
        } else {
            0
        };
        let mut a = pot.clone();
        a.amount = Chips(pot.amount.0 - second_amount);
        let mut b = pot.clone();
        b.amount = Chips(second_amount);
        first.push(a);
        second.push(b);
    }
    (first, second)
}

/// Merge the two runs' awards back into one result per settlement pot.
/// Winners keep first-run order; a player who won both halves is listed once
/// with the sum.
pub(super) fn merge_run_pots(
    pots: &[SidePot],
    first: &[PotResult],
    second: &[PotResult],
) -> Vec<PotResult> {
    first
        .iter()
        .map(|run1| {
            let mut winners = run1.winners.clone();
            if let Some(run2) = second.iter().find(|r| r.index == run1.index) {
                for w in &run2.winners {
                    match winners.iter_mut().find(|x| x.player_id == w.player_id) {
                        Some(existing) => existing.amount_won.0 += w.amount_won.0,
                        None => winners.push(w.clone()),
                    }
                }
            }
            PotResult {
                index: run1.index,
                amount: pots.get(run1.index).map_or(run1.amount, |p| p.amount),
                eligible_player_ids: run1.eligible_player_ids.clone(),
                winners,
                is_refund: run1.is_refund,
            }
        })
        .collect()
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
pub fn distribute_pots(
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

/// 7-2 offsuit (either card order).
fn is_seven_deuce_offsuit(cards: &HoleCards) -> bool {
    use crate::card::Rank;
    let ranks = (cards.card1.rank, cards.card2.rank);
    let seven_deuce = matches!(ranks, (Rank::Seven, Rank::Two) | (Rank::Two, Rank::Seven));
    seven_deuce && cards.card1.suit != cards.card2.suit
}

#[cfg(test)]
mod premium_bounty_tests {
    use super::*;
    fn card(value: &str) -> Card {
        {
            use crate::card::{Rank, Suit};
            let rank = match value.as_bytes()[0] {
                b'A' => Rank::Ace,
                b'K' => Rank::King,
                b'Q' => Rank::Queen,
                b'J' => Rank::Jack,
                b'T' => Rank::Ten,
                b'2' => Rank::Two,
                b'3' => Rank::Three,
                b'4' => Rank::Four,
                b'7' => Rank::Seven,
                _ => panic!("test rank"),
            };
            let suit = match value.as_bytes()[1] {
                b's' => Suit::Spades,
                b'h' => Suit::Hearts,
                b'd' => Suit::Diamonds,
                b'c' => Suit::Clubs,
                _ => panic!("test suit"),
            };
            Card::new(rank, suit)
        }
    }
    fn fixture(hole: [&str; 2], board: [&str; 5]) -> (GameHand, HandResult) {
        let players = (1..=4)
            .map(|p| (PlayerId(p), Chips(100), p as u8))
            .collect();
        let mut hand = GameHand::new_with_rng(
            players,
            0,
            Chips(2),
            Chips(1),
            PokerRng::from_seed_bytes([1; 32]),
        )
        .with_premium_hand_bounty(Chips(20));
        let holes = HoleCards::new(card(hole[0]), card(hole[1]));
        let board = BoardCards {
            flop: Some([card(board[0]), card(board[1]), card(board[2])]),
            turn: Some(card(board[3])),
            river: Some(card(board[4])),
        };
        let result = HandResult {
            deck_seed: [1; 32],
            board: board.clone(),
            pots: vec![PotResult {
                index: 0,
                amount: Chips(40),
                eligible_player_ids: vec![PlayerId(1), PlayerId(2)],
                winners: vec![PotWinner {
                    player_id: PlayerId(1),
                    amount_won: Chips(40),
                }],
                is_refund: false,
            }],
            chips_awarded: HashMap::from([(1, 40)]),
            final_stacks: HashMap::from([(1, 140), (2, 7), (3, 100), (4, 100)]),
            actions: vec![],
            showdown: vec![ShowdownEntry {
                player_id: PlayerId(1),
                hole_cards: holes,
                rank: crate::eval::rank_hand(&holes, &board),
                rank_name: "test".into(),
            }],
            show_order: vec![PlayerId(1)],
            folded_hole_cards: HashMap::new(),
            run_twice: None,
            seven_deuce: vec![],
            premium_hand: vec![],
            bounty_topups: HashMap::new(),
            host_insurance: None,
        };
        hand.events.push(EngineEvent::HandFinished {
            result: result.clone(),
        });
        (hand, result)
    }

    #[test]
    fn host_insurance_funds_shortage_without_changing_pot_awards() {
        let (mut hand, mut result) =
            fixture(["Ah", "Ad"], ["As", "Kc", "2s", "3d", "4c"]);
        let awards = result.chips_awarded.clone();
        let contract = HostInsurance {
            host: PlayerId(2),
            buyer: PlayerId(1),
            outs: 4,
            odds_tenths: 80,
            premium: 5,
            payout: 40,
            host_topup: 0,
        };

        assert!(hand.apply_host_insurance(&mut result, contract));
        assert_eq!(result.final_stacks[&1], 175);
        assert_eq!(result.final_stacks[&2], 0);
        assert_eq!(result.insurance_topup(2), 28);
        assert_eq!(i64::from(result.final_stacks[&2]) - 7 - 28, -35);
        assert_eq!(result.chips_awarded, awards);
        assert!(!hand.apply_host_insurance(&mut result, contract));
        assert_eq!(result.final_stacks[&1], 175);
        assert!(matches!(
            hand.events.last(),
            Some(EngineEvent::HandFinished { result: queued })
                if queued.host_insurance == result.host_insurance
        ));
    }

    #[test]
    fn bounty_shared_transfer_matches_27_and_quads() {
        for premium in [false, true] {
            let (hole, board) = if premium {
                (["Ah", "Ad"], ["As", "Ac", "2s", "3d", "4c"])
            } else {
                (["7s", "2h"], ["As", "Kd", "Qs", "Jd", "3c"])
            };
            let (mut hand, mut result) = fixture(hole, board);
            hand.seven_deuce_bounty = Chips(20);
            let exempt = HashSet::from([PlayerId(4)]);
            for _ in 0..2 {
                if premium {
                    hand.apply_premium_hand_bounty(&mut result, &exempt);
                } else {
                    hand.apply_seven_deuce_bounty(&mut result, &exempt);
                }
                assert_eq!(
                    result.final_stacks,
                    HashMap::from([(1, 180), (2, 0), (3, 80), (4, 100)])
                );
                assert_eq!(
                    result.final_stacks.values().sum::<u32>(),
                    347 + result.bounty_topups.values().sum::<u64>() as u32
                );
                assert_eq!(result.chips_awarded[&1], 40);
            }
            let bounties = if premium {
                &result.premium_hand
            } else {
                &result.seven_deuce
            };
            assert_eq!(bounties.len(), 1);
            assert_eq!(bounties[0].payments.len(), 2);
            assert_eq!(
                bounties[0]
                    .payments
                    .iter()
                    .map(|p| p.amount_won.0)
                    .sum::<u32>(),
                40
            );
        }
    }
    #[test]
    fn bounty_all_in_payers_still_pay_full_10bb() {
        for premium in [false, true] {
            let (hole, board) = if premium {
                (["Ah", "Ad"], ["As", "Ac", "2s", "3d", "4c"])
            } else {
                (["7s", "2h"], ["As", "Kd", "Qs", "Jd", "3c"])
            };
            let (mut hand, mut result) = fixture(hole, board);
            result.final_stacks = HashMap::from([(1, 140), (2, 0), (3, 0), (4, 0)]);
            hand.seven_deuce_bounty = Chips(20);
            if premium {
                hand.apply_premium_hand_bounty(&mut result, &HashSet::new());
            } else {
                hand.apply_seven_deuce_bounty(&mut result, &HashSet::new());
            }
            assert_eq!(result.final_stacks[&1], 200);
            assert_eq!(
                result.bounty_topups,
                HashMap::from([(2, 20), (3, 20), (4, 20)])
            );
            // Real P&L is zero-sum even though table chips grew by auto funding.
            assert_eq!(
                result
                    .final_stacks
                    .values()
                    .map(|v| i64::from(*v))
                    .sum::<i64>()
                    - 140
                    - result.bounty_topups.values().sum::<u64>() as i64,
                0
            );
        }
    }

    #[test]
    fn bounty_27_trigger_and_fold_around_remain_unchanged() {
        let (mut hand, mut result) = fixture(["7s", "2h"], ["As", "Kd", "Qs", "Jd", "3c"]);
        hand.seven_deuce_bounty = Chips(20);
        // No flop, no 27 payment.
        let board = result.board.clone();
        result.board = BoardCards::empty();
        hand.apply_seven_deuce_bounty(&mut result, &HashSet::new());
        assert!(result.seven_deuce.is_empty());
        // Fold-around: released winner cards still collect after a flop.
        result.board = board;
        let hole = result.showdown.remove(0).hole_cards;
        result.folded_hole_cards.insert(1, hole);
        hand.apply_seven_deuce_bounty(&mut result, &HashSet::new());
        assert_eq!(result.seven_deuce.len(), 1);
        assert_eq!(result.final_stacks[&1], 200);
    }

    #[test]
    fn premium_bounty_full_funding_conserves_exempts_and_is_idempotent() {
        for (hole, board) in [
            (["As", "Ks"], ["Qs", "Js", "Ts", "2d", "3c"]),
            (["Ah", "Ad"], ["As", "Ac", "2s", "3d", "4c"]),
        ] {
            let (mut hand, mut result) = fixture(hole, board);
            let before: u32 = result.final_stacks.values().sum();
            hand.apply_premium_hand_bounty(&mut result, &HashSet::from([PlayerId(4)]));
            assert_eq!(result.premium_hand.len(), 1);
            assert_eq!(
                result.final_stacks,
                HashMap::from([(1, 180), (2, 0), (3, 80), (4, 100)])
            );
            assert_eq!(
                before + result.bounty_topups.values().sum::<u64>() as u32,
                result.final_stacks.values().sum::<u32>()
            );
            assert_eq!(result.chips_awarded[&1], 40);
            hand.apply_premium_hand_bounty(&mut result, &HashSet::new());
            assert_eq!(result.final_stacks[&1], 180);
            if let EngineEvent::HandFinished { result: queued } = &hand.events[0] {
                assert_eq!(queued.final_stacks, result.final_stacks);
            }
        }
    }
    #[test]
    fn premium_bounty_rejects_board_quads_even_with_better_hole_kicker() {
        let (mut hand, mut result) = fixture(["As", "Kd"], ["2s", "2h", "2d", "2c", "3c"]);
        hand.apply_premium_hand_bounty(&mut result, &HashSet::new());
        assert!(result.premium_hand.is_empty());
        result.board.river = None;
        hand.apply_premium_hand_bounty(&mut result, &HashSet::new());
        assert!(result.premium_hand.is_empty());
    }
    #[test]
    fn premium_bounty_rejects_shared_straight_flush_split_and_disabled() {
        for (hole, board) in [
            (["2h", "3d"], ["As", "Ks", "Qs", "Js", "Ts"]),
            (["As", "Ks"], ["Qs", "Js", "Ts", "2d", "3c"]),
        ] {
            let (mut hand, mut result) = fixture(hole, board);
            result.pots[0].winners.push(PotWinner {
                player_id: PlayerId(2),
                amount_won: Chips(20),
            });
            hand.apply_premium_hand_bounty(&mut result, &HashSet::new());
            assert!(result.premium_hand.is_empty());
            result.pots[0].winners.pop();
            hand.premium_hand_bounty = Chips::ZERO;
            hand.apply_premium_hand_bounty(&mut result, &HashSet::new());
            assert!(result.premium_hand.is_empty());
        }
    }
    #[test]
    fn premium_bounty_fold_around_uses_released_deck_cards() {
        let (mut hand, mut result) = fixture(["Ah", "Ad"], ["As", "Ac", "2s", "3d", "4c"]);
        let hole = result.showdown.remove(0).hole_cards;
        result.folded_hole_cards.insert(1, hole);
        hand.apply_premium_hand_bounty(&mut result, &HashSet::new());
        assert_eq!(result.premium_hand.len(), 1);
        assert_eq!(result.final_stacks[&1], 200);
    }

    #[test]
    fn premium_bounty_two_run_winners_never_pay_each_other() {
        let (mut hand, mut result) = fixture(["As", "Ks"], ["Qs", "Js", "Ts", "2d", "3c"]);
        let other_hole = HoleCards::new(card("Ah"), card("Kh"));
        let other_board = BoardCards {
            flop: Some([card("Qh"), card("Jh"), card("Th")]),
            turn: Some(card("2d")),
            river: Some(card("3c")),
        };
        let mut second_pots = result.pots.clone();
        second_pots[0].winners[0].player_id = PlayerId(2);
        result.run_twice = Some(RunTwiceResult {
            from_street: Street::Preflop,
            second_board: other_board.clone(),
            first_pots: result.pots.clone(),
            second_pots,
            second_showdown: vec![ShowdownEntry {
                player_id: PlayerId(2),
                hole_cards: other_hole,
                rank: crate::eval::rank_hand(&other_hole, &other_board),
                rank_name: "straight_flush".into(),
            }],
        });
        result.final_stacks = HashMap::from([(1, 3), (2, 3), (3, 7), (4, 100)]);
        hand.apply_premium_hand_bounty(&mut result, &HashSet::new());
        assert_eq!(result.premium_hand.len(), 2);
        assert_eq!(
            result.final_stacks,
            HashMap::from([(1, 43), (2, 43), (3, 0), (4, 60)])
        );
        assert!(result.premium_hand.iter().all(|b| b
            .payments
            .iter()
            .all(|p| p.player_id != PlayerId(1) && p.player_id != PlayerId(2))));
    }

    #[test]
    fn premium_bounty_second_run_qualifies_once() {
        let (mut hand, mut result) = fixture(["As", "Ks"], ["Qs", "Js", "Ts", "2d", "3c"]);
        result.run_twice = Some(RunTwiceResult {
            from_street: Street::Preflop,
            second_board: result.board.clone(),
            first_pots: result.pots.clone(),
            second_pots: result.pots.clone(),
            second_showdown: result.showdown.clone(),
        });
        // First run is only a pair; the second run still qualifies.
        result.board.flop = Some([card("Ah"), card("2h"), card("3h")]);
        hand.apply_premium_hand_bounty(&mut result, &HashSet::new());
        assert_eq!(result.premium_hand.len(), 1);
        assert_eq!(result.final_stacks[&1], 200);
    }
}
