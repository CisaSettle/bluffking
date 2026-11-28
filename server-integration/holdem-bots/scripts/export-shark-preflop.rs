//! Offline exporter for GTOpen 874f780186fbe2291ea9e319ef191151da875644.
//! Clone https://github.com/MatthewPDingle/GTOpen, check out that commit,
//! and copy this file to
//! crates/solver/examples/export_shark.rs, then run from its root:
//!   RAYON_NUM_THREADS=4 cargo run --locked -p solver --example export_shark -- 100 /tmp/hu100.json
//!   gzip -nc9 /tmp/hu100.json > <BluffKing>/server/src/bot/hu100.json.gz
//! The original HU depths are 25, 50, 75, 100, 150 and 200. Additional exact
//! whole and half-BB depths also include a 4 BB open in the same root solve.
//! For 3–4 players (and 5 with a short stack), compile the same example once with CARGO_PROFILE_DEV_OPT_LEVEL=3,
//! then run `export_shark train-ring 3 /tmp/ring3.preflop` and
//! `export_shark export-ring /tmp/ring3.preflop /tmp/ring3.json`. Compress with
//! the same gzip options. For an asymmetric game append the short player's
//! zero-based preflop action-order seat and stack BB to the training command,
//! e.g. "train-ring 3 /tmp/btn50.preflop 0 50". Unequal stacks are inputs to
//! the solver, including side pots and raises capped by the deepest caller.
//! The ring export independently checks every seat's gap.
//! See scripts/gtopen/pack-stack-policies.py reproduce --help to regenerate
//! the embedded exact-stack catalog; pass this example's unpatched binary,
//! the patched train_shark_ring binary and the upstream directory with cache/.
//! Each catalog member retains the original measured gaps and solve config.
//! The trained v3 network and 20k-sample class equity matrix are from that
//! revision's cache directory. Standard cash chip-EV, no rake/ante/ICM.
//! The reported bounds are for the complete abstract preflop model, including
//! its estimated flop leaves. Live lookups admit only supported paths.

use serde::Serialize;
use solver::preflop::{
    equity::{class_index, class_parts, class_prob, EquityTable},
    PreflopConfig, PreflopSolver,
};
use std::{path::Path, sync::Arc};

#[derive(Clone, Copy, PartialEq, Eq, Serialize)]
enum Move {
    Fold,
    Check,
    Call,
    Raise(u16), // raise-to, half big blinds
}

#[derive(Serialize)]
struct Edge {
    play: Move,
    next: Option<usize>,
}

#[derive(Serialize)]
struct Node {
    actor: usize,
    posted: Vec<u16>,
    reach: Vec<f32>,
    edges: Vec<Edge>,
    strategy: Vec<f32>, // action-major, 169 classes per action
}

fn half_blinds(bb: f64) -> u16 {
    let value = (2.0 * bb) as u16;
    assert_eq!(f64::from(value), 2.0 * bb);
    value
}

// All suits are interchangeable preflop. Condition the opponent's path on
// each concrete hero class by counting its 1,225 compatible opponent hands.
fn compatible_counts() -> Vec<[u16; 169]> {
    (0..169)
        .map(|h| {
            let (hi, lo, suited) = class_parts(h);
            let hero = [4 * hi, 4 * lo + u8::from(!suited)];
            let mut counts = [0; 169];
            for a in 0..52 {
                for b in a + 1..52 {
                    if !hero.contains(&a) && !hero.contains(&b) {
                        counts[class_index(a / 4, b / 4, a % 4 == b % 4)] += 1;
                    }
                }
            }
            assert_eq!(counts.iter().sum::<u16>(), 1225);
            counts
        })
        .collect()
}

// Lower bound on the joint public path probability given hero's exact class.
// Previous opponents can remove at most two cards apiece. Among the 1,225
// available pairs, bound the mass they remove by BOTH the number of lost
// pairs times the largest weight and the largest blocked-card weight sums.
// For the first opponent this is exact. Starting with the tightest range
// avoids spending that range's blockers on players who have not acted.
fn joint_reach(ranges: &[Vec<f32>], actor: usize) -> Vec<f32> {
    (0..169)
        .map(|class| {
            let (hi, lo, suited) = class_parts(class);
            let hero = [4 * hi, 4 * lo + u8::from(!suited)];
            let mut summaries = Vec::new();
            for (i, range) in ranges.iter().enumerate() {
                if i == actor {
                    continue;
                }
                let mut sum = 0.0;
                let mut card_mass = [0.0; 52];
                let mut largest = 0.0_f64;
                let mut smallest = 1.0_f64;
                for a in 0..52u8 {
                    if hero.contains(&a) {
                        continue;
                    }
                    for b in a + 1..52 {
                        if hero.contains(&b) {
                            continue;
                        }
                        let weight = f64::from(range[class_index(a / 4, b / 4, a % 4 == b % 4)]);
                        sum += weight;
                        card_mass[a as usize] += weight;
                        card_mass[b as usize] += weight;
                        largest = largest.max(weight);
                        smallest = smallest.min(weight);
                    }
                }
                card_mass.sort_unstable_by(|a, b| b.total_cmp(a));
                summaries.push((sum, card_mass, largest, smallest));
            }
            summaries.sort_unstable_by(|a, b| a.0.total_cmp(&b.0));
            let mut p = f64::from(ranges[actor][class]);
            for (prior, (sum, card_mass, largest, smallest)) in summaries.iter().enumerate() {
                let left = 50 - 2 * prior;
                let pairs = (left * (left - 1) / 2) as f64;
                let loss = ((1225.0 - pairs) * largest).min(card_mass[..2 * prior].iter().sum());
                p *= ((sum - loss) / pairs).clamp(*smallest, 1.0);
            }
            p as f32
        })
        .collect()
}

fn export(
    s: &PreflopSolver,
    path: &mut Vec<usize>,
    nodes: &mut Vec<Node>,
    prior: &[Vec<f32>],
    counts: &[[u16; 169]],
) -> Result<Option<usize>, String> {
    let v = s.node_view(path)?;
    if v.kind != "action" || v.branch_probability < 0.001 || v.strategy_note.is_some() {
        return Ok(None);
    }
    let actor = v.actor.unwrap();
    let own = v.reach.unwrap();
    let strategy = v.strategy.unwrap();
    let reach = if s.n == 2 {
        own.iter()
            .zip(counts)
            .map(|(&actor_reach, row)| {
                let compatible: f64 = row
                    .iter()
                    .zip(&prior[1 - actor])
                    .map(|(&count, &prob)| f64::from(count) * f64::from(prob))
                    .sum();
                (f64::from(actor_reach) * compatible / 1225.0) as f32
            })
            .collect()
    } else {
        joint_reach(prior, actor)
    };
    let mut edges = Vec::new();
    for (i, action) in v.actions.iter().enumerate() {
        let play = match action.kind.as_str() {
            "fold" => Move::Fold,
            "check" => Move::Check,
            "call" => Move::Call,
            "raise" | "jam" => Move::Raise(half_blinds(action.to)),
            _ => return Err(format!("Unknown action {:?}", action.kind)),
        };
        path.push(i);
        let mut next_ranges = prior.to_vec();
        next_ranges[actor] = own
            .iter()
            .zip(&strategy[i * 169..])
            .map(|(&w, &p)| w * p)
            .collect();
        let next = export(s, path, nodes, &next_ranges, counts)?;
        path.pop();
        edges.push(Edge { play, next });
    }
    nodes.push(Node {
        actor: v.actor.unwrap(),
        posted: v.invested.iter().map(|&x| half_blinds(x)).collect(),
        reach,
        edges,
        strategy,
    });
    Ok(Some(nodes.len() - 1))
}

/// A closing call can contribute a modeled range after it was observed,
/// without publishing a strategy for choosing an action at its rare node.
#[derive(Serialize)]
struct HandoffCall {
    parent: usize,
    edge: usize,
    actor: usize,
    weights: Vec<f32>,
    source_path: Vec<usize>,
    public_probability: f64,
    terminal_probability: f64,
    call_probability: f64,
    node_gap_bb: f64,
    call_gap_bb: f64,
}

fn audited_closing_calls(
    s: &PreflopSolver,
    root: usize,
    nodes: &[Node],
) -> Result<Vec<HandoffCall>, String> {
    use Move::{Call, Fold, Raise};
    if s.n != 5
        || s.cfg.stacks.is_some()
        || s.cfg.stack != 100.0
        || s.cfg.open_raises != [2.0, 2.5, 3.0]
    {
        return Ok(Vec::new());
    }
    let mut out = Vec::new();
    for actions in [[Fold, Raise(6), Call, Call], [Raise(5), Fold, Call, Call]] {
        let mut path = Vec::new();
        let mut next = Some(root);
        let mut parent = root;
        let mut edge = 0;
        for action in actions {
            parent = next.ok_or("missing admitted path to the SB response")?;
            edge = nodes[parent]
                .edges
                .iter()
                .position(|e| e.play == action)
                .ok_or("missing source action")?;
            next = nodes[parent].edges[edge].next;
            path.push(edge);
        }
        // If this is already a regular decision, its normal exported weights
        // cover the handoff. Never attach a second row to that path.
        if next.is_some() {
            continue;
        }
        let v = s.node_view(&path)?;
        if v.kind != "action"
            || v.actor != Some(4)
            || v.low_reach
            || v.strategy_note.is_some()
            || !(0.00035..0.001).contains(&v.branch_probability)
        {
            return Err("four-live BB node failed its public reach gate".into());
        }
        let call = v
            .actions
            .iter()
            .position(|a| a.kind == "call")
            .ok_or("no BB call")?;
        let mut terminal_path = path.clone();
        terminal_path.push(call);
        let terminal = s.node_view(&terminal_path)?;
        if terminal.kind != "pot_share"
            || terminal.live.iter().filter(|&&l| l).count() != 4
            || !terminal.side_pots.is_empty()
            || terminal.branch_probability < 0.0002
            || !terminal.branch_probability.is_finite()
            || terminal
                .live
                .iter()
                .zip(&terminal.invested)
                .any(|(&live, &paid)| live && (paid >= s.cfg.stack || paid * 4.0 != terminal.pot))
        {
            return Err("expected a sufficiently reached four-live flop with equal stakes".into());
        }

        // Same native action tree, with the BB's subsequent play frozen
        // versus optimized. Both traversals hold every other player fixed.
        let saved = s.diagnostic_action_evs(&path, false)?;
        let br = s.diagnostic_action_evs(&path, true)?;
        let strategy = v.strategy.ok_or("missing native policy")?;
        let own = v.reach.ok_or("missing BB reach")?;
        let mut weights = Vec::with_capacity(169);
        let (mut mass, mut calls, mut node_loss, mut call_loss) = (0.0, 0.0, 0.0, 0.0);
        for h in 0..169 {
            let row = |a: usize| f64::from(strategy[a * 169 + h]);
            let sum: f64 = (0..v.actions.len()).map(row).sum();
            let best = br.iter().map(|a| a[h]).fold(f64::NEG_INFINITY, f64::max);
            if (sum - 1.0).abs() > 1e-5
                || !sum.is_finite()
                || !best.is_finite()
                || br.iter().chain(&saved).any(|a| !a[h].is_finite())
            {
                return Err("invalid native action diagnostic".into());
            }
            let value: f64 = saved
                .iter()
                .enumerate()
                .map(|(a, v)| row(a) * v[h] / sum)
                .sum();
            let prior = f64::from(class_prob(h)) * f64::from(own[h]);
            let p = row(call) / sum;
            // Check the row a second way against the native reached flop range.
            if !(0.0..=1.0).contains(&p)
                || (f64::from(terminal.reaches_all[4][h]) - p * f64::from(own[h])).abs() > 1e-6
            {
                return Err("invalid native call weight".into());
            }
            weights.push(p as f32);
            mass += prior;
            calls += prior * p;
            node_loss += prior * (best - value);
            call_loss += prior * p * (best - saved[call][h]);
        }
        let node_gap_bb = node_loss / mass;
        let call_gap_bb = call_loss / calls;
        let call_probability = calls / mass;
        if !(0.0..=0.0025 * v.pot).contains(&node_gap_bb)
            || !(0.0..=0.01_f64.min(0.0025 * v.pot)).contains(&call_gap_bb)
            || (call_probability * v.branch_probability - terminal.branch_probability).abs() > 1e-8
        {
            return Err("observed call failed conditional local checks".into());
        }
        out.push(HandoffCall {
            parent,
            edge,
            actor: 4,
            weights,
            source_path: path,
            public_probability: v.branch_probability,
            terminal_probability: terminal.branch_probability,
            call_probability,
            node_gap_bb,
            call_gap_bb,
        });
    }
    Ok(out)
}

fn train_ring(n: usize, saved: &str, short: Option<(usize, f64)>) -> Result<(), String> {
    if !(3..=5).contains(&n) || (n == 5 && short.is_none()) {
        return Err("use direct training for 3–4 players and 5 with one short stack".into());
    }
    let names = ["HJ", "CO", "BTN", "SB", "BB"];
    let mut posts = vec![0.0; n];
    posts[n - 2] = 0.5;
    posts[n - 1] = 1.0;
    let mut cfg: PreflopConfig = serde_json::from_value(serde_json::json!({
      "positions":&names[5-n..],"posts":posts,"stack":100,"limp":n==3,"open_raises":[2.5],
      "raise_mults":[3.0],"fourbet_mults":[2.0],"max_raises":if n==3 {4} else {3},
      "add_allin":true, "allin_threshold":0.8,"rake_pct":0.0,"card_removal":n!=5,
      "realization":"leafnet","leaf_net":"v3"
    }))
    .map_err(|e| e.to_string())?;
    if n == 3 || (n == 4 && short.is_none()) {
        cfg.open_raises = vec![2.0, 2.5, 3.0];
    }
    if let Some((seat, depth)) = short {
        if seat >= n || !(10.0..100.0).contains(&depth) || (2.0 * depth).fract() != 0.0 {
            return Err(
                "short stack must name a dealt seat and have 10–99.5 BB in half-blinds".into(),
            );
        }
        let mut stacks = vec![cfg.stack; n];
        stacks[seat] = depth;
        cfg.stacks = Some(stacks);
        cfg.max_raises = if n == 3 { 3 } else { 2 };
    }
    let equity = Arc::new(EquityTable::load_or_build("cache/preflop_eq169.bin", 20000));
    let mut solver = PreflopSolver::new(cfg, equity.clone())?;
    assert_eq!(solver.multiway_equity_model(), "coupled_deck_v1");
    assert!(solver.fit.is_some() && solver.refresh_leaf_values().is_some_and(|r| r.priced > 0));
    for i in 1..=20000 {
        solver.iterate();
        if matches!(i, 1 | 8 | 32 | 100) || i % 250 == 0 {
            solver.save_game(saved)?;
            let post = PreflopSolver::load_game(saved, equity.clone())?;
            let (gaps, _) = post.gaps_and_evs();
            eprintln!("{n} seats at {i} iterations: {gaps:?} BB");
            if gaps
                .iter()
                .all(|g| g.is_finite() && (0.0..=0.01).contains(g))
            {
                return Ok(());
            }
        }
    }
    Err("ring model did not meet target".into())
}

fn main() -> Result<(), String> {
    let mut args = std::env::args().skip(1);
    if std::env::vars().any(|(k, _)| k.starts_with("PREFLOP_")) {
        return Err("Unset PREFLOP_* research overrides before exporting".into());
    }
    let first = args
        .next()
        .ok_or("provide stack in BB, train-ring or export-ring")?;
    if first == "train-ring" {
        let n = args
            .next()
            .ok_or("provide number of players")?
            .parse()
            .map_err(|_| "invalid seat count")?;
        let saved = args.next().ok_or("provide snapshot output")?;
        let short = match (args.next(), args.next()) {
            (Some(seat), Some(depth)) => Some((
                seat.parse().map_err(|_| "invalid short seat")?,
                depth.parse().map_err(|_| "invalid short depth")?,
            )),
            (None, None) => None,
            _ => return Err("supply both short seat (zero-based) and its stack in BB".into()),
        };
        return train_ring(n, &saved, short);
    }
    if matches!(first.as_str(), "export-ring" | "export-projection") {
        let saved = args.next().ok_or("provide a GTOpen snapshot")?;
        let output = args.next().ok_or("provide output path")?;
        let equity = Arc::new(EquityTable::load_or_build("cache/preflop_eq169.bin", 20000));
        let post = PreflopSolver::load_game(&saved, equity)?;
        assert!((3..=9).contains(&post.n) && !post.has_overrides());
        assert_eq!(post.multiway_equity_model(), "coupled_deck_v1");
        let projection = (first == "export-projection").then_some("equal_100bb_public_actions_v1");
        if projection.is_some() {
            assert!(post.iteration == 1 && post.cfg.stacks.is_some());
        }
        let (gaps, evs) = post.gaps_and_evs();
        eprintln!("unrestricted frozen-profile BR: {gaps:?}");
        assert!(gaps
            .iter()
            .all(|g| g.is_finite() && (0.0..=0.01).contains(g)));
        let mut nodes = Vec::new();
        let root = export(
            &post,
            &mut Vec::new(),
            &mut nodes,
            &vec![vec![1.0; 169]; post.n],
            &[],
        )?
        .ok_or("missing root")?;
        #[derive(Serialize)]
        struct RingOutput<'a> {
            config: &'a PreflopConfig,
            model: &'a str,
            #[serde(skip_serializing_if = "Option::is_none")]
            projection: Option<&'a str>,
            #[serde(skip_serializing_if = "Vec::is_empty")]
            handoff_calls: Vec<HandoffCall>,
            iterations: u32,
            model_br_gap_bb: Vec<f64>,
            model_evs_bb: Vec<f64>,
            root: usize,
            nodes: Vec<Node>,
        }
        eprintln!(
            "{} seats: {} exported nodes, gaps {gaps:?}",
            post.n,
            nodes.len()
        );
        let data = RingOutput {
            config: &post.cfg,
            model: post.multiway_equity_model(),
            projection,
            handoff_calls: audited_closing_calls(&post, root, &nodes)?,
            iterations: post.iteration,
            model_br_gap_bb: gaps,
            model_evs_bb: evs,
            root,
            nodes,
        };
        return std::fs::write(output, serde_json::to_vec(&data).unwrap())
            .map_err(|e| e.to_string());
    }
    let depth: f64 = first.parse().map_err(|_| "invalid stack in BB")?;
    let output = args.next().ok_or("provide an output path")?;
    assert!((10.0..=200.0).contains(&depth) && (2.0 * depth).fract() == 0.0);
    let cfg: PreflopConfig = serde_json::from_value(serde_json::json!({
        "positions":["SB","BB"], "stack":depth, "posts":[0.5,1.0],
        "limp":true, "open_raises":if [25.0, 50.0, 75.0, 100.0, 150.0, 200.0].contains(&depth) {vec![2.0,2.5,3.0]} else {vec![2.0,2.5,3.0,4.0]}, "raise_mults":[3.0],
        "fourbet_mults":[2.0], "max_raises":8, "add_allin":true, "allin_threshold":0.8,
        "rake_pct":0.0, "card_removal":true, "realization":"leafnet", "leaf_net":"v3"
    }))
    .map_err(|e| e.to_string())?;
    let equity = Arc::new(EquityTable::load_or_build("cache/preflop_eq169.bin", 20000));
    let mut solver = PreflopSolver::new(cfg.clone(), equity.clone())?;
    assert!(solver.fit.is_some());
    assert!(solver.refresh_leaf_values().is_some_and(|r| r.priced > 0));
    let saved = Path::new(&output).with_extension("preflop");
    for iteration in 1..=20_000 {
        solver.iterate();
        if iteration % 250 != 0 {
            continue;
        }
        // Rebuild learned flop values at the final, frozen average ranges.
        // The post-pass is an unrestricted best response by each seat.
        solver.save_game(saved.to_str().unwrap())?;
        let post = PreflopSolver::load_game(saved.to_str().unwrap(), equity.clone())?;
        assert!(!post.has_overrides());
        let (gaps, evs) = post.gaps_and_evs();
        eprintln!("iteration {iteration}, gaps {gaps:?}, EVs {evs:?}");
        if gaps
            .iter()
            .any(|&g| !g.is_finite() || !(0.0..=0.01).contains(&g))
        {
            continue;
        }
        let mut nodes = Vec::new();
        let root = export(
            &post,
            &mut Vec::new(),
            &mut nodes,
            &vec![vec![1.0; 169]; post.n],
            &compatible_counts(),
        )?
        .ok_or("missing root")?;
        #[derive(Serialize)]
        struct Output {
            config: PreflopConfig,
            iterations: u32,
            model_br_gap_bb: Vec<f64>,
            root: usize,
            nodes: Vec<Node>,
        }
        let file = std::fs::File::create(&output).map_err(|e| e.to_string())?;
        serde_json::to_writer(
            file,
            &Output {
                config: cfg,
                iterations: iteration,
                model_br_gap_bb: gaps,
                root,
                nodes,
            },
        )
        .map_err(|e| e.to_string())?;
        return Ok(());
    }
    Err("model did not meet the target".into())
}
