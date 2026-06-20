//! Progressive trust model (Phase 3, ADR-017).
//!
//! Auto-remediation earns autonomy. A remediation *category* starts untrusted —
//! its auto-approve confidence floor is the ADR-009 base of 0.95 — and ratchets
//! the floor down as it accumulates **consecutive successful** executions:
//!
//! | consecutive successes | floor | cooldown on failure |
//! |----------------------:|------:|--------------------:|
//! | 0   (base)            | 0.95  | none                |
//! | 10                    | 0.93  | 15 min              |
//! | 25                    | 0.90  | 30 min              |
//! | 50                    | 0.87  | 60 min              |
//! | 100 (minimum)         | 0.85  | 60 min              |
//!
//! A single failure **resets** the category to the base floor and, if it had
//! graduated past base, imposes a cooldown during which auto-remediation for that
//! category is suspended (the gate routes proposals to human approval). This
//! mirrors the *Progressive Trust Model* panel in
//! `docs/diagrams/phase3-architecture.md`.
//!
//! Pure policy with an **injected clock** (like the guardrail `RateLimiter`):
//! `now` is always passed in, so cooldown behaviour is deterministically
//! testable. The floor only ever modulates the LowRisk auto-approve bar between
//! `min_threshold` and `base_threshold`; HighRisk/Destructive are never
//! auto-approved regardless of trust (the gate enforces that independently).

use std::collections::HashMap;
use std::sync::Mutex;

use chrono::{DateTime, Duration, Utc};

use sre_common::remediation::RemediationCategory;

/// Trust thresholds and bounds.
#[derive(Debug, Clone, Copy)]
pub struct TrustPolicy {
    /// The starting (untrusted) auto-approve confidence floor — ADR-009's 0.95.
    pub base_threshold: f64,
    /// The lowest floor a fully-trusted category can reach.
    pub min_threshold: f64,
}

impl Default for TrustPolicy {
    fn default() -> Self {
        Self {
            base_threshold: 0.95,
            min_threshold: 0.85,
        }
    }
}

/// `(min_consecutive_successes, threshold, cooldown_minutes_on_failure)`, highest
/// tier first. The first row whose success count is met wins.
const TIERS: &[(u32, f64, i64)] = &[
    (100, 0.85, 60),
    (50, 0.87, 60),
    (25, 0.90, 30),
    (10, 0.93, 15),
    (0, 0.95, 0),
];

fn tier_for(successes: u32) -> (f64, i64) {
    for &(min, threshold, cooldown) in TIERS {
        if successes >= min {
            return (threshold, cooldown);
        }
    }
    (0.95, 0)
}

#[derive(Debug, Clone, Copy, Default)]
struct CategoryTrust {
    consecutive_successes: u32,
    cooldown_until: Option<DateTime<Utc>>,
}

/// Per-category trust state. Shared as `Arc<TrustModel>`; critical sections are
/// O(1) and never cross an await, so a `std::sync::Mutex` is correct.
pub struct TrustModel {
    policy: TrustPolicy,
    state: Mutex<HashMap<RemediationCategory, CategoryTrust>>,
}

impl TrustModel {
    pub fn new(policy: TrustPolicy) -> Self {
        Self {
            policy,
            state: Mutex::new(HashMap::new()),
        }
    }

    pub fn with_defaults() -> Self {
        Self::new(TrustPolicy::default())
    }

    /// The effective auto-approve confidence floor for `category` at `now`.
    ///
    /// Returns `None` when the category is **in cooldown** — auto-remediation is
    /// suspended and the caller must route the proposal to human approval.
    /// Otherwise returns the earned floor, clamped to `[min_threshold,
    /// base_threshold]`.
    pub fn threshold_for(&self, category: RemediationCategory, now: DateTime<Utc>) -> Option<f64> {
        let st = self.state.lock().unwrap_or_else(|e| e.into_inner());
        let ct = st.get(&category).copied().unwrap_or_default();
        if let Some(until) = ct.cooldown_until {
            if now < until {
                return None;
            }
        }
        let (threshold, _) = tier_for(ct.consecutive_successes);
        Some(threshold.clamp(self.policy.min_threshold, self.policy.base_threshold))
    }

    /// Record an executed remediation's outcome, advancing trust on success or
    /// resetting (with a cooldown) on failure. `success` is `true` when the
    /// action executed within scope and held; `false` on ARM failure or an
    /// over-scope rollback.
    pub fn record(&self, category: RemediationCategory, success: bool, now: DateTime<Utc>) {
        let mut st = self.state.lock().unwrap_or_else(|e| e.into_inner());
        let ct = st.entry(category).or_default();

        // Clear an expired cooldown so a post-cooldown success starts climbing.
        if let Some(until) = ct.cooldown_until {
            if now >= until {
                ct.cooldown_until = None;
            }
        }

        if success {
            ct.consecutive_successes = ct.consecutive_successes.saturating_add(1);
        } else {
            // Cooldown duration is set by the tier the category had reached.
            let (_, cooldown_min) = tier_for(ct.consecutive_successes);
            ct.consecutive_successes = 0;
            if cooldown_min > 0 {
                ct.cooldown_until = Some(now + Duration::minutes(cooldown_min));
            }
        }
    }

    /// Consecutive successes for `category` (introspection / metrics).
    pub fn successes(&self, category: RemediationCategory) -> u32 {
        let st = self.state.lock().unwrap_or_else(|e| e.into_inner());
        st.get(&category)
            .map(|c| c.consecutive_successes)
            .unwrap_or(0)
    }

    /// Whether `category` is currently in a failure cooldown.
    pub fn is_in_cooldown(&self, category: RemediationCategory, now: DateTime<Utc>) -> bool {
        self.threshold_for(category, now).is_none()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn at(secs: i64) -> DateTime<Utc> {
        DateTime::<Utc>::from_timestamp(secs, 0).unwrap()
    }

    const CAT: RemediationCategory = RemediationCategory::RestartWorkload;

    #[test]
    fn new_category_starts_at_base_threshold() {
        let trust = TrustModel::with_defaults();
        assert_eq!(trust.threshold_for(CAT, at(0)), Some(0.95));
    }

    #[test]
    fn ten_successes_unlocks_first_discount() {
        let trust = TrustModel::with_defaults();
        for _ in 0..9 {
            trust.record(CAT, true, at(0));
        }
        assert_eq!(trust.threshold_for(CAT, at(0)), Some(0.95), "9 not enough");
        trust.record(CAT, true, at(0));
        assert_eq!(
            trust.threshold_for(CAT, at(0)),
            Some(0.93),
            "10 unlocks 0.93"
        );
    }

    #[test]
    fn thresholds_ratchet_down_through_all_tiers() {
        let trust = TrustModel::with_defaults();
        let checkpoints = [(10, 0.93), (25, 0.90), (50, 0.87), (100, 0.85)];
        let mut recorded = 0u32;
        for (target, expected) in checkpoints {
            while recorded < target {
                trust.record(CAT, true, at(0));
                recorded += 1;
            }
            assert_eq!(
                trust.threshold_for(CAT, at(0)),
                Some(expected),
                "at {recorded} successes"
            );
        }
    }

    #[test]
    fn threshold_never_drops_below_minimum() {
        let trust = TrustModel::with_defaults();
        for _ in 0..500 {
            trust.record(CAT, true, at(0));
        }
        assert_eq!(trust.threshold_for(CAT, at(0)), Some(0.85));
    }

    #[test]
    fn failure_at_base_resets_without_cooldown() {
        let trust = TrustModel::with_defaults();
        for _ in 0..5 {
            trust.record(CAT, true, at(0));
        }
        trust.record(CAT, false, at(0));
        assert_eq!(trust.successes(CAT), 0);
        // Below the first tier, so no cooldown — still auto-eligible at base.
        assert_eq!(trust.threshold_for(CAT, at(0)), Some(0.95));
    }

    #[test]
    fn failure_after_graduation_resets_and_cools_down() {
        let trust = TrustModel::with_defaults();
        for _ in 0..25 {
            trust.record(CAT, true, at(0)); // reaches the 0.90 tier (30-min cooldown)
        }
        assert_eq!(trust.threshold_for(CAT, at(0)), Some(0.90));

        trust.record(CAT, false, at(0)); // failure at the 0.90 tier
        assert_eq!(trust.successes(CAT), 0);
        // In cooldown now → suspended (None).
        assert!(trust.is_in_cooldown(CAT, at(0)));
        assert_eq!(trust.threshold_for(CAT, at(0)), None);
        // Still in cooldown at +29 min…
        assert!(trust.is_in_cooldown(CAT, at(29 * 60)));
        // …expired by +31 min → back to base, auto-eligible again.
        assert_eq!(trust.threshold_for(CAT, at(31 * 60)), Some(0.95));
    }

    #[test]
    fn cooldown_duration_scales_with_tier() {
        // The 0.93 tier (10 successes) → 15-min cooldown.
        let trust = TrustModel::with_defaults();
        for _ in 0..10 {
            trust.record(CAT, true, at(0));
        }
        trust.record(CAT, false, at(0));
        assert!(trust.is_in_cooldown(CAT, at(14 * 60)));
        assert!(!trust.is_in_cooldown(CAT, at(16 * 60)));
    }

    #[test]
    fn trust_is_tracked_per_category_independently() {
        let trust = TrustModel::with_defaults();
        for _ in 0..10 {
            trust.record(RemediationCategory::RestartWorkload, true, at(0));
        }
        // A different category is unaffected — still at base.
        assert_eq!(
            trust.threshold_for(RemediationCategory::ScaleCompute, at(0)),
            Some(0.95)
        );
        assert_eq!(
            trust.threshold_for(RemediationCategory::RestartWorkload, at(0)),
            Some(0.93)
        );
    }

    #[test]
    fn success_after_cooldown_expiry_starts_climbing_again() {
        let trust = TrustModel::with_defaults();
        for _ in 0..10 {
            trust.record(CAT, true, at(0));
        }
        trust.record(CAT, false, at(0)); // 15-min cooldown
                                         // After cooldown, a success should count from zero again.
        trust.record(CAT, true, at(16 * 60));
        assert_eq!(trust.successes(CAT), 1);
        assert!(!trust.is_in_cooldown(CAT, at(16 * 60)));
    }
}
