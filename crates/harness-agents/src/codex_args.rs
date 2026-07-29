use harness_core::config::agents::SandboxMode;
use std::ffi::OsString;

const READ_ONLY_WITH_NETWORK_PROFILE: &str = "harness_read_only_with_network";

pub(super) fn push_codex_sandbox_args(args: &mut Vec<OsString>, mode: SandboxMode) {
    if mode == SandboxMode::ReadOnlyWithNetwork {
        args.push(OsString::from("-c"));
        args.push(OsString::from(format!(
            "default_permissions=\"{READ_ONLY_WITH_NETWORK_PROFILE}\""
        )));
        args.push(OsString::from("-c"));
        args.push(OsString::from(format!(
            "permissions.{READ_ONLY_WITH_NETWORK_PROFILE}.filesystem={{\":minimal\"=\"read\",\":project_roots\"={{\".\"=\"read\"}}}}"
        )));
        args.push(OsString::from("-c"));
        args.push(OsString::from(format!(
            "permissions.{READ_ONLY_WITH_NETWORK_PROFILE}.network.enabled=true"
        )));
        return;
    }

    args.push(OsString::from("-s"));
    args.push(OsString::from(codex_sandbox_mode(mode)));
}

pub(super) fn push_codex_approval_policy_args(args: &mut Vec<OsString>, approval_policy: &str) {
    let approval_policy = escape_codex_config_string(approval_policy);
    args.push(OsString::from("-c"));
    args.push(OsString::from(format!(
        "approval_policy=\"{approval_policy}\""
    )));
}

pub(super) fn push_codex_developer_instructions_args(args: &mut Vec<OsString>, instructions: &str) {
    let instructions = escape_codex_config_string(instructions);
    args.push(OsString::from("-c"));
    args.push(OsString::from(format!(
        "developer_instructions=\"{instructions}\""
    )));
}

pub(super) fn codex_sandbox_mode(mode: SandboxMode) -> &'static str {
    match mode {
        SandboxMode::ReadOnly | SandboxMode::ReadOnlyWithNetwork => "read-only",
        SandboxMode::WorkspaceWrite => "workspace-write",
        SandboxMode::DangerFullAccess => "danger-full-access",
    }
}

fn escape_codex_config_string(value: &str) -> String {
    let mut escaped = String::with_capacity(value.len());
    for ch in value.chars() {
        match ch {
            '\\' => escaped.push_str("\\\\"),
            '"' => escaped.push_str("\\\""),
            '\n' => escaped.push_str("\\n"),
            '\r' => escaped.push_str("\\r"),
            '\t' => escaped.push_str("\\t"),
            '\u{08}' => escaped.push_str("\\b"),
            '\u{0C}' => escaped.push_str("\\f"),
            ch => escaped.push(ch),
        }
    }
    escaped
}

#[cfg(test)]
mod tests {
    use super::push_codex_approval_policy_args;

    #[test]
    fn approval_policy_args_escape_config_string_delimiters() {
        let mut args = Vec::new();

        push_codex_approval_policy_args(&mut args, "ask\"me\\first\nnext");

        let args: Vec<String> = args
            .into_iter()
            .map(|arg| arg.to_string_lossy().into_owned())
            .collect();
        assert_eq!(
            args,
            vec![
                "-c".to_string(),
                "approval_policy=\"ask\\\"me\\\\first\\nnext\"".to_string()
            ]
        );
    }
}
