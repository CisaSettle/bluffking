//! The dealing-protocol state machine.
//!
//! [`ProtocolState`] is the single source of truth for *what events are legal
//! when*. Both the transcript **builder** and the **verifier** drive the exact
//! same `apply` logic — the builder to compute `state_hash_before` /
//! `state_hash_after`, the verifier to re-derive and check them. Because the
//! machinery is shared, a transcript that the builder produced will always
//! replay cleanly, and any structural tampering diverges the state hash or
//! trips an explicit [`StateError`].

use crate::card_id::{is_valid_card_id, DECK_SIZE};
use crate::crypto::{card_commit, deck_hash, EncCard, Salt};
use crate::events::*;
use crate::hash::{canonical_json, ds_hash, hex_hash, parse_hash, Hash, ZERO_HASH};

/// Hash of the canonical plaintext starting deck `[0, 1, …, 51]` as a
/// **wire deck** (52 single-byte card-id parts).
///
/// Used by the interactive dealing path (ADR-041 §5.1 "Round-0 input rule"):
/// `wire_deck_hash([0,1,…,51])`.
pub fn canonical_wire_deck_hash() -> Hash {
    let identity: Vec<u8> = (0u8..DECK_SIZE as u8).collect();
    let parts: Vec<&[u8]> = identity.iter().map(std::slice::from_ref).collect();
    ds_hash("mp:deck-hash:v1", &parts)
}
use serde::Serialize;
use serde_json::Value;
use thiserror::Error;

/// All-zero salt used for the public, agreed-upon canonical starting deck.
pub const ZERO_SALT: Salt = [0u8; 32];

/// Phases of the dealing protocol.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Phase {
    /// Before `hand_init`.
    PreInit,
    /// `hand_init` seen; parties registering keys.
    KeyReg,
    /// Keys registered; round-robin shuffling.
    Shuffle,
    /// Final deck committed; parties acknowledging.
    Committed,
    /// Deck acknowledged by all; hole cards opening.
    Dealing,
    /// Flop revealed.
    Flop,
    /// Turn revealed.
    Turn,
    /// River revealed.
    River,
    /// Terminal — hand complete.
    Complete,
    /// Terminal — hand aborted.
    Aborted,
}

impl Phase {
    fn is_terminal(self) -> bool {
        matches!(self, Phase::Complete | Phase::Aborted)
    }
}

/// A structural / state-machine violation found while applying an event.
#[derive(Debug, Clone, PartialEq, Eq, Error)]
pub enum StateError {
    /// Event is not legal in the current phase.
    #[error("event '{event}' illegal in phase {phase:?}")]
    WrongPhase {
        /// The offending event type.
        event: String,
        /// The phase the machine was in.
        phase: Phase,
    },
    /// Payload could not be parsed into its typed form.
    #[error("malformed payload: {0}")]
    MalformedPayload(String),
    /// `hand_init` declared an invalid seat set.
    #[error("invalid player set: {0}")]
    InvalidPlayerSet(String),
    /// A party registered a key twice (or an unknown party).
    #[error("bad key registration: {0}")]
    BadKeyRegistration(String),
    /// Shuffle round number or shuffling party is wrong.
    #[error("bad shuffle: {0}")]
    BadShuffle(String),
    /// The input deck of a shuffle does not match the prior output deck.
    #[error("deck hash discontinuity: {0}")]
    DeckDiscontinuity(String),
    /// The committed final deck does not match the last shuffle output, or its
    /// hash does not match its contents.
    #[error("final deck mismatch: {0}")]
    FinalDeckMismatch(String),
    /// mp-phase4 re-audit r1 (HIGH): on the real re-encryption path the committed
    /// ciphertext deck (`final_deck_committed.deck_ct`) is absent, the wrong
    /// length, undecodable, or its `ec::deck_hash` v2 does not equal the
    /// verified-shuffle `final_deck_hash` the parties signed. Binding it here, at
    /// apply time, rejects a coordinator-substituted ciphertext deck BEFORE any
    /// threshold open is anchored to it (without this bind the offline verifier's
    /// F3 anchor would be rooted in an unverified, attacker-chosen deck).
    #[error("unbound ciphertext deck: {0}")]
    UnboundCiphertextDeck(String),
    /// A party acknowledged a final deck hash different from the committed one
    /// — evidence the coordinator forked the deck.
    #[error("forked final deck: party acked {acked}, committed {committed}")]
    ForkedFinalDeck {
        /// The hash the party acked.
        acked: String,
        /// The hash actually committed.
        committed: String,
    },
    /// A duplicate acknowledgement.
    #[error("duplicate ack from {0}")]
    DuplicateAck(String),
    /// A card open occurred before the deck was committed and acknowledged.
    #[error("open before deck ready: {0}")]
    OpenBeforeDeckReady(String),
    /// A deck index is out of range or not the expected one.
    #[error("bad deck index: {0}")]
    BadDeckIndex(String),
    /// A deck index was opened twice.
    #[error("deck index {0} opened twice")]
    IndexReopened(u32),
    /// A hole card index does not map to its declared owner.
    #[error("wrong hole owner: {0}")]
    WrongHoleOwner(String),
    /// A revealed card id is outside `0..=51`.
    #[error("invalid card id {0}")]
    InvalidCardId(u32),
    /// The same card id was opened twice.
    #[error("duplicate card id {0}")]
    DuplicateCard(u32),
    /// An opened card's `(card_id, salt)` does not match the committed deck.
    #[error("opened card does not match committed deck at index {0}")]
    CommitmentMismatch(u32),
    /// A community stage appeared out of order.
    #[error("community stage out of order: {0}")]
    StageOutOfOrder(String),
    /// Hole cards were not all opened before a community stage.
    #[error("holes incomplete: {0}")]
    HolesIncomplete(String),
    /// `hand_complete` carried the wrong revealed-card count.
    #[error("revealed count mismatch: stated {stated}, actual {actual}")]
    RevealedCountMismatch {
        /// Count declared in the payload.
        stated: u32,
        /// Count the verifier observed.
        actual: u32,
    },
    /// An event arrived after the transcript already reached a terminal phase.
    #[error("event after terminal phase {0:?}")]
    AfterTerminal(Phase),
    /// A `hand_aborted` event carried an unusable payload (empty reason, or an
    /// `aborted_by` that is neither the coordinator nor a known party).
    #[error("invalid abort: {0}")]
    InvalidAbort(String),
    /// An unknown `event_type`.
    #[error("unknown event type '{0}'")]
    UnknownEvent(String),
}

/// The replayable dealing-protocol state.
#[derive(Debug, Clone, Serialize)]
pub struct ProtocolState {
    /// Current phase.
    pub phase: Phase,
    /// Number of players (set at `hand_init`).
    pub num_players: u8,
    /// Seats that have registered keys (sorted).
    pub registered_seats: Vec<u8>,
    /// Number of shuffle rounds completed.
    pub shuffle_rounds_done: u32,
    /// Hex hash of the most recent deck (canonical start, then each shuffle).
    pub last_deck_hash: String,
    /// Hex hash of the committed final deck, once committed.
    pub final_deck_hash: Option<String>,
    /// Per-card commitments of the committed final deck (hex).
    pub final_deck_commits: Vec<String>,
    /// Seats that acknowledged the final deck (sorted).
    pub acked_seats: Vec<u8>,
    /// Deck indices opened so far (sorted).
    pub opened_indices: Vec<u32>,
    /// Card ids revealed so far (sorted).
    pub opened_card_ids: Vec<u8>,
    /// Number of hole cards opened.
    pub holes_opened: u32,
    /// Total cards revealed (holes + community).
    pub revealed_count: u32,
    /// mp-phase4 (F1): `true` when `hand_init` declared `deck_repr == "reenc"` —
    /// the REAL re-encryption verifiable-shuffle path. On this path the
    /// shuffle-chain hashes (`last_deck_hash`) are `ec::deck_hash` **v2** values
    /// over the CIPHERTEXT deck (round 0 = `canonical_starting_deck()`), and the
    /// committed `final_deck_hash` (the per-card-commit hash) is DECOUPLED from
    /// the shuffle chain — the committed ciphertext deck travels in the last
    /// shuffle proof's attestation (`final_ciphertext_deck` / F3 anchor) and the
    /// per-card commits anchor the threshold opens (`apply_hole`/`apply_community`
    /// `card_commit` check). The mock path (`deck_repr == "wire"` / absent) keeps
    /// `final_deck_hash == last_deck_hash`. `#[serde(skip)]`: NOT part of
    /// `state_hash()` so the mock transcript stays byte-identical (HARD INVARIANT
    /// 2).
    #[serde(skip)]
    pub reenc_shuffle: bool,
}

impl ProtocolState {
    /// Fresh state, before any event.
    pub fn new() -> Self {
        Self {
            phase: Phase::PreInit,
            num_players: 0,
            registered_seats: Vec::new(),
            shuffle_rounds_done: 0,
            last_deck_hash: String::new(),
            final_deck_hash: None,
            final_deck_commits: Vec::new(),
            acked_seats: Vec::new(),
            opened_indices: Vec::new(),
            opened_card_ids: Vec::new(),
            holes_opened: 0,
            revealed_count: 0,
            reenc_shuffle: false,
        }
    }

    /// Deterministic hash of the entire state — used for
    /// `state_hash_before` / `state_hash_after`.
    pub fn state_hash(&self) -> Hash {
        let value = serde_json::to_value(self).expect("state always serializes");
        ds_hash("mp:state:v1", &[&canonical_json(&value)])
    }

    /// `true` once the transcript reached a terminal phase.
    pub fn is_terminal(&self) -> bool {
        self.phase.is_terminal()
    }

    /// Apply one transcript event, validating and mutating the state.
    ///
    /// Atomic: on any [`StateError`] the state is left **unchanged**, so a
    /// failing event never partially mutates `opened_indices` and friends.
    pub fn apply(&mut self, event_type: &str, payload: &Value) -> Result<(), StateError> {
        let mut next = self.clone();
        next.apply_inner(event_type, payload)?;
        *self = next;
        Ok(())
    }

    fn apply_inner(&mut self, event_type: &str, payload: &Value) -> Result<(), StateError> {
        if self.phase.is_terminal() {
            return Err(StateError::AfterTerminal(self.phase));
        }
        match event_type {
            event_type::HAND_INIT => self.apply_hand_init(payload),
            event_type::KEY_REGISTERED => self.apply_key_registered(payload),
            event_type::SHUFFLE_CONTRIBUTION => self.apply_shuffle(payload),
            event_type::FINAL_DECK_COMMITTED => self.apply_final_deck(payload),
            event_type::FINAL_DECK_ACK => self.apply_ack(payload),
            event_type::HOLE_CARD_OPENED => self.apply_hole(payload),
            event_type::COMMUNITY_REVEALED => self.apply_community(payload),
            event_type::HAND_COMPLETE => self.apply_complete(payload),
            event_type::HAND_ABORTED => self.apply_abort(payload),
            other => Err(StateError::UnknownEvent(other.to_string())),
        }
    }

    fn require_phase(&self, event: &str, want: Phase) -> Result<(), StateError> {
        if self.phase != want {
            return Err(StateError::WrongPhase {
                event: event.to_string(),
                phase: self.phase,
            });
        }
        Ok(())
    }

    fn n(&self) -> u32 {
        self.num_players as u32
    }

    fn apply_hand_init(&mut self, payload: &Value) -> Result<(), StateError> {
        self.require_phase(event_type::HAND_INIT, Phase::PreInit)?;
        let p: HandInitPayload = parse(payload)?;
        let n = p.players.len();
        if n < 2 {
            return Err(StateError::InvalidPlayerSet("fewer than 2 players".into()));
        }
        if 2 * n + 5 > DECK_SIZE {
            return Err(StateError::InvalidPlayerSet(
                "too many players for one deck".into(),
            ));
        }
        // Seats must be exactly 0..n-1, each declaring its canonical party id.
        let mut seats: Vec<u8> = p.players.iter().map(|e| e.seat).collect();
        seats.sort_unstable();
        for entry in &p.players {
            if entry.party_id != party_id(entry.seat) {
                return Err(StateError::InvalidPlayerSet(format!(
                    "seat {} has non-canonical party id",
                    entry.seat
                )));
            }
        }
        let expected: Vec<u8> = (0..n as u8).collect();
        if seats != expected {
            return Err(StateError::InvalidPlayerSet(
                "seats not dense 0..n-1".into(),
            ));
        }
        self.num_players = n as u8;
        // ADR-041 §5.1 Round-0 input rule (+ mp-phase4 F1 reenc seed):
        // - REAL re-encryption shuffle (`deck_repr == "reenc"`): seed with the
        //   `ec::deck_hash` **v2** of `canonical_starting_deck()` — the ciphertext
        //   deck the round-0 shuffler re-encrypts + permutes. The shuffle chain is
        //   then ciphertext-deck hashes throughout, decoupled from the per-card
        //   commit `final_deck_hash` (see `apply_final_deck`).
        // - Interactive mock path (`deck_repr == "wire"`): seed with wire_deck_hash([0..51]).
        // - Phase-1 simulated path (absent / None): seed with canonical_initial_deck_hash().
        match p.deck_repr.as_deref() {
            Some("reenc") => {
                self.reenc_shuffle = true;
                self.last_deck_hash = hex_hash(&crate::crypto_real::ec::deck_hash(
                    &crate::crypto_real::ec::canonical_starting_deck(),
                ));
            }
            Some("wire") => {
                self.last_deck_hash = hex_hash(&canonical_wire_deck_hash());
            }
            _ => {
                self.last_deck_hash = hex_hash(&canonical_initial_deck_hash());
            }
        }
        self.phase = Phase::KeyReg;
        Ok(())
    }

    fn apply_key_registered(&mut self, payload: &Value) -> Result<(), StateError> {
        self.require_phase(event_type::KEY_REGISTERED, Phase::KeyReg)?;
        let p: KeyRegisteredPayload = parse(payload)?;
        if p.seat as u32 >= self.n() || p.party_id != party_id(p.seat) {
            return Err(StateError::BadKeyRegistration(format!("seat {}", p.seat)));
        }
        if self.registered_seats.contains(&p.seat) {
            return Err(StateError::BadKeyRegistration(format!(
                "seat {} registered twice",
                p.seat
            )));
        }
        self.registered_seats.push(p.seat);
        self.registered_seats.sort_unstable();
        if self.registered_seats.len() as u32 == self.n() {
            self.phase = Phase::Shuffle;
        }
        Ok(())
    }

    fn apply_shuffle(&mut self, payload: &Value) -> Result<(), StateError> {
        self.require_phase(event_type::SHUFFLE_CONTRIBUTION, Phase::Shuffle)?;
        let p: ShuffleContributionPayload = parse(payload)?;
        if p.round != self.shuffle_rounds_done {
            return Err(StateError::BadShuffle(format!(
                "expected round {}, got {}",
                self.shuffle_rounds_done, p.round
            )));
        }
        if p.round >= self.n() {
            return Err(StateError::BadShuffle("too many shuffle rounds".into()));
        }
        // Round r is shuffled by the party in seat r.
        if p.party_id != party_id(p.round as u8) {
            return Err(StateError::BadShuffle(format!(
                "round {} must be shuffled by {}",
                p.round,
                party_id(p.round as u8)
            )));
        }
        if p.input_deck_hash != self.last_deck_hash {
            return Err(StateError::DeckDiscontinuity(format!(
                "round {} input does not match prior deck",
                p.round
            )));
        }
        // The proof object must be self-consistent with the declared decks.
        if p.proof.input_deck_hash != p.input_deck_hash
            || p.proof.output_deck_hash != p.output_deck_hash
        {
            return Err(StateError::BadShuffle(
                "proof deck hashes mismatch event".into(),
            ));
        }
        if parse_hash(&p.output_deck_hash).is_none() {
            return Err(StateError::BadShuffle("output deck hash malformed".into()));
        }
        self.last_deck_hash = p.output_deck_hash;
        self.shuffle_rounds_done += 1;
        Ok(())
    }

    fn apply_final_deck(&mut self, payload: &Value) -> Result<(), StateError> {
        self.require_phase(event_type::FINAL_DECK_COMMITTED, Phase::Shuffle)?;
        if self.shuffle_rounds_done != self.n() {
            return Err(StateError::FinalDeckMismatch(
                "not every party has shuffled".into(),
            ));
        }
        let p: FinalDeckCommittedPayload = parse(payload)?;
        if p.deck.len() != DECK_SIZE {
            return Err(StateError::FinalDeckMismatch(format!(
                "deck has {} cards, expected {DECK_SIZE}",
                p.deck.len()
            )));
        }
        // The committed deck hash must be exactly the output of the last shuffle.
        //
        // mp-phase4 F1: on BOTH paths `final_deck_hash` equals `last_deck_hash`
        // (the last shuffle's output hash) — but the digest TYPE differs by path:
        //   * Mock path: `last_deck_hash` is the per-card-COMMITMENT deck hash (the
        //     last mock round set its output to commit_deck_hash), and `deck` is the
        //     52 per-card commitments — so `deck_hash(deck) == final_deck_hash` holds.
        //   * REAL path (reenc): `last_deck_hash` is the CIPHERTEXT-deck `deck_hash`
        //     v2 of the final shuffle output (the value clients acked + signed + the
        //     F3 ciphertext-deck anchor). `deck` carries the per-card commitments of
        //     the OPENED card values (the open-anchor layer `apply_hole`/`apply_community`
        //     use), whose `deck_hash` is a DIFFERENT digest — so the commitment-hash
        //     equality is intentionally DECOUPLED on the reenc path. The committed
        //     ciphertext deck is bound by the last shuffle proof's attestation
        //     (`final_ciphertext_deck` → F3 `with_expected_deck`) and each open is
        //     cross-checked against `final_deck_commits` below.
        if p.final_deck_hash != self.last_deck_hash {
            return Err(StateError::FinalDeckMismatch(
                "committed deck differs from last shuffle output".into(),
            ));
        }
        // ...and (mock path only) its hash must match its declared per-card commits.
        let commits: Vec<EncCard> = {
            let mut v = Vec::with_capacity(DECK_SIZE);
            for c in &p.deck {
                match parse_hash(c) {
                    Some(h) => v.push(h),
                    None => {
                        return Err(StateError::FinalDeckMismatch(
                            "deck commitment malformed".into(),
                        ))
                    }
                }
            }
            v
        };
        if !self.reenc_shuffle && hex_hash(&deck_hash(&commits)) != p.final_deck_hash {
            return Err(StateError::FinalDeckMismatch(
                "final_deck_hash does not match deck contents".into(),
            ));
        }
        // mp-phase4 re-audit r1 (HIGH): on the REAL re-encryption path the offline
        // verifier anchors every threshold open to `final_deck_committed.deck_ct`
        // (`final_ciphertext_deck` → F3 `with_expected_deck`). But `final_deck_hash`
        // — the ONLY quantity the parties sign in `final_deck_ack` and the only one
        // tied to the verified ciphertext shuffle output — is the per-card-commit /
        // ciphertext-`deck_hash` value, NOT a hash of `deck_ct` itself. Without
        // binding `deck_ct` to `final_deck_hash` here, an untrusted (supposedly
        // server-blind) coordinator could keep the genuine `final_deck_hash` (so the
        // acks + the last-shuffle-output check above still pass) while substituting a
        // `deck_ct` of its choosing, then anchor self-supplied opens to that
        // attacker-chosen deck — a soundness hole in the independent replay verifier.
        //
        // We close it at the state machine: on the reenc path `final_deck_hash` IS
        // `ec::deck_hash` v2 of the committed ciphertext deck (`apply_hand_init` seeds
        // the chain with that digest, `apply_shuffle` carries it, and
        // `final_deck_hash == last_deck_hash`). So requiring
        // `hex_hash(ec::deck_hash(deck_ct)) == final_deck_hash` pins `deck_ct` to the
        // exact verified-shuffle output. A substituted `deck_ct` is rejected BEFORE
        // any open is processed. The mock path has no ciphertext deck and is
        // untouched (HARD INVARIANT 2: mock transcript byte-identical).
        if self.reenc_shuffle {
            use crate::crypto_real::ec::{deck_hash as ct_deck_hash, Ct, DECK_SIZE};
            let wire = p.deck_ct.as_ref().ok_or_else(|| {
                StateError::UnboundCiphertextDeck(
                    "real re-encryption path requires final_deck_committed.deck_ct".into(),
                )
            })?;
            if wire.len() != DECK_SIZE {
                return Err(StateError::UnboundCiphertextDeck(format!(
                    "deck_ct has {} entries, expected {DECK_SIZE}",
                    wire.len()
                )));
            }
            let decoded: Option<Vec<Ct>> = wire.iter().map(Ct::from_wire).collect();
            let decoded = decoded.ok_or_else(|| {
                StateError::UnboundCiphertextDeck("deck_ct has a malformed ElGamal point".into())
            })?;
            if hex_hash(&ct_deck_hash(&decoded)) != p.final_deck_hash {
                return Err(StateError::UnboundCiphertextDeck(
                    "deck_ct does not hash to the verified-shuffle final_deck_hash".into(),
                ));
            }
        }
        self.final_deck_hash = Some(p.final_deck_hash.clone());
        self.final_deck_commits = p.deck;
        self.phase = Phase::Committed;
        Ok(())
    }

    fn apply_ack(&mut self, payload: &Value) -> Result<(), StateError> {
        self.require_phase(event_type::FINAL_DECK_ACK, Phase::Committed)?;
        let p: FinalDeckAckPayload = parse(payload)?;
        let seat = seat_of_party(&p.party_id)
            .ok_or_else(|| StateError::BadKeyRegistration("non-party ack".into()))?;
        if seat as u32 >= self.n() {
            return Err(StateError::BadKeyRegistration(
                "ack from unknown seat".into(),
            ));
        }
        let committed = self
            .final_deck_hash
            .clone()
            .expect("final deck hash set in Committed phase");
        if p.final_deck_hash != committed {
            return Err(StateError::ForkedFinalDeck {
                acked: p.final_deck_hash,
                committed,
            });
        }
        if self.acked_seats.contains(&seat) {
            return Err(StateError::DuplicateAck(p.party_id));
        }
        self.acked_seats.push(seat);
        self.acked_seats.sort_unstable();
        if self.acked_seats.len() as u32 == self.n() {
            self.phase = Phase::Dealing;
        }
        Ok(())
    }

    fn apply_hole(&mut self, payload: &Value) -> Result<(), StateError> {
        if self.phase != Phase::Dealing {
            return Err(StateError::OpenBeforeDeckReady(format!(
                "hole open in phase {:?}",
                self.phase
            )));
        }
        let p: HoleCardOpenedPayload = parse(payload)?;
        if self.holes_opened >= 2 * self.n() {
            return Err(StateError::BadDeckIndex(
                "more hole cards than seats".into(),
            ));
        }
        let idx = p.card.deck_index;
        if idx >= 2 * self.n() {
            return Err(StateError::BadDeckIndex(format!(
                "index {idx} is not a hole-card slot"
            )));
        }
        // Layout: index < n → first card of seat `index`; else second card of `index-n`.
        let owner_seat = if idx < self.n() {
            idx as u8
        } else {
            (idx - self.n()) as u8
        };
        if p.seat != owner_seat || p.owner_party_id != party_id(owner_seat) {
            return Err(StateError::WrongHoleOwner(format!(
                "index {idx} belongs to seat {owner_seat}, claimed seat {}",
                p.seat
            )));
        }
        self.open_card(&p.card)?;
        self.holes_opened += 1;
        Ok(())
    }

    fn apply_community(&mut self, payload: &Value) -> Result<(), StateError> {
        let p: CommunityRevealedPayload = parse(payload)?;
        if self.holes_opened != 2 * self.n() {
            return Err(StateError::HolesIncomplete(format!(
                "{} of {} hole cards opened",
                self.holes_opened,
                2 * self.n()
            )));
        }
        let base = 2 * self.n();
        let (want_phase, next_phase, indices) = match p.stage.as_str() {
            "flop" => (Phase::Dealing, Phase::Flop, vec![base, base + 1, base + 2]),
            "turn" => (Phase::Flop, Phase::Turn, vec![base + 3]),
            "river" => (Phase::Turn, Phase::River, vec![base + 4]),
            other => {
                return Err(StateError::StageOutOfOrder(format!(
                    "unknown stage '{other}'"
                )))
            }
        };
        if self.phase != want_phase {
            return Err(StateError::StageOutOfOrder(format!(
                "stage '{}' requires phase {:?}, in {:?}",
                p.stage, want_phase, self.phase
            )));
        }
        if p.cards.len() != indices.len() {
            return Err(StateError::BadDeckIndex(format!(
                "stage '{}' expects {} cards, got {}",
                p.stage,
                indices.len(),
                p.cards.len()
            )));
        }
        for (card, expected_idx) in p.cards.iter().zip(indices.iter()) {
            if card.deck_index != *expected_idx {
                return Err(StateError::BadDeckIndex(format!(
                    "stage '{}' expected index {expected_idx}, got {}",
                    p.stage, card.deck_index
                )));
            }
            self.open_card(card)?;
        }
        self.phase = next_phase;
        Ok(())
    }

    /// Shared open-card validation: index range, no reopen, valid + unique card
    /// id, and the commitment matches the committed deck.
    fn open_card(&mut self, card: &OpenedCard) -> Result<(), StateError> {
        let idx = card.deck_index;
        if idx as usize >= DECK_SIZE {
            return Err(StateError::BadDeckIndex(format!("index {idx} >= 52")));
        }
        if self.opened_indices.contains(&idx) {
            return Err(StateError::IndexReopened(idx));
        }
        if !is_valid_card_id(card.card_id as u32) {
            return Err(StateError::InvalidCardId(card.card_id as u32));
        }
        if self.opened_card_ids.contains(&card.card_id) {
            return Err(StateError::DuplicateCard(card.card_id as u32));
        }
        // Salt + card id must reconstruct the committed final-deck commitment.
        let salt = parse_salt(&card.salt)
            .ok_or_else(|| StateError::MalformedPayload("salt not 32 hex bytes".into()))?;
        let expected = self
            .final_deck_commits
            .get(idx as usize)
            .ok_or_else(|| StateError::BadDeckIndex(format!("no commitment at {idx}")))?;
        if hex_hash(&card_commit(card.card_id, &salt)) != *expected {
            return Err(StateError::CommitmentMismatch(idx));
        }
        self.opened_indices.push(idx);
        self.opened_indices.sort_unstable();
        self.opened_card_ids.push(card.card_id);
        self.opened_card_ids.sort_unstable();
        self.revealed_count += 1;
        Ok(())
    }

    fn apply_complete(&mut self, payload: &Value) -> Result<(), StateError> {
        self.require_phase(event_type::HAND_COMPLETE, Phase::River)?;
        let p: HandCompletePayload = parse(payload)?;
        if p.revealed_card_count != self.revealed_count {
            return Err(StateError::RevealedCountMismatch {
                stated: p.revealed_card_count,
                actual: self.revealed_count,
            });
        }
        self.phase = Phase::Complete;
        Ok(())
    }

    fn apply_abort(&mut self, payload: &Value) -> Result<(), StateError> {
        // Abort is legal from any non-terminal phase (terminal already guarded),
        // but — like every other event — its payload must be well-formed. An
        // empty/garbage `hand_aborted` must NOT replay clean: ADR-041 §5.3 frames
        // an abort as an accountable, evidence-bearing terminal event, so the
        // state machine records *who* raised it and *why* rather than accepting a
        // bare phase flip the untrusted coordinator can inject silently.
        let p: HandAbortedPayload = parse(payload)?;
        if p.reason.trim().is_empty() {
            return Err(StateError::InvalidAbort("empty reason".into()));
        }
        // `aborted_by` must be an identity the transcript can attribute the abort
        // to: the coordinator, or a canonical party that is part of this hand.
        // (Pre-`hand_init` `num_players` is 0, so only the coordinator can abort —
        // no parties exist yet, which is correct.)
        let known = p.aborted_by == COORDINATOR
            || seat_of_party(&p.aborted_by).is_some_and(|seat| (seat as u32) < self.n());
        if !known {
            return Err(StateError::InvalidAbort(format!(
                "aborted_by '{}' is neither coordinator nor a party in this hand",
                p.aborted_by
            )));
        }
        self.phase = Phase::Aborted;
        Ok(())
    }
}

impl Default for ProtocolState {
    fn default() -> Self {
        Self::new()
    }
}

/// The public, agreed-upon starting deck: 52 commitments to card ids `0..51`
/// with the all-zero salt. Both builder and verifier derive the same constant.
pub fn canonical_initial_deck() -> Vec<EncCard> {
    (0..DECK_SIZE as u8)
        .map(|id| card_commit(id, &ZERO_SALT))
        .collect()
}

/// Hash of [`canonical_initial_deck`].
pub fn canonical_initial_deck_hash() -> Hash {
    deck_hash(&canonical_initial_deck())
}

/// Zero hash convenience re-export for the first event's `previous_event_hash`.
pub fn genesis_prev_hash() -> Hash {
    ZERO_HASH
}

fn parse<T: serde::de::DeserializeOwned>(payload: &Value) -> Result<T, StateError> {
    serde_json::from_value(payload.clone()).map_err(|e| StateError::MalformedPayload(e.to_string()))
}

fn parse_salt(hex_str: &str) -> Option<Salt> {
    // `Salt` and `Hash` are both `[u8; 32]`; reuse the shared hex→32-byte decoder.
    parse_hash(hex_str)
}

/// Extract the seat from a canonical `party:N` id.
pub fn seat_of_party(party_id: &str) -> Option<u8> {
    party_id.strip_prefix("party:")?.parse().ok()
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    /// A two-player state advanced past `hand_init` (in `KeyReg`, so two parties
    /// exist) — the realistic point an abort could be injected.
    fn two_player_state() -> ProtocolState {
        let mut s = ProtocolState::new();
        s.apply(
            event_type::HAND_INIT,
            &json!({
                "players": [
                    { "seat": 0, "party_id": "party:0" },
                    { "seat": 1, "party_id": "party:1" }
                ],
                "button_seat": 0,
                "big_blind": 2,
                "small_blind": 1
            }),
        )
        .expect("hand_init applies");
        assert_eq!(s.phase, Phase::KeyReg);
        s
    }

    #[test]
    fn abort_with_valid_payload_by_coordinator_succeeds() {
        let mut s = two_player_state();
        s.apply(
            event_type::HAND_ABORTED,
            &json!({ "aborted_by": "coordinator", "reason": "timeout", "evidence": {} }),
        )
        .expect("a well-formed coordinator abort is accepted");
        assert_eq!(s.phase, Phase::Aborted);
        assert!(s.is_terminal());
    }

    #[test]
    fn abort_by_a_seated_party_succeeds() {
        let mut s = two_player_state();
        s.apply(
            event_type::HAND_ABORTED,
            &json!({ "aborted_by": "party:1", "reason": "peer dropped", "evidence": {} }),
        )
        .expect("a well-formed abort by a party in the hand is accepted");
        assert_eq!(s.phase, Phase::Aborted);
    }

    #[test]
    fn abort_with_garbage_payload_is_rejected() {
        // A bare phase flip — no `aborted_by` / `reason` / `evidence` — must NOT
        // replay clean (the gap the audit flagged: silent unilateral abort).
        let mut s = two_player_state();
        let err = s
            .apply(event_type::HAND_ABORTED, &json!({}))
            .expect_err("an empty abort payload must be rejected");
        assert!(matches!(err, StateError::MalformedPayload(_)));
        // State is left unchanged on error (atomic apply).
        assert_eq!(s.phase, Phase::KeyReg);
    }

    #[test]
    fn abort_with_empty_reason_is_rejected() {
        let mut s = two_player_state();
        let err = s
            .apply(
                event_type::HAND_ABORTED,
                &json!({ "aborted_by": "coordinator", "reason": "  ", "evidence": {} }),
            )
            .expect_err("a blank reason must be rejected");
        assert!(matches!(err, StateError::InvalidAbort(_)));
        assert_eq!(s.phase, Phase::KeyReg);
    }

    #[test]
    fn abort_by_unknown_party_is_rejected() {
        // `party:9` is not a seat in this 2-player hand → unattributable abort.
        let mut s = two_player_state();
        let err = s
            .apply(
                event_type::HAND_ABORTED,
                &json!({ "aborted_by": "party:9", "reason": "x", "evidence": {} }),
            )
            .expect_err("an abort attributed to a non-party must be rejected");
        assert!(matches!(err, StateError::InvalidAbort(_)));
        assert_eq!(s.phase, Phase::KeyReg);
    }

    #[test]
    fn abort_by_non_party_string_is_rejected() {
        let mut s = two_player_state();
        let err = s
            .apply(
                event_type::HAND_ABORTED,
                &json!({ "aborted_by": "attacker", "reason": "x", "evidence": {} }),
            )
            .expect_err("an abort from an arbitrary id must be rejected");
        assert!(matches!(err, StateError::InvalidAbort(_)));
    }
}
