use anyhow::Context;
use clap::{Args, Subcommand, ValueEnum};
use harness_core::stack::{
    diff_agent_stack_snapshots, inventory_repository_stack, AgentStackInventoryOptions,
    AgentStackSnapshot, AgentStackSnapshotDiff, AgentStackSnapshotScope,
};
use std::fs;
use std::path::{Path, PathBuf};

#[derive(Subcommand)]
pub enum StackCommand {
    /// Emit a repository-observed Agent Stack snapshot
    Snapshot(StackSnapshotArgs),
    /// Compare two saved Agent Stack snapshots
    Diff(StackDiffArgs),
}

#[derive(Args)]
pub struct StackSnapshotArgs {
    /// Repository root to observe
    #[arg(long, value_name = "PATH")]
    pub root: PathBuf,
    /// Observation scope. Only repository-observed components are supported.
    #[arg(long, value_enum)]
    pub scope: StackScopeArg,
    /// Snapshot JSON output path, or '-' for stdout
    #[arg(long, value_name = "PATH")]
    pub output: PathBuf,
}

#[derive(Args)]
pub struct StackDiffArgs {
    /// Baseline Agent Stack snapshot JSON
    pub baseline: PathBuf,
    /// Candidate Agent Stack snapshot JSON
    pub candidate: PathBuf,
    /// Print JSON instead of compact text
    #[arg(long)]
    pub json: bool,
    /// Diff output path, or '-' for stdout
    #[arg(long, value_name = "PATH")]
    pub output: Option<PathBuf>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, ValueEnum)]
pub enum StackScopeArg {
    Repository,
}

impl StackScopeArg {
    const fn to_snapshot_scope(self) -> AgentStackSnapshotScope {
        match self {
            Self::Repository => AgentStackSnapshotScope::Repository,
        }
    }
}

pub fn run(cmd: StackCommand) -> anyhow::Result<()> {
    match cmd {
        StackCommand::Snapshot(args) => snapshot_stack(args),
        StackCommand::Diff(args) => diff_stack(args),
    }
}

fn snapshot_stack(args: StackSnapshotArgs) -> anyhow::Result<()> {
    match args.scope.to_snapshot_scope() {
        AgentStackSnapshotScope::Repository => {}
    }
    let inventory = inventory_repository_stack(&AgentStackInventoryOptions::new(args.root.clone()))
        .with_context(|| {
            format!(
                "failed to collect repository Agent Stack inventory from {}",
                args.root.display()
            )
        })?;
    let snapshot = AgentStackSnapshot::from_inventory(&inventory)?;
    let rendered = format!("{}\n", serde_json::to_string_pretty(&snapshot)?);
    write_output(&rendered, Some(args.output.as_path()))
}

fn diff_stack(args: StackDiffArgs) -> anyhow::Result<()> {
    let baseline = read_snapshot(&args.baseline)?;
    let candidate = read_snapshot(&args.candidate)?;
    let diff = diff_agent_stack_snapshots(&baseline, &candidate)?;
    let rendered = if args.json {
        format!("{}\n", serde_json::to_string_pretty(&diff)?)
    } else {
        render_stack_diff(&diff)
    };
    write_output(&rendered, args.output.as_deref())
}

fn read_snapshot(path: &Path) -> anyhow::Result<AgentStackSnapshot> {
    let content = fs::read_to_string(path)
        .with_context(|| format!("failed to read Agent Stack snapshot at {}", path.display()))?;
    AgentStackSnapshot::from_json(&content).map_err(|error| {
        anyhow::anyhow!("invalid Agent Stack snapshot {}: {error}", path.display())
    })
}

pub(crate) fn render_stack_diff(diff: &AgentStackSnapshotDiff) -> String {
    let counts = diff.counts();
    let mut output = String::new();
    output.push_str(&format!("Agent Stack diff ({})\n", diff.scope().as_str()));
    output.push_str(&format!(
        "components: added={} removed={} modified={} unchanged={}\n",
        counts.added(),
        counts.removed(),
        counts.modified(),
        counts.unchanged()
    ));
    if diff.changes().is_empty() {
        output.push_str("No component changes\n");
        return output;
    }
    output.push_str("changes:\n");
    for change in diff.changes() {
        output.push_str(&format!(
            "- {} {}",
            change.kind().as_str(),
            change.component_id()
        ));
        if !change.changed_fields().is_empty() {
            let fields = change
                .changed_fields()
                .iter()
                .map(|field| field.as_str())
                .collect::<Vec<_>>()
                .join(",");
            output.push_str(&format!(" fields={fields}"));
        }
        output.push('\n');
    }
    output
}

fn write_output(contents: &str, output: Option<&Path>) -> anyhow::Result<()> {
    match output {
        Some(path) if path != Path::new("-") => {
            if let Some(parent) = path
                .parent()
                .filter(|parent| !parent.as_os_str().is_empty())
            {
                fs::create_dir_all(parent).with_context(|| {
                    format!("failed to create output directory {}", parent.display())
                })?;
            }
            fs::write(path, contents)
                .with_context(|| format!("failed to write output to {}", path.display()))?;
        }
        _ => {
            print!("{contents}");
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::commands::{Cli, Command};
    use clap::Parser;
    use harness_core::stack::{
        AgentStackComponent, AgentStackComponentKind, AgentStackEntryClass, AgentStackFreshness,
        AgentStackObservationClass, AgentStackSelectionState, AgentStackSnapshotEntry,
        AgentStackSource, AgentStackSourceScope, AgentStackTrustLevel, Sha256Digest,
    };

    const HASH_A: &str = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";
    const HASH_B: &str = "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb";

    #[test]
    fn stack_cli_parses_snapshot_and_diff_commands() {
        let cli = Cli::try_parse_from([
            "harness",
            "stack",
            "snapshot",
            "--root",
            ".",
            "--scope",
            "repository",
            "--output",
            "stack.json",
        ])
        .unwrap_or_else(|error| panic!("stack snapshot should parse: {error}"));
        match cli.command {
            Command::Stack {
                cmd: StackCommand::Snapshot(args),
            } => {
                assert_eq!(args.root, PathBuf::from("."));
                assert_eq!(args.scope, StackScopeArg::Repository);
                assert_eq!(args.output, PathBuf::from("stack.json"));
            }
            _ => panic!("expected stack snapshot command"),
        }

        let cli = Cli::try_parse_from([
            "harness",
            "stack",
            "diff",
            "baseline.json",
            "candidate.json",
            "--json",
            "--output",
            "diff.json",
        ])
        .unwrap_or_else(|error| panic!("stack diff should parse: {error}"));
        match cli.command {
            Command::Stack {
                cmd: StackCommand::Diff(args),
            } => {
                assert_eq!(args.baseline, PathBuf::from("baseline.json"));
                assert_eq!(args.candidate, PathBuf::from("candidate.json"));
                assert!(args.json);
                assert_eq!(args.output, Some(PathBuf::from("diff.json")));
            }
            _ => panic!("expected stack diff command"),
        }
    }

    #[test]
    fn stack_snapshot_writes_repository_json_with_boundaries() {
        let tempdir = tempfile::tempdir().expect("tempdir");
        fs::write(tempdir.path().join("AGENTS.md"), "instructions").expect("write AGENTS.md");
        let output = tempdir.path().join("out").join("stack.json");

        snapshot_stack(StackSnapshotArgs {
            root: tempdir.path().to_path_buf(),
            scope: StackScopeArg::Repository,
            output: output.clone(),
        })
        .unwrap_or_else(|error| panic!("snapshot should write: {error}"));

        let snapshot = read_snapshot(&output).expect("snapshot output should parse");
        assert_eq!(snapshot.scope(), AgentStackSnapshotScope::Repository);
        assert!(snapshot
            .observation_boundaries()
            .iter()
            .any(|boundary| boundary.contains("does not collect runtime")));
        assert!(snapshot.components().iter().any(|entry| entry
            .component()
            .source()
            .locator()
            .as_str()
            == "AGENTS.md"));
    }

    #[test]
    fn stack_diff_text_lists_changed_fields() {
        let tempdir = tempfile::tempdir().expect("tempdir");
        let baseline_path = tempdir.path().join("baseline.json");
        let candidate_path = tempdir.path().join("candidate.json");
        let output_path = tempdir.path().join("diff.txt");

        let baseline =
            AgentStackSnapshot::repository(vec![file_entry("AGENTS.md", HASH_A)]).unwrap();
        let candidate =
            AgentStackSnapshot::repository(vec![file_entry("AGENTS.md", HASH_B)]).unwrap();
        fs::write(
            &baseline_path,
            serde_json::to_string_pretty(&baseline).unwrap(),
        )
        .unwrap();
        fs::write(
            &candidate_path,
            serde_json::to_string_pretty(&candidate).unwrap(),
        )
        .unwrap();

        diff_stack(StackDiffArgs {
            baseline: baseline_path,
            candidate: candidate_path,
            json: false,
            output: Some(output_path.clone()),
        })
        .unwrap_or_else(|error| panic!("diff should write: {error}"));

        let rendered = fs::read_to_string(output_path).expect("diff text");
        assert!(rendered.contains("components: added=0 removed=0 modified=1 unchanged=0"));
        assert!(rendered.contains("- modified repository:instructions:AGENTS.md fields=integrity"));
    }

    fn file_entry(locator: &str, integrity: &str) -> AgentStackSnapshotEntry {
        AgentStackSnapshotEntry::new(
            AgentStackComponent::new(
                AgentStackComponentKind::Instructions,
                AgentStackSource::new(AgentStackSourceScope::Repository, locator).unwrap(),
                AgentStackObservationClass::RepositoryObserved,
                AgentStackSelectionState::Discovered,
                AgentStackTrustLevel::RepositoryObserved,
                AgentStackFreshness::Fresh,
            )
            .unwrap()
            .with_integrity(Some(Sha256Digest::parse(integrity).unwrap())),
            AgentStackEntryClass::RegularFile {
                unix_executable: Some(false),
            },
        )
    }
}
