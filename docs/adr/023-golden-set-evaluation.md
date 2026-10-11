# ADR-023: Golden-Set Evaluation of Investigation Quality

**Status:** Accepted (2026-10-11)

## Context

The platform has 300+ unit and integration tests, and every one of them checks
that a component behaves as coded: a KQL template contains a time filter, an
agent turns 120 exceptions into a Critical finding, a state transition is legal.
None of them checks whether an investigation reaches the **right answer**. Nothing
measures whether the platform names the true root cause of an incident, whether it
pages a human when it should, or whether the remediation it proposes would fix the
problem. Those three outcomes are what the platform exists for, and they were
unmeasured.

The answer to an incident depends on several agents working together, so a unit
test cannot see it. A deploy that causes an exception storm produces a Critical
`ErrorSpike` finding from the logs agent and a Medium `DeploymentCorrelation`
finding from the infra agent. Each finding is individually correct. Whether the
report ranks the deploy or the exceptions first is a property of the whole
pipeline.

## Decision

Add a **golden-set evaluation**: a small, hand-labeled set of realistic incidents
replayed through the real investigation pipeline and scored against ground truth.

1. **Real pipeline, fixture telemetry.** Each scenario runs the production
   `Orchestrator::investigate` with all five agents (triage, logs, metrics, trace,
   infra) and the real `AzureQueryClient`. Only the Azure HTTP endpoint is
   replaced: a local fixture server identifies which KQL template each request
   came from and returns that scenario's fixture rows. Agent logic, thresholds,
   aggregation, ranking and escalation all run unmodified. The fixture rows are
   hand-authored to match the columns and magnitudes Azure returns for each
   template; they are not captured from production. Replacing them with
   sanitized captures from real incidents is a follow-up.
2. **Labels describe the incident, not the code.** Each scenario states the true
   root cause, the signals that are genuinely present, whether a human must be
   paged, and which first remediation actions an SRE would accept. Labels are
   written from the incident narrative before the pipeline is run against it.
   Equivalent categories within one causal family (for example a config change
   reported as a deployment correlation) are accepted explicitly per scenario,
   using `a|b` syntax. A symptom category is never accepted for a cause.
3. **Order-independent scoring.** Agents reply asynchronously, so findings that
   tie on severity arrive in a nondeterministic order. The scorer never gives
   credit for a tie: if the top severity tier holds more than one category, the
   root-cause call counts as a miss and is reported as ambiguous. Remediation ties
   on confidence among the proposals the proposer returns are handled the same
   way. (The proposer already keeps one proposal per remediation category before
   the scorer sees them, so a tie inside one category cannot be observed.) Scores
   are therefore reproducible run to run. A run in which any agent the
   orchestrator tasked did not reply is rejected rather than scored, so a
   pipeline failure can never pass as "claimed nothing and paged a human".
4. **Metrics.** Root-cause top-1 accuracy, root-cause recall among detections
   (separates "never found it" from "found it but ranked it below a symptom"),
   per-category detection precision and recall, escalation accuracy with missed
   pages counted separately, first-choice remediation accuracy with unwarranted
   actions counted separately, and a list of fixture telemetry the agents never
   queried (coverage gaps).
5. **Ratchet gate in CI.** `crates/evals/baseline.json` records which checks pass
   per scenario. `cargo test --workspace` fails if a previously passing check
   regresses. New passes are allowed and reported, so the baseline only moves up.
6. **Optional LLM tier.** With `--engine claude` and `ANTHROPIC_API_KEY` set, the
   Claude reasoning engine synthesizes a root-cause analysis from each scenario's
   findings and an LLM judge grades it against the label. This tier costs money
   and is not deterministic, so it is opt-in and never part of the CI gate.

## Consequences

- Investigation quality becomes a number that can move. Ranking, threshold and
  playbook changes can be judged by their effect on the scorecard instead of by
  argument.
- The first run measures the platform as it stands. Low scores are findings
  about the platform, not defects in the evaluation. The generated scorecard is
  `docs/evals/scorecard.md`; the analysis of what the misses mean is
  `docs/evals/golden-set-report.md`.
- **One labeler.** Every label was written by one author. A second SRE should
  review the labels before the numbers are used to make decisions.
- **Overfitting risk.** Tuning the pipeline against the same 14 scenarios that
  score it inflates the numbers. Mitigations: labels are fixed before tuning,
  any change made to improve a score must be justified by the incident narrative,
  and new scenarios should be added for each change. A held-out set is a
  follow-up once the set is large enough to split.
- **Small sample.** Fourteen scenarios give coarse percentages: one scenario is
  about seven percentage points. The scorecard reports counts alongside
  percentages for that reason.
- The fixture server keys on a template-specific expression in each KQL
  template, never a bare table name. Unit tests assert that every template maps
  to exactly one fixture key and that `kql_templates.rs` has as many templates
  as there are keys, so editing a template's marker or adding a template
  without a key fails the build instead of silently returning the wrong rows.
  A new template that happens to contain another template's marker text would
  still be misrouted; reviewers should check markers when adding templates.
