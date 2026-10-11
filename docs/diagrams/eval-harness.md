# Golden-Set Evaluation Harness — Data Flow

See [ADR-023](../adr/023-golden-set-evaluation.md).

```mermaid
flowchart LR
    subgraph Golden["crates/evals/golden/*.json"]
        ALERT["alert<br/>(rule, severity, signal, resource)"]
        TELEM["telemetry<br/>(rows per KQL template)"]
        LABEL["expected<br/>(root cause, signals,<br/>escalation, remediation)"]
    end

    subgraph Pipeline["Production pipeline (unmodified)"]
        ORCH["Orchestrator::investigate"]
        TRIAGE["TriageAgent"]
        AGENTS["Logs / Metrics / Trace / Infra agents"]
        QC["AzureQueryClient"]
        REM["RemediationAgent::propose"]
    end

    FIX["Fixture server (wiremock)<br/>KQL text → template key → rows"]

    ALERT --> ORCH
    ORCH --> TRIAGE --> AGENTS --> QC
    QC -- "POST /v1/workspaces|apps/{id}/query" --> FIX
    TELEM --> FIX
    FIX -- "tables[] JSON" --> QC
    ORCH -- "InvestigationReport" --> SCORE
    REM --> SCORE
    LABEL --> SCORE

    SCORE["Scorer<br/>(order-independent)"] --> CARD["Scorecard<br/>docs/evals/scorecard.md<br/>+ scorecard.json"]
    SCORE --> GATE["CI ratchet<br/>baseline.json"]

    subgraph Optional["--engine claude (opt-in)"]
        LLM["ClaudeReasoningEngine"] --> JUDGE["LLM judge vs label"]
    end
    ORCH -. findings .-> LLM
    JUDGE -.-> CARD
```

## What is real and what is replaced

| Component | In the eval | Why |
|---|---|---|
| Orchestrator, workflow, aggregator | Real | These decide ranking and escalation, the behavior under test. |
| Triage, logs, metrics, trace, infra agents | Real | Thresholds and finding categories are under test. |
| `AzureQueryClient` (HTTP, auth header, parsing) | Real | Exercises the same request and response path as production. |
| Azure Log Analytics / App Insights endpoints | Replaced by fixture server | Hand-authored rows per scenario, shaped like real query results, make runs reproducible and free. |
| Workflow store and event log | In-memory | Persistence is not under test and needs no Docker. |
| Remediation proposer | Real (`propose` only) | Measures what would be proposed. Nothing executes. |
| Reasoning engine | Off by default; Claude when `--engine claude` | The LLM tier is optional and not deterministic. |
