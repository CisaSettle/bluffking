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
/// satisfy the verifier).
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
    let needed = n.saturating_mul(2).saturating_add(rec.community.len());
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
