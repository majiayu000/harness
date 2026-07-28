//! Lint over runtime store implementations: no function may take ordered row
//! locks out of the order documented in [`super::lock_order`].
//!
//! This is a source lint rather than a runtime check because the invariant is
//! about the *shape* of every transaction, including ones no test happens to
//! exercise concurrently. It reads the same files the compiler does, so it
//! cannot drift from the code the way a comment does.

use super::lock_order::{LOCK_HIERARCHY, LOCK_TAKING_HELPERS};
use std::path::{Path, PathBuf};

/// A `FOR UPDATE` lock found in the source, with enough context to report it.
#[derive(Debug)]
struct LockSite {
    table: String,
    rank: usize,
    line: usize,
    function: String,
}

fn store_dir() -> PathBuf {
    runtime_dir().join("store")
}

fn runtime_dir() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("src/runtime")
}

fn production_runtime_sources() -> Vec<(PathBuf, String)> {
    fn collect(dir: &Path, sources: &mut Vec<(PathBuf, String)>) {
        for entry in std::fs::read_dir(dir).expect("runtime source directory") {
            let path = entry.expect("readable dir entry").path();
            if path.is_dir() {
                if path.file_name().is_some_and(|name| name == "tests") {
                    continue;
                }
                collect(&path, sources);
                continue;
            }
            if path.extension().is_none_or(|ext| ext != "rs") {
                continue;
            }
            let stem = path.file_stem().unwrap_or_default().to_string_lossy();
            if stem == "tests" || stem.ends_with("_tests") {
                continue;
            }
            let source = std::fs::read_to_string(&path).expect("readable runtime source");
            sources.push((path, source));
        }
    }

    let mut sources = Vec::new();
    collect(&runtime_dir(), &mut sources);
    sources
}

/// Rank of a lock hierarchy table, or `None` for tables outside the hierarchy
/// (`workflow_events`, artifacts, and friends, which are ordered by advisory
/// locks or single-row access instead).
fn rank_of(table: &str) -> Option<usize> {
    LOCK_HIERARCHY.iter().position(|known| *known == table)
}

/// Extracts the table a `FOR UPDATE` statement locks by walking back to the
/// nearest `FROM <table>` in the same SQL literal.
fn table_for_lock(lines: &[&str], lock_line: usize) -> Option<String> {
    lines[..=lock_line].iter().rev().find_map(|line| {
        let (_, after) = line.split_once("FROM ")?;
        let table = after
            .split_whitespace()
            .next()?
            .trim_matches(|c: char| !c.is_alphanumeric() && c != '_');
        (!table.is_empty()).then(|| table.to_string())
    })
}

/// Name of the nearest enclosing `fn`, used only for error messages.
fn enclosing_fn(lines: &[&str], line: usize) -> String {
    lines[..=line]
        .iter()
        .rev()
        .find_map(|line| {
            let (_, after) = line.trim_start().split_once("fn ")?;
            Some(after.split(['(', '<']).next()?.to_string())
        })
        .unwrap_or_else(|| "<unknown>".to_string())
}

/// The table a line locks: either directly via `FOR UPDATE`, or indirectly by
/// calling a helper listed in [`LOCK_TAKING_HELPERS`].
fn locked_table(lines: &[&str], index: usize) -> Option<String> {
    if lines[index].contains("FOR UPDATE") {
        return table_for_lock(lines, index);
    }
    LOCK_TAKING_HELPERS
        .iter()
        .find(|(helper, _)| lines[index].contains(&format!("{helper}(")))
        // A helper's own definition is not a call site.
        .filter(|_| !lines[index].trim_start().starts_with("fn "))
        .filter(|_| !lines[index].contains("async fn "))
        .map(|(_, table)| table.to_string())
}

fn lock_sites(source: &str) -> Vec<LockSite> {
    let lines: Vec<&str> = source.lines().collect();
    (0..lines.len())
        .filter_map(|index| {
            let table = locked_table(&lines, index)?;
            let rank = rank_of(&table)?;
            Some(LockSite {
                table,
                rank,
                line: index + 1,
                function: enclosing_fn(&lines, index),
            })
        })
        .collect()
}

/// A lock taken at a shallower level than one already held, without that level
/// having been held first.
#[derive(Debug)]
struct Inversion {
    late: usize,
    late_table: String,
    function: String,
    after_table: String,
    after_line: usize,
}

impl Inversion {
    fn render(&self, path: &Path) -> String {
        format!(
            "{}:{} — `{}` first locks {} only after {} (line {}); \
             the documented order is {}",
            path.file_name().unwrap_or_default().to_string_lossy(),
            self.late,
            self.function,
            self.late_table,
            self.after_table,
            self.after_line,
            LOCK_HIERARCHY.join(" -> "),
        )
    }
}

/// Finds order violations, one function at a time — separate functions are
/// separate transactions.
///
/// Re-locking a row the transaction already holds is a no-op, so a lock only
/// violates the order when its level is acquired *for the first time* after a
/// deeper level was already taken.
fn inversions(sites: &[LockSite]) -> Vec<Inversion> {
    let mut violations = Vec::new();
    let mut function = String::new();
    let mut held: Vec<&LockSite> = Vec::new();

    for site in sites {
        if site.function != function {
            function = site.function.clone();
            held.clear();
        }
        let already_held = held.iter().any(|earlier| earlier.rank == site.rank);
        if !already_held {
            if let Some(deeper) = held.iter().rev().find(|earlier| earlier.rank > site.rank) {
                violations.push(Inversion {
                    late: site.line,
                    late_table: site.table.clone(),
                    function: site.function.clone(),
                    after_table: deeper.table.clone(),
                    after_line: deeper.line,
                });
            }
        }
        held.push(site);
    }
    violations
}

#[test]
fn runtime_store_sources_lock_hierarchy_tables_parent_first() {
    let mut violations = Vec::new();
    let mut checked_files = 0;
    let mut checked_locks = 0;

    for (path, source) in production_runtime_sources() {
        checked_files += 1;

        let sites = lock_sites(&source);
        checked_locks += sites.len();
        violations.extend(
            inversions(&sites)
                .into_iter()
                .map(|violation| violation.render(&path)),
        );
    }

    assert!(checked_files > 0, "lint found no runtime sources to scan");
    assert!(
        checked_locks > 0,
        "lint found no FOR UPDATE sites — the extraction heuristic has drifted"
    );
    assert!(
        violations.is_empty(),
        "row locks acquired out of order:\n{}",
        violations.join("\n")
    );
}

#[test]
fn workflow_event_inserts_are_centralized() {
    let root = runtime_dir();
    let mut writers = production_runtime_sources()
        .into_iter()
        .filter(|(_, source)| source.contains("INSERT INTO workflow_events"))
        .map(|(path, _)| {
            path.strip_prefix(&root)
                .expect("runtime source path")
                .display()
                .to_string()
        })
        .collect::<Vec<_>>();
    writers.sort();

    assert_eq!(
        writers,
        vec!["store/transaction_helpers.rs"],
        "workflow event inserts must use the canonical helper so every path \
         locks the instance row before the event-sequence advisory lock"
    );
}

#[test]
fn event_sequence_helper_locks_parent_before_advisory() {
    let source = std::fs::read_to_string(store_dir().join("transaction_helpers.rs"))
        .expect("transaction helper source");
    let parent_lock = source
        .find("lock_instance_for_event_sequence_tx(tx, workflow_id)")
        .expect("event helper must explicitly lock the workflow instance");
    let advisory_lock = source
        .find("pg_advisory_xact_lock")
        .expect("event helper must serialize sequence allocation");
    let event_insert = source
        .find("INSERT INTO workflow_events")
        .expect("event helper must persist the event");

    assert!(
        parent_lock < advisory_lock && advisory_lock < event_insert,
        "event writes must lock workflow_instances before the sequence advisory lock"
    );
}

#[test]
fn runtime_event_inserts_are_centralized() {
    let root = runtime_dir();
    let mut writers = production_runtime_sources()
        .into_iter()
        .filter(|(_, source)| source.contains("INSERT INTO runtime_events"))
        .map(|(path, _)| {
            path.strip_prefix(&root)
                .expect("runtime source path")
                .display()
                .to_string()
        })
        .collect::<Vec<_>>();
    writers.sort();

    assert_eq!(
        writers,
        vec!["store/runtime_job_leases.rs"],
        "runtime event inserts must use the canonical helper so every path \
         locks the runtime job before the event-sequence advisory lock"
    );
}

#[test]
fn runtime_event_sequence_helper_locks_parent_before_advisory() {
    let source = std::fs::read_to_string(store_dir().join("runtime_job_leases.rs"))
        .expect("runtime job lease helper source");
    let helper = source
        .split_once("pub(crate) async fn append_runtime_event_tx")
        .map(|(_, helper)| helper)
        .expect("canonical runtime event helper");
    let parent_lock = helper
        .find("FROM runtime_jobs WHERE id = $1 FOR KEY SHARE")
        .expect("runtime event helper must explicitly lock the runtime job");
    let advisory_lock = helper
        .find("pg_advisory_xact_lock")
        .expect("runtime event helper must serialize sequence allocation");
    let event_insert = helper
        .find("INSERT INTO runtime_events")
        .expect("runtime event helper must persist the event");

    assert!(
        parent_lock < advisory_lock && advisory_lock < event_insert,
        "runtime event writes must lock runtime_jobs before the sequence advisory lock"
    );
}

#[test]
fn multi_command_runtime_job_cancellation_uses_one_global_id_order() {
    let state_source = std::fs::read_to_string(store_dir().join("runtime_job_state.rs"))
        .expect("runtime job state source");
    let helper = state_source
        .split_once("async fn cancel_unfinished_runtime_jobs_for_commands_tx")
        .map(|(_, helper)| helper)
        .expect("multi-command cancellation helper");
    let all_commands = helper
        .find("command_id = ANY($1::text[])")
        .expect("multi-command cancellation must select all command jobs together");
    let id_order = helper
        .find("ORDER BY id")
        .expect("multi-command cancellation must define a global job ID order");
    let row_lock = helper
        .find("FOR UPDATE")
        .expect("multi-command cancellation must lock selected jobs");
    assert!(
        all_commands < id_order && id_order < row_lock,
        "all unfinished jobs must be selected and locked once in global ID order"
    );

    for caller in ["recovery.rs", "coverage_recovery.rs"] {
        let source =
            std::fs::read_to_string(store_dir().join(caller)).expect("cancellation caller source");
        assert!(
            source.contains("cancel_unfinished_runtime_jobs_for_commands_tx"),
            "{caller} must batch all command cancellations before updating jobs"
        );
    }
}

#[test]
fn lint_detects_an_inverted_order() {
    // Guards the lint itself: the ABBA shape this issue fixed must still be
    // reported if it is reintroduced.
    let source = r#"
        async fn offending_completion(&self) -> anyhow::Result<()> {
            sqlx::query("SELECT id FROM workflow_commands WHERE id = $1 FOR UPDATE");
            sqlx::query("SELECT id FROM workflow_instances WHERE id = $1 FOR UPDATE");
            Ok(())
        }
    "#;
    let sites = lock_sites(source);
    assert_eq!(sites.len(), 2, "both locks should be extracted");
    assert_eq!(sites[0].table, "workflow_commands");
    assert_eq!(sites[1].table, "workflow_instances");
    assert_eq!(sites[0].function, "offending_completion");

    let found = inversions(&sites);
    assert_eq!(found.len(), 1, "the inverted pair must be reported");
    assert_eq!(found[0].late_table, "workflow_instances");
    assert_eq!(found[0].after_table, "workflow_commands");
}

#[test]
fn lint_accepts_the_documented_order() {
    let source = r#"
        async fn ordered_completion(&self) -> anyhow::Result<()> {
            sqlx::query("SELECT id FROM workflow_instances WHERE id = $1 FOR UPDATE");
            sqlx::query("SELECT id FROM workflow_commands WHERE id = $1 FOR UPDATE");
            sqlx::query("SELECT id FROM runtime_jobs WHERE id = $1 FOR UPDATE");
            Ok(())
        }
    "#;
    let sites = lock_sites(source);
    let ranks: Vec<usize> = sites.iter().map(|site| site.rank).collect();
    assert_eq!(ranks, vec![0, 1, 2]);
    assert!(inversions(&sites).is_empty());
}

#[test]
fn lint_allows_relocking_a_level_already_held() {
    // The fixed completion path: the instance is locked first, then re-entered
    // through `apply_runtime_completion_decision_tx` at the end of the same
    // transaction. Re-locking a held row is a no-op, not an inversion.
    let source = r#"
        async fn ordered_completion(&self) -> anyhow::Result<()> {
            transaction_helpers::select_instance_for_update_tx(&mut tx, workflow_id).await?;
            sqlx::query("SELECT id FROM workflow_commands WHERE id = $1 FOR UPDATE");
            sqlx::query("SELECT id FROM runtime_jobs WHERE id = $1 FOR UPDATE");
            runtime_completion::apply_runtime_completion_decision_tx(&mut tx).await?;
            Ok(())
        }
    "#;
    let sites = lock_sites(source);
    assert_eq!(sites.len(), 4, "helper calls count as lock sites");
    assert_eq!(sites[3].table, "workflow_instances");
    assert!(
        inversions(&sites).is_empty(),
        "re-locking the already-held instance must not be reported"
    );
}

#[test]
fn lint_detects_an_inversion_taken_inside_a_helper() {
    // The pre-fix shape: the instance lock only ever happens inside
    // `apply_runtime_completion_decision_tx`, after the job lock.
    let source = r#"
        async fn offending_completion(&self) -> anyhow::Result<()> {
            sqlx::query("SELECT id FROM workflow_commands WHERE id = $1 FOR UPDATE");
            sqlx::query("SELECT id FROM runtime_jobs WHERE id = $1 FOR UPDATE");
            runtime_completion::apply_runtime_completion_decision_tx(&mut tx).await?;
            Ok(())
        }
    "#;
    let found = inversions(&lock_sites(source));
    assert_eq!(found.len(), 1);
    assert_eq!(found[0].late_table, "workflow_instances");
    assert_eq!(found[0].after_table, "runtime_jobs");
}

#[test]
fn lint_ignores_tables_outside_the_hierarchy() {
    let source = r#"
        async fn artifact_write(&self) -> anyhow::Result<()> {
            sqlx::query("SELECT id FROM workflow_artifacts WHERE id = $1 FOR UPDATE");
            Ok(())
        }
    "#;
    assert!(lock_sites(source).is_empty());
}

#[test]
fn lint_does_not_compare_across_functions() {
    let source = r#"
        async fn first(&self) -> anyhow::Result<()> {
            sqlx::query("SELECT id FROM runtime_jobs WHERE id = $1 FOR UPDATE");
            Ok(())
        }
        async fn second(&self) -> anyhow::Result<()> {
            sqlx::query("SELECT id FROM workflow_instances WHERE id = $1 FOR UPDATE");
            Ok(())
        }
    "#;
    let sites = lock_sites(source);
    assert_eq!(sites.len(), 2);
    assert_ne!(
        sites[0].function, sites[1].function,
        "locks in different functions are different transactions"
    );
}
