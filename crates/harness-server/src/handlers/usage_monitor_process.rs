use chrono::{DateTime, Utc};
use serde::Serialize;
use std::collections::BTreeSet;
use std::path::Path;
use std::sync::{Mutex, OnceLock};
use sysinfo::{ProcessRefreshKind, ProcessesToUpdate, System, UpdateKind};

#[derive(Debug, Serialize)]
pub(crate) struct AgentProcess {
    pub(crate) pid: u32,
    pub(crate) name: String,
    pub(crate) agent: &'static str,
    pub(crate) age_secs: u64,
    pub(crate) cpu_pct: f32,
    pub(crate) memory_bytes: u64,
    pub(crate) command_label: &'static str,
}

#[derive(Debug, Default)]
pub(crate) struct ProcessSample {
    pub(crate) processes: Vec<AgentProcess>,
    pub(crate) error_count: u64,
}

static PROCESS_SYSTEM: OnceLock<Mutex<System>> = OnceLock::new();

pub(crate) async fn sample_agent_processes_for_monitor(
    now: DateTime<Utc>,
    attribution_tokens: BTreeSet<String>,
) -> ProcessSample {
    match tokio::task::spawn_blocking(move || try_sample_agent_processes(now, &attribution_tokens))
        .await
    {
        Ok(Ok(processes)) => ProcessSample {
            processes,
            error_count: 0,
        },
        Ok(Err(error)) => {
            tracing::error!("usage_monitor: process sampling failed: {error}");
            ProcessSample {
                processes: Vec::new(),
                error_count: 1,
            }
        }
        Err(error) => {
            tracing::error!("usage_monitor: process sampler task failed: {error}");
            ProcessSample {
                processes: Vec::new(),
                error_count: 1,
            }
        }
    }
}

pub(crate) fn try_sample_agent_processes(
    now: DateTime<Utc>,
    attribution_tokens: &BTreeSet<String>,
) -> Result<Vec<AgentProcess>, &'static str> {
    let process_system = PROCESS_SYSTEM.get_or_init(|| Mutex::new(System::new()));
    let Ok(mut system) = process_system.lock() else {
        return Err("process sampler lock poisoned");
    };
    system.refresh_processes_specifics(
        ProcessesToUpdate::All,
        true,
        ProcessRefreshKind::new()
            .with_cpu()
            .with_memory()
            .with_cmd(UpdateKind::OnlyIfNotSet),
    );
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
            if is_attributed_runtime_process(&command, attribution_tokens) {
                return None;
            }
            Some(AgentProcess {
                pid: pid.as_u32(),
                name,
                agent,
                age_secs: now_ts.saturating_sub(process.start_time()),
                cpu_pct: process.cpu_usage(),
                memory_bytes: process.memory(),
                command_label: command_label(agent, &command),
            })
        })
        .collect::<Vec<_>>();
    processes.sort_by(|a, b| b.age_secs.cmp(&a.age_secs).then_with(|| a.pid.cmp(&b.pid)));
    Ok(processes)
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

fn is_attributed_runtime_process(command: &str, attribution_tokens: &BTreeSet<String>) -> bool {
    attribution_tokens
        .iter()
        .filter(|token| token.len() >= 8)
        .any(|token| command.contains(token))
}

fn command_label(agent: &str, command: &str) -> &'static str {
    if agent == "codex" && command.to_lowercase().contains(" exec ") {
        return "codex exec";
    }
    if agent == "claude" {
        return "claude";
    }
    "codex"
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

    #[test]
    fn attributed_runtime_processes_are_not_external() {
        let mut tokens = BTreeSet::new();
        tokens.insert("runtime-job-123".to_string());
        assert!(is_attributed_runtime_process(
            "codex -p 'Runtime job id: runtime-job-123'",
            &tokens
        ));
        assert!(!is_attributed_runtime_process("codex exec", &tokens));
    }

    #[test]
    fn command_label_does_not_expose_prompt_text() {
        assert_eq!(
            command_label("codex", "codex exec -p 'secret prompt body'"),
            "codex exec"
        );
        assert_eq!(
            command_label("claude", "claude -p 'secret prompt body'"),
            "claude"
        );
    }
}
