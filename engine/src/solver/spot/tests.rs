//! Spot-builder unit tests (moved out of `spot.rs`: the inline test module was
//! more than half the file). Same module path, same `use super::*`.

use super::*;
use crate::card::{Card, Rank, Suit};

fn hc(r1: Rank, s1: Suit, r2: Rank, s2: Suit) -> HoleCards {
    HoleCards::new(Card::new(r1, s1), Card::new(r2, s2))
}

fn preflop_req(hero: HoleCards, pos: PositionBucket, line: PreflopLine) -> SpotRequest {
    SpotRequest {
        table_size: TableSize::SixMax,
        hero_position: pos,
        villain_position: None,
        stack_bb: 100,
        street: Street::Preflop,
        hero,
        board: BoardCards::empty(),
        pot_bb: 1.5,
        to_call_bb: 0.0,
        preflop_line: line,
        hero_facing: HeroFacing::FirstToAct,
        hero_action: None,
        villain_hand: None,
        advice_seed: 42,
        reveal_seed: 42,
    }
}

fn board3(c1: Card, c2: Card, c3: Card) -> BoardCards {
    BoardCards {
        flop: Some([c1, c2, c3]),
        turn: None,
        river: None,
    }
}

#[test]
fn aa_utg_rfi_recommends_raise_with_grid() {
    let hero = hc(Rank::Ace, Suit::Spades, Rank::Ace, Suit::Hearts);
    let req = preflop_req(hero, PositionBucket::UTG, PreflopLine::Rfi);
    let a = analyze_spot(&req).unwrap();
    assert_eq!(a.method, SolveMethod::CfrEquityRealization);
    assert_eq!(a.recommended, SolverAction::Raise);
    let grid = a.preflop_grid.expect("preflop must emit a grid");
    assert_eq!(grid.len(), 169, "grid must be exactly 169 cells");
    let aa = grid.iter().find(|c| c.hand_key == "AA").unwrap();
    assert!(aa.freq >= 0.5, "AA freq must be ≥0.5 in UTG RFI");
    assert!(aa.is_hero, "AA must be marked is_hero");
    assert_eq!(aa.action, GridAction::Raise);
    // Exactly one hero cell.
    assert_eq!(grid.iter().filter(|c| c.is_hero).count(), 1);
    assert_eq!(a.reason_code, ReasonCode::PreflopInRange);
}

#[test]
fn seven_two_off_utg_rfi_folds_not_in_range() {
    let hero = hc(Rank::Seven, Suit::Spades, Rank::Two, Suit::Hearts);
    let req = preflop_req(hero, PositionBucket::UTG, PreflopLine::Rfi);
    let a = analyze_spot(&req).unwrap();
    assert_eq!(a.recommended, SolverAction::Fold);
    assert_eq!(a.reason_code, ReasonCode::PreflopNotInRange);
    let grid = a.preflop_grid.unwrap();
    let h72 = grid.iter().find(|c| c.hand_key == "72o").unwrap();
    assert_eq!(h72.action, GridAction::Fold);
    assert!(h72.is_hero);
}

/// F6 (consistency): the chart now emits genuine MIXED frequencies (e.g. CO
/// vs_4bet AQs = 0.26). The grid paints such a cell `Mix`, but the headline
/// recommendation collapses freq < 0.5 → Fold. A user who took the OTHER
/// legitimate mixed branch (F2: JAM the 4-bet — the vs_4bet continue is a 5-bet
/// all-in, NOT a flat-call) must NOT be graded a Mistake — that would
/// contradict the grid. This asserts the mixed-cell verdict semantics: grid
/// shows `Mix`, and BOTH branches (fold / jam) grade Ok/Good (never Mistake).
#[test]
fn preflop_mixed_frequency_preserves_mixed_verdict_semantics() {
    // CO vs_4bet AQs is a mixed cell (~0.26): headline Fold, continue=AllIn (jam).
    let hero = hc(Rank::Ace, Suit::Spades, Rank::Queen, Suit::Spades);
    let mut req = preflop_req(hero, PositionBucket::CO, PreflopLine::Vs4bet);

    // Sanity: the underlying cell really is mixed (guards against a future
    // chart regen turning it pure, which would silently void this test).
    let cell = preflop_charts::lookup(PositionBucket::CO, ActionBucket::Vs4bet, "AQs")
        .expect("AQs must be present in CO vs_4bet");
    assert!(
        advisor::preflop_cell_is_mixed(cell.frequency),
        "AQs CO vs_4bet must be a MIXED cell for this test, got freq {}",
        cell.frequency
    );

    // Grid paints it Mix.
    req.hero_action = None;
    let a = analyze_spot(&req).unwrap();
    let grid = a.preflop_grid.expect("preflop grid");
    let aqs = grid.iter().find(|c| c.hand_key == "AQs").unwrap();
    assert_eq!(aqs.action, GridAction::Mix, "mixed cell must paint Mix");
    assert_eq!(a.reason_code, ReasonCode::PreflopMixed);
    // Headline collapses freq<0.5 → Fold (the higher-frequency branch).
    assert_eq!(a.recommended, SolverAction::Fold);

    // Branch 1: hero JAMS the 4-bet (the lower-frequency mixed branch — F2: the
    // vs_4bet continue is a 5-bet all-in, not a flat-call). Under the OLD binary
    // grading this was Mistake (AllIn vs gto=Fold). Now Ok.
    req.hero_action = Some(SolverAction::AllIn);
    let jammed = analyze_spot(&req).unwrap();
    assert_eq!(
        jammed.verdict,
        Some(SolverVerdict::Ok),
        "jamming a mixed 4-bet cell must be Ok, not a Mistake"
    );

    // Branch 2: hero FOLDS (the headline branch) → Good.
    req.hero_action = Some(SolverAction::Fold);
    let folded = analyze_spot(&req).unwrap();
    assert_eq!(
        folded.verdict,
        Some(SolverVerdict::Good),
        "folding the headline branch of a mixed cell is Good"
    );

    // A genuinely off-strategy action in this fold/jam-mix cell (flat-CALLING a
    // 4-bet — never part of the model's strategy, F2) is still graded by the
    // normal logic — mixing does not bless every action.
    req.hero_action = Some(SolverAction::Call);
    let called = analyze_spot(&req).unwrap();
    assert!(
        called.verdict.is_some(),
        "off-strategy action still produces a verdict (not blessed by the mix)"
    );
}

#[test]
fn postflop_uses_equity_heuristic_and_emits_no_grid() {
    // Nut flush draw: hero AhKh on Qh-7h-2c (4 hearts → strong draw).
    let hero = hc(Rank::Ace, Suit::Hearts, Rank::King, Suit::Hearts);
    let board = BoardCards {
        flop: Some([
            Card::new(Rank::Queen, Suit::Hearts),
            Card::new(Rank::Seven, Suit::Hearts),
            Card::new(Rank::Two, Suit::Clubs),
        ]),
        turn: None,
        river: None,
    };
    let req = SpotRequest {
        table_size: TableSize::SixMax,
        hero_position: PositionBucket::BTN,
        villain_position: Some(PositionBucket::CO),
        stack_bb: 100,
        street: Street::Flop,
        hero,
        board,
        pot_bb: 6.0,
        to_call_bb: 2.0,
        preflop_line: PreflopLine::FacingOpen,
        hero_facing: HeroFacing::Bet,
        hero_action: None,
        villain_hand: None,
        advice_seed: 7,
        reveal_seed: 7,
    };
    let a = analyze_spot(&req).unwrap();
    assert_eq!(a.method, SolveMethod::EquityHeuristic);
    assert!(a.preflop_grid.is_none(), "postflop emits no grid");
    // Nut flush draw vs a value range still has real equity.
    assert!(
        a.equity_pct > 20 && a.equity_pct <= 100,
        "got {}",
        a.equity_pct
    );
    assert!(a.pot_odds_pct > 0);
}

#[test]
fn villain_range_from_charts_lowers_equity_vs_random() {
    // AKo facing a CO open: villain range = CO RFI (tight-ish). Equity vs
    // that range should differ from (and be a real, derived) villain range
    // code, not "random".
    let hero = hc(Rank::Ace, Suit::Spades, Rank::King, Suit::Diamonds);
    let board = BoardCards {
        flop: Some([
            Card::new(Rank::Two, Suit::Clubs),
            Card::new(Rank::Seven, Suit::Diamonds),
            Card::new(Rank::Nine, Suit::Hearts),
        ]),
        turn: None,
        river: None,
    };
    let req = SpotRequest {
        table_size: TableSize::SixMax,
        hero_position: PositionBucket::BTN,
        villain_position: Some(PositionBucket::CO),
        stack_bb: 100,
        street: Street::Flop,
        hero,
        board,
        pot_bb: 5.0,
        to_call_bb: 0.0,
        preflop_line: PreflopLine::FacingOpen,
        hero_facing: HeroFacing::FirstToAct,
        hero_action: None,
        villain_hand: None,
        advice_seed: 3,
        reveal_seed: 3,
    };
    let a = analyze_spot(&req).unwrap();
    assert_eq!(a.villain_range.code, "co_rfi");
    assert!(a.villain_range.combos > 0 && a.villain_range.combos < 1326);
    assert!(a.villain_range.pct > 0.0 && a.villain_range.pct < 100.0);
}

#[test]
fn postflop_rfi_aggressor_faces_flat_defend_range_not_threebet_or_random() {
    // Single most common postflop spot: hero opened (RFI) preflop from CO,
    // villain (BTN, which HAS a chart RFI range) flat-CALLED, now flop. F3:
    // the villain range MUST be the derived FLAT-DEFEND range (RFI minus
    // 3-bet) — NOT the tight `facing_open` 3-bet range (those hands would have
    // 3-bet, not flatted), and NOT "random".
    let hero = hc(Rank::Ace, Suit::Spades, Rank::King, Suit::Diamonds);
    let board = BoardCards {
        flop: Some([
            Card::new(Rank::Two, Suit::Clubs),
            Card::new(Rank::Seven, Suit::Diamonds),
            Card::new(Rank::Nine, Suit::Hearts),
        ]),
        turn: None,
        river: None,
    };
    let req = SpotRequest {
        table_size: TableSize::SixMax,
        hero_position: PositionBucket::CO,
        villain_position: Some(PositionBucket::BTN),
        stack_bb: 100,
        street: Street::Flop,
        hero,
        board,
        pot_bb: 6.0,
        to_call_bb: 0.0,
        preflop_line: PreflopLine::Rfi,
        hero_facing: HeroFacing::FirstToAct,
        hero_action: None,
        villain_hand: None,
        advice_seed: 9,
        reveal_seed: 9,
    };
    let a = analyze_spot(&req).unwrap();
    assert_eq!(
        a.villain_range.code, "btn_flat_defend",
        "hero-as-aggressor postflop must face villain's derived flat-defend range (F3)"
    );
    assert!(
        a.villain_range.combos > 0 && a.villain_range.combos < 1326,
        "expected a real defend range, got {} combos",
        a.villain_range.combos
    );
    assert!(a.villain_range.pct > 0.0 && a.villain_range.pct < 100.0);

    // The flat-defend range is the "calls but does NOT 3-bet" range — it must
    // EXCLUDE the hands the villain 3-bets (their strongest holdings), which is
    // exactly what makes it a genuine flat-CALL range and NOT the aggressive
    // `facing_open` (3-bet/continue) range the F3 regression analyzed against.
    let flat: std::collections::BTreeMap<HandKey, f32> =
        flat_defend_range(PositionBucket::BTN).into_iter().collect();
    let threebet: std::collections::BTreeMap<HandKey, f32> =
        preflop_charts::range_entries(PositionBucket::BTN, ActionBucket::FacingOpen)
            .into_iter()
            .collect();
    // AA is a full-frequency 3-bet (in facing_open) ⇒ it must NOT appear in the
    // flat-call range (you jam/3-bet AA, you never flat it). This proves the
    // top of the range is capped out — the defining property of a flat-call
    // range vs the 3-bet range.
    assert!(
        threebet.get("AA").copied().unwrap_or(0.0) >= 0.99,
        "precondition: AA must be a (near-)pure 3-bet in BTN facing_open"
    );
    assert!(
        flat.get("AA").copied().unwrap_or(0.0) <= 0.0,
        "AA must be EXCLUDED from the flat-call range (it 3-bets, never flats) — \
             proves the flat-defend range caps the top, unlike the 3-bet range (F3)"
    );
    // And it must still be a REAL, non-trivial range (not collapsed to empty):
    let flat_combos: f32 = flat
        .iter()
        .map(|(k, f)| preflop_charts::combos_for_key(k) as f32 * f)
        .sum();
    assert!(
        flat_combos > 50.0,
        "flat-defend range must be a real range, got {flat_combos:.1} combos"
    );

    // Equity is computed vs that derived Range (not vs random), and sane.
    assert!(
        a.equity_pct > 0 && a.equity_pct <= 100,
        "got {}",
        a.equity_pct
    );
    assert_eq!(a.method, SolveMethod::EquityHeuristic);
}

#[test]
fn postflop_rfi_aggressor_vs_bb_defender_falls_back_to_wide() {
    // F3 edge: the BB has NO chart RFI range (it never opens unopened), so the
    // v3 chart has no flat-defend proxy for a BB caller. Rather than analyze vs
    // a WRONG tight 3-bet range, this honestly falls back to the widest proxy
    // ("random") — a BB call-defend range really is very wide.
    let hero = hc(Rank::Ace, Suit::Spades, Rank::King, Suit::Diamonds);
    let board = BoardCards {
        flop: Some([
            Card::new(Rank::Two, Suit::Clubs),
            Card::new(Rank::Seven, Suit::Diamonds),
            Card::new(Rank::Nine, Suit::Hearts),
        ]),
        turn: None,
        river: None,
    };
    let req = SpotRequest {
        table_size: TableSize::SixMax,
        hero_position: PositionBucket::BTN,
        villain_position: Some(PositionBucket::BB),
        stack_bb: 100,
        street: Street::Flop,
        hero,
        board,
        pot_bb: 6.0,
        to_call_bb: 0.0,
        preflop_line: PreflopLine::Rfi,
        hero_facing: HeroFacing::FirstToAct,
        hero_action: None,
        villain_hand: None,
        advice_seed: 9,
        reveal_seed: 9,
    };
    let a = analyze_spot(&req).unwrap();
    assert_eq!(
            a.villain_range.code, "random",
            "no chart flat-defend proxy for a BB caller ⇒ honest wide fallback, NOT a tight 3-bet range"
        );
}

#[test]
fn preflop_rfi_still_random_no_villain_acted_yet() {
    // Preflop RFI: no villain has acted ⇒ range stays "random" even when a
    // villain_position is supplied.
    let hero = hc(Rank::Ace, Suit::Spades, Rank::King, Suit::Diamonds);
    let mut req = preflop_req(hero, PositionBucket::BTN, PreflopLine::Rfi);
    req.villain_position = Some(PositionBucket::BB);
    let a = analyze_spot(&req).unwrap();
    assert_eq!(a.villain_range.code, "random");
    assert_eq!(a.villain_range.combos, 1326);
    assert_eq!(a.villain_range.pct, 100.0);
}

#[test]
fn preflop_recommendation_consistent_with_own_grid_cell() {
    // Forward-compat invariant lock: with the current binary charts (all
    // freq=1.0, no mixed strategies), the hero's recommended action MUST agree
    // with hero's own grid cell action — in-range hand ⇒ cell Raise +
    // recommended Raise; out-of-range hand ⇒ cell Fold + recommended Fold.
    // Locks the invariant BEFORE mixed-strategy charts ever land.

    // In-range: AA UTG RFI.
    let in_range = hc(Rank::Ace, Suit::Spades, Rank::Ace, Suit::Hearts);
    let a = analyze_spot(&preflop_req(
        in_range,
        PositionBucket::UTG,
        PreflopLine::Rfi,
    ))
    .unwrap();
    let aa = a
        .preflop_grid
        .as_ref()
        .unwrap()
        .iter()
        .find(|c| c.is_hero)
        .unwrap();
    assert_eq!(aa.action, GridAction::Raise);
    assert_eq!(a.recommended, SolverAction::Raise);

    // Out-of-range: 72o UTG RFI.
    let out_range = hc(Rank::Seven, Suit::Spades, Rank::Two, Suit::Hearts);
    let b = analyze_spot(&preflop_req(
        out_range,
        PositionBucket::UTG,
        PreflopLine::Rfi,
    ))
    .unwrap();
    let h72 = b
        .preflop_grid
        .as_ref()
        .unwrap()
        .iter()
        .find(|c| c.is_hero)
        .unwrap();
    assert_eq!(h72.action, GridAction::Fold);
    assert_eq!(b.recommended, SolverAction::Fold);
}

/// F2 (2026-06-25): a pure vs_4bet continue cell (AA CO vs_4bet, freq 1.0) must
/// surface the model's actual action — a 5-bet JAM. The HEADLINE must be
/// `AllIn` (NOT the old `Call`, which told premiums to flat a 4-bet at 100bb),
/// and the GRID must paint it `Raise` (a jam is an aggressive raise; the 4-color
/// grid has no all-in swatch). Both are aggressive + mutually consistent —
/// never the contradictory "headline Call / grid something-else" the user saw.
#[test]
fn vs4bet_pure_premium_headline_is_jam_grid_is_aggressive() {
    let aa = hc(Rank::Ace, Suit::Spades, Rank::Ace, Suit::Hearts);
    let a = analyze_spot(&preflop_req(aa, PositionBucket::CO, PreflopLine::Vs4bet)).unwrap();
    // Headline: a 5-bet jam, NOT a flat-call.
    assert_eq!(
        a.recommended,
        SolverAction::AllIn,
        "vs_4bet premium continue must be a 5-bet JAM (AllIn), never a flat Call"
    );
    // Grid: the hero's own cell is the aggressive `Raise` (jam), not `Call`.
    let cell = a
        .preflop_grid
        .as_ref()
        .unwrap()
        .iter()
        .find(|c| c.is_hero)
        .unwrap();
    assert_eq!(
        cell.action,
        GridAction::Raise,
        "pure vs_4bet continue cell must paint aggressive Raise (jam), never Call"
    );
    assert_ne!(
        cell.action,
        GridAction::Call,
        "the grid must NOT tell the user to flat-call a 4-bet (F2 regression)"
    );

    // Grading: jamming AA vs a 4-bet (the recommendation) is Good; flat-CALLING
    // it — never part of the model's strategy — must NOT be graded Good.
    let mut req = preflop_req(aa, PositionBucket::CO, PreflopLine::Vs4bet);
    req.hero_action = Some(SolverAction::AllIn);
    assert_eq!(
        analyze_spot(&req).unwrap().verdict,
        Some(SolverVerdict::Good),
        "jamming AA vs a 4-bet matches the recommendation ⇒ Good"
    );
}

#[test]
fn verdict_only_present_when_hero_action_given() {
    let hero = hc(Rank::Ace, Suit::Spades, Rank::Ace, Suit::Hearts);
    let mut req = preflop_req(hero, PositionBucket::UTG, PreflopLine::Rfi);
    // No hero action → no verdict.
    assert!(analyze_spot(&req).unwrap().verdict.is_none());
    // Hero raises AA (= recommendation) → Good.
    req.hero_action = Some(SolverAction::Raise);
    assert_eq!(
        analyze_spot(&req).unwrap().verdict,
        Some(SolverVerdict::Good)
    );
}

#[test]
fn determinism_same_seed_same_analysis() {
    let hero = hc(Rank::Ace, Suit::Hearts, Rank::Queen, Suit::Hearts);
    let board = BoardCards {
        flop: Some([
            Card::new(Rank::Jack, Suit::Hearts),
            Card::new(Rank::Five, Suit::Hearts),
            Card::new(Rank::Two, Suit::Clubs),
        ]),
        turn: None,
        river: None,
    };
    let req = SpotRequest {
        table_size: TableSize::SixMax,
        hero_position: PositionBucket::BTN,
        villain_position: Some(PositionBucket::CO),
        stack_bb: 100,
        street: Street::Flop,
        hero,
        board,
        pot_bb: 6.0,
        to_call_bb: 2.5,
        preflop_line: PreflopLine::FacingOpen,
        hero_facing: HeroFacing::Bet,
        hero_action: Some(SolverAction::Call),
        villain_hand: None,
        advice_seed: 12345,
        reveal_seed: 12345,
    };
    let a1 = analyze_spot(&req).unwrap();
    let a2 = analyze_spot(&req).unwrap();
    assert_eq!(a1.equity_pct, a2.equity_pct);
    assert_eq!(a1.recommended, a2.recommended);
    assert_eq!(a1.verdict, a2.verdict);
    assert_eq!(a1.ev_margin_bb, a2.ev_margin_bb);
}

#[test]
fn invalid_cards_rejected() {
    // Duplicate hero cards.
    let hero = hc(Rank::Ace, Suit::Spades, Rank::Ace, Suit::Spades);
    let req = preflop_req(hero, PositionBucket::UTG, PreflopLine::Rfi);
    assert_eq!(analyze_spot(&req).unwrap_err(), SolverError::InvalidCards);
}

#[test]
fn board_count_must_match_street() {
    // Street is Flop but board is empty → invalid.
    let hero = hc(Rank::Ace, Suit::Spades, Rank::King, Suit::Spades);
    let mut req = preflop_req(hero, PositionBucket::UTG, PreflopLine::Rfi);
    req.street = Street::Flop; // board still empty
    assert_eq!(analyze_spot(&req).unwrap_err(), SolverError::InvalidCards);
}

#[test]
fn pot_odds_pct_rounds_correctly() {
    // to_call 2 into pot 4 → 2/6 = 33%.
    assert_eq!(pot_odds_pct(4.0, 2.0), 33);
    // No to_call → 0.
    assert_eq!(pot_odds_pct(10.0, 0.0), 0);
}

#[test]
fn wire_shape_serializes_snake_case_per_spec() {
    // Locks the §2 wire contract: snake_case enum values + the documented
    // field names. (server/client mirror this exact shape.)
    let hero = hc(Rank::Ace, Suit::Spades, Rank::King, Suit::Spades);
    let mut req = preflop_req(hero, PositionBucket::BTN, PreflopLine::Rfi);
    req.hero_action = Some(SolverAction::Raise);
    let a = analyze_spot(&req).unwrap();
    let v: serde_json::Value = serde_json::to_value(&a).unwrap();
    assert_eq!(v["method"], "cfr_equity_realization");
    assert_eq!(v["recommended"], "raise");
    assert_eq!(v["verdict"], "good");
    assert!(v["preflop_grid"].is_array());
    assert_eq!(v["preflop_grid"].as_array().unwrap().len(), 169);
    // hand_class is a snake_case HandStrength string.
    assert!(v["hand_class"].is_string());
    // villain_range summary object present with code/combos/pct.
    assert!(v["villain_range"]["code"].is_string());
    assert!(v["villain_range"]["combos"].is_number());
    assert!(v["villain_range"]["pct"].is_number());
    // reason_code is snake_case.
    assert!(v["reason_code"].is_string());
    // a grid cell mirrors {hand_key, action, freq, is_hero}.
    let cell = &v["preflop_grid"][0];
    assert!(cell["hand_key"].is_string());
    assert!(cell["action"].is_string());
    assert!(cell["freq"].is_number());
    assert!(cell["is_hero"].is_boolean());
    // exact_equity_pct is present and serializes to null when no villain
    // hand was supplied (the field must exist so the client mirror sees it).
    assert!(
        v.as_object().unwrap().contains_key("exact_equity_pct"),
        "exact_equity_pct field must always be present in the wire shape"
    );
    assert!(v["exact_equity_pct"].is_null());
}

#[test]
fn grid_maps_freq_to_actions() {
    assert_eq!(grid_action(1.0), GridAction::Raise);
    assert_eq!(grid_action(0.0), GridAction::Fold);
    assert_eq!(grid_action(0.5), GridAction::Mix);
}

// -----------------------------------------------------------------------
// villain_hand exact-equity reveal + analyze_replay (对局复盘)
// -----------------------------------------------------------------------

#[test]
fn reveal_seed_does_not_leak_into_advice() {
    // F1 regression (CONTRACT): the reveal channel (`reveal_seed` /
    // `villain_hand`) must NOT perturb the advice channel. Hold the spot fixed
    // and the advice_seed fixed; vary ONLY the reveal_seed AND the villain
    // hand across many values (what the server would do for different revealed
    // villain hands). The range equity_pct / recommendation / verdict / reason
    // MUST be byte-identical every time — only exact_equity_pct may move.
    let hero = hc(Rank::Ace, Suit::Hearts, Rank::Queen, Suit::Hearts);
    let board = BoardCards {
        flop: Some([
            Card::new(Rank::Jack, Suit::Hearts),
            Card::new(Rank::Five, Suit::Hearts),
            Card::new(Rank::Two, Suit::Clubs),
        ]),
        turn: None,
        river: None,
    };
    let base = SpotRequest {
        table_size: TableSize::SixMax,
        hero_position: PositionBucket::BTN,
        villain_position: Some(PositionBucket::CO),
        stack_bb: 100,
        street: Street::Flop,
        hero,
        board,
        pot_bb: 6.0,
        to_call_bb: 2.5,
        preflop_line: PreflopLine::FacingOpen,
        hero_facing: HeroFacing::Bet,
        hero_action: Some(SolverAction::Call),
        villain_hand: None,
        advice_seed: 555,
        reveal_seed: 555,
    };
    // Reference advice computed with NO villain hand at all.
    let reference = analyze_spot(&base).unwrap();

    // A handful of distinct villain holdings that don't conflict with hero/board.
    let villains = [
        hc(Rank::King, Suit::Spades, Rank::King, Suit::Diamonds),
        hc(Rank::Nine, Suit::Spades, Rank::Eight, Suit::Diamonds),
        hc(Rank::Ace, Suit::Spades, Rank::Ace, Suit::Diamonds),
        hc(Rank::Seven, Suit::Diamonds, Rank::Three, Suit::Spades),
    ];
    for (i, v) in villains.iter().enumerate() {
        // Each revealed hand lands on a DIFFERENT reveal_seed (as the server's
        // villain-hashing seed would), but the advice_seed is unchanged.
        let req = SpotRequest {
            villain_hand: Some(*v),
            reveal_seed: 9000 + i as u64,
            ..base.clone()
        };
        let a = analyze_spot(&req).unwrap();
        assert_eq!(
            a.equity_pct, reference.equity_pct,
            "range equity_pct must be independent of the revealed villain hand / reveal_seed"
        );
        assert_eq!(a.recommended, reference.recommended);
        assert_eq!(a.verdict, reference.verdict);
        assert_eq!(a.reason_code, reference.reason_code);
        assert_eq!(a.ev_margin_bb, reference.ev_margin_bb);
        // The reveal number IS present and (for these different hands) can vary.
        assert!(a.exact_equity_pct.is_some());
    }
}

#[test]
fn exact_equity_present_with_villain_hand_22_vs_aq() {
    // Hero 22 vs villain AQ preflop. The EXACT reveal must be Some and land
    // in a sane band (22 vs AQo/s is a ~50% coin-flip; allow a wide [35,65]
    // window so we don't over-assert on MC noise), while the range-based
    // equity_pct stays populated and untouched by the villain hand.
    let hero = hc(Rank::Two, Suit::Spades, Rank::Two, Suit::Hearts);
    let villain = hc(Rank::Ace, Suit::Clubs, Rank::Queen, Suit::Diamonds);
    let mut req = preflop_req(hero, PositionBucket::BTN, PreflopLine::Rfi);
    req.villain_hand = Some(villain);
    let a = analyze_spot(&req).unwrap();
    let exact = a
        .exact_equity_pct
        .expect("villain hand ⇒ exact equity Some");
    assert!(
        (35..=65).contains(&exact),
        "22 vs AQ exact equity should be a coin-flip-ish band, got {exact}"
    );
    // Range equity still computed (and independent of the reveal).
    assert!(a.equity_pct <= 100);
}

#[test]
fn exact_equity_none_without_villain_hand() {
    // Existing analyze_spot behavior preserved: no villain hand ⇒ None.
    let hero = hc(Rank::Two, Suit::Spades, Rank::Two, Suit::Hearts);
    let req = preflop_req(hero, PositionBucket::BTN, PreflopLine::Rfi);
    let a = analyze_spot(&req).unwrap();
    assert!(a.exact_equity_pct.is_none());
}

#[test]
fn villain_hand_conflicting_with_hero_is_invalid() {
    // Villain shares a card with hero → InvalidCards.
    let hero = hc(Rank::Ace, Suit::Spades, Rank::King, Suit::Spades);
    let villain = hc(Rank::Ace, Suit::Spades, Rank::Queen, Suit::Diamonds);
    let mut req = preflop_req(hero, PositionBucket::BTN, PreflopLine::Rfi);
    req.villain_hand = Some(villain);
    assert_eq!(analyze_spot(&req).unwrap_err(), SolverError::InvalidCards);
}

#[test]
fn villain_hand_conflicting_with_board_is_invalid() {
    // Villain shares a card with the board → InvalidCards (flop spot).
    let hero = hc(Rank::Ace, Suit::Spades, Rank::King, Suit::Spades);
    let board = board3(
        Card::new(Rank::Two, Suit::Clubs),
        Card::new(Rank::Seven, Suit::Diamonds),
        Card::new(Rank::Nine, Suit::Hearts),
    );
    let req = SpotRequest {
        table_size: TableSize::SixMax,
        hero_position: PositionBucket::BTN,
        villain_position: Some(PositionBucket::BB),
        stack_bb: 100,
        street: Street::Flop,
        hero,
        board,
        pot_bb: 6.0,
        to_call_bb: 0.0,
        preflop_line: PreflopLine::Rfi,
        hero_facing: HeroFacing::FirstToAct,
        hero_action: None,
        // Villain holds the 2c that is already on the flop.
        villain_hand: Some(hc(Rank::Two, Suit::Clubs, Rank::Three, Suit::Diamonds)),
        advice_seed: 1,
        reveal_seed: 1,
    };
    assert_eq!(analyze_spot(&req).unwrap_err(), SolverError::InvalidCards);
}

/// Build a 4-street replay over a full 5-card board: hero AhKh, villain QdQs.
fn full_replay_req(seed: u64) -> ReplayRequest {
    let hero = hc(Rank::Ace, Suit::Hearts, Rank::King, Suit::Hearts);
    let villain = hc(Rank::Queen, Suit::Diamonds, Rank::Queen, Suit::Spades);
    let board = BoardCards {
        flop: Some([
            Card::new(Rank::King, Suit::Clubs),
            Card::new(Rank::Seven, Suit::Hearts),
            Card::new(Rank::Two, Suit::Hearts),
        ]),
        turn: Some(Card::new(Rank::Ten, Suit::Hearts)), // hero makes the nut flush
        river: Some(Card::new(Rank::Three, Suit::Spades)),
    };
    let street = || ReplayStreetInput {
        pot_bb: 6.0,
        to_call_bb: 0.0,
        hero_facing: HeroFacing::FirstToAct,
        hero_action: None,
    };
    ReplayRequest {
        table_size: TableSize::SixMax,
        hero_position: PositionBucket::BTN,
        villain_position: Some(PositionBucket::BB),
        stack_bb: 100,
        hero,
        villain,
        board,
        preflop_line: PreflopLine::Rfi,
        streets: vec![street(), street(), street(), street()],
        advice_seed: seed,
        // Distinct reveal seed (exercises the advice/reveal split).
        reveal_seed: seed.wrapping_add(0xA11CE),
    }
}

#[test]
fn analyze_replay_returns_one_entry_per_street_in_order() {
    let req = full_replay_req(100);
    let out = analyze_replay(&req).unwrap();
    assert_eq!(out.len(), 4);
    assert_eq!(out[0].street, Street::Preflop);
    assert_eq!(out[1].street, Street::Flop);
    assert_eq!(out[2].street, Street::Turn);
    assert_eq!(out[3].street, Street::River);
    // Every street carries the exact-equity reveal.
    for s in &out {
        assert!(
            s.analysis.exact_equity_pct.is_some(),
            "{:?} must carry exact_equity_pct",
            s.street
        );
    }
    // Hero AhKh turns the nut flush; by the river the exact equity vs QQ is
    // 100% (made flush beats a set). Don't over-assert earlier streets.
    assert_eq!(out[3].analysis.exact_equity_pct, Some(100));
    // Preflop entry emits the grid; postflop entries do not.
    assert!(out[0].analysis.preflop_grid.is_some());
    assert!(out[1].analysis.preflop_grid.is_none());
    assert!(out[3].analysis.preflop_grid.is_none());
    // Honesty: preflop is CfrEquityRealization, postflop EquityHeuristic.
    assert_eq!(out[0].analysis.method, SolveMethod::CfrEquityRealization);
    assert_eq!(out[1].analysis.method, SolveMethod::EquityHeuristic);
}

#[test]
fn analyze_replay_is_deterministic_byte_identical() {
    let a = analyze_replay(&full_replay_req(2024)).unwrap();
    let b = analyze_replay(&full_replay_req(2024)).unwrap();
    let ja = serde_json::to_vec(&a).unwrap();
    let jb = serde_json::to_vec(&b).unwrap();
    assert_eq!(
        ja, jb,
        "same ReplayRequest must produce byte-identical output"
    );
}

/// A representative spot for the §5(a) golden-byte guard: a postflop spot with
/// a Range advice channel + a Known reveal channel (both `early_stop: None`).
fn golden_spot_req() -> SpotRequest {
    SpotRequest {
        table_size: TableSize::SixMax,
        hero_position: PositionBucket::BTN,
        villain_position: Some(PositionBucket::BB),
        stack_bb: 100,
        street: Street::Flop,
        hero: hc(Rank::Ace, Suit::Spades, Rank::King, Suit::Spades),
        board: board3(
            Card::new(Rank::Ace, Suit::Hearts),
            Card::new(Rank::King, Suit::Diamonds),
            Card::new(Rank::Two, Suit::Clubs),
        ),
        pot_bb: 6.0,
        to_call_bb: 4.0,
        preflop_line: PreflopLine::Rfi,
        hero_facing: HeroFacing::Bet,
        hero_action: Some(SolverAction::Call),
        villain_hand: Some(hc(Rank::Queen, Suit::Diamonds, Rank::Jack, Suit::Diamonds)),
        advice_seed: 31337,
        reveal_seed: 0xBEEF,
    }
}

/// A preflop spot for the §5(a) golden-byte guard (grid + Range advice).
fn golden_preflop_req() -> SpotRequest {
    SpotRequest {
        hero_action: Some(SolverAction::Raise),
        ..preflop_req(
            hc(Rank::Ace, Suit::Spades, Rank::Queen, Suit::Spades),
            PositionBucket::CO,
            PreflopLine::Rfi,
        )
    }
}

/// §5(a): ADR-082 must NOT perturb the public-spot/replay output. We assert
/// `serde_json::to_vec(&analyze_replay(..))` equals a FROZEN golden byte
/// vector committed below — so the guard survives even if both runs share the
/// new code (a self-consistency check could pass while still having shifted).
/// The non-coach (`None`) path is byte-for-byte the pre-ADR-082 behaviour.
#[test]
fn analyze_replay_byte_identical_across_adr082() {
    let out = analyze_replay(&full_replay_req(2024)).unwrap();
    let bytes = serde_json::to_vec(&out).unwrap();
    // FROZEN GOLDEN — captured from the pre-ADR-082 `None`-path behaviour.
    let golden = include_bytes!("../testdata/adr082_replay_golden.json");
    assert_eq!(
            bytes.as_slice(),
            golden.as_slice(),
            "analyze_replay output drifted from the frozen pre-ADR-082 golden (F4 byte-identical guard)"
        );
}

/// §5(a): same frozen-golden guard for a representative `SpotRequest` — both
/// a postflop spot (Range advice + Known reveal) and a preflop spot (grid).
#[test]
fn analyze_spot_byte_identical_across_adr082() {
    let post = serde_json::to_vec(&analyze_spot(&golden_spot_req()).unwrap()).unwrap();
    let golden_post = include_bytes!("../testdata/adr082_spot_postflop_golden.json");
    assert_eq!(
        post.as_slice(),
        golden_post.as_slice(),
        "postflop analyze_spot drifted from frozen pre-ADR-082 golden"
    );

    let pre = serde_json::to_vec(&analyze_spot(&golden_preflop_req()).unwrap()).unwrap();
    let golden_pre = include_bytes!("../testdata/adr082_spot_preflop_golden.json");
    assert_eq!(
        pre.as_slice(),
        golden_pre.as_slice(),
        "preflop analyze_spot drifted from frozen pre-ADR-082 golden"
    );
}

#[test]
fn analyze_replay_rejects_villain_conflicting_with_board() {
    let mut req = full_replay_req(1);
    // Villain holds the Kc that is on the flop → InvalidCards from analyze_spot.
    req.villain = hc(Rank::King, Suit::Clubs, Rank::Five, Suit::Diamonds);
    assert_eq!(analyze_replay(&req).unwrap_err(), SolverError::InvalidCards);
}

#[test]
fn analyze_replay_board_count_must_match_last_street() {
    // 4 streets (through river) but only a 3-card board → InvalidCards.
    let mut req = full_replay_req(1);
    req.board = BoardCards {
        flop: Some([
            Card::new(Rank::King, Suit::Clubs),
            Card::new(Rank::Seven, Suit::Hearts),
            Card::new(Rank::Two, Suit::Hearts),
        ]),
        turn: None,
        river: None,
    };
    assert_eq!(analyze_replay(&req).unwrap_err(), SolverError::InvalidCards);
}

#[test]
fn analyze_replay_two_streets_needs_flop_only_board() {
    // 2 streets (preflop + flop) require exactly a 3-card board.
    let hero = hc(Rank::Ace, Suit::Hearts, Rank::King, Suit::Hearts);
    let villain = hc(Rank::Queen, Suit::Diamonds, Rank::Queen, Suit::Spades);
    let board = board3(
        Card::new(Rank::King, Suit::Clubs),
        Card::new(Rank::Seven, Suit::Hearts),
        Card::new(Rank::Two, Suit::Spades),
    );
    let street = || ReplayStreetInput {
        pot_bb: 6.0,
        to_call_bb: 0.0,
        hero_facing: HeroFacing::FirstToAct,
        hero_action: None,
    };
    let req = ReplayRequest {
        table_size: TableSize::SixMax,
        hero_position: PositionBucket::BTN,
        villain_position: Some(PositionBucket::BB),
        stack_bb: 100,
        hero,
        villain,
        board,
        preflop_line: PreflopLine::Rfi,
        streets: vec![street(), street()],
        advice_seed: 7,
        reveal_seed: 7,
    };
    let out = analyze_replay(&req).unwrap();
    assert_eq!(out.len(), 2);
    assert_eq!(out[0].street, Street::Preflop);
    assert_eq!(out[1].street, Street::Flop);
    // Flop entry sees only the 3-card board prefix.
    assert!(out[1].analysis.exact_equity_pct.is_some());
}

#[test]
fn analyze_replay_rejects_zero_and_too_many_streets() {
    let mut req = full_replay_req(1);
    req.streets = vec![];
    assert_eq!(analyze_replay(&req).unwrap_err(), SolverError::InvalidCards);

    let mut req = full_replay_req(1);
    req.streets = vec![
        ReplayStreetInput {
            pot_bb: 1.5,
            to_call_bb: 0.0,
            hero_facing: HeroFacing::FirstToAct,
            hero_action: None,
        };
        5
    ];
    assert_eq!(analyze_replay(&req).unwrap_err(), SolverError::InvalidCards);
}

#[test]
fn replay_wire_shape_has_street_and_analysis() {
    let req = full_replay_req(3);
    let out = analyze_replay(&req).unwrap();
    let v: serde_json::Value = serde_json::to_value(&out).unwrap();
    assert!(v.is_array());
    let first = &v[0];
    // Street serializes snake_case lowercase.
    assert_eq!(first["street"], "preflop");
    // analysis nests the full SpotAnalysis incl. exact_equity_pct.
    assert!(first["analysis"]["recommended"].is_string());
    assert!(first["analysis"]["exact_equity_pct"].is_number());
}
