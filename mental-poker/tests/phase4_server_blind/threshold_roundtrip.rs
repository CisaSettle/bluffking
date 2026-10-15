//! Phase-4 server-blind cryptographic integration tests: threshold roundtrip.

use super::*;

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
