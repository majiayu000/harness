use super::{inferred_raw, push_unique, AgentStackCapabilityExtractionConfidence, RawCapability};
use crate::stack::{AgentStackCapability, AgentStackComponent, AgentStackComponentKind};

const MAX_STATIC_LINES: usize = 256;
const MAX_STATIC_LINE_BYTES: usize = 4096;

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
        let Some(commands) = shell_commands_outside_quotes(line) else {
            continue;
        };
        for tokens in commands {
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
    }
    by_capability.into_values().collect()
}

pub(super) fn classify_command_tokens(tokens: &[String]) -> Vec<AgentStackCapability> {
    use AgentStackCapability::{Destructive, FileWrite, Network, ProductionWrite, Shell};
    let mut capabilities = Vec::new();
    let Some(program) = tokens.first().map(|token| command_basename(token)) else {
        return capabilities;
    };
    match program {
        "bash" | "sh" | "zsh" | "fish" | "python" | "python3" | "node" | "ruby" | "perl" => {
            capabilities.push(Shell);
        }
        "curl" | "wget" | "ssh" | "scp" | "rsync" => {
            capabilities.push(Network);
        }
        "rm" | "rmdir" | "unlink" => capabilities.extend([Destructive, FileWrite]),
        "mv" | "cp" | "touch" | "mkdir" | "tee" | "chmod" | "chown" => {
            capabilities.push(FileWrite);
        }
        "git" => classify_git(tokens, &mut capabilities),
        "gh" => {
            capabilities.push(Network);
            if contains_any(tokens, &["delete", "edit", "merge", "close", "release"]) {
                capabilities.push(ProductionWrite);
            }
        }
        "kubectl" | "helm" | "terraform" | "aws" | "gcloud" | "az" | "docker" => {
            capabilities.push(Network);
            if contains_any(
                tokens,
                &[
                    "apply", "delete", "destroy", "deploy", "push", "release", "update",
                ],
            ) {
                capabilities.push(ProductionWrite);
            }
        }
        _ => {}
    }
    capabilities
}

fn shell_commands_outside_quotes(line: &str) -> Option<Vec<Vec<String>>> {
    if line.len() > MAX_STATIC_LINE_BYTES {
        return None;
    }
    let mut commands = Vec::new();
    let mut tokens = Vec::new();
    let mut current = String::new();
    let mut quote = None;
    let mut escaped = false;
    for ch in line.chars() {
        if escaped {
            current.push(ch);
            escaped = false;
            continue;
        }
        if let Some(active) = quote {
            if ch == active {
                quote = None;
            } else if active == '"' && ch == '\\' {
                escaped = true;
            } else {
                current.push(ch);
            }
            continue;
        }
        match ch {
            '\'' | '"' => quote = Some(ch),
            '\\' => escaped = true,
            '#' => break,
            ';' | '|' | '&' | '(' | ')' => {
                push_token(&mut tokens, &mut current);
                push_command(&mut commands, &mut tokens);
            }
            ch if ch.is_whitespace() || matches!(ch, '<' | '>') => {
                push_token(&mut tokens, &mut current)
            }
            _ => current.push(ch),
        }
    }
    if quote.is_some() || escaped {
        return None;
    }
    push_token(&mut tokens, &mut current);
    push_command(&mut commands, &mut tokens);
    Some(commands)
}

fn push_token(tokens: &mut Vec<String>, current: &mut String) {
    if !current.is_empty() {
        tokens.push(std::mem::take(current));
    }
}

fn push_command(commands: &mut Vec<Vec<String>>, tokens: &mut Vec<String>) {
    let command = std::mem::take(tokens);
    let start = command
        .iter()
        .position(|token| !is_assignment(token))
        .unwrap_or(command.len());
    if start < command.len() {
        commands.push(command.into_iter().skip(start).collect());
    }
}

fn is_assignment(token: &str) -> bool {
    let Some((name, _)) = token.split_once('=') else {
        return false;
    };
    let mut chars = name.chars();
    chars
        .next()
        .is_some_and(|first| first == '_' || first.is_ascii_alphabetic())
        && chars.all(|ch| ch == '_' || ch.is_ascii_alphanumeric())
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
    tokens.iter().any(|token| needles.contains(&token.as_str()))
}
