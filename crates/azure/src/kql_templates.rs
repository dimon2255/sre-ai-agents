/// Parameterized KQL query templates (ADR-005).
///
/// Every template MUST include:
/// - A time filter (`| where TimeGenerated > ago(...)`)
/// - A row limit (`| take N`)
///
/// Parameters are interpolated into known-safe templates.
/// No freeform KQL generation is allowed in Phase 1.

/// Maximum allowed timespan in minutes (24 hours)
pub const MAX_TIMESPAN_MINUTES: u32 = 1440;

/// Default row limit
pub const DEFAULT_MAX_ROWS: u32 = 1000;

/// Clamp timespan to safe range
fn safe_timespan(minutes: u32) -> u32 {
    minutes.min(MAX_TIMESPAN_MINUTES).max(1)
}

/// Sanitize a resource name to prevent KQL injection.
/// Allows alphanumeric, hyphens, underscores, dots, and slashes.
fn sanitize_resource_name(name: &str) -> String {
    name.chars()
        .filter(|c| c.is_alphanumeric() || *c == '-' || *c == '_' || *c == '.' || *c == '/')
        .collect()
}

// ─── LogsAgent Templates ───

/// Error spike analysis — identifies exception types and their frequency
pub fn error_spike(resource_name: &str, timespan_min: u32, max_rows: u32) -> String {
    let resource = sanitize_resource_name(resource_name);
    let ts = safe_timespan(timespan_min);
    let rows = max_rows.min(DEFAULT_MAX_ROWS);

    format!(
        r#"AppExceptions
| where TimeGenerated > ago({ts}m)
| where AppRoleName == "{resource}"
| summarize ErrorCount=count(),
            DistinctErrors=dcount(ExceptionType)
            by bin(TimeGenerated, 1m), ExceptionType, ProblemId
| order by ErrorCount desc
| take {rows}"#
    )
}

/// Error details — get recent exception messages and stack traces
pub fn error_details(resource_name: &str, timespan_min: u32, max_rows: u32) -> String {
    let resource = sanitize_resource_name(resource_name);
    let ts = safe_timespan(timespan_min);
    let rows = max_rows.min(50); // Limit stack trace results

    format!(
        r#"AppExceptions
| where TimeGenerated > ago({ts}m)
| where AppRoleName == "{resource}"
| project TimeGenerated, ExceptionType, OuterMessage,
          InnermostMessage, ProblemId, SeverityLevel
| order by TimeGenerated desc
| take {rows}"#
    )
}

/// Log pattern analysis — find most frequent log messages
pub fn log_patterns(resource_name: &str, timespan_min: u32, max_rows: u32) -> String {
    let resource = sanitize_resource_name(resource_name);
    let ts = safe_timespan(timespan_min);
    let rows = max_rows.min(100);

    format!(
        r#"AppTraces
| where TimeGenerated > ago({ts}m)
| where AppRoleName == "{resource}"
| where SeverityLevel >= 3
| summarize Count=count() by Message=substring(Message, 0, 200), SeverityLevel
| order by Count desc
| take {rows}"#
    )
}

// ─── MetricsAgent Templates ───

/// Latency analysis — p50/p95/p99 percentiles, failure rate, request count
pub fn latency_analysis(resource_name: &str, timespan_min: u32) -> String {
    let resource = sanitize_resource_name(resource_name);
    let ts = safe_timespan(timespan_min);

    format!(
        r#"AppRequests
| where TimeGenerated > ago({ts}m)
| where AppRoleName == "{resource}"
| summarize
    P50=percentile(DurationMs, 50),
    P95=percentile(DurationMs, 95),
    P99=percentile(DurationMs, 99),
    FailureRate=countif(Success == false) * 100.0 / count(),
    TotalRequests=count()
    by bin(TimeGenerated, 1m)
| order by TimeGenerated desc
| take 60"#
    )
}

/// Request failure breakdown — which operations are failing
pub fn failure_breakdown(resource_name: &str, timespan_min: u32) -> String {
    let resource = sanitize_resource_name(resource_name);
    let ts = safe_timespan(timespan_min);

    format!(
        r#"AppRequests
| where TimeGenerated > ago({ts}m)
| where AppRoleName == "{resource}"
| where Success == false
| summarize FailureCount=count(),
            AvgDuration=avg(DurationMs)
            by OperationName, ResultCode
| order by FailureCount desc
| take 50"#
    )
}

/// Resource utilization — performance counters (CPU, memory, etc.)
pub fn resource_utilization(resource_name: &str, timespan_min: u32) -> String {
    let resource = sanitize_resource_name(resource_name);
    let ts = safe_timespan(timespan_min);

    format!(
        r#"AppPerformanceCounters
| where TimeGenerated > ago({ts}m)
| where AppRoleName == "{resource}"
| where Name in ("% Processor Time", "Available Bytes", "Private Bytes")
| summarize AvgValue=avg(Value), MaxValue=max(Value)
            by bin(TimeGenerated, 1m), Name
| order by TimeGenerated desc
| take 180"#
    )
}

/// Dependency performance — how downstream services are performing
pub fn dependency_performance(resource_name: &str, timespan_min: u32) -> String {
    let resource = sanitize_resource_name(resource_name);
    let ts = safe_timespan(timespan_min);

    format!(
        r#"AppDependencies
| where TimeGenerated > ago({ts}m)
| where AppRoleName == "{resource}"
| summarize
    CallCount=count(),
    FailureCount=countif(Success == false),
    AvgDuration=avg(DurationMs),
    P95Duration=percentile(DurationMs, 95)
    by DependencyType, Target
| extend FailureRate=FailureCount * 100.0 / CallCount
| order by FailureCount desc
| take 50"#
    )
}

/// Format a timespan value for the KQL REST API timespan parameter
/// e.g., 30 minutes -> "PT30M"
pub fn format_api_timespan(minutes: u32) -> String {
    let ts = safe_timespan(minutes);
    format!("PT{ts}M")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_safe_timespan_clamps_high() {
        assert_eq!(safe_timespan(5000), MAX_TIMESPAN_MINUTES);
    }

    #[test]
    fn test_safe_timespan_clamps_low() {
        assert_eq!(safe_timespan(0), 1);
    }

    #[test]
    fn test_sanitize_resource_name() {
        assert_eq!(sanitize_resource_name("my-app_v2.0"), "my-app_v2.0");
        assert_eq!(
            sanitize_resource_name("'; DROP TABLE--"),
            "DROPTABLE"
        );
    }

    #[test]
    fn test_error_spike_contains_time_filter() {
        let kql = error_spike("payments", 30, 100);
        assert!(kql.contains("ago(30m)"));
        assert!(kql.contains("take 100"));
        assert!(kql.contains("payments"));
    }

    #[test]
    fn test_latency_analysis_contains_time_filter() {
        let kql = latency_analysis("payments", 60);
        assert!(kql.contains("ago(60m)"));
        assert!(kql.contains("payments"));
    }

    #[test]
    fn test_format_api_timespan() {
        assert_eq!(format_api_timespan(30), "PT30M");
        assert_eq!(format_api_timespan(5000), "PT1440M");
    }
}
