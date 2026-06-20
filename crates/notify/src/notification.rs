use chrono::{DateTime, Utc};

use sre_common::findings::{Finding, FindingSeverity, InvestigationReport};

/// A platform notification derived from an [`InvestigationReport`].
///
/// Transport-agnostic: each provider renders this into its own wire format
/// (Slack mrkdwn, Teams MessageCard, PagerDuty Events API).
#[derive(Debug, Clone)]
pub struct Notification {
    pub investigation_id: String,
    pub title: String,
    pub summary: String,
    /// Highest finding severity in the report (most severe).
    pub severity: FindingSeverity,
    pub affected_resource: String,
    pub overall_confidence: f64,
    pub escalated: bool,
    pub finding_count: usize,
    /// The most severe finding titles (up to 3), for the notification body.
    pub top_findings: Vec<String>,
    pub occurred_at: DateTime<Utc>,
}

impl Notification {
    /// Build a notification from an investigation report.
    pub fn from_report(report: &InvestigationReport) -> Self {
        let severity = report_severity(report);

        let mut sorted: Vec<&Finding> = report.findings.iter().collect();
        // FindingSeverity derives Ord in declaration order
        // (Critical < High < Medium < Low < Info), so ascending sort puts
        // the most severe findings first.
        sorted.sort_by_key(|f| f.severity);
        let top_findings = sorted
            .iter()
            .take(3)
            .map(|f| format!("[{}] {}", f.severity, f.title))
            .collect();

        let title = if report.escalated {
            format!("SRE investigation escalated — {}", report.affected_resource)
        } else {
            format!(
                "SRE investigation ({severity}) — {}",
                report.affected_resource
            )
        };

        Self {
            investigation_id: report.investigation_id.clone(),
            title,
            summary: report.summary.clone(),
            severity,
            affected_resource: report.affected_resource.clone(),
            overall_confidence: report.overall_confidence,
            escalated: report.escalated,
            finding_count: report.findings.len(),
            top_findings,
            occurred_at: report.completed_at,
        }
    }
}

/// The most severe finding severity in a report (Info if there are none).
///
/// `FindingSeverity` derives `Ord` in declaration order, so `Critical` is the
/// minimum and the *most* severe — hence `.min()`.
pub fn report_severity(report: &InvestigationReport) -> FindingSeverity {
    report
        .findings
        .iter()
        .map(|f| f.severity)
        .min()
        .unwrap_or(FindingSeverity::Info)
}
