//! Offline GTOpen example for the five- through nine-handed 100 BB policies.
//! See docs/architecture/poker-gto-baseline.md for the pinned solver and replay commands.
use solver::preflop::{
    equity::EquityTable,
    multiway::{set_training_window, SAMPLES},
    PreflopConfig, PreflopSolver,
};
use std::sync::Arc;

fn config(seats: usize) -> Result<PreflopConfig, String> {
    let positions = ["UTG", "UTG1", "UTG2", "LJ", "HJ", "CO", "BTN", "SB", "BB"];
    let mut posts = vec![0.0; seats];
    posts[seats - 2] = 0.5;
    posts[seats - 1] = 1.0;
    serde_json::from_value(serde_json::json!({
        "positions": &positions[9-seats..], "posts": posts, "stack":100, "limp":false,
        "open_raises":[2.5], "raise_mults":[3.0], "fourbet_mults":[2.0], "max_raises":2,
        "add_allin":true, "allin_threshold":0.8, "rake_pct":0.0, "card_removal":false,
        "realization":"leafnet", "leaf_net":"v3"
    }))
    .map_err(|e| e.to_string())
}

fn verify_snapshot(solver: &PreflopSolver, path: &str) -> Result<bool, String> {
    solver.save_game(path)?;
    // Reload refreshes the learned flop values at the saved average ranges.
    let frozen = PreflopSolver::load_game(path, solver.eq.clone())?;
    assert!(!frozen.has_overrides());
    let (gaps, _) = frozen.gaps_and_evs();
    eprintln!(
        "{} seats, {} iterations, seat gaps {gaps:?}",
        frozen.n, frozen.iteration
    );
    Ok(gaps
        .iter()
        .all(|gap| gap.is_finite() && (0.0..=0.01).contains(gap)))
}

fn next_particle_start(state: &mut u64) -> usize {
    *state = state.wrapping_add(0x9e3779b97f4a7c15);
    let mut z = *state;
    z = (z ^ (z >> 30)).wrapping_mul(0xbf58476d1ce4e5b9);
    z = (z ^ (z >> 27)).wrapping_mul(0x94d049bb133111eb);
    (z ^ (z >> 31)) as usize
}

fn project(args: &[String]) -> Result<(), String> {
    if args.len() < 5 {
        return Err("project <100BB snapshot> <output.preflop> <stack BB for every seat>".into());
    }
    let eq = Arc::new(EquityTable::load_or_build("cache/preflop_eq169.bin", 20000));
    let seed = PreflopSolver::load_game(&args[1], eq.clone())?;
    let stacks: Vec<f64> = args[3..]
        .iter()
        .map(|s| s.parse().map_err(|_| "invalid stack".to_string()))
        .collect::<Result<_, _>>()?;
    if seed.n != stacks.len()
        || seed.cfg.stacks.is_some()
        || seed.cfg.stack != 100.0
        || seed.multiway_equity_model() != "coupled_deck_v1"
        || stacks
            .iter()
            .any(|v| !v.is_finite() || *v <= 1.0 || (2.0 * v).fract() != 0.0)
    {
        return Err(
            "expected an unrestricted equal-100BB game and exact contestable half-blind stacks"
                .into(),
        );
    }
    let mut cfg = seed.cfg.clone();
    cfg.stack = stacks.iter().copied().fold(0.0, f64::max);
    cfg.stacks = Some(stacks);
    let mut target = PreflopSolver::new(cfg, eq)?;
    target.project_shark_average(&seed);
    target.save_game(&args[2])
}

fn main() -> Result<(), String> {
    let args: Vec<_> = std::env::args().skip(1).collect();
    if std::env::vars().any(|(k, _)| k.starts_with("PREFLOP_")) {
        return Err("unset PREFLOP_* research overrides".into());
    }
    if args.first().is_some_and(|a| a == "project") {
        return project(&args);
    }
    if !matches!(args.len(), 2 | 4) {
        return Err(
            "Run train_shark_ring <5..9> <output.preflop> [short-seat 50] without PREFLOP_* overrides".into(),
        );
    }
    let seats: usize = args[0].parse().map_err(|_| "invalid seat count")?;
    if !(5..=9).contains(&seats) || (seats == 5 && args.len() != 2) {
        return Err(
            "use this trainer for 5–9 equal stacks or 6–9 with a single short stack".into(),
        );
    }
    let target = &args[1];
    let seed_path = format!("{target}.seed");
    let equity = Arc::new(EquityTable::load_or_build("cache/preflop_eq169.bin", 20000));
    let mut cfg = config(seats)?;
    if args.len() == 2 && seats <= 6 {
        cfg.open_raises = vec![2.0, 2.5, 3.0];
        if seats == 5 {
            cfg.max_raises = 3;
        }
    }
    if args.len() == 4 {
        let seat: usize = args[2].parse().map_err(|_| "invalid short seat")?;
        let depth: f64 = args[3].parse().map_err(|_| "invalid short stack")?;
        if seat >= seats || !(10.0..100.0).contains(&depth) || (2.0 * depth).fract() != 0.0 {
            return Err(
                "short stack must name a dealt seat and have 10–99.5 BB in half-blinds".into(),
            );
        }
        let mut stacks = vec![cfg.stack; seats];
        stacks[seat] = depth;
        cfg.stacks = Some(stacks);
    }
    {
        // Inexpensive initialization. This model is never exported to the table.
        let mut seed = PreflopSolver::new(cfg.clone(), equity.clone())?;
        seed.set_multiway_equity_model("legacy_product")?;
        assert!(seed.refresh_leaf_values().is_some_and(|r| r.priced > 0));
        for i in 1..=20000 {
            seed.iterate();
            if (matches!(i, 1 | 8 | 32 | 100) || i % 250 == 0)
                && verify_snapshot(&seed, &seed_path)?
            {
                break;
            }
            if i == 20000 {
                return Err("seed did not converge".into());
            }
        }
    }
    let mut solver = PreflopSolver::new(cfg, equity.clone())?;
    assert_eq!(solver.multiway_equity_model(), "coupled_deck_v1");
    let seed = PreflopSolver::load_game(&seed_path, equity.clone())?;
    solver.initialize_shark_training(&seed);
    drop(seed);
    // Recreate the solver's traversal caches after initializing the new target game.
    solver.save_game(target)?;
    drop(solver);
    let mut solver = PreflopSolver::load_game(target, equity)?;
    assert!(solver.refresh_leaf_values().is_some_and(|r| r.priced > 0));
    let batch = if seats <= 7 { 8 } else { 4 };
    let mut random_state = 0x6365752d42;
    for i in 1..=4000 {
        set_training_window(next_particle_start(&mut random_state), batch);
        solver.iterate();
        if matches!(i, 100 | 250 | 500 | 1000 | 2000 | 4000) {
            set_training_window(0, SAMPLES);
            if verify_snapshot(&solver, target)? {
                return Ok(());
            }
        }
    }
    Err("profile did not meet the full model target".into())
}
