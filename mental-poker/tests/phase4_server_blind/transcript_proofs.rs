//! Phase-4 server-blind cryptographic integration tests: transcript proofs.

use super::*;

/// **F3 (offline verifier wiring).** An exported transcript whose
/// `decryption_scheme = cp-threshold-ristretto-v1` is checked END TO END by
/// `mental_poker::verify()`: the offline verifier reconstructs the DKG party
/// public keys from `key_registered`, builds the real
/// `RealThresholdDecryptionProvider`, and verifies every hole/community open's
/// n-of-n Chaum–Pedersen threshold proof (recovering exactly the committed
/// card id). A tampered decryption share makes `verify()` REJECT.
///
/// The SHUFFLE scheme is the mock here (so the state-machine deck-hash
/// continuity holds the simple way); the DECRYPTION scheme — the F3 subject —
/// is the REAL threshold-ElGamal one. Before F3, `verify()` returned
/// `UnsupportedScheme("cp-threshold-ristretto-v1")` for such a transcript.
#[test]
fn f3_real_threshold_decryption_transcript_verifies_end_to_end() {
    use mental_poker::crypto::{
        card_commit, deck_hash as commit_deck_hash, MockShuffleProofProvider, ShuffleProofProvider,
    };
    use mental_poker::crypto_real::decrypt::{
        encode_threshold_attestation, partial_decrypt, ThresholdDecryptionProof, ThresholdOpenWire,
        SCHEME as DECRYPT_SCHEME,
    };
    use mental_poker::events::{
        event_type, party_id as mp_party_id, CommunityRevealedPayload, FinalDeckAckPayload,
        FinalDeckCommittedPayload, HandCompletePayload, HandInitPayload, HoleCardOpenedPayload,
        KeyRegisteredPayload, OpenedCard, PlayerEntry, ShuffleContributionPayload, COORDINATOR,
    };
    use mental_poker::hash::{canonical_json, ds_hash, hex_hash};
    use mental_poker::signing::{KeyDirectory, MockSignatureProvider, SignatureProvider};
    use mental_poker::state::canonical_wire_deck_hash;
    use mental_poker::transcript::{to_payload, TranscriptBuilder};
    use mental_poker::verify;
    use std::collections::BTreeMap;

    fn wire_deck_hash(deck: &[u8]) -> [u8; 32] {
        let parts: Vec<&[u8]> = deck.iter().map(std::slice::from_ref).collect();
        ds_hash("mp:deck-hash:v1", &parts)
    }

    let mut rng = OsRng;
    let n: usize = 3;
    let hand_id = "hand-f3-real-decrypt";
    let table_id = "table-f3";

    // --- Real DKG → joint key Q; party public shares Q_i become shuffle_pubkey. ---
    let run = DkgRun::simulate(n, &mut rng);
    let q = run.joint_key;
    let qi_hex: Vec<String> = run.parties.iter().map(|p| point_to_hex(&p.q_i)).collect();

    // --- Mock signing key directory (per-party + coordinator). ---
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

    // --- Deck plan: a permutation of 0..51, per-position salt; the ELGAMAL
    //     ciphertext for each position is encrypted under the joint key Q. ---
    let mut deck_ids: Vec<u8> = (0u8..52).collect();
    // A fixed, non-trivial permutation (rotate) so the dealt cards are not in
    // canonical order (proves the threshold open recovers the right id per index).
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

    // Build the real n-of-n threshold opening for one deck index, encoded into a
    // DecryptionProof attestation (the F3 wire form).
    let make_open = |idx: usize, rng: &mut OsRng| -> mental_poker::crypto::DecryptionProof {
        let shares = run
            .parties
            .iter()
            .map(|p| partial_decrypt(p, idx as u32, &cts[idx], rng))
            .collect();
        let wire = ThresholdOpenWire {
            ct: cts[idx].to_wire(),
            threshold: ThresholdDecryptionProof {
                scheme: DECRYPT_SCHEME.to_string(),
                shares,
            },
        };
        mental_poker::crypto::DecryptionProof {
            scheme: DECRYPT_SCHEME.to_string(),
            attestation: encode_threshold_attestation(&wire),
        }
    };

    let shuffle_provider = MockShuffleProofProvider;
    // Provide the REAL decryption scheme label to the builder (so the transcript
    // records decryption_scheme = cp-threshold-ristretto-v1). The builder only
    // reads `.scheme()` from the provider for the label; it does not prove opens.
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

    // hand_init (interactive wire path).
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

    // key_registered — shuffle_pubkey = the REAL DKG public share Q_i (so the
    // offline verifier reconstructs the party-pubkey directory, F3, and the joint
    // key, F2). Even though the shuffle here is the MOCK scheme, the DECRYPTION is
    // the real cp-threshold one — so the F2 rogue-key PoK gate now runs (mp-phase4
    // audit: gate on the decryption scheme, not only the shuffle). Each party
    // therefore carries a genuine party-bound Schnorr PoK of log_G(Q_i).
    use mental_poker::crypto_real::dkg::schnorr_prove;
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
                // cp-threshold decryption requires a party-bound PoK of log_G(Q_i)
                // — the verifier sums only PoK-backed keys into the directory the
                // threshold opens trust (mp-phase4 F2, decryption-gated).
                key_pok: Some(pok),
            }),
            COORDINATOR,
            Some((pid.as_str(), csig.as_str())),
        );
    }

    // shuffle round-robin (MOCK shuffle proofs; the wire-hash chain ends at the
    // commit hash so the state machine's continuity check passes).
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

    // final_deck_committed — transcript-bind the committed CIPHERTEXT deck
    // (`deck_ct`) so the offline verifier anchors every threshold open to
    // `deck_ct[deck_index]` even though the shuffle here is the mock scheme
    // (mp-phase4 F3, decryption-gated anchor).
    builder.append(
        event_type::FINAL_DECK_COMMITTED,
        to_payload(&FinalDeckCommittedPayload {
            final_deck_hash: commit_hash_hex.clone(),
            deck: commits_hex,
            deck_ct: Some(cts.iter().map(|c| c.to_wire()).collect()),
        }),
        COORDINATOR,
    );

    // final_deck_ack (per party).
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

    // hole_card_opened — REAL threshold attestation per hole card.
    for idx in 0..2 * n {
        let owner_seat = if idx < n { idx as u8 } else { (idx - n) as u8 };
        let pid = mp_party_id(owner_seat);
        let card_id = deck_ids[idx];
        let proof = make_open(idx, &mut rng);
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

    // community_revealed (flop/turn/river) — REAL threshold attestations.
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

    // hand_complete.
    builder.append(
        event_type::HAND_COMPLETE,
        to_payload(&HandCompletePayload {
            revealed_card_count: (2 * n + 5) as u32,
        }),
        COORDINATOR,
    );

    let transcript = builder.finish();
    assert_eq!(transcript.decryption_scheme, DECRYPT_SCHEME);

    // (a) The honest real-crypto transcript verifies END TO END — every real
    //     threshold open is checked by the offline verifier (F3).
    let report = verify(&transcript).expect("real-decryption transcript must verify end to end");
    assert_eq!(report.final_phase, mental_poker::state::Phase::Complete);
    assert_eq!(report.revealed_card_ids.len(), 2 * n + 5);

    // (b) Tamper ONE decryption share of the first hole open → verify() REJECTS
    //     with BadDecryptionProof (the real DLEQ fails). Find the first
    //     hole_card_opened event and corrupt a share inside its attestation.
    let mut tampered = transcript.clone();
    let ev = tampered
        .events
        .iter_mut()
        .find(|e| e.event_type == event_type::HOLE_CARD_OPENED)
        .expect("a hole open exists");
    // Decode the attestation, flip a share's DLEQ response, re-encode, and fix
    // the payload_hash so we reach the proof check (not a chain/payload error).
    {
        use mental_poker::crypto_real::decrypt::ThresholdOpenWire;
        let att = ev.payload["card"]["proof"]["attestation"].as_str().unwrap();
        let bytes = hex::decode(att).unwrap();
        let mut wire: ThresholdOpenWire = serde_json::from_slice(&bytes).unwrap();
        let s = mental_poker::crypto_real::ec::scalar_from_hex(&wire.threshold.shares[0].dleq.s)
            .unwrap();
        wire.threshold.shares[0].dleq.s = scalar_to_hex(&(s + Scalar::ONE));
        let new_att = encode_threshold_attestation(&wire);
        ev.payload["card"]["proof"]["attestation"] = serde_json::Value::String(new_att);
        // Recompute payload_hash so the tamper survives the (1) payload-hash gate
        // and is caught by the (8/12) proof gate instead.
        ev.payload_hash = hex_hash(&ev.computed_payload_hash());
    }
    // NOTE: re-signing the event/chain is unnecessary for THIS assertion only if
    // the proof check runs before the signature check — but the verifier checks
    // signatures first. So rebuild the chain + signatures over the tampered
    // payload to isolate the decryption-proof rejection.
    rechain_and_sign(&mut tampered, &coord_sig);

    let err = verify(&tampered).expect_err("a tampered decryption share must be rejected");
    assert!(
        matches!(
            err.kind,
            mental_poker::verifier::VerifyErrorKind::BadDecryptionProof(_)
        ),
        "expected BadDecryptionProof, got {:?}",
        err.kind
    );
}

/// **F2 (round 1 re-audit — HIGH soundness hole).** The offline verifier MUST
/// NOT trust a registered shuffle public key without a proof-of-knowledge of its
/// discrete log. For the REAL re-encryption-shuffle scheme the verifier
/// reconstructs the joint key as `Σ Q_i` and binds every shuffle proof to it. A
/// malicious LAST registrant can otherwise pick a known scalar `a`, wait for the
/// honest `Q_j`, and register `Q_rogue = a·G − Σ Q_honest` so the joint key sums
/// to `a·G` — a key it controls the secret of. Since `Ct.c2 = M + r·Q`, it then
/// recovers EVERY card via `C2 − a·C1 = M` (total server-blindness collapse).
///
/// The fix gates the verifier's joint-key sum on a per-key, party-bound Schnorr
/// PoK (`KeyRegisteredPayload.key_pok`). A rogue cannot produce one for
/// `Q_rogue` because it does not know `log_G(Q_rogue)` (it knows `a`, the log of
/// the *sum*, not of its own share).
///
/// This drives the real-shuffle code path through `mental_poker::verify()` (so
/// `bind_shuffle_key` is on and the PoK gate runs on each `key_registered`),
/// then aborts after key registration — the F2 hole lives entirely in the
/// joint-key reconstruction, which happens before any shuffle. Before the fix,
/// the rogue transcript verified (no PoK was required); after it, it is rejected
/// with `BadKeyProofOfKnowledge`. The honest transcript verifies in both worlds.
#[test]
fn f2_verifier_rejects_rogue_dkg_key_without_pok() {
    use mental_poker::crypto_real::dkg::{schnorr_prove, DkgParty, SchnorrPok};
    use mental_poker::crypto_real::ec::point_to_hex;
    use mental_poker::crypto_real::shuffle::SCHEME as SHUFFLE_SCHEME;
    use mental_poker::events::{
        event_type, party_id as mp_party_id, HandAbortedPayload, HandInitPayload,
        KeyRegisteredPayload, PlayerEntry, COORDINATOR,
    };
    use mental_poker::hash::{canonical_json, ds_hash};
    use mental_poker::signing::{KeyDirectory, MockSignatureProvider, SignatureProvider};
    use mental_poker::transcript::{to_payload, Transcript, TranscriptBuilder};
    use mental_poker::verifier::VerifyErrorKind;
    use std::collections::BTreeMap;

    let n: usize = 3;
    let hand_id = "hand-f2-rogue-key";
    let table_id = "table-f2";

    // Per-party + coordinator mock signing keys.
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

    // Build a real-shuffle-scheme transcript whose declared decryption scheme is
    // the mock (we never reach decryption — we abort right after key reg). The
    // shuffle SCHEME being the real one is what flips `bind_shuffle_key` on so the
    // joint-key PoK gate runs.
    //
    // `shares` holds per-party (Q_i, PoK). For the HONEST set every PoK is a
    // genuine, party-bound proof. For the ROGUE set the last party's Q is
    // Q_rogue = a·G − Σ Q_honest and carries NO valid PoK (it cannot — it does
    // not know log_G(Q_rogue)).
    let build = |shares: &[(RistrettoPoint, Option<SchnorrPok>)]| -> Transcript {
        // A verifier-only real-shuffle provider just to label the scheme.
        let shuffle_label =
            mental_poker::crypto_real::shuffle::RealShuffleProofProvider::verifier();
        let decrypt_label = mental_poker::crypto::MockDecryptionProvider;
        let mut builder = TranscriptBuilder::new(
            hand_id,
            table_id,
            "mental_poker_mock",
            &coord_sig,
            &shuffle_label,
            &decrypt_label,
            key_directory.clone(),
        );

        // hand_init (simulated path — deck_repr None).
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
                deck_repr: None,
            }),
            COORDINATOR,
        );

        // key_registered — shuffle_pubkey = Q_i, key_pok = its PoK (or None).
        for i in 0..n {
            let pid = mp_party_id(i as u8);
            let signing_pubkey = hex::encode(&party_keys[i]);
            let shuffle_pubkey = point_to_hex(&shares[i].0);
            let claim = serde_json::json!({
                "hand_id": hand_id, "party_id": pid,
                "signing_pubkey": signing_pubkey, "shuffle_pubkey": shuffle_pubkey,
            });
            let claim_hash = ds_hash("mp:claim:v1", &[&canonical_json(&claim)]);
            let csig = party_sigs[i].sign(&pid, &claim_hash);
            builder.append_with_contributor(
                event_type::KEY_REGISTERED,
                to_payload(&KeyRegisteredPayload {
                    party_id: pid.clone(),
                    seat: i as u8,
                    signing_pubkey,
                    shuffle_pubkey,
                    contributor: None,
                    contributor_signature: None,
                    key_pok: shares[i].1.clone(),
                }),
                COORDINATOR,
                Some((pid.as_str(), csig.as_str())),
            );
        }

        // hand_aborted — a legal terminal from the Shuffle phase. We stop here:
        // the F2 hole is fully exercised by the joint-key reconstruction above.
        builder.append(
            event_type::HAND_ABORTED,
            to_payload(&HandAbortedPayload {
                aborted_by: COORDINATOR.to_string(),
                reason: "F2 regression: stop after key registration".to_string(),
                evidence: serde_json::json!({}),
            }),
            COORDINATOR,
        );

        builder.finish()
    };

    // ---- HONEST: each party's Q_i carries a genuine party-bound PoK. ----
    let mut rng = OsRng;
    let honest_parties: Vec<DkgParty> = (0..n)
        .map(|i| DkgParty::generate(mp_party_id(i as u8), &mut rng))
        .collect();
    let honest_shares: Vec<(RistrettoPoint, Option<SchnorrPok>)> = honest_parties
        .iter()
        .map(|p| {
            let pok = schnorr_prove(&p.party_id, &p.x_i, &p.q_i, &mut rng);
            (p.q_i, Some(pok))
        })
        .collect();
    let honest = build(&honest_shares);
    // The transcript is unambiguously on the REAL-shuffle path (so the verifier's
    // joint-key PoK gate runs — the F2 subject).
    assert_eq!(honest.shuffle_scheme, SHUFFLE_SCHEME);
    let report = mental_poker::verify(&honest)
        .expect("an all-PoK-backed real-shuffle key registration must verify");
    assert_eq!(report.final_phase, mental_poker::state::Phase::Aborted);

    // ---- ROGUE: the last registrant picks a known scalar a and sets
    //      Q_rogue = a·G − Σ Q_honest_others, so Σ Q_i = a·G (a key it controls).
    //      It cannot produce a valid party-bound PoK for Q_rogue. ----
    let sum_others: RistrettoPoint = honest_parties[..n - 1].iter().map(|p| p.q_i).sum();
    let a = Scalar::random(&mut rng); // the secret the attacker DOES know (log of the SUM)
    let q_rogue = a * G - sum_others;
    // Sanity: the joint key the verifier would reconstruct is exactly a·G.
    let joint: RistrettoPoint = honest_parties[..n - 1]
        .iter()
        .map(|p| p.q_i)
        .sum::<RistrettoPoint>()
        + q_rogue;
    assert_eq!(joint, a * G, "attack setup: Σ Q_i must equal a·G");

    // (a) NO PoK on the rogue key → rejected at the rogue's key_registered event.
    let mut rogue_no_pok = honest_shares.clone();
    rogue_no_pok[n - 1] = (q_rogue, None);
    let err = mental_poker::verify(&build(&rogue_no_pok))
        .expect_err("a rogue key with NO proof-of-knowledge must be rejected (F2)");
    assert!(
        matches!(err.kind, VerifyErrorKind::BadKeyProofOfKnowledge(_)),
        "expected BadKeyProofOfKnowledge, got {:?}",
        err.kind
    );

    // (b) The attacker cannot forge a PoK either: a PoK it produces for a
    //     DIFFERENT secret (e.g. its known `a`, whose Q is a·G ≠ Q_rogue) does
    //     not verify against Q_rogue, so attaching it is still rejected. This
    //     proves the rogue is caught because it lacks log_G(Q_rogue), not merely
    //     because a field was empty.
    let bogus_pok = schnorr_prove(&mp_party_id((n - 1) as u8), &a, &(a * G), &mut rng);
    let mut rogue_bogus = honest_shares.clone();
    rogue_bogus[n - 1] = (q_rogue, Some(bogus_pok));
    let err2 = mental_poker::verify(&build(&rogue_bogus))
        .expect_err("a rogue key with a PoK for the WRONG secret must be rejected (F2)");
    assert!(
        matches!(err2.kind, VerifyErrorKind::BadKeyProofOfKnowledge(_)),
        "expected BadKeyProofOfKnowledge for a mismatched PoK, got {:?}",
        err2.kind
    );
}
