use super::*;
use engine::card::{Rank, Suit};

#[test]
fn count_bet_sizes_handles_commas_and_blanks() {
    assert_eq!(count_bet_sizes("50%"), 1);
    assert_eq!(count_bet_sizes("33%,75%"), 2);
    assert_eq!(count_bet_sizes("33%, 75%, "), 2);
    assert_eq!(count_bet_sizes(""), 0);
}

#[test]
fn parse_board_enforces_count_for_street() {
    // flop needs 3
    assert!(parse_board(SolveStreet::Flop, &["Td".into(), "9d".into()]).is_err());
    assert!(parse_board(SolveStreet::Flop, &["Td".into(), "9d".into(), "6h".into()]).is_ok());
    // turn needs 4
    assert!(parse_board(SolveStreet::Turn, &["Td".into(), "9d".into(), "6h".into()]).is_err());
    // river needs 5
    assert!(parse_board(
        SolveStreet::River,
        &[
            "Td".into(),
            "9d".into(),
            "6h".into(),
            "Qc".into(),
            "2s".into()
        ]
    )
    .is_ok());
}

#[test]
fn parse_board_rejects_duplicate_and_bad_cards() {
    assert!(matches!(
        parse_board(SolveStreet::Flop, &["Td".into(), "Td".into(), "6h".into()]),
        Err((code, _)) if code == "duplicate_card"
    ));
    assert!(matches!(
        parse_board(SolveStreet::Flop, &["XX".into(), "9d".into(), "6h".into()]),
        Err((code, _)) if code == "invalid_card"
    ));
}

fn sample_req() -> SolveRequest {
    SolveRequest {
        street: SolveStreet::Flop,
        flop: [
            Card::new(Rank::Ten, Suit::Diamonds),
            Card::new(Rank::Nine, Suit::Diamonds),
            Card::new(Rank::Six, Suit::Hearts),
        ],
        turn: None,
        river: None,
        oop_range: "AA,KK".into(),
        ip_range: "JJ,TT".into(),
        starting_pot: 60,
        effective_stack: 100,
        bet_sizes: "50%".into(),
        raise_sizes: "2.5x".into(),
        hero: None,
        solving_player: Player::Oop,
        limits: SolveLimits::default(),
    }
}

#[test]
fn response_key_is_stable_and_case_insensitive() {
    let a = sample_req();
    let mut b = sample_req();
    b.oop_range = "aa,kk".into(); // case + (no) whitespace equivalent
    b.bet_sizes = "50%".into();
    assert_eq!(
        response_key(&a),
        response_key(&b),
        "equivalent spots → same key"
    );
}

// F3: equivalent ranges that differ only by token ORDER / whitespace must
// canonicalize to the SAME spot/response key so the GB solve is reused
// instead of re-run.
#[test]
fn response_key_is_order_and_whitespace_insensitive_on_ranges() {
    let a = sample_req();
    let mut b = sample_req();
    b.oop_range = " KK , AA ".into(); // reordered + whitespace
    b.ip_range = "TT,JJ".into(); // reordered
    assert_eq!(
        spot_key(&a),
        spot_key(&b),
        "reordered/whitespace range variants → same hero-independent spot key"
    );
    assert_eq!(
        response_key(&a),
        response_key(&b),
        "reordered/whitespace range variants → same response key"
    );
}

// U07 (dual-AI OSS review): WEIGHTED range tokens are order-sensitive
// (upstream: first occurrence wins), so reordering must NOT collapse them to
// one cache key. Unweighted tokens stay order-insensitive.
#[test]
fn canonical_range_preserves_weighted_token_order() {
    // Unweighted: order-insensitive (still sorted → equal).
    assert_eq!(canonical_range("AA,KK"), canonical_range("KK,AA"));
    // Weighted/overlapping: order is semantic → must differ.
    assert_ne!(
        canonical_range("AA:0.2,AA"),
        canonical_range("AA,AA:0.2"),
        "weighted-range token order changes the effective range — must not collapse"
    );
}

#[test]
fn spot_key_distinguishes_reordered_weighted_ranges() {
    let mut a = sample_req();
    let mut b = sample_req();
    a.oop_range = "AA:0.2,AA".into(); // effective AA@0.2
    b.oop_range = "AA,AA:0.2".into(); // effective AA@1.0
    assert_ne!(
        spot_key(&a),
        spot_key(&b),
        "semantically different weighted ranges must not share a cache key"
    );
    assert_ne!(
        spot_repr(&a),
        spot_repr(&b),
        "stored spot repr must also differ"
    );
}

// F3: the spot key is HERO-INDEPENDENT (the equilibrium does not depend on the
// hero hand), while the response key DOES fold in the hero (the response body
// carries that hand's strategy).
#[test]
fn spot_key_ignores_hero_but_response_key_does_not() {
    let a = sample_req();
    let mut withhero = sample_req();
    withhero.hero = Some(HoleCards::new(
        Card::new(Rank::Ace, Suit::Spades),
        Card::new(Rank::Ace, Suit::Hearts),
    ));
    assert_eq!(
        spot_key(&a),
        spot_key(&withhero),
        "the equilibrium spot key must be hero-independent"
    );
    assert_ne!(
        response_key(&a),
        response_key(&withhero),
        "the response key must distinguish a hero-bearing response"
    );
}

#[test]
fn response_key_changes_with_board() {
    let a = sample_req();
    let mut b = sample_req();
    b.flop[2] = Card::new(Rank::Seven, Suit::Hearts);
    assert_ne!(response_key(&a), response_key(&b));
}

#[test]
fn response_key_changes_with_solving_player_and_hero() {
    let a = sample_req();
    let mut b = sample_req();
    b.solving_player = Player::Ip;
    assert_ne!(response_key(&a), response_key(&b));

    let mut c = sample_req();
    c.hero = Some(HoleCards::new(
        Card::new(Rank::Ace, Suit::Spades),
        Card::new(Rank::Ace, Suit::Hearts),
    ));
    assert_ne!(response_key(&a), response_key(&c));
}

#[test]
fn canonical_range_normalizes_order_case_and_whitespace() {
    assert_eq!(canonical_range(" KK , AA "), canonical_range("aa,kk"));
    assert_eq!(canonical_range("AKs,AA"), "aa,aks");
    // empty tokens are dropped.
    assert_eq!(canonical_range("AA,,KK,"), "aa,kk");
    assert_eq!(canonical_range(""), "");
}

#[test]
fn count_range_tokens_counts_segments_including_empties() {
    // The DoS vector is split-segment count, so empties COUNT (unlike
    // count_bet_sizes which ignores them).
    assert_eq!(count_range_tokens("AA,KK,QQ"), 3);
    assert_eq!(count_range_tokens(",,,"), 4);
    assert_eq!(count_range_tokens(""), 1);
}

// F1: a single IP can hold at most MAX_INFLIGHT_PER_IP concurrent solves; a
// second acquire from the SAME ip is refused while the first guard is held,
// and a DIFFERENT ip is unaffected.
#[test]
fn per_ip_inflight_caps_one_concurrent_solve_per_ip() {
    let ip_a = IpAddr::V4(std::net::Ipv4Addr::new(10, 0, 0, 1));
    let ip_b = IpAddr::V4(std::net::Ipv4Addr::new(10, 0, 0, 2));
    let g1 = acquire_ip_inflight(ip_a).expect("first solve for ip_a is granted");
    assert!(
        acquire_ip_inflight(ip_a).is_none(),
        "a second concurrent solve from the same IP must be refused"
    );
    // A different IP always has a free slot.
    let g_b = acquire_ip_inflight(ip_b).expect("a different IP is unaffected");
    drop(g1);
    // Once the first guard drops, the IP can solve again.
    let g2 = acquire_ip_inflight(ip_a).expect("after the first solve ends the IP can solve");
    drop(g2);
    drop(g_b);
    // The registry reclaims now-idle entries on drop (no unbounded growth).
    assert!(
        !ip_inflight_registry().contains_key(&ip_a),
        "an idle IP's registry entry is reclaimed on drop"
    );
}

#[test]
fn client_ip_uses_peer_or_loopback_fallback() {
    let peer = ConnectInfo(SocketAddr::from(([203, 0, 113, 7], 4444)));
    assert_eq!(
        client_ip(Some(&peer)),
        IpAddr::V4(std::net::Ipv4Addr::new(203, 0, 113, 7))
    );
    assert_eq!(
        client_ip(None),
        IpAddr::V4(std::net::Ipv4Addr::LOCALHOST),
        "a missing ConnectInfo falls back to the loopback sentinel"
    );
}

/// PERF guard (2026-09-16 MED): `CachedSolveView` serializes the cached body
/// BY REFERENCE instead of deep-cloning it, so it restates `SolveResponse`'s
/// field list. If the two ever drift — a field added, renamed or reordered
/// on one side only — the cache-hit response would silently differ from the
/// fresh-solve response. Assert they are wire-identical.
#[test]
fn cached_view_is_wire_identical_to_solve_response() {
    let spot = synthetic_spot(3, 2);
    let hero = HandStrategyDto {
        hand: "AhKh".to_string(),
        frequencies: vec![0.4, 0.6],
        equity: 0.55,
        ev: 3.25,
    };
    for cached in [true, false] {
        let mut expected = (*spot.response).clone();
        expected.cached = cached;
        expected.hero = Some(hero.clone());
        let view = CachedSolveView::new(&spot.response, Some(hero.clone()), cached);
        assert_eq!(
            serde_json::to_value(&view).expect("view"),
            serde_json::to_value(&expected).expect("response"),
            "CachedSolveView drifted from SolveResponse (cached={cached})"
        );
    }
    // …and with no hero row.
    let mut expected = (*spot.response).clone();
    expected.cached = true;
    let view = CachedSolveView::new(&spot.response, None, true);
    assert_eq!(
        serde_json::to_value(&view).expect("view"),
        serde_json::to_value(&expected).expect("response")
    );
}

#[test]
fn dto_deserializes_with_optional_fields_defaulted() {
    let dto: SolveRequestDto = serde_json::from_value(serde_json::json!({
        "street": "flop",
        "board": ["Td", "9d", "6h"],
        "oop_range": "AA,KK",
        "ip_range": "JJ,TT",
        "starting_pot": 60,
        "effective_stack": 100,
        "solving_player": "oop"
    }))
    .unwrap();
    assert!(dto.bet_sizes.is_none());
    assert!(dto.raise_sizes.is_none());
    assert!(dto.hero.is_none());
}

// F7: build a synthetic CachedSpot whose per-hand table is `hands` rows wide,
// each carrying a `freqs`-long frequency vector — large enough that a handful
// of entries would blow a coarse entry-count cap but still be "a few MB".
fn synthetic_spot(hands: usize, freqs: usize) -> CachedSpot {
    let mut table = Vec::with_capacity(hands);
    for i in 0..hands {
        table.push(HandStrategy {
            hand: format!("h{i:08}"),
            frequencies: vec![0.25_f32; freqs],
            equity: 0.5,
            ev: 1.0,
        });
    }
    CachedSpot {
        response: Arc::new(SolveResponse {
            method: "cfr_equilibrium".into(),
            solving_player: "oop".into(),
            exploitability: 0.1,
            exploitability_pct_of_pot: 0.1,
            actions: Vec::new(),
            range_equity: 0.5,
            range_ev: 1.0,
            hero: None,
            bet_sizes: "50%".into(),
            raise_sizes: "2.5x".into(),
            max_iterations: PUBLIC_MAX_ITERATIONS,
            cached: false,
            source_url: None,
        }),
        hands: table,
        bytes: 0,
        key_repr: String::new(),
    }
}

// F7: the solved-spot cache is bounded by BYTES, not entry count. Insert many
// large DISTINCT spots whose combined estimated footprint FAR exceeds the byte
// budget and assert the tracked total never exceeds `MAX_CACHE_BYTES`, that the
// map is kept under the budget (entries evicted), and that the accountant stays
// in lock-step with the map's real estimated size.
#[test]
fn cache_is_bounded_by_bytes_not_entry_count() {
    // Each entry: ~4000 hands × 8 freqs ≈ a few hundred KB — comparable to a
    // wide early-street per-hand table. Inserting far more than fit in the
    // budget proves eviction keeps total bytes capped.
    let per_entry = cached_spot_bytes(&synthetic_spot(4_000, 8));
    assert!(per_entry > 0, "a non-empty spot must estimate > 0 bytes");
    // Enough entries that their SUM is several × the budget.
    let n = (MAX_CACHE_BYTES / per_entry) * 4 + 16;

    // Use a key range disjoint from any other test so this is self-contained
    // against the process-global cache (other unit tests don't touch it).
    const BASE: u64 = 0xF7F7_0000_0000_0000;
    for i in 0..(n as u64) {
        cache_insert(BASE + i, synthetic_spot(4_000, 8));
        let tracked = cache_bytes().load(std::sync::atomic::Ordering::Relaxed);
        assert!(
            tracked <= MAX_CACHE_BYTES,
            "after insert #{i} the tracked cache bytes ({tracked}) must stay \
             within the byte budget ({MAX_CACHE_BYTES})"
        );
    }

    // The cache must have evicted: it cannot hold all `n` huge entries.
    let entries = solve_cache()
        .iter()
        .filter(|kv| (*kv.key() & 0xFFFF_0000_0000_0000) == 0xF7F7_0000_0000_0000)
        .count();
    assert!(
        entries < n,
        "the byte budget must have evicted entries (held {entries} of {n})"
    );

    // The accountant must equal the SUM of the (recorded) bytes of the entries
    // actually in the map — proof it stayed in lock-step through eviction.
    let real: usize = solve_cache().iter().map(|kv| kv.value().bytes).sum();
    let tracked = cache_bytes().load(std::sync::atomic::Ordering::Relaxed);
    assert_eq!(
        tracked, real,
        "the byte accountant must match the map's recorded entry bytes"
    );

    // A single entry larger than the WHOLE budget is simply not cached (it
    // would force an empty cache), and it does not corrupt the accountant.
    let before = cache_bytes().load(std::sync::atomic::Ordering::Relaxed);
    let oversized = synthetic_spot(MAX_CACHE_BYTES / 8 + 1_000, 16);
    assert!(
        cached_spot_bytes(&oversized) > MAX_CACHE_BYTES,
        "the oversized spot must exceed the whole budget for this assertion"
    );
    cache_insert(0xF7F7_FFFF_FFFF_FFFF, oversized);
    assert!(
        !solve_cache().contains_key(&0xF7F7_FFFF_FFFF_FFFF),
        "an entry larger than the whole budget must not be cached"
    );
    assert_eq!(
        cache_bytes().load(std::sync::atomic::Ordering::Relaxed),
        before,
        "rejecting an oversized entry must not change the byte accountant"
    );

    // Clean up our keys so we leave the process-global cache as we found it.
    let keys: Vec<u64> = solve_cache()
        .iter()
        .map(|kv| *kv.key())
        .filter(|k| (*k & 0xFFFF_0000_0000_0000) == 0xF7F7_0000_0000_0000)
        .collect();
    for k in keys {
        if let Some((_, removed)) = solve_cache().remove(&k) {
            cache_bytes().fetch_sub(removed.bytes, std::sync::atomic::Ordering::Relaxed);
        }
    }
    if let Ok(mut q) = cache_order().lock() {
        q.retain(|k| (*k & 0xFFFF_0000_0000_0000) != 0xF7F7_0000_0000_0000);
    }
}
