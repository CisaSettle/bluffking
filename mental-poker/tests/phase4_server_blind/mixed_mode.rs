//! Phase-4 server-blind cryptographic integration tests: mixed mode.

use super::*;

// ===========================================================================
// mp-phase4 audit F2 + F3 — DEFENSIVE regression: the offline verifier's
// rogue-key PoK gate (F2) and committed-ciphertext-deck anchor (F3) must hold
// for the real `cp-threshold-ristretto-v1` decryption scheme REGARDLESS of the
// shuffle scheme — so a transcript that pairs real threshold decryption with a
// NON-real (mock) shuffle cannot regress past either guarantee.
// ===========================================================================

/// A complete honest transcript on the MOCK-shuffle + REAL-`cp-threshold`
/// decryption path, plus everything a mutation test needs. The shuffle scheme is
/// the mock one (so the F2/F3 fixes cannot be passing merely because the shuffle
/// is real); the decryption scheme is the real threshold-ElGamal one. The
/// `key_pok` of each `key_registered` is a genuine party-bound Schnorr PoK; the
/// `final_deck_committed` carries the committed ciphertext deck in `deck_ct`.
struct MixedModeFixture {
    transcript: mental_poker::Transcript,
    coord_sig: mental_poker::signing::MockSignatureProvider,
    /// The committed ciphertext deck, deck-index order (== final_deck_committed.deck_ct).
    cts: Vec<Ct>,
    /// The dealt card id per deck index.
    deck_ids: Vec<u8>,
    /// Per-index salt.
    salts: Vec<[u8; 32]>,
    /// The DKG run (per-party secret/public shares).
    run: DkgRun,
}

/// Build [`MixedModeFixture`]. `pok_override(i, genuine)` lets a test substitute a
/// party's `key_pok` (e.g. `None` to drop it, or a bogus PoK) — `genuine` is the
/// honest PoK for party `i`. Returns the materials so a test can tamper one event
/// then re-chain + re-sign with [`rechain_and_sign`].
fn build_mixed_mode_fixture(
    n: usize,
    pok_override: impl Fn(
        usize,
        mental_poker::crypto_real::dkg::SchnorrPok,
    ) -> Option<mental_poker::crypto_real::dkg::SchnorrPok>,
) -> MixedModeFixture {
    use mental_poker::crypto::{
        card_commit, deck_hash as commit_deck_hash, MockShuffleProofProvider, ShuffleProofProvider,
    };
    use mental_poker::crypto_real::decrypt::{
        encode_threshold_attestation, partial_decrypt, ThresholdDecryptionProof, ThresholdOpenWire,
        SCHEME as DECRYPT_SCHEME,
    };
    use mental_poker::crypto_real::dkg::schnorr_prove;
    use mental_poker::events::{
        event_type, party_id as mp_party_id, CommunityRevealedPayload, FinalDeckAckPayload,
        FinalDeckCommittedPayload, HandCompletePayload, HandInitPayload, HoleCardOpenedPayload,
        KeyRegisteredPayload, OpenedCard, PlayerEntry, ShuffleContributionPayload, COORDINATOR,
    };
    use mental_poker::hash::{canonical_json, ds_hash, hex_hash};
    use mental_poker::signing::{KeyDirectory, MockSignatureProvider, SignatureProvider};
    use mental_poker::state::canonical_wire_deck_hash;
    use mental_poker::transcript::{to_payload, TranscriptBuilder};
    use std::collections::BTreeMap;

    fn wire_deck_hash(deck: &[u8]) -> [u8; 32] {
        let parts: Vec<&[u8]> = deck.iter().map(std::slice::from_ref).collect();
        ds_hash("mp:deck-hash:v1", &parts)
    }

    let mut rng = OsRng;
    let hand_id = "hand-mixed-mode";
    let table_id = "table-mixed";

    let run = DkgRun::simulate(n, &mut rng);
    let q = run.joint_key;
    let qi_hex: Vec<String> = run.parties.iter().map(|p| point_to_hex(&p.q_i)).collect();

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

    let mut deck_ids: Vec<u8> = (0u8..52).collect();
    deck_ids.rotate_left(7);
    let salts: Vec<[u8; 32]> = (0usize..52)
        .map(|j| {
            ds_hash(
                "mp:salt:v1",
                &[hand_id.as_bytes(), &(j as u64).to_le_bytes()],
            )
        })
        .collect();
    let cts: Vec<Ct> = (0..52)
        .map(|j| Ct::encrypt_card(deck_ids[j], &q, &Scalar::random(&mut rng)))
        .collect();
    let commits: Vec<[u8; 32]> = (0..52)
        .map(|j| card_commit(deck_ids[j], &salts[j]))
        .collect();
    let commit_hash = commit_deck_hash(&commits);
    let commit_hash_hex = hex_hash(&commit_hash);
    let commits_hex: Vec<String> = commits.iter().map(hex_hash).collect();

    let make_open =
        |idx: usize, ct: &Ct, rng: &mut OsRng| -> mental_poker::crypto::DecryptionProof {
            let shares = run
                .parties
                .iter()
                .map(|p| partial_decrypt(p, idx as u32, ct, rng))
                .collect();
            mental_poker::crypto::DecryptionProof {
                scheme: DECRYPT_SCHEME.to_string(),
                attestation: encode_threshold_attestation(&ThresholdOpenWire {
                    ct: ct.to_wire(),
                    threshold: ThresholdDecryptionProof {
                        scheme: DECRYPT_SCHEME.to_string(),
                        shares,
                    },
                }),
            }
        };

    let shuffle_provider = MockShuffleProofProvider;
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
        &shuffle_provider,
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
            deck_repr: Some("wire".to_string()),
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
        let genuine = schnorr_prove(&pid, &run.parties[i].x_i, &run.parties[i].q_i, &mut rng);
        builder.append_with_contributor(
            event_type::KEY_REGISTERED,
            to_payload(&KeyRegisteredPayload {
                party_id: pid.clone(),
                seat: i as u8,
                signing_pubkey,
                shuffle_pubkey,
                contributor: None,
                contributor_signature: None,
                key_pok: pok_override(i, genuine),
            }),
            COORDINATOR,
            Some((pid.as_str(), csig.as_str())),
        );
    }

    // Mock shuffle round-robin; the wire-hash chain ends at the commit hash.
    let initial: Vec<u8> = (0u8..52).collect();
    let mut round_decks: Vec<Vec<u8>> = Vec::new();
    let mut cur = initial.clone();
    for r in 0..n {
        let mut next = cur.clone();
        let seed = ds_hash("mp:test-perm:v1", &[hand_id.as_bytes(), &[r as u8]]);
        next.rotate_left((seed[0] % 52) as usize);
        round_decks.push(next.clone());
        cur = next;
    }
    let mut prev_hash: [u8; 32] = canonical_wire_deck_hash();
    for r in 0..n {
        let input_hash = prev_hash;
        let output_hash: [u8; 32] = if r == n - 1 {
            commit_hash
        } else {
            wire_deck_hash(&round_decks[r])
        };
        let pid = mp_party_id(r as u8);
        let proof = shuffle_provider.prove_shuffle(&pid, r as u32, &input_hash, &output_hash);
        let claim = serde_json::json!({
            "hand_id": hand_id, "round": r as u64,
            "input_deck_hash": hex_hash(&input_hash),
            "output_deck_hash": hex_hash(&output_hash),
            "proof_attestation": proof.attestation,
        });
        let claim_hash = ds_hash("mp:claim:v1", &[&canonical_json(&claim)]);
        let csig = party_sigs[r].sign(&pid, &claim_hash);
        builder.append_with_contributor(
            event_type::SHUFFLE_CONTRIBUTION,
            to_payload(&ShuffleContributionPayload {
                party_id: pid.clone(),
                round: r as u32,
                input_deck_hash: hex_hash(&input_hash),
                output_deck_hash: hex_hash(&output_hash),
                proof,
                contributor: None,
                contributor_signature: None,
            }),
            COORDINATOR,
            Some((pid.as_str(), csig.as_str())),
        );
        prev_hash = output_hash;
    }

    builder.append(
        event_type::FINAL_DECK_COMMITTED,
        to_payload(&FinalDeckCommittedPayload {
            final_deck_hash: commit_hash_hex.clone(),
            deck: commits_hex,
            // F3 anchor: the committed ciphertext deck, bound on this event.
            deck_ct: Some(cts.iter().map(|c| c.to_wire()).collect()),
        }),
        COORDINATOR,
    );

    for (i, party_sig) in party_sigs.iter().enumerate() {
        let pid = mp_party_id(i as u8);
        let claim = serde_json::json!({
            "hand_id": hand_id, "party_id": pid, "final_deck_hash": commit_hash_hex,
        });
        let claim_hash = ds_hash("mp:claim:v1", &[&canonical_json(&claim)]);
        let csig = party_sig.sign(&pid, &claim_hash);
        builder.append_with_contributor(
            event_type::FINAL_DECK_ACK,
            to_payload(&FinalDeckAckPayload {
                party_id: pid.clone(),
                final_deck_hash: commit_hash_hex.clone(),
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
        let card_id = deck_ids[idx];
        let proof = make_open(idx, &cts[idx], &mut rng);
        let claim = serde_json::json!({
            "hand_id": hand_id, "deck_index": idx as u64, "card_id": card_id as u64,
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
                    card_id,
                    salt: hex::encode(salts[idx]),
                    proof,
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
                card_id: deck_ids[i],
                salt: hex::encode(salts[i]),
                proof: make_open(i, &cts[i], &mut rng),
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

    MixedModeFixture {
        transcript: builder.finish(),
        coord_sig,
        cts,
        deck_ids,
        salts,
        run,
    }
}

/// **mp-phase4 audit F2 (DEFENSIVE — gate the rogue-key PoK on the DECRYPTION
/// scheme, not just the shuffle scheme).** A transcript whose
/// `decryption_scheme == cp-threshold-ristretto-v1` but whose SHUFFLE is the
/// dev-mock scheme MUST still require + verify each party's party-bound Schnorr
/// PoK of `log_G(Q_i)` — because the `Q_i` sum to the joint key the deck is
/// encrypted under (`Ct.c2 = M + r·Q`). Without this, a mixed-mode transcript
/// (real decryption over a non-real shuffle) re-opens the rogue-key hole
/// (`Q_rogue = a·G − Σ Q_honest` ⇒ joint key `a·G` ⇒ decrypt every card).
///
/// RED-first: before the fix, `require_key_pok` was `shuffle_scheme == real`
/// only, so this MOCK-shuffle transcript skipped the PoK gate entirely and a
/// missing/forged `key_pok` verified clean. After the fix it is rejected with
/// `BadKeyProofOfKnowledge`. The all-genuine-PoK transcript verifies in both
/// the honest case here and proves the gate does not over-reject.
#[test]
fn f2_pok_gate_runs_for_cp_threshold_even_with_mock_shuffle() {
    use mental_poker::crypto_real::dkg::{schnorr_prove, SchnorrPok};
    use mental_poker::crypto_real::ec::point_to_hex as pt_hex;
    use mental_poker::crypto_real::shuffle::SCHEME as REAL_SHUFFLE_SCHEME;
    use mental_poker::verifier::VerifyErrorKind;
    use mental_poker::verify;

    let n = 3usize;

    // (0) HONEST: every party carries its genuine PoK → verifies end to end, and
    // the transcript is unambiguously on the MOCK-shuffle + REAL-decryption path.
    let honest = build_mixed_mode_fixture(n, |_, genuine| Some(genuine));
    assert_ne!(
        honest.transcript.shuffle_scheme, REAL_SHUFFLE_SCHEME,
        "fixture must use the NON-real (mock) shuffle so the gate is decryption-driven"
    );
    assert_eq!(
        honest.transcript.decryption_scheme,
        mental_poker::crypto_real::decrypt::SCHEME
    );
    let report = verify(&honest.transcript)
        .expect("an all-PoK-backed cp-threshold transcript must verify (mock shuffle)");
    assert_eq!(report.final_phase, mental_poker::state::Phase::Complete);

    // (a) MISSING PoK on the last party → rejected with BadKeyProofOfKnowledge,
    // even though the shuffle is the mock scheme. (Before the fix this verified.)
    let missing = build_mixed_mode_fixture(
        n,
        |i, genuine| {
            if i == n - 1 {
                None
            } else {
                Some(genuine)
            }
        },
    );
    let err = verify(&missing.transcript)
        .expect_err("a missing key_pok under cp-threshold must be rejected (F2, mock shuffle)");
    assert!(
        matches!(err.kind, VerifyErrorKind::BadKeyProofOfKnowledge(_)),
        "expected BadKeyProofOfKnowledge for the missing PoK, got {:?}",
        err.kind
    );

    // (b) FORGED PoK — a rogue last key Q_rogue = a·G − Σ Q_honest with a PoK for
    // the WRONG secret (a, whose Q is a·G ≠ Q_rogue). The PoK does not verify
    // against the registered Q_rogue, so it is rejected. This proves the rogue is
    // caught for lacking log_G(Q_rogue), not merely an empty field — under a mock
    // shuffle. We must register Q_rogue as the shuffle_pubkey, so build a fixture
    // whose last party's Q is replaced. The fixture always registers the DKG Q_i,
    // so we mutate the finished transcript's last key_registered event in place
    // and re-chain+sign (the PoK check runs after signatures).
    let mut forged = build_mixed_mode_fixture(n, |_, genuine| Some(genuine));
    let mut rng = OsRng;
    // Σ Q_i of all but the last party, from the fixture's own DKG run.
    let sum_others: RistrettoPoint = forged.run.parties[..n - 1].iter().map(|p| p.q_i).sum();
    let a = Scalar::random(&mut rng);
    let q_rogue = a * G - sum_others;
    let bogus_pok: SchnorrPok = schnorr_prove(
        &mental_poker::events::party_id((n - 1) as u8),
        &a,
        &(a * G),
        &mut rng,
    );
    {
        // Find the last key_registered event and rewrite its shuffle_pubkey +
        // key_pok to the rogue values. The contributor signature over the claim
        // would no longer match the rewritten shuffle_pubkey, so we also re-sign
        // the contributor claim with the party key — but the F2 gate fires before
        // we depend on that being honest. To keep the transcript structurally
        // valid up to the PoK check, recompute the contributor signature too.
        use mental_poker::events::{event_type, party_id as mp_party_id, COORDINATOR};
        use mental_poker::hash::{canonical_json, ds_hash};
        use mental_poker::signing::{KeyDirectory, MockSignatureProvider, SignatureProvider};

        let hand_id = forged.transcript.hand_id.clone();
        let last_pid = mp_party_id((n - 1) as u8);
        // Reconstruct the last party's signing key the fixture used.
        let party_key = ds_hash(
            "mp:test-party-key:v1",
            &[hand_id.as_bytes(), &[(n - 1) as u8]],
        )
        .to_vec();
        let party_sig = MockSignatureProvider::from_directory(&KeyDirectory {
            keys: {
                let mut m = std::collections::BTreeMap::new();
                m.insert(last_pid.clone(), hex::encode(&party_key));
                m
            },
            is_mock: true,
        })
        .unwrap();

        let ev = forged
            .transcript
            .events
            .iter_mut()
            .rfind(|e| e.event_type == event_type::KEY_REGISTERED)
            .expect("a key_registered exists");
        let rogue_hex = pt_hex(&q_rogue);
        ev.payload["shuffle_pubkey"] = serde_json::Value::String(rogue_hex.clone());
        ev.payload["key_pok"] = serde_json::to_value(&bogus_pok).unwrap();
        // Re-sign the contributor claim over the rewritten shuffle_pubkey.
        let signing_pubkey = ev.payload["signing_pubkey"].as_str().unwrap().to_string();
        let claim = serde_json::json!({
            "hand_id": hand_id, "party_id": last_pid,
            "signing_pubkey": signing_pubkey, "shuffle_pubkey": rogue_hex,
        });
        let claim_hash = ds_hash("mp:claim:v1", &[&canonical_json(&claim)]);
        ev.payload["contributor_signature"] =
            serde_json::Value::String(party_sig.sign(&last_pid, &claim_hash));
        let _ = COORDINATOR;
        ev.payload_hash = mental_poker::hash::hex_hash(&ev.computed_payload_hash());
    }
    rechain_and_sign(&mut forged.transcript, &forged.coord_sig);
    let err2 = verify(&forged.transcript)
        .expect_err("a rogue key with a PoK for the wrong secret must be rejected (F2)");
    assert!(
        matches!(err2.kind, VerifyErrorKind::BadKeyProofOfKnowledge(_)),
        "expected BadKeyProofOfKnowledge for the forged PoK, got {:?}",
        err2.kind
    );
}

/// **mp-phase4 audit F3 (DEFENSIVE — anchor every threshold open to the COMMITTED
/// ciphertext deck under cp-threshold, even with a mock shuffle).** A threshold
/// open whose self-supplied ciphertext does NOT equal the committed deck entry at
/// its `deck_index` MUST be rejected — the open is pinned to the EXACT ciphertext
/// the `final_deck_committed` step froze (transcript-bound in `deck_ct`), not a
/// ciphertext the prover substituted.
///
/// RED-first: before the fix, the verifier built `RealThresholdDecryptionProvider`
/// WITHOUT an expected-deck anchor whenever the shuffle was the mock scheme (the
/// committed ciphertext deck was recoverable only from a real shuffle proof). So a
/// fully self-consistent open of a substituted ciphertext verified clean. After
/// the fix the verifier reads the committed deck from `final_deck_committed.deck_ct`
/// and rejects the substitution with `BadDecryptionProof`.
#[test]
fn f3_open_must_match_committed_ciphertext_deck_under_cp_threshold() {
    use mental_poker::crypto_real::decrypt::{
        encode_threshold_attestation, partial_decrypt, ThresholdDecryptionProof, ThresholdOpenWire,
        SCHEME as DECRYPT_SCHEME,
    };
    use mental_poker::events::event_type;
    use mental_poker::hash::hex_hash;
    use mental_poker::verifier::VerifyErrorKind;
    use mental_poker::verify;

    let n = 3usize;

    // (0) HONEST: the committed-deck-anchored transcript verifies end to end.
    let honest = build_mixed_mode_fixture(n, |_, genuine| Some(genuine));
    let report = verify(&honest.transcript).expect("the honest mixed-mode transcript must verify");
    assert_eq!(report.final_phase, mental_poker::state::Phase::Complete);

    // (a) SUBSTITUTE the first hole open's ciphertext with a FRESH encryption of
    // the SAME card id committed at index 0, but under different randomness `r`
    // (so it is a DIFFERENT ciphertext that still opens to the same plaintext).
    // We keep `card_id` unchanged, so the state machine's commitment-layer check
    // (card_id↔salt vs final_deck_commits[0]) and verify_and_open's
    // recovered==claimed check BOTH pass — ONLY the ciphertext-deck anchor (the
    // F3 subject) can reject it. This is precisely the gap the anchor closes: the
    // commitment layer pins the card VALUE, not which ciphertext was opened.
    let mut fixture = build_mixed_mode_fixture(n, |_, genuine| Some(genuine));
    let mut rng = OsRng;
    let committed_id0 = fixture.deck_ids[0];
    let ct_sub = Ct::encrypt_card(
        committed_id0,
        &fixture.run.joint_key,
        &Scalar::random(&mut rng),
    );
    // Sanity: the substituted ciphertext is NOT the committed one (different `r`).
    assert_ne!(
        (ct_sub.to_wire().c1, ct_sub.to_wire().c2),
        (fixture.cts[0].to_wire().c1, fixture.cts[0].to_wire().c2),
        "the substituted ciphertext must differ from the committed deck[0]"
    );
    let shares: Vec<_> = fixture
        .run
        .parties
        .iter()
        .map(|p| partial_decrypt(p, 0, &ct_sub, &mut rng))
        .collect();
    let sub_proof = mental_poker::crypto::DecryptionProof {
        scheme: DECRYPT_SCHEME.to_string(),
        attestation: encode_threshold_attestation(&ThresholdOpenWire {
            ct: ct_sub.to_wire(),
            threshold: ThresholdDecryptionProof {
                scheme: DECRYPT_SCHEME.to_string(),
                shares,
            },
        }),
    };
    {
        let ev = fixture
            .transcript
            .events
            .iter_mut()
            .find(|e| {
                e.event_type == event_type::HOLE_CARD_OPENED
                    && e.payload["card"]["deck_index"].as_u64() == Some(0)
            })
            .expect("the deck_index 0 hole open exists");
        // card_id + salt are UNCHANGED — only the attestation's ciphertext differs.
        ev.payload["card"]["proof"]["attestation"] =
            serde_json::Value::String(sub_proof.attestation);
        ev.payload_hash = hex_hash(&ev.computed_payload_hash());
    }
    let _ = &fixture.salts;
    rechain_and_sign(&mut fixture.transcript, &fixture.coord_sig);

    let err = verify(&fixture.transcript).expect_err(
        "an open of a ciphertext other than the committed deck entry must be rejected (F3 anchor)",
    );
    assert!(
        matches!(err.kind, VerifyErrorKind::BadDecryptionProof(_)),
        "expected BadDecryptionProof from the deck anchor, got {:?}",
        err.kind
    );

    // (b) PROOF that the anchor is what rejects it: strip the committed ciphertext
    // deck from final_deck_committed (deck_ct → absent) and the SAME substituted
    // open now makes the transcript FAIL TO BUILD A PROVIDER (mandatory anchor) —
    // a cp-threshold transcript with no committed ciphertext deck is malformed.
    {
        let ev = fixture
            .transcript
            .events
            .iter_mut()
            .find(|e| e.event_type == event_type::FINAL_DECK_COMMITTED)
            .expect("final_deck_committed exists");
        ev.payload.as_object_mut().unwrap().remove("deck_ct");
        ev.payload_hash = hex_hash(&ev.computed_payload_hash());
    }
    rechain_and_sign(&mut fixture.transcript, &fixture.coord_sig);
    let err_no_deck = verify(&fixture.transcript)
        .expect_err("a cp-threshold transcript with no committed ciphertext deck must be rejected");
    assert!(
        matches!(err_no_deck.kind, VerifyErrorKind::CiphertextDeckUnbound(_)),
        "expected CiphertextDeckUnbound (mandatory anchor), got {:?}",
        err_no_deck.kind
    );
}
