//! CI regression gate for investigation quality (ADR-023).
//!
//! Replays the golden set and fails if any check that passes in
//! `crates/evals/baseline.json` now fails. Newly passing checks are allowed and
//! printed; lock them in with `cargo run -p sre-evals -- --write-baseline`.

use sre_evals::score::{compare, Baseline};
use sre_evals::{default_baseline_path, default_golden_dir, evaluate, load_dir, summarize};

#[tokio::test]
async fn golden_set_does_not_regress() {
    let scenarios = load_dir(&default_golden_dir()).expect("golden set loads and validates");
    let evaluated = evaluate(scenarios).await.expect("every scenario runs");
    let scores: Vec<_> = evaluated.iter().map(|e| e.score.clone()).collect();

    for s in &scores {
        assert_eq!(
            s.unrecognized_queries, 0,
            "{}: an agent sent KQL the fixture server could not route; update fixture::TEMPLATE_KEYS",
            s.id
        );
    }

    let text = std::fs::read_to_string(default_baseline_path()).expect("baseline.json exists");
    let baseline: Baseline = serde_json::from_str(&text).expect("baseline.json parses");
    let (regressions, improvements) = compare(&baseline, &scores);

    let summary = summarize(&scores);
    println!(
        "golden set: root cause {} | escalation {} | remediation {} | all checks {}",
        summary.root_cause_top1.display(),
        summary.escalation.display(),
        summary.remediation.display(),
        summary.all_checks.display(),
    );
    for i in &improvements {
        println!("IMPROVED  {i}");
    }
    assert!(
        regressions.is_empty(),
        "investigation quality regressed against crates/evals/baseline.json:\n  {}",
        regressions.join("\n  ")
    );
}
