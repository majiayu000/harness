use regex::Regex;
use std::collections::HashMap;
use std::sync::OnceLock;

static SECRET_PATTERNS: OnceLock<Vec<Regex>> = OnceLock::new();

fn secret_patterns() -> &'static Vec<Regex> {
    SECRET_PATTERNS.get_or_init(|| {
        let raw = [
            r"ghp_[A-Za-z0-9]{36,}",
            r"ghs_[A-Za-z0-9]{36,}",
            r"[0-9a-f]{40}",
            r"Bearer\s+\S+",
            r"sk-[A-Za-z0-9]{32,}",
        ];
        raw.iter().filter_map(|p| Regex::new(p).ok()).collect()
    })
}

/// Redact secret values from a prompt string before persistence.
///
/// Pass 1: env var values ≥ 8 chars, sorted longest-first to avoid partial matches.
/// Pass 2: regex patterns for common token formats.
pub fn redact_secrets(prompt: &str, env_vars: &HashMap<String, String>) -> String {
    let mut sorted_vars: Vec<(&str, &str)> = env_vars
        .iter()
        .filter(|(_, v)| v.len() >= 8)
        .map(|(k, v)| (k.as_str(), v.as_str()))
        .collect();
    sorted_vars.sort_by(|a, b| b.1.len().cmp(&a.1.len()));

    let mut result = prompt.to_string();
    for (key, value) in &sorted_vars {
        result = result.replace(value, &format!("[REDACTED:{key}]"));
    }

    for re in secret_patterns() {
        result = re.replace_all(&result, "[REDACTED]").into_owned();
    }

    result
}

#[cfg(test)]
mod tests {
    use super::*;

    fn env(pairs: &[(&str, &str)]) -> HashMap<String, String> {
        pairs
            .iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect()
    }

    #[test]
    fn known_env_var_is_replaced() {
        let vars = env(&[("GITHUB_TOKEN", "supersecrettoken123")]);
        let result = redact_secrets("token: supersecrettoken123 end", &vars);
        assert_eq!(result, "token: [REDACTED:GITHUB_TOKEN] end");
    }

    #[test]
    fn unknown_secret_passes_through() {
        let result = redact_secrets("hello world", &env(&[]));
        assert_eq!(result, "hello world");
    }

    #[test]
    fn multiple_values_replaced_longest_first() {
        let vars = env(&[("SHORT", "abcdefgh"), ("LONG", "abcdefghijklmnop")]);
        let prompt = "abcdefghijklmnop and abcdefgh";
        let result = redact_secrets(prompt, &vars);
        assert!(result.contains("[REDACTED:LONG]"));
        assert!(result.contains("[REDACTED:SHORT]"));
        assert!(!result.contains("abcdefghijklmnop"));
        assert!(!result.contains("abcdefgh"));
    }

    #[test]
    fn regex_catches_ghp_token() {
        let result = redact_secrets(
            "token=ghp_AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA end",
            &env(&[]),
        );
        assert!(result.contains("[REDACTED]"));
        assert!(!result.contains("ghp_"));
    }

    #[test]
    fn empty_prompt_returns_empty() {
        let result = redact_secrets("", &env(&[("KEY", "somesecret")]));
        assert_eq!(result, "");
    }

    #[test]
    fn short_value_not_redacted() {
        let vars = env(&[("FLAG", "true")]);
        let result = redact_secrets("flag is true", &vars);
        assert_eq!(result, "flag is true");
    }
}
