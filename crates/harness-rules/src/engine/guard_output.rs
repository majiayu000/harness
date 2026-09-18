use super::Rule;
use harness_core::types::{RuleId, Severity, Violation};
use std::path::PathBuf;
use std::process::Output;

pub(super) fn parse_guard_output(
    rules: &[Rule],
    output: &Output,
    guard_id: &str,
) -> anyhow::Result<Vec<Violation>> {
    let stdout = String::from_utf8_lossy(&output.stdout);
    let mut violations = Vec::new();
    for line in stdout.lines() {
        // Expected format: FILE:LINE:RULE_ID:MESSAGE
        let parts: Vec<&str> = line.splitn(4, ':').collect();
        if parts.len() >= 4 {
            let rule_id = RuleId::from_str(parts[2].trim());
            let severity = rules
                .iter()
                .find(|rule| rule.id == rule_id)
                .map(|rule| rule.severity)
                .unwrap_or(Severity::Medium);
            violations.push(Violation {
                rule_id,
                file: PathBuf::from(parts[0]),
                line: parts[1].parse().ok(),
                message: parts[3].to_string(),
                severity,
            });
        }
    }

    // Guards print findings on stdout and exit 1. Fail closed only when a
    // non-zero exit produced no parseable finding lines (crash / empty stdout).
    if !output.status.success() && violations.is_empty() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        anyhow::bail!(
            "guard `{guard_id}` exited {}: stderr=[{}] stdout=[{}]",
            output.status,
            stderr.trim(),
            stdout.trim()
        );
    }

    Ok(violations)
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;
    use std::os::unix::process::ExitStatusExt;
    use std::process::ExitStatus;

    fn output(exit_code: i32, stdout: &str, stderr: &str) -> Output {
        Output {
            status: ExitStatus::from_raw(exit_code << 8),
            stdout: stdout.as_bytes().to_vec(),
            stderr: stderr.as_bytes().to_vec(),
        }
    }

    #[test]
    fn successful_guard_with_empty_stdout_is_clean() {
        let violations = parse_guard_output(&[], &output(0, "", ""), "NOOP").unwrap();
        assert!(violations.is_empty());
    }

    #[test]
    fn successful_guard_parses_violation_lines() {
        let rules = [Rule {
            id: RuleId::from_str("SEC-01"),
            title: "test".to_string(),
            severity: Severity::Critical,
            category: harness_core::types::Category::Security,
            paths: Vec::new(),
            description: String::new(),
            fix_pattern: None,
        }];
        let violations = parse_guard_output(
            &rules,
            &output(0, "src/lib.rs:12:SEC-01:bad query\n", ""),
            "SEC",
        )
        .unwrap();
        assert_eq!(violations.len(), 1);
        assert_eq!(violations[0].rule_id.as_str(), "SEC-01");
        assert_eq!(violations[0].severity, Severity::Critical);
        assert_eq!(violations[0].message, "bad query");
    }

    #[test]
    fn failed_guard_with_empty_stdout_is_an_error() {
        let error = parse_guard_output(&[], &output(1, "", "crash"), "CRASH")
            .expect_err("nonzero exit must fail closed");
        let message = error.to_string();
        assert!(message.contains("guard `CRASH`"));
        assert!(message.contains("crash"));
        assert!(message.contains("exited"));
    }

    #[test]
    fn failed_guard_with_finding_lines_returns_violations() {
        let rules = [Rule {
            id: RuleId::from_str("RS-03"),
            title: "test".to_string(),
            severity: Severity::High,
            category: harness_core::types::Category::Stability,
            paths: Vec::new(),
            description: String::new(),
            fix_pattern: None,
        }];
        let violations = parse_guard_output(
            &rules,
            &output(1, "src/lib.rs:9:RS-03:unwrap in library code\n", ""),
            "RS-03",
        )
        .unwrap();
        assert_eq!(violations.len(), 1);
        assert_eq!(violations[0].rule_id.as_str(), "RS-03");
        assert_eq!(violations[0].severity, Severity::High);
        assert_eq!(violations[0].message, "unwrap in library code");
    }
}
