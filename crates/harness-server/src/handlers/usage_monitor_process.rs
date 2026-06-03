use chrono::{DateTime, Utc};
use serde::Serialize;
use std::path::Path;
use sysinfo::{ProcessesToUpdate, System};

#[derive(Debug, Serialize)]
pub(crate) struct AgentProcess {
    pub(crate) pid: u32,
    pub(crate) name: String,
    pub(crate) agent: &'static str,
    pub(crate) age_secs: u64,
    pub(crate) cpu_pct: f32,
    pub(crate) memory_bytes: u64,
    pub(crate) cwd: Option<String>,
    pub(crate) command: String,
}

pub(crate) fn sample_agent_processes(now: DateTime<Utc>) -> Vec<AgentProcess> {
    let mut system = System::new_all();
    system.refresh_processes(ProcessesToUpdate::All, true);
    let now_ts = now.timestamp().max(0) as u64;
    let mut processes = system
        .processes()
        .iter()
        .filter_map(|(pid, process)| {
            let name = process.name().to_string_lossy().into_owned();
            let command = process
                .cmd()
                .iter()
                .map(|part| part.to_string_lossy())
                .collect::<Vec<_>>()
                .join(" ");
            let executable = process
                .cmd()
                .first()
                .and_then(|part| Path::new(part).file_name())
                .map(|part| part.to_string_lossy().into_owned())
                .unwrap_or_else(|| name.clone());
            let agent = classify_agent_process(&name, &executable, &command)?;
            Some(AgentProcess {
                pid: pid.as_u32(),
                name,
                agent,
                age_secs: now_ts.saturating_sub(process.start_time()),
                cpu_pct: process.cpu_usage(),
                memory_bytes: process.memory(),
                cwd: process.cwd().map(|path| path.display().to_string()),
                command: truncate(&command, 240),
            })
        })
        .collect::<Vec<_>>();
    processes.sort_by(|a, b| b.age_secs.cmp(&a.age_secs).then_with(|| a.pid.cmp(&b.pid)));
    processes
}

fn classify_agent_process(name: &str, executable: &str, command: &str) -> Option<&'static str> {
    let name = name.to_lowercase();
    let executable = executable.to_lowercase();
    let command = command.to_lowercase();

    if name.contains("helper")
        || executable.contains("helper")
        || command.contains("--chrome-native-host")
        || command.contains("claude helper")
    {
        return None;
    }

    if name == "codex"
        || executable == "codex"
        || (command.contains("codex") && command.contains(" exec "))
    {
        return Some("codex");
    }

    if name == "claude" || executable == "claude" {
        return Some("claude");
    }

    None
}

fn truncate(value: &str, max_chars: usize) -> String {
    if value.chars().count() <= max_chars {
        return value.to_string();
    }
    let mut out = value
        .chars()
        .take(max_chars.saturating_sub(1))
        .collect::<String>();
    out.push_str("...");
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn classify_agent_process_accepts_agent_cli_names() {
        assert_eq!(
            classify_agent_process("codex", "codex", "codex exec"),
            Some("codex")
        );
        assert_eq!(
            classify_agent_process("claude", "claude", "claude --dangerously-skip-permissions"),
            Some("claude")
        );
    }

    #[test]
    fn classify_agent_process_rejects_desktop_helpers_and_shells() {
        assert_eq!(
            classify_agent_process(
                "Claude Helper",
                "Claude Helper",
                "/Applications/Claude.app/Contents/Frameworks/Claude Helper"
            ),
            None
        );
        assert_eq!(
            classify_agent_process("zsh", "zsh", "zsh -c 'until cd /tmp/repo && echo claude'"),
            None
        );
        assert_eq!(
            classify_agent_process(
                "2.1.159",
                "2.1.159",
                "/Users/apple/.local/share/claude/versions/2.1.159 --chrome-native-host"
            ),
            None
        );
    }
}
