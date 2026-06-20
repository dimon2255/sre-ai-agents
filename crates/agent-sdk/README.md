# sre-agent-sdk

Build **custom investigation agents** for the SRE AI Agents platform.

The platform is an orchestrator + specialized sub-agents. Each agent is a
long-lived async task that receives tasks on an inbox channel, does
domain-specific analysis, and replies with findings on the per-investigation
reply channel bundled with each message (ADR-006). This crate is the stable,
lightweight surface you implement to add your own agent — it depends only on
`sre-common`, not on the platform's Azure layer, store, or orchestrator.

## Install

```toml
[dependencies]
sre-agent-sdk = { path = "../agent-sdk" }   # or a registry/git version once published
tokio = { version = "1", features = ["full"] }
async-trait = "0.1"
```

## Implement `SreAgent`

```rust
use sre_agent_sdk::prelude::*;

struct DiskSpaceAgent { id: AgentId }

#[async_trait]
impl SreAgent for DiskSpaceAgent {
    fn id(&self) -> &AgentId { &self.id }
    fn agent_type(&self) -> AgentType { AgentType::Custom("disk-space".to_string()) }
    fn capabilities(&self) -> Vec<String> { vec!["disk.utilization".to_string()] }

    async fn run(&self, mut inbox: mpsc::Receiver<InboundMessage>) -> Result<(), AgentError> {
        while let Some(inbound) = inbox.recv().await {
            if let AgentPayload::AnalysisTask(task) = &inbound.message.payload {
                // ...analyze task.params.resource_name, build Finding(s)...
                let result = AnalysisResult { findings: vec![/* ... */], summary: "...".into(), queries_executed: vec![] };
                let response = AgentMessage::new(
                    inbound.message.correlation_id,
                    self.id.clone(),
                    MessageTarget::Orchestrator,
                    AgentPayload::AnalysisResult(result),
                    0.9,
                ).with_parent(inbound.message.id);
                let _ = inbound.reply_tx.send(response).await;
            }
        }
        Ok(())
    }
}
```

A complete, runnable version lives in `examples/custom_agent.rs`:

```
cargo run -p sre-agent-sdk --example custom_agent
```

## The contract

- **One task, one reply.** For each `AgentPayload::AnalysisTask`, send exactly one
  `AgentPayload::AnalysisResult` back on `inbound.reply_tx`, tagged
  `.with_parent(msg.id)` so the orchestrator can correlate it.
- **Run until the inbox closes.** Loop until `inbox.recv()` returns `None` — that
  is the orchestrator's shutdown signal.
- **Answer health checks.** Reply to `AgentPayload::HealthCheck` with an
  `AgentHealth`.
- **Stay read-only** unless you are explicitly building a remediation agent
  (ADR-004) — investigation agents observe, they don't mutate.

## Registering with the orchestrator

The host application spawns `agent.run(inbox)` and registers an `AgentHandle`
(id, `agent_type`, inbox `Sender`, capabilities) with its router. Custom agents
(`AgentType::Custom(kind)`) are dispatched when triage recommends them or via
broadcast; wiring triage to recommend a new kind is the host's extension point.

See ADR-019 for the design rationale.
