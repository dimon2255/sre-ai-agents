# ADR-002: Azure-First Observability Stack

## Status

Accepted (2026-04-05)

## Context

The SRE agent platform needs to query telemetry data — logs, metrics, traces, and alerts — to investigate incidents. We must choose which observability backends to integrate with first.

Our production environment runs on Azure. The primary observability tools in use are:

- **Azure Monitor** — alerting and metric collection
- **Log Analytics** — centralized log ingestion and KQL-based querying
- **Application Insights** — APM, distributed tracing, request/dependency telemetry
- **Azure Alerts** — alert rules with action groups that fire webhooks

### Options Considered

| Option | Pros | Cons |
|--------|------|------|
| **Azure-first** (Monitor, Log Analytics, App Insights) | Matches our stack, one auth model (Entra ID), one query language (KQL), webhook-driven alerts | Coupled to Azure, no multi-cloud |
| Generic abstraction (support Prometheus, Grafana, Datadog from day 1) | Multi-provider from start | Leaky abstractions, slower delivery, premature generalization |
| Prometheus + Grafana stack | Open-source, vendor-neutral | Not our primary stack, would need dual ingestion |

## Decision

**Azure-first for Phase 1 and Phase 2.** All agents will query Azure services directly:

- **Log Analytics REST API** (`api.loganalytics.io`) — KQL queries for log data
- **Application Insights REST API** (`api.applicationinsights.io`) — KQL queries for APM data
- **Azure Monitor Alerts API** (`management.azure.com`) — alert ingestion and status
- **Azure Resource Manager API** — resource health, activity log, deployment status (Phase 2)

### Authentication

- **Production**: Azure Managed Identity (zero-credential deployment)
- **Development**: Azure CLI credential / Service Principal via environment variables
- **Crate**: `azure_identity::DefaultAzureCredential` (auto-selects appropriate credential)

### Required RBAC Assignments

| Phase | Role | Scope | Purpose |
|-------|------|-------|---------|
| Phase 1 | `Log Analytics Reader` | Per workspace | KQL queries |
| Phase 1 | `Monitoring Reader` | Subscription | Alert ingestion |
| Phase 1 | `Reader` | Resource groups | Resource metadata |
| Phase 2 | `Contributor` | Specific resources only | Remediation actions (tightly scoped) |

### API Endpoints

| Service | Endpoint | Auth Scope |
|---------|----------|------------|
| Log Analytics | `https://api.loganalytics.io/v1/workspaces/{id}/query` | `https://api.loganalytics.io/.default` |
| App Insights | `https://api.applicationinsights.io/v1/apps/{id}/query` | `https://api.applicationinsights.io/.default` |
| Azure Alerts | `https://management.azure.com/subscriptions/{sub}/providers/Microsoft.AlertsManagement/alerts` | `https://management.azure.com/.default` |
| Resource Health | `https://management.azure.com/{resourceId}/providers/Microsoft.ResourceHealth/availabilityStatuses` | `https://management.azure.com/.default` |

### Alert Ingestion

Azure Alert action groups push to our webhook using the **Common Alert Schema** — a standardized JSON payload across all signal types (metric, log, activity log, App Insights). This means one webhook handler parses all alert types.

## Consequences

### What becomes easier

- **Single auth model** — Entra ID + Managed Identity for everything. No credential sprawl.
- **Single query language** — KQL covers both Log Analytics and App Insights. Agents share a `KqlQueryExecutor`.
- **Webhook-driven** — Azure Alerts push to us via action groups. No polling infrastructure needed.
- **Operational simplicity** — Fewer integrations to maintain, monitor, and debug in Phase 1.

### What becomes harder

- **Multi-cloud support** — If we later need GCP Cloud Logging or AWS CloudWatch, we'll need an abstraction layer. Plan: introduce `SreDataSource` trait in Phase 2 to allow pluggable backends.
- **Vendor lock-in risk** — Deep coupling to Azure APIs and KQL. Mitigated by isolating all Azure code in the `sre-azure` crate behind trait interfaces.
- **Testing without Azure** — Integration tests require Azure resources. Mitigation: mock the `AzureQueryClient` trait in unit tests, use recorded responses for integration tests.
