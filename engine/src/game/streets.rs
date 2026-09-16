//! Street transitions: the board (dealt from the deck in plaintext mode,
//! injected in blind mode), street closing / advancing, and showdown reveals.
//!
//! Split out of the old 6k-line `game.rs`. These are `impl GameHand` methods on
//! the very same type — no behaviour change, no public-path change.

use super::*;

impl GameHand {
    /// Return the real community cards that were still undealt when this hand
    /// finished.
    ///
    /// Plaintext hands deal directly from `deck`, with no burn cards, so the
    /// next `5 - board.count()` cards are the only valid rabbit-hunt runout.
    /// This accessor is deliberately read-only and does not advance the deck.
    /// Blind hands return an empty vector because their coordinator owns the
    /// unopened deck and the server must never invent a continuation.
    pub fn undealt_board(&self) -> Vec<Card> {
        if self.mode != HandMode::Plaintext || !self.is_done() {
            return Vec::new();
        }
        let needed = 5usize.saturating_sub(self.board.count());
        if needed == 0 {
            return Vec::new();
        }
        let remaining = self.deck.remaining();
        let start = self.deck.cards().len().saturating_sub(remaining);
        self.deck
            .cards()
            .iter()
            .skip(start)
            .take(needed.min(remaining))
            .copied()
            .collect()
    }

    // ---------------------------------------------------------------------------
    // Blind-mode injection API (ADR-066 P1)
    // ---------------------------------------------------------------------------

    /// Write the board cards for `street` and return them as the `new_cards`
    /// payload for [`EngineEvent::StreetRevealed`].
    ///
    /// The single place that writes `board.flop` / `board.turn` /
    /// `board.river` after the hand starts — the plaintext deal
    /// (`close_street_and_advance`) and the blind-mode injection
    /// (`inject_board_for_street`) used to keep two copies of this mapping.
    pub(super) fn set_board_for_street(&mut self, street: Street, cards: &[Card]) -> Vec<Card> {
        debug_assert_eq!(
            cards.len(),
            cards_for_street(street),
            "board write for {street:?} needs exactly {} cards",
            cards_for_street(street)
        );
        match street {
            Street::Flop => self.board.flop = Some([cards[0], cards[1], cards[2]]),
            Street::Turn => self.board.turn = Some(cards[0]),
            Street::River => self.board.river = Some(cards[0]),
            // Preflop has no board; callers never reach this arm.
            Street::Preflop => {}
        }
        cards.to_vec()
    }

    /// Feed externally-supplied, already-verified board cards for the street the
    /// engine has advanced to (ADR-066 §2 step 4). Replaces the `self.deck.deal()`
    /// board path that plaintext mode uses.
    ///
    /// The caller (server) supplies the board cards that all parties threshold-
    /// decrypted and the server verified against the committed ciphertext. This
    /// fills `self.board` for the pending street and emits the same
    /// [`EngineEvent::StreetRevealed`] event a plaintext deal would, carrying the
    /// current betting state.
    ///
    /// `cards.len()` must match the pending street: **3** for the flop, **1** for
    /// the turn, **1** for the river — otherwise [`BlindInjectError::WrongBoardCount`].
    ///
    /// During an all-in run-out the betting round for the just-advanced street may
    /// already be done; after filling the board this method advances to the next
    /// street (via `close_street_and_advance`) so the caller can then inject that
    /// street's board too. When the hand reaches showdown / completion this
    /// returns with `is_done()` true and the snapshot reflects the final state.
    ///
    /// Errors (never panics, never deals a server-visible card):
    /// - [`BlindInjectError::NotBlindMode`] if the hand is plaintext;
    /// - [`BlindInjectError::NoBoardPending`] if no street is awaiting a board;
    /// - [`BlindInjectError::WrongBoardCount`] on a bad card count.
    pub fn inject_board_for_street(
        &mut self,
        cards: &[crate::card::Card],
    ) -> Result<GameSnapshot, BlindInjectError> {
        if self.mode != HandMode::Blind {
            return Err(BlindInjectError::NotBlindMode);
        }
        let street = self
            .pending_board_street
            .ok_or(BlindInjectError::NoBoardPending)?;

        // Preflop never has a board to inject; we never record it as pending.
        if street == Street::Preflop {
            return Err(BlindInjectError::NoBoardPending);
        }
        let expected = cards_for_street(street);
        if cards.len() != expected {
            return Err(BlindInjectError::WrongBoardCount {
                street,
                expected,
                got: cards.len(),
            });
        }

        // The engine is the authoritative ranker; an injected board card that
        // collides with another card it already holds (an existing board card,
        // an already-`Revealed` hole card, or another card in this same injected
        // set) would silently mis-rank in release builds (`rank_hand` only
        // `debug_assert!`s uniqueness). Reject before storing anything.
        self.check_injected_cards_unique(cards)?;

        // Fill the board for this street and emit StreetRevealed with the cards
        // plus the live betting state (mirrors the plaintext deal event).
        let new_cards = self.set_board_for_street(street, cards);

        let (current_bet, min_raise_to, next_actor_seat) = self.new_street_betting_state();
        self.events.push(EngineEvent::StreetRevealed {
            street,
            new_cards,
            current_bet,
            min_raise_to,
            next_actor_seat,
        });

        // Board for this street is now known.
        self.pending_board_street = None;

        // All-in run-out: if the betting round for this street was immediately
        // done (everyone all-in), advance to the next street now that the board
        // is recorded — re-arming `pending_board_street` for the next inject.
        if self
            .betting_round
            .as_ref()
            .map(|r| r.is_done())
            .unwrap_or(false)
        {
            return self
                .close_street_and_advance()
                .map_err(|_| BlindInjectError::NoBoardPending);
        }

        Ok(self.snapshot())
    }

    /// Feed an externally-verified showdown reveal for a **non-folded** player
    /// (ADR-066 §2 step 5). Only injected reveals are used by `finish_blind` /
    /// `rank_hand` in blind mode.
    ///
    /// The caller (server) supplies the hole-card plaintext only after verifying
    /// it against the player's pre-committed ciphertext (`verify_and_open`). This
    /// transitions the seat from [`HoleSlot::Opaque`] to [`HoleSlot::Revealed`].
    ///
    /// Errors (never panics, never invents cards):
    /// - [`BlindInjectError::NotBlindMode`] if the hand is plaintext;
    /// - [`BlindInjectError::NotAtShowdown`] if the hand is not done (reveals are
    ///   only consumed at showdown);
    /// - [`BlindInjectError::UnknownPlayer`] for an unknown player id;
    /// - [`BlindInjectError::PlayerFolded`] for a folded seat — folded cards stay
    ///   sealed forever (ADR-066 §2 step 6).
    pub fn inject_showdown_reveal(
        &mut self,
        player_id: PlayerId,
        hole_cards: HoleCards,
    ) -> Result<(), BlindInjectError> {
        if self.mode != HandMode::Blind {
            return Err(BlindInjectError::NotBlindMode);
        }
        // Reveals are a showdown concept — only meaningful once betting is over.
        if self.phase != Phase::Done {
            return Err(BlindInjectError::NotAtShowdown);
        }
        // Validate target seat WITHOUT taking a mutable borrow yet — the
        // uniqueness check below needs an immutable borrow of all seats/board.
        match self.seats.iter().find(|s| s.player_id == player_id) {
            None => return Err(BlindInjectError::UnknownPlayer(player_id)),
            Some(seat) if seat.folded => {
                // A folded player never reveals — their cards stay sealed forever.
                return Err(BlindInjectError::PlayerFolded(player_id));
            }
            // A verified reveal is immutable showdown evidence: reject a second
            // reveal with DIFFERENT cards, but accept an identical re-injection
            // idempotently (U32, dual-AI OSS review).
            Some(seat) => {
                if let HoleSlot::Revealed(existing) = seat.hole {
                    if existing == hole_cards {
                        return Ok(());
                    }
                    return Err(BlindInjectError::AlreadyRevealed(player_id));
                }
            }
        }

        // Uniqueness: a revealed hole card must not collide with any board card,
        // any already-`Revealed` hole card, or its own pair-mate. The engine
        // ranks from these reveals and must never store a deck with a repeated
        // card (ADR-066 §2 — the authoritative ranker does not trust injected
        // cards; `rank_hand` only `debug_assert!`s uniqueness in debug builds).
        // The revealing seat is still `Opaque` here, so its own (about-to-be)
        // cards are not yet in the known set — no false self-collision.
        self.check_injected_cards_unique(&hole_cards.as_array())?;

        let seat = self
            .seats
            .iter_mut()
            .find(|s| s.player_id == player_id)
            .ok_or(BlindInjectError::UnknownPlayer(player_id))?;
        seat.hole = HoleSlot::Revealed(hole_cards);
        Ok(())
    }

    /// Validate that none of `injected` collide with a card the engine already
    /// holds (any board card, any already-`Revealed` hole card) and that the
    /// injected set has no internal duplicate. Returns the first colliding card
    /// as [`BlindInjectError::DuplicateCard`].
    ///
    /// The engine is the authoritative ranker (ADR-066 §2). `rank_hand` only
    /// `debug_assert!`s card uniqueness, which is compiled out in release — so an
    /// injected duplicate would silently mis-rank in production. This is the
    /// release-safe guard that keeps that from happening.
    pub(super) fn check_injected_cards_unique(
        &self,
        injected: &[crate::card::Card],
    ) -> Result<(), BlindInjectError> {
        // Known set: every board card + every already-Revealed hole card.
        let mut known: std::collections::HashSet<crate::card::Card> =
            self.board.all_cards().into_iter().collect();
        for seat in &self.seats {
            if let HoleSlot::Revealed(h) = seat.hole {
                known.insert(h.card1);
                known.insert(h.card2);
            }
        }
        // Reject collisions against the known set AND within the injected set
        // itself (a flop with two equal cards, or a reveal pair of identical
        // cards). Insert each injected card into the same set as we go so an
        // intra-set duplicate is caught too.
        for &card in injected {
            if !known.insert(card) {
                return Err(BlindInjectError::DuplicateCard { card });
            }
        }
        Ok(())
    }

    /// The street currently awaiting a board injection in blind mode, if any.
    /// `None` in plaintext mode or when no board is pending.
    pub fn pending_board_street(&self) -> Option<Street> {
        self.pending_board_street
    }

    /// Compute the new-street betting-state triple
    /// `(current_bet, min_raise_to, next_actor_seat)` from the active round.
    /// Shared by the plaintext StreetRevealed patch and the blind
    /// `inject_board_for_street` event. `min_raise_to`/`next_actor_seat` are
    /// `None` when the round is already done (all-in runout).
    pub(super) fn new_street_betting_state(&self) -> (u64, Option<u64>, Option<u8>) {
        let current_bet = self
            .betting_round
            .as_ref()
            .map(|r| r.current_bet().0 as u64)
            .unwrap_or(0);
        let min_raise_to = self.betting_round.as_ref().and_then(|r| {
            if r.is_done() {
                None
            } else {
                Some(r.min_raise_to().0 as u64)
            }
        });
        let next_actor_seat = self.betting_round.as_ref().and_then(|r| {
            if r.is_done() {
                None
            } else {
                r.current_player().and_then(|pid| {
                    self.seats
                        .iter()
                        .find(|s| s.player_id == pid)
                        .map(|s| s.seat)
                })
            }
        });
        (current_bet, min_raise_to, next_actor_seat)
    }

    // ---------------------------------------------------------------------------
    // Test-only helpers
    // ---------------------------------------------------------------------------

    /// Override the hole cards for a specific player (by `PlayerId`).
    ///
    /// Intended exclusively for integration tests that need to control which
    /// starting hand a player holds after `start()` has already dealt random cards.
    /// Gated behind the `test-helpers` cargo feature so production builds cannot
    /// call this method.
    #[cfg(any(test, feature = "test-helpers"))]
    pub fn override_hole_cards_for_test(&mut self, player_id: PlayerId, cards: HoleCards) {
        let mut found = false;
        for seat in &mut self.seats {
            if seat.player_id == player_id {
                seat.hole = HoleSlot::Plain(cards);
                found = true;
                break;
            }
        }
        if !found {
            return;
        }
        // Reserve the pinned cards out of the remaining deck so a later board
        // deal cannot collide with them. Without this, the board (still dealt
        // from the unshuffled remainder) can repeat a pinned hole card, and
        // `rank_hand` then receives overlapping cards — a real-deck impossibility
        // that its debug_assert (see eval.rs) correctly rejects. Test-only
        // helper, so rebuilding the deck from its remaining cards is fine.
        let remaining: Vec<crate::card::Card> = self
            .deck
            .cards()
            .iter()
            .copied()
            .filter(|c| *c != cards.card1 && *c != cards.card2)
            .collect();
        self.deck = Deck::from_cards(remaining);
    }

    // ---------------------------------------------------------------------------
    // Private helpers
    // ---------------------------------------------------------------------------

    pub(super) fn current_street(&self) -> Street {
        match &self.phase {
            Phase::Betting(s) => *s,
            // A finished hand reports the street it ended on (U34), not a Preflop
            // sentinel. `finished_street` is set at every Done transition; it is
            // only `None` for a hand that never started.
            Phase::Done => self.finished_street.unwrap_or(Street::Preflop),
            Phase::NotStarted => Street::Preflop,
        }
    }

    /// Record the final street and transition the hand to `Done` (U34). Capturing
    /// here (while `self.phase` is still `Betting(s)`) is what lets a completed
    /// hand report its real final street.
    pub(super) fn set_done(&mut self) {
        self.finished_street = Some(self.current_street());
        self.phase = Phase::Done;
    }

    /// Sync seat stacks from the betting round's current state.
    pub(super) fn sync_stacks_from_round(&mut self) {
        if let Some(round) = &self.betting_round {
            for seat in &mut self.seats {
                // The round tracks stacks after contributions.
                // We need the round's player stacks (which account for blinds).
                let round_stack = round
                    .player_states()
                    .iter()
                    .find(|ps| ps.player_id == seat.player_id)
                    .map(|ps| ps.stack);
                if let Some(stack) = round_stack {
                    seat.stack = stack;
                }
                seat.all_in = round.player_all_in(seat.player_id);
                seat.folded = round.player_folded(seat.player_id);
            }
        }
    }

    /// Close the current betting round and advance to the next street.
    pub(super) fn close_street_and_advance(&mut self) -> Result<GameSnapshot, ActionError> {
        // Absorb the round's pots into cumulative state.
        if let Some(round) = self.betting_round.take() {
            let new_pots = round.side_pots();
            let round_total = new_pots.iter().map(|p| p.amount.0).sum::<u32>();
            self.completed_pot.0 += round_total;
            self.accumulate_pots(new_pots);
        }

        // Check if hand is over (only one non-folded player, or we've exhausted streets).
        let active_count = self.seats.iter().filter(|s| !s.folded).count();
        if active_count <= 1 {
            self.set_done();
            return Ok(self.snapshot());
        }

        // Advance to next street.
        let next_street = match self.current_street() {
            Street::Preflop => Some(Street::Flop),
            Street::Flop => Some(Street::Turn),
            Street::Turn => Some(Street::River),
            Street::River => None,
        };

        match next_street {
            None => {
                self.set_done();
            }
            Some(street) => {
                // Deal community cards and emit StreetRevealed.
                //
                // Blind mode (ADR-066 P1): the engine does NOT deal the board
                // from a deck. It advances to the next street and builds the
                // betting round (board-independent), but the board cards arrive
                // later via `inject_board_for_street`, which emits StreetRevealed
                // once they are verified. We record the pending street and skip
                // the deck deal + StreetRevealed emission here.
                match self.mode {
                    HandMode::Plaintext => {
                        let count = cards_for_street(street);
                        if count > 0 {
                            let mut dealt: Vec<Card> = Vec::with_capacity(count);
                            for _ in 0..count {
                                dealt
                                    .push(self.deck.deal().map_err(|_| ActionError::HandFinished)?);
                            }
                            let new_cards = self.set_board_for_street(street, &dealt);
                            // Betting-state fields (current_bet, min_raise_to, next_actor_seat)
                            // are placeholder zeros/None here; they are patched to the real
                            // values immediately after BettingRound construction below.
                            self.events.push(EngineEvent::StreetRevealed {
                                street,
                                new_cards,
                                current_bet: 0,
                                min_raise_to: None,
                                next_actor_seat: None,
                            });
                        }
                    }
                    HandMode::Blind => {
                        // Defer the board: it will be filled by
                        // `inject_board_for_street(street, ...)`. Record which
                        // street is awaiting its verified cards.
                        if street != Street::Preflop {
                            self.pending_board_street = Some(street);
                        }
                    }
                }

                self.phase = Phase::Betting(street);

                // Determine first actor post-flop: first non-folded/all-in seat
                // strictly LEFT OF the button (WSOP/TDA standard).
                //
                // For 3+ players this is the SB seat (dealer+1).  In heads-up
                // (n=2) the dealer IS the SB, so we must walk to dealer+1 (the
                // BB) — the non-button player acts first post-flop in HU.
                // Starting the walk at `dealer + 1` unifies both cases.
                let n = self.seats.len();
                let dealer_idx = self.dealer_idx % n;
                let first_seat_idx = self.first_active_after((dealer_idx + 1) % n);
                let first_actor_pid = self.seats[first_seat_idx].player_id;

                // Build the round with current stacks (post-previous-round).
                // Filter out folded seats so they never re-enter the actor sequence.
                let players_for_round: Vec<(PlayerId, Chips)> = self
                    .seats
                    .iter()
                    .filter(|s| !s.folded)
                    .map(|s| (s.player_id, s.stack))
                    .collect();

                // Resolve first_actor as an index into the filtered vec.
                let first_actor = players_for_round
                    .iter()
                    .position(|(pid, _)| *pid == first_actor_pid)
                    .unwrap_or(0);

                let round =
                    BettingRound::new(players_for_round, first_actor, Chips::ZERO, self.big_blind);
                self.betting_round = Some(round);

                // Read post-construction betting state for the StreetRevealed event.
                // If all players are all-in, the round is immediately done and there
                // is no next actor — `next_actor_seat` and `min_raise_to` will be None.
                let (new_street_current_bet, new_street_min_raise_to, new_street_next_actor_seat) =
                    self.new_street_betting_state();

                // Patch the StreetRevealed event that was pushed above with the
                // now-known betting state fields.  We pushed it before constructing
                // the round so the card data is already there; replace the last
                // event (which must be the StreetRevealed we just pushed).
                if let Some(EngineEvent::StreetRevealed {
                    current_bet,
                    min_raise_to,
                    next_actor_seat,
                    ..
                }) = self.events.last_mut()
                {
                    *current_bet = new_street_current_bet;
                    *min_raise_to = new_street_min_raise_to;
                    *next_actor_seat = new_street_next_actor_seat;
                }

                // If all remaining players are already all-in, skip forward.
                //
                // Blind mode (ADR-066 §2 step 7 — run-out reveal phase): do NOT
                // auto-recurse through the runout. Each street's board must be
                // injected separately, so we pause here with the round done and a
                // pending board. `inject_board_for_street` re-drives the advance
                // after it fills this street's board (it calls
                // `close_street_and_advance` if the just-revealed round is done).
                if self.mode == HandMode::Plaintext
                    && self
                        .betting_round
                        .as_ref()
                        .map(|r| r.is_done())
                        .unwrap_or(false)
                {
                    return self.close_street_and_advance();
                }
            }
        }

        Ok(self.snapshot())
    }

    /// Find the first active (non-folded, non-all-in) seat starting from `start`.
    pub(super) fn first_active_after(&self, start: usize) -> usize {
        let n = self.seats.len();
        for i in 0..n {
            let idx = (start + i) % n;
            if !self.seats[idx].folded && !self.seats[idx].all_in {
                return idx;
            }
        }
        start % n
    }
}
