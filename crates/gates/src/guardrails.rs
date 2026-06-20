use std::collections::{HashMap, VecDeque};

use chrono::{DateTime, Duration, Timelike, Utc};

/// Guardrail tuning (ADR-009 §4). Defaults match the ADR.
#[derive(Debug, Clone)]
pub struct GuardrailConfig {
    /// Global kill switch — when true, nothing auto-approves.
    pub kill_switch: bool,
    /// Per-subscription auto-approvals allowed per hour.
    pub per_subscription_hourly_limit: usize,
    /// Per-resource-type auto-approvals allowed per hour.
    pub per_resource_type_hourly_limit: usize,
    /// Minimum confidence for a LowRisk action to auto-approve.
    pub auto_approve_min_confidence: f64,
    /// Maximum blast radius (resource count) for auto-approve. Non-overridable
    /// in spirit — larger always routes to a human.
    pub max_auto_approve_blast_radius: usize,
    /// Restrict auto-approve to business hours (09:00–17:00 UTC).
    pub business_hours_only: bool,
}

impl Default for GuardrailConfig {
    fn default() -> Self {
        Self {
            kill_switch: false,
            per_subscription_hourly_limit: 5,
            per_resource_type_hourly_limit: 3,
            auto_approve_min_confidence: 0.95,
            max_auto_approve_blast_radius: 1,
            business_hours_only: false,
        }
    }
}

/// Sliding-window rate limiter keyed by arbitrary strings. `now` is injected so
/// the policy is deterministic under test (no hidden clock).
#[derive(Debug)]
pub(crate) struct RateLimiter {
    events: HashMap<String, VecDeque<DateTime<Utc>>>,
    window: Duration,
}

impl RateLimiter {
    pub fn hourly() -> Self {
        Self {
            events: HashMap::new(),
            window: Duration::hours(1),
        }
    }

    /// Number of events for `key` within `[now - window, now]`, pruning older.
    pub fn count(&mut self, key: &str, now: DateTime<Utc>) -> usize {
        let q = self.events.entry(key.to_string()).or_default();
        while let Some(&front) = q.front() {
            if now - front > self.window {
                q.pop_front();
            } else {
                break;
            }
        }
        q.len()
    }

    pub fn record(&mut self, key: &str, now: DateTime<Utc>) {
        self.events
            .entry(key.to_string())
            .or_default()
            .push_back(now);
    }
}

/// Extract the ARM resource type (`Microsoft.App/containerApps`) from a resource
/// id for per-resource-type rate limiting. Returns `"unknown"` when the id has
/// no `providers/<ns>/<type>` segment.
pub fn resource_type_of(resource_id: &str) -> String {
    let parts: Vec<&str> = resource_id.split('/').collect();
    if let Some(i) = parts
        .iter()
        .position(|p| p.eq_ignore_ascii_case("providers"))
    {
        if i + 2 < parts.len() {
            return format!("{}/{}", parts[i + 1], parts[i + 2]);
        }
    }
    "unknown".to_string()
}

/// 09:00–17:00 UTC business-hours window for the optional auto-approve window.
pub(crate) fn in_business_hours(now: DateTime<Utc>) -> bool {
    (9..17).contains(&now.hour())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn resource_type_parsing() {
        assert_eq!(
            resource_type_of(
                "/subscriptions/s/resourceGroups/rg/providers/Microsoft.App/containerApps/web"
            ),
            "Microsoft.App/containerApps"
        );
        assert_eq!(resource_type_of("just-a-name"), "unknown");
    }

    #[test]
    fn rate_limiter_prunes_outside_window() {
        let mut rl = RateLimiter::hourly();
        let t0 = DateTime::<Utc>::from_timestamp(0, 0).unwrap();
        rl.record("k", t0);
        rl.record("k", t0);
        assert_eq!(rl.count("k", t0), 2);
        // 2 hours later, both events have aged out of the 1h window.
        let t2 = t0 + Duration::hours(2);
        assert_eq!(rl.count("k", t2), 0);
    }
}
