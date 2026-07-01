//! Phase-4 server-blind core — integration tests + KAT vector emission.
//!
//! **Cross-vendor AI-audited (ADR-076/077/078); open-source + independently
//! verifiable (ADR-063 §4).** These tests
//! exercise the real threshold-ElGamal / DKG / Chaum–Pedersen path
//! (`mental_poker::crypto_real`) end to end, with **all parties simulated
//! locally using the real OS CSPRNG**. In production the real path runs only for
//! the engine-blind table class (ADR-070); the generic `mental_poker_production`
//! provider stays rejected.
//!
//! No database required: `cargo test -p mental-poker`.

use mental_poker::crypto_real::decrypt::{
    combine, dleq_prove, partial_decrypt, verify_and_open, DecryptionShare, OpenError,
    ThresholdDecryptionProof, SCHEME,
};
use mental_poker::crypto_real::dkg::{verify_dkg, DkgRun};
use mental_poker::crypto_real::ec::{card_point, point_to_hex, scalar_to_hex, Ct, DECK_SIZE};
use rand::rngs::OsRng;

use curve25519_dalek::constants::RISTRETTO_BASEPOINT_POINT as G;
use curve25519_dalek::ristretto::RistrettoPoint;
use curve25519_dalek::scalar::Scalar;

fn pubkeys(run: &DkgRun) -> Vec<(String, RistrettoPoint)> {
    run.parties
        .iter()
        .map(|p| (p.party_id.clone(), p.q_i))
        .collect()
}

fn open_proof(run: &DkgRun, deck_index: u32, ct: &Ct, rng: &mut OsRng) -> ThresholdDecryptionProof {
    ThresholdDecryptionProof {
        scheme: SCHEME.to_string(),
        shares: run
            .parties
            .iter()
            .map(|p| partial_decrypt(p, deck_index, ct, rng))
            .collect(),
    }
}

/// **The brief's headline round-trip.** DKG with n=3 parties → encrypt 52
/// card-ids under the joint key → each party partial-decrypts with its share +
/// Chaum–Pedersen proof → combine → RECOVER the original 52 card-ids; AND a
/// tampered partial-decryption proof is REJECTED.
#[test]
fn full_round_trip_dkg_encrypt_decrypt_recover_52() {
    let mut rng = OsRng;

    // 1) DKG with n=3 parties (each holds its own secret x_i, real OsRng).
    let run = DkgRun::simulate(3, &mut rng);
    // The joint key an independent verifier reconstructs == Σ Q_i, all PoKs hold.
    let q = verify_dkg(&run.commitments, &run.shares).expect("DKG verifies");
    assert_eq!(q, run.joint_key);
    // No single party (and not the coordinator, which holds nothing) knows x=Σx_i.

    let pks = pubkeys(&run);
    let mut recovered = [false; DECK_SIZE];

    for id in 0..DECK_SIZE as u8 {
        // 2) Encrypt card id under the JOINT key with fresh randomness.
        let r = Scalar::random(&mut rng);
        let ct = Ct::encrypt_card(id, &run.joint_key, &r);

        // 3) Each party partial-decrypts with its share + a Chaum–Pedersen proof.
        let proof = open_proof(&run, id as u32, &ct, &mut rng);

        // 4) Verify every share's DLEQ against the DKG public keys, then combine
        //    M = C2 − Σ D_i and recover the card id.
        let got = verify_and_open(id as u32, &ct, &pks, &proof).expect("opens");
        assert_eq!(got, id, "card {id} mis-recovered");
        assert!(!recovered[got as usize], "card {got} recovered twice");
        recovered[got as usize] = true;
    }
    // All 52 original card-ids recovered, each exactly once (a permutation-free
    // bijection — every id appears).
    assert!(
        recovered.iter().all(|&b| b),
        "all 52 card-ids must be recovered exactly once"
    );

    // AND a tampered partial-decryption proof is REJECTED.
    let r = Scalar::random(&mut rng);
    let ct = Ct::encrypt_card(0, &run.joint_key, &r);
    let mut tampered = open_proof(&run, 0, &ct, &mut rng);
    // Flip the response scalar of party:1's DLEQ.
    let s = mental_poker::crypto_real::ec::scalar_from_hex(&tampered.shares[1].dleq.s).unwrap();
    tampered.shares[1].dleq.s = scalar_to_hex(&(s + Scalar::ONE));
    assert_eq!(
        verify_and_open(0, &ct, &pks, &tampered),
        Err(OpenError::BadProof("party:1".into())),
        "a tampered partial-decryption proof MUST be rejected"
    );
}

/// A partial decryption computed from a DIFFERENT secret than the DKG share is
/// rejected even with a self-consistent proof (checked against the public Q_i).
#[test]
fn wrong_secret_partial_decryption_rejected() {
    let mut rng = OsRng;
    let run = DkgRun::simulate(3, &mut rng);
    let pks = pubkeys(&run);
    let r = Scalar::random(&mut rng);
    let ct = Ct::encrypt_card(20, &run.joint_key, &r);

    let wrong_x = Scalar::random(&mut rng);
    let wrong_d = wrong_x * ct.c1;
    let cheat = dleq_prove(
        "party:0",
        0,
        &wrong_x,
        &(wrong_x * G),
        &ct,
        &wrong_d,
        &mut rng,
    );

    let mut proof = open_proof(&run, 0, &ct, &mut rng);
    proof.shares[0] = DecryptionShare {
        party_id: "party:0".into(),
        d_i: point_to_hex(&wrong_d),
        dleq: cheat,
    };
    assert_eq!(
        verify_and_open(0, &ct, &pks, &proof),
        Err(OpenError::BadProof("party:0".into()))
    );
}

/// Server-blindness across a range of table sizes (n = 2..=6): a coordinator-
/// only view (ciphertext + every public key, ZERO secret shares) cannot recover
/// any card; the owner (all n shares) can.
#[test]
fn server_blind_across_table_sizes() {
    let mut rng = OsRng;
    for n in 2..=6 {
        let run = DkgRun::simulate(n, &mut rng);
        let pks = pubkeys(&run);
        let hole = 11u8;
        let r = Scalar::random(&mut rng);
        let ct = Ct::encrypt_card(hole, &run.joint_key, &r);

        // Owner with all n shares recovers it.
        let proof = open_proof(&run, 0, &ct, &mut rng);
        assert_eq!(verify_and_open(0, &ct, &pks, &proof).unwrap(), hole);

        // Coordinator-only: it can sum all public Q_i (= Q), but Σ Q_i ≠ Σ x_i·C1,
        // so C2 − ΣQ_i is not the card point.
        let public_sum: RistrettoPoint = pks.iter().map(|(_, q)| *q).sum();
        assert_ne!(
            mental_poker::crypto_real::ec::card_id_from_point(&(ct.c2 - public_sum)),
            Some(hole),
            "n={n}: coordinator-only view must not recover the card"
        );

        // Any strict subset of real shares (n−1) fails too.
        let subset: Vec<RistrettoPoint> =
            run.parties[..n - 1].iter().map(|p| p.x_i * ct.c1).collect();
        assert_ne!(
            combine(&ct, &subset),
            Some(hole),
            "n={n}: n−1 shares must not open"
        );
    }
}

/// **RT-3 (the increment's headline correctness claim, spec §8.1).** The FULL
/// server-blind deal composed end to end across `crypto_real`:
///   DKG → trivial-encrypt the canonical deck → `n` REAL verifiable shuffles
///   (each proof verified) → n-of-n open every index → recover a PERMUTATION of
///   `0..51` (all 52 ids, no dup, no miss).
///
/// This is the cross-module proof that the verifiable shuffle preserves the
/// plaintext multiset (each shuffle re-encrypts + permutes, never replaces a
/// card) AND that threshold decryption recovers it — the two halves of
/// server-blind dealing composing correctly. The coordinator never holds a key
/// share, so it learns nothing throughout.
#[test]
fn rt3_full_deal_dkg_shuffles_open_recovers_permutation() {
    use mental_poker::crypto::ShuffleProofProvider;
    use mental_poker::crypto_real::ec::{canonical_starting_deck, deck_hash};
    use mental_poker::crypto_real::shuffle::{RealShuffleProofProvider, Shuffle};

    let mut rng = OsRng;
    let n = 3usize; // 3 human parties (all-human, bot-free — ADR-063 §7).

    // 1) DKG → joint key Q (no party, esp. the coordinator, can decrypt alone).
    let run = DkgRun::simulate(n, &mut rng);
    let pks = pubkeys(&run);
    let q = run.joint_key;

    // 2) Starting deck D_0 = trivial encryption of card order 0..51 under Q.
    let mut deck = canonical_starting_deck();

    // 3) Round-robin: each party performs a real shuffle and proves it; every
    //    proof is verified before the next party shuffles (a bad shuffle aborts).
    //    F2: the verifier PINS the DKG joint key, so a shuffle proven under any
    //    other key (one a shuffler controls) is rejected — not just trusted from
    //    the attestation.
    use mental_poker::crypto_real::ec::point_to_hex;
    let q_hex = point_to_hex(&q);
    let v = RealShuffleProofProvider::verifier_with_expected_key(q_hex);
    for r in 0..n {
        let party = format!("party:{r}");
        let input = deck.clone();
        let shuffle = Shuffle::perform(input.clone(), &q, &mut rng);
        let ih = deck_hash(&input);
        let oh = deck_hash(&shuffle.output);
        let proof = shuffle.prove(&party, r as u32, &mut rng);
        assert!(
            v.verify_shuffle(&party, r as u32, &ih, &oh, None, &proof),
            "round {r}: honest shuffle proof must verify"
        );
        deck = shuffle.output;
    }

    // 4) n-of-n open every deck index; recovery must be a permutation of 0..51.
    let mut seen = [false; DECK_SIZE];
    for (idx, ct) in deck.iter().enumerate() {
        let proof = open_proof(&run, idx as u32, ct, &mut rng);
        let id = verify_and_open(idx as u32, ct, &pks, &proof)
            .expect("every index opens to a valid card");
        assert!(
            !seen[id as usize],
            "card {id} recovered twice (NOT a permutation)"
        );
        seen[id as usize] = true;
    }
    assert!(
        seen.iter().all(|&b| b),
        "all 52 ids must appear exactly once after {n} shuffles (a permutation)"
    );

    // 5) Server-blindness over the dealt deck (TR-11): the coordinator holds the
    //    full ciphertext deck + every PUBLIC key share (Σ Q_i = Q) but ZERO
    //    secret shares. The only correct opener is C2 − Σ x_i·C1 (needs the
    //    secrets). The coordinator's best public-only guess C2 − Σ Q_i must NOT
    //    equal the genuine recovered card.
    use mental_poker::crypto_real::ec::card_id_from_point;
    let public_sum: RistrettoPoint = pks.iter().map(|(_, qi)| *qi).sum();
    assert_eq!(public_sum, q, "internal: Σ Q_i == Q");
    for ct in &deck {
        let genuine: RistrettoPoint = run.parties.iter().map(|p| p.x_i * ct.c1).sum();
        let true_card = card_id_from_point(&(ct.c2 - genuine)); // Some(id) — the real card
        assert!(true_card.is_some(), "the genuine open must yield a card");
        let coord_guess = card_id_from_point(&(ct.c2 - public_sum)); // public-only
        assert_ne!(
            coord_guess, true_card,
            "coordinator-only view (no secret shares) must NOT recover the dealt card"
        );
    }
}

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

/// Re-chain (recompute previous_event_hash) and re-sign every event of a
/// transcript with the coordinator key — so a post-hoc payload edit yields a
/// structurally valid transcript whose ONLY defect is the edited content (here:
/// a tampered decryption share), isolating the cryptographic-proof rejection.
fn rechain_and_sign(
    t: &mut mental_poker::Transcript,
    coord_sig: &mental_poker::signing::MockSignatureProvider,
) {
    use mental_poker::signing::SignatureProvider;
    let mut prev = mental_poker::hash::ZERO_HASH;
    for ev in &mut t.events {
        ev.previous_event_hash = mental_poker::hash::hex_hash(&prev);
        ev.signature = String::new();
        let eh = ev.event_hash();
        ev.signature = coord_sig.sign(mental_poker::events::COORDINATOR, &eh);
        prev = eh;
    }
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

/// Emit the Phase-4 EC KAT vectors (KAT-1..3, KAT-5 challenge determinism) to
/// `tests/vectors/mp_phase4_ec.json` — the future 3-runtime parity file
/// (spec §7 / §8.3). The values are byte-pinned: a WASM/Dart port must
/// reproduce them. This test ASSERTS the freshly-computed values equal what it
/// writes, so it is self-checking (no fabricated expecteds).
#[test]
fn emit_phase4_ec_kat_vectors() {
    use mental_poker::crypto_real::ec::{hash_to_ristretto, pedersen_h};
    use serde_json::{json, Value};
    use std::io::Write;

    // KAT-1: H = hash_to_ristretto("mp:gen-H:v1").
    let h_hex = point_to_hex(pedersen_h());
    assert_eq!(h_hex, point_to_hex(&hash_to_ristretto("mp:gen-H:v1")));

    // KAT-2: all 52 card_point(id).compress().
    let card_points: Vec<String> = (0..DECK_SIZE as u8)
        .map(|id| point_to_hex(&card_point(id)))
        .collect();
    // sanity: 52 distinct.
    let distinct: std::collections::HashSet<&String> = card_points.iter().collect();
    assert_eq!(distinct.len(), DECK_SIZE);

    // KAT-3: a fixed ciphertext of card 7 under a fixed Q, fixed r.
    let q = Scalar::from(12345u64) * G;
    let r = Scalar::from(67890u64);
    let ct = Ct::encrypt_card(7, &q, &r);
    let ct_wire = ct.to_wire();

    // KAT-4: deck_hash v2 of a fixed deck.
    let deck: Vec<Ct> = (0..DECK_SIZE as u8)
        .map(|id| Ct::encrypt_card(id, &q, &Scalar::from((id as u64) + 1)))
        .collect();
    let deck_hash_hex = hex::encode(mental_poker::crypto_real::ec::deck_hash(&deck));

    let vectors: Value = json!({
        "_comment": "Phase-4 server-blind EC KAT vectors (ADR-063; cross-vendor AI-audited per ADR-076/077/078, open-source + verifiable); future 3-runtime (WASM/Dart) parity gate. Values are byte-pinned: a port must reproduce them.",
        "kat1_pedersen_h": h_hex,
        "kat2_card_points": card_points,
        "kat3_fixed_ciphertext": {
            "card_id": 7,
            "q_scalar": "12345",
            "r_scalar": "67890",
            "c1": ct_wire.c1,
            "c2": ct_wire.c2
        },
        "kat4_deck_hash_v2": deck_hash_hex
    });

    let dir = concat!(env!("CARGO_MANIFEST_DIR"), "/tests/vectors");
    std::fs::create_dir_all(dir).expect("vectors dir");
    let path = format!("{dir}/mp_phase4_ec.json");
    let pretty = serde_json::to_string_pretty(&vectors).unwrap();
    let mut f = std::fs::File::create(&path).expect("create vector file");
    f.write_all(pretty.as_bytes()).expect("write vector file");

    // Self-check: re-read and assert the pinned values still match a fresh compute.
    let read: Value = serde_json::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
    assert_eq!(read["kat1_pedersen_h"], json!(point_to_hex(pedersen_h())));
    assert_eq!(read["kat3_fixed_ciphertext"]["c1"], json!(ct.to_wire().c1));
}

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
