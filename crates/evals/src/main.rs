//! `sre-evals` — run the golden set and write the scorecard.
//!
//! ```text
//! cargo run -p sre-evals                      # score, write docs/evals/scorecard.{md,json}
//! cargo run -p sre-evals -- --only GS-01      # one scenario
//! cargo run -p sre-evals -- --write-baseline  # accept current results as the CI ratchet
//! cargo run -p sre-evals -- --engine claude   # add the LLM tier (needs ANTHROPIC_API_KEY)
//! ```

use std::path::{Path, PathBuf};

use anyhow::Context;
use clap::{Parser, ValueEnum};

use sre_common::config::PlatformConfig;
use sre_evals::judge::{grade_all, Judge, DEFAULT_JUDGE_MODEL};
use sre_evals::score::{compare, to_baseline, Baseline};
use sre_evals::{default_baseline_path, default_golden_dir, evaluate, load_dir, report, summarize};
use sre_intelligence::ClaudeReasoningEngine;

#[derive(Clone, Copy, PartialEq, Eq, ValueEnum)]
enum Engine {
    /// The deterministic pipeline only. Free and reproducible.
    RuleBased,
    /// Also have Claude write an RCA per scenario and grade it with an LLM judge.
    Claude,
}

#[derive(Parser)]
#[command(about = "Golden-set evaluation of investigation quality (ADR-023)")]
struct Cli {
    /// Directory of scenario JSON files.
    #[arg(long)]
    golden: Option<PathBuf>,
    /// Directory to write the scorecard into.
    #[arg(long)]
    out: Option<PathBuf>,
    /// Run only scenarios whose id starts with this prefix.
    #[arg(long)]
    only: Option<String>,
    /// Overwrite the CI ratchet baseline with the current results.
    #[arg(long)]
    write_baseline: bool,
    /// Reasoning tier to evaluate.
    #[arg(long, value_enum, default_value = "rule-based")]
    engine: Engine,
    /// Model for the Claude RCA. Defaults to the platform's configured
    /// `reasoning.model`, so the eval measures what production would run.
    #[arg(long)]
    rca_model: Option<String>,
    /// Model for the LLM judge.
    #[arg(long, default_value = DEFAULT_JUDGE_MODEL)]
    judge_model: String,
    /// Override the Anthropic API base URL (testing).
    #[arg(long)]
    anthropic_base_url: Option<String>,
    /// Output file name without extension. Defaults to `scorecard` for the
    /// rule-based tier and `scorecard-claude` for the LLM tier; set it per run
    /// (for example `claude-run-2`) to keep repeated LLM runs side by side.
    #[arg(long)]
    file_stem: Option<String>,
    /// Instead of running scenarios, combine the LLM-tier run files
    /// (`claude-run-*.json` in the output directory) into `scorecard-claude.md`.
    #[arg(long)]
    summarize_runs: bool,
}

fn default_out_dir() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../../docs/evals")
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let cli = Cli::parse();
    if cli.summarize_runs {
        let out = cli.out.clone().unwrap_or_else(default_out_dir);
        let path = sre_evals::runs::write_summary(&out)?;
        eprintln!("wrote {}", path.display());
        return Ok(());
    }
    let golden = cli.golden.clone().unwrap_or_else(default_golden_dir);
    let mut scenarios = load_dir(&golden)?;
    if let Some(prefix) = &cli.only {
        scenarios.retain(|s| s.id.starts_with(prefix.as_str()));
        anyhow::ensure!(
            !scenarios.is_empty(),
            "no scenario id starts with '{prefix}'"
        );
    }
    let partial = cli.only.is_some();

    let mut evaluated = evaluate(scenarios).await?;

    let (engine_label, file_stem) = match cli.engine {
        Engine::RuleBased => ("rule-based (no LLM)".to_string(), "scorecard"),
        Engine::Claude => {
            let reasoning = PlatformConfig::default().reasoning;
            let key = std::env::var(&reasoning.api_key_env)
                .ok()
                .filter(|k| !k.is_empty())
                .with_context(|| format!("--engine claude needs {} set", reasoning.api_key_env))?;
            let rca_model = cli.rca_model.clone().unwrap_or(reasoning.model);
            let engine = ClaudeReasoningEngine::new(
                key.clone(),
                rca_model.clone(),
                reasoning.max_tokens,
                cli.anthropic_base_url.clone(),
            );
            let judge = Judge::new(key, cli.judge_model.clone(), cli.anthropic_base_url.clone());
            eprintln!(
                "LLM tier: {} scenarios, RCA by {rca_model}, judged by {}",
                evaluated.len(),
                cli.judge_model
            );
            grade_all(&mut evaluated, &engine, &judge).await;
            (
                format!(
                    "rule-based pipeline + Claude RCA ({rca_model}), judged by {}",
                    cli.judge_model
                ),
                "scorecard-claude",
            )
        }
    };

    let file_stem = cli
        .file_stem
        .clone()
        .unwrap_or_else(|| file_stem.to_string());
    let scores: Vec<_> = evaluated.iter().map(|e| e.score.clone()).collect();
    let summary = summarize(&scores);
    let markdown = report::render_markdown(&summary, &scores, &engine_label);
    println!("{markdown}");

    if !partial {
        let out = cli.out.clone().unwrap_or_else(default_out_dir);
        std::fs::create_dir_all(&out).with_context(|| format!("creating {}", out.display()))?;
        std::fs::write(out.join(format!("{file_stem}.md")), &markdown)?;
        let json =
            serde_json::json!({ "engine": engine_label, "summary": summary, "scenarios": scores });
        std::fs::write(
            out.join(format!("{file_stem}.json")),
            serde_json::to_string_pretty(&json)? + "\n",
        )?;
        eprintln!("wrote {}", out.join(format!("{file_stem}.md")).display());
    }

    let baseline_path = default_baseline_path();
    if cli.write_baseline {
        anyhow::ensure!(
            !partial,
            "--write-baseline needs the full golden set (drop --only)"
        );
        let baseline = to_baseline(&scores);
        std::fs::write(
            &baseline_path,
            serde_json::to_string_pretty(&baseline)? + "\n",
        )?;
        eprintln!("wrote {}", baseline_path.display());
    } else if let Ok(text) = std::fs::read_to_string(&baseline_path) {
        let mut baseline: Baseline = serde_json::from_str(&text)?;
        if partial {
            // Scenarios left out by --only are not regressions.
            baseline.retain(|id, _| scores.iter().any(|s| &s.id == id));
        }
        let (regressions, improvements) = compare(&baseline, &scores);
        for r in &regressions {
            eprintln!("REGRESSION  {r}");
        }
        for i in &improvements {
            eprintln!("IMPROVED    {i}");
        }
        if !improvements.is_empty() && regressions.is_empty() && !partial {
            eprintln!("run with --write-baseline to lock in the improvements");
        }
    }
    Ok(())
}
