//! Mental Poker dealing provider (mock crypto).
//!
//! `MentalPokerDealingProvider` runs the full dealing protocol — key
//! registration, round-robin re-encryption shuffle, final deck commitment,
//! per-party acknowledgement, owner-only hole-card reveal, staged community
//! reveal — and produces a signed, hash-chained [`Transcript`] alongside the
//! 52-card deck the engine consumes.
//!
//! # MVP scope & honest limitations
//!
//! - **Mock crypto.** Shuffle and decryption proofs come from
//!   [`crate::crypto::MockShuffleProofProvider`] / [`MockDecryptionProvider`];
//!   signatures from [`crate::signing::MockSignatureProvider`]. None are
//!   production-safe — see those modules and `docs/...refactor.md` §7.
//! - **Coordinator-simulated parties.** All `n` parties are simulated locally
//!   from one master seed. There is not yet a real distributed key-exchange /
//!   shuffle choreography over WebSocket (rollout §8 phase 3).
//! - **Deal-time transcript.** The full reveal schedule (all hole cards + the
//!   board) is generated when the hand is dealt. Production opens community
//!   cards progressively as streets advance; the event *ordering* is identical,
//!   so the verifier is unaffected.
//!
//! Despite the mock crypto the transcript is a genuine, fully-verifiable
//! artifact: every rule in `verifier.rs` is exercised.

use crate::card_id::{id_to_card, CardId, DECK_SIZE};
use crate::crypto::{
    card_commit, deck_hash, DecryptionProvider, MockDecryptionProvider, MockShuffleProofProvider,
    Salt, ShuffleProofProvider,
};
use crate::events::*;
use crate::hash::{canonical_json, ds_hash, hex_hash, Hash};
use crate::provider::{DealRequest, DealingProvider, DealtHand};
use crate::signing::{MockSignatureProvider, SignatureProvider};
use crate::state::canonical_initial_deck_hash;
use crate::transcript::{to_payload, TranscriptBuilder};
use engine::Card;
use serde_json::{json, Value};

/// Fault-injection scenarios for negative testing.
///
/// `Valid` is the normal path; the rest deliberately corrupt the transcript so
/// the verifier's rejection paths can be exercised. All produce a transcript
/// that is still cryptographically well-formed (correct chain + signatures) —
/// the fault is *semantic*.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Scenario {
    /// A correct, fully-verifiable hand.
    Valid,
    /// A hole card opened to a value inconsistent with the committed deck.
    TamperedHoleCard,
    /// One party acked a final deck hash different from the committed one.
    ForkedFinalDeck,
    /// The flop reveals the same card id twice.
    DuplicateCommunityCard,
    /// A hole card is opened before the deck is committed + acknowledged.
    EarlyHoleOpen,
    /// The turn is revealed before the flop.
    EarlyCommunityCard,
    /// A shuffle proof's attestation is corrupted.
    InvalidShuffleProof,
    /// A decryption proof's attestation is corrupted.
    InvalidDecryptionProof,
    /// The terminal `hand_complete` event is missing.
    MissingComplete,
    /// ADR-041 §4 contributor-binding forgery: a `final_deck_ack` is signed by a
    /// DIFFERENT party than the one the state machine records the ack for. The
    /// transcript is otherwise fully valid — only the contributor identity is
    /// decoupled from the acking `party_id`. A verifier that does not bind
    /// `contributor == party_id` accepts it (a malicious coordinator forging a
    /// player's deck acknowledgement).
    ForgedAckContributor,
}

/// Mental Poker dealing provider — see module docs.
#[derive(Debug, Clone)]
pub struct MentalPokerDealingProvider {
    /// Caller-supplied entropy folded into the master seed. The server passes
    /// OS randomness; tests pass a fixed value for determinism.
    entropy: Vec<u8>,
}

impl MentalPokerDealingProvider {
    /// Construct with explicit entropy (the server passes OS randomness).
    pub fn new(entropy: Vec<u8>) -> Self {
        Self { entropy }
    }

    /// A deterministic provider for tests / the `mp-verify --demo` CLI.
    pub fn deterministic() -> Self {
        Self {
            entropy: b"mental-poker-deterministic-entropy".to_vec(),
        }
    }

    /// Deal a hand under a specific [`Scenario`] (fault injection for tests).
    ///
    /// # Panics
    ///
    /// U65 (dual-AI OSS review): panics if `request.num_players` is outside
    /// `2..=23` — `2 * n + 5` cards must fit one 52-card deck (the defensive
    /// guard in `ProtocolRun::new`; Hold'em tables cap at 9 seats, so a live
    /// caller never hits it).
    pub fn deal_scenario(&self, request: &DealRequest, scenario: Scenario) -> DealtHand {
        let run = ProtocolRun::new(&self.entropy, request);
        let mut events = run.plan_events();
        apply_scenario(&mut events, scenario);

        let signers = run.signer_ids();
        let sig = MockSignatureProvider::from_seed(&run.master_seed, &signers);
        let shuffle = MockShuffleProofProvider;
        let decryption = MockDecryptionProvider;

        let mut builder = TranscriptBuilder::new(
            request.hand_id.clone(),
            request.table_id.clone(),
            "mental_poker_mock",
            &sig,
            &shuffle,
            &decryption,
            sig.directory(),
        );
        let mut ack_forged = false;
        for ev in events {
            // ADR-041 §4.2: for client-action events the coordinator (simulating
            // each party) also produces a contributor claim + signature. The
            // contributor is normally `ev.signer` (the acting party). The
            // ForgedAckContributor fault decouples the FIRST final_deck_ack's
            // contributor from its acking party_id — the binding forgery.
            let contributor_signer: String = if matches!(scenario, Scenario::ForgedAckContributor)
                && ev.event_type == event_type::FINAL_DECK_ACK
                && !ack_forged
            {
                ack_forged = true;
                // Sign + claim as a DIFFERENT party than the one this ack is for.
                if ev.signer == party_id(0) {
                    party_id(1)
                } else {
                    party_id(0)
                }
            } else {
                ev.signer.clone()
            };
            let contributor_claim = build_contributor_claim(
                ev.event_type,
                &request.hand_id,
                &contributor_signer,
                &ev.payload,
                &sig,
            );
            builder.append_with_contributor(
                ev.event_type,
                ev.payload,
                &ev.signer,
                contributor_claim
                    .as_ref()
                    .map(|(contributor, claim_sig)| (contributor.as_str(), claim_sig.as_str())),
            );
        }

        DealtHand {
            deck: run.deck_cards(),
            deck_seed: run.deck_seed(),
            transcript: Some(builder.finish()),
        }
    }
}

impl DealingProvider for MentalPokerDealingProvider {
    fn name(&self) -> &'static str {
        "mental_poker_mock"
    }

    fn is_verifiable(&self) -> bool {
        true
    }

    /// Deal a correct, fully-verifiable hand ([`Scenario::Valid`]).
    ///
    /// # Panics
    ///
    /// U65 (dual-AI OSS review): panics if `request.num_players` is outside
    /// `2..=23` — see [`MentalPokerDealingProvider::deal_scenario`].
    fn deal(&self, request: &DealRequest) -> DealtHand {
        self.deal_scenario(request, Scenario::Valid)
    }
}

// ---------------------------------------------------------------------------
// Protocol run — derives every value deterministically from the master seed.
// ---------------------------------------------------------------------------

#[derive(Clone)]
struct PlannedEvent {
    event_type: &'static str,
    payload: Value,
    signer: String,
}

struct ProtocolRun {
    master_seed: Hash,
    n: usize,
    button_seat: u8,
    big_blind: u64,
    small_blind: u64,
    /// `final_ids[deck_index]` = the card id at that position in the dealt deck.
    final_ids: Vec<CardId>,
}

impl ProtocolRun {
    fn new(entropy: &[u8], request: &DealRequest) -> Self {
        let master_seed = ds_hash(
            "mp:master:v1",
            &[
                entropy,
                request.hand_id.as_bytes(),
                request.table_id.as_bytes(),
            ],
        );
        let n = request.num_players as usize;

        // Defensive guard: one 52-card deck must hold `2n` hole cards + 5 board
        // cards. Without this, a `num_players >= 24` request indexes
        // `final_ids[deck_index]` past the 52-card deck and panics with an opaque
        // slice-index error deep inside `plan_events`. Hold'em tables cap at 9
        // seats so this is not reachable today, but the provider must reject the
        // bad input loudly rather than rely on the caller's seat cap. Mirrors the
        // state machine's `apply_hand_init` guard (`2*n + 5 > DECK_SIZE`).
        assert!(
            n >= 2 && 2 * n + 5 <= DECK_SIZE,
            "mental-poker deal requires 2..={} players to fit one 52-card deck, got {n}",
            (DECK_SIZE - 5) / 2,
        );

        // Final deck = canonical order put through `n` re-encryption shuffles,
        // one permutation per party.
        let mut order: Vec<CardId> = (0..DECK_SIZE as CardId).collect();
        for party in 0..n {
            let perm = random_permutation(&master_seed, party);
            order = perm.iter().map(|&k| order[k]).collect();
        }

        Self {
            master_seed,
            n,
            button_seat: request.button_seat,
            big_blind: request.big_blind,
            small_blind: request.small_blind,
            final_ids: order,
        }
    }

    fn deck_seed(&self) -> engine::DeckSeed {
        // The full 256-bit master seed is the reproducibility seed (ADR-062 §2).
        self.master_seed
    }

    fn deck_cards(&self) -> Vec<Card> {
        self.final_ids
            .iter()
            .map(|&id| id_to_card(id).expect("final ids are 0..51"))
            .collect()
    }

    fn signer_ids(&self) -> Vec<String> {
        let mut ids: Vec<String> = (0..self.n as u8).map(party_id).collect();
        ids.push(COORDINATOR.to_string());
        ids
    }

    /// Salt for the final-deck commitment at `deck_index`.
    fn salt(&self, deck_index: usize) -> Salt {
        ds_hash(
            "mp:salt:v1",
            &[&self.master_seed, &(deck_index as u64).to_le_bytes()],
        )
    }

    fn final_commits(&self) -> Vec<Hash> {
        (0..DECK_SIZE)
            .map(|j| card_commit(self.final_ids[j], &self.salt(j)))
            .collect()
    }

    /// Hash of the encrypted deck after shuffle round `r` (`r < n`).
    /// The last round's output is the committed final deck.
    fn round_output_hash(&self, r: usize, final_hash: &Hash) -> Hash {
        if r + 1 == self.n {
            *final_hash
        } else {
            let interim: Vec<Hash> = (0..DECK_SIZE)
                .map(|j| {
                    ds_hash(
                        "mp:interim:v1",
                        &[
                            &self.master_seed,
                            &(r as u64).to_le_bytes(),
                            &(j as u64).to_le_bytes(),
                        ],
                    )
                })
                .collect();
            deck_hash(&interim)
        }
    }

    /// A decryption witness for `deck_index`: `(card_id, salt)`.
    fn opened_card(
        &self,
        decryption: &dyn DecryptionProvider,
        party: &str,
        deck_index: usize,
    ) -> OpenedCard {
        let card_id = self.final_ids[deck_index];
        let salt = self.salt(deck_index);
        OpenedCard {
            deck_index: deck_index as u32,
            card_id,
            salt: hex::encode(salt),
            proof: decryption.prove_decryption(party, deck_index as u32, card_id, &salt),
        }
    }

    /// Build the full, ordered, valid event plan for the hand.
    fn plan_events(&self) -> Vec<PlannedEvent> {
        let shuffle = MockShuffleProofProvider;
        let decryption = MockDecryptionProvider;
        let mut events: Vec<PlannedEvent> = Vec::new();

        // hand_init.
        let players: Vec<PlayerEntry> = (0..self.n as u8)
            .map(|seat| PlayerEntry {
                seat,
                party_id: party_id(seat),
            })
            .collect();
        events.push(PlannedEvent {
            event_type: event_type::HAND_INIT,
            payload: to_payload(&HandInitPayload {
                players,
                button_seat: self.button_seat,
                big_blind: self.big_blind,
                small_blind: self.small_blind,
                // Phase-1 coordinator-simulated path: no deck_repr field.
                // The state machine seeds with canonical_initial_deck_hash().
                deck_repr: None,
            }),
            signer: COORDINATOR.to_string(),
        });

        // key_registered, one per party.
        // contributor fields are None here — injected later by deal_scenario.
        for seat in 0..self.n as u8 {
            events.push(PlannedEvent {
                event_type: event_type::KEY_REGISTERED,
                payload: to_payload(&KeyRegisteredPayload {
                    party_id: party_id(seat),
                    seat,
                    signing_pubkey: self.pubkey(seat, b"sign"),
                    shuffle_pubkey: self.pubkey(seat, b"shuffle"),
                    contributor: None,
                    contributor_signature: None,
                    // Mock-shuffle path: shuffle_pubkey is a hash, not a curve
                    // point — no discrete log to prove (F2 PoK is real-shuffle only).
                    key_pok: None,
                }),
                signer: party_id(seat),
            });
        }

        // Round-robin shuffle: round r is performed by party r.
        let final_commits = self.final_commits();
        let final_hash = deck_hash(&final_commits);
        let mut input_hash = canonical_initial_deck_hash();
        for r in 0..self.n {
            let output_hash = self.round_output_hash(r, &final_hash);
            let party = party_id(r as u8);
            let proof = shuffle.prove_shuffle(&party, r as u32, &input_hash, &output_hash);
            events.push(PlannedEvent {
                event_type: event_type::SHUFFLE_CONTRIBUTION,
                payload: to_payload(&ShuffleContributionPayload {
                    party_id: party.clone(),
                    round: r as u32,
                    input_deck_hash: hex_hash(&input_hash),
                    output_deck_hash: hex_hash(&output_hash),
                    proof,
                    contributor: None,
                    contributor_signature: None,
                }),
                signer: party,
            });
            input_hash = output_hash;
        }

        // final_deck_committed.
        events.push(PlannedEvent {
            event_type: event_type::FINAL_DECK_COMMITTED,
            payload: to_payload(&FinalDeckCommittedPayload {
                final_deck_hash: hex_hash(&final_hash),
                deck: final_commits.iter().map(hex_hash).collect(),
                // Mock-decryption path: no ciphertext deck (F3 anchor is real-only).
                deck_ct: None,
            }),
            signer: COORDINATOR.to_string(),
        });

        // final_deck_ack, one per party.
        for seat in 0..self.n as u8 {
            events.push(PlannedEvent {
                event_type: event_type::FINAL_DECK_ACK,
                payload: to_payload(&FinalDeckAckPayload {
                    party_id: party_id(seat),
                    final_deck_hash: hex_hash(&final_hash),
                    contributor: None,
                    contributor_signature: None,
                }),
                signer: party_id(seat),
            });
        }

        // Hole cards: deck index < n → first card of seat `index`;
        // n <= index < 2n → second card of seat `index - n`.
        for idx in 0..2 * self.n {
            let owner_seat = if idx < self.n {
                idx as u8
            } else {
                (idx - self.n) as u8
            };
            let party = party_id(owner_seat);
            events.push(PlannedEvent {
                event_type: event_type::HOLE_CARD_OPENED,
                payload: to_payload(&HoleCardOpenedPayload {
                    seat: owner_seat,
                    owner_party_id: party.clone(),
                    card: self.opened_card(&decryption, &party, idx),
                    contributor: None,
                    contributor_signature: None,
                }),
                signer: party,
            });
        }

        // Community: flop / turn / river. Jointly decrypted → coordinator signs.
        let base = 2 * self.n;
        for (stage, indices) in [
            ("flop", vec![base, base + 1, base + 2]),
            ("turn", vec![base + 3]),
            ("river", vec![base + 4]),
        ] {
            let cards: Vec<OpenedCard> = indices
                .iter()
                .map(|&i| self.opened_card(&decryption, COORDINATOR, i))
                .collect();
            events.push(PlannedEvent {
                event_type: event_type::COMMUNITY_REVEALED,
                payload: to_payload(&CommunityRevealedPayload {
                    stage: stage.to_string(),
                    cards,
                }),
                signer: COORDINATOR.to_string(),
            });
        }

        // hand_complete.
        events.push(PlannedEvent {
            event_type: event_type::HAND_COMPLETE,
            payload: to_payload(&HandCompletePayload {
                revealed_card_count: (2 * self.n + 5) as u32,
            }),
            signer: COORDINATOR.to_string(),
        });

        events
    }

    fn pubkey(&self, seat: u8, kind: &[u8]) -> String {
        hex_hash(&ds_hash(
            "mp:pubkey:v1",
            &[&self.master_seed, &[seat], kind],
        ))
    }
}

/// Re-encryption shuffle permutation for `party`, derived from the master seed.
fn random_permutation(seed: &Hash, party: usize) -> Vec<usize> {
    let mut drbg = HashDrbg::new(seed, party as u64);
    let mut perm: Vec<usize> = (0..DECK_SIZE).collect();
    for i in (1..DECK_SIZE).rev() {
        let j = drbg.below(i as u64 + 1) as usize;
        perm.swap(i, j);
    }
    perm
}

/// SHA-256 counter-mode deterministic byte generator (mock — not a CSPRNG API,
/// but adequate for reproducible shuffle permutations).
struct HashDrbg {
    seed: Hash,
    label: u64,
    counter: u64,
}

impl HashDrbg {
    fn new(seed: &Hash, label: u64) -> Self {
        Self {
            seed: *seed,
            label,
            counter: 0,
        }
    }

    fn next_u64(&mut self) -> u64 {
        let block = ds_hash(
            "mp:drbg:v1",
            &[
                &self.seed,
                &self.label.to_le_bytes(),
                &self.counter.to_le_bytes(),
            ],
        );
        self.counter += 1;
        u64::from_le_bytes(block[0..8].try_into().unwrap())
    }

    /// Uniform integer in `[0, n)` with rejection sampling (no modulo bias).
    fn below(&mut self, n: u64) -> u64 {
        if n <= 1 {
            return 0;
        }
        let threshold = u64::MAX - (u64::MAX % n);
        loop {
            let v = self.next_u64();
            if v < threshold {
                return v % n;
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Fault injection
// ---------------------------------------------------------------------------

fn find(events: &[PlannedEvent], event_type: &str) -> Option<usize> {
    events.iter().position(|e| e.event_type == event_type)
}

fn apply_scenario(events: &mut Vec<PlannedEvent>, scenario: Scenario) {
    match scenario {
        Scenario::Valid => {}

        Scenario::TamperedHoleCard => {
            if let Some(i) = find(events, event_type::HOLE_CARD_OPENED) {
                if let Some(card_id) = events[i].payload["card"]["card_id"].as_u64() {
                    // Flip to a different valid id — breaks the deck commitment.
                    events[i].payload["card"]["card_id"] = json!((card_id + 1) % 52);
                }
            }
        }

        Scenario::ForkedFinalDeck => {
            if let Some(i) = find(events, event_type::FINAL_DECK_ACK) {
                // Party acks a deck hash the coordinator never committed.
                events[i].payload["final_deck_hash"] =
                    json!(hex_hash(&ds_hash("mp:fork:v1", &[b"forged-deck"])));
            }
        }

        Scenario::DuplicateCommunityCard => {
            if let Some(i) = find(events, event_type::COMMUNITY_REVEALED) {
                let first = events[i].payload["cards"][0].clone();
                if let Some(second) = events[i].payload["cards"].get_mut(1) {
                    // Reuse the first flop card's id — a duplicate public card.
                    second["card_id"] = first["card_id"].clone();
                }
            }
        }

        Scenario::EarlyHoleOpen => {
            // Re-insert the first hole open just after final_deck_committed,
            // before any ack — i.e. before the deck is acknowledged.
            if let (Some(commit_i), Some(hole_i)) = (
                find(events, event_type::FINAL_DECK_COMMITTED),
                find(events, event_type::HOLE_CARD_OPENED),
            ) {
                let early = events[hole_i].clone();
                events.insert(commit_i + 1, early);
            }
        }

        Scenario::EarlyCommunityCard => {
            // Swap flop and turn so the turn is revealed first.
            let community: Vec<usize> = events
                .iter()
                .enumerate()
                .filter(|(_, e)| e.event_type == event_type::COMMUNITY_REVEALED)
                .map(|(i, _)| i)
                .collect();
            if community.len() >= 2 {
                events.swap(community[0], community[1]);
            }
        }

        Scenario::InvalidShuffleProof => {
            if let Some(i) = find(events, event_type::SHUFFLE_CONTRIBUTION) {
                events[i].payload["proof"]["attestation"] =
                    json!(hex_hash(&ds_hash("mp:forge:v1", &[b"bad-shuffle"])));
            }
        }

        Scenario::InvalidDecryptionProof => {
            if let Some(i) = find(events, event_type::HOLE_CARD_OPENED) {
                events[i].payload["card"]["proof"]["attestation"] =
                    json!(hex_hash(&ds_hash("mp:forge:v1", &[b"bad-decrypt"])));
            }
        }

        Scenario::MissingComplete => {
            if let Some(i) = find(events, event_type::HAND_COMPLETE) {
                events.remove(i);
            }
        }

        // The contributor forgery is injected later, while signing (deal_scenario),
        // not by mutating a planned event here.
        Scenario::ForgedAckContributor => {}
    }
}

// ---------------------------------------------------------------------------
// ADR-041 §4: contributor claim construction + signing
// ---------------------------------------------------------------------------

/// Build the contributor claim signature for a client-action event.
///
/// Returns `Some((contributor_id, hex_signature))` for events that require a
/// contributor signature, or `None` for coordinator-only events.
fn build_contributor_claim(
    event_type_str: &'static str,
    hand_id: &str,
    signer: &str,
    payload: &Value,
    sig: &MockSignatureProvider,
) -> Option<(String, String)> {
    // Single canonical builder, shared with the verifier so the signed claim and
    // the verified claim can never drift (ADR-041 §4.1).
    let claim_obj =
        crate::verifier::contributor_claim_object(event_type_str, hand_id, signer, payload)?;
    let claim_hash = ds_hash("mp:claim:v1", &[&canonical_json(&claim_obj)]);
    let claim_sig = sig.sign(signer, &claim_hash);
    Some((signer.to_string(), claim_sig))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::provider::DealRequest;

    fn request(num_players: u8) -> DealRequest {
        DealRequest {
            hand_id: "hand-test".into(),
            table_id: "table-test".into(),
            num_players,
            button_seat: 0,
            big_blind: 2,
            small_blind: 1,
        }
    }

    #[test]
    fn deals_at_the_largest_size_that_fits_one_deck() {
        // 23 players → 2*23 + 5 = 51 cards, the largest that fits a 52-card deck.
        let dealt = MentalPokerDealingProvider::deterministic().deal(&request(23));
        assert_eq!(dealt.deck.len(), DECK_SIZE);
    }

    #[test]
    #[should_panic(expected = "players to fit one 52-card deck")]
    fn rejects_too_many_players_with_a_clear_message() {
        // 24 players would need 2*24 + 5 = 53 cards and previously panicked deep
        // inside plan_events with an opaque slice-index error. The defensive
        // guard now rejects it loudly at the provider boundary.
        let _ = MentalPokerDealingProvider::deterministic().deal(&request(24));
    }
}
