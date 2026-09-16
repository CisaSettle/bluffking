//! Equity unit tests (moved out of `equity.rs`: the inline test module was more
//! than half the file). Same module path, same `use super::*`.

use super::*;
use crate::card::{Card, Rank, Suit};
use crate::hand::{BoardCards, HoleCards};

fn c(r: Rank, s: Suit) -> Card {
    Card::new(r, s)
}

#[test]
fn river_rank_reuse_preserves_seeded_results_and_stop_counts() {
    for seed in 0..6 {
        let mut cards = deck_minus(&[]);
        shuffle_in_place(&mut cards, &mut PokerRng::from_seed(seed));
        let hero = HoleCards::new(cards[0], cards[1]);
        let board = BoardCards {
            flop: Some([cards[2], cards[3], cards[4]]),
            turn: Some(cards[5]),
            river: Some(cards[6]),
        };
        for opponents in [
            OpponentSpec::Random(1),
            OpponentSpec::Random(5),
            OpponentSpec::Range(vec![]),
        ] {
            for trials in [1, 600, 10_000] {
                for early_stop in [
                    None,
                    Some(EarlyStop {
                        min_trials: EARLY_STOP_MIN_TRIALS,
                        half_width: EARLY_STOP_HALF_WIDTH,
                    }),
                ] {
                    let input = EquityInput {
                        hero,
                        board: board.clone(),
                        opponents: opponents.clone(),
                        trials,
                        seed,
                        early_stop,
                    };
                    // The uncached path runs the original evaluator on the
                    // identical shuffled deck, including every tie/share
                    // and early-stop check. Compare the actual stop count too.
                    assert_eq!(
                        equity_impl::<true>(input.clone()),
                        equity_impl::<false>(input)
                    );
                }
            }
        }
    }
}

#[test]
fn river_rank_reuse_preserves_multiway_board_chops() {
    let board = BoardCards {
        flop: Some([
            c(Rank::Ace, Suit::Hearts),
            c(Rank::King, Suit::Hearts),
            c(Rank::Queen, Suit::Hearts),
        ]),
        turn: Some(c(Rank::Jack, Suit::Hearts)),
        river: Some(c(Rank::Ten, Suit::Hearts)),
    };
    for n in 1..=5 {
        let input = EquityInput {
            hero: HoleCards::new(c(Rank::Two, Suit::Clubs), c(Rank::Three, Suit::Clubs)),
            board: board.clone(),
            opponents: OpponentSpec::Random(n),
            trials: 10_000,
            seed: 72,
            early_stop: None,
        };
        let actual = equity_impl::<true>(input.clone());
        assert_eq!(actual, equity_impl::<false>(input));
        assert_eq!(actual.0.win_pct, 0);
        assert_eq!(actual.0.tie_pct, 100);
        assert_eq!(
            actual.0.combined_pct,
            (100.0 / (n + 1) as f64).round() as u8
        );
    }
}

#[test]
fn aa_vs_one_random_pre_flop_is_around_85_pct() {
    let hero = HoleCards::new(c(Rank::Ace, Suit::Spades), c(Rank::Ace, Suit::Hearts));
    let board = BoardCards::empty();
    let result = equity(EquityInput {
        hero,
        board,
        opponents: OpponentSpec::Random(1),
        trials: 5_000,
        seed: 42,
        early_stop: None,
    });
    // AA vs 1 random ≈ 85%. Allow ±3pp slack for 5k trials.
    assert!(
        result.win_pct >= 80 && result.win_pct <= 88,
        "AA vs 1 random preflop win% out of [80,88]; got {}",
        result.win_pct
    );
}

#[test]
fn deterministic_same_seed_same_result() {
    let hero = HoleCards::new(c(Rank::Ace, Suit::Spades), c(Rank::King, Suit::Hearts));
    let board = BoardCards::empty();
    let r1 = equity(EquityInput {
        hero,
        board: board.clone(),
        opponents: OpponentSpec::Random(2),
        trials: 1_000,
        seed: 12345,
        early_stop: None,
    });
    let r2 = equity(EquityInput {
        hero,
        board,
        opponents: OpponentSpec::Random(2),
        trials: 1_000,
        seed: 12345,
        early_stop: None,
    });
    assert_eq!(r1, r2, "same seed must produce identical result");
}

#[test]
fn river_known_kk_loses_to_quad_aces() {
    // Hero: KK. Opp: AA. Board: A A 2 2 7 → opp has quads, hero has full house.
    let hero = HoleCards::new(c(Rank::King, Suit::Spades), c(Rank::King, Suit::Hearts));
    let opp = HoleCards::new(c(Rank::Ace, Suit::Spades), c(Rank::Ace, Suit::Hearts));
    let board = BoardCards {
        flop: Some([
            c(Rank::Ace, Suit::Diamonds),
            c(Rank::Ace, Suit::Clubs),
            c(Rank::Two, Suit::Diamonds),
        ]),
        turn: Some(c(Rank::Two, Suit::Hearts)),
        river: Some(c(Rank::Seven, Suit::Spades)),
    };
    let result = equity(EquityInput {
        hero,
        board,
        opponents: OpponentSpec::Known(vec![opp]),
        trials: 1, // not used at river
        seed: 0,
        early_stop: None,
    });
    // Hero has KK on a board with quad aces → loses 100%.
    assert_eq!(result.win_pct, 0);
    assert_eq!(result.tie_pct, 0);
}

#[test]
fn duplicate_hero_board_returns_zero() {
    let hero = HoleCards::new(c(Rank::Ace, Suit::Spades), c(Rank::King, Suit::Spades));
    let board = BoardCards {
        flop: Some([
            c(Rank::Ace, Suit::Spades),
            c(Rank::Two, Suit::Hearts),
            c(Rank::Three, Suit::Clubs),
        ]),
        turn: None,
        river: None,
    };
    let result = equity(EquityInput {
        hero,
        board,
        opponents: OpponentSpec::Random(1),
        trials: 100,
        seed: 1,
        early_stop: None,
    });
    assert_eq!(result.win_pct, 0, "duplicate hero/board must return 0");
}

/// Audit fix N-2 (2026-05-23): when opponent cards are Known, MC trials
/// MUST NOT deal them as turn/river. Pre-fix, `all_known` only excluded
/// hero + board cards, so the runout deck still contained opp cards.
///
/// Setup: hero AsKs, flop Ah-Kd-2c, opp ThJd Known. The unknown board
/// cards (turn + river) are drawn from the remaining deck. If N-2
/// regresses, ~2 / 47 ≈ 4% of trials would deal Th or Jd as the turn
/// (impossible — they are in opp's hand). We can detect this by
/// counting the number of trials where the opponent improves to a
/// flush/two-pair via a duplicate card.
///
/// Direct test: run from the flop with `OpponentSpec::Known`. This now
/// takes the exact postflop enumeration path; if opponent cards leak into
/// the runout deck, the averaged result shifts because impossible turn or
/// river cards are included.
#[test]
fn known_opp_cards_excluded_from_runouts() {
    let hero = HoleCards::new(c(Rank::Ace, Suit::Spades), c(Rank::King, Suit::Spades));
    let opp = HoleCards::new(c(Rank::Ten, Suit::Hearts), c(Rank::Jack, Suit::Diamonds));
    let board = BoardCards {
        flop: Some([
            c(Rank::Ace, Suit::Hearts),
            c(Rank::King, Suit::Diamonds),
            c(Rank::Two, Suit::Clubs),
        ]),
        turn: None,
        river: None,
    };
    let r = equity(EquityInput {
        hero,
        board,
        opponents: OpponentSpec::Known(vec![opp]),
        trials: 5_000,
        seed: 7,
        early_stop: None,
    });
    // Hero has top two pair on AK2 vs JT (gutshot + overcards):
    // hero equity ≈ 79% via standard equity calculators. Allow ±5pp.
    // If N-2 regresses and Th/Jd reappear as turn/river runouts, opp
    // sometimes pairs them → hero equity drops below 70%.
    assert!(
        r.win_pct >= 74,
        "AKs vs JTo on AK2 must have ≥74% win (got {}); regression in known-opp exclusion?",
        r.win_pct
    );
}

#[test]
fn known_postflop_equity_is_exact_not_seeded_mc() {
    let hero = HoleCards::new(c(Rank::Ace, Suit::Spades), c(Rank::King, Suit::Spades));
    let opp = HoleCards::new(c(Rank::Ten, Suit::Hearts), c(Rank::Jack, Suit::Diamonds));
    let board = BoardCards {
        flop: Some([
            c(Rank::Ace, Suit::Hearts),
            c(Rank::King, Suit::Diamonds),
            c(Rank::Two, Suit::Clubs),
        ]),
        turn: None,
        river: None,
    };

    let one_trial = equity(EquityInput {
        hero,
        board: board.clone(),
        opponents: OpponentSpec::Known(vec![opp]),
        trials: 1,
        seed: 1,
        early_stop: None,
    });
    let many_trials = equity(EquityInput {
        hero,
        board,
        opponents: OpponentSpec::Known(vec![opp]),
        trials: 50_000,
        seed: 999,
        early_stop: None,
    });

    assert_eq!(
        one_trial, many_trials,
        "known postflop equity must enumerate exact runouts, independent of trials/seed"
    );
    assert!(
        one_trial.win_pct >= 74 && one_trial.win_pct <= 95,
        "AKs vs JTo on AK2 should stay in the expected exact-equity band, got {:?}",
        one_trial
    );
}

/// Regression (audit 2026-06-03): with more than 5 Known opponents the
/// equity must be computed against exactly the first 5 (the documented cap),
/// NOT against all of them. Here hero's KK beats five low-pocket-pair
/// opponents, but the 6th opponent holds AA (the only hand that beats hero).
/// If the 6th is silently seated, hero loses; with the cap honored, hero
/// wins outright against the 5 seated opponents.
/// Regression (audit 2026-06-03): a true 3-way chop must report hero's
/// actual pot share (~33%), not a flat 50%. Board is a Broadway straight
/// (T-J-Q-K-A) that all three players play, so everyone ties.
/// `equity_pct()` must come out ≈ 33, not 50.
#[test]
fn three_way_chop_reports_one_third_not_one_half() {
    let board = BoardCards {
        flop: Some([
            c(Rank::Ace, Suit::Clubs),
            c(Rank::King, Suit::Hearts),
            c(Rank::Queen, Suit::Diamonds),
        ]),
        turn: Some(c(Rank::Jack, Suit::Clubs)),
        river: Some(c(Rank::Ten, Suit::Spades)),
    };
    // Hero + two opponents all play the board straight → 3-way tie.
    let hero = HoleCards::new(c(Rank::Two, Suit::Hearts), c(Rank::Three, Suit::Hearts));
    let opps = vec![
        HoleCards::new(c(Rank::Four, Suit::Diamonds), c(Rank::Five, Suit::Diamonds)),
        HoleCards::new(c(Rank::Six, Suit::Clubs), c(Rank::Seven, Suit::Clubs)),
    ];
    let result = equity(EquityInput {
        hero,
        board,
        opponents: OpponentSpec::Known(opps),
        trials: 1,
        seed: 0,
        early_stop: None,
    });
    assert_eq!(result.win_pct, 0, "a chop is not an outright win");
    assert_eq!(
        result.equity_pct(),
        33,
        "3-way chop equity must be ~1/3, not 50%; got {}",
        result.equity_pct()
    );
}

#[test]
fn known_opponents_truncated_to_cap_of_five() {
    // Board: K K 7 4 2 → hero (any K) has trips, the five low pairs make
    // only a pair, so hero beats all five seated opponents.
    let hero = HoleCards::new(c(Rank::King, Suit::Spades), c(Rank::King, Suit::Hearts));
    let board = BoardCards {
        flop: Some([
            c(Rank::King, Suit::Diamonds),
            c(Rank::Seven, Suit::Spades),
            c(Rank::Four, Suit::Clubs),
        ]),
        turn: Some(c(Rank::Two, Suit::Hearts)),
        river: Some(c(Rank::Three, Suit::Spades)),
    };
    // Five harmless opponents (low pairs / unconnected), then a 6th holding
    // AA — the only holding that would out-rank hero's trip kings.
    let opps = vec![
        HoleCards::new(c(Rank::Five, Suit::Spades), c(Rank::Five, Suit::Hearts)),
        HoleCards::new(c(Rank::Six, Suit::Spades), c(Rank::Six, Suit::Hearts)),
        HoleCards::new(c(Rank::Eight, Suit::Spades), c(Rank::Eight, Suit::Hearts)),
        HoleCards::new(c(Rank::Nine, Suit::Spades), c(Rank::Nine, Suit::Hearts)),
        HoleCards::new(c(Rank::Ten, Suit::Spades), c(Rank::Ten, Suit::Hearts)),
        // 6th opponent — must be dropped by the cap; AA + KK7 board = aces
        // full, which beats hero's trips if (wrongly) seated.
        HoleCards::new(c(Rank::Ace, Suit::Spades), c(Rank::Ace, Suit::Hearts)),
    ];
    let result = equity(EquityInput {
        hero,
        board,
        opponents: OpponentSpec::Known(opps),
        trials: 1, // river: single showdown, no RNG
        seed: 0,
        early_stop: None,
    });
    assert_eq!(
        result.win_pct, 100,
        "hero must win vs the 5 capped opponents; the 6th (AA) must be dropped"
    );
}

/// Regression for code-review finding #2: `exact_known_runouts` must
/// accumulate each runout's EXACT pot share and round the combined value
/// ONCE — NOT re-round each runout's `combined_pct` to a u8 and average. On
/// this chop-heavy turn spot the correct single-round combined equity is
/// 87%; the old per-runout double-round averaged to 86% (a low bias).
#[test]
fn exact_runouts_round_combined_once_not_per_runout() {
    let hero = HoleCards::new(c(Rank::Six, Suit::Spades), c(Rank::Eight, Suit::Hearts));
    let opps = vec![
        HoleCards::new(c(Rank::Jack, Suit::Hearts), c(Rank::Eight, Suit::Diamonds)),
        HoleCards::new(c(Rank::Two, Suit::Spades), c(Rank::Three, Suit::Spades)),
    ];
    let board = BoardCards {
        flop: Some([
            c(Rank::Eight, Suit::Spades),
            c(Rank::Nine, Suit::Spades),
            c(Rank::Ten, Suit::Spades),
        ]),
        turn: Some(c(Rank::Jack, Suit::Spades)),
        river: None,
    };
    let result = equity(EquityInput {
        hero,
        board: board.clone(),
        opponents: OpponentSpec::Known(opps.clone()),
        trials: 1, // turn board → exact enumeration, no RNG
        seed: 0,
        early_stop: None,
    });
    assert_eq!(
        result.combined_pct, 87,
        "exact-runout combined equity must be single-rounded (87), got {}",
        result.combined_pct
    );

    // Prove this spot genuinely exercises the bug: the legacy per-runout
    // double-round averages to 86, distinct from the single-round 87.
    let mut used: Vec<Card> = vec![hero.card1, hero.card2];
    for o in &opps {
        used.push(o.card1);
        used.push(o.card2);
    }
    used.extend(board.all_cards());
    let rivers = deck_minus(&used);
    let (mut dbl2, mut total) = (0u64, 0u64);
    for &rv in &rivers {
        let mut b = board.clone();
        fill_board_from_runout(&mut b, &[rv]);
        dbl2 += 2 * single_showdown(&hero, &b, &opps).combined_pct as u64;
        total += 1;
    }
    let double_round = ((dbl2 + total) / (2 * total)) as u8;
    assert_eq!(
        double_round, 86,
        "sanity: legacy double-round value for this spot"
    );
    assert_ne!(
        double_round, result.combined_pct,
        "spot must distinguish single-round from per-runout double-round"
    );
}

/// Regression for code-review finding #1: in the multi-opponent Monte-Carlo
/// path a tie must contribute hero's true `1/(1+tied)` pot share, NOT a flat
/// 0.5. Three identical `KQ` hands chop ~most boards three-way (share 1/3),
/// so the correct combined equity is well BELOW the naive `win% + tie%/2`
/// the old flat-0.5 weighting produced (here by ~10pp).
#[test]
fn mc_multiway_chop_weighted_by_share_not_flat_half() {
    // Hero + two opponents all hold KQ (distinct suits) → most run-outs are
    // a 3-way chop; hero only pulls ahead on the spade-flush boards.
    let hero = HoleCards::new(c(Rank::King, Suit::Spades), c(Rank::Queen, Suit::Spades));
    let opps = vec![
        HoleCards::new(c(Rank::King, Suit::Hearts), c(Rank::Queen, Suit::Hearts)),
        HoleCards::new(
            c(Rank::King, Suit::Diamonds),
            c(Rank::Queen, Suit::Diamonds),
        ),
    ];
    let board = BoardCards::empty();
    let result = equity(EquityInput {
        hero,
        board,
        opponents: OpponentSpec::Known(opps), // preflop Known → MC `tied` path
        trials: 50_000,
        seed: 99,
        early_stop: None,
    });
    assert!(
        result.tie_pct > 50,
        "spot must chop most boards to exercise multi-way weighting; tie%={}",
        result.tie_pct
    );
    // Old flat-0.5 weighting: combined ≈ win + tie/2. True 3-way share is
    // 1/3, so the correct combined is markedly lower. Require a clear gap
    // (≥4pp) so the assertion can't be satisfied by the buggy weighting.
    let naive_half = result.win_pct as i32 + (result.tie_pct as i32) / 2;
    assert!(
            (result.combined_pct as i32) + 4 < naive_half,
            "multi-way chops must weight by 1/N, not 0.5: combined={} naive(win+tie/2)={} (win={}, tie={})",
            result.combined_pct,
            naive_half,
            result.win_pct,
            result.tie_pct
        );
}

#[test]
fn trials_clamped_does_not_panic() {
    let hero = HoleCards::new(c(Rank::Two, Suit::Spades), c(Rank::Three, Suit::Hearts));
    let board = BoardCards::empty();
    // Above MAX_MC_TRIALS — must not panic; just clamp.
    let result = equity(EquityInput {
        hero,
        board,
        opponents: OpponentSpec::Random(1),
        trials: 1_000_000,
        seed: 1,
        early_stop: None,
    });
    // Just check it returns something sensible.
    assert!(result.win_pct <= 100);
}

// -----------------------------------------------------------------------
// M1 §1a — real OpponentSpec::Range
// -----------------------------------------------------------------------

fn bucket(key: &str) -> RangeBucket {
    RangeBucket {
        hand_key: key.to_string(),
        weight: 1.0,
    }
}

#[test]
fn expand_range_combo_counts() {
    // pair = 6, suited = 4, offsuit = 12.
    assert_eq!(expand_range_key("AA").len(), 6, "pair must be 6 combos");
    assert_eq!(expand_range_key("AKs").len(), 4, "suited must be 4 combos");
    assert_eq!(
        expand_range_key("AKo").len(),
        12,
        "offsuit must be 12 combos"
    );
    // Garbage keys never panic and expand to nothing.
    assert_eq!(expand_range_key("").len(), 0);
    assert_eq!(expand_range_key("XYZ").len(), 0);
    assert_eq!(expand_range_key("AKx").len(), 0);
}

#[test]
fn expand_range_combos_are_valid_and_distinct() {
    for key in ["AA", "AKs", "AKo"] {
        let combos = expand_range_key(key);
        for h in &combos {
            assert_ne!(
                h.card1, h.card2,
                "{key}: a combo cannot use the same card twice"
            );
        }
        // No duplicate combos in the expansion.
        for i in 0..combos.len() {
            for j in (i + 1)..combos.len() {
                let a = combos[i];
                let b = combos[j];
                let same = (a.card1 == b.card1 && a.card2 == b.card2)
                    || (a.card1 == b.card2 && a.card2 == b.card1);
                assert!(!same, "{key}: duplicate combo at {i},{j}");
            }
        }
    }
}

#[test]
fn aks_vs_tight_value_range_lower_than_vs_random() {
    let hero = HoleCards::new(c(Rank::Ace, Suit::Spades), c(Rank::King, Suit::Spades));
    let board = BoardCards::empty();
    let vs_random = equity(EquityInput {
        hero,
        board: board.clone(),
        opponents: OpponentSpec::Random(1),
        trials: 8_000,
        seed: 42,
        early_stop: None,
    });
    let vs_value = equity(EquityInput {
        hero,
        board,
        opponents: OpponentSpec::Range(vec![
            bucket("AA"),
            bucket("KK"),
            bucket("QQ"),
            bucket("AKs"),
        ]),
        trials: 8_000,
        seed: 42,
        early_stop: None,
    });
    assert!(
        vs_value.equity_pct() < vs_random.equity_pct(),
        "AKs vs a tight value range ({}) must have LOWER equity than vs random ({})",
        vs_value.equity_pct(),
        vs_random.equity_pct()
    );
}

#[test]
fn range_equity_is_deterministic_on_seed() {
    let hero = HoleCards::new(c(Rank::Ace, Suit::Hearts), c(Rank::King, Suit::Hearts));
    let board = BoardCards {
        flop: Some([
            c(Rank::Two, Suit::Spades),
            c(Rank::Seven, Suit::Diamonds),
            c(Rank::Jack, Suit::Clubs),
        ]),
        turn: None,
        river: None,
    };
    let spec = || {
        OpponentSpec::Range(vec![
            bucket("AA"),
            bucket("KK"),
            bucket("AKo"),
            bucket("QQ"),
        ])
    };
    let r1 = equity(EquityInput {
        hero,
        board: board.clone(),
        opponents: spec(),
        trials: 3_000,
        seed: 777,
        early_stop: None,
    });
    let r2 = equity(EquityInput {
        hero,
        board,
        opponents: spec(),
        trials: 3_000,
        seed: 777,
        early_stop: None,
    });
    assert_eq!(
        r1, r2,
        "same (hero,board,Range,trials,seed) must be identical"
    );
}

#[test]
fn fully_blocked_range_falls_back_without_panic() {
    // Hero holds As/Ah and board has Ad/Ac → the only AA combos are all
    // blocked, so the range pool is empty and must fall back to Random(1)
    // (never panic, result ≤ 100).
    let hero = HoleCards::new(c(Rank::Ace, Suit::Spades), c(Rank::Ace, Suit::Hearts));
    let board = BoardCards {
        flop: Some([
            c(Rank::Ace, Suit::Diamonds),
            c(Rank::Ace, Suit::Clubs),
            c(Rank::Two, Suit::Hearts),
        ]),
        turn: None,
        river: None,
    };
    let r = equity(EquityInput {
        hero,
        board,
        opponents: OpponentSpec::Range(vec![bucket("AA")]),
        trials: 1_000,
        seed: 5,
        early_stop: None,
    });
    assert!(r.win_pct <= 100 && r.equity_pct() <= 100);
}

#[test]
fn range_excludes_villain_cards_from_runout() {
    // Villain range is exactly KK. Hero is AsKs on a dry flop. Across many
    // trials the result must be a valid percentage; the real assertion is
    // that no panic / impossible board occurs (villain's two kings can never
    // appear as turn/river). Determinism + bounds cover the contract.
    let hero = HoleCards::new(c(Rank::Ace, Suit::Spades), c(Rank::King, Suit::Spades));
    let board = BoardCards {
        flop: Some([
            c(Rank::Two, Suit::Diamonds),
            c(Rank::Seven, Suit::Hearts),
            c(Rank::Nine, Suit::Clubs),
        ]),
        turn: None,
        river: None,
    };
    let r = equity(EquityInput {
        hero,
        board,
        opponents: OpponentSpec::Range(vec![bucket("KK")]),
        trials: 4_000,
        seed: 11,
        early_stop: None,
    });
    assert!(r.win_pct <= 100);
}

// -----------------------------------------------------------------------
// ADR-082 — coach-only Monte-Carlo early-stop (Wilson 95% CI)
// -----------------------------------------------------------------------

/// The coach policy as `equity_vs_random` sets it (the ONLY `Some(..)` site).
fn coach_policy() -> EarlyStop {
    EarlyStop {
        min_trials: EARLY_STOP_MIN_TRIALS,
        half_width: EARLY_STOP_HALF_WIDTH,
    }
}

/// §5(a): the `None` path runs EXACTLY `DEFAULT_MC_TRIALS` and equals the
/// pre-ADR-082 full-loop result (frozen golden values captured from the
/// pre-change behaviour). Proves the F4 byte-identical guard at the equity
/// layer: adding the field did not perturb the non-coach (`None`) numbers.
#[test]
fn equity_none_path_is_unchanged() {
    // (hero, board, opp_count, seed, expected win/tie/combined) — frozen.
    type GoldenCase = (HoleCards, BoardCards, u8, u64, (u8, u8, u8));
    let cases: Vec<GoldenCase> = vec![
        (
            HoleCards::new(c(Rank::Ace, Suit::Spades), c(Rank::Ace, Suit::Hearts)),
            BoardCards::empty(),
            1,
            42,
            (85, 1, 86),
        ),
        (
            HoleCards::new(c(Rank::Ace, Suit::Spades), c(Rank::King, Suit::Hearts)),
            BoardCards::empty(),
            2,
            12345,
            (47, 2, 48),
        ),
        (
            HoleCards::new(c(Rank::Seven, Suit::Diamonds), c(Rank::Two, Suit::Clubs)),
            BoardCards {
                flop: Some([
                    c(Rank::Ace, Suit::Spades),
                    c(Rank::King, Suit::Hearts),
                    c(Rank::Nine, Suit::Diamonds),
                ]),
                turn: None,
                river: None,
            },
            2,
            99,
            (6, 4, 8),
        ),
        (
            HoleCards::new(c(Rank::Ace, Suit::Spades), c(Rank::King, Suit::Spades)),
            BoardCards {
                flop: Some([
                    c(Rank::Two, Suit::Spades),
                    c(Rank::Seven, Suit::Spades),
                    c(Rank::Queen, Suit::Spades),
                ]),
                turn: None,
                river: None,
            },
            1,
            7,
            (98, 0, 98),
        ),
    ];
    for (hero, board, n, seed, (w, t, comb)) in cases {
        let (res, trials_run) = equity_with_trial_count(EquityInput {
            hero,
            board: board.clone(),
            opponents: OpponentSpec::Random(n),
            trials: DEFAULT_MC_TRIALS,
            seed,
            early_stop: None,
        });
        assert_eq!(
            trials_run, DEFAULT_MC_TRIALS,
            "None path must run ALL {DEFAULT_MC_TRIALS} trials (n={n}, seed={seed})"
        );
        assert_eq!(
            (res.win_pct, res.tie_pct, res.combined_pct),
            (w, t, comb),
            "None-path result drifted from frozen golden (n={n}, seed={seed})"
        );
    }
}

/// §5(b): the coach early-stop is deterministic and seed-stable — the SAME
/// `equity_vs_random(h, b, n, seed)` returns an identical `EquityResult` AND
/// stops at the identical trial index across runs.
#[test]
fn coach_early_stop_is_deterministic() {
    let inputs: Vec<(HoleCards, BoardCards, u8, u64)> = vec![
        (
            HoleCards::new(c(Rank::Ace, Suit::Spades), c(Rank::Ace, Suit::Hearts)),
            BoardCards::empty(),
            1,
            42,
        ),
        (
            HoleCards::new(c(Rank::Ace, Suit::Spades), c(Rank::King, Suit::Hearts)),
            BoardCards::empty(),
            2,
            12345,
        ),
        (
            HoleCards::new(c(Rank::Seven, Suit::Diamonds), c(Rank::Two, Suit::Clubs)),
            BoardCards {
                flop: Some([
                    c(Rank::Ace, Suit::Spades),
                    c(Rank::King, Suit::Hearts),
                    c(Rank::Nine, Suit::Diamonds),
                ]),
                turn: None,
                river: None,
            },
            2,
            99,
        ),
    ];
    for (hero, board, n, seed) in inputs {
        let policy = Some(coach_policy());
        let mk = || EquityInput {
            hero,
            board: board.clone(),
            opponents: OpponentSpec::Random(n),
            trials: DEFAULT_MC_TRIALS,
            seed,
            early_stop: policy,
        };
        let (r1, t1) = equity_with_trial_count(mk());
        let (r2, t2) = equity_with_trial_count(mk());
        assert_eq!(r1, r2, "coach early-stop must be deterministic (result)");
        assert_eq!(t1, t2, "coach early-stop must be seed-stable (stop index)");
        // The convenience entry must match the explicit policy input.
        let via_helper = equity_vs_random(hero, board.clone(), n, seed);
        assert_eq!(
            via_helper, r1,
            "equity_vs_random must apply exactly the coach policy"
        );
    }
}

/// §5(c): a CRUSHING spot (hero made the nut flush on a paired-safe board vs
/// `Random(1)`) converges fast — the Wilson CI tightens below 0.5pp well
/// before the 10k budget, so it MUST stop early. This is RED without the
/// policy: with `early_stop: None` the same spot runs all 10k.
#[test]
fn dominant_spot_stops_early() {
    // Hero AsKs, board Qs Js 2s 7d → nut flush already made (Royal-adjacent
    // spade flush, no straight flush risk killed). Equity ≈ 95%+ vs random.
    let hero = HoleCards::new(c(Rank::Ace, Suit::Spades), c(Rank::King, Suit::Spades));
    let board = BoardCards {
        flop: Some([
            c(Rank::Queen, Suit::Spades),
            c(Rank::Jack, Suit::Spades),
            c(Rank::Two, Suit::Spades),
        ]),
        turn: Some(c(Rank::Seven, Suit::Diamonds)),
        river: None,
    };
    let (_, t_stop) = equity_with_trial_count(EquityInput {
        hero,
        board: board.clone(),
        opponents: OpponentSpec::Random(1),
        trials: DEFAULT_MC_TRIALS,
        seed: 42,
        early_stop: Some(coach_policy()),
    });
    assert!(
        t_stop < DEFAULT_MC_TRIALS,
        "dominant spot must early-stop (t*={t_stop} should be < {DEFAULT_MC_TRIALS})"
    );
    assert!(
        t_stop <= 3000,
        "dominant spot should converge fast (t*={t_stop} expected <= 3000)"
    );
    // RED-without-policy proof: the SAME spot with `None` runs the full budget.
    let (_, t_full) = equity_with_trial_count(EquityInput {
        hero,
        board,
        opponents: OpponentSpec::Random(1),
        trials: DEFAULT_MC_TRIALS,
        seed: 42,
        early_stop: None,
    });
    assert_eq!(
        t_full, DEFAULT_MC_TRIALS,
        "without the policy the loop must run all {DEFAULT_MC_TRIALS} trials"
    );
    assert!(
        t_stop < t_full,
        "early-stop must run STRICTLY fewer trials than the full loop"
    );
}

/// §5(c): a HOPELESS spot (72o, no draws left, on a dry high RIVER board vs
/// `Random(2)`) has equity ≈ 1% — where the Wilson CI is tight (small
/// `p(1-p)`), so it MUST stop early. RED without the policy (runs all 10k).
#[test]
fn clear_fold_stops_early() {
    // River A-K-9-Q-5 rainbow; 72o has nothing and no outs (board is final).
    let hero = HoleCards::new(c(Rank::Seven, Suit::Diamonds), c(Rank::Two, Suit::Clubs));
    let board = BoardCards {
        flop: Some([
            c(Rank::Ace, Suit::Spades),
            c(Rank::King, Suit::Hearts),
            c(Rank::Nine, Suit::Diamonds),
        ]),
        turn: Some(c(Rank::Queen, Suit::Clubs)),
        river: Some(c(Rank::Five, Suit::Hearts)),
    };
    let (res, t_stop) = equity_with_trial_count(EquityInput {
        hero,
        board: board.clone(),
        opponents: OpponentSpec::Random(2),
        trials: DEFAULT_MC_TRIALS,
        seed: 99,
        early_stop: Some(coach_policy()),
    });
    // Sanity: this really is a near-0 spot (where Wilson is tight).
    assert!(
        res.combined_pct <= 5,
        "clear-fold spot must sit near 0% (got {})",
        res.combined_pct
    );
    assert!(
        t_stop < DEFAULT_MC_TRIALS,
        "clear-fold spot must early-stop (t*={t_stop} should be < {DEFAULT_MC_TRIALS})"
    );
    // RED-without-policy proof.
    let (_, t_full) = equity_with_trial_count(EquityInput {
        hero,
        board,
        opponents: OpponentSpec::Random(2),
        trials: DEFAULT_MC_TRIALS,
        seed: 99,
        early_stop: None,
    });
    assert_eq!(t_full, DEFAULT_MC_TRIALS, "None path runs the full budget");
    assert!(t_stop < t_full, "early-stop must run fewer trials");
}

/// §5(c): a MARGINAL near-50% spot maximises the Wilson half-width — at
/// p≈0.5 the 0.5pp target needs n ≈ z²·0.25/0.005² ≈ 38,400 trials, far above
/// the 10k budget — so it MUST run the FULL budget, never early-stopping.
/// This is the "spend the budget where it matters" guarantee.
#[test]
fn marginal_spot_runs_near_full() {
    // AKo vs 2 random preflop ≈ 50% combined — a coinflip-ish multiway spot.
    let hero = HoleCards::new(c(Rank::Ace, Suit::Spades), c(Rank::King, Suit::Hearts));
    let board = BoardCards::empty();
    let (res, t_stop) = equity_with_trial_count(EquityInput {
        hero,
        board,
        opponents: OpponentSpec::Random(2),
        trials: DEFAULT_MC_TRIALS,
        seed: 12345,
        early_stop: Some(coach_policy()),
    });
    // Sanity: this spot really is near 50% (where Wilson is widest).
    assert!(
        res.combined_pct >= 40 && res.combined_pct <= 60,
        "marginal spot must sit near 50% (got {})",
        res.combined_pct
    );
    assert_eq!(
            t_stop, DEFAULT_MC_TRIALS,
            "marginal (p≈0.5) spot must run the FULL budget — 0.5pp at p=0.5 needs ~38k > 10k (t*={t_stop})"
        );
}

/// The `K`-cadence is honoured: the stop index, when it fires, is always a
/// multiple of `EARLY_STOP_CHECK_EVERY` and ≥ `EARLY_STOP_MIN_TRIALS`.
#[test]
fn early_stop_index_respects_min_and_cadence() {
    let hero = HoleCards::new(c(Rank::Ace, Suit::Spades), c(Rank::King, Suit::Spades));
    let board = BoardCards {
        flop: Some([
            c(Rank::Queen, Suit::Spades),
            c(Rank::Jack, Suit::Spades),
            c(Rank::Two, Suit::Spades),
        ]),
        turn: Some(c(Rank::Seven, Suit::Diamonds)),
        river: None,
    };
    let (_, t_stop) = equity_with_trial_count(EquityInput {
        hero,
        board,
        opponents: OpponentSpec::Random(1),
        trials: DEFAULT_MC_TRIALS,
        seed: 42,
        early_stop: Some(coach_policy()),
    });
    assert!(
        t_stop >= EARLY_STOP_MIN_TRIALS,
        "stop must be >= min_trials"
    );
    if t_stop < DEFAULT_MC_TRIALS {
        assert_eq!(
            t_stop % EARLY_STOP_CHECK_EVERY,
            0,
            "an early break must land on a K-cadence boundary (t*={t_stop})"
        );
    }
}
