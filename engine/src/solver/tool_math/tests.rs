//! Poker-math tool unit tests (moved out of `tool_math.rs`: the inline test
//! module was more than half the file). Same module path, same `use super::*`.

use super::*;
use crate::card::{Card, Rank, Suit};
use crate::hand::{BoardCards, HoleCards};

fn c(r: Rank, s: Suit) -> Card {
    Card::new(r, s)
}

// --- hypergeometric outs odds ---

#[test]
fn flush_draw_9_outs_on_flop() {
    // 9-out flush draw on the flop: 47 unseen, ≈ 35.0% by river, 19.1% next.
    // unseen = 52 − 2 hero − 3 board = 47.
    let odds = outs_to_odds(9, OutsStreet::Flop, 47);
    assert_eq!(odds.outs, 9);
    assert_eq!(odds.unseen, 47);
    // by river: 1 − C(38,2)/C(47,2) = 1 − 703/1081 = 0.34968... ≈ 35.0
    assert!(
        (odds.exact_by_river_pct - 35.0).abs() < 0.1,
        "9-out flush draw by river should be ≈35.0, got {}",
        odds.exact_by_river_pct
    );
    // next card: 9/47 = 0.19148... ≈ 19.1
    assert!(
        (odds.exact_next_card_pct - 19.1).abs() < 0.1,
        "9-out next card should be ≈19.1, got {}",
        odds.exact_next_card_pct
    );
    // 2/4 rule: flop ×4 = 36.
    assert_eq!(odds.rule_2_4_pct, 36);
}

#[test]
fn flush_draw_9_outs_next_card_matches_spec_value() {
    // The spec quotes 19.6% for the "next card" reference; that value uses
    // unseen=46 (a turn-card perspective). Confirm 9/46 ≈ 19.6 so the math
    // is consistent regardless of which unseen the caller passes.
    let odds = outs_to_odds(9, OutsStreet::Turn, 46);
    assert!(
        (odds.exact_next_card_pct - 19.6).abs() < 0.1,
        "9/46 next card should be ≈19.6, got {}",
        odds.exact_next_card_pct
    );
}

#[test]
fn oesd_8_outs_on_flop_is_about_31_5() {
    // 8-out OESD on the flop, 47 unseen: 1 − C(39,2)/C(47,2)
    // = 1 − 741/1081 = 0.31452 ≈ 31.5.
    let odds = outs_to_odds(8, OutsStreet::Flop, 47);
    assert!(
        (odds.exact_by_river_pct - 31.5).abs() < 0.1,
        "8-out OESD by river should be ≈31.5, got {}",
        odds.exact_by_river_pct
    );
    assert_eq!(odds.rule_2_4_pct, 32);
}

#[test]
fn gutshot_4_outs_on_flop_is_about_16_5() {
    // 4-out gutshot on the flop, 47 unseen: 1 − C(43,2)/C(47,2)
    // = 1 − 903/1081 = 0.16466 ≈ 16.5.
    let odds = outs_to_odds(4, OutsStreet::Flop, 47);
    assert!(
        (odds.exact_by_river_pct - 16.5).abs() < 0.1,
        "4-out gutshot by river should be ≈16.5, got {}",
        odds.exact_by_river_pct
    );
    assert_eq!(odds.rule_2_4_pct, 16);
}

#[test]
fn turn_by_river_equals_next_card() {
    // On the turn there's one card to come, so "by river" == "next card".
    let odds = outs_to_odds(9, OutsStreet::Turn, 46);
    assert_eq!(odds.exact_by_river_pct, odds.exact_next_card_pct);
}

#[test]
fn outs_clamped_to_unseen_never_exceeds_100() {
    let odds = outs_to_odds(999, OutsStreet::Flop, 47);
    assert_eq!(odds.outs, 47, "outs clamped to unseen");
    assert!(odds.exact_by_river_pct <= 100.0);
    assert_eq!(odds.rule_2_4_pct, 100, "rule value capped at 100");
}

#[test]
fn zero_outs_is_zero_percent() {
    let odds = outs_to_odds(0, OutsStreet::Flop, 47);
    assert_eq!(odds.exact_by_river_pct, 0.0);
    assert_eq!(odds.exact_next_card_pct, 0.0);
    assert_eq!(odds.rule_2_4_pct, 0);
}

// --- draw detection ---

#[test]
fn detects_clear_flush_draw() {
    // Hero AhKh, board Qh-7h-2c → four hearts = flush draw (9 outs).
    let hero = HoleCards::new(c(Rank::Ace, Suit::Hearts), c(Rank::King, Suit::Hearts));
    let board = BoardCards {
        flop: Some([
            c(Rank::Queen, Suit::Hearts),
            c(Rank::Seven, Suit::Hearts),
            c(Rank::Two, Suit::Clubs),
        ]),
        turn: None,
        river: None,
    };
    let d = detect_draws(&hero, &board);
    assert!(
        d.draws.contains(&DrawKind::FlushDraw),
        "must detect the flush draw, got {:?}",
        d.draws
    );
    assert_eq!(d.outs, 9, "flush draw is 9 outs");
}

#[test]
fn detects_clear_oesd() {
    // Hero 8s9d, board 6h-7c-2s → held 6-7-8-9 = open-ended (8 outs: 5s & Ts).
    let hero = HoleCards::new(c(Rank::Eight, Suit::Spades), c(Rank::Nine, Suit::Diamonds));
    let board = BoardCards {
        flop: Some([
            c(Rank::Six, Suit::Hearts),
            c(Rank::Seven, Suit::Clubs),
            c(Rank::Two, Suit::Spades),
        ]),
        turn: None,
        river: None,
    };
    let d = detect_draws(&hero, &board);
    assert!(
        d.draws.contains(&DrawKind::OpenEndedStraightDraw),
        "must detect the OESD, got {:?}",
        d.draws
    );
    assert_eq!(d.outs, 8, "OESD is 8 outs");
}

#[test]
fn detects_gutshot() {
    // Hero 9s5d, board 6h-7c-2s → 5-6-7- -9 (missing 8) = inside gutshot (4 outs).
    let hero = HoleCards::new(c(Rank::Nine, Suit::Spades), c(Rank::Five, Suit::Diamonds));
    let board = BoardCards {
        flop: Some([
            c(Rank::Six, Suit::Hearts),
            c(Rank::Seven, Suit::Clubs),
            c(Rank::Two, Suit::Spades),
        ]),
        turn: None,
        river: None,
    };
    let d = detect_draws(&hero, &board);
    assert!(
        d.draws.contains(&DrawKind::Gutshot),
        "must detect the gutshot, got {:?}",
        d.draws
    );
    assert!(
        !d.draws.contains(&DrawKind::OpenEndedStraightDraw),
        "an inside gutshot must NOT be reported as OESD"
    );
    assert_eq!(d.outs, 4, "gutshot is 4 outs");
}

#[test]
fn no_draw_board_detects_nothing() {
    // Hero As2d, board 7h-9c-Kd → no flush draw, no four-to-a-straight.
    let hero = HoleCards::new(c(Rank::Ace, Suit::Spades), c(Rank::Two, Suit::Diamonds));
    let board = BoardCards {
        flop: Some([
            c(Rank::Seven, Suit::Hearts),
            c(Rank::Nine, Suit::Clubs),
            c(Rank::King, Suit::Diamonds),
        ]),
        turn: None,
        river: None,
    };
    let d = detect_draws(&hero, &board);
    assert!(d.draws.is_empty(), "no draws expected, got {:?}", d.draws);
    assert_eq!(d.outs, 0);
}

#[test]
fn made_straight_is_not_a_draw() {
    // Hero 8s9d, board 6h-7c-Ts → already a made straight (6-7-8-9-T).
    let hero = HoleCards::new(c(Rank::Eight, Suit::Spades), c(Rank::Nine, Suit::Diamonds));
    let board = BoardCards {
        flop: Some([
            c(Rank::Six, Suit::Hearts),
            c(Rank::Seven, Suit::Clubs),
            c(Rank::Ten, Suit::Spades),
        ]),
        turn: None,
        river: None,
    };
    let d = detect_draws(&hero, &board);
    assert!(
        !d.draws.contains(&DrawKind::OpenEndedStraightDraw)
            && !d.draws.contains(&DrawKind::Gutshot),
        "a made straight must not be reported as a draw, got {:?}",
        d.draws
    );
}

#[test]
fn made_flush_is_not_a_flush_draw() {
    // Hero AhKh, board Qh-7h-2h → five hearts = made flush, not a draw.
    let hero = HoleCards::new(c(Rank::Ace, Suit::Hearts), c(Rank::King, Suit::Hearts));
    let board = BoardCards {
        flop: Some([
            c(Rank::Queen, Suit::Hearts),
            c(Rank::Seven, Suit::Hearts),
            c(Rank::Two, Suit::Hearts),
        ]),
        turn: None,
        river: None,
    };
    let d = detect_draws(&hero, &board);
    assert!(
        !d.draws.contains(&DrawKind::FlushDraw),
        "a made flush must not be reported as a flush draw"
    );
}

#[test]
fn combo_draw_takes_larger_outs_not_sum() {
    // Hero Th9h, board 8h-7c-Qh → flush draw (9) + OESD (6-... wait, held
    // 7-8-9-T = OESD). Conservative outs = max(9, 8) = 9, NOT 17.
    let hero = HoleCards::new(c(Rank::Ten, Suit::Hearts), c(Rank::Nine, Suit::Hearts));
    let board = BoardCards {
        flop: Some([
            c(Rank::Eight, Suit::Hearts),
            c(Rank::Seven, Suit::Clubs),
            c(Rank::Queen, Suit::Hearts),
        ]),
        turn: None,
        river: None,
    };
    let d = detect_draws(&hero, &board);
    assert!(d.draws.contains(&DrawKind::FlushDraw));
    assert!(d.draws.contains(&DrawKind::OpenEndedStraightDraw));
    assert_eq!(
        d.outs, 9,
        "combo draw must not naively sum outs (would overcount shared cards)"
    );
}

#[test]
fn wheel_2345_is_oesd_not_gutshot() {
    // Hero 2s3d, board 4h-5c-9s → held 2-3-4-5 completes with an Ace (wheel
    // A-2-3-4-5) OR a Six (2-3-4-5-6) = open-ended, 8 outs. (Regression: the
    // low end was wrongly treated as deck-blocked → mis-classified gutshot.)
    let hero = HoleCards::new(c(Rank::Two, Suit::Spades), c(Rank::Three, Suit::Diamonds));
    let board = BoardCards {
        flop: Some([
            c(Rank::Four, Suit::Hearts),
            c(Rank::Five, Suit::Clubs),
            c(Rank::Nine, Suit::Spades),
        ]),
        turn: None,
        river: None,
    };
    let d = detect_draws(&hero, &board);
    assert!(
        d.draws.contains(&DrawKind::OpenEndedStraightDraw),
        "2-3-4-5 is open-ended (A or 6), got {:?}",
        d.draws
    );
    assert_eq!(
        d.outs, 8,
        "wheel-low OESD is 8 outs (four Aces + four Sixes)"
    );
}

#[test]
fn pot_odds_overflow_inputs_rejected() {
    // Individually-finite but astronomically large inputs whose sum overflows
    // to +inf must be rejected, not produce a bogus required-equity / ratio.
    assert!(pot_odds(f64::MAX, f64::MAX, None).is_err());
}

#[test]
fn edge_run_broadway_is_gutshot_not_oesd() {
    // Hero AsKd, board Qc-Jh-2s → held A-K-Q-J, missing T. The run is at the
    // top of the deck (can't extend past the Ace), so the only completing
    // card is the T → gutshot (4 outs), NOT an 8-out OESD.
    let hero = HoleCards::new(c(Rank::Ace, Suit::Spades), c(Rank::King, Suit::Diamonds));
    let board = BoardCards {
        flop: Some([
            c(Rank::Queen, Suit::Clubs),
            c(Rank::Jack, Suit::Hearts),
            c(Rank::Two, Suit::Spades),
        ]),
        turn: None,
        river: None,
    };
    let d = detect_draws(&hero, &board);
    assert!(
        d.draws.contains(&DrawKind::Gutshot),
        "Broadway draw is a one-way (gutshot), got {:?}",
        d.draws
    );
    assert!(
        !d.draws.contains(&DrawKind::OpenEndedStraightDraw),
        "an edge-bounded run must not be an OESD"
    );
}

#[test]
fn wheel_draw_is_gutshot() {
    // Hero As2d, board 3h-4c-9s → A-2-3-4 (wheel draw, needs the 5) = gutshot.
    let hero = HoleCards::new(c(Rank::Ace, Suit::Spades), c(Rank::Two, Suit::Diamonds));
    let board = BoardCards {
        flop: Some([
            c(Rank::Three, Suit::Hearts),
            c(Rank::Four, Suit::Clubs),
            c(Rank::Nine, Suit::Spades),
        ]),
        turn: None,
        river: None,
    };
    let d = detect_draws(&hero, &board);
    assert!(
        d.draws.contains(&DrawKind::Gutshot),
        "wheel draw should be a gutshot, got {:?}",
        d.draws
    );
}

#[test]
fn detection_skipped_off_flop_and_turn() {
    // Preflop (0 board) → no detection.
    let hero = HoleCards::new(c(Rank::Ace, Suit::Hearts), c(Rank::King, Suit::Hearts));
    let preflop = detect_draws(&hero, &BoardCards::empty());
    assert!(preflop.draws.is_empty());
    assert_eq!(preflop.outs, 0);
}

// --- hero participation (POKER-PRO-F1 / BUG-201) ---
//
// A four-flush or four-straight lying ENTIRELY on the board is not hero's
// draw: every completing card improves every opponent identically. The
// detector must require at least one hero card in the pattern.

#[test]
fn board_only_four_flush_is_not_hero_flush_draw() {
    // Hero AhKd, board Ks-Qs-7s-2s (turn): four spades, none in hero's hand.
    let hero = HoleCards::new(c(Rank::Ace, Suit::Hearts), c(Rank::King, Suit::Diamonds));
    let board = BoardCards {
        flop: Some([
            c(Rank::King, Suit::Spades),
            c(Rank::Queen, Suit::Spades),
            c(Rank::Seven, Suit::Spades),
        ]),
        turn: Some(c(Rank::Two, Suit::Spades)),
        river: None,
    };
    let d = detect_draws(&hero, &board);
    assert!(
        !d.draws.contains(&DrawKind::FlushDraw),
        "board-only four-flush must not be hero's flush draw, got {:?}",
        d.draws
    );
    assert!(d.draws.is_empty(), "no draws expected, got {:?}", d.draws);
    assert_eq!(d.outs, 0, "hero has zero flush outs");
}

#[test]
fn board_only_four_straight_is_not_hero_straight_draw() {
    // Hero AcKd, board 5h-6s-7d-8c (turn): 5-6-7-8 run entirely on the board.
    let hero = HoleCards::new(c(Rank::Ace, Suit::Clubs), c(Rank::King, Suit::Diamonds));
    let board = BoardCards {
        flop: Some([
            c(Rank::Five, Suit::Hearts),
            c(Rank::Six, Suit::Spades),
            c(Rank::Seven, Suit::Diamonds),
        ]),
        turn: Some(c(Rank::Eight, Suit::Clubs)),
        river: None,
    };
    let d = detect_draws(&hero, &board);
    assert!(
        !d.draws.contains(&DrawKind::OpenEndedStraightDraw)
            && !d.draws.contains(&DrawKind::Gutshot),
        "board-only four-run must not be hero's straight draw, got {:?}",
        d.draws
    );
    assert!(d.draws.is_empty(), "no draws expected, got {:?}", d.draws);
    assert_eq!(d.outs, 0);
}

#[test]
fn board_only_gutshot_is_not_hero_draw() {
    // Hero KdQc, board 5h-6s-8d-9c (turn): 5-6-_-8-9 gutshot entirely on the board.
    let hero = HoleCards::new(c(Rank::King, Suit::Diamonds), c(Rank::Queen, Suit::Clubs));
    let board = BoardCards {
        flop: Some([
            c(Rank::Five, Suit::Hearts),
            c(Rank::Six, Suit::Spades),
            c(Rank::Eight, Suit::Diamonds),
        ]),
        turn: Some(c(Rank::Nine, Suit::Clubs)),
        river: None,
    };
    let d = detect_draws(&hero, &board);
    assert!(d.draws.is_empty(), "no draws expected, got {:?}", d.draws);
    assert_eq!(d.outs, 0);
}

#[test]
fn board_only_wheel_draw_is_not_hero_draw() {
    // Hero 9hTh, board Ad-2c-3s-4h (turn): A-2-3-4 wheel draw entirely on the board.
    let hero = HoleCards::new(c(Rank::Nine, Suit::Hearts), c(Rank::Ten, Suit::Hearts));
    let board = BoardCards {
        flop: Some([
            c(Rank::Ace, Suit::Diamonds),
            c(Rank::Two, Suit::Clubs),
            c(Rank::Three, Suit::Spades),
        ]),
        turn: Some(c(Rank::Four, Suit::Hearts)),
        river: None,
    };
    let d = detect_draws(&hero, &board);
    assert!(d.draws.is_empty(), "no draws expected, got {:?}", d.draws);
    assert_eq!(d.outs, 0);
}

#[test]
fn hero_rank_duplicating_board_run_is_not_participation() {
    // Hero 8h2c, board 5d-6s-7c-8s (turn): hero's 8 duplicates the board's 8, so
    // hero adds no rank to the 5-6-7-8 run — still not hero's draw.
    let hero = HoleCards::new(c(Rank::Eight, Suit::Hearts), c(Rank::Two, Suit::Clubs));
    let board = BoardCards {
        flop: Some([
            c(Rank::Five, Suit::Diamonds),
            c(Rank::Six, Suit::Spades),
            c(Rank::Seven, Suit::Clubs),
        ]),
        turn: Some(c(Rank::Eight, Suit::Spades)),
        river: None,
    };
    let d = detect_draws(&hero, &board);
    assert!(d.draws.is_empty(), "no draws expected, got {:?}", d.draws);
    assert_eq!(d.outs, 0);
}

#[test]
fn board_four_run_with_hero_overcard_is_gutshot_not_oesd() {
    // Hero TdKc, board 5h-6s-7d-8c (turn): the board-only 5-6-7-8 OESD is not
    // hero's, but hero's T makes 6-7-_-9-T a genuine gutshot (a 9 gives hero
    // the T-high straight over the board's 9-high). Expect Gutshot / 4, not
    // OESD / 8.
    let hero = HoleCards::new(c(Rank::Ten, Suit::Diamonds), c(Rank::King, Suit::Clubs));
    let board = BoardCards {
        flop: Some([
            c(Rank::Five, Suit::Hearts),
            c(Rank::Six, Suit::Spades),
            c(Rank::Seven, Suit::Diamonds),
        ]),
        turn: Some(c(Rank::Eight, Suit::Clubs)),
        river: None,
    };
    let d = detect_draws(&hero, &board);
    assert_eq!(d.draws, vec![DrawKind::Gutshot], "got {:?}", d.draws);
    assert_eq!(d.outs, 4);
}

#[test]
fn hero_one_suited_card_plus_three_board_is_flush_draw() {
    // Hero AsKd, board Qs-7s-2s (flop): hero contributes one spade to a four-flush.
    let hero = HoleCards::new(c(Rank::Ace, Suit::Spades), c(Rank::King, Suit::Diamonds));
    let board = BoardCards {
        flop: Some([
            c(Rank::Queen, Suit::Spades),
            c(Rank::Seven, Suit::Spades),
            c(Rank::Two, Suit::Spades),
        ]),
        turn: None,
        river: None,
    };
    let d = detect_draws(&hero, &board);
    assert!(
        d.draws.contains(&DrawKind::FlushDraw),
        "hero holding one of the four spades is a flush draw, got {:?}",
        d.draws
    );
    assert_eq!(d.outs, 9);
}

#[test]
fn hero_card_completing_four_run_is_oesd() {
    // Hero 9dKc, board 6h-7c-8s (flop): hero's 9 completes 6-7-8-9 = OESD (8 outs).
    let hero = HoleCards::new(c(Rank::Nine, Suit::Diamonds), c(Rank::King, Suit::Clubs));
    let board = BoardCards {
        flop: Some([
            c(Rank::Six, Suit::Hearts),
            c(Rank::Seven, Suit::Clubs),
            c(Rank::Eight, Suit::Spades),
        ]),
        turn: None,
        river: None,
    };
    let d = detect_draws(&hero, &board);
    assert!(
        d.draws.contains(&DrawKind::OpenEndedStraightDraw),
        "hero completing the four-run is an OESD, got {:?}",
        d.draws
    );
    assert_eq!(d.outs, 8);
}

#[test]
fn hero_card_completing_gutshot_window_is_gutshot() {
    // Hero Td2c, board 6h-7c-9s (flop): hero's T makes 6-7-_-9-T = gutshot (4 outs).
    let hero = HoleCards::new(c(Rank::Ten, Suit::Diamonds), c(Rank::Two, Suit::Clubs));
    let board = BoardCards {
        flop: Some([
            c(Rank::Six, Suit::Hearts),
            c(Rank::Seven, Suit::Clubs),
            c(Rank::Nine, Suit::Spades),
        ]),
        turn: None,
        river: None,
    };
    let d = detect_draws(&hero, &board);
    assert_eq!(d.draws, vec![DrawKind::Gutshot], "got {:?}", d.draws);
    assert_eq!(d.outs, 4);
}

#[test]
fn hero_card_in_wheel_window_is_gutshot() {
    // Hero 5c9d, board Ah-2s-3d-Kc (turn): hero's 5 joins A-2-3-_-5 = wheel gutshot.
    let hero = HoleCards::new(c(Rank::Five, Suit::Clubs), c(Rank::Nine, Suit::Diamonds));
    let board = BoardCards {
        flop: Some([
            c(Rank::Ace, Suit::Hearts),
            c(Rank::Two, Suit::Spades),
            c(Rank::Three, Suit::Diamonds),
        ]),
        turn: Some(c(Rank::King, Suit::Clubs)),
        river: None,
    };
    let d = detect_draws(&hero, &board);
    assert!(
        d.draws.contains(&DrawKind::Gutshot),
        "hero's 5 in the wheel window is a gutshot, got {:?}",
        d.draws
    );
    assert_eq!(d.outs, 4);
}

// --- pot odds ---

#[test]
fn half_pot_bet_needs_25pct() {
    // Pot 100, bet 50 (½ pot) → required equity 50/150 = 33.3%? No: a ½-pot
    // BET means hero calls 50 into a pot that already shows 100, so
    // to_call/(pot+to_call) = 50/150 = 33.3. The spec's "½-pot bet → 25%"
    // refers to pot=150 (after the bettor's 50 is already in) / to_call 50:
    // 50/200 = 25%. Test that arithmetic explicitly.
    let po = pot_odds(150.0, 50.0, None).unwrap();
    assert!(
        (po.equity_needed_pct - 25.0).abs() < 0.05,
        "½-pot bet (call 50 into 150) needs 25%, got {}",
        po.equity_needed_pct
    );
}

#[test]
fn pot_size_bet_needs_33pct() {
    // Pot-size bet: pot 100, call 50 → 50/150 = 33.3%.
    let po = pot_odds(100.0, 50.0, None).unwrap();
    assert!(
        (po.equity_needed_pct - 33.3).abs() < 0.05,
        "call 50 into 100 needs 33.3%, got {}",
        po.equity_needed_pct
    );
    // Canonical pot odds are pot:to_call = 100:50 = 2:1, and 1/(2+1) = 33.3%
    // matches equity_needed — U18 (dual-AI OSS review) fixed the prior 3:1
    // that implied 25% and disagreed with equity_needed.
    assert_eq!(po.ratio, "2:1", "pot 100 : call 50 simplifies to 2:1");
    assert_eq!(po.pot_odds_pct, po.equity_needed_pct);
    assert!(po.verdict.is_none(), "no equity ⇒ no verdict");
    assert!(po.margin_pct.is_none());
}

#[test]
fn ratio_and_equity_needed_are_consistent() {
    // U18: ratio X:1 must satisfy equity_needed = 1/(X+1) for clean spots.
    for (pot, call, ratio, needed) in [
        (150.0, 50.0, "3:1", 25.0),
        (100.0, 50.0, "2:1", 33.3),
        (100.0, 25.0, "4:1", 20.0),
    ] {
        let po = pot_odds(pot, call, None).unwrap();
        assert_eq!(po.ratio, ratio, "pot {pot} call {call}");
        assert!(
            (po.equity_needed_pct - needed).abs() < 0.05,
            "pot {pot} call {call}: needed {needed}, got {}",
            po.equity_needed_pct
        );
    }
}

#[test]
fn verdict_call_when_equity_clears_with_margin() {
    let po = pot_odds(100.0, 50.0, Some(45.0)).unwrap();
    // needed 33.3, equity 45 → margin +11.7 → call.
    assert_eq!(po.verdict, Some(PotOddsVerdict::Call));
    assert!((po.margin_pct.unwrap() - 11.7).abs() < 0.05);
}

#[test]
fn verdict_fold_when_equity_short_with_margin() {
    let po = pot_odds(100.0, 50.0, Some(20.0)).unwrap();
    // needed 33.3, equity 20 → margin -13.3 → fold.
    assert_eq!(po.verdict, Some(PotOddsVerdict::Fold));
    assert!(po.margin_pct.unwrap() < 0.0);
}

#[test]
fn verdict_marginal_inside_dead_zone() {
    let po = pot_odds(100.0, 50.0, Some(34.0)).unwrap();
    // needed 33.3, equity 34 → margin +0.7 (< 2) → marginal.
    assert_eq!(po.verdict, Some(PotOddsVerdict::Marginal));
}

#[test]
fn ratio_simplifies_via_gcd() {
    // pot 100, call 25 → reward 100 : risk 25 = 4:1 (needed 1/5 = 20%).
    let po = pot_odds(100.0, 25.0, None).unwrap();
    assert_eq!(po.ratio, "4:1");
}

#[test]
fn ratio_non_integer_falls_back_to_x_to_1() {
    // pot 100, call 33 → reward 100 : 33 = 3.0:1 (rounded).
    let po = pot_odds(100.0, 33.0, None).unwrap();
    assert_eq!(po.ratio, "3:1");
}

#[test]
fn rejects_non_positive_inputs() {
    assert_eq!(pot_odds(0.0, 50.0, None), Err(PotOddsError::InvalidPot));
    assert_eq!(pot_odds(100.0, 0.0, None), Err(PotOddsError::InvalidToCall));
    assert_eq!(
        pot_odds(f64::NAN, 50.0, None),
        Err(PotOddsError::InvalidPot)
    );
    assert_eq!(
        pot_odds(100.0, f64::INFINITY, None),
        Err(PotOddsError::InvalidToCall)
    );
}

#[test]
fn rejects_out_of_range_equity() {
    assert_eq!(
        pot_odds(100.0, 50.0, Some(150.0)),
        Err(PotOddsError::InvalidEquity)
    );
    assert_eq!(
        pot_odds(100.0, 50.0, Some(-1.0)),
        Err(PotOddsError::InvalidEquity)
    );
    assert_eq!(
        pot_odds(100.0, 50.0, Some(f64::NAN)),
        Err(PotOddsError::InvalidEquity)
    );
}

// --- scare-card (exact hypergeometric) ---

/// Tolerance for comparing an EXACT closed-form probability against a
/// hand-computed reference fraction — generous slack, the math is exact.
const SC_EPS: f64 = 1e-9;

#[test]
fn scare_card_flop_one_ace_by_river_no_aces_seen() {
    // Flop spot: hero Qh-Jc, board 8d-7s-2h (NO aces anywhere). Known = 5,
    // N = 47, S = 4 aces, k = 2 (flop→river).
    // P(≥1 ace) = 1 − C(43,2)/C(47,2) = 1 − 903/1081 = 178/1081 = 0.1646623…
    let known = vec![
        c(Rank::Queen, Suit::Hearts),
        c(Rank::Jack, Suit::Clubs),
        c(Rank::Eight, Suit::Diamonds),
        c(Rank::Seven, Suit::Spades),
        c(Rank::Two, Suit::Hearts),
    ];
    let odds = scare_card(&known, &[Rank::Ace], 2).unwrap();
    assert_eq!(odds.unseen_total, 47, "N = 52 − 5 known");
    assert_eq!(odds.matching_unseen, 4, "all four aces unseen");
    assert_eq!(odds.cards_to_come, 2);
    let expected = 178.0 / 1081.0;
    assert!(
        (odds.p_at_least_one - expected).abs() < SC_EPS,
        "P(≥1 ace) should be 178/1081 = {expected}, got {}",
        odds.p_at_least_one
    );
    assert!(
        (odds.p_none - (903.0 / 1081.0)).abs() < SC_EPS,
        "P(none) should be 903/1081, got {}",
        odds.p_none
    );
    // p_none and p_at_least_one always sum to exactly 1.
    assert!((odds.p_at_least_one + odds.p_none - 1.0).abs() < SC_EPS);
}

#[test]
fn scare_card_turn_to_river_one_card_to_come() {
    // Turn spot, k = 1: hero Qh-Jc, board 8d-7s-2h-3c. Known = 6, N = 46,
    // S = 4 aces. P(≥1 ace) = 4/46 = 0.0869565…
    let known = vec![
        c(Rank::Queen, Suit::Hearts),
        c(Rank::Jack, Suit::Clubs),
        c(Rank::Eight, Suit::Diamonds),
        c(Rank::Seven, Suit::Spades),
        c(Rank::Two, Suit::Hearts),
        c(Rank::Three, Suit::Clubs),
    ];
    let odds = scare_card(&known, &[Rank::Ace], 1).unwrap();
    assert_eq!(odds.unseen_total, 46);
    assert_eq!(odds.matching_unseen, 4);
    assert_eq!(odds.cards_to_come, 1);
    let expected = 4.0 / 46.0;
    assert!(
        (odds.p_at_least_one - expected).abs() < SC_EPS,
        "single card to come: P(ace) = 4/46 = {expected}, got {}",
        odds.p_at_least_one
    );
}

#[test]
fn scare_card_multi_rank_set_ace_or_king() {
    // Flop, target {A, K}, none seen: hero Qh-Jc, board 8d-7s-2h. N = 47,
    // S = 8, k = 2. P(≥1 of A/K) = 1 − C(39,2)/C(47,2) = 340/1081 = 0.3145236…
    let known = vec![
        c(Rank::Queen, Suit::Hearts),
        c(Rank::Jack, Suit::Clubs),
        c(Rank::Eight, Suit::Diamonds),
        c(Rank::Seven, Suit::Spades),
        c(Rank::Two, Suit::Hearts),
    ];
    let odds = scare_card(&known, &[Rank::Ace, Rank::King], 2).unwrap();
    assert_eq!(odds.matching_unseen, 8, "four aces + four kings unseen");
    let expected = 340.0 / 1081.0;
    assert!(
        (odds.p_at_least_one - expected).abs() < SC_EPS,
        "P(≥1 of A/K) should be 340/1081 = {expected}, got {}",
        odds.p_at_least_one
    );
}

#[test]
fn scare_card_dedupes_duplicate_target_ranks() {
    // Passing [Ace, Ace] is identical to [Ace] — distinct rank count is 1.
    let known = vec![
        c(Rank::Queen, Suit::Hearts),
        c(Rank::Jack, Suit::Clubs),
        c(Rank::Eight, Suit::Diamonds),
        c(Rank::Seven, Suit::Spades),
        c(Rank::Two, Suit::Hearts),
    ];
    let once = scare_card(&known, &[Rank::Ace], 2).unwrap();
    let twice = scare_card(&known, &[Rank::Ace, Rank::Ace], 2).unwrap();
    assert_eq!(once, twice, "duplicate target ranks must collapse");
}

#[test]
fn scare_card_blocker_in_hand_reduces_s() {
    // Hero HOLDS the Ace of spades (a blocker). Target {A}, flop board with
    // no aces: hero As-Jc, board 8d-7s-2h. Known = 5, N = 47, but S = 3
    // (only three aces remain unseen). P(≥1 ace) = 1 − C(44,2)/C(47,2)
    // = 135/1081 = 0.1248843… — strictly LOWER than the no-blocker 178/1081.
    let known = vec![
        c(Rank::Ace, Suit::Spades),
        c(Rank::Jack, Suit::Clubs),
        c(Rank::Eight, Suit::Diamonds),
        c(Rank::Seven, Suit::Spades),
        c(Rank::Two, Suit::Hearts),
    ];
    let odds = scare_card(&known, &[Rank::Ace], 2).unwrap();
    assert_eq!(odds.matching_unseen, 3, "hero's ace is a blocker → S = 3");
    let expected = 135.0 / 1081.0;
    assert!(
        (odds.p_at_least_one - expected).abs() < SC_EPS,
        "blocker case P(≥1 ace) = 135/1081 = {expected}, got {}",
        odds.p_at_least_one
    );
    // Sanity: strictly below the no-blocker value (178/1081).
    assert!(
        odds.p_at_least_one < 178.0 / 1081.0,
        "a blocker must lower the probability"
    );
}

#[test]
fn scare_card_river_complete_k0_is_deterministic() {
    // k = 0: river already dealt, nothing to come. P(≥1) = 0, P(none) = 1
    // regardless of the target set.
    let known = vec![
        c(Rank::Queen, Suit::Hearts),
        c(Rank::Jack, Suit::Clubs),
        c(Rank::Eight, Suit::Diamonds),
        c(Rank::Seven, Suit::Spades),
        c(Rank::Two, Suit::Hearts),
        c(Rank::Three, Suit::Clubs),
        c(Rank::Four, Suit::Diamonds),
    ];
    let odds = scare_card(&known, &[Rank::Ace], 0).unwrap();
    assert_eq!(odds.cards_to_come, 0);
    assert_eq!(odds.p_at_least_one, 0.0, "no cards to come → never lands");
    assert_eq!(odds.p_none, 1.0);
}

#[test]
fn scare_card_all_targets_already_dealt_is_zero() {
    // S = 0: every ace is already visible (4 in the known set), so no ace can
    // land. P(≥1 ace) = 0. (A contrived but valid known set.)
    let known = vec![
        c(Rank::Ace, Suit::Spades),
        c(Rank::Ace, Suit::Hearts),
        c(Rank::Ace, Suit::Diamonds),
        c(Rank::Ace, Suit::Clubs),
        c(Rank::Two, Suit::Hearts),
    ];
    let odds = scare_card(&known, &[Rank::Ace], 2).unwrap();
    assert_eq!(odds.matching_unseen, 0, "all four aces seen → S = 0");
    assert_eq!(odds.p_at_least_one, 0.0);
    assert_eq!(odds.p_none, 1.0);
}

#[test]
fn scare_card_empty_rank_set_errors() {
    let known = vec![c(Rank::Queen, Suit::Hearts), c(Rank::Jack, Suit::Clubs)];
    assert_eq!(
        scare_card(&known, &[], 2),
        Err(ScareCardError::EmptyRankSet)
    );
}

#[test]
fn scare_card_duplicate_known_card_errors() {
    // Qh appears twice in the known set.
    let known = vec![
        c(Rank::Queen, Suit::Hearts),
        c(Rank::Queen, Suit::Hearts),
        c(Rank::Eight, Suit::Diamonds),
    ];
    assert_eq!(
        scare_card(&known, &[Rank::Ace], 2),
        Err(ScareCardError::DuplicateCard)
    );
}

#[test]
fn scare_card_k_exceeds_unseen_errors() {
    // A full deck of 52 known cards leaves N = 0; any k > 0 is impossible.
    let mut known = Vec::with_capacity(52);
    for &s in Suit::ALL.iter() {
        for &r in Rank::ALL.iter() {
            known.push(c(r, s));
        }
    }
    assert_eq!(known.len(), 52);
    assert_eq!(
        scare_card(&known, &[Rank::Ace], 1),
        Err(ScareCardError::InvalidCardsToCome)
    );
}

#[test]
fn scare_card_n_minus_s_below_k_is_certain() {
    // Construct N − S < k so every remaining draw must include a target rank
    // → P(≥1) = 1. Use a tiny remaining pool: 50 known cards leave N = 2.
    // Target = every NON-Ace, NON-King rank still in the deck. We engineer a
    // known set where the 2 unseen cards are BOTH a target rank, S = 2, N = 2,
    // k = 2 → N − S = 0 < 2 → certain.
    // Simpler: keep all but two cards known; the two unseen are 5d and 6c.
    let mut known: Vec<Card> = Vec::new();
    for &s in Suit::ALL.iter() {
        for &r in Rank::ALL.iter() {
            let card = c(r, s);
            // Leave out exactly two target-rank cards (5d, 6c).
            if (r == Rank::Five && s == Suit::Diamonds) || (r == Rank::Six && s == Suit::Clubs) {
                continue;
            }
            known.push(card);
        }
    }
    assert_eq!(known.len(), 50, "N = 2 unseen");
    let odds = scare_card(&known, &[Rank::Five, Rank::Six], 2).unwrap();
    assert_eq!(odds.unseen_total, 2);
    assert_eq!(odds.matching_unseen, 2, "both unseen cards are targets");
    assert_eq!(
        odds.p_at_least_one, 1.0,
        "N − S < k ⇒ a target rank is certain"
    );
    assert_eq!(odds.p_none, 0.0);
}

#[test]
fn scare_card_from_board_derives_k_from_board() {
    // The wrapper derives k = 5 − board_len. Flop board → k = 2; identical to
    // the explicit flop call above (hero Qh-Jc, board 8d-7s-2h, target {A}).
    let hero = HoleCards::new(c(Rank::Queen, Suit::Hearts), c(Rank::Jack, Suit::Clubs));
    let board = BoardCards {
        flop: Some([
            c(Rank::Eight, Suit::Diamonds),
            c(Rank::Seven, Suit::Spades),
            c(Rank::Two, Suit::Hearts),
        ]),
        turn: None,
        river: None,
    };
    let odds = scare_card_from_board(&hero, &board, &[Rank::Ace]).unwrap();
    assert_eq!(odds.cards_to_come, 2, "flop → 2 cards to come");
    assert_eq!(odds.unseen_total, 47);
    let expected = 178.0 / 1081.0;
    assert!((odds.p_at_least_one - expected).abs() < SC_EPS);

    // Pre-flop board → k = 5.
    let pre = scare_card_from_board(&hero, &BoardCards::empty(), &[Rank::Ace]).unwrap();
    assert_eq!(pre.cards_to_come, 5);
    assert_eq!(pre.unseen_total, 50);
}

#[test]
fn overcards_to_board_top_picks_strictly_higher_ranks() {
    // Board Q-7-2 → overcards are K and A.
    let board = BoardCards {
        flop: Some([
            c(Rank::Queen, Suit::Hearts),
            c(Rank::Seven, Suit::Spades),
            c(Rank::Two, Suit::Diamonds),
        ]),
        turn: None,
        river: None,
    };
    let over = overcards_to_board_top(&board);
    assert_eq!(over, vec![Rank::King, Rank::Ace], "Q-high → K, A are over");

    // Ace-high board → no overcard possible.
    let ace_board = BoardCards {
        flop: Some([
            c(Rank::Ace, Suit::Hearts),
            c(Rank::Seven, Suit::Spades),
            c(Rank::Two, Suit::Diamonds),
        ]),
        turn: None,
        river: None,
    };
    assert!(
        overcards_to_board_top(&ace_board).is_empty(),
        "nothing out-ranks an Ace"
    );

    // Empty board → no top rank → empty.
    assert!(overcards_to_board_top(&BoardCards::empty()).is_empty());
}

#[test]
fn scare_card_overcards_to_q72_flop_matches_ak_set() {
    // Combine the helper + scare_card: board Q-7-2, hero 9d-8c (no over to
    // help). Overcards = {K, A}; none seen, so this equals the {A,K} multi-
    // rank case: S = 8, N = 47, k = 2 → 340/1081.
    let hero = HoleCards::new(c(Rank::Nine, Suit::Diamonds), c(Rank::Eight, Suit::Clubs));
    let board = BoardCards {
        flop: Some([
            c(Rank::Queen, Suit::Hearts),
            c(Rank::Seven, Suit::Spades),
            c(Rank::Two, Suit::Diamonds),
        ]),
        turn: None,
        river: None,
    };
    let over = overcards_to_board_top(&board);
    assert_eq!(over, vec![Rank::King, Rank::Ace]);
    let odds = scare_card_from_board(&hero, &board, &over).unwrap();
    assert_eq!(odds.matching_unseen, 8);
    let expected = 340.0 / 1081.0;
    assert!((odds.p_at_least_one - expected).abs() < SC_EPS);
}

#[test]
fn comb_u128_known_values() {
    // Spot-check the binomial helper against known values.
    assert_eq!(comb_u128(47, 2), 1081);
    assert_eq!(comb_u128(43, 2), 903);
    assert_eq!(comb_u128(50, 5), 2_118_760);
    assert_eq!(comb_u128(5, 0), 1, "C(n,0) = 1");
    assert_eq!(comb_u128(2, 3), 0, "k > n → 0");
    assert_eq!(comb_u128(52, 52), 1, "C(n,n) = 1");
}
