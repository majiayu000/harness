use super::{inferred_raw, push_unique, AgentStackCapabilityExtractionConfidence, RawCapability};
use crate::stack::{AgentStackCapability, AgentStackComponent, AgentStackComponentKind};

const MAX_STATIC_LINES: usize = 256;

pub(super) fn extract_static(
    component: &AgentStackComponent,
    locator: &str,
    text: &str,
) -> Vec<RawCapability> {
    if component.kind() != AgentStackComponentKind::Hook {
        return Vec::new();
    }
    let mut by_capability = std::collections::BTreeMap::<&'static str, RawCapability>::new();
    for line in text.lines().take(MAX_STATIC_LINES) {
        let tokens = shell_tokens_outside_quotes(line);
        for capability in classify_command_tokens(&tokens) {
            by_capability.entry(capability.as_str()).or_insert_with(|| {
                inferred_raw(
                    capability,
                    "hook.static_command",
                    format!(
                        "{locator} invokes a command associated with {}",
                        capability.as_str()
                    ),
                    AgentStackCapabilityExtractionConfidence::Low,
                )
            });
        }
    }
    by_capability.into_values().collect()
}

pub(super) fn classify_command_tokens(tokens: &[String]) -> Vec<AgentStackCapability> {
    let mut capabilities = Vec::new();
    let Some(program) = tokens.first().map(|token| command_basename(token)) else {
        return capabilities;
    };
    match program {
        "bash" | "sh" | "zsh" | "fish" | "python" | "python3" | "node" | "ruby" | "perl" => {
            push_unique(&mut capabilities, AgentStackCapability::Shell);
        }
        "curl" | "wget" | "ssh" | "scp" | "rsync" => {
            push_unique(&mut capabilities, AgentStackCapability::Network);
        }
        "rm" | "rmdir" | "unlink" => {
            push_unique(&mut capabilities, AgentStackCapability::Destructive);
            push_unique(&mut capabilities, AgentStackCapability::FileWrite);
        }
        "mv" | "cp" | "touch" | "mkdir" | "tee" | "chmod" | "chown" => {
            push_unique(&mut capabilities, AgentStackCapability::FileWrite);
        }
        "git" => classify_git(tokens, &mut capabilities),
        "gh" => {
            push_unique(&mut capabilities, AgentStackCapability::Network);
            if contains_any(tokens, &["delete", "edit", "merge", "close", "release"]) {
                push_unique(&mut capabilities, AgentStackCapability::ProductionWrite);
            }
        }
        "kubectl" | "helm" | "terraform" | "aws" | "gcloud" | "az" | "docker" => {
            push_unique(&mut capabilities, AgentStackCapability::Network);
            if contains_any(
                tokens,
                &[
                    "apply", "delete", "destroy", "deploy", "push", "release", "update",
                ],
            ) {
                push_unique(&mut capabilities, AgentStackCapability::ProductionWrite);
            }
        }
        _ => {}
    }
    capabilities
}

fn shell_tokens_outside_quotes(line: &str) -> Vec<String> {
    let mut tokens = Vec::new();
    let mut current = String::new();
    let mut quote = None;
    for ch in line.chars() {
        if let Some(active) = quote {
            if ch == active {
                quote = None;
            }
            continue;
        }
        match ch {
            '\'' | '"' => quote = Some(ch),
            '#' => break,
            ch if ch.is_whitespace() || matches!(ch, ';' | '|' | '&' | '(' | ')' | '<' | '>') => {
                push_token(&mut tokens, &mut current);
            }
            _ => current.push(ch),
        }
    }
    push_token(&mut tokens, &mut current);
    while tokens.first().is_some_and(|token| token.contains('=')) {
        tokens.remove(0);
    }
    tokens
}

fn push_token(tokens: &mut Vec<String>, current: &mut String) {
    if !current.is_empty() {
        tokens.push(std::mem::take(current));
    }
}

fn command_basename(token: &str) -> &str {
    token.rsplit('/').next().unwrap_or(token)
}

fn classify_git(tokens: &[String], capabilities: &mut Vec<AgentStackCapability>) {
    if contains_any(tokens, &["push", "pull", "fetch", "clone", "ls-remote"]) {
        push_unique(capabilities, AgentStackCapability::Network);
    }
    if contains_any(tokens, &["push", "reset", "clean", "checkout", "rebase"]) {
        push_unique(capabilities, AgentStackCapability::FileWrite);
    }
    if contains_any(tokens, &["push", "reset", "clean"]) {
        push_unique(capabilities, AgentStackCapability::Destructive);
    }
}

fn contains_any(tokens: &[String], needles: &[&str]) -> bool {
    tokens
        .iter()
        .any(|token| needles.iter().any(|needle| token == needle))
}
