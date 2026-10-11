# Golden-Set Evaluation: Baseline Report

**Date:** 2026-10-11 · **Method:** [ADR-023](../adr/023-golden-set-evaluation.md) ·
**Full results:** [scorecard.md](scorecard.md) (generated) ·
**Run it:** `cargo run -p sre-evals`

## Overview

Until now the platform's tests checked that each component behaves as coded. None
checked whether an investigation reaches the right answer. This evaluation
replays 14 labeled incidents through the production investigation pipeline and
scores three outcomes: did the report lead with the true root cause, did the
platform page a human when it should have, and was the first remediation it
proposed one an SRE would accept.

The platform finds most of the right evidence but often draws the wrong
conclusion from it. It detects 88 percent of the real signals with 92 percent
precision, and in 9 of the 12 incidents that have a root cause, the cause is
among its findings. But the report leads with the true root cause in only 4 of
14 incidents (29 percent), because findings are ranked by severity and symptoms
such as error spikes carry higher severity than their causes. First-choice
remediation is right in 7 of 14 (50 percent), and in one incident, a traffic
blackhole, the platform marked a revenue-impacting outage resolved without
paging anyone.

These are baseline numbers for the platform as it stands. Every miss traces to a
specific, fixable behavior, listed below.

The platform's optional Claude reasoning layer does much better on the same
evidence. Given exactly the findings the rule-based report ranks, Claude's
root-cause analysis was judged correct in 10 of 14 incidents (71 percent) in
each of three runs, the same 10 every time. It does not change the automated
decisions, though: remediation proposals and paging are still made by the
rule-based code. See
[LLM tier](#llm-tier-claude-root-cause-analysis).

## Results

Fourteen scenarios. One scenario moves a percentage by about seven points, so
counts are shown alongside percentages.

| Outcome | Result | What it means |
|---|---|---|
| Report leads with the true root cause | 4 of 14 (29%) | A responder reading the report top-down is pointed at the wrong cause in most incidents. |
| True root cause found somewhere in the findings | 9 of 12 (75%) | The evidence is usually there; the ranking hides it. Excludes the two incidents that have no root cause. |
| Real signals detected (recall) | 22 of 25 (88%) | The agents' detection thresholds work for the signals they query. |
| Detections that are real (precision) | 22 of 24 (92%) | Both false detections are error spikes reported from a handful of routine failures. |
| Correct escalation decision | 13 of 14 (93%) | One missed page, described below. |
| First remediation proposal acceptable | 7 of 14 (50%) | Four proposals were for incidents where no automated action fits. |
| All four checks correct | 3 of 14 (21%) | |

## LLM tier: Claude root-cause analysis

When reasoning is enabled, the orchestrator sends the agents' findings to
Claude and puts Claude's root-cause analysis at the top of the report summary.
This tier replays that step for every scenario, using the same findings the
rule-based results above were scored on, and an LLM judge grades each analysis
against the label.

Run three times on 2026-10-11: analysis by `claude-opus-4-8` (the platform's
configured model), judged by `claude-opus-5-5`. 84 API calls, 0 failed.
Summary across runs: [scorecard-claude.md](scorecard-claude.md); each run:
[1](claude-run-1.md), [2](claude-run-2.md), [3](claude-run-3.md).

| Outcome | Rule-based ranking | Claude analysis |
|---|---|---|
| Names the true root cause | 4 of 14 (29%) | 10 of 14 (71%) in each of 3 runs |
| Rule-based misses that Claude got right | | 7 of 10 (GS-01, 02, 03, 04, 05, 08, 10), in every run |
| Rule-based passes that Claude got wrong | | 1 of 4 (GS-12, partial), in every run |

The three runs agreed completely on which scenarios were right: the same 10 were
judged correct every time, and no scenario moved between correct and not
correct. Only the kind of miss varied, between partial and incorrect, on GS-06
and GS-14.

The seven corrections are exactly the symptom-over-cause cases. For the bad
deploy in GS-01, Claude wrote that "a recent deployment to payments-api …
introduced a code defect" and recommended a rollback, while the rule-based
report led with the error spike. It also named memory exhaustion in GS-04 from
the OutOfMemoryException messages alone, although no agent reports memory
usage.

The four scenarios never judged correct:

| Scenario | Verdicts (runs 1, 2, 3) | What happened |
|---|---|---|
| GS-06 noise | Incorrect, partial, partial | Correctly explained the six errors as client disconnects and noted the 0.2 percent rate. In later runs it even suggested tuning the alert. It still presented the disconnects as a root cause needing follow-up instead of calling the alert noise. |
| GS-09 CPU saturation | Partial in all 3 | Hedged between a slow dependency, saturation and a bad deploy. It never received CPU data, because no agent queries utilization (cause 2 below). |
| GS-12 stuck instance | Partial in all 3 | Identified the disposed-connection mechanism but blamed a code regression, contradicting "nothing was deployed", and missed the restart remedy. |
| GS-14 traffic blackhole | Partial, incorrect, incorrect | Said no root cause could be found. In runs 2 and 3 it leaned toward a transient blip or a misconfigured alert, and treated the 37 requests as a sample-size problem rather than as the signal. That reading argues against paging during a real outage. It never saw the alert rule ("Request count < 100"), because the orchestrator passes only the alert id (cause 7 below). |

What the LLM tier does **not** fix: with reasoning enabled the report reads
better, but the remediation proposals and the escalation decision still come
from the rule-based findings. In GS-01 a responder would see Claude recommend a
rollback directly above a remediation proposal to restart. The proposals and
the missed page in GS-14 are unchanged.

Caveats on these numbers: three runs of 14 scenarios is still a small sample,
and one scenario is about seven percentage points. All three runs used the same
model pair; other analysis models are untested. I read every verdict that was
not "correct" and found them consistent with the labels; the "correct" verdicts
were spot-checked, not audited one by one.

## Operational impact

| Dimension | Finding | Evidence |
|---|---|---|
| Responder misdirection | In 5 incidents the cause was detected but ranked below its own symptom; in a sixth it tied with a symptom. A responder reading top-down starts on the wrong problem. | GS-01, GS-02, GS-05, GS-08, GS-10 (ranked below), GS-03 (tie) |
| Missed page | A collapse in request volume with otherwise healthy telemetry was marked resolved. Nobody would have been paged for a revenue-impacting outage. | GS-14 |
| Automation risk | Restart or scale was proposed for a database outage, a platform outage, a slow partner API and a bad input batch. None of these actions fixes the problem; scaling against a slow partner adds load to it. | GS-02, GS-03, GS-05, GS-11 |
| Wrong action on bad deploys | For all three change-caused incidents, rollback was not the first proposal. In two, the rollback proposal was discarded entirely. | GS-01, GS-08, GS-10 |
| Detection gap | Memory and CPU exhaustion are never detected, because the query that reads those counters exists but no agent runs it. | GS-04, GS-09 |

Remediation is disabled by default, and a startup guard refuses to enable it
against real Azure (ADR-022), so none of these proposals executes today. They
matter before remediation is enabled.

## What causes the misses

Each item below is verified by reading the code and by the scenario results
cited. None has been fixed in this change; this change only measures them.

1. **Findings are ranked by severity alone, so symptoms outrank causes.**
   `InvestigationWorkflow::generate_report` sorts findings by severity, and the
   rule-based RCA takes the most severe one. An error spike from a bad deploy is
   Critical; the deployment correlation that explains it is Medium. The same
   happens with dependency failures (High) under error spikes (Critical).
   Affects GS-01, GS-02, GS-05, GS-08, GS-10, and the GS-03 tie.
2. **No agent queries resource utilization.** The KQL template
   `resource_utilization` exists in `sre-azure` but is never called, so no
   finding is ever categorized as resource exhaustion. Affects GS-04 and GS-09.
3. **A rollback can be silently discarded.** The rollback proposal is filed
   under the `RestartWorkload` category, and the proposer keeps only the most
   confident proposal per category. When a High or Critical error spike is also
   present and its restart proposal is more confident than the rollback (0.90
   against 0.80 in both affected scenarios), the restart replaces the rollback.
   Affects GS-01 and GS-08. In GS-10, a scale-out (0.90) outranks the rollback
   (0.80).
4. **The remediation playbook maps symptoms to actions.** Any High or Critical
   error spike proposes a restart, and any High latency proposes a scale-out,
   even when a detected cause (a failing dependency, a platform outage) means
   neither action can help. Affects GS-02, GS-03, GS-05, GS-11.
5. **Request failure counts ignore volume.** The failure breakdown reports at
   least a Medium error spike whenever any request fails. In GS-06, 6 failures
   out of 3,100 requests become the reported root cause of a false alarm; in
   GS-05, 8 out of 2,100 become a false error-spike signal.
6. **No traffic-volume detection, and low-value findings lift confidence.** No
   agent compares request volume to a baseline. In GS-14 the only finding is a
   Low-severity latency readout at 0.6 confidence, which clears the 0.5
   escalation threshold, so the incident resolves instead of escalating.
7. **The LLM reasoning engine never sees what the alert measured.** The
   orchestrator passes the alert id as the trigger, not the alert rule or
   description. In GS-14 the rule says "Request count < 100", which is the one
   clue to the cause, and the reasoning engine cannot see it.

## Proposed remediation

These are proposals for follow-up changes, not part of this change. Each
alters investigation behavior, and two alter which remediation the platform
proposes, so each needs its own review. The scenarios each one targets are
listed so its effect can be measured with this evaluation; the size of the
effect has not been measured.

| # | Change | Targets |
|---|---|---|
| 1 | Rank causal findings (change correlation, dependency failure, service health, resource exhaustion) above symptom findings (error spike, latency) when both are present. | GS-01, 02, 03, 05, 08, 10 |
| 2 | Run `resource_utilization` in the metrics agent and emit resource-exhaustion findings above set thresholds. | GS-04, 09 |
| 3 | Give rollback its own remediation category, and prefer it when a change correlates with the incident. | GS-01, 08, 10 |
| 4 | Suppress restart and scale-out proposals when a dependency or platform cause is detected. | GS-02, 03, 05 |
| 5 | Grade request failures by failure rate, not absolute count. | GS-05, 06 |
| 6 | Add a traffic-volume check, and exclude Low and Info findings from the escalation confidence. | GS-14 |
| 7 | Pass the alert rule name and description into the reasoning engine's input. | GS-14 (LLM tier) |

To avoid fitting the platform to these 14 incidents, each change should be
justified by its incident narrative and should add at least one new scenario
that exercises it.

## Acceptance criteria for this evaluation

1. `cargo run -p sre-evals` runs all 14 scenarios without Azure, Docker or an
   API key, and writes `docs/evals/scorecard.md` and `scorecard.json`.
2. Three consecutive runs produce identical scores. Verified on 2026-10-11.
3. `cargo test -p sre-evals` fails when any check recorded as passing in
   `crates/evals/baseline.json` fails. Verified by setting one baseline check
   to passing that the pipeline fails; the test failed with
   `GS-01: root_cause regressed (pass -> fail)`.
4. A unit test fails if any KQL template stops routing to exactly one fixture
   key.
5. `--engine claude` grades the Claude RCA with an LLM judge. Verified against
   a stubbed Anthropic endpoint in the unit tests, and against the real API on
   2026-10-11: three runs, 84 calls, 0 failures, summarized in
   `scorecard-claude.md`.

## Verified, inferred, and not yet checked

- **Verified:** every number in the Results table (generated by the harness);
  the seven causes above (code reading plus the per-scenario findings in the
  scorecard); determinism across three runs; the ratchet gate failing on a
  regression; the LLM-tier counts (generated in `claude-run-1.json` to
  `claude-run-3.json` from three runs against the real API, and combined in
  `scorecard-claude.json`).
- **Inferred:** that the fixture telemetry is representative. The rows are
  hand-authored to match the columns and magnitudes Azure returns; they are not
  captured from production incidents.
- **Not yet checked:**
  - The LLM tier used one model pair. Whether a newer analysis model does
    better on the same findings is untested.
  - The labels have one author. A second SRE should review them before the
    numbers drive decisions.
  - Timings exclude Azure query latency, so the mean investigation time in the
    scorecard is not a production latency figure.

## Follow-ups (out of scope here)

- Implement the seven proposals above, one change at a time, re-running the
  evaluation after each.
- Compare analysis models on the same findings (`--rca-model`), for example the
  platform's default `claude-opus-4-8` against `claude-opus-5-5`.
- Replace hand-authored fixtures with sanitized captures from real incidents
  as they occur, and grow the set past 30 scenarios so a held-out split
  becomes meaningful.
- Second-reviewer pass on all labels.

## Technical reference

| Item | Location |
|---|---|
| Scenarios (one JSON file each) | `crates/evals/golden/` |
| Fixture server and KQL template routing | `crates/evals/src/fixture.rs` |
| Pipeline runner | `crates/evals/src/runner.rs` |
| Scoring rules (tie handling, detection threshold) | `crates/evals/src/score.rs` |
| LLM judge | `crates/evals/src/judge.rs` |
| Repeated-run summary | `crates/evals/src/runs.rs` |
| CI ratchet baseline | `crates/evals/baseline.json` |
| CI gate test | `crates/evals/tests/golden_set.rs` |

```text
cargo run -p sre-evals                         # deterministic tier, writes the scorecard
cargo run -p sre-evals -- --only GS-14         # one scenario, printed only
cargo run -p sre-evals -- --write-baseline     # accept improvements into the ratchet
cargo run -p sre-evals -- --engine claude --file-stem claude-run-4   # one LLM-tier run (needs ANTHROPIC_API_KEY)
cargo run -p sre-evals -- --summarize-runs     # combine claude-run-*.json into scorecard-claude.md
cargo test -p sre-evals                        # unit tests + ratchet gate
```

Scoring rules in brief. A finding counts as a detection, and as a root-cause
claim, when it is Medium severity or above and not informational. The report's
root cause is the category of its most severe detection; if two categories tie
at that severity, the call is scored as a miss, because agent arrival order
would decide which one a reader sees. The first remediation is the most
confident proposal, with ties also scored as a miss. A rollback is told apart
from a restart by the ARM call it would make.
