//! Combine repeated LLM-tier runs into one summary.
//!
//! The Claude RCA and the LLM judge both vary from run to run, so a single
//! run's accuracy is a sample, not a measurement. This reads every
//! `claude-run-*.json` written by `--engine claude --file-stem claude-run-N`
//! and reports per-run accuracy, the pooled mean and range, and how stable
//! each scenario's verdict is across runs.

use std::collections::BTreeMap;
use std::fmt::Write as _;
use std::path::{Path, PathBuf};

use anyhow::{bail, Context};
use serde_json::{json, Value};

use crate::score::{Ratio, RCA_ERROR_VERDICT};

pub const RUN_FILE_PREFIX: &str = "claude-run-";
pub const SUMMARY_STEM: &str = "scorecard-claude";

/// One run's file, parsed down to what the summary needs.
struct Run {
    name: String,
    engine: String,
    correct: Ratio,
    errors: usize,
    /// Scenario id → (title, verdict).
    verdicts: BTreeMap<String, (String, String)>,
}

fn load_runs(dir: &Path) -> anyhow::Result<Vec<Run>> {
    let mut paths: Vec<PathBuf> = std::fs::read_dir(dir)
        .with_context(|| format!("reading {}", dir.display()))?
        .filter_map(|e| e.ok().map(|e| e.path()))
        .filter(|p| {
            p.extension().and_then(|e| e.to_str()) == Some("json")
                && p.file_stem()
                    .and_then(|s| s.to_str())
                    .is_some_and(|s| s.starts_with(RUN_FILE_PREFIX))
        })
        .collect();
    paths.sort();
    if paths.is_empty() {
        bail!("no {RUN_FILE_PREFIX}*.json files in {}", dir.display());
    }
    paths
        .iter()
        .map(|p| {
            let text = std::fs::read_to_string(p)?;
            let doc: Value =
                serde_json::from_str(&text).with_context(|| format!("parsing {}", p.display()))?;
            let summary = &doc["summary"];
            let correct = Ratio {
                hits: summary["rca_correct"]["hits"].as_u64().unwrap_or(0) as usize,
                total: summary["rca_correct"]["total"].as_u64().unwrap_or(0) as usize,
            };
            let verdicts = doc["scenarios"]
                .as_array()
                .context("run file has no scenarios")?
                .iter()
                .map(|s| {
                    let id = s["id"].as_str().unwrap_or_default().to_string();
                    let title = s["title"].as_str().unwrap_or_default().to_string();
                    let verdict = s["rca"]["verdict"]
                        .as_str()
                        .unwrap_or("missing")
                        .to_string();
                    (id, (title, verdict))
                })
                .collect();
            Ok(Run {
                name: p
                    .file_stem()
                    .and_then(|s| s.to_str())
                    .unwrap_or_default()
                    .to_string(),
                engine: doc["engine"].as_str().unwrap_or_default().to_string(),
                correct,
                errors: summary["rca_errors"].as_u64().unwrap_or(0) as usize,
                verdicts,
            })
        })
        .collect()
}

/// Write `scorecard-claude.md` and `.json` summarizing every run in `dir`.
pub fn write_summary(dir: &Path) -> anyhow::Result<PathBuf> {
    let runs = load_runs(dir)?;
    let n = runs.len();
    let engines: Vec<&str> = runs.iter().map(|r| r.engine.as_str()).collect();
    if engines.windows(2).any(|w| w[0] != w[1]) {
        bail!("runs used different engines or models; summarize like with like: {engines:?}");
    }

    let pooled = Ratio {
        hits: runs.iter().map(|r| r.correct.hits).sum(),
        total: runs.iter().map(|r| r.correct.total).sum(),
    };
    let per_run_pct: Vec<f64> = runs.iter().filter_map(|r| r.correct.pct()).collect();
    let min = per_run_pct.iter().cloned().fold(f64::INFINITY, f64::min);
    let max = per_run_pct
        .iter()
        .cloned()
        .fold(f64::NEG_INFINITY, f64::max);

    // Scenario ids in the order of the first run, which is sorted by id.
    let ids: Vec<String> = runs[0].verdicts.keys().cloned().collect();
    let majority = n / 2 + 1;
    let mut rows = Vec::new();
    let (mut always, mut never, mut mixed, mut majority_correct) = (0, 0, 0, 0);
    for id in &ids {
        let title = runs[0].verdicts[id].0.clone();
        let verdicts: Vec<String> = runs
            .iter()
            .map(|r| {
                r.verdicts
                    .get(id)
                    .map(|v| v.1.clone())
                    .unwrap_or_else(|| "missing".to_string())
            })
            .collect();
        let graded: Vec<&String> = verdicts
            .iter()
            .filter(|v| v.as_str() != RCA_ERROR_VERDICT && v.as_str() != "missing")
            .collect();
        let correct = graded.iter().filter(|v| v.as_str() == "correct").count();
        if !graded.is_empty() && correct == graded.len() {
            always += 1;
        } else if correct == 0 {
            never += 1;
        } else {
            mixed += 1;
        }
        if correct >= majority {
            majority_correct += 1;
        }
        let graded_count = graded.len();
        let stable = verdicts.windows(2).all(|w| w[0] == w[1]);
        rows.push((id.clone(), title, verdicts, correct, graded_count, stable));
    }

    let mut md = String::new();
    let _ = writeln!(md, "# Claude RCA: Repeated Runs\n");
    let _ = writeln!(
        md,
        "Generated by `cargo run -p sre-evals -- --summarize-runs` from {n} runs of \
         `--engine claude`. Do not edit by hand. Engine: {}. Method: \
         [ADR-023](../adr/023-golden-set-evaluation.md). Per-run detail: {}.\n",
        runs[0].engine,
        runs.iter()
            .map(|r| format!("[{0}]({0}.md)", r.name))
            .collect::<Vec<_>>()
            .join(", ")
    );
    let _ = writeln!(
        md,
        "The Claude analysis and the LLM judge both vary between runs, so each run \
         is a sample. Every run replays the same deterministic findings; only the \
         LLM steps differ.\n"
    );

    let _ = writeln!(md, "## Accuracy\n");
    let _ = writeln!(md, "| Run | Judged correct | Failed calls |");
    let _ = writeln!(md, "|---|---|---|");
    for r in &runs {
        let _ = writeln!(
            md,
            "| {} | {} | {} |",
            r.name,
            r.correct.display(),
            r.errors
        );
    }
    let _ = writeln!(
        md,
        "| **All runs pooled** | **{}** | {} |\n",
        pooled.display(),
        runs.iter().map(|r| r.errors).sum::<usize>()
    );
    let _ = writeln!(md, "| Measure | Result |");
    let _ = writeln!(md, "|---|---|");
    let _ = writeln!(md, "| Range across runs | {min:.0}% to {max:.0}% |");
    let _ = writeln!(
        md,
        "| Correct in a majority of runs ({majority} or more of {n}) | {majority_correct} of {} |",
        ids.len()
    );
    let _ = writeln!(md, "| Correct in every run | {always} of {} |", ids.len());
    let _ = writeln!(md, "| Never correct | {never} of {} |", ids.len());
    let _ = writeln!(md, "| Mixed verdicts | {mixed} of {} |\n", ids.len());

    let _ = writeln!(md, "## Per scenario\n");
    let header: Vec<String> = runs.iter().map(|r| r.name.clone()).collect();
    let _ = writeln!(
        md,
        "| ID | Incident | {} | Correct in |",
        header.join(" | ")
    );
    let _ = writeln!(md, "|---|---|{}---|", "---|".repeat(n));
    for (id, title, verdicts, correct, graded, stable) in &rows {
        let cells: Vec<String> = verdicts
            .iter()
            .map(|v| {
                if v == "correct" {
                    v.clone()
                } else {
                    format!("**{v}**")
                }
            })
            .collect();
        let note = if *stable { "" } else { " (unstable)" };
        let _ = writeln!(
            md,
            "| {id} | {} | {} | {correct} of {graded}{note} |",
            title.replace('|', "\\|"),
            cells.join(" | ")
        );
    }

    let md_path = dir.join(format!("{SUMMARY_STEM}.md"));
    std::fs::write(&md_path, md)?;
    let summary_json = json!({
        "runs": runs.iter().map(|r| json!({
            "name": r.name,
            "correct": r.correct,
            "errors": r.errors,
        })).collect::<Vec<_>>(),
        "engine": runs[0].engine,
        "pooled": pooled,
        "range_pct": [min, max],
        "majority_correct": majority_correct,
        "always_correct": always,
        "never_correct": never,
        "mixed": mixed,
        "scenarios": rows.iter().map(|(id, title, verdicts, correct, graded, stable)| json!({
            "id": id,
            "title": title,
            "verdicts": verdicts,
            "correct": correct,
            "graded": graded,
            "stable": stable,
        })).collect::<Vec<_>>(),
    });
    std::fs::write(
        dir.join(format!("{SUMMARY_STEM}.json")),
        serde_json::to_string_pretty(&summary_json)? + "\n",
    )?;
    Ok(md_path)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn run_file(dir: &Path, name: &str, verdicts: &[(&str, &str)]) {
        let hits = verdicts.iter().filter(|(_, v)| *v == "correct").count();
        let graded = verdicts.iter().filter(|(_, v)| *v != "error").count();
        let doc = json!({
            "engine": "test-engine",
            "summary": {
                "rca_correct": {"hits": hits, "total": graded},
                "rca_errors": verdicts.len() - graded,
            },
            "scenarios": verdicts.iter().map(|(id, v)| json!({
                "id": id, "title": format!("title {id}"), "rca": {"verdict": v}
            })).collect::<Vec<_>>(),
        });
        std::fs::write(dir.join(format!("{name}.json")), doc.to_string()).unwrap();
    }

    #[test]
    fn summary_pools_runs_and_flags_unstable_verdicts() {
        let dir = std::env::temp_dir().join(format!("sre-evals-runs-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        run_file(&dir, "claude-run-1", &[("A", "correct"), ("B", "partial")]);
        run_file(&dir, "claude-run-2", &[("A", "correct"), ("B", "correct")]);
        run_file(&dir, "claude-run-3", &[("A", "correct"), ("B", "error")]);
        write_summary(&dir).unwrap();

        let summary: Value = serde_json::from_str(
            &std::fs::read_to_string(dir.join("scorecard-claude.json")).unwrap(),
        )
        .unwrap();
        // Pooled: A correct 3 times, B correct once out of 2 graded = 4 of 5.
        assert_eq!(summary["pooled"]["hits"], 4);
        assert_eq!(summary["pooled"]["total"], 5);
        assert_eq!(summary["always_correct"], 1);
        assert_eq!(summary["mixed"], 1);
        // B: correct in 1 of 3 runs, below the majority of 2.
        assert_eq!(summary["majority_correct"], 1);
        assert_eq!(summary["scenarios"][1]["stable"], false);
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn runs_with_different_engines_are_refused() {
        let dir = std::env::temp_dir().join(format!("sre-evals-runs-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        run_file(&dir, "claude-run-1", &[("A", "correct")]);
        let other = json!({"engine": "other", "summary": {}, "scenarios": []});
        std::fs::write(dir.join("claude-run-2.json"), other.to_string()).unwrap();
        assert!(write_summary(&dir).is_err());
        std::fs::remove_dir_all(&dir).ok();
    }
}
