# ADR-005: KQL Template-Driven Queries

## Status

Accepted (2026-04-05)

## Context

The LogsAgent and MetricsAgent need to query Azure Log Analytics and Application Insights using KQL (Kusto Query Language). We must decide how agents construct their queries.

### Cost and Safety Concerns

- **Log Analytics charges per GB scanned.** A freeform query like `AppExceptions | where TimeGenerated > ago(30d)` on a high-volume workspace could scan terabytes and cost hundreds of dollars.
- **Unbounded queries can timeout.** Log Analytics has a 10-minute query timeout; large scans may hit it and return partial results.
- **Freeform KQL from an LLM (Phase 3) could produce syntactically valid but semantically dangerous queries** — e.g., cross-joining large tables or missing time filters.

### Options Considered

| Option | Pros | Cons |
|--------|------|------|
| **Parameterized KQL templates** | Cost-controlled, safe, predictable, testable | Less flexible, requires new template for new analysis patterns |
| Freeform KQL generation | Maximum flexibility, LLM can craft novel queries | Cost risk, timeout risk, injection risk, hard to test |
| Pre-built Azure Workbooks | Zero query cost (pre-computed) | Limited to what's already built, can't customize per investigation |

## Decision

**Parameterized KQL templates for Phase 1 and Phase 2.** Each agent selects from a library of pre-built, tested templates and fills in parameters (resource name, timespan, thresholds).

### Template Design Principles

1. **Every template MUST include a time filter** — `| where TimeGenerated > ago({timespan})` is mandatory. No unbounded scans.
2. **Every template MUST include a row limit** — `| take {max_rows}` (default 1000) prevents runaway result sets.
3. **Parameters are validated before interpolation** — timespan capped at configurable max (default 24h), resource names sanitized.
4. **Templates are versioned and tested** — each template has unit tests that verify valid KQL syntax and expected output shape.

### Template Categories

| Category | Agent | KQL Tables | Purpose |
|----------|-------|------------|---------|
| Error Analysis | LogsAgent | `AppExceptions`, `AppTraces` | Error spike detection, exception clustering |
| Performance | MetricsAgent | `AppRequests`, `AppMetrics` | Latency percentiles, failure rates, throughput |
| Dependency | TraceAgent (Phase 2) | `AppDependencies` | Dependency failure correlation |
| Infrastructure | InfraAgent (Phase 2) | `AzureActivity`, `AzureDiagnostics` | Deployment detection, config changes |
| Custom Log | LogsAgent | `Syslog`, `ContainerLog` | Container/VM log analysis |

### Example Template

```rust
pub fn error_spike(resource_name: &str, timespan_min: u32) -> String {
    format!(r#"
AppExceptions
| where TimeGenerated > ago({timespan_min}m)
| where AppRoleName == "{resource_name}"
| summarize ErrorCount=count(),
            DistinctErrors=dcount(ExceptionType)
            by bin(TimeGenerated, 1m), ExceptionType, ProblemId
| order by ErrorCount desc
| take 50
"#)
}
```

### Query Budget Enforcement

```rust
pub struct QueryBudget {
    pub max_timespan: Duration,          // Default: 24 hours
    pub max_rows: u32,                   // Default: 1000
    pub max_queries_per_investigation: u32, // Default: 20
    pub max_queries_per_minute: u32,     // Default: 30 (below Azure's 200/min throttle)
}
```

### Phase 3 Evolution

When the LLM reasoning layer is added in Phase 3, it will NOT generate freeform KQL. Instead:

1. LLM selects which template to use based on investigation context
2. LLM provides parameters for the selected template
3. Parameters are validated against budget constraints
4. Template is rendered and executed

This keeps the cost and safety guardrails intact while allowing LLM intelligence in template selection and parameter choice. Freeform KQL may be considered as a Phase 3+ capability with additional safeguards (query cost estimation, dry-run validation, human approval for novel queries).

## Consequences

### What becomes easier

- **Cost control** — every query has bounded time range and row limits. No surprise Azure bills.
- **Safety** — no KQL injection risk. Parameters are interpolated into known-safe templates.
- **Testing** — templates can be unit tested for valid syntax and expected output shape.
- **Performance** — bounded queries return quickly and predictably.
- **Auditability** — the audit log records which template was used with which parameters, not opaque query strings.

### What becomes harder

- **Novel analysis** — if an incident requires a query pattern not covered by existing templates, a new template must be added (code change + deploy).
- **LLM flexibility (Phase 3)** — the LLM can't craft arbitrary KQL; it's constrained to template selection. This is a deliberate trade-off favoring safety over flexibility.
- **Template maintenance** — as Azure adds new tables or changes schemas, templates need updating. Mitigation: integration tests that run templates against real workspaces.
