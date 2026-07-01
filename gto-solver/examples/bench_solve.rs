//! Measure real per-solve compute cost on this machine for representative
//! public-tier spots. Run with:
//!   cargo run -p gto-solver --release --example bench_solve
//!
//! NOT a test (it allocates >1 GB for the flop case); a one-shot measurement so
//! the integration report quotes real numbers, not the design's probe numbers.

use std::time::Instant;

use engine::card::{parse_card, Card};
use engine::hand::HoleCards;
use gto_solver::{solve_spot, Player, SolveLimits, SolveRequest, SolveStreet};

fn flop(a: &str, b: &str, c: &str) -> [Card; 3] {
    [
        parse_card(a).unwrap(),
        parse_card(b).unwrap(),
        parse_card(c).unwrap(),
    ]
}

fn run(label: &str, req: &SolveRequest) {
    let t0 = Instant::now();
    match solve_spot(req) {
        Ok(out) => {
            let dt = t0.elapsed();
            println!(
                "{label:<34} mem={:>6.0} MB  time={:>6.2} s  exploit={:.4} ({:.3}% pot)  actions={}",
                out.cost.memory_bytes as f64 / 1.0e6,
                dt.as_secs_f64(),
                out.exploitability,
                out.exploitability_pct_of_pot,
                out.actions.len(),
            );
        }
        Err(e) => println!("{label:<34} ERROR: {e}"),
    }
}

fn main() {
    // Wide 6-max-ish ranges, the realistic public-tier inputs.
    let oop = "66+,A8s+,A5s-A4s,AJo+,K9s+,KQo,QTs+,JTs,96s+,85s+,75s+,65s,54s";
    let ip = "QQ-22,AQs-A2s,ATo+,K5s+,KJo+,Q8s+,J8s+,T7s+,96s+,86s+,75s+,64s+,53s+";

    let base = SolveRequest {
        street: SolveStreet::Flop,
        flop: flop("Td", "9d", "6h"),
        turn: None,
        river: None,
        oop_range: oop.into(),
        ip_range: ip.into(),
        starting_pot: 60,
        effective_stack: 100,
        bet_sizes: "50%".into(),
        raise_sizes: "2.5x".into(),
        hero: Some(HoleCards::new(
            parse_card("Ad").unwrap(),
            parse_card("Kd").unwrap(),
        )),
        solving_player: Player::Oop,
        // Lift the memory cap for the measurement so the wide flop can run.
        limits: SolveLimits {
            max_memory_bytes: 4_000_000_000,
            max_iterations: 1000,
            target_exploitability_pct_of_pot: 0.005,
        },
    };

    run("FLOP wide-range 1 bet size", &base);

    let mut turn = base.clone();
    turn.street = SolveStreet::Turn;
    turn.turn = Some(parse_card("Qc").unwrap());
    run("TURN wide-range 1 bet size", &turn);

    let mut river = base.clone();
    river.street = SolveStreet::River;
    river.turn = Some(parse_card("Qc").unwrap());
    river.river = Some(parse_card("2s").unwrap());
    run("RIVER wide-range 1 bet size", &river);
}
