//! Phase-4 server-blind cryptographic integration tests: vectors.

use super::*;

/// Assert the Phase-4 EC KAT vectors (KAT-1..4) against the COMMITTED
/// `tests/vectors/mp_phase4_ec.json` — the future 3-runtime parity file
/// (spec §7 / §8.3). The values are byte-pinned: a WASM/Dart port must
/// reproduce them.
///
/// U14 (dual-AI OSS review): this test previously `File::create`-OVERWROTE the
/// committed vector file on every run and then re-read what it had just written
/// (a tautology). It now fails on any divergence from the committed file;
/// regeneration is an explicit opt-in:
///
/// ```sh
/// UPDATE_KAT_VECTORS=1 cargo test -p mental-poker --test phase4_server_blind \
///     emit_phase4_ec_kat_vectors
/// ```
#[test]
fn emit_phase4_ec_kat_vectors() {
    use mental_poker::crypto_real::ec::{hash_to_ristretto, pedersen_h};
    use serde_json::{json, Value};

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
    let path = format!("{dir}/mp_phase4_ec.json");

    // U14 (dual-AI OSS review): regeneration is gated — never a silent overwrite.
    if std::env::var("UPDATE_KAT_VECTORS").as_deref() == Ok("1") {
        std::fs::create_dir_all(dir).expect("vectors dir");
        let pretty = serde_json::to_string_pretty(&vectors).unwrap();
        std::fs::write(&path, &pretty).expect("write vector file");
        println!("[emit_phase4_ec_kat_vectors] regenerated {path}");
    }

    // Assert the freshly-computed values equal the COMMITTED vector file — any
    // mismatch is a cross-runtime parity break, not something to paper over.
    let committed: Value =
        serde_json::from_str(&std::fs::read_to_string(&path).unwrap_or_else(|e| {
            panic!(
                "committed KAT vector file missing/unreadable at {path}: {e} \
                 (regenerate with UPDATE_KAT_VECTORS=1)"
            )
        }))
        .expect("committed KAT vector file parses");
    assert_eq!(
        committed, vectors,
        "freshly-computed Phase-4 EC KAT vectors diverge from the committed \
         {path} — a byte-pinned value changed; if intentional, regenerate with \
         UPDATE_KAT_VECTORS=1 and review the diff"
    );
    // Spot-checks kept for readable failure messages on the highest-value pins.
    assert_eq!(
        committed["kat1_pedersen_h"],
        json!(point_to_hex(pedersen_h()))
    );
    assert_eq!(
        committed["kat3_fixed_ciphertext"]["c1"],
        json!(ct.to_wire().c1)
    );
}
