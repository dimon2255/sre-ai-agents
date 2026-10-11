//! Golden-set evaluation of investigation quality (ADR-023).
//!
//! Replays hand-labeled incidents through the production orchestrator and all
//! five agents, with fixture telemetry served from a local stand-in for Azure, and
//! scores each report against ground truth: root cause, signals, escalation and
//! first-choice remediation.

pub mod fixture;
pub mod judge;
pub mod report;
pub mod runner;
pub mod runs;
pub mod scenario;
pub mod score;

use std::path::{Path, PathBuf};

pub use runner::{run_scenario, RunOutcome};
pub use scenario::{load_dir, Scenario};
pub use score::{score, summarize, ScenarioScore, Summary};

/// The committed golden set.
pub fn default_golden_dir() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("golden")
}

/// The committed CI ratchet baseline.
pub fn default_baseline_path() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("baseline.json")
}

/// One scenario's run and score, kept together for the optional LLM tier.
pub struct Evaluated {
    pub scenario: Scenario,
    pub outcome: RunOutcome,
    pub score: ScenarioScore,
}

/// Run and score every scenario. Scenarios run one at a time so each has the
/// machine to itself and timings are comparable.
pub async fn evaluate(scenarios: Vec<Scenario>) -> anyhow::Result<Vec<Evaluated>> {
    let mut out = Vec::with_capacity(scenarios.len());
    for scenario in scenarios {
        let outcome = run_scenario(&scenario).await?;
        let score = score(&scenario, &outcome);
        out.push(Evaluated {
            scenario,
            outcome,
            score,
        });
    }
    Ok(out)
}
