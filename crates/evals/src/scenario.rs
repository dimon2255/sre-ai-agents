//! Golden-set scenario schema: one labeled incident per JSON file.

use std::collections::BTreeMap;
use std::path::Path;

use anyhow::{bail, Context};
use serde::{Deserialize, Serialize};

use sre_common::findings::FindingCategory;
use sre_common::messages::AlertSeverity;

use crate::fixture::{TEMPLATE_KEYS, WILDCARD_KEY};

/// Root-cause label for an incident where no root cause should be claimed
/// (a false alarm, or telemetry the platform cannot see).
pub const NONE_LABEL: &str = "none";

/// First remediation actions a scenario may accept.
pub const REMEDIATION_ACTIONS: &[&str] = &[
    "restart_workload",
    "rollback_deployment",
    "scale_compute",
    "none",
];

/// One labeled incident.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Scenario {
    pub id: String,
    pub title: String,
    /// What actually happened, written before the pipeline was run against it.
    pub narrative: String,
    #[serde(default)]
    pub tags: Vec<String>,
    pub alert: AlertSpec,
    /// Fixture KQL result rows keyed by template name (see
    /// [`crate::fixture::TEMPLATE_KEYS`]); `"*"` applies to every template
    /// without its own entry. A template with no entry returns an empty table.
    #[serde(default)]
    pub telemetry: BTreeMap<String, Fixture>,
    pub expected: Expected,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AlertSpec {
    pub rule: String,
    pub severity: AlertSeverity,
    /// Azure Common Alert Schema signal type: `Metric`, `Log` or `Activity Log`.
    pub signal_type: String,
    /// Resource name or full ARM resource id.
    pub resource: String,
    #[serde(default)]
    pub description: Option<String>,
    #[serde(default)]
    pub raw_context: Option<serde_json::Value>,
}

/// The fixture answer to one KQL template.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(untagged)]
pub enum Fixture {
    /// Result rows, one JSON object per row.
    Rows(Vec<serde_json::Map<String, serde_json::Value>>),
    /// The query fails with this HTTP status (RBAC, throttling, outage).
    Error { status: u16, message: String },
}

/// Ground truth for one incident.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Expected {
    /// True root cause as a finding category, or `none`. `a|b` accepts `b` as
    /// an equivalent within the same causal family; `a` is the canonical name.
    pub root_cause: String,
    /// Every signal genuinely present in the telemetry, in the same `a|b` syntax.
    #[serde(default)]
    pub signals: Vec<String>,
    /// Whether a human must be paged: the evidence does not support a
    /// confident automated conclusion.
    pub escalate: bool,
    /// First remediation actions an SRE would accept. `none` means no
    /// automated action is appropriate.
    pub remediation: Vec<String>,
    /// Why the labels are what they are.
    pub rationale: String,
}

/// A parsed `a|b` label: the canonical category plus accepted equivalents.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Label {
    pub alternatives: Vec<String>,
}

impl Label {
    pub fn parse(raw: &str) -> Self {
        Self {
            alternatives: raw.split('|').map(|s| s.trim().to_string()).collect(),
        }
    }

    pub fn canonical(&self) -> &str {
        &self.alternatives[0]
    }

    pub fn matches(&self, category: &str) -> bool {
        self.alternatives.iter().any(|a| a == category)
    }
}

/// Snake-case name of a finding category, as it appears in labels.
pub fn category_name(category: &FindingCategory) -> String {
    serde_json::to_value(category)
        .ok()
        .and_then(|v| v.as_str().map(String::from))
        .unwrap_or_else(|| format!("{category:?}"))
}

fn is_category(name: &str) -> bool {
    serde_json::from_value::<FindingCategory>(serde_json::Value::String(name.to_string())).is_ok()
}

impl Scenario {
    /// Reject labels and fixtures that would silently score wrong.
    pub fn validate(&self) -> anyhow::Result<()> {
        let root = Label::parse(&self.expected.root_cause);
        for alt in &root.alternatives {
            if alt != NONE_LABEL && !is_category(alt) {
                bail!("{}: unknown root_cause category '{alt}'", self.id);
            }
        }
        if root.alternatives.len() > 1 && root.matches(NONE_LABEL) {
            bail!(
                "{}: 'none' cannot be combined with other root causes",
                self.id
            );
        }
        for signal in &self.expected.signals {
            for alt in &Label::parse(signal).alternatives {
                if !is_category(alt) {
                    bail!("{}: unknown signal category '{alt}'", self.id);
                }
            }
        }
        if self.expected.remediation.is_empty() {
            bail!("{}: remediation must list at least one action", self.id);
        }
        for action in &self.expected.remediation {
            if !REMEDIATION_ACTIONS.contains(&action.as_str()) {
                bail!("{}: unknown remediation action '{action}'", self.id);
            }
        }
        for key in self.telemetry.keys() {
            if key != WILDCARD_KEY && !TEMPLATE_KEYS.iter().any(|(k, _)| k == key) {
                bail!("{}: unknown telemetry template '{key}'", self.id);
            }
        }
        Ok(())
    }
}

/// Load and validate every `*.json` scenario in `dir`, sorted by id.
pub fn load_dir(dir: &Path) -> anyhow::Result<Vec<Scenario>> {
    let mut scenarios = Vec::new();
    let entries = std::fs::read_dir(dir)
        .with_context(|| format!("reading golden set directory {}", dir.display()))?;
    for entry in entries {
        let path = entry?.path();
        if path.extension().and_then(|e| e.to_str()) != Some("json") {
            continue;
        }
        let text = std::fs::read_to_string(&path)
            .with_context(|| format!("reading {}", path.display()))?;
        let scenario: Scenario =
            serde_json::from_str(&text).with_context(|| format!("parsing {}", path.display()))?;
        scenario.validate()?;
        scenarios.push(scenario);
    }
    scenarios.sort_by(|a, b| a.id.cmp(&b.id));
    let mut ids: Vec<&str> = scenarios.iter().map(|s| s.id.as_str()).collect();
    ids.dedup();
    if ids.len() != scenarios.len() {
        bail!("duplicate scenario ids in {}", dir.display());
    }
    if scenarios.is_empty() {
        bail!("no scenarios found in {}", dir.display());
    }
    Ok(scenarios)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn label_parses_alternatives() {
        let l = Label::parse("config_change|deployment_correlation");
        assert_eq!(l.canonical(), "config_change");
        assert!(l.matches("deployment_correlation"));
        assert!(!l.matches("error_spike"));
    }

    #[test]
    fn category_name_is_snake_case() {
        assert_eq!(
            category_name(&FindingCategory::DeploymentCorrelation),
            "deployment_correlation"
        );
    }

    #[test]
    fn known_categories_validate() {
        assert!(is_category("service_health"));
        assert!(!is_category("ServiceHealth"));
        assert!(!is_category("none"));
    }
}
