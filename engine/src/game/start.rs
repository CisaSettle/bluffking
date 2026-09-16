//! Hand construction and `start()`: seating, blinds / straddle, the initial
//! deal and the preflop betting round.
//!
//! Split out of the old 6k-line `game.rs`. These are `impl GameHand` methods on
//! the very same type — no behaviour change, no public-path change.

use super::*;

impl GameHand {
    /// Production constructor using OS-random seed.
    pub fn new(
        players: Vec<(PlayerId, Chips, u8)>,
        dealer_idx: usize,
        big_blind: Chips,
        small_blind: Chips,
    ) -> Self {
        let rng = PokerRng::from_os();
        Self::new_with_rng(players, dealer_idx, big_blind, small_blind, rng)
    }

    /// Test constructor with injected deterministic RNG.
    pub fn new_with_rng(
        players: Vec<(PlayerId, Chips, u8)>,
        dealer_idx: usize,
        big_blind: Chips,
        small_blind: Chips,
        mut rng: PokerRng,
    ) -> Self {
        let deck = Deck::new(&mut rng);
        let seed = rng.seed();
        Self::new_with_deck(players, dealer_idx, big_blind, small_blind, deck, seed)
    }

    /// Construct a hand from a deck produced by the dealing abstraction.
    ///
    /// This is the constructor the `DealingProvider` path uses: the deck order
    /// is decided outside the engine (legacy server shuffle, or a verified
    /// Mental Poker protocol run) and handed in via [`Deck::from_cards`]. The
    /// engine no longer depends on a server-side RNG to deal a hand.
    ///
    /// `deck_seed` is recorded on the `HandResult` for hand-history
    /// reproducibility; for Mental Poker it is a transcript-derived value
    /// rather than an RNG seed.
    pub fn new_with_deck(
        players: Vec<(PlayerId, Chips, u8)>,
        dealer_idx: usize,
        big_blind: Chips,
        small_blind: Chips,
        deck: Deck,
        deck_seed: DeckSeed,
    ) -> Self {
        let seed = deck_seed;
        let seats = players
            .into_iter()
            .map(|(player_id, stack, seat)| HandSeat {
                player_id,
                starting_stack: stack,
                stack,
                hole: HoleSlot::Empty,
                folded: false,
                all_in: false,
                seat,
            })
            .collect();
        Self {
            seats,
            dealer_idx,
            big_blind,
            small_blind,
            forced_straddle: None,
            preflop_bring_in: big_blind,
            mode: HandMode::Plaintext,
            deck,
            deck_seed: seed,
            board: BoardCards::empty(),
            phase: Phase::NotStarted,
            betting_round: None,
            completed_pot: Chips::ZERO,
            cumulative_side_pots: vec![],
            actions: vec![],
            action_seq: 0,
            last_action: HashMap::new(),
            events: vec![],
            pending_board_street: None,
            finished_street: None,
        }
    }

    /// Blind-mode constructor (ADR-066 P1).
    ///
    /// Takes **no** `Deck`, shuffles nothing, and deals **no** hole cards. Seats
    /// begin with [`HoleSlot::Opaque`] — the engine knows each seat has two
    /// cards but not their value. Blinds, positions, and stacks are set up
    /// exactly as the normal constructor; only the card source differs.
    ///
    /// `deck_seed` is a transcript-derived identifier recorded on the
    /// `HandResult` for hand-history reproducibility (it is **not** an RNG seed
    /// and reveals nothing about hole values). [`DeckSeed`] is a `[u8; 32]`, so
    /// it is fine to pass a default all-zero seed when no transcript identifier
    /// is available.
    ///
    /// After construction:
    /// - [`GameHand::start`] posts blinds and begins preflop betting but deals
    ///   no hole cards and emits no `HoleCardsDealt` event at all (no plaintext
    ///   value, and no redacted variant either — blind start emits nothing for
    ///   the deal);
    /// - board cards are fed per street via [`GameHand::inject_board_for_street`];
    /// - showdown reveals are fed via [`GameHand::inject_showdown_reveal`].
    pub fn new_blind(
        players: Vec<(PlayerId, Chips, u8)>,
        dealer_idx: usize,
        big_blind: Chips,
        small_blind: Chips,
        deck_seed: DeckSeed,
    ) -> Self {
        let seats = players
            .into_iter()
            .map(|(player_id, stack, seat)| HandSeat {
                player_id,
                starting_stack: stack,
                stack,
                // Opaque, not Empty: a blind seat is dealt two cards whose value
                // the engine does not know — the type-level blind invariant.
                hole: HoleSlot::Opaque,
                folded: false,
                all_in: false,
                seat,
            })
            .collect();
        Self {
            seats,
            dealer_idx,
            big_blind,
            small_blind,
            forced_straddle: None,
            preflop_bring_in: big_blind,
            mode: HandMode::Blind,
            // Empty deck: blind mode never deals from it. Any accidental
            // `self.deck.deal()` would error (HandFinished) rather than leak a
            // plaintext card — but the blind paths never call it.
            deck: Deck::from_cards(Vec::new()),
            deck_seed,
            board: BoardCards::empty(),
            phase: Phase::NotStarted,
            betting_round: None,
            completed_pot: Chips::ZERO,
            cumulative_side_pots: vec![],
            actions: vec![],
            action_seq: 0,
            last_action: HashMap::new(),
            events: vec![],
            pending_board_street: None,
            finished_street: None,
        }
    }

    /// Enable a live, forced UTG straddle for this hand.
    ///
    /// A zero amount disables the option. Heads-up hands never post a
    /// straddle, even when the table-level setting remains enabled.
    pub fn with_forced_straddle(mut self, amount: Chips) -> Self {
        self.forced_straddle = (amount.0 > 0).then_some(amount);
        self
    }

    /// Start the hand: post blinds, deal hole cards, begin preflop betting.
    pub fn start(&mut self) -> Result<GameSnapshot, ActionError> {
        if self.phase != Phase::NotStarted {
            return Err(ActionError::HandFinished);
        }
        // Every public chip/pot amount is u32. Reject an unrepresentable table
        // before blinds or pot math can overflow in debug or wrap in release.
        if self
            .seats
            .iter()
            .map(|seat| u64::from(seat.starting_stack.0))
            .sum::<u64>()
            > u64::from(u32::MAX)
        {
            return Err(ActionError::InvalidAction);
        }
        let n = self.seats.len();
        if n < 2 {
            return Err(ActionError::NotInHand);
        }

        // Determine blind positions.
        let (sb_idx, bb_idx) = blind_positions(self.dealer_idx, n);
        let sb_id = self.seats[sb_idx].player_id;
        let bb_id = self.seats[bb_idx].player_id;
        let straddle_idx = self
            .forced_straddle
            .filter(|_| n >= 3)
            .map(|_| (bb_idx + 1) % n);
        let straddle_id = straddle_idx.map(|idx| self.seats[idx].player_id);

        // Emit HandStarted as the FIRST event (ADR-024 §5.2, ADR-025 §4).
        // Must be emitted before any ActionApplied { Blind, .. } events.
        {
            // Bound the dealer index the same way `blind_positions` already
            // normalizes (`% n`) so an out-of-range `dealer_idx` wraps instead
            // of panicking with index-out-of-bounds (defensive hardening,
            // audit 2026-06-03).
            let dealer_seat = self.seats[self.dealer_idx % n].seat;
            let sb_seat = self.seats[sb_idx].seat;
            let bb_seat = self.seats[bb_idx].seat;
            self.events.push(EngineEvent::HandStarted {
                dealer_seat,
                sb_seat,
                bb_seat,
                big_blind: self.big_blind.0 as u64,
                small_blind: self.small_blind.0 as u64,
                straddle_seat: straddle_idx.map(|idx| self.seats[idx].seat),
                straddle_amount: self.forced_straddle.filter(|_| n >= 3).map(|v| v.0 as u64),
                deck_seed: self.deck_seed,
            });
        }

        // Record blind action metadata (using starting stacks).
        let sb_stack_start = self.seats[sb_idx].stack;
        let bb_stack_start = self.seats[bb_idx].stack;
        let sb_blind_amount = self.small_blind.0.min(sb_stack_start.0);
        let bb_blind_amount = self.big_blind.0.min(bb_stack_start.0);

        // Record SB action.
        // ADR-079 (F2): capture the blind seqs so the emitted ActionApplied events
        // carry the same engine-authoritative seq as the recorded ActionRecord
        // (SB = 0, BB = 1; first voluntary action = 2).
        let sb_action_seq = self.action_seq;
        self.actions.push(ActionRecord {
            seq: self.action_seq,
            street: Street::Preflop,
            player_id: sb_id,
            action: PlayerAction::Blind {
                kind: BlindKind::Small,
                amount: Chips(sb_blind_amount),
            },
            amount: Chips(sb_blind_amount),
            stack_before: sb_stack_start,
            stack_after: Chips(sb_stack_start.0 - sb_blind_amount),
            pot_before: Chips::ZERO,
            pot_after: Chips(sb_blind_amount),
        });
        self.last_action.insert(
            sb_id.inner(),
            PlayerAction::Blind {
                kind: BlindKind::Small,
                amount: Chips(sb_blind_amount),
            },
        );
        self.action_seq = self.action_seq.saturating_add(1); // SB seq=0 before BB seq=1; saturating (U33)

        // Record BB action.
        let bb_action_seq = self.action_seq;
        self.actions.push(ActionRecord {
            seq: self.action_seq,
            street: Street::Preflop,
            player_id: bb_id,
            action: PlayerAction::Blind {
                kind: BlindKind::Big,
                amount: Chips(bb_blind_amount),
            },
            amount: Chips(bb_blind_amount),
            stack_before: bb_stack_start,
            stack_after: Chips(bb_stack_start.0 - bb_blind_amount),
            pot_before: Chips(sb_blind_amount),
            pot_after: Chips(sb_blind_amount + bb_blind_amount),
        });
        self.last_action.insert(
            bb_id.inner(),
            PlayerAction::Blind {
                kind: BlindKind::Big,
                amount: Chips(bb_blind_amount),
            },
        );
        self.action_seq = self.action_seq.saturating_add(1); // saturating (U33)

        // Record the forced UTG straddle after the two standard blinds. A
        // short stack posts what it has; that partial post does not create a
        // larger nominal bring-in (resolved below), but action still begins to
        // the straddler's left.
        let straddle_record = straddle_idx.zip(straddle_id).and_then(|(idx, id)| {
            let requested = self.forced_straddle?;
            let stack_start = self.seats[idx].stack;
            let posted = requested.0.min(stack_start.0);
            let seq = self.action_seq;
            let pot_before = sb_blind_amount.saturating_add(bb_blind_amount);
            self.actions.push(ActionRecord {
                seq,
                street: Street::Preflop,
                player_id: id,
                action: PlayerAction::Blind {
                    kind: BlindKind::Straddle,
                    amount: Chips(posted),
                },
                amount: Chips(posted),
                stack_before: stack_start,
                stack_after: Chips(stack_start.0 - posted),
                pot_before: Chips(pot_before),
                pot_after: Chips(pot_before.saturating_add(posted)),
            });
            self.last_action.insert(
                id.inner(),
                PlayerAction::Blind {
                    kind: BlindKind::Straddle,
                    amount: Chips(posted),
                },
            );
            self.action_seq = self.action_seq.saturating_add(1);
            Some((idx, id, posted, requested, seq))
        });

        // Deal hole cards (two passes over all active seats).
        //
        // Blind mode (ADR-066 P1): deal NOTHING from the deck. Seats keep their
        // `HoleSlot::Opaque` holding from `new_blind` — the engine never holds a
        // plaintext hole value before a verified showdown reveal. (Skipping this
        // block is what makes "no plaintext before showdown" structural: there
        // is no blind code path that writes `Plain`.)
        if self.mode == HandMode::Plaintext {
            let n_seats = self.seats.len();
            let mut first_cards: Vec<crate::card::Card> = Vec::with_capacity(n_seats);
            let mut second_cards: Vec<crate::card::Card> = Vec::with_capacity(n_seats);
            for _ in 0..n_seats {
                first_cards.push(self.deck.deal().map_err(|_| ActionError::HandFinished)?);
            }
            for _ in 0..n_seats {
                second_cards.push(self.deck.deal().map_err(|_| ActionError::HandFinished)?);
            }
            for (i, seat) in self.seats.iter_mut().enumerate() {
                seat.hole = HoleSlot::Plain(HoleCards::new(first_cards[i], second_cards[i]));
            }
        }

        // Build the preflop betting round BEFORE emitting blind events so that
        // the post-blind current_bet / min_raise_to / next_actor are accurate.
        // Pass FULL stacks to the round; new_preflop handles blind deductions internally.
        let preflop_players: Vec<(PlayerId, Chips)> =
            self.seats.iter().map(|s| (s.player_id, s.stack)).collect();

        // UTG acts first without a straddle. With a straddle, action opens one
        // seat to the straddler's left.
        let first_actor_in_round = straddle_record
            .map(|(idx, _, _, _, _)| (idx + 1) % n)
            .unwrap_or((bb_idx + 1) % n);

        let mut blinds = vec![
            (sb_id, Chips(sb_blind_amount)),
            (bb_id, Chips(bb_blind_amount)),
        ];
        if let Some((_, id, posted, _, _)) = straddle_record {
            blinds.push((id, Chips(posted)));
        }

        // A fully funded live straddle is the opening wager and therefore the
        // minimum raise increment (2 BB straddle → first legal raise to 4 BB).
        // A short all-in straddle remains a forced post but does not inflate the
        // nominal bring-in above the big blind.
        let preflop_bring_in = match straddle_record {
            Some((_, _, posted, requested, _)) if posted >= requested.0 => requested,
            _ => self.big_blind,
        };
        // Remember it: `current_street_action_facts` must project preflop raise
        // depth from the SAME baseline the betting round enforces (BUG-175).
        self.preflop_bring_in = preflop_bring_in;

        let round = BettingRound::new_preflop(
            preflop_players,
            first_actor_in_round,
            &blinds,
            preflop_bring_in,
        );

        self.phase = Phase::Betting(Street::Preflop);
        self.betting_round = Some(round);

        // Sync seat stacks from round (blinds have been applied inside round).
        self.sync_stacks_from_round();

        // Now that the round is constructed, read post-forced-bet state for the
        // Blind ActionApplied events. Every forced post carries the SAME final
        // state after SB, BB, and the optional straddle are posted.
        let post_blind_current_bet = self
            .betting_round
            .as_ref()
            .map(|r| r.current_bet().0 as u64)
            .unwrap_or(0);
        let post_blind_min_raise_to = self
            .betting_round
            .as_ref()
            .map(|r| r.min_raise_to().0 as u64);
        let post_blind_next_actor_seat = self.betting_round.as_ref().and_then(|r| {
            r.current_player().and_then(|pid| {
                self.seats
                    .iter()
                    .find(|s| s.player_id == pid)
                    .map(|s| s.seat)
            })
        });

        // Emit SB blind event.
        let sb_seat = self.seats[sb_idx].seat;
        self.events.push(EngineEvent::ActionApplied {
            seat: sb_seat,
            action: PlayerAction::Blind {
                kind: BlindKind::Small,
                amount: Chips(sb_blind_amount),
            },
            contributed: sb_blind_amount as u64,
            current_bet: post_blind_current_bet,
            min_raise_to: post_blind_min_raise_to,
            next_actor_seat: post_blind_next_actor_seat,
            action_seq: sb_action_seq,
        });

        // Emit BB blind event.
        let bb_seat = self.seats[bb_idx].seat;
        self.events.push(EngineEvent::ActionApplied {
            seat: bb_seat,
            action: PlayerAction::Blind {
                kind: BlindKind::Big,
                amount: Chips(bb_blind_amount),
            },
            contributed: bb_blind_amount as u64,
            current_bet: post_blind_current_bet,
            min_raise_to: post_blind_min_raise_to,
            next_actor_seat: post_blind_next_actor_seat,
            action_seq: bb_action_seq,
        });

        // The forced-post event stream remains chronological: SB (seq 0), BB
        // (seq 1), then the optional live straddle (seq 2).
        if let Some((idx, _, posted, _, seq)) = straddle_record {
            self.events.push(EngineEvent::ActionApplied {
                seat: self.seats[idx].seat,
                action: PlayerAction::Blind {
                    kind: BlindKind::Straddle,
                    amount: Chips(posted),
                },
                contributed: posted as u64,
                current_bet: post_blind_current_bet,
                min_raise_to: post_blind_min_raise_to,
                next_actor_seat: post_blind_next_actor_seat,
                action_seq: seq,
            });
        }

        // Emit PotUpdated after all forced posts are made.
        // Blinds are never all-in for normal stacks; compute accurately anyway.
        let total_blind_pot = sb_blind_amount as u64
            + bb_blind_amount as u64
            + straddle_record
                .map(|(_, _, posted, _, _)| posted as u64)
                .unwrap_or(0);
        let has_all_in_after_blinds = self.seats.iter().any(|s| s.all_in);
        self.events.push(EngineEvent::PotUpdated {
            pot: total_blind_pot,
            side_pots: vec![],
            has_all_in: has_all_in_after_blinds,
        });

        // Emit HoleCardsDealt events for each seat in seat order.
        //
        // Plaintext mode: cards are unredacted here — server is responsible for
        // per-client filtering.
        //
        // Blind mode (ADR-066 §2 step 3): emit NOTHING. The engine never
        // originates or broadcasts a plaintext hole value, and we deliberately
        // do NOT introduce a redacted event variant (the contract allows "or
        // none") so the not-yet-blind-aware server needs no changes. A client
        // learns its own cards out-of-band via the threshold-decrypt protocol.
        if self.mode == HandMode::Plaintext {
            for seat in &self.seats {
                if let Some(hole) = seat.hole_cards() {
                    self.events.push(EngineEvent::HoleCardsDealt {
                        seat: seat.seat,
                        cards: hole.as_array(),
                    });
                }
            }
        }

        if self
            .betting_round
            .as_ref()
            .map(|r| r.is_done())
            .unwrap_or(false)
        {
            return self.close_street_and_advance();
        }

        Ok(self.snapshot())
    }
}
