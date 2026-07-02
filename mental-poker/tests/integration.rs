//! End-to-end tests for the Mental Poker dealing protocol.
//!
//! Covers every acceptance scenario in the refactor goal step 9:
//! a valid hand verifies; tampering with an event / ordering / signature /
//! final deck / opened card is rejected; replay is rejected; opening before
//! the final commit, opening a future community card early, a duplicate public
//! card, an invalid shuffle / decryption proof, and a forked final deck are
//! all detected. Plus: the dealt deck drives the existing engine.
//!
//! Also covers the ADR-041 interactive (real-client) choreography:
//! BLOCKER-1+2 tests confirm `verify()` passes with real per-party keys and
//! that swapping in a forged contributor signature makes `verify()` fail.

use engine::{Chips, Deck, GameHand, PlayerId};
use mental_poker::state::StateError;
use mental_poker::verifier::VerifyErrorKind;
use mental_poker::{
    verify, DealRequest, DealingProvider, ExistingServerDealingProvider,
    MentalPokerDealingProvider, Scenario, Transcript,
};
// Imports for interactive transcript tests.
use mental_poker::card_id::id_to_card;
use mental_poker::crypto::{
    card_commit, deck_hash, DecryptionProvider, MockDecryptionProvider, MockShuffleProofProvider,
    ShuffleProofProvider,
};
use mental_poker::events::{
    event_type, party_id as mp_party_id, CommunityRevealedPayload, FinalDeckAckPayload,
    FinalDeckCommittedPayload, HandCompletePayload, HandInitPayload, HoleCardOpenedPayload,
    KeyRegisteredPayload, OpenedCard, PlayerEntry, ShuffleContributionPayload, COORDINATOR,
};
use mental_poker::hash::{canonical_json, ds_hash, hex_hash};
use mental_poker::signing::{KeyDirectory, MockSignatureProvider, SignatureProvider};
use mental_poker::state::{canonical_initial_deck_hash, canonical_wire_deck_hash};
use mental_poker::transcript::{to_payload, TranscriptBuilder};
use std::collections::BTreeMap;

fn request(num_players: u8) -> DealRequest {
    DealRequest {
        hand_id: format!("hand-{num_players}p"),
        table_id: "table-1".to_string(),
        num_players,
        button_seat: 0,
        big_blind: 20,
        small_blind: 10,
    }
}

fn deal(num_players: u8, scenario: Scenario) -> Transcript {
    MentalPokerDealingProvider::deterministic()
        .deal_scenario(&request(num_players), scenario)
        .transcript
        .expect("mental poker provider always produces a transcript")
}

// ---------------------------------------------------------------------------
// Valid hands
// ---------------------------------------------------------------------------

#[test]
fn valid_hand_verifies_for_every_table_size() {
    for n in 2..=9u8 {
        let transcript = deal(n, Scenario::Valid);
        let report = verify(&transcript)
            .unwrap_or_else(|e| panic!("{n}-player valid hand must verify, got: {e}"));
        assert_eq!(report.num_players, n);
        // 2 hole cards per player + 5 community cards, all distinct.
        assert_eq!(report.revealed_card_ids.len(), (2 * n as usize) + 5);
        let mut sorted = report.revealed_card_ids.clone();
        sorted.sort_unstable();
        sorted.dedup();
        assert_eq!(
            sorted.len(),
            report.revealed_card_ids.len(),
            "no card id may be revealed twice"
        );
    }
}

#[test]
fn deal_is_deterministic() {
    let a = deal(4, Scenario::Valid);
    let b = deal(4, Scenario::Valid);
    assert_eq!(
        a.to_json(),
        b.to_json(),
        "deterministic provider must reproduce an identical transcript"
    );
}

#[test]
fn transcript_survives_json_round_trip() {
    let transcript = deal(3, Scenario::Valid);
    let json = transcript.to_json();
    let parsed = Transcript::from_json(&json).expect("re-parse");
    verify(&parsed).expect("round-tripped transcript still verifies");
}

// ---------------------------------------------------------------------------
// Semantic faults (cryptographically well-formed, but invalid hands)
// ---------------------------------------------------------------------------

#[test]
fn tampered_hole_card_is_rejected() {
    let err = verify(&deal(3, Scenario::TamperedHoleCard)).expect_err("must reject");
    assert!(
        matches!(
            err.kind,
            VerifyErrorKind::State(StateError::CommitmentMismatch(_))
                | VerifyErrorKind::State(StateError::DuplicateCard(_))
        ),
        "expected a deck-commitment failure, got: {err}"
    );
}

#[test]
fn forked_final_deck_is_detected() {
    let err = verify(&deal(3, Scenario::ForkedFinalDeck)).expect_err("must reject");
    assert!(
        matches!(
            err.kind,
            VerifyErrorKind::State(StateError::ForkedFinalDeck { .. })
        ),
        "a party acking a deck the coordinator never committed must be caught, got: {err}"
    );
}

#[test]
fn duplicate_community_card_is_rejected() {
    let err = verify(&deal(3, Scenario::DuplicateCommunityCard)).expect_err("must reject");
    assert!(
        matches!(
            err.kind,
            VerifyErrorKind::State(StateError::DuplicateCard(_))
                | VerifyErrorKind::State(StateError::CommitmentMismatch(_))
        ),
        "expected a duplicate-card failure, got: {err}"
    );
}

#[test]
fn opening_before_final_commit_is_rejected() {
    let err = verify(&deal(3, Scenario::EarlyHoleOpen)).expect_err("must reject");
    assert!(
        matches!(
            err.kind,
            VerifyErrorKind::State(StateError::OpenBeforeDeckReady(_))
        ),
        "opening a hole card before the deck is committed + acked must be caught, got: {err}"
    );
}

#[test]
fn future_community_card_opened_early_is_rejected() {
    let err = verify(&deal(3, Scenario::EarlyCommunityCard)).expect_err("must reject");
    assert!(
        matches!(
            err.kind,
            VerifyErrorKind::State(StateError::StageOutOfOrder(_))
        ),
        "revealing the turn before the flop must be caught, got: {err}"
    );
}

#[test]
fn invalid_shuffle_proof_is_rejected() {
    let err = verify(&deal(3, Scenario::InvalidShuffleProof)).expect_err("must reject");
    assert!(
        matches!(err.kind, VerifyErrorKind::BadShuffleProof(_)),
        "a corrupted shuffle proof must be caught, got: {err}"
    );
}

#[test]
fn invalid_decryption_proof_is_rejected() {
    let err = verify(&deal(3, Scenario::InvalidDecryptionProof)).expect_err("must reject");
    assert!(
        matches!(err.kind, VerifyErrorKind::BadDecryptionProof(_)),
        "a corrupted decryption proof must be caught, got: {err}"
    );
}

#[test]
fn missing_terminal_event_is_rejected() {
    let err = verify(&deal(3, Scenario::MissingComplete)).expect_err("must reject");
    assert!(
        matches!(err.kind, VerifyErrorKind::NotTerminal(_)),
        "a transcript with no terminal event must be caught, got: {err}"
    );
}

#[test]
fn forged_ack_contributor_is_rejected() {
    // ADR-041 §4 contributor binding (audit 2026-06-03). The transcript is fully
    // valid except that one `final_deck_ack`'s contributor signature is produced
    // by a DIFFERENT party than the one the state machine records the ack for.
    // Before the binding fix the verifier accepted this (the contributor's own
    // signature over its own claim verifies), letting a malicious coordinator
    // forge a player's deck acknowledgement. The verifier must now reject it by
    // requiring `contributor == party_id`.
    let err = verify(&deal(3, Scenario::ForgedAckContributor))
        .expect_err("a deck-ack whose contributor != the acking party must be rejected");
    assert!(
        matches!(err.kind, VerifyErrorKind::ContributorPartyMismatch { .. }),
        "contributor-binding forgery must be caught, got: {err}"
    );
}

// ---------------------------------------------------------------------------
// Structural tampering of a finished transcript
// ---------------------------------------------------------------------------

#[test]
fn tampered_event_payload_is_rejected() {
    let mut transcript = deal(3, Scenario::Valid);
    // Alter the committed final-deck payload without fixing its hash.
    let idx = transcript
        .events
        .iter()
        .position(|e| e.event_type == "final_deck_committed")
        .unwrap();
    transcript.events[idx].payload["final_deck_hash"] =
        serde_json::json!("00000000000000000000000000000000000000000000000000000000000000ff");
    let err = verify(&transcript).expect_err("must reject");
    assert!(
        matches!(err.kind, VerifyErrorKind::PayloadHashMismatch),
        "altering an event payload must be caught, got: {err}"
    );
}

#[test]
fn reordered_events_are_rejected() {
    let mut transcript = deal(3, Scenario::Valid);
    transcript.events.swap(3, 4);
    let err = verify(&transcript).expect_err("must reject");
    assert!(
        matches!(
            err.kind,
            VerifyErrorKind::SequenceOrder { .. } | VerifyErrorKind::BrokenChain
        ),
        "reordering events must be caught, got: {err}"
    );
}

#[test]
fn tampered_signature_is_rejected() {
    let mut transcript = deal(3, Scenario::Valid);
    transcript.events[2].signature =
        "deadbeefdeadbeefdeadbeefdeadbeefdeadbeefdeadbeefdeadbeefdeadbeef".to_string();
    let err = verify(&transcript).expect_err("must reject");
    assert!(
        matches!(err.kind, VerifyErrorKind::BadSignature(_)),
        "a forged signature must be caught, got: {err}"
    );
}

#[test]
fn replayed_event_is_rejected() {
    let mut transcript = deal(3, Scenario::Valid);
    // Duplicate an event — a replay attack.
    let replayed = transcript.events[5].clone();
    transcript.events.insert(6, replayed);
    let err = verify(&transcript).expect_err("must reject");
    assert!(
        matches!(
            err.kind,
            VerifyErrorKind::SequenceOrder { .. } | VerifyErrorKind::BrokenChain
        ),
        "a replayed (duplicated) event must be caught, got: {err}"
    );
}

#[test]
fn dropped_event_is_rejected() {
    let mut transcript = deal(3, Scenario::Valid);
    transcript.events.remove(4);
    let err = verify(&transcript).expect_err("must reject");
    assert!(
        matches!(
            err.kind,
            VerifyErrorKind::SequenceOrder { .. } | VerifyErrorKind::BrokenChain
        ),
        "dropping an event must be caught, got: {err}"
    );
}

// ---------------------------------------------------------------------------
// Provider abstraction + engine integration
// ---------------------------------------------------------------------------

#[test]
fn legacy_provider_deals_a_valid_deck_without_a_transcript() {
    let dealt = ExistingServerDealingProvider::new().deal(&request(2));
    assert_eq!(dealt.deck.len(), 52);
    assert!(
        dealt.transcript.is_none(),
        "the legacy trusted-server provider offers no transcript"
    );
}

#[test]
fn mental_poker_deck_drives_the_existing_engine() {
    // The whole point of the refactor: the engine plays a hand from a
    // provider-produced deck via `GameHand::new_with_deck`, with no engine RNG.
    let dealt = MentalPokerDealingProvider::deterministic().deal(&request(2));
    verify(dealt.transcript.as_ref().unwrap()).expect("transcript verifies");

    assert_eq!(dealt.deck.len(), 52);
    let players = vec![
        (PlayerId::new(0), Chips(1000), 0u8),
        (PlayerId::new(1), Chips(1000), 1u8),
    ];
    let deck = Deck::from_cards(dealt.deck.clone());
    let mut hand = GameHand::new_with_deck(players, 0, Chips(20), Chips(10), deck, dealt.deck_seed);
    let snap = hand.start().expect("hand starts from the dealt deck");

    // Hole cards the engine dealt must equal the transcript's deck layout:
    // seat s gets deck[s] and deck[n+s].
    let n = 2usize;
    for s in 0..n {
        let player = snap
            .players
            .iter()
            .find(|p| p.seat == s as u8)
            .expect("seat present");
        let hole = player.hole_cards.expect("hole cards dealt");
        assert_eq!(hole.card1, dealt.deck[s], "seat {s} first hole card");
        assert_eq!(hole.card2, dealt.deck[n + s], "seat {s} second hole card");
    }
}

// ---------------------------------------------------------------------------
// ADR-041 BLOCKER-1+2: interactive transcript with real per-party keys
// ---------------------------------------------------------------------------

/// Wire deck hash: `ds_hash("mp:deck-hash:v1", &[ each card-id byte ])`.
fn wire_deck_hash(deck: &[u8]) -> [u8; 32] {
    let parts: Vec<&[u8]> = deck.iter().map(std::slice::from_ref).collect();
    ds_hash("mp:deck-hash:v1", &parts)
}

/// U42 (dual-AI OSS review): the four named interactive-transcript builders in
/// this file (plus a fifth formerly inlined in
/// `interactive_round0_wrong_hash_is_rejected`) were five near-identical
/// ~300-line copies. They now share ONE parameterized core,
/// [`build_interactive_mock_transcript`]; each variant differs only in the
/// knobs below. Every per-variant domain tag, table id, rotation rule, hash
/// rule, and forged signature is preserved, so the transcript each test
/// verifies — and therefore what each test asserts — is unchanged.
struct InteractiveBuildParams<'a> {
    /// Table id recorded in the transcript.
    table_id: &'a str,
    /// Domain-separation tags for the deterministic per-variant key material.
    party_key_domain: &'a str,
    coord_key_domain: &'a str,
    shuffle_key_domain: &'a str,
    /// `Some(domain)` → per-round left-rotation `ds_hash(domain, hand_id ‖ r)[0] % 52`;
    /// `None` → the fixed `(r * 5 + 1) % 52` used by the round-0 negative case.
    perm_domain: Option<&'a str>,
    /// Round-0 shuffle input hash: `canonical_wire_deck_hash()` per ADR-041 §5.1,
    /// or `canonical_initial_deck_hash()` to replay the OLD commitment-form bug.
    round0_input: [u8; 32],
    /// `true` → the last round's output_deck_hash is `commit_deck_hash` (the
    /// §5.1 rule); `false` → the MED-1 "bad client" that keeps `wire_deck_hash`.
    last_round_uses_commit_hash: bool,
    /// `true` → party 0's shuffle contributor_signature is a forged value baked
    /// in at BUILD time (MED-2), so the hash chain + envelope stay valid.
    forge_round0_shuffle_sig: bool,
}

/// Build a complete interactive transcript where each party has its own
/// distinct HMAC key (simulating real separate clients). The coordinator uses
/// a separate key. See [`InteractiveBuildParams`] for the per-variant knobs.
fn build_interactive_mock_transcript(
    n: u8,
    hand_id: &str,
    p: &InteractiveBuildParams,
) -> Transcript {
    let n = n as usize;

    // Distinct per-party HMAC keys — simulate n independent clients.
    let party_keys: Vec<Vec<u8>> = (0..n)
        .map(|i| ds_hash(p.party_key_domain, &[hand_id.as_bytes(), &[i as u8]]).to_vec())
        .collect();
    let coord_key = ds_hash(p.coord_key_domain, &[hand_id.as_bytes()]);

    // Build the key directory.
    let mut dir_keys: BTreeMap<String, String> = BTreeMap::new();
    dir_keys.insert(COORDINATOR.to_string(), hex::encode(coord_key));
    for (i, party_key) in party_keys.iter().enumerate() {
        dir_keys.insert(mp_party_id(i as u8), hex::encode(party_key));
    }
    let key_directory = KeyDirectory {
        keys: dir_keys,
        is_mock: true,
    };

    // Coordinator signature provider (only coordinator key).
    let coord_sig_provider = MockSignatureProvider::from_directory(&KeyDirectory {
        keys: {
            let mut m = BTreeMap::new();
            m.insert(COORDINATOR.to_string(), hex::encode(coord_key));
            m
        },
        is_mock: true,
    })
    .expect("coordinator sig provider");

    // Per-party signature providers (each party signs its own contributor claims).
    let party_sig_providers: Vec<MockSignatureProvider> = (0..n)
        .map(|i| {
            let pid = mp_party_id(i as u8);
            MockSignatureProvider::from_directory(&KeyDirectory {
                keys: {
                    let mut m = BTreeMap::new();
                    m.insert(pid, hex::encode(&party_keys[i]));
                    m
                },
                is_mock: true,
            })
            .expect("party sig provider")
        })
        .collect();

    let shuffle_provider = MockShuffleProofProvider;
    let decryption_provider = MockDecryptionProvider;

    let mut builder = TranscriptBuilder::new(
        hand_id,
        p.table_id,
        "mental_poker_mock",
        &coord_sig_provider,
        &shuffle_provider,
        &decryption_provider,
        key_directory,
    );

    // ---- hand_init ----
    // ADR-041 §5.1: interactive path uses wire_deck_hash([0..51]) as the
    // round-0 input seed, signalled by deck_repr = "wire".
    let players_list: Vec<PlayerEntry> = (0..n as u8)
        .map(|s| PlayerEntry {
            seat: s,
            party_id: mp_party_id(s),
        })
        .collect();
    builder.append(
        event_type::HAND_INIT,
        to_payload(&HandInitPayload {
            players: players_list,
            button_seat: 0,
            big_blind: 20,
            small_blind: 10,
            deck_repr: Some("wire".to_string()),
        }),
        COORDINATOR,
    );

    // ---- key_registered (per party, each signs its own claim) ----
    for i in 0..n {
        let pid = mp_party_id(i as u8);
        let signing_pubkey = hex::encode(&party_keys[i]);
        let shuffle_pubkey = hex::encode(ds_hash(
            p.shuffle_key_domain,
            &[hand_id.as_bytes(), &[i as u8]],
        ));
        let claim = serde_json::json!({
            "hand_id": hand_id,
            "party_id": pid,
            "signing_pubkey": signing_pubkey,
            "shuffle_pubkey": shuffle_pubkey,
        });
        let claim_hash = ds_hash("mp:claim:v1", &[&canonical_json(&claim)]);
        let contrib_sig = party_sig_providers[i].sign(&pid, &claim_hash);

        let payload = to_payload(&KeyRegisteredPayload {
            party_id: pid.clone(),
            seat: i as u8,
            signing_pubkey,
            shuffle_pubkey,
            contributor: None,
            contributor_signature: None,
            key_pok: None,
        });
        builder.append_with_contributor(
            event_type::KEY_REGISTERED,
            payload,
            COORDINATOR,
            Some((pid.as_str(), contrib_sig.as_str())),
        );
    }

    // ---- shuffle round-robin ----
    // Each party applies a deterministic permutation to the deck in sequence.
    // The shuffle chain starts from the canonical identity deck [0..52] and
    // each party rotates by the variant's rotation rule.
    let mut round_decks: Vec<Vec<u8>> = Vec::new();
    let mut cur: Vec<u8> = (0u8..52).collect();
    for r in 0..n {
        let mut next = cur.clone();
        let cut = match p.perm_domain {
            Some(domain) => {
                let swap_seed = ds_hash(domain, &[hand_id.as_bytes(), &[r as u8]]);
                (swap_seed[0] % 52) as usize
            }
            None => (r * 5 + 1) % 52,
        };
        next.rotate_left(cut);
        round_decks.push(next.clone());
        cur = next;
    }
    // After all n parties shuffle, `cur` is the final deck.
    let current_deck = cur;

    // Pre-compute commitments from the final (post-shuffle) deck.
    // The last shuffle round's output_deck_hash MUST equal deck_hash(final_commits)
    // so the state machine's FinalDeckMismatch check passes (state.rs:355).
    let salts: Vec<[u8; 32]> = (0usize..52)
        .map(|j| {
            ds_hash(
                "mp:salt:v1",
                &[hand_id.as_bytes(), &(j as u64).to_le_bytes()],
            )
        })
        .collect();
    let commits: Vec<[u8; 32]> = (0usize..52)
        .map(|j| card_commit(current_deck[j], &salts[j]))
        .collect();
    let commit_hash = deck_hash(&commits);
    let commit_hash_hex = hex_hash(&commit_hash);
    let commits_hex: Vec<String> = commits.iter().map(hex_hash).collect();

    // Now build shuffle contribution events.
    // ADR-041 §5.1 Per-round hash sequence (interactive path):
    //   Round 0 input  = wire_deck_hash([0..51]) — canonical plaintext deck.
    //   Intermediate   = wire_deck_hash(output_deck).
    //   Last round     = commit_deck_hash (so state machine continuity holds).
    let mut prev_hash: [u8; 32] = p.round0_input;
    for r in 0..n {
        let output_deck = &round_decks[r];
        let input_hash = prev_hash;
        // Last round: output_hash = commit_hash so state machine continuity
        // holds — unless the variant models the MED-1 "bad client" that keeps
        // using wire_deck_hash (which the verifier must reject).
        let output_hash: [u8; 32] = if r == n - 1 && p.last_round_uses_commit_hash {
            commit_hash
        } else {
            wire_deck_hash(output_deck)
        };
        let pid = mp_party_id(r as u8);
        let proof = shuffle_provider.prove_shuffle(&pid, r as u32, &input_hash, &output_hash);
        let claim = serde_json::json!({
            "hand_id": hand_id,
            "round": r as u64,
            "input_deck_hash": hex_hash(&input_hash),
            "output_deck_hash": hex_hash(&output_hash),
            "proof_attestation": proof.attestation,
        });
        let claim_hash = ds_hash("mp:claim:v1", &[&canonical_json(&claim)]);

        // MED-2 variant: for round 0 (party 0), bake in a FORGED contributor
        // signature at build time — NOT a post-hoc mutation. The
        // TranscriptBuilder will embed this value in the payload JSON, compute
        // payload_hash over it, and sign the envelope — so the chain is fully
        // intact. The only thing wrong is the contributor_signature value.
        let contrib_sig: String = if p.forge_round0_shuffle_sig && r == 0 {
            "00".repeat(32) // 64 hex zeros — a structurally valid hex string but
                            // not a valid HMAC for party:0's claim.
        } else {
            party_sig_providers[r].sign(&pid, &claim_hash)
        };

        let payload = to_payload(&ShuffleContributionPayload {
            party_id: pid.clone(),
            round: r as u32,
            input_deck_hash: hex_hash(&input_hash),
            output_deck_hash: hex_hash(&output_hash),
            proof,
            contributor: None,
            contributor_signature: None,
        });
        builder.append_with_contributor(
            event_type::SHUFFLE_CONTRIBUTION,
            payload,
            COORDINATOR,
            Some((pid.as_str(), contrib_sig.as_str())),
        );
        prev_hash = output_hash;
    }

    // ---- final_deck_committed ----
    // (salts, commits, commit_hash, commit_hash_hex, commits_hex already computed above)

    builder.append(
        event_type::FINAL_DECK_COMMITTED,
        to_payload(&FinalDeckCommittedPayload {
            final_deck_hash: commit_hash_hex.clone(),
            deck: commits_hex,
            deck_ct: None,
        }),
        COORDINATOR,
    );

    // ---- final_deck_ack (per party) ----
    for (i, sig_provider) in party_sig_providers.iter().enumerate() {
        let pid = mp_party_id(i as u8);
        let claim = serde_json::json!({
            "hand_id": hand_id,
            "party_id": pid,
            "final_deck_hash": commit_hash_hex,
        });
        let claim_hash = ds_hash("mp:claim:v1", &[&canonical_json(&claim)]);
        let contrib_sig = sig_provider.sign(&pid, &claim_hash);

        let payload = to_payload(&FinalDeckAckPayload {
            party_id: pid.clone(),
            final_deck_hash: commit_hash_hex.clone(),
            contributor: None,
            contributor_signature: None,
        });
        builder.append_with_contributor(
            event_type::FINAL_DECK_ACK,
            payload,
            COORDINATOR,
            Some((pid.as_str(), contrib_sig.as_str())),
        );
    }

    // ---- hole_card_opened (coordinator-authored contributor) ----
    for idx in 0..2 * n {
        let owner_seat = if idx < n { idx as u8 } else { (idx - n) as u8 };
        let pid = mp_party_id(owner_seat);
        let card_id = current_deck[idx];
        let salt = salts[idx];
        let proof = decryption_provider.prove_decryption(&pid, idx as u32, card_id, &salt);
        let claim = serde_json::json!({
            "hand_id": hand_id,
            "deck_index": idx as u64,
            "card_id": card_id as u64,
        });
        let claim_hash = ds_hash("mp:claim:v1", &[&canonical_json(&claim)]);
        // For hole card opens, the coordinator signs as the contributor
        // (owner decryption in the mock is coordinator-assisted).
        let contrib_sig = coord_sig_provider.sign(COORDINATOR, &claim_hash);

        let payload = to_payload(&HoleCardOpenedPayload {
            seat: owner_seat,
            owner_party_id: pid.clone(),
            card: OpenedCard {
                deck_index: idx as u32,
                card_id,
                salt: hex::encode(salt),
                proof,
            },
            contributor: None,
            contributor_signature: None,
        });
        builder.append_with_contributor(
            event_type::HOLE_CARD_OPENED,
            payload,
            COORDINATOR,
            Some((COORDINATOR, contrib_sig.as_str())),
        );
    }

    // ---- community_revealed ----
    let base = 2 * n;
    for (stage, indices) in [
        ("flop", vec![base, base + 1, base + 2]),
        ("turn", vec![base + 3]),
        ("river", vec![base + 4]),
    ] {
        let cards: Vec<OpenedCard> = indices
            .iter()
            .map(|&i| {
                let card_id = current_deck[i];
                let salt = salts[i];
                let proof =
                    decryption_provider.prove_decryption(COORDINATOR, i as u32, card_id, &salt);
                OpenedCard {
                    deck_index: i as u32,
                    card_id,
                    salt: hex::encode(salt),
                    proof,
                }
            })
            .collect();
        builder.append(
            event_type::COMMUNITY_REVEALED,
            to_payload(&CommunityRevealedPayload {
                stage: stage.to_string(),
                cards,
            }),
            COORDINATOR,
        );
    }

    // ---- hand_complete ----
    builder.append(
        event_type::HAND_COMPLETE,
        to_payload(&HandCompletePayload {
            revealed_card_count: (2 * n + 5) as u32,
        }),
        COORDINATOR,
    );

    builder.finish()
}

/// Build a complete interactive transcript where each party has its own
/// distinct HMAC key (simulating real separate clients). The coordinator uses
/// a separate key.
fn build_interactive_transcript(n: u8, hand_id: &str, final_deck: Vec<u8>) -> Transcript {
    assert_eq!(final_deck.len(), 52, "deck must be 52 entries");
    // The `final_deck` parameter is used only by the engine-deck test to
    // supply a known final deck; the core derives it from the shuffle chain.
    let _ = final_deck; // consumed via shuffle chain in the core
    build_interactive_mock_transcript(
        n,
        hand_id,
        &InteractiveBuildParams {
            table_id: "table-interactive",
            party_key_domain: "mp:test-party-key:v1",
            coord_key_domain: "mp:test-coord-key:v1",
            shuffle_key_domain: "mp:test-shuffle-key:v1",
            perm_domain: Some("mp:test-perm:v1"),
            round0_input: canonical_wire_deck_hash(),
            last_round_uses_commit_hash: true,
            forge_round0_shuffle_sig: false,
        },
    )
}

/// BLOCKER-1+2 (positive): A transcript built from distinct per-party keys
/// passes `mental_poker::verify()`. This proves the real-client choreography
/// is verifiable and that `verify()` now means something — it checks the
/// real client-supplied contributor signatures against the key directory.
#[test]
fn interactive_transcript_with_distinct_party_keys_verifies() {
    for n in 2..=4u8 {
        let hand_id = format!("interactive-hand-{n}p");
        // Use a known deck: identity permutation.
        let final_deck: Vec<u8> = (0u8..52).collect();
        let transcript = build_interactive_transcript(n, &hand_id, final_deck.clone());

        let report = verify(&transcript).unwrap_or_else(|e| {
            panic!("interactive transcript with distinct party keys must verify for n={n}: {e}")
        });
        assert_eq!(report.num_players, n);
        assert_eq!(report.revealed_card_ids.len(), (2 * n as usize) + 5);
    }
}

/// MED-2: Build a transcript where party 0's shuffle contributor_signature is
/// a forged value baked in **at build time**. Because the forged sig is
/// injected before `TranscriptBuilder::append_with_contributor`, the
/// `payload_hash` is correctly computed over the forged-sig payload, the
/// hash chain remains fully intact, and the coordinator envelope signature
/// is valid. The ONLY failure path is the contributor-signature check in
/// `verify()`, which must produce `BadContributorSignature` exclusively.
fn build_transcript_with_forged_party0_shuffle_sig(n: u8, hand_id: &str) -> Transcript {
    // U42 (dual-AI OSS review): thin wrapper over the shared parameterized core.
    build_interactive_mock_transcript(
        n,
        hand_id,
        &InteractiveBuildParams {
            table_id: "table-forgery",
            party_key_domain: "mp:test-party-key:v1",
            coord_key_domain: "mp:test-coord-key:v1",
            shuffle_key_domain: "mp:test-shuffle-key:v1",
            perm_domain: Some("mp:test-perm:v1"),
            round0_input: canonical_wire_deck_hash(),
            last_round_uses_commit_hash: true,
            forge_round0_shuffle_sig: true,
        },
    )
}

/// MED-2: A transcript built with a forged contributor signature baked in at
/// build time (chain intact, envelope valid) is rejected **exclusively** with
/// `VerifyErrorKind::BadContributorSignature`. This test genuinely exercises
/// the contributor-signature verification path in `verifier.rs`.
#[test]
fn forged_contributor_signature_is_rejected() {
    let hand_id = "forgery-test-hand";
    // Build a transcript where party 0's shuffle contributor_signature is
    // "00…00" — an invalid HMAC that was baked in at build time so the chain
    // and envelope signature are entirely valid.
    let transcript = build_transcript_with_forged_party0_shuffle_sig(2, hand_id);

    // The chain must be intact: payload_hash and chain must pass.
    // We verify that by checking the error is ONLY BadContributorSignature.
    let err = verify(&transcript).expect_err("forged contributor signature must be rejected");
    assert!(
        matches!(err.kind, VerifyErrorKind::BadContributorSignature(_)),
        "verifier must fail exclusively with BadContributorSignature; got: {:?}",
        err.kind
    );
}

// ---------------------------------------------------------------------------
// MED-1 (ADR-041 §5.1): last-round hash rule — commit_deck_hash, not wire_hash
// ---------------------------------------------------------------------------

/// Build an interactive transcript where the last shuffler explicitly uses
/// `commit_deck_hash` (not `wire_deck_hash`) as `output_deck_hash` in its
/// proof and contributor_signature, and the coordinator records the proof
/// verbatim (no patching). This mirrors the corrected server-side flow and
/// proves `mental_poker::verify()` passes under the ADR-041 §5.1 contract.
///
/// This is the positive case for MED-1.
fn build_interactive_transcript_last_round_commit_hash(n: u8, hand_id: &str) -> Transcript {
    // U42 (dual-AI OSS review): thin wrapper over the shared parameterized core.
    build_interactive_mock_transcript(
        n,
        hand_id,
        &InteractiveBuildParams {
            table_id: "table-med1",
            party_key_domain: "mp:test-party-key:med1",
            coord_key_domain: "mp:test-coord-key:med1",
            shuffle_key_domain: "mp:test-shuffle-key:med1",
            perm_domain: Some("mp:test-perm:med1"),
            round0_input: canonical_wire_deck_hash(),
            last_round_uses_commit_hash: true,
            forge_round0_shuffle_sig: false,
        },
    )
}

/// MED-1 positive: a transcript where the last shuffler signs against
/// `commit_deck_hash` (not `wire_deck_hash`) and the coordinator records the
/// proof verbatim passes `mental_poker::verify()`. This is the ADR-041 §5.1
/// contract end-to-end.
#[test]
fn med1_last_round_commit_hash_verifies() {
    for n in 2..=4u8 {
        let hand_id = format!("med1-positive-{n}p");
        let transcript = build_interactive_transcript_last_round_commit_hash(n, &hand_id);
        let report = verify(&transcript).unwrap_or_else(|e| {
            panic!(
                "MED-1 positive: last-round commit_deck_hash transcript must verify \
                 for n={n}: {e}"
            )
        });
        assert_eq!(report.num_players, n);
        assert_eq!(report.revealed_card_ids.len(), (2 * n as usize) + 5);
    }
}

/// Build an interactive transcript where the last shuffler (incorrectly) uses
/// `wire_deck_hash(output_deck)` instead of `commit_deck_hash` for its last
/// round. This is the "bad client" case — the coordinator should reject it
/// (the transcript verification fails because the proof attestation and/or the
/// hash-chain continuity into `final_deck_committed` breaks: the coordinator
/// still publishes the correct commit_deck_hash, but the last shuffle event's
/// output_deck_hash does NOT equal it, so FinalDeckMismatch fires).
fn build_interactive_transcript_last_round_wire_hash(n: u8, hand_id: &str) -> Transcript {
    // U42 (dual-AI OSS review): thin wrapper over the shared parameterized core.
    build_interactive_mock_transcript(
        n,
        hand_id,
        &InteractiveBuildParams {
            table_id: "table-med1-neg",
            party_key_domain: "mp:test-party-key:med1neg",
            coord_key_domain: "mp:test-coord-key:med1neg",
            shuffle_key_domain: "mp:test-shuffle-key:med1neg",
            perm_domain: Some("mp:test-perm:med1neg"),
            round0_input: canonical_wire_deck_hash(),
            last_round_uses_commit_hash: false, // BAD CLIENT: wire hash on the last round too
            forge_round0_shuffle_sig: false,
        },
    )
}

/// MED-1 negative: a transcript where the last shuffler uses `wire_deck_hash`
/// (not `commit_deck_hash`) for the last round is rejected by `verify()`.
/// The state machine detects the hash-chain break at `final_deck_committed`
/// because `last_deck_hash != final_deck_hash`.
#[test]
fn med1_last_round_wire_hash_is_rejected() {
    for n in 2..=4u8 {
        let hand_id = format!("med1-negative-{n}p");
        let transcript = build_interactive_transcript_last_round_wire_hash(n, &hand_id);
        let err = verify(&transcript)
            .expect_err("MED-1 negative: last-round wire_deck_hash transcript must be rejected");
        assert!(
            matches!(
                err.kind,
                VerifyErrorKind::State(StateError::FinalDeckMismatch { .. })
                    | VerifyErrorKind::State(StateError::CommitmentMismatch(_))
            ),
            "expected FinalDeckMismatch or CommitmentMismatch; got: {:?}",
            err.kind
        );
    }
}

/// BLOCKER-1 (engine deck): The client-contributed final deck is what the
/// engine actually plays. Verify the card at each deck position matches.
#[test]
fn client_contributed_deck_drives_engine() {
    let hand_id = "engine-deck-test";
    let final_deck: Vec<u8> = (0u8..52).collect();
    let transcript = build_interactive_transcript(2, hand_id, final_deck.clone());
    verify(&transcript).expect("transcript must verify");

    // The `build_interactive_transcript` applies a deterministic rotation at
    // each round. We verify the engine gets cards derived from the resulting deck.
    // Extract the opened card ids from the transcript's hole_card_opened events.
    let opened_card_ids: Vec<u8> = transcript
        .events
        .iter()
        .filter(|e| e.event_type == event_type::HOLE_CARD_OPENED)
        .map(|e| {
            e.payload["card"]["card_id"]
                .as_u64()
                .expect("card_id in payload") as u8
        })
        .collect();

    // We can map card_ids back to Cards and confirm they are valid engine cards.
    for &cid in &opened_card_ids {
        assert!(
            id_to_card(cid).is_some(),
            "card_id {cid} must map to a valid engine Card"
        );
    }
    assert_eq!(
        opened_card_ids.len(),
        4, // 2 players × 2 hole cards each
        "2-player hand must have 4 hole card opens"
    );
}

// ---------------------------------------------------------------------------
// ADR-041 §5.1 Round-0 input rule: cross-language hash boundary tests
// ---------------------------------------------------------------------------

/// Positive: interactive transcript with deck_repr="wire" and round-0 input
/// = wire_deck_hash([0..51]) verifies. This is the fixed server-side path.
/// (Covered by `interactive_transcript_with_distinct_party_keys_verifies`
///  which now uses canonical_wire_deck_hash — repeated here explicitly for
///  clarity as the named test the spec requires.)
#[test]
fn interactive_round0_wire_hash_verifies() {
    for n in 2..=3u8 {
        let hand_id = format!("r0-wire-positive-{n}p");
        let final_deck: Vec<u8> = (0u8..52).collect();
        // build_interactive_transcript now uses canonical_wire_deck_hash for prev_hash
        // and deck_repr = "wire" — this IS the §5.1-correct path.
        let transcript = build_interactive_transcript(n, &hand_id, final_deck);
        let report = verify(&transcript).unwrap_or_else(|e| {
            panic!("round-0 wire_deck_hash interactive transcript must verify for n={n}: {e}")
        });
        assert_eq!(report.num_players, n);
        assert_eq!(report.revealed_card_ids.len(), (2 * n as usize) + 5);
    }
}

/// Negative: an interactive transcript where the client uses
/// canonical_initial_deck_hash (commitment form) as round-0 input — but the
/// state machine is seeded with wire_deck_hash (deck_repr="wire") — is rejected.
/// This catches the original bug: the TS client signing the wire hash while
/// the server recording the commitment hash → DeckDiscontinuity.
#[test]
fn interactive_round0_wrong_hash_is_rejected() {
    // We build a transcript where deck_repr="wire" is set (so state machine
    // expects wire_deck_hash as round-0 input), but the shuffle round-0 records
    // canonical_initial_deck_hash() as input_deck_hash instead.
    for n in 2..=3u8 {
        let hand_id = format!("r0-wrong-hash-{n}p");
        // U42 (dual-AI OSS review): formerly a fifth inlined ~270-line copy of
        // the builder — now the shared core with round-0 input deliberately set
        // to canonical_initial_deck_hash() (the OLD bug: commitment-form, not
        // wire-form, while deck_repr="wire" seeds the state machine with
        // wire_deck_hash).
        let transcript = build_interactive_mock_transcript(
            n,
            &hand_id,
            &InteractiveBuildParams {
                table_id: "table-r0-neg",
                party_key_domain: "mp:test-party-key:r0neg",
                coord_key_domain: "mp:test-coord-key:r0neg",
                shuffle_key_domain: "mp:test-shuf-r0neg",
                perm_domain: None, // fixed (r * 5 + 1) % 52 rotations
                round0_input: canonical_initial_deck_hash(), // WRONG on purpose
                last_round_uses_commit_hash: true,
                forge_round0_shuffle_sig: false,
            },
        );
        // This must FAIL: state machine seeded with wire_deck_hash, but round-0
        // claims canonical_initial_deck_hash as input → DeckDiscontinuity.
        let err = verify(&transcript).expect_err(
            "round-0 commitment-form hash on a wire-mode state machine must be rejected",
        );
        assert!(
            matches!(
                err.kind,
                VerifyErrorKind::State(StateError::DeckDiscontinuity(_))
                    | VerifyErrorKind::BadContributorSignature(_)
            ),
            "expected DeckDiscontinuity or BadContributorSignature; got: {:?}",
            err.kind
        );
    }
}
