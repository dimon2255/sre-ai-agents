//! Command grammar for the ChatOps interface.
//!
//! Deliberately small and forgiving — a chat user types `investigate
//! payments-api sev1`, not JSON. Parsing is a pure function so the grammar is
//! exhaustively unit-tested with no transport.

use sre_common::messages::AlertSeverity;

/// A parsed ChatOps command.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ChatCommand {
    /// Start an investigation for a resource at a severity.
    Investigate {
        resource: String,
        severity: AlertSeverity,
    },
    /// Look up an investigation by id.
    Status { investigation_id: String },
    /// Show usage.
    Help,
    /// Unrecognized input (echoed back with help).
    Unknown { raw: String },
}

/// Parse a free-form chat message into a [`ChatCommand`].
///
/// Accepts an optional leading slash-command token (e.g. `/sre investigate …`),
/// which Slack strips into a separate field but users still type. Verbs have
/// short aliases; severity is optional and defaults to `Sev3`.
pub fn parse_command(input: &str) -> ChatCommand {
    let trimmed = input.trim();
    if trimmed.is_empty() {
        return ChatCommand::Help;
    }

    let mut tokens: Vec<&str> = trimmed.split_whitespace().collect();
    // Drop a leading "/command" token if present (Slack sends it separately).
    if tokens[0].starts_with('/') {
        tokens.remove(0);
        if tokens.is_empty() {
            return ChatCommand::Help;
        }
    }

    let verb = tokens[0].to_lowercase();
    let rest = &tokens[1..];

    match verb.as_str() {
        "investigate" | "inv" | "triage" => match rest.first() {
            Some(resource) => {
                let severity = rest
                    .iter()
                    .skip(1)
                    .find_map(|t| parse_severity(t))
                    .unwrap_or(AlertSeverity::Sev3);
                ChatCommand::Investigate {
                    resource: (*resource).to_string(),
                    severity,
                }
            }
            None => ChatCommand::Unknown {
                raw: trimmed.to_string(),
            },
        },
        "status" | "show" | "get" => match rest.first() {
            Some(id) => ChatCommand::Status {
                // Tolerate Slack's `code`/<links> decoration around the id.
                investigation_id: id
                    .trim_matches(|c| c == '`' || c == '<' || c == '>')
                    .to_string(),
            },
            None => ChatCommand::Unknown {
                raw: trimmed.to_string(),
            },
        },
        "help" | "?" | "h" => ChatCommand::Help,
        _ => ChatCommand::Unknown {
            raw: trimmed.to_string(),
        },
    }
}

/// Parse a severity token. Accepts `sev0`–`sev4`, bare `0`–`4`, and the common
/// English labels. Returns `None` for anything else (so the caller can default).
pub fn parse_severity(token: &str) -> Option<AlertSeverity> {
    match token.to_lowercase().as_str() {
        "sev0" | "0" | "critical" | "crit" => Some(AlertSeverity::Sev0),
        "sev1" | "1" | "high" => Some(AlertSeverity::Sev1),
        "sev2" | "2" | "medium" | "error" => Some(AlertSeverity::Sev2),
        "sev3" | "3" | "low" | "warning" | "warn" => Some(AlertSeverity::Sev3),
        "sev4" | "4" | "info" | "informational" => Some(AlertSeverity::Sev4),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_investigate_with_resource_and_severity() {
        assert_eq!(
            parse_command("investigate payments-api sev1"),
            ChatCommand::Investigate {
                resource: "payments-api".to_string(),
                severity: AlertSeverity::Sev1,
            }
        );
    }

    #[test]
    fn investigate_defaults_severity_to_sev3() {
        assert_eq!(
            parse_command("investigate billing-cron"),
            ChatCommand::Investigate {
                resource: "billing-cron".to_string(),
                severity: AlertSeverity::Sev3,
            }
        );
    }

    #[test]
    fn accepts_leading_slash_command_and_aliases() {
        assert_eq!(
            parse_command("/sre inv api critical"),
            ChatCommand::Investigate {
                resource: "api".to_string(),
                severity: AlertSeverity::Sev0,
            }
        );
    }

    #[test]
    fn parses_status_and_strips_decoration() {
        assert_eq!(
            parse_command("status `7c05783a-af59`"),
            ChatCommand::Status {
                investigation_id: "7c05783a-af59".to_string(),
            }
        );
    }

    #[test]
    fn empty_and_help_yield_help() {
        assert_eq!(parse_command("   "), ChatCommand::Help);
        assert_eq!(parse_command("help"), ChatCommand::Help);
        assert_eq!(parse_command("/sre"), ChatCommand::Help);
    }

    #[test]
    fn investigate_without_resource_is_unknown() {
        assert!(matches!(
            parse_command("investigate"),
            ChatCommand::Unknown { .. }
        ));
    }

    #[test]
    fn unrecognized_verb_is_unknown() {
        assert!(matches!(
            parse_command("deploy the thing"),
            ChatCommand::Unknown { .. }
        ));
    }

    #[test]
    fn severity_aliases_map_correctly() {
        assert_eq!(parse_severity("sev0"), Some(AlertSeverity::Sev0));
        assert_eq!(parse_severity("HIGH"), Some(AlertSeverity::Sev1));
        assert_eq!(parse_severity("2"), Some(AlertSeverity::Sev2));
        assert_eq!(parse_severity("info"), Some(AlertSeverity::Sev4));
        assert_eq!(parse_severity("banana"), None);
    }
}
