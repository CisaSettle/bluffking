//! Phase-4 server-blind cryptographic integration tests: reenc binding.

use super::*;

/// Build a COMPLETE real re-encryption-shuffle (`deck_repr = "reenc"`) transcript:
/// DKG → n REAL verifiable shuffles (each chained by `ec::deck_hash` v2 of its
/// ciphertext deck) → `final_deck_committed` carrying the verified-shuffle output
/// as BOTH `final_deck_hash` (= `ec::deck_hash(final_ct)`) and `deck_ct` → n-of-n
/// threshold opens of every dealt index, anchored to `final_ct[idx]`. Returns the
/// finished transcript, the genuine committed ciphertext deck, the joint key, and
/// the coordinator signer (so a regression can mutate + re-sign).
struct ReencFixture {
    transcript: mental_poker::Transcript,
    final_ct: Vec<Ct>,
    joint_key: RistrettoPoint,
    coord_sig: mental_poker::signing::MockSignatureProvider,
    /// The genuine DKG parties (their secret shares) — so a regression can
    /// re-anchor threshold opens to a substituted ciphertext deck.
    parties: Vec<mental_poker::crypto_real::dkg::DkgParty>,
}

fn build_real_reenc_fixture(n: usize) -> ReencFixture {
    use mental_poker::crypto::card_commit;
    use mental_poker::crypto_real::decrypt::{
        encode_threshold_attestation, ThresholdOpenWire, SCHEME as DECRYPT_SCHEME,
    };
    use mental_poker::crypto_real::dkg::schnorr_prove;
    use mental_poker::crypto_real::ec::{
        canonical_starting_deck, card_id_from_point, deck_hash as ct_deck_hash,
    };
    use mental_poker::crypto_real::shuffle::{RealShuffleProofProvider, Shuffle};
    use mental_poker::events::{
        event_type, party_id as mp_party_id, CommunityRevealedPayload, FinalDeckAckPayload,
        FinalDeckCommittedPayload, HandCompletePayload, HandInitPayload, HoleCardOpenedPayload,
        KeyRegisteredPayload, OpenedCard, PlayerEntry, ShuffleContributionPayload, COORDINATOR,
    };
    use mental_poker::hash::{canonical_json, ds_hash, hex_hash};
    use mental_poker::signing::{KeyDirectory, MockSignatureProvider, SignatureProvider};
    use mental_poker::transcript::{to_payload, TranscriptBuilder};
    use std::collections::BTreeMap;

    let mut rng = OsRng;
    let hand_id = "hand-reenc-anchor";
    let table_id = "table-reenc-anchor";

    let run = DkgRun::simulate(n, &mut rng);
    let q = run.joint_key;
    let qi_hex: Vec<String> = run.parties.iter().map(|p| point_to_hex(&p.q_i)).collect();

    // Mock signing key directory (per-party + coordinator).
    let party_keys: Vec<Vec<u8>> = (0..n)
        .map(|i| ds_hash("mp:test-party-key:v1", &[hand_id.as_bytes(), &[i as u8]]).to_vec())
        .collect();
    let coord_key = ds_hash("mp:test-coord-key:v1", &[hand_id.as_bytes()]);
    let mut dir_keys: BTreeMap<String, String> = BTreeMap::new();
    dir_keys.insert(COORDINATOR.to_string(), hex::encode(coord_key));
    for (i, k) in party_keys.iter().enumerate() {
        dir_keys.insert(mp_party_id(i as u8), hex::encode(k));
    }
    let key_directory = KeyDirectory {
        keys: dir_keys,
        is_mock: true,
    };
    let coord_sig = MockSignatureProvider::from_directory(&KeyDirectory {
        keys: {
            let mut m = BTreeMap::new();
            m.insert(COORDINATOR.to_string(), hex::encode(coord_key));
            m
        },
        is_mock: true,
    })
    .unwrap();
    let party_sigs: Vec<MockSignatureProvider> = (0..n)
        .map(|i| {
            MockSignatureProvider::from_directory(&KeyDirectory {
                keys: {
                    let mut m = BTreeMap::new();
                    m.insert(mp_party_id(i as u8), hex::encode(&party_keys[i]));
                    m
                },
                is_mock: true,
            })
            .unwrap()
        })
        .collect();

    // REAL re-encryption shuffle chain (the F1 server-blind shuffle). Each round's
    // proof is built by a real prover; the chain hashes are `ec::deck_hash` v2.
    let mut deck = canonical_starting_deck();
    let mut shuffles: Vec<mental_poker::crypto::ShuffleProof> = Vec::with_capacity(n);
    let mut chain: Vec<([u8; 32], [u8; 32])> = Vec::with_capacity(n); // (input_hash, output_hash)
    for r in 0..n {
        let input = deck.clone();
        let s = Shuffle::perform(input.clone(), &q, &mut rng);
        let ih = ct_deck_hash(&input);
        let oh = ct_deck_hash(&s.output);
        let proof = s.prove(&mp_party_id(r as u8), r as u32, &mut rng);
        shuffles.push(proof);
        chain.push((ih, oh));
        deck = s.output;
    }
    let final_ct = deck; // the verified-shuffle output == the committed ciphertext deck.

    // Salts + recovered card ids per index (the open-anchor / commitment layer).
    let salts: Vec<[u8; 32]> = (0usize..52)
        .map(|j| {
            ds_hash(
                "mp:salt:v1",
                &[hand_id.as_bytes(), &(j as u64).to_le_bytes()],
            )
        })
        .collect();
    let card_ids: Vec<u8> = final_ct
        .iter()
        .map(|ct| {
            let secret_sum: RistrettoPoint = run.parties.iter().map(|p| p.x_i * ct.c1).sum();
            let m: RistrettoPoint = ct.c2 - secret_sum;
            card_id_from_point(&m).expect("every committed ciphertext opens to a valid card id")
        })
        .collect();
    let commits: Vec<[u8; 32]> = (0..52)
        .map(|j| card_commit(card_ids[j], &salts[j]))
        .collect();
    let commits_hex: Vec<String> = commits.iter().map(hex_hash).collect();

    // On the reenc path `final_deck_hash` == the ciphertext-deck `ec::deck_hash` v2
    // of the verified-shuffle output (== `last_deck_hash` in the state machine).
    let final_hash_hex = hex_hash(&ct_deck_hash(&final_ct));

    let make_open = |idx: usize, rng: &mut OsRng| -> mental_poker::crypto::DecryptionProof {
        let shares = run
            .parties
            .iter()
            .map(|p| {
                mental_poker::crypto_real::decrypt::partial_decrypt(
                    p,
                    idx as u32,
                    &final_ct[idx],
                    rng,
                )
            })
            .collect();
        mental_poker::crypto::DecryptionProof {
            scheme: DECRYPT_SCHEME.to_string(),
            attestation: encode_threshold_attestation(&ThresholdOpenWire {
                ct: final_ct[idx].to_wire(),
                threshold: mental_poker::crypto_real::decrypt::ThresholdDecryptionProof {
                    scheme: DECRYPT_SCHEME.to_string(),
                    shares,
                },
            }),
        }
    };

    // The builder only reads provider labels (the real schemes), not their proofs.
    let shuffle_label = RealShuffleProofProvider::verifier();
    let decrypt_label = mental_poker::crypto_real::decrypt::RealThresholdDecryptionProvider::new(
        run.parties
            .iter()
            .map(|p| (p.party_id.clone(), p.q_i))
            .collect(),
    );
    let mut builder = TranscriptBuilder::new(
        hand_id,
        table_id,
        "mental_poker_mock",
        &coord_sig,
        &shuffle_label,
        &decrypt_label,
        key_directory,
    );

    let players: Vec<PlayerEntry> = (0..n as u8)
        .map(|s| PlayerEntry {
            seat: s,
            party_id: mp_party_id(s),
        })
        .collect();
    builder.append(
        event_type::HAND_INIT,
        to_payload(&HandInitPayload {
            players,
            button_seat: 0,
            big_blind: 20,
            small_blind: 10,
            deck_repr: Some("reenc".to_string()),
        }),
        COORDINATOR,
    );

    for i in 0..n {
        let pid = mp_party_id(i as u8);
        let signing_pubkey = hex::encode(&party_keys[i]);
        let shuffle_pubkey = qi_hex[i].clone();
        let claim = serde_json::json!({
            "hand_id": hand_id, "party_id": pid,
            "signing_pubkey": signing_pubkey, "shuffle_pubkey": shuffle_pubkey,
        });
        let claim_hash = ds_hash("mp:claim:v1", &[&canonical_json(&claim)]);
        let csig = party_sigs[i].sign(&pid, &claim_hash);
        let pok = schnorr_prove(&pid, &run.parties[i].x_i, &run.parties[i].q_i, &mut rng);
        builder.append_with_contributor(
            event_type::KEY_REGISTERED,
            to_payload(&KeyRegisteredPayload {
                party_id: pid.clone(),
                seat: i as u8,
                signing_pubkey,
                shuffle_pubkey,
                contributor: None,
                contributor_signature: None,
                key_pok: Some(pok),
            }),
            COORDINATOR,
            Some((pid.as_str(), csig.as_str())),
        );
    }

    for r in 0..n {
        let pid = mp_party_id(r as u8);
        let (ih, oh) = chain[r];
        let proof = shuffles[r].clone();
        let claim = serde_json::json!({
            "hand_id": hand_id, "round": r as u64,
            "input_deck_hash": hex_hash(&ih),
            "output_deck_hash": hex_hash(&oh),
            "proof_attestation": proof.attestation,
        });
        let claim_hash = ds_hash("mp:claim:v1", &[&canonical_json(&claim)]);
        let csig = party_sigs[r].sign(&pid, &claim_hash);
        builder.append_with_contributor(
            event_type::SHUFFLE_CONTRIBUTION,
            to_payload(&ShuffleContributionPayload {
                party_id: pid.clone(),
                round: r as u32,
                input_deck_hash: hex_hash(&ih),
                output_deck_hash: hex_hash(&oh),
                proof,
                contributor: None,
                contributor_signature: None,
            }),
            COORDINATOR,
            Some((pid.as_str(), csig.as_str())),
        );
    }

    builder.append(
        event_type::FINAL_DECK_COMMITTED,
        to_payload(&FinalDeckCommittedPayload {
            final_deck_hash: final_hash_hex.clone(),
            deck: commits_hex,
            deck_ct: Some(final_ct.iter().map(|c| c.to_wire()).collect()),
        }),
        COORDINATOR,
    );

    for (i, party_sig) in party_sigs.iter().enumerate() {
        let pid = mp_party_id(i as u8);
        let claim = serde_json::json!({
            "hand_id": hand_id, "party_id": pid, "final_deck_hash": final_hash_hex,
        });
        let claim_hash = ds_hash("mp:claim:v1", &[&canonical_json(&claim)]);
        let csig = party_sig.sign(&pid, &claim_hash);
        builder.append_with_contributor(
            event_type::FINAL_DECK_ACK,
            to_payload(&FinalDeckAckPayload {
                party_id: pid.clone(),
                final_deck_hash: final_hash_hex.clone(),
                contributor: None,
                contributor_signature: None,
            }),
            COORDINATOR,
            Some((pid.as_str(), csig.as_str())),
        );
    }

    for idx in 0..2 * n {
        let owner_seat = if idx < n { idx as u8 } else { (idx - n) as u8 };
        let pid = mp_party_id(owner_seat);
        let claim = serde_json::json!({
            "hand_id": hand_id, "deck_index": idx as u64, "card_id": card_ids[idx] as u64,
        });
        let claim_hash = ds_hash("mp:claim:v1", &[&canonical_json(&claim)]);
        let csig = coord_sig.sign(COORDINATOR, &claim_hash);
        builder.append_with_contributor(
            event_type::HOLE_CARD_OPENED,
            to_payload(&HoleCardOpenedPayload {
                seat: owner_seat,
                owner_party_id: pid.clone(),
                card: OpenedCard {
                    deck_index: idx as u32,
                    card_id: card_ids[idx],
                    salt: hex::encode(salts[idx]),
                    proof: make_open(idx, &mut rng),
                },
                contributor: None,
                contributor_signature: None,
            }),
            COORDINATOR,
            Some((COORDINATOR, csig.as_str())),
        );
    }

    let base = 2 * n;
    for (stage, indices) in [
        ("flop", vec![base, base + 1, base + 2]),
        ("turn", vec![base + 3]),
        ("river", vec![base + 4]),
    ] {
        let cards: Vec<OpenedCard> = indices
            .iter()
            .map(|&i| OpenedCard {
                deck_index: i as u32,
                card_id: card_ids[i],
                salt: hex::encode(salts[i]),
                proof: make_open(i, &mut rng),
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

    builder.append(
        event_type::HAND_COMPLETE,
        to_payload(&HandCompletePayload {
            revealed_card_count: (2 * n + 5) as u32,
        }),
        COORDINATOR,
    );

    ReencFixture {
        transcript: builder.finish(),
        final_ct,
        joint_key: q,
        coord_sig,
        parties: run.parties,
    }
}

/// **mp-phase4 re-audit r1 (HIGH) — the F3 threshold-open anchor must be rooted in
/// the VERIFIED shuffle output on the reenc path.** On the real re-encryption
/// shuffle path the offline verifier anchors every threshold open to
/// `final_deck_committed.deck_ct` (`final_ciphertext_deck` → `with_expected_deck`).
/// But `final_deck_hash` — the only quantity the parties sign in `final_deck_ack`
/// and the only one tied to the verified ciphertext shuffle output — was NEVER
/// bound to `deck_ct` (`apply_final_deck` skipped the deck-commitments hash check
/// for reenc and never read `deck_ct`). So an untrusted (supposedly-blind)
/// coordinator could keep the genuine, acked `final_deck_hash` + verified last
/// shuffle proof while substituting a `deck_ct` of its choosing, and the verifier
/// would anchor self-supplied opens against that attacker-chosen deck — a
/// soundness hole in the independent replay verifier's trust anchor.
///
/// RED-first (the full attack the finding describes): substitute
/// `final_deck_committed.deck_ct` with a DIFFERENT well-formed 52-entry ciphertext
/// deck of the coordinator's choosing AND re-anchor every threshold open to that
/// substituted deck — while leaving `final_deck_hash` AND the last verified shuffle
/// proof UNCHANGED. Before the fix this verified CLEAN: `apply_final_deck` never
/// read `deck_ct` on the reenc path (it only checked `final_deck_hash` ==
/// last-shuffle-output), `final_ciphertext_deck` returned the substituted `deck_ct`
/// as the anchor, and the re-anchored opens matched it — the soundness hole. After
/// the fix `verify()` MUST reject: the state-machine bind (`apply_final_deck` reenc
/// branch) catches the deck_ct≠final_deck_hash mismatch as
/// `State(UnboundCiphertextDeck)` BEFORE any open, and the verifier's
/// `final_ciphertext_deck` cross-check against the verified shuffle output is a
/// second independent guard (`CiphertextDeckUnbound`).
///
/// To keep the substituted opens openable to the SAME card ids (so the per-card
/// commitment layer and `verify_and_open`'s recovered==claimed check still pass —
/// isolating the deck-anchor as the sole defect), the substituted deck is a fresh
/// RE-ENCRYPTION of the genuine deck under the same joint key (same plaintexts,
/// different ciphertexts).
#[test]
fn reenc_committed_deck_ct_must_be_bound_to_verified_shuffle_output() {
    use mental_poker::crypto_real::decrypt::{
        encode_threshold_attestation, partial_decrypt, ThresholdDecryptionProof, ThresholdOpenWire,
        SCHEME as DECRYPT_SCHEME,
    };
    use mental_poker::crypto_real::ec::{deck_hash as ct_deck_hash, Ct};
    use mental_poker::events::event_type;
    use mental_poker::hash::hex_hash;
    use mental_poker::state::StateError;
    use mental_poker::verifier::VerifyErrorKind;
    use mental_poker::verify;

    let n = 3usize;

    // (0) HONEST: the genuine reenc transcript verifies end to end, on the REAL
    //     re-encryption shuffle path with the cp-threshold decryption scheme.
    let honest = build_real_reenc_fixture(n);
    assert_eq!(
        honest.transcript.shuffle_scheme,
        mental_poker::crypto_real::shuffle::SCHEME,
        "fixture must be on the REAL re-encryption shuffle path"
    );
    assert_eq!(
        honest.transcript.decryption_scheme,
        mental_poker::crypto_real::decrypt::SCHEME
    );
    let report =
        verify(&honest.transcript).expect("the honest reenc transcript must verify end to end");
    assert_eq!(report.final_phase, mental_poker::state::Phase::Complete);

    // The FULL attack: substitute deck_ct with a fresh re-encryption of the genuine
    // committed deck under the same joint key (same plaintexts, different
    // ciphertexts), AND re-anchor every threshold open to the substituted deck using
    // the fixture's genuine DKG party secrets (exposed on the fixture). The signed
    // final_deck_hash + the last verified shuffle proof are left UNCHANGED.
    let mut rng = OsRng;
    let mut fixture = build_real_reenc_fixture(n);
    let substituted: Vec<Ct> = fixture
        .final_ct
        .iter()
        .map(|ct| ct.reencrypt(&fixture.joint_key, &Scalar::random(&mut rng)))
        .collect();
    assert_ne!(
        hex_hash(&ct_deck_hash(&substituted)),
        hex_hash(&ct_deck_hash(&fixture.final_ct)),
        "the substituted deck_ct must differ from the genuine committed deck"
    );

    // Re-anchor every open to the substituted ciphertext, using the fixture's DKG
    // parties (exposed on the fixture). This is the FULL attack: deck_ct + matching
    // opens, both attacker-chosen, final_deck_hash + shuffle proof genuine.
    let reanchor_open = |idx: usize, rng: &mut OsRng| -> String {
        let shares = fixture
            .parties
            .iter()
            .map(|p| partial_decrypt(p, idx as u32, &substituted[idx], rng))
            .collect::<Vec<_>>();
        encode_threshold_attestation(&ThresholdOpenWire {
            ct: substituted[idx].to_wire(),
            threshold: ThresholdDecryptionProof {
                scheme: DECRYPT_SCHEME.to_string(),
                shares,
            },
        })
    };

    let original_final_hash;
    {
        let ev = fixture
            .transcript
            .events
            .iter_mut()
            .find(|e| e.event_type == event_type::FINAL_DECK_COMMITTED)
            .expect("final_deck_committed exists");
        original_final_hash = ev.payload["final_deck_hash"]
            .as_str()
            .expect("final_deck_hash present")
            .to_string();
        ev.payload["deck_ct"] =
            serde_json::to_value(substituted.iter().map(|c| c.to_wire()).collect::<Vec<_>>())
                .unwrap();
        ev.payload_hash = hex_hash(&ev.computed_payload_hash());
        assert_eq!(
            ev.payload["final_deck_hash"].as_str().unwrap(),
            original_final_hash,
            "final_deck_hash must remain the genuine verified-shuffle output"
        );
    }
    // Re-anchor the hole + community opens to the substituted deck (so the attack is
    // fully self-consistent against the substituted anchor).
    for ev in fixture.transcript.events.iter_mut() {
        match ev.event_type.as_str() {
            event_type::HOLE_CARD_OPENED => {
                let idx = ev.payload["card"]["deck_index"].as_u64().unwrap() as usize;
                ev.payload["card"]["proof"]["attestation"] =
                    serde_json::Value::String(reanchor_open(idx, &mut rng));
                ev.payload_hash = hex_hash(&ev.computed_payload_hash());
            }
            event_type::COMMUNITY_REVEALED => {
                if let Some(cards) = ev.payload["cards"].as_array().cloned() {
                    let mut new_cards = cards;
                    for card in new_cards.iter_mut() {
                        let idx = card["deck_index"].as_u64().unwrap() as usize;
                        card["proof"]["attestation"] =
                            serde_json::Value::String(reanchor_open(idx, &mut rng));
                    }
                    ev.payload["cards"] = serde_json::Value::Array(new_cards);
                    ev.payload_hash = hex_hash(&ev.computed_payload_hash());
                }
            }
            _ => {}
        }
    }
    rechain_and_sign(&mut fixture.transcript, &fixture.coord_sig);

    let err = verify(&fixture.transcript).expect_err(
        "a substituted deck_ct + matching opens (with a genuine final_deck_hash + shuffle \
         proof) must be rejected — the F3 anchor cannot be rooted in an attacker-chosen deck",
    );
    // The state machine rejects first (apply_final_deck reenc bind), as
    // State(UnboundCiphertextDeck). Accept either that or the verifier-level
    // CiphertextDeckUnbound cross-check (defense in depth — both are valid guards).
    assert!(
        matches!(
            err.kind,
            VerifyErrorKind::State(StateError::UnboundCiphertextDeck(_))
                | VerifyErrorKind::CiphertextDeckUnbound(_)
        ),
        "expected the deck_ct binding to reject (State(UnboundCiphertextDeck) or \
         CiphertextDeckUnbound), got {:?}",
        err.kind
    );

    // (b) Prove the attack KEPT final_deck_hash genuine (so the acks + the
    //     state.rs final-shuffle-output check all passed up to the new bind) — the
    //     bind is what rejects it, not a perturbed signed hash.
    let committed_ev = fixture
        .transcript
        .events
        .iter()
        .find(|e| e.event_type == event_type::FINAL_DECK_COMMITTED)
        .unwrap();
    assert_eq!(
        committed_ev.payload["final_deck_hash"].as_str().unwrap(),
        original_final_hash,
        "the attack must leave final_deck_hash genuine — the bind is what rejects it"
    );
}

// ---------------------------------------------------------------------------
// BUG-108 (fail-closed) — SUCCESS counterpart for `verify_fairness`.
//
// The fail-closed tests in verifier.rs cover the DevMock branch (mock deal →
// NotProvablyFair) and the Replay branch (corrupt → Replay). The Sound branch —
// `verify_fairness` returning Ok for a FULLY real-crypto transcript — had no
// coverage, so a regression that stopped certifying a genuinely sound transcript
// (a false NEGATIVE — the fairness gate refusing a provably-fair deal) would not
// be caught. This builds the full real composition (real re-encryption shuffle +
// real threshold-ElGamal decryption + REAL Ed25519 signing → is_mock = false →
// `SchemeSoundness::Sound`) and asserts both `verify()` reports Sound and the
// strict `verify_fairness()` gate ACCEPTS it. It also exercises the verifier's
// asymmetric (Ed25519) signature arm end to end.
// ---------------------------------------------------------------------------

/// Build a COMPLETE, cryptographically SOUND real re-encryption-shuffle
/// transcript: identical crypto to [`build_real_reenc_fixture`] but signed with
/// REAL Ed25519 keys (coordinator + every party) so the exported key directory
/// is `is_mock = false` and the whole composition classifies as
/// [`SchemeSoundness::Sound`].
fn build_sound_reenc_transcript(n: usize) -> mental_poker::Transcript {
    use mental_poker::crypto::card_commit;
    use mental_poker::crypto_real::decrypt::{
        encode_threshold_attestation, ThresholdOpenWire, SCHEME as DECRYPT_SCHEME,
    };
    use mental_poker::crypto_real::dkg::schnorr_prove;
    use mental_poker::crypto_real::ec::{
        canonical_starting_deck, card_id_from_point, deck_hash as ct_deck_hash,
    };
    use mental_poker::crypto_real::ed25519_signer::Ed25519SignatureProvider;
    use mental_poker::crypto_real::shuffle::{RealShuffleProofProvider, Shuffle};
    use mental_poker::events::{
        event_type, party_id as mp_party_id, CommunityRevealedPayload, FinalDeckAckPayload,
        FinalDeckCommittedPayload, HandCompletePayload, HandInitPayload, HoleCardOpenedPayload,
        KeyRegisteredPayload, OpenedCard, PlayerEntry, ShuffleContributionPayload, COORDINATOR,
    };
    use mental_poker::hash::{canonical_json, ds_hash, hex_hash};
    use mental_poker::signing::SignatureProvider;
    use mental_poker::transcript::{to_payload, TranscriptBuilder};

    let mut rng = OsRng;
    let hand_id = "hand-sound-reenc";
    let table_id = "table-sound-reenc";

    let run = DkgRun::simulate(n, &mut rng);
    let q = run.joint_key;
    let qi_hex: Vec<String> = run.parties.iter().map(|p| point_to_hex(&p.q_i)).collect();

    // REAL asymmetric (Ed25519) signing for the coordinator + every party. One
    // provider holds all secret keys (test-only); its directory carries ONLY the
    // public verifying keys (is_mock = false), which is what makes the transcript
    // Sound. The verifier routes is_mock=false through the Ed25519 arm.
    let mut ed = Ed25519SignatureProvider::new();
    let coord_seed = ds_hash("mp:test-ed-coord:v1", &[hand_id.as_bytes()]);
    ed = ed.with_signer(COORDINATOR, &coord_seed);
    for i in 0..n {
        let seed = ds_hash("mp:test-ed-party:v1", &[hand_id.as_bytes(), &[i as u8]]);
        ed = ed.with_signer(mp_party_id(i as u8), &seed);
    }
    let key_directory = ed.directory();
    assert!(
        !key_directory.is_mock,
        "Ed25519 directory must be asymmetric"
    );

    // REAL re-encryption shuffle chain (identical to the mock fixture's crypto).
    let mut deck = canonical_starting_deck();
    let mut shuffles: Vec<mental_poker::crypto::ShuffleProof> = Vec::with_capacity(n);
    let mut chain: Vec<([u8; 32], [u8; 32])> = Vec::with_capacity(n);
    for r in 0..n {
        let input = deck.clone();
        let s = Shuffle::perform(input.clone(), &q, &mut rng);
        let ih = ct_deck_hash(&input);
        let oh = ct_deck_hash(&s.output);
        let proof = s.prove(&mp_party_id(r as u8), r as u32, &mut rng);
        shuffles.push(proof);
        chain.push((ih, oh));
        deck = s.output;
    }
    let final_ct = deck;

    let salts: Vec<[u8; 32]> = (0usize..52)
        .map(|j| {
            ds_hash(
                "mp:salt:v1",
                &[hand_id.as_bytes(), &(j as u64).to_le_bytes()],
            )
        })
        .collect();
    let card_ids: Vec<u8> = final_ct
        .iter()
        .map(|ct| {
            let secret_sum: RistrettoPoint = run.parties.iter().map(|p| p.x_i * ct.c1).sum();
            let m: RistrettoPoint = ct.c2 - secret_sum;
            card_id_from_point(&m).expect("every committed ciphertext opens to a valid card id")
        })
        .collect();
    let commits_hex: Vec<String> = (0..52)
        .map(|j| hex_hash(&card_commit(card_ids[j], &salts[j])))
        .collect();
    let final_hash_hex = hex_hash(&ct_deck_hash(&final_ct));

    let make_open = |idx: usize, rng: &mut OsRng| -> mental_poker::crypto::DecryptionProof {
        let shares = run
            .parties
            .iter()
            .map(|p| {
                mental_poker::crypto_real::decrypt::partial_decrypt(
                    p,
                    idx as u32,
                    &final_ct[idx],
                    rng,
                )
            })
            .collect();
        mental_poker::crypto::DecryptionProof {
            scheme: DECRYPT_SCHEME.to_string(),
            attestation: encode_threshold_attestation(&ThresholdOpenWire {
                ct: final_ct[idx].to_wire(),
                threshold: mental_poker::crypto_real::decrypt::ThresholdDecryptionProof {
                    scheme: DECRYPT_SCHEME.to_string(),
                    shares,
                },
            }),
        }
    };

    let shuffle_label = RealShuffleProofProvider::verifier();
    let decrypt_label = mental_poker::crypto_real::decrypt::RealThresholdDecryptionProvider::new(
        run.parties
            .iter()
            .map(|p| (p.party_id.clone(), p.q_i))
            .collect(),
    );
    let mut builder = TranscriptBuilder::new(
        hand_id,
        table_id,
        "mental_poker_engine_blind",
        &ed,
        &shuffle_label,
        &decrypt_label,
        key_directory,
    );

    let players: Vec<PlayerEntry> = (0..n as u8)
        .map(|s| PlayerEntry {
            seat: s,
            party_id: mp_party_id(s),
        })
        .collect();
    builder.append(
        event_type::HAND_INIT,
        to_payload(&HandInitPayload {
            players,
            button_seat: 0,
            big_blind: 20,
            small_blind: 10,
            deck_repr: Some("reenc".to_string()),
        }),
        COORDINATOR,
    );

    // Index-driven (party id, shuffle pubkey, DKG share) parallel to the mock
    // fixture's key-registration loop.
    #[allow(clippy::needless_range_loop)]
    for i in 0..n {
        let pid = mp_party_id(i as u8);
        // signing_pubkey is the party's REAL Ed25519 verifying key (== directory).
        let signing_pubkey = ed.verifying_key_hex(&pid).expect("ed vk for party");
        let shuffle_pubkey = qi_hex[i].clone();
        let claim = serde_json::json!({
            "hand_id": hand_id, "party_id": pid,
            "signing_pubkey": signing_pubkey, "shuffle_pubkey": shuffle_pubkey,
        });
        let claim_hash = ds_hash("mp:claim:v1", &[&canonical_json(&claim)]);
        let csig = ed.sign(&pid, &claim_hash);
        let pok = schnorr_prove(&pid, &run.parties[i].x_i, &run.parties[i].q_i, &mut rng);
        builder.append_with_contributor(
            event_type::KEY_REGISTERED,
            to_payload(&KeyRegisteredPayload {
                party_id: pid.clone(),
                seat: i as u8,
                signing_pubkey,
                shuffle_pubkey,
                contributor: None,
                contributor_signature: None,
                key_pok: Some(pok),
            }),
            COORDINATOR,
            Some((pid.as_str(), csig.as_str())),
        );
    }

    for r in 0..n {
        let pid = mp_party_id(r as u8);
        let (ih, oh) = chain[r];
        let proof = shuffles[r].clone();
        let claim = serde_json::json!({
            "hand_id": hand_id, "round": r as u64,
            "input_deck_hash": hex_hash(&ih),
            "output_deck_hash": hex_hash(&oh),
            "proof_attestation": proof.attestation,
        });
        let claim_hash = ds_hash("mp:claim:v1", &[&canonical_json(&claim)]);
        let csig = ed.sign(&pid, &claim_hash);
        builder.append_with_contributor(
            event_type::SHUFFLE_CONTRIBUTION,
            to_payload(&ShuffleContributionPayload {
                party_id: pid.clone(),
                round: r as u32,
                input_deck_hash: hex_hash(&ih),
                output_deck_hash: hex_hash(&oh),
                proof,
                contributor: None,
                contributor_signature: None,
            }),
            COORDINATOR,
            Some((pid.as_str(), csig.as_str())),
        );
    }

    builder.append(
        event_type::FINAL_DECK_COMMITTED,
        to_payload(&FinalDeckCommittedPayload {
            final_deck_hash: final_hash_hex.clone(),
            deck: commits_hex,
            deck_ct: Some(final_ct.iter().map(|c| c.to_wire()).collect()),
        }),
        COORDINATOR,
    );

    for i in 0..n {
        let pid = mp_party_id(i as u8);
        let claim = serde_json::json!({
            "hand_id": hand_id, "party_id": pid, "final_deck_hash": final_hash_hex,
        });
        let claim_hash = ds_hash("mp:claim:v1", &[&canonical_json(&claim)]);
        let csig = ed.sign(&pid, &claim_hash);
        builder.append_with_contributor(
            event_type::FINAL_DECK_ACK,
            to_payload(&FinalDeckAckPayload {
                party_id: pid.clone(),
                final_deck_hash: final_hash_hex.clone(),
                contributor: None,
                contributor_signature: None,
            }),
            COORDINATOR,
            Some((pid.as_str(), csig.as_str())),
        );
    }

    for idx in 0..2 * n {
        let owner_seat = if idx < n { idx as u8 } else { (idx - n) as u8 };
        let pid = mp_party_id(owner_seat);
        let claim = serde_json::json!({
            "hand_id": hand_id, "deck_index": idx as u64, "card_id": card_ids[idx] as u64,
        });
        let claim_hash = ds_hash("mp:claim:v1", &[&canonical_json(&claim)]);
        let csig = ed.sign(COORDINATOR, &claim_hash);
        builder.append_with_contributor(
            event_type::HOLE_CARD_OPENED,
            to_payload(&HoleCardOpenedPayload {
                seat: owner_seat,
                owner_party_id: pid.clone(),
                card: OpenedCard {
                    deck_index: idx as u32,
                    card_id: card_ids[idx],
                    salt: hex::encode(salts[idx]),
                    proof: make_open(idx, &mut rng),
                },
                contributor: None,
                contributor_signature: None,
            }),
            COORDINATOR,
            Some((COORDINATOR, csig.as_str())),
        );
    }

    let base = 2 * n;
    for (stage, indices) in [
        ("flop", vec![base, base + 1, base + 2]),
        ("turn", vec![base + 3]),
        ("river", vec![base + 4]),
    ] {
        let cards: Vec<OpenedCard> = indices
            .iter()
            .map(|&i| OpenedCard {
                deck_index: i as u32,
                card_id: card_ids[i],
                salt: hex::encode(salts[i]),
                proof: make_open(i, &mut rng),
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

    builder.append(
        event_type::HAND_COMPLETE,
        to_payload(&HandCompletePayload {
            revealed_card_count: (2 * n + 5) as u32,
        }),
        COORDINATOR,
    );

    builder.finish()
}

#[test]
fn verify_fairness_accepts_real_crypto_transcript() {
    use mental_poker::{verify, verify_fairness, SchemeSoundness};

    let transcript = build_sound_reenc_transcript(3);

    // The FULL real composition: real shuffle + real decrypt + real Ed25519
    // signing (is_mock = false).
    assert_eq!(
        transcript.shuffle_scheme,
        mental_poker::crypto_real::shuffle::SCHEME,
        "must use the real re-encryption shuffle scheme"
    );
    assert_eq!(
        transcript.decryption_scheme,
        mental_poker::crypto_real::decrypt::SCHEME,
        "must use the real threshold-decryption scheme"
    );
    assert!(
        !transcript.key_directory.is_mock,
        "must use the real asymmetric (Ed25519) signing directory"
    );

    // (1) replay self-check passes AND classifies Sound (also exercises the
    //     verifier's Ed25519 signature arm end to end).
    let report = verify(&transcript)
        .expect("a fully real-crypto transcript must replay + verify consistently");
    assert_eq!(report.soundness, SchemeSoundness::Sound);
    assert!(
        report.is_provably_fair(),
        "a sound real-crypto transcript IS a provable-fairness guarantee"
    );

    // (2) the strict fairness gate ACCEPTS it (Sound branch → Ok) — the success
    //     counterpart to the DevMock/Replay fail-closed tests in verifier.rs.
    let fair = verify_fairness(&transcript)
        .expect("verify_fairness must return Ok on a sound real-crypto transcript");
    assert_eq!(fair.soundness, SchemeSoundness::Sound);
}

/// U40 (dual-AI OSS review): committed regeneration path for the frozen
/// `tests/vectors/mp_sound_reenc.json` fixture consumed by
/// `mp_verify_exit_code.rs::mp_verify_real_crypto_transcript_exits_zero`.
///
/// The fixture is a cryptographically SOUND real-crypto transcript from
/// [`build_sound_reenc_transcript`]. Its key material is drawn from `OsRng`, so
/// each regeneration produces DIFFERENT bytes — that is fine: verification is
/// deterministic over whatever bytes are committed, and any sound transcript
/// satisfies the consumers. Regenerate (then commit + review the diff) with:
///
/// ```sh
/// cargo test -p mental-poker --test phase4_server_blind \
///     regenerate_sound_reenc_fixture -- --ignored
/// ```
#[test]
#[ignore = "fixture regenerator — rewrites tests/vectors/mp_sound_reenc.json; run explicitly"]
fn regenerate_sound_reenc_fixture() {
    use mental_poker::{verify, SchemeSoundness};

    let transcript = build_sound_reenc_transcript(3);
    // Never freeze a fixture that is not provably Sound.
    let report = verify(&transcript).expect("regenerated fixture must verify");
    assert_eq!(report.soundness, SchemeSoundness::Sound);

    let path = concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/tests/vectors/mp_sound_reenc.json"
    );
    std::fs::write(path, transcript.to_json()).expect("write sound-transcript fixture");
    println!("[regenerate_sound_reenc_fixture] rewrote {path}");
}
