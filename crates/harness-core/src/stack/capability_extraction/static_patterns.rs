use super::{inferred_raw, push_unique, AgentStackCapabilityExtractionConfidence, RawCapability};
use crate::stack::{AgentStackCapability, AgentStackComponent, AgentStackComponentKind};
use std::collections::VecDeque;

const MAX_COMMAND_CLASSIFICATION_DEPTH: usize = 16;

struct Heredoc {
    delimiter: String,
    allows_tabs: bool,
    expands: bool,
}

#[rustfmt::skip]
pub(super) fn extract_static(component: &AgentStackComponent, locator: &str, text: &str) -> Vec<RawCapability> {
    if component.kind() != AgentStackComponentKind::Hook { return Vec::new(); }
    let mut by_capability = std::collections::BTreeMap::<&'static str, RawCapability>::new();
    let mut quote = None;
    let mut heredocs = VecDeque::<Heredoc>::new();
    let mut pending_heredocs = VecDeque::new();
    let mut logical_line = String::new();
    let mut executable_source = String::new();
    let mut arithmetic_depth = 0usize;
    for physical_line in text.lines() {
        if let Some(heredoc) = heredocs.front() {
            let candidate = if heredoc.allows_tabs { physical_line.trim_start_matches('\t') } else { physical_line };
            if candidate == heredoc.delimiter { heredocs.pop_front(); }
            else if heredoc.expands { executable_source.push_str(physical_line); executable_source.push('\n'); }
            continue;
        }
        executable_source.push_str(physical_line);
        executable_source.push('\n');
        logical_line.push_str(physical_line);
        if logical_line.as_bytes().iter().rev().take_while(|byte| **byte == b'\\').count() % 2 == 1 { logical_line.pop(); continue; }
        let line = logical_line.as_str();
        let next_heredocs = heredoc_delimiters(line, quote, &mut arithmetic_depth);
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
    let (substitution_commands, substitution_writes, substitution_incomplete) = shell_substitution_commands(&executable_source);
    for tokens in substitution_commands {
        for capability in classify_command_tokens(&tokens) {
            by_capability.entry(capability.as_str()).or_insert_with(|| inferred_raw(
                capability, "hook.static_command", format!("{locator} invokes a command associated with {} inside shell command substitution", capability.as_str()), AgentStackCapabilityExtractionConfidence::Low,
            ));
        }
    }
    if substitution_writes { by_capability.entry(AgentStackCapability::FileWrite.as_str()).or_insert_with(|| inferred_raw(AgentStackCapability::FileWrite, "hook.static_command", format!("{locator} uses an output redirection inside shell command substitution associated with file_write"), AgentStackCapabilityExtractionConfidence::Low)); }
    if substitution_incomplete {
        use AgentStackCapability::{Destructive, FileWrite, Network, Privileged, ProductionWrite, Shell};
        for capability in [Destructive, FileWrite, Network, Privileged, ProductionWrite, Shell] {
            by_capability.entry(capability.as_str()).or_insert_with(|| inferred_raw(
                capability, "hook.static_command", format!("{locator} could not complete bounded shell command substitution analysis; {} is retained conservatively", capability.as_str()), AgentStackCapabilityExtractionConfidence::Low,
            ));
        }
    }
    by_capability.into_values().collect()
}
#[rustfmt::skip]
pub(super) fn classify_command_tokens(tokens: &[String]) -> Vec<AgentStackCapability> {
    classify_command_tokens_at_depth(tokens, 0)
}

#[rustfmt::skip]
fn classify_command_tokens_at_depth(tokens: &[String], depth: usize) -> Vec<AgentStackCapability> {
    use AgentStackCapability::{Destructive, FileWrite, Network, Privileged, ProductionWrite, Shell};
    if depth >= MAX_COMMAND_CLASSIFICATION_DEPTH {
        return vec![Destructive, FileWrite, Network, Privileged, ProductionWrite, Shell];
    }
    let mut capabilities = Vec::new();
    let Some(program) = tokens.first().map(|token| command_basename(token)) else {
        return capabilities;
    };
    match program {
        "sudo" | "doas" => {
            capabilities.push(Privileged);
            if let Some(start) = wrapped_command_start(tokens) { for capability in classify_command_tokens_at_depth(&tokens[start..], depth + 1) { push_unique(&mut capabilities, capability); } }
        }
        "bash" | "sh" | "zsh" | "fish" => {
            capabilities.push(Shell);
            if let Some(payload) = shell_command_payload(tokens) {
                classify_embedded_commands(payload, depth + 1, &mut capabilities);
            }
        }
        "python" | "python3" | "node" | "ruby" | "perl" => capabilities.push(Shell),
        "env" => {
            match super::env_command::resolve(tokens) {
                super::env_command::Resolution::Command(command) => for capability in classify_command_tokens_at_depth(&command, depth + 1) { push_unique(&mut capabilities, capability); },
                super::env_command::Resolution::Ambiguous => for capability in [Destructive, FileWrite, Network, Privileged, ProductionWrite, Shell] { push_unique(&mut capabilities, capability); },
                super::env_command::Resolution::None => {},
            }
        }
        "command" | "exec" => if let Some(start) = ordinary_wrapper_start(tokens) { for capability in classify_command_tokens_at_depth(&tokens[start..], depth + 1) { push_unique(&mut capabilities, capability); } },
        "curl" | "wget" | "ssh" | "scp" | "rsync" => capabilities.push(Network),
        "rm" | "rmdir" | "unlink" => capabilities.extend([Destructive, FileWrite]),
        "mv" | "cp" | "touch" | "mkdir" | "tee" | "chmod" | "chown" => capabilities.push(FileWrite),
        "git" => classify_git(tokens, &mut capabilities),
        "gh" => {
            capabilities.push(Network);
            if gh_is_production_write(tokens) { capabilities.push(ProductionWrite); }
        }
        "kubectl" | "helm" | "terraform" | "aws" | "gcloud" | "az" | "docker" => {
            capabilities.push(Network);
            if contains_any(tokens, &["apply", "delete", "destroy", "deploy", "push", "release", "update"]) { capabilities.push(ProductionWrite); }
        }
        _ => {}
    }
    capabilities
}

fn classify_embedded_commands(
    payload: &str,
    depth: usize,
    capabilities: &mut Vec<AgentStackCapability>,
) {
    let mut quote = None;
    for line in payload.lines() {
        let (commands, writes) = shell_commands_outside_quotes(line, &mut quote);
        if writes {
            push_unique(capabilities, AgentStackCapability::FileWrite);
        }
        for command in commands {
            for capability in classify_command_tokens_at_depth(&command, depth) {
                push_unique(capabilities, capability);
            }
        }
    }
    let (commands, writes, incomplete) = shell_substitution_commands(payload);
    if writes {
        push_unique(capabilities, AgentStackCapability::FileWrite);
    }
    for command in commands {
        for capability in classify_command_tokens_at_depth(&command, depth) {
            push_unique(capabilities, capability);
        }
    }
    if incomplete {
        use AgentStackCapability::{
            Destructive, FileWrite, Network, Privileged, ProductionWrite, Shell,
        };
        for capability in [
            Destructive,
            FileWrite,
            Network,
            Privileged,
            ProductionWrite,
            Shell,
        ] {
            push_unique(capabilities, capability);
        }
    }
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
            else if active == '"' && ch == '\\' {
                if chars.get(index + 1).is_some_and(|next| matches!(next, '$' | '`' | '"' | '\\')) { escaped = true; }
                else if !suppress_carried_content { current.push('\\'); }
            }
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

fn shell_substitution_commands(source: &str) -> (Vec<Vec<String>>, bool, bool) {
    const MAX_SUBSTITUTION_DEPTH: usize = 16;
    const MAX_SUBSTITUTIONS: usize = 256;

    fn without_line_continuations(source: &str) -> String {
        let bytes = source.as_bytes();
        let mut output = Vec::with_capacity(bytes.len());
        let mut index = 0;
        while index < bytes.len() {
            if bytes[index] != b'\\' {
                output.push(bytes[index]);
                index += 1;
                continue;
            }
            let start = index;
            while bytes.get(index) == Some(&b'\\') {
                index += 1;
            }
            let count = index - start;
            let newline_len = if bytes.get(index) == Some(&b'\n') {
                1
            } else if bytes.get(index) == Some(&b'\r') && bytes.get(index + 1) == Some(&b'\n') {
                2
            } else {
                0
            };
            let preserved = count - usize::from(newline_len > 0 && count % 2 == 1);
            output.extend_from_slice(&bytes[start..start + preserved]);
            if preserved != count {
                index += newline_len;
            }
        }
        String::from_utf8(output).unwrap_or_else(|_| source.to_owned())
    }

    fn find_parenthesis_close(source: &str, start: usize) -> Option<usize> {
        let bytes = source.as_bytes();
        let mut depth = 1usize;
        let mut quote = None;
        let mut escaped = false;
        let mut word_started = false;
        let mut index = start;
        while index < bytes.len() {
            let byte = bytes[index];
            if escaped {
                escaped = false;
            } else if let Some(active) = quote {
                if byte == active {
                    quote = None;
                } else if active != b'\'' && byte == b'\\' {
                    escaped = true;
                }
            } else {
                match byte {
                    b'\\' => escaped = true,
                    b'\'' | b'"' | b'`' => quote = Some(byte),
                    b'#' if !word_started => return None,
                    b'<' if bytes.get(index + 1) == Some(&b'<')
                        && bytes.get(index + 2) != Some(&b'<') =>
                    {
                        return None
                    }
                    b'(' => depth += 1,
                    b')' => {
                        depth -= 1;
                        if depth == 0 {
                            return Some(index);
                        }
                    }
                    _ => {}
                }
            }
            word_started = !(byte == b'\n'
                || quote.is_none() && (byte.is_ascii_whitespace() || b";|&()<>".contains(&byte)));
            index += 1;
        }
        None
    }

    fn find_backtick_close(source: &str, start: usize) -> Option<usize> {
        let bytes = source.as_bytes();
        let mut escaped = false;
        let mut word_started = false;
        for (offset, &byte) in bytes[start..].iter().enumerate() {
            if escaped {
                escaped = false;
            } else if byte == b'\\' {
                escaped = true;
            } else if byte == b'#' && !word_started
                || byte == b'<'
                    && bytes.get(start + offset + 1) == Some(&b'<')
                    && bytes.get(start + offset + 2) != Some(&b'<')
            {
                return None;
            } else if byte == b'`' {
                return Some(start + offset);
            }
            word_started =
                !(byte == b'\n' || byte.is_ascii_whitespace() || b";|&()<>".contains(&byte));
        }
        None
    }

    fn unescape_nested_backticks(source: &str) -> String {
        let bytes = source.as_bytes();
        let mut output = Vec::with_capacity(bytes.len());
        let mut index = 0;
        while index < bytes.len() {
            if bytes[index] != b'\\' {
                output.push(bytes[index]);
                index += 1;
                continue;
            }
            let start = index;
            while bytes.get(index) == Some(&b'\\') {
                index += 1;
            }
            let count = index - start;
            if bytes.get(index) == Some(&b'`') && count % 2 == 1 {
                output.extend(std::iter::repeat_n(b'\\', count / 2));
                output.push(b'`');
                index += 1;
            } else {
                output.extend_from_slice(&bytes[start..index]);
            }
        }
        String::from_utf8(output).unwrap_or_else(|_| source.to_owned())
    }

    fn collect(source: &str, depth: usize, fragments: &mut Vec<String>) -> bool {
        if depth > MAX_SUBSTITUTION_DEPTH {
            return false;
        }
        let bytes = source.as_bytes();
        let mut quote = None;
        let mut escaped = false;
        let mut word_started = false;
        let mut index = 0;
        while index < bytes.len() {
            let byte = bytes[index];
            if escaped {
                escaped = false;
                word_started = true;
                index += 1;
                continue;
            }
            if quote == Some(b'\'') {
                if byte == b'\'' {
                    quote = None;
                }
                index += 1;
                continue;
            }
            if byte == b'\\' {
                escaped = true;
                index += 1;
                continue;
            }
            if quote.is_none() && byte == b'\'' {
                quote = Some(b'\'');
                word_started = true;
                index += 1;
                continue;
            }
            if byte == b'"' {
                quote = if quote == Some(b'"') {
                    None
                } else {
                    Some(b'"')
                };
                word_started = true;
                index += 1;
                continue;
            }
            if quote.is_none() && byte == b'#' && !word_started {
                index = source[index..]
                    .find('\n')
                    .map_or(bytes.len(), |offset| index + offset + 1);
                word_started = false;
                continue;
            }
            if byte == b'$'
                && bytes.get(index + 1) == Some(&b'(')
                && bytes.get(index + 2) != Some(&b'(')
            {
                let Some(end) = find_parenthesis_close(source, index + 2) else {
                    return false;
                };
                if fragments.len() == MAX_SUBSTITUTIONS {
                    return false;
                }
                let fragment = source[index + 2..end].to_owned();
                fragments.push(fragment.clone());
                if !collect(&fragment, depth + 1, fragments) {
                    return false;
                }
                index = end + 1;
                word_started = true;
                continue;
            }
            if byte == b'`' {
                let Some(end) = find_backtick_close(source, index + 1) else {
                    return false;
                };
                if fragments.len() == MAX_SUBSTITUTIONS {
                    return false;
                }
                let fragment = unescape_nested_backticks(&source[index + 1..end]);
                fragments.push(fragment.clone());
                if !collect(&fragment, depth + 1, fragments) {
                    return false;
                }
                index = end + 1;
                word_started = true;
                continue;
            }
            word_started = !(byte == b'\n'
                || quote.is_none() && (byte.is_ascii_whitespace() || b";|&()<>".contains(&byte)));
            index += 1;
        }
        true
    }

    let normalized = without_line_continuations(source);
    let mut fragments = Vec::new();
    let complete = collect(&normalized, 0, &mut fragments);
    let mut commands = Vec::new();
    let mut writes_file = false;
    for fragment in fragments {
        let mut quote = None;
        for line in fragment.lines() {
            let (mut found, writes) = shell_commands_outside_quotes(line, &mut quote);
            commands.append(&mut found);
            writes_file |= writes;
        }
    }
    (commands, writes_file, !complete)
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
fn heredoc_delimiters(line: &str, mut quote: Option<char>, arithmetic_depth: &mut usize) -> Vec<Heredoc> {
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
                b'(' if bytes[index + 1] == b'(' => { *arithmetic_depth += 1; word_started = true; index += 2; continue; }
                b')' if *arithmetic_depth > 0 && bytes[index + 1] == b')' => { *arithmetic_depth -= 1; word_started = true; index += 2; continue; }
                b'<' if bytes[index + 1] == b'<' && bytes.get(index + 2) == Some(&b'<') => { index += 2; continue; }
                b'<' if *arithmetic_depth == 0 && bytes[index + 1] == b'<' && bytes.get(index + 2) != Some(&b'<') => {
                    index += 2;
                    let allows_tabs = bytes.get(index) == Some(&b'-');
                    if allows_tabs { index += 1; }
                    while bytes.get(index).is_some_and(u8::is_ascii_whitespace) { index += 1; }
                    let mut value = Vec::new();
                    let mut delimiter_quote = None;
                    let mut quoted = false;
                    while let Some(&byte) = bytes.get(index) {
                        if delimiter_quote.is_none() && (byte.is_ascii_whitespace() || b";|&()<>".contains(&byte)) { break; }
                        match (delimiter_quote, byte) {
                            (Some(active), byte) if byte == active => delimiter_quote = None,
                            (None, b'\'' | b'"') => { delimiter_quote = Some(byte); quoted = true; },
                            (Some(b'\''), _) => value.push(byte),
                            (Some(b'"'), b'\\') if bytes.get(index + 1).is_some_and(|byte| b"$`\"\\\n".contains(byte)) => { value.push(bytes[index + 1]); index += 1; },
                            (Some(b'"'), b'\\') => value.push(b'\\'),
                            (None, b'\\') => if let Some(&escaped) = bytes.get(index + 1) { quoted = true; value.push(escaped); index += 1; },
                            _ => value.push(byte),
                        }
                        index += 1;
                    }
                    if let Ok(delimiter) = String::from_utf8(value) { delimiters.push(Heredoc { delimiter, allows_tabs, expands: !quoted }); }
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
pub(super) fn is_assignment(token: &str) -> bool {
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
fn ordinary_wrapper_start(tokens: &[String]) -> Option<usize> {
    let program = tokens.first().map(|token| command_basename(token))?;
    let mut index = 1;
    while let Some(token) = tokens.get(index) {
        if token == "--" { index += 1; break; }
        if !token.starts_with('-') { break; }
        if program == "command" && token.chars().skip(1).any(|flag| matches!(flag, 'v' | 'V')) { return None; }
        let takes_value = program == "exec" && token == "-a";
        index += 1 + usize::from(takes_value);
    }
    (index < tokens.len()).then_some(index)
}

#[rustfmt::skip]
fn shell_command_payload(tokens: &[String]) -> Option<&str> {
    let index = tokens
        .iter()
        .skip(1)
        .position(|token| {
            token == "-c"
                || token.starts_with('-')
                    && !token.starts_with("--")
                    && token[1..].contains('c')
        })?
        + 1;
    tokens.get(index + 1).map(String::as_str)
}

fn gh_is_production_write(tokens: &[String]) -> bool {
    const GROUPS: &[&str] = &[
        "alias",
        "api",
        "auth",
        "browse",
        "cache",
        "codespace",
        "completion",
        "config",
        "extension",
        "gist",
        "gpg-key",
        "issue",
        "label",
        "org",
        "pr",
        "project",
        "release",
        "repo",
        "run",
        "search",
        "secret",
        "ssh-key",
        "status",
        "variable",
        "workflow",
    ];
    let Some(group_index) =
        gh_next_positional(tokens, 1).filter(|index| GROUPS.contains(&tokens[*index].as_str()))
    else {
        return false;
    };
    let group = tokens[group_index].as_str();
    if group == "api" {
        let mut explicit_method = None;
        for (index, token) in tokens.iter().enumerate() {
            if matches!(token.as_str(), "-X" | "--method") {
                explicit_method = tokens.get(index + 1).map(String::as_str);
            } else if let Some(method) = token.strip_prefix("-X").filter(|value| !value.is_empty())
            {
                explicit_method = Some(method.trim_start_matches('='));
            } else if let Some(method) = token.strip_prefix("--method=") {
                explicit_method = Some(method);
            }
        }
        if let Some(method) = explicit_method {
            return !matches!(method.to_ascii_uppercase().as_str(), "GET" | "HEAD");
        }
        return tokens.iter().any(|token| {
            matches!(
                token.as_str(),
                "-f" | "-F" | "--field" | "--raw-field" | "--input"
            ) || token.starts_with("--field=")
                || token.starts_with("-f") && token.len() > 2
                || token.starts_with("-F") && token.len() > 2
                || token.starts_with("--raw-field=")
                || token.starts_with("--input=")
        });
    }
    let Some(action_index) = gh_next_positional(tokens, group_index + 1) else {
        return false;
    };
    let action = tokens[action_index].as_str();
    match group {
        "issue" => !matches!(action, "list" | "status" | "view"),
        "pr" => !matches!(
            action,
            "checks" | "checkout" | "diff" | "list" | "status" | "view"
        ),
        "repo" => !matches!(action, "clone" | "list" | "set-default" | "view"),
        "release" => !matches!(
            action,
            "download" | "list" | "view" | "verify" | "verify-asset"
        ),
        "cache" | "label" | "secret" | "workflow" => !matches!(action, "list" | "view"),
        "run" => !matches!(action, "download" | "list" | "view" | "watch"),
        "variable" => !matches!(action, "get" | "list"),
        "gist" => !matches!(action, "clone" | "list" | "view"),
        "gpg-key" | "ssh-key" => !matches!(action, "list" | "view"),
        "project" => !matches!(action, "field-list" | "item-list" | "list" | "view"),
        "codespace" => !matches!(action, "list" | "logs" | "ports" | "ssh" | "view"),
        _ => false,
    }
}

fn gh_next_positional(tokens: &[String], mut index: usize) -> Option<usize> {
    while let Some(token) = tokens.get(index) {
        if token == "--" {
            return (index + 1 < tokens.len()).then_some(index + 1);
        }
        if !token.starts_with('-') {
            return Some(index);
        }
        let takes_value =
            matches!(token.as_str(), "-R" | "--repo" | "--hostname") && !token.contains('=');
        index += 1 + usize::from(takes_value);
    }
    None
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
