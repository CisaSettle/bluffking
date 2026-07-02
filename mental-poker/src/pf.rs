//! Offline verification for ADR-064 commit-reveal provably-fair hands.
//!
//! This module is the offline counterpart of the server's `pf_dealing.rs`. It
//! reproduces — byte-for-byte, in the same `engine` crate the server uses — the
//! deck of any `existing_server` hand from its revealed secrets and confirms it
//! matches the persisted cards. It does NOT touch `verifier.rs` / `mp_verify`
//! (the Mental Poker path).
//!
//! Checks (must mirror the spec §3/§6 exactly):
//!   1. `ds_hash("pf:seed-commit:v1",[server_seed]) == seed_commit`  (T2)
//!   2. `deck_seed = ds_hash("pf:deck-seed:v1",[server_seed, client_seeds(sorted
//!      by seat ascending).., hand_id.as_bytes()])`
//!   3. `Deck::new(PokerRng::from_seed_bytes(deck_seed))` re-derives the deal in
//!      engine deal order and matches the persisted `hole_cards`/`community` (T1).
//!
//! Engine deal order (no burns — see `engine/src/game.rs`):
//!   The engine deals by PLAYER-VECTOR INDEX, NOT by seat number. The
//!   players-vec is built from OCCUPIED seats only, so for a SPARSE table
//!   (seats `[2, 5]`) the deal positions are `[0, 1]` and the dealt-seat order
//!   is `[2, 5]`. With `dealt_seats[deal_pos] = seat`:
//!   hole cards: pass 1 = card[deal_pos] for deal_pos 0..n, pass 2 =
//!   card[n+deal_pos] for deal_pos 0..n (so the seat at deal position `p` gets
//!   `(deck[p], deck[n+p])`); then board = `deck[2n], deck[2n+1], deck[2n+2]`
//!   (flop), `deck[2n+3]` (turn), `deck[2n+4]` (river).
//!
//!   For solo/bots/MP-eligible tables the seats are contiguous from 0, so
//!   `dealt_seats == [0, 1, .., n-1]` and deal_pos == seat. A PF multi-seat row
//!   that omits `dealt_seats` is rejected FAIL-CLOSED (we never guess the
//!   mapping — guessing would let a sparse honest hand silently pass with the
//!   wrong cards, or a tampered one slip through).

use std::collections::BTreeMap;

use engine::{Card, Deck, PokerRng};
use serde::{Deserialize, Serialize};

use crate::hash::ds_hash;

/// Domain string for the pre-deal commitment hash (LOCKED, ADR-064 §3).
pub const DS_SEED_COMMIT: &str = "pf:seed-commit:v1";
/// Domain string for the deck-seed derivation hash (LOCKED, ADR-064 §3).
pub const DS_DECK_SEED: &str = "pf:deck-seed:v1";

/// A persisted hand record. The canonical, end-to-end **verifiable** fixture
/// source is `cargo run -p mental-poker --bin pf_demo_hand` (it deals a real
/// engine hand and emits a record whose `server_seed`/`seed_commit`/cards all
/// satisfy the verifier). `dump_hand_detail` emits a *different* (replay-mock)
/// shape and ILLUSTRATIVE-only pf fields — it is NOT a `pf_verify` input.
/// Never hand-authored, per [[lock-interface-spec-first]].
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct HandRecord {
    /// UUID string (e.g. "01972f3a-...").
    pub hand_id: String,
    /// 64-hex committed+revealed server seed.
    pub server_seed: String,
    /// 64-hex commitment `ds_hash("pf:seed-commit:v1",[server_seed])`.
    pub seed_commit: String,
    /// seat (string key) → 64-hex client seed actually mixed in. May be `{}`.
    #[serde(default)]
    pub client_seeds: BTreeMap<String, String>,
    /// Number of seats dealt this hand (deal-vec length). For solo/bots and
    /// Mental-Poker-eligible tables this equals the seated-player count and the
    /// deal-vec index equals the seat index.
    pub num_players: usize,
    /// The ACTUAL dealt-seat order: `dealt_seats[deal_pos] = seat`. This is the
    /// players-vec seat order the engine used at hand start (occupied seats only,
    /// ascending). For a sparse table (seats `[2, 5]`) this is `[2, 5]` so the
    /// verifier can map each deal position back to its real seat.
    ///
    /// `#[serde(default)]` makes legacy / contiguous-only fixtures (no
    /// `dealt_seats` field) deserialize as empty. The verifier treats an empty
    /// `dealt_seats` as "contiguous from 0" (deal_pos == seat) ONLY when that is
    /// safe (single-seat hole-card checks against contiguous seats); a PF
    /// multi-seat row whose persisted seats are NOT contiguous-from-0 and that
    /// omits `dealt_seats` is rejected FAIL-CLOSED.
    #[serde(default)]
    pub dealt_seats: Vec<u8>,
    /// seat (string key) → the two hole cards persisted for that seat
    /// (`["As","Kd"]`). May be partial (e.g. just the verifying user's seat).
    #[serde(default)]
    pub hole_cards: BTreeMap<String, Vec<Card>>,
    /// The board, in deal order (0..=5 cards). Compared against the recomputed
    /// flop/turn/river prefix.
    #[serde(default)]
    pub community: Vec<Card>,
}

/// A successful verification report.
#[derive(Debug, Clone)]
pub struct PfVerifyReport {
    /// The recomputed deck seed (hex) — cross-check against `hands.deck_seed_b`.
    pub deck_seed_hex: String,
    /// Number of seats whose hole cards were checked.
    pub hole_seats_checked: usize,
    /// Number of board cards checked.
    pub board_cards_checked: usize,
}

/// Verification failure with a human-readable reason.
#[derive(Debug, Clone)]
pub struct PfVerifyError(pub String);

impl std::fmt::Display for PfVerifyError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.0)
    }
}
impl std::error::Error for PfVerifyError {}

fn decode_32(hex_str: &str, what: &str) -> Result<[u8; 32], PfVerifyError> {
    let v = hex::decode(hex_str.trim())
        .map_err(|e| PfVerifyError(format!("{what} is not valid hex: {e}")))?;
    if v.len() != 32 {
        return Err(PfVerifyError(format!(
            "{what} must be 32 bytes (64 hex), got {} bytes",
            v.len()
        )));
    }
    let mut out = [0u8; 32];
    out.copy_from_slice(&v);
    Ok(out)
}

/// Parse a UUID string into its 16 raw bytes (mirrors `uuid::Uuid::as_bytes`).
fn hand_id_bytes(hand_id: &str) -> Result<[u8; 16], PfVerifyError> {
    let uuid = uuid::Uuid::parse_str(hand_id.trim())
        .map_err(|e| PfVerifyError(format!("hand_id is not a valid UUID: {e}")))?;
    Ok(*uuid.as_bytes())
}

/// Derive the deck seed exactly as the server does (ADR-064 §3): client seeds in
/// ASCENDING SEAT ORDER, `hand_id` bytes last.
pub fn derive_deck_seed(
    server_seed: &[u8; 32],
    client_seeds_by_seat: &BTreeMap<u8, [u8; 32]>,
    hand_id_bytes: &[u8; 16],
) -> [u8; 32] {
    let mut parts: Vec<&[u8]> = Vec::with_capacity(2 + client_seeds_by_seat.len());
    parts.push(server_seed);
    for seed in client_seeds_by_seat.values() {
        parts.push(seed);
    }
    parts.push(hand_id_bytes);
    ds_hash(DS_DECK_SEED, &parts)
}

/// Verify a commit-reveal hand record. Returns the report on success.
pub fn verify_hand(rec: &HandRecord) -> Result<PfVerifyReport, PfVerifyError> {
    if rec.num_players == 0 {
        return Err(PfVerifyError("num_players must be >= 1".into()));
    }

    let server_seed = decode_32(&rec.server_seed, "server_seed")?;
    let seed_commit = decode_32(&rec.seed_commit, "seed_commit")?;

    // (1) commit check (T2).
    let recomputed_commit = ds_hash(DS_SEED_COMMIT, &[&server_seed]);
    if recomputed_commit != seed_commit {
        return Err(PfVerifyError(
            "commit mismatch: SHA-256(server_seed) != seed_commit (T2 — fake reveal)".into(),
        ));
    }

    // Decode client seeds into a seat-sorted map.
    let mut by_seat: BTreeMap<u8, [u8; 32]> = BTreeMap::new();
    for (seat_str, hex_seed) in &rec.client_seeds {
        let seat: u8 = seat_str
            .parse()
            .map_err(|_| PfVerifyError(format!("client_seeds key '{seat_str}' is not a seat")))?;
        let seed = decode_32(hex_seed, &format!("client_seeds[{seat_str}]"))?;
        by_seat.insert(seat, seed);
    }

    // (2) derive deck_seed.
    let hid = hand_id_bytes(&rec.hand_id)?;
    let deck_seed = derive_deck_seed(&server_seed, &by_seat, &hid);

    // (3) rebuild the deck + re-derive the deal order (no burns).
    let mut rng = PokerRng::from_seed_bytes(deck_seed);
    let deck = Deck::new(&mut rng);
    let cards = deck.cards();
    let n = rec.num_players;
    let needed = 2 * n + rec.community.len();
    if cards.len() < needed {
        return Err(PfVerifyError(format!(
            "deck has {} cards but the hand needs {} (2*{n} hole + {} board)",
            cards.len(),
            needed,
            rec.community.len()
        )));
    }

    // Build the seat → deal-position map. The engine deals by player-vector
    // index, NOT by seat (F2): for a sparse table the deal position differs from
    // the seat. `dealt_seats[deal_pos] = seat`, so we invert it.
    //
    // FAIL-CLOSED (F3): if `dealt_seats` is present it MUST be well-formed
    // (length == n, no dup, every persisted/board seat resolvable). If it is
    // ABSENT, we may only assume deal_pos == seat when the persisted seats are
    // already contiguous-from-0 — otherwise we reject rather than guess.
    let deal_pos_of_seat: BTreeMap<u8, usize> = if rec.dealt_seats.is_empty() {
        // Legacy / contiguous fixture: deal_pos == seat. Validate that the
        // persisted hole-card seats are all < n AND that none of them could be a
        // sparse seat we'd silently mis-map. Any persisted seat >= n is rejected
        // below; a seat < n is treated as its own deal position.
        let mut needs_explicit = false;
        for seat_str in rec.hole_cards.keys() {
            let seat: usize = seat_str
                .parse()
                .map_err(|_| PfVerifyError(format!("hole_cards key '{seat_str}' is not a seat")))?;
            // A persisted seat at index >= n means the table was sparse (a gap
            // pushed a real seat past the deal-vec length). Without dealt_seats we
            // cannot map it — fail closed.
            if seat >= n {
                needs_explicit = true;
            }
        }
        if needs_explicit {
            return Err(PfVerifyError(format!(
                "sparse-seat PF row requires dealt_seats: a persisted hole-card seat \
                 is >= num_players {n} but dealt_seats is absent (F3 fail-closed — \
                 refusing to guess the deal order)"
            )));
        }
        // Safe contiguous assumption: deal_pos == seat for seat in 0..n.
        (0..n as u8).map(|s| (s, s as usize)).collect()
    } else {
        // Explicit dealt order. Validate it (F3): length, range, no duplicates.
        if rec.dealt_seats.len() != n {
            return Err(PfVerifyError(format!(
                "dealt_seats has {} entries but num_players is {n} (F3 fail-closed)",
                rec.dealt_seats.len()
            )));
        }
        let mut map: BTreeMap<u8, usize> = BTreeMap::new();
        for (pos, &seat) in rec.dealt_seats.iter().enumerate() {
            if map.insert(seat, pos).is_some() {
                return Err(PfVerifyError(format!(
                    "dealt_seats contains duplicate seat {seat} (F3 fail-closed)"
                )));
            }
        }
        map
    };

    // Compare provided hole cards by mapping each seat → its deal position.
    let mut hole_seats_checked = 0usize;
    for (seat_str, persisted) in &rec.hole_cards {
        let seat: u8 = seat_str
            .parse()
            .map_err(|_| PfVerifyError(format!("hole_cards key '{seat_str}' is not a seat")))?;
        if persisted.len() != 2 {
            // Folded-but-uncaptured rows can persist an empty array; skip those
            // (nothing to compare — the board check still anchors the deck).
            continue;
        }
        // FAIL-CLOSED (F3): a persisted seat that is not in the dealt order is
        // unverifiable — never silently skip it.
        let pos = *deal_pos_of_seat.get(&seat).ok_or_else(|| {
            PfVerifyError(format!(
                "hole_cards seat {seat} is not in the dealt-seat order \
                 (F3 fail-closed — cannot map to a deal position)"
            ))
        })?;
        let expected = [cards[pos], cards[n + pos]];
        if persisted[0] != expected[0] || persisted[1] != expected[1] {
            return Err(PfVerifyError(format!(
                "deck mismatch (T1): seat {seat} (deal_pos {pos}) hole cards persisted \
                 {:?} but recomputed deck yields [{}, {}]",
                persisted, expected[0], expected[1]
            )));
        }
        hole_seats_checked += 1;
    }

    // Compare the board prefix (flop/turn/river) in deal order.
    let board_start = 2 * n;
    for (i, persisted) in rec.community.iter().enumerate() {
        let expected = cards[board_start + i];
        if *persisted != expected {
            return Err(PfVerifyError(format!(
                "deck mismatch (T1): community[{i}] persisted {persisted} but \
                 recomputed deck yields {expected}"
            )));
        }
    }

    // U12 (dual-AI OSS review): FAIL-CLOSED when ZERO cards were compared. Both
    // `hole_cards` and `community` are `#[serde(default)]`, non-2-card seats are
    // skipped, and an empty board runs zero iterations — so a card-free record
    // would otherwise sail through every check above and be reported (and
    // printed by `pf_verify`) as verified/provably fair while attesting NOTHING
    // about the deal. An honest export always carries at least the owner's two
    // hole cards or a board card; a record with neither is corrupt or
    // fabricated → reject rather than certify an empty comparison.
    if hole_seats_checked == 0 && rec.community.is_empty() {
        return Err(PfVerifyError(
            "nothing verified: record contains no comparable cards (no 2-card hole \
             set, empty board) — refusing to report a card-free record as verified \
             (U12 fail-closed)"
                .into(),
        ));
    }

    Ok(PfVerifyReport {
        deck_seed_hex: hex::encode(deck_seed),
        hole_seats_checked,
        board_cards_checked: rec.community.len(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use engine::{Chips, GameHand, PlayerId};

    /// Helper: deal a real hand with a known deck_seed, then verify it.
    fn build_and_record(
        server_seed: [u8; 32],
        client_seeds: BTreeMap<u8, [u8; 32]>,
        hand_id: uuid::Uuid,
        n: usize,
    ) -> HandRecord {
        let hid = *hand_id.as_bytes();
        let deck_seed = derive_deck_seed(&server_seed, &client_seeds, &hid);

        // Deal a real engine hand from this exact seed (mirrors the server).
        let players: Vec<(PlayerId, Chips, u8)> = (0..n)
            .map(|i| (PlayerId::new(i as u64), Chips(1000), i as u8))
            .collect();
        let mut hand = GameHand::new_with_rng(
            players,
            0,
            Chips(20),
            Chips(10),
            PokerRng::from_seed_bytes(deck_seed),
        );
        hand.start().expect("start ok");

        // Collect hole cards per seat from the engine snapshot.
        let snap = hand.snapshot();
        let mut hole: BTreeMap<String, Vec<Card>> = BTreeMap::new();
        for p in &snap.players {
            if let Some(hc) = p.hole_cards {
                hole.insert(p.seat.to_string(), vec![hc.card1, hc.card2]);
            }
        }

        // Re-derive the board deterministically by rebuilding the deck (the test
        // hand never reaches the river on its own); just read the deck prefix.
        let mut rng2 = PokerRng::from_seed_bytes(deck_seed);
        let deck = Deck::new(&mut rng2);
        let cards = deck.cards();
        let board: Vec<Card> = (0..5).map(|i| cards[2 * n + i]).collect();

        let client_seeds_hex: BTreeMap<String, String> = client_seeds
            .iter()
            .map(|(s, seed)| (s.to_string(), hex::encode(seed)))
            .collect();

        HandRecord {
            hand_id: hand_id.to_string(),
            server_seed: hex::encode(server_seed),
            seed_commit: hex::encode(ds_hash(DS_SEED_COMMIT, &[&server_seed])),
            client_seeds: client_seeds_hex,
            num_players: n,
            // Contiguous-from-0 fixture: deal_pos == seat.
            dealt_seats: (0..n as u8).collect(),
            hole_cards: hole,
            community: board,
        }
    }

    /// Helper: deal a real engine hand on a SPARSE seat set (e.g. seats `[2, 5]`)
    /// and record it with the explicit `dealt_seats` order — this is the F2/F3
    /// regression scenario the verifier must handle.
    fn build_and_record_sparse(
        server_seed: [u8; 32],
        client_seeds_by_seat: BTreeMap<u8, [u8; 32]>,
        hand_id: uuid::Uuid,
        seats: &[u8],
    ) -> HandRecord {
        let hid = *hand_id.as_bytes();
        let deck_seed = derive_deck_seed(&server_seed, &client_seeds_by_seat, &hid);
        let n = seats.len();

        // Players-vec built from OCCUPIED seats only, in ascending seat order —
        // exactly how session.rs builds it. deal_pos == index in this vec.
        let players: Vec<(PlayerId, Chips, u8)> = seats
            .iter()
            .map(|&s| (PlayerId::new(s as u64 + 1), Chips(1000), s))
            .collect();
        let mut hand = GameHand::new_with_rng(
            players,
            0,
            Chips(20),
            Chips(10),
            PokerRng::from_seed_bytes(deck_seed),
        );
        hand.start().expect("start ok");

        // Capture hole cards keyed by REAL seat (persist.rs does this).
        let snap = hand.snapshot();
        let mut hole: BTreeMap<String, Vec<Card>> = BTreeMap::new();
        for p in &snap.players {
            if let Some(hc) = p.hole_cards {
                hole.insert(p.seat.to_string(), vec![hc.card1, hc.card2]);
            }
        }

        let mut rng2 = PokerRng::from_seed_bytes(deck_seed);
        let deck = Deck::new(&mut rng2);
        let cards = deck.cards();
        let board: Vec<Card> = (0..5).map(|i| cards[2 * n + i]).collect();

        let client_seeds_hex: BTreeMap<String, String> = client_seeds_by_seat
            .iter()
            .map(|(s, seed)| (s.to_string(), hex::encode(seed)))
            .collect();

        HandRecord {
            hand_id: hand_id.to_string(),
            server_seed: hex::encode(server_seed),
            seed_commit: hex::encode(ds_hash(DS_SEED_COMMIT, &[&server_seed])),
            client_seeds: client_seeds_hex,
            num_players: n,
            dealt_seats: seats.to_vec(),
            hole_cards: hole,
            community: board,
        }
    }

    #[test]
    fn verifies_a_real_server_only_hand() {
        let rec = build_and_record(
            [0x11; 32],
            BTreeMap::new(),
            uuid::Uuid::from_bytes([0xAB; 16]),
            3,
        );
        let report = verify_hand(&rec).expect("must verify");
        assert!(report.hole_seats_checked >= 3);
        assert_eq!(report.board_cards_checked, 5);
    }

    #[test]
    fn verifies_a_real_hand_with_client_entropy() {
        let mut cs = BTreeMap::new();
        cs.insert(0u8, [0xC0; 32]);
        cs.insert(2u8, [0xC2; 32]);
        let rec = build_and_record([0x22; 32], cs, uuid::Uuid::from_bytes([0xCD; 16]), 4);
        verify_hand(&rec).expect("must verify with client seeds");
    }

    #[test]
    fn rejects_flipped_server_seed_byte() {
        let mut rec = build_and_record(
            [0x11; 32],
            BTreeMap::new(),
            uuid::Uuid::from_bytes([0xAB; 16]),
            3,
        );
        // Flip a byte of server_seed but keep the (now stale) seed_commit → T2.
        let mut bytes = hex::decode(&rec.server_seed).unwrap();
        bytes[0] ^= 0xFF;
        rec.server_seed = hex::encode(bytes);
        let err = verify_hand(&rec).expect_err("must reject");
        assert!(err.0.contains("commit mismatch"), "got: {}", err.0);
    }

    #[test]
    fn rejects_wrong_hand_id() {
        let mut rec = build_and_record(
            [0x11; 32],
            BTreeMap::new(),
            uuid::Uuid::from_bytes([0xAB; 16]),
            3,
        );
        // Different hand_id → different deck_seed → deck no longer matches the
        // persisted cards (commit still passes; deck reproduction fails — T1/T5).
        rec.hand_id = uuid::Uuid::from_bytes([0x01; 16]).to_string();
        let err = verify_hand(&rec).expect_err("must reject");
        assert!(err.0.contains("deck mismatch"), "got: {}", err.0);
    }

    #[test]
    fn rejects_tampered_board_card() {
        let mut rec = build_and_record(
            [0x11; 32],
            BTreeMap::new(),
            uuid::Uuid::from_bytes([0xAB; 16]),
            3,
        );
        // Swap the first board card for something else.
        let bogus =
            if rec.community[0] == engine::Card::new(engine::Rank::Ace, engine::Suit::Spades) {
                engine::Card::new(engine::Rank::King, engine::Suit::Hearts)
            } else {
                engine::Card::new(engine::Rank::Ace, engine::Suit::Spades)
            };
        rec.community[0] = bogus;
        let err = verify_hand(&rec).expect_err("must reject");
        assert!(err.0.contains("deck mismatch"), "got: {}", err.0);
    }

    // -----------------------------------------------------------------------
    // F2/F3 — sparse-seat soundness + dealt_seats input
    // -----------------------------------------------------------------------

    /// F2 RED-first: an HONEST sparse-seat hand (seats [2, 5]) must VERIFY. The
    /// engine deals by deal position (0, 1) but persist keys hole_cards by seat
    /// (2, 5); without the deal_pos→seat mapping the verifier would compare
    /// seat-2's cards against deck[2]/deck[n+2] (wrong) and reject an honest hand.
    #[test]
    fn verifies_honest_sparse_seat_hand() {
        let rec = build_and_record_sparse(
            [0x33; 32],
            BTreeMap::new(),
            uuid::Uuid::from_bytes([0x44; 16]),
            &[2, 5],
        );
        let report = verify_hand(&rec).expect("honest sparse-seat hand must verify");
        assert_eq!(report.hole_seats_checked, 2, "both sparse seats checked");
        assert_eq!(report.board_cards_checked, 5);
    }

    /// F2: sparse hand WITH client entropy keyed by real (sparse) seat.
    #[test]
    fn verifies_honest_sparse_seat_hand_with_client_entropy() {
        let mut cs = BTreeMap::new();
        cs.insert(2u8, [0xC2; 32]);
        cs.insert(5u8, [0xC5; 32]);
        let rec =
            build_and_record_sparse([0x77; 32], cs, uuid::Uuid::from_bytes([0x88; 16]), &[2, 5]);
        let report = verify_hand(&rec).expect("sparse + client entropy must verify");
        assert_eq!(report.hole_seats_checked, 2);
    }

    /// F2: a TAMPERED sparse-seat hand (swap one seat's hole card) must REJECT —
    /// the deal_pos mapping must not let tampering slip through.
    #[test]
    fn rejects_tampered_sparse_seat_hole_card() {
        let mut rec = build_and_record_sparse(
            [0x33; 32],
            BTreeMap::new(),
            uuid::Uuid::from_bytes([0x44; 16]),
            &[2, 5],
        );
        // Tamper seat 5's first hole card.
        let entry = rec.hole_cards.get_mut("5").expect("seat 5 present");
        let bogus = if entry[0] == engine::Card::new(engine::Rank::Ace, engine::Suit::Spades) {
            engine::Card::new(engine::Rank::King, engine::Suit::Hearts)
        } else {
            engine::Card::new(engine::Rank::Ace, engine::Suit::Spades)
        };
        entry[0] = bogus;
        let err = verify_hand(&rec).expect_err("tampered sparse hole card must reject");
        assert!(err.0.contains("deck mismatch"), "got: {}", err.0);
    }

    /// F3 RED-first FAIL-CLOSED: a sparse PF row that OMITS `dealt_seats` (e.g. a
    /// NULL seats_log row) must be REJECTED, not silently mis-mapped. Here seat 5
    /// is past num_players=2, so without the dealt order it is unverifiable.
    #[test]
    fn rejects_sparse_row_missing_dealt_seats() {
        let mut rec = build_and_record_sparse(
            [0x33; 32],
            BTreeMap::new(),
            uuid::Uuid::from_bytes([0x44; 16]),
            &[2, 5],
        );
        // Simulate a NULL-seats_log row: dealt_seats unavailable.
        rec.dealt_seats = Vec::new();
        let err = verify_hand(&rec).expect_err("missing dealt_seats on sparse row must reject");
        assert!(
            err.0.contains("dealt_seats") || err.0.contains("fail-closed"),
            "got: {}",
            err.0
        );
    }

    /// F3: a malformed `dealt_seats` (wrong length) must be rejected fail-closed.
    #[test]
    fn rejects_malformed_dealt_seats_wrong_length() {
        let mut rec = build_and_record_sparse(
            [0x33; 32],
            BTreeMap::new(),
            uuid::Uuid::from_bytes([0x44; 16]),
            &[2, 5],
        );
        rec.dealt_seats = vec![2]; // length 1 != num_players 2
        let err = verify_hand(&rec).expect_err("malformed dealt_seats must reject");
        assert!(err.0.contains("dealt_seats"), "got: {}", err.0);
    }

    /// F3: duplicate seats in `dealt_seats` must be rejected fail-closed.
    #[test]
    fn rejects_malformed_dealt_seats_duplicate() {
        let mut rec = build_and_record_sparse(
            [0x33; 32],
            BTreeMap::new(),
            uuid::Uuid::from_bytes([0x44; 16]),
            &[2, 5],
        );
        rec.dealt_seats = vec![2, 2];
        let err = verify_hand(&rec).expect_err("duplicate dealt_seats must reject");
        assert!(err.0.contains("duplicate"), "got: {}", err.0);
    }

    /// U12 (dual-AI OSS review) RED-first: a record that compares ZERO cards
    /// (no hole cards, empty board) must NOT verify — a card-free record would
    /// otherwise print "provably fair" and exit 0 while attesting nothing.
    #[test]
    fn rejects_card_free_record() {
        let mut rec = build_and_record(
            [0x11; 32],
            BTreeMap::new(),
            uuid::Uuid::from_bytes([0xAB; 16]),
            3,
        );
        // Strip every comparable card: no hole cards, empty community.
        rec.hole_cards = BTreeMap::new();
        rec.community = Vec::new();
        let err = verify_hand(&rec).expect_err("card-free record must not verify");
        assert!(err.0.contains("nothing verified"), "got: {}", err.0);
    }

    /// U12: skipped (non-2-card) hole entries alone must not count as a
    /// comparison — with an empty board they still leave zero cards checked.
    #[test]
    fn rejects_record_with_only_skipped_hole_entries() {
        let mut rec = build_and_record(
            [0x11; 32],
            BTreeMap::new(),
            uuid::Uuid::from_bytes([0xAB; 16]),
            3,
        );
        // Folded-but-uncaptured shape: present keys but empty card arrays.
        for cards in rec.hole_cards.values_mut() {
            cards.clear();
        }
        rec.community = Vec::new();
        let err = verify_hand(&rec).expect_err("zero-comparison record must not verify");
        assert!(err.0.contains("nothing verified"), "got: {}", err.0);
    }

    /// U12 boundary: ONE comparable surface (board only, no hole cards) still
    /// verifies — the guard only rejects records with NOTHING to compare.
    #[test]
    fn board_only_record_still_verifies() {
        let mut rec = build_and_record(
            [0x11; 32],
            BTreeMap::new(),
            uuid::Uuid::from_bytes([0xAB; 16]),
            3,
        );
        rec.hole_cards = BTreeMap::new();
        let report = verify_hand(&rec).expect("board-only record must still verify");
        assert_eq!(report.hole_seats_checked, 0);
        assert_eq!(report.board_cards_checked, 5);
    }

    /// Back-compat: a contiguous-from-0 hand that omits `dealt_seats` still
    /// verifies (deal_pos == seat is the safe assumption only here).
    #[test]
    fn contiguous_hand_without_dealt_seats_still_verifies() {
        let mut rec = build_and_record(
            [0x11; 32],
            BTreeMap::new(),
            uuid::Uuid::from_bytes([0xAB; 16]),
            3,
        );
        rec.dealt_seats = Vec::new(); // legacy fixture, contiguous seats 0,1,2
        verify_hand(&rec).expect("contiguous hand without dealt_seats must still verify");
    }
}
