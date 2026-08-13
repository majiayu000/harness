use super::{inferred_raw, push_unique, AgentStackCapabilityExtractionConfidence, RawCapability};
use crate::stack::{AgentStackCapability, AgentStackComponent, AgentStackComponentKind};
use std::collections::VecDeque;
#[rustfmt::skip]
pub(super) fn extract_static(component: &AgentStackComponent, locator: &str, text: &str) -> Vec<RawCapability> {
    if component.kind() != AgentStackComponentKind::Hook { return Vec::new(); }
    let mut by_capability = std::collections::BTreeMap::<&'static str, RawCapability>::new();
    let mut quote = None;
    let mut heredocs = VecDeque::<(String, bool)>::new();
    let mut pending_heredocs = VecDeque::new();
    let mut logical_line = String::new();
    for physical_line in text.lines() {
        if let Some((delimiter, allows_tabs)) = heredocs.front() {
            let candidate = if *allows_tabs { physical_line.trim_start_matches('\t') } else { physical_line };
            if candidate == delimiter { heredocs.pop_front(); }
            continue;
        }
        logical_line.push_str(physical_line);
        if logical_line.as_bytes().iter().rev().take_while(|byte| **byte == b'\\').count() % 2 == 1 { logical_line.pop(); continue; }
        let line = logical_line.as_str();
        let next_heredocs = heredoc_delimiters(line, quote);
        let (commands, writes_file) = shell_commands_outside_quotes(line, &mut quote);
        pending_heredocs.extend(next_heredocs);
        if quote.is_none() { heredocs.append(&mut pending_heredocs); }
        for tokens in commands {
            for capability in classify_command_tokens(&tokens) {
                by_capability.entry(capability.as_str()).or_insert_with(|| inferred_raw(
                    capability, "hook.static_command", format!("{locator} invokes a command associated with {}", capability.as_str()), AgentStackCapabilityExtractionConfidence::Low,
                ));
            }
        }
        if writes_file { by_capability.entry(AgentStackCapability::FileWrite.as_str()).or_insert_with(|| inferred_raw(AgentStackCapability::FileWrite, "hook.static_command", format!("{locator} uses an output redirection associated with file_write"), AgentStackCapabilityExtractionConfidence::Low)); }
        logical_line.clear();
    }
    by_capability.into_values().collect()
}
#[rustfmt::skip]
pub(super) fn classify_command_tokens(tokens: &[String]) -> Vec<AgentStackCapability> {
    use AgentStackCapability::{Destructive, FileWrite, Network, Privileged, ProductionWrite, Shell};
    let mut capabilities = Vec::new();
    let Some(program) = tokens.first().map(|token| command_basename(token)) else {
        return capabilities;
    };
    match program {
        "sudo" | "doas" => {
            capabilities.push(Privileged);
            if let Some(start) = wrapped_command_start(tokens) { for capability in classify_command_tokens(&tokens[start..]) { push_unique(&mut capabilities, capability); } }
        }
        "bash" | "sh" | "zsh" | "fish" | "python" | "python3" | "node" | "ruby" | "perl" => capabilities.push(Shell),
        "curl" | "wget" | "ssh" | "scp" | "rsync" => capabilities.push(Network),
        "rm" | "rmdir" | "unlink" => capabilities.extend([Destructive, FileWrite]),
        "mv" | "cp" | "touch" | "mkdir" | "tee" | "chmod" | "chown" => capabilities.push(FileWrite),
        "git" => classify_git(tokens, &mut capabilities),
        "gh" => {
            capabilities.push(Network);
            if contains_any(tokens, &["delete", "edit", "merge", "close", "release"]) { capabilities.push(ProductionWrite); }
        }
        "kubectl" | "helm" | "terraform" | "aws" | "gcloud" | "az" | "docker" => {
            capabilities.push(Network);
            if contains_any(tokens, &["apply", "delete", "destroy", "deploy", "push", "release", "update"]) { capabilities.push(ProductionWrite); }
        }
        _ => {}
    }
    capabilities
}
#[rustfmt::skip]
fn shell_commands_outside_quotes(line: &str, quote: &mut Option<char>) -> (Vec<Vec<String>>, bool) {
    let chars = line.chars().collect::<Vec<_>>();
    let mut commands = Vec::new(); let mut tokens = Vec::new(); let mut current = String::new();
    let mut escaped = false; let mut suppress_carried_content = quote.is_some(); let mut word_started = quote.is_some(); let mut writes_file = false;
    for (index, &ch) in chars.iter().enumerate() {
        if escaped {
            if !suppress_carried_content { current.push(ch); word_started = true; }
            escaped = false; continue;
        }
        if let Some(active) = *quote {
            if ch == active { *quote = None; suppress_carried_content = false; }
            else if active == '"' && ch == '\\' { escaped = true; }
            else if !suppress_carried_content { current.push(ch); }
            continue;
        }
        match ch {
            '\'' | '"' => { *quote = Some(ch); word_started = true; }
            '\\' => escaped = true,
            '#' if !word_started => break,
            '#' => current.push(ch),
            ';' | '|' | '&' | '(' | ')' => { push_token(&mut tokens, &mut current, &mut word_started); push_command(&mut commands, &mut tokens); }
            '>' => { writes_file |= output_redirection_writes_path(&chars, index); push_token(&mut tokens, &mut current, &mut word_started) }
            ch if ch.is_whitespace() || ch == '<' => push_token(&mut tokens, &mut current, &mut word_started),
            _ => { current.push(ch); word_started = true; }
        }
    }
    push_token(&mut tokens, &mut current, &mut word_started); push_command(&mut commands, &mut tokens);
    (commands, writes_file)
}

#[rustfmt::skip]
fn output_redirection_writes_path(chars: &[char], index: usize) -> bool {
    let mut next = index + 1;
    if chars.get(next).is_some_and(|ch| matches!(ch, '>' | '|')) { next += 1; }
    while chars.get(next).is_some_and(|ch| ch.is_whitespace()) { next += 1; }
    if chars.get(next) != Some(&'&') { return next < chars.len(); }
    next += 1; while chars.get(next).is_some_and(|ch| ch.is_whitespace()) { next += 1; }
    let boundary = |at: usize| chars.get(at).is_none_or(|ch| ch.is_whitespace() || ";|&()<>".contains(*ch));
    if chars.get(next) == Some(&'-') && boundary(next + 1) { return false; }
    let start = next; while chars.get(next).is_some_and(|ch| ch.is_ascii_digit()) { next += 1; }
    !(start < next && boundary(next)) && start < chars.len()
}
#[rustfmt::skip]
fn heredoc_delimiters(line: &str, mut quote: Option<char>) -> Vec<(String, bool)> {
    let bytes = line.as_bytes();
    let mut delimiters = Vec::new();
    let mut escaped = false;
    let mut word_started = false;
    let mut index = 0;
    while index + 1 < bytes.len() {
        let byte = bytes[index];
        if escaped {
            escaped = false; word_started = true;
        } else if let Some(active) = quote {
            if byte == active as u8 {
                quote = None;
            } else if active == '"' && byte == b'\\' {
                escaped = true;
            }
        } else {
            match byte {
                b'\\' => { escaped = true; word_started = true; }
                b'\'' | b'"' => { quote = Some(byte as char); word_started = true; }
                b'#' if !word_started => break,
                b'<' if bytes[index + 1] == b'<' && bytes.get(index + 2) == Some(&b'<') => { index += 2; continue; }
                b'<' if bytes[index + 1] == b'<' && bytes.get(index + 2) != Some(&b'<') => {
                    index += 2;
                    let allows_tabs = bytes.get(index) == Some(&b'-');
                    if allows_tabs { index += 1; }
                    while bytes.get(index).is_some_and(u8::is_ascii_whitespace) { index += 1; }
                    let mut value = Vec::new();
                    let mut delimiter_quote = None;
                    while let Some(&byte) = bytes.get(index) {
                        if delimiter_quote.is_none() && (byte.is_ascii_whitespace() || b";|&()<>".contains(&byte)) { break; }
                        match (delimiter_quote, byte) {
                            (Some(active), byte) if byte == active => delimiter_quote = None,
                            (None, b'\'' | b'"') => delimiter_quote = Some(byte),
                            (Some(b'\''), _) => value.push(byte),
                            (Some(b'"'), b'\\') if bytes.get(index + 1).is_some_and(|byte| b"$`\"\\\n".contains(byte)) => { value.push(bytes[index + 1]); index += 1; },
                            (Some(b'"'), b'\\') => value.push(b'\\'),
                            (None, b'\\') => if let Some(&escaped) = bytes.get(index + 1) { value.push(escaped); index += 1; },
                            _ => value.push(byte),
                        }
                        index += 1;
                    }
                    if let Ok(value) = String::from_utf8(value) { delimiters.push((value, allows_tabs)); }
                    continue;
                }
                byte if byte.is_ascii_whitespace() || b";|&()<>".contains(&byte) => word_started = false,
                _ => word_started = true,
            }
        }
        index += 1;
    }
    delimiters
}
#[rustfmt::skip]
fn push_token(tokens: &mut Vec<String>, current: &mut String, word_started: &mut bool) { if *word_started { tokens.push(std::mem::take(current)); *word_started = false; } }

#[rustfmt::skip]
fn push_command(commands: &mut Vec<Vec<String>>, tokens: &mut Vec<String>) {
    let command = std::mem::take(tokens);
    let start = command.iter().position(|token| !is_assignment(token) && !is_shell_control(token)).unwrap_or(command.len());
    if start < command.len() { commands.push(command.into_iter().skip(start).collect()); }
}

#[rustfmt::skip]
fn is_shell_control(token: &str) -> bool {
    matches!(token, "!" | "{" | "}" | "do" | "done" | "elif" | "else" | "fi" | "if" | "then" | "time" | "until" | "while")
}

#[rustfmt::skip]
fn is_assignment(token: &str) -> bool {
    let Some((name, _)) = token.split_once('=') else {
        return false;
    };
    let mut chars = name.chars();
    chars.next().is_some_and(|first| first == '_' || first.is_ascii_alphabetic())
        && chars.all(|ch| ch == '_' || ch.is_ascii_alphanumeric())
}

#[rustfmt::skip]
fn command_basename(token: &str) -> &str { token.rsplit('/').next().unwrap_or(token) }

#[rustfmt::skip]
fn wrapped_command_start(tokens: &[String]) -> Option<usize> {
    let program = tokens.first().map(|token| command_basename(token))?;
    let mut index = 1;
    while let Some(token) = tokens.get(index) {
        if token == "--" { index += 1; break; } else if !token.starts_with('-') { break; }
        index += 1 + usize::from(wrapper_option_takes_next(program, token));
    }
    while tokens.get(index).is_some_and(|token| is_assignment(token)) { index += 1; }
    (index < tokens.len()).then_some(index)
}

#[rustfmt::skip]
fn wrapper_option_takes_next(program: &str, option: &str) -> bool {
    const LONG: &[&str] = &["--user", "--group", "--host", "--prompt", "--close-from", "--command-timeout", "--chroot", "--chdir", "--role", "--type"];
    if option.starts_with("--") { return program == "sudo" && LONG.contains(&option) && !option.contains('='); }
    let value_options = if program == "doas" { "aCu" } else { "CDghpRrtTu" };
    option.strip_prefix('-').and_then(|cluster| cluster.char_indices().find(|(_, ch)| value_options.contains(*ch)).map(|(at, ch)| at + ch.len_utf8() == cluster.len())).unwrap_or(false)
}

#[rustfmt::skip]
fn classify_git(tokens: &[String], capabilities: &mut Vec<AgentStackCapability>) {
    let Some(command) = git_subcommand(tokens) else { return; };
    if ["push", "pull", "fetch", "clone", "ls-remote"].contains(&command) { push_unique(capabilities, AgentStackCapability::Network); }
    if ["add", "am", "apply", "branch", "checkout", "cherry-pick", "clean", "clone", "commit", "fetch", "init", "merge", "mv", "pull", "push", "rebase", "reset", "restore", "revert", "rm", "stash", "submodule", "switch", "tag", "worktree"].contains(&command) { push_unique(capabilities, AgentStackCapability::FileWrite); }
    if ["checkout", "clean", "push", "rebase", "reset", "restore", "rm"].contains(&command) { push_unique(capabilities, AgentStackCapability::Destructive); }
}

#[rustfmt::skip]
fn git_subcommand(tokens: &[String]) -> Option<&str> {
    const TAKES_VALUE: &[&str] = &["-C", "-c", "--git-dir", "--work-tree", "--namespace", "--super-prefix", "--config-env", "--exec-path"];
    let mut index = 1;
    while let Some(token) = tokens.get(index) {
        if token == "--" { return tokens.get(index + 1).map(String::as_str); } else if !token.starts_with('-') { return Some(token.as_str()); }
        index += 1 + usize::from(TAKES_VALUE.contains(&token.as_str()) && !token.contains('='));
    }
    None
}

#[rustfmt::skip]
fn contains_any(tokens: &[String], needles: &[&str]) -> bool { tokens.iter().any(|token| needles.contains(&token.as_str())) }
