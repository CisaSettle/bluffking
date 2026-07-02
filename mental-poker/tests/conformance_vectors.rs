//! ADR-041 §7 — cross-language conformance vectors.
//!
//! Generates and asserts a fixed JSON **array** of `{ "kind", "input", "expected" }`
//! objects for `ds_hash`, `canonical_json`, `card_commit`, `mock_sign`,
//! `wire_deck_hash`, and `shuffle_proof_attestation`.
//!
//! The vector file is committed at `mental-poker/tests/vectors/mp_conformance.json`.
//! The frontend engineer consumes the **same** file and asserts the TypeScript port
//! reproduces every value. Any divergence in either language means the conformance
//! gate fails.
//!
//! **Format (LOCKED per ADR-041 §7):** a JSON array where each element is
//! `{ "kind": "<type>", "input": <...>, "expected": "<hex>" }`.
//! The `expected` field is always a lowercase hex string.
//!
//! **Stability contract:** `expected` values must never change. If an implementation
//! change alters an expected output, the test fails — that's the point.

use hmac::{Hmac, Mac};
use mental_poker::crypto::{
    card_commit, deck_hash, MockShuffleProofProvider, ShuffleProofProvider,
};
use mental_poker::hash::{canonical_json, ds_hash, hex_hash};
use mental_poker::state::canonical_wire_deck_hash;
use serde_json::{json, Value};
use sha2::Sha256;

type HmacSha256 = Hmac<Sha256>;

/// Compute HMAC-SHA256(key, message), returning lowercase hex.
fn mock_sign(key: &[u8], message: &[u8]) -> String {
    let mut mac = HmacSha256::new_from_slice(key).expect("HMAC accepts any key length");
    mac.update(message);
    hex::encode(mac.finalize().into_bytes())
}

/// Wire deck hash per ADR-041 §5.1:
/// `ds_hash("mp:deck-hash:v1", &[ each card-id byte ])` — 52 single-byte entries.
fn wire_deck_hash(deck: &[u8]) -> [u8; 32] {
    let parts: Vec<&[u8]> = deck.iter().map(std::slice::from_ref).collect();
    ds_hash("mp:deck-hash:v1", &parts)
}

/// Commit-deck hash per ADR-041 §5.1 / `mp_dealing.rs:compute_commit_deck_hash_hex`.
///
/// For each card index j:
///   salt_j  = ds_hash("mp:salt:v1", &[hand_id_str.as_bytes(), (j as u64).to_le_bytes()])
///   commit_j = card_commit(deck[j], &salt_j)
/// result = deck_hash([commit_0 .. commit_51])
///
/// IMPORTANT: `hand_id_str` is the **36-byte hyphenated UUID string**, NOT the
/// 16-byte binary UUID.  The server variable `hand_id_bytes` must equal
/// `hand_id_str.as_bytes()` (this is the root of the BLOCKER-FIX).
fn compute_commit_deck_hash(hand_id_str: &str, deck: &[u8]) -> [u8; 32] {
    assert_eq!(deck.len(), 52, "deck must be exactly 52 cards");
    let commits: Vec<[u8; 32]> = (0usize..52)
        .map(|j| {
            let salt: [u8; 32] = ds_hash(
                "mp:salt:v1",
                &[hand_id_str.as_bytes(), &(j as u64).to_le_bytes()],
            );
            card_commit(deck[j], &salt)
        })
        .collect();
    deck_hash(&commits)
}

/// Build the full conformance vector array in the LOCKED format.
///
/// Each entry: `{ "kind": "<type>", "input": <json>, "expected": "<hex>" }`.
fn build_vectors() -> Vec<Value> {
    let mut vectors: Vec<Value> = Vec::new();

    // -------------------------------------------------------------------------
    // ds_hash vectors
    // -------------------------------------------------------------------------
    {
        let domain = "mp:test:v1";
        let parts: Vec<&[u8]> = vec![b"hello"];
        let result = ds_hash(domain, &parts);
        vectors.push(json!({
            "kind": "ds_hash",
            "input": {
                "domain": domain,
                "parts": [hex::encode(b"hello")]
            },
            "expected": hex_hash(&result)
        }));
    }
    {
        let domain = "mp:test:v1";
        let parts: Vec<&[u8]> = vec![b"hello", b"world"];
        let result = ds_hash(domain, &parts);
        vectors.push(json!({
            "kind": "ds_hash",
            "input": {
                "domain": domain,
                "parts": [hex::encode(b"hello"), hex::encode(b"world")]
            },
            "expected": hex_hash(&result)
        }));
    }
    {
        // Empty parts list.
        let domain = "mp:deck-hash:v1";
        let parts: Vec<&[u8]> = vec![];
        let result = ds_hash(domain, &parts);
        vectors.push(json!({
            "kind": "ds_hash",
            "input": {
                "domain": domain,
                "parts": []
            },
            "expected": hex_hash(&result)
        }));
    }
    {
        // Single byte part + 32-byte salt.
        let domain = "mp:card-commit:v1";
        let part: &[u8] = &[7u8];
        let salt = [1u8; 32];
        let result = ds_hash(domain, &[part, &salt]);
        vectors.push(json!({
            "kind": "ds_hash",
            "input": {
                "domain": domain,
                "parts": [hex::encode(part), hex::encode(salt)]
            },
            "expected": hex_hash(&result)
        }));
    }
    {
        // The canonical claim domain from ADR-041 §4.1.
        let domain = "mp:claim:v1";
        let message = b"test-claim-body";
        let result = ds_hash(domain, &[message.as_slice()]);
        vectors.push(json!({
            "kind": "ds_hash",
            "input": {
                "domain": domain,
                "parts": [hex::encode(message)]
            },
            "expected": hex_hash(&result)
        }));
    }

    // -------------------------------------------------------------------------
    // canonical_json vectors — expected is the canonical JSON string (UTF-8).
    // -------------------------------------------------------------------------
    {
        let v = json!({"b": 1, "a": 2, "c": 3});
        let bytes = canonical_json(&v);
        vectors.push(json!({
            "kind": "canonical_json",
            "input": {"b": 1, "a": 2, "c": 3},
            "expected": String::from_utf8(bytes).unwrap()
        }));
    }
    {
        let v = json!({"outer": {"y": 1, "x": 2}});
        let bytes = canonical_json(&v);
        vectors.push(json!({
            "kind": "canonical_json",
            "input": {"outer": {"y": 1, "x": 2}},
            "expected": String::from_utf8(bytes).unwrap()
        }));
    }
    {
        let v = json!([3, 1, 2]);
        let bytes = canonical_json(&v);
        vectors.push(json!({
            "kind": "canonical_json",
            "input": [3, 1, 2],
            "expected": String::from_utf8(bytes).unwrap()
        }));
    }
    {
        let v = json!({
            "hand_id": "test-hand-1",
            "party_id": "party:0",
            "signing_pubkey": "aabbccdd",
            "shuffle_pubkey": "eeff0011"
        });
        let bytes = canonical_json(&v);
        vectors.push(json!({
            "kind": "canonical_json",
            "input": {
                "hand_id": "test-hand-1",
                "party_id": "party:0",
                "signing_pubkey": "aabbccdd",
                "shuffle_pubkey": "eeff0011"
            },
            "expected": String::from_utf8(bytes).unwrap()
        }));
    }

    // -------------------------------------------------------------------------
    // card_commit vectors
    // -------------------------------------------------------------------------
    {
        let card_id: u8 = 0;
        let salt = [0u8; 32];
        let commit = card_commit(card_id, &salt);
        vectors.push(json!({
            "kind": "card_commit",
            "input": {
                "card_id": card_id,
                "salt": hex::encode(salt)
            },
            "expected": hex_hash(&commit)
        }));
    }
    {
        let card_id: u8 = 7;
        let salt = [1u8; 32];
        let commit = card_commit(card_id, &salt);
        vectors.push(json!({
            "kind": "card_commit",
            "input": {
                "card_id": card_id,
                "salt": hex::encode(salt)
            },
            "expected": hex_hash(&commit)
        }));
    }
    {
        let card_id: u8 = 51;
        let salt = [255u8; 32];
        let commit = card_commit(card_id, &salt);
        vectors.push(json!({
            "kind": "card_commit",
            "input": {
                "card_id": card_id,
                "salt": hex::encode(salt)
            },
            "expected": hex_hash(&commit)
        }));
    }

    // -------------------------------------------------------------------------
    // mock_sign (HMAC-SHA256) vectors — expected is lowercase hex.
    // -------------------------------------------------------------------------
    {
        let key = b"test-key-1";
        let message = b"test-message-1";
        let sig = mock_sign(key, message);
        vectors.push(json!({
            "kind": "mock_sign",
            "input": {
                "key": hex::encode(key),
                "message": hex::encode(message)
            },
            "expected": sig
        }));
    }
    {
        let key = b"mp:mock-sig-key:v1";
        let message = b"some-hash-bytes";
        let sig = mock_sign(key, message);
        vectors.push(json!({
            "kind": "mock_sign",
            "input": {
                "key": hex::encode(key),
                "message": hex::encode(message)
            },
            "expected": sig
        }));
    }
    {
        // Zero key, empty message (edge case).
        let key = [0u8; 32];
        let message = b"";
        let sig = mock_sign(&key, message);
        vectors.push(json!({
            "kind": "mock_sign",
            "input": {
                "key": hex::encode(key),
                "message": hex::encode(message)
            },
            "expected": sig
        }));
    }

    // -------------------------------------------------------------------------
    // wire_deck_hash vector (ADR-041 §5.1)
    //
    // `ds_hash("mp:deck-hash:v1", &[ each card-id byte ])` over a 52-byte
    // wire deck. Input is the identity permutation 0..=51.
    // -------------------------------------------------------------------------
    {
        let identity_deck: Vec<u8> = (0u8..52).collect();
        let hash = wire_deck_hash(&identity_deck);
        vectors.push(json!({
            "kind": "wire_deck_hash",
            "input": {
                "deck": identity_deck
            },
            "expected": hex_hash(&hash)
        }));
    }
    {
        // A simple non-identity permutation.
        let mut deck: Vec<u8> = (0u8..52).collect();
        deck.swap(0, 51);
        deck.swap(1, 50);
        let hash = wire_deck_hash(&deck);
        vectors.push(json!({
            "kind": "wire_deck_hash",
            "input": {
                "deck": deck
            },
            "expected": hex_hash(&hash)
        }));
    }

    // -------------------------------------------------------------------------
    // shuffle_proof_attestation vector (ADR-041 §5.1 / crypto.rs)
    //
    // MockShuffleProofProvider attestation: the canonical proof used in the
    // interactive transcript. Input: party, round, input_deck_hash, output_deck_hash.
    // -------------------------------------------------------------------------
    {
        let p = MockShuffleProofProvider;
        let ih = ds_hash("mp:deck-hash:v1", &[b"input"]);
        let oh = ds_hash("mp:deck-hash:v1", &[b"output"]);
        let proof = p.prove_shuffle("party:0", 0, &ih, &oh);
        vectors.push(json!({
            "kind": "shuffle_proof_attestation",
            "input": {
                "party": "party:0",
                "round": 0,
                "input_deck_hash": hex_hash(&ih),
                "output_deck_hash": hex_hash(&oh)
            },
            "expected": proof.attestation
        }));
    }
    {
        let p = MockShuffleProofProvider;
        let ih = ds_hash("mp:deck-hash:v1", &[b"in2"]);
        let oh = ds_hash("mp:deck-hash:v1", &[b"out2"]);
        let proof = p.prove_shuffle("party:1", 1, &ih, &oh);
        vectors.push(json!({
            "kind": "shuffle_proof_attestation",
            "input": {
                "party": "party:1",
                "round": 1,
                "input_deck_hash": hex_hash(&ih),
                "output_deck_hash": hex_hash(&oh)
            },
            "expected": proof.attestation
        }));
    }

    // -------------------------------------------------------------------------
    // commit_deck_hash vectors (ADR-041 §5.1 / mp_dealing.rs BLOCKER-FIX)
    //
    // These vectors lock in the fact that `hand_id_bytes` in the server is
    // the UTF-8 bytes of the 36-character hyphenated UUID string, NOT the
    // 16-byte binary UUID.  The frontend `computeCommitDeckHash` hashes the
    // same string-bytes — so these two vectors are the cross-language bridge.
    //
    // Vector 1: identity deck (card IDs 0..=51), UUID 550e8400-e29b-41d4-a716-446655440000
    // Vector 2: swapped deck (swap cards 0↔51 and 1↔50), same UUID
    // -------------------------------------------------------------------------
    {
        let hand_id_str = "550e8400-e29b-41d4-a716-446655440000";
        let identity_deck: Vec<u8> = (0u8..52).collect();
        let hash = compute_commit_deck_hash(hand_id_str, &identity_deck);
        vectors.push(json!({
            "kind": "commit_deck_hash",
            "input": {
                "hand_id": hand_id_str,
                "deck": identity_deck
            },
            "expected": hex_hash(&hash)
        }));
    }
    {
        let hand_id_str = "550e8400-e29b-41d4-a716-446655440000";
        let mut deck: Vec<u8> = (0u8..52).collect();
        deck.swap(0, 51);
        deck.swap(1, 50);
        let hash = compute_commit_deck_hash(hand_id_str, &deck);
        vectors.push(json!({
            "kind": "commit_deck_hash",
            "input": {
                "hand_id": hand_id_str,
                "deck": deck
            },
            "expected": hex_hash(&hash)
        }));
    }

    // -------------------------------------------------------------------------
    // interactive_round_hashes vector (ADR-041 §5.1 "STOP THE WHACK-A-MOLE")
    //
    // Records the complete per-round `(input_deck_hash, output_deck_hash)` sequence
    // for a fixed 3-party interactive deal so the TS client can verify it computes
    // the identical hash chain.
    //
    // Contract (locked per ADR-041 §5.1):
    //   Round 0 input  = wire_deck_hash([0,1,…,51])  — NOT canonical_initial_deck_hash
    //   Round r input  = previous round's output_deck_hash
    //   Rounds 0..n-2  output = wire_deck_hash(output_deck)
    //   Round n-1 output = commit_deck_hash(final_deck, hand_id)
    //
    // The `rounds` array has one entry per shuffle round with fields:
    //   { round, input_deck_hash, output_deck_hash }
    // -------------------------------------------------------------------------
    {
        let hand_id_str = "550e8400-e29b-41d4-a716-446655440000";
        let n = 3usize;

        // Fixed per-round output decks (deterministic rotations for reproducibility).
        // Each party applies a left-rotation by a fixed amount.
        let mut cur: Vec<u8> = (0u8..52).collect();
        let mut round_output_decks: Vec<Vec<u8>> = Vec::new();
        for r in 0..n {
            let rotation = (r * 7 + 3) % 52;
            let mut next = cur.clone();
            next.rotate_left(rotation);
            round_output_decks.push(next.clone());
            cur = next;
        }
        let final_deck = cur.clone();

        // Compute commit_deck_hash for the final deck.
        let commit_hash = compute_commit_deck_hash(hand_id_str, &final_deck);

        // Build the round hash sequence per ADR-041 §5.1.
        let mut prev_hash: [u8; 32] = canonical_wire_deck_hash();
        let mut rounds_json: Vec<Value> = Vec::new();
        // r is a round ordinal: the last round's output hash is commit_hash, not
        // round_output_decks[r], so enumerate() over the deck slice can't express it.
        #[allow(clippy::needless_range_loop)]
        for r in 0..n {
            let input_hash = prev_hash;
            let output_hash: [u8; 32] = if r == n - 1 {
                commit_hash
            } else {
                wire_deck_hash(&round_output_decks[r])
            };
            rounds_json.push(json!({
                "round": r,
                "input_deck_hash": hex_hash(&input_hash),
                "output_deck_hash": hex_hash(&output_hash)
            }));
            prev_hash = output_hash;
        }

        vectors.push(json!({
            "kind": "interactive_round_hashes",
            "input": {
                "hand_id": hand_id_str,
                "n": n,
                "round_output_decks": round_output_decks
            },
            "expected": rounds_json
        }));
    }

    vectors
}

const VECTOR_PATH: &str = "tests/vectors/mp_conformance.json";

#[test]
fn conformance_vectors_stable() {
    let vectors = build_vectors();
    let vectors_json = serde_json::to_string_pretty(&vectors).expect("vectors serialize");

    // U38 (dual-AI OSS review): regeneration is an explicit opt-in — a missing
    // golden file is a FAILURE, never a silent write-and-pass (which would let
    // a deleted/renamed golden skip every comparison).
    if std::env::var("UPDATE_KAT_VECTORS").as_deref() == Ok("1") {
        std::fs::write(VECTOR_PATH, &vectors_json).expect("write conformance vectors");
        println!(
            "[conformance_vectors] regenerated {VECTOR_PATH} ({} bytes)",
            vectors_json.len()
        );
    }

    let content = std::fs::read_to_string(VECTOR_PATH).unwrap_or_else(|e| {
        panic!(
            "committed golden file {VECTOR_PATH} missing/unreadable: {e} \
             (regenerate with UPDATE_KAT_VECTORS=1 and commit + review the diff)"
        )
    });

    // Parse and re-assert every vector against the committed golden content.
    let stored: Value = serde_json::from_str(&content).expect("parse stored vectors");

    // Must be a JSON array.
    let stored_arr = stored
        .as_array()
        .expect("mp_conformance.json must be a JSON array (ADR-041 §7)");
    let live_arr = &vectors;

    assert_eq!(
        stored_arr.len(),
        live_arr.len(),
        "conformance vector count changed: stored={} live={}",
        stored_arr.len(),
        live_arr.len()
    );

    for (i, (stored_v, live_v)) in stored_arr.iter().zip(live_arr.iter()).enumerate() {
        let kind = live_v["kind"].as_str().unwrap_or("<unknown>");
        assert_eq!(
            stored_v["kind"], live_v["kind"],
            "vector[{i}] kind mismatch"
        );
        assert_eq!(
            stored_v["expected"], live_v["expected"],
            "vector[{i}] ({kind}) expected value changed: \
             stored={} live={}",
            stored_v["expected"], live_v["expected"]
        );
    }

    // Count by kind for the summary line.
    let mut counts: std::collections::HashMap<&str, usize> = Default::default();
    for v in live_arr {
        *counts.entry(v["kind"].as_str().unwrap_or("?")).or_insert(0) += 1;
    }
    println!(
        "[conformance_vectors] all {} vectors verified ({:?})",
        live_arr.len(),
        counts,
    );
}

/// Positive test: build vectors and assert the `expected` fields are non-empty.
/// - Hex-valued kinds must be valid hex.
/// - `canonical_json` kind has a string expected (the JSON).
/// - `interactive_round_hashes` has an array of `{round, input_deck_hash, output_deck_hash}`.
#[test]
fn conformance_vectors_have_nonempty_expected() {
    let vectors = build_vectors();
    assert!(!vectors.is_empty(), "must produce at least one vector");
    for v in &vectors {
        let kind = v["kind"].as_str().unwrap_or("<unknown>");
        match kind {
            "interactive_round_hashes" => {
                let rounds = v["expected"]
                    .as_array()
                    .unwrap_or_else(|| panic!("vector kind={kind} expected must be an array"));
                assert!(
                    !rounds.is_empty(),
                    "interactive_round_hashes must have at least one round"
                );
                for rnd in rounds {
                    let idh = rnd["input_deck_hash"]
                        .as_str()
                        .unwrap_or_else(|| panic!("round missing input_deck_hash"));
                    let odh = rnd["output_deck_hash"]
                        .as_str()
                        .unwrap_or_else(|| panic!("round missing output_deck_hash"));
                    assert!(
                        idh.chars().all(|c| c.is_ascii_hexdigit()),
                        "input_deck_hash not hex: {idh}"
                    );
                    assert!(
                        odh.chars().all(|c| c.is_ascii_hexdigit()),
                        "output_deck_hash not hex: {odh}"
                    );
                }
            }
            "canonical_json" => {
                let expected = v["expected"]
                    .as_str()
                    .unwrap_or_else(|| panic!("vector kind={kind} missing expected field"));
                assert!(
                    !expected.is_empty(),
                    "vector kind={kind} has empty expected field"
                );
            }
            _ => {
                let expected = v["expected"]
                    .as_str()
                    .unwrap_or_else(|| panic!("vector kind={kind} missing expected field"));
                assert!(
                    !expected.is_empty(),
                    "vector kind={kind} has empty expected field"
                );
                assert!(
                    expected.chars().all(|c| c.is_ascii_hexdigit()),
                    "vector kind={kind} expected is not hex: {expected}"
                );
            }
        }
    }
}

/// Negative test: swapping one byte in the key for a mock_sign vector produces
/// a different HMAC (ensures the Rust implementation is sensitive to key material).
#[test]
fn mock_sign_is_key_sensitive() {
    let key1 = b"key-one";
    let key2 = b"key-two";
    let message = b"same-message";
    let s1 = {
        let mut mac = HmacSha256::new_from_slice(key1).unwrap();
        mac.update(message);
        hex::encode(mac.finalize().into_bytes())
    };
    let s2 = {
        let mut mac = HmacSha256::new_from_slice(key2).unwrap();
        mac.update(message);
        hex::encode(mac.finalize().into_bytes())
    };
    assert_ne!(s1, s2, "different keys must produce different HMACs");
}
