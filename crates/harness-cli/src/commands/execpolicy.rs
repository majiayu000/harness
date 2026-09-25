use clap::Subcommand;
use harness_core::config::HarnessConfig;
use std::path::PathBuf;

use super::rule::configured_rule_engine;

#[derive(Subcommand)]
pub enum ExecPolicyCommand {
    /// Check a command against Starlark policy rules
    Check {
        /// Paths to policy files (repeatable). Falls back to `rules.exec_policy_paths`.
        #[arg(short = 'r', long = "rules", value_name = "PATH")]
        rules: Vec<PathBuf>,
        /// Optional requirements.toml path. Falls back to `rules.requirements_path` when omitted.
        #[arg(long, value_name = "PATH")]
        requirements: Option<PathBuf>,
        /// Resolve absolute executables against basename rules.
        #[arg(long)]
        resolve_host_executables: bool,
        /// Pretty-print JSON output.
        #[arg(long)]
        pretty: bool,
        /// Command tokens to evaluate.
        #[arg(
            value_name = "COMMAND",
            required = true,
            trailing_var_arg = true,
            allow_hyphen_values = true
        )]
        command: Vec<String>,
    },
}

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
