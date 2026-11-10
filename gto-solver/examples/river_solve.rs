//! Offline bounded river solving and teacher-data generation. See
//! docs/architecture/poker-multiway-river.md for the JSON contract.
use engine::HoleCards;
use gto_solver::{river::*, SolveControl};
use serde::Deserialize;
use serde_json::{json, Value};
use std::{
    io::Read,
    time::{Duration, Instant},
};

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Job {
    request: RiverRequest,
    #[serde(default)]
    queries: Vec<Query>,
    exploit: Option<ExploitJob>,
    #[serde(default = "deadline")]
    deadline_millis: u64,
}
fn deadline() -> u64 {
    30_000
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Query {
    path: Vec<RiverStep>,
    hand: HoleCards,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct ExploitJob {
    hero: usize,
    locks: Vec<OpponentLock>,
    limits: ExploitLimits,
}

fn run() -> Result<(Value, bool), Box<dyn std::error::Error>> {
    let path = std::env::args()
        .nth(1)
        .ok_or("usage: river_solve JOB.json")?;
    let mut bytes = Vec::new();
    std::fs::File::open(path)?
        .take(8_000_001)
        .read_to_end(&mut bytes)?;
    if bytes.len() > 8_000_000 {
        return Err("job exceeds 8 MB".into());
    }
    let job: Job = serde_json::from_slice(&bytes)?;
    if job.deadline_millis == 0
        || job.deadline_millis > 300_000
        || job.queries.len() > 1_000
        || job
            .queries
            .iter()
            .any(|q| q.path.len() > job.request.limits.max_depth)
    {
        return Err("invalid deadline or query budget".into());
    }
    let control = SolveControl {
        deadline: Some(Instant::now() + Duration::from_millis(job.deadline_millis)),
        ..Default::default()
    };
    let solution = RiverSolution::solve(job.request, &control)?;
    let mut output = json!({
        "schema": "bluffking.river-teacher.v1",
        "scope": "specified river ranges and action tree; finite-precision unilateral deviation check",
        "request": solution.request(), "report": solution.report()
    });
    if !solution.report().admitted {
        return Ok((output, false));
    }
    output["teacher_values"] = serde_json::to_value(solution.teacher_values(&control)?)?;
    output["decisions"] = serde_json::to_value(
        job.queries
            .iter()
            .map(|q| solution.decision(&q.path, q.hand))
            .collect::<Result<Vec<_>, _>>()?,
    )?;
    if let Some(exploit) = job.exploit {
        let candidate =
            solution.exploit(exploit.hero, &exploit.locks, &exploit.limits, &control)?;
        output["exploit"] = serde_json::to_value(candidate.report())?;
        let mut decisions = Vec::new();
        for query in &job.queries {
            if solution.decision(&query.path, query.hand)?.player == exploit.hero {
                decisions.push(candidate.decision(&query.path, query.hand)?);
            }
        }
        output["exploit_decisions"] = serde_json::to_value(decisions)?;
    }
    Ok((output, true))
}

fn main() {
    match run() {
        Ok((value, admitted)) => {
            println!("{value}");
            if !admitted {
                std::process::exit(2);
            }
        }
        Err(error) => {
            eprintln!("{}", json!({"admitted": false, "error": error.to_string()}));
            std::process::exit(1);
        }
    }
}
