use super::{configured_rule_engine, ExecPolicyCommand};
use harness_core::config::HarnessConfig;

pub fn run(cmd: ExecPolicyCommand, config: &HarnessConfig) -> anyhow::Result<()> {
    match cmd {
        ExecPolicyCommand::Check {
            rules,
            requirements,
            resolve_host_executables,
            pretty,
            command,
        } => {
            let mut engine = configured_rule_engine(config);
            let policy_paths = if rules.is_empty() {
                config.rules.exec_policy_paths.clone()
            } else {
                rules
            };
            if policy_paths.is_empty() {
                anyhow::bail!(
                    "no execpolicy rules supplied; pass --rules or set rules.exec_policy_paths"
                );
            }

            engine.load_exec_policy_files(&policy_paths)?;
            if let Some(path) = requirements {
                engine.load_requirements_toml(&path)?;
            } else {
                engine.load_configured_requirements()?;
            }

            let result = engine.check_command_policy(
                &command,
                &harness_rules::exec_policy::MatchOptions {
                    resolve_host_executables,
                },
            );
            let rendered = if pretty {
                serde_json::to_string_pretty(&result)?
            } else {
                serde_json::to_string(&result)?
            };
            println!("{rendered}");
        }
    }
    Ok(())
}
