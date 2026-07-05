use clap::Parser;
use harness_core::config::HarnessConfig;
use harness_core::db::{
    apply_pg_schema_cleanup, apply_pg_test_schema_cleanup, configure_pg_pool_from_server,
    pg_open_pool, pg_schema_cleanup_plan, pg_test_schema_cleanup_plan,
    reap_orphaned_path_schemas_with_workspace_roots, resolve_database_url, PgSchemaCleanupAction,
    PgSchemaCleanupPlan, PgTestSchemaCleanupPlan,
};
use std::path::PathBuf;

#[derive(Debug, Parser)]
#[command(
    name = "harness-pg-schema-cleanup",
    about = "Plan or apply cleanup for legacy path-derived Postgres schemas"
)]
struct Args {
    /// Config file path.
    #[arg(long)]
    config: Option<PathBuf>,
    /// Drop explicitly named registered cleanup candidates. Defaults to dry-run.
    #[arg(long)]
    apply: bool,
    /// Required with --apply to acknowledge DROP SCHEMA CASCADE.
    #[arg(long)]
    confirm_drop: bool,
    /// Registered drop-candidate schema to drop with --apply. Repeatable.
    #[arg(long = "schema")]
    schema: Vec<String>,
    /// Include unregistered h* schemas in the dry-run report as blocked.
    #[arg(long)]
    include_unregistered: bool,
    /// Explicitly allow an unregistered schema to be dropped with --apply.
    #[arg(long = "allow-unregistered")]
    allow_unregistered: Vec<String>,
    /// Drop registered path-derived schemas whose owning workspace directory no
    /// longer exists. Dry-run unless combined with --confirm-drop.
    #[arg(long)]
    reap_orphans: bool,
    /// Dry-run known Harness test schemas such as event_store_*_test_*.
    #[arg(long)]
    test_schemas: bool,
    /// Explicit test schema to drop with --apply. Repeatable.
    #[arg(long = "test-schema")]
    test_schema: Vec<String>,
    /// With --apply and --confirm-drop, drop every known test schema in the
    /// dry-run plan.
    #[arg(long)]
    drop_test_schemas: bool,
    /// Print JSON output.
    #[arg(long)]
    json: bool,
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let args = Args::parse();
    let mut config = load_cleanup_config(args.config.as_deref())?;
    config.apply_env_overrides()?;
    configure_pg_pool_from_server(&config.server);
    let database_url = resolve_database_url(config.server.database_url.as_deref())?;
    let pool = pg_open_pool(&database_url).await?;

    if args.reap_orphans {
        let workspace_roots = configured_workspace_roots(&config);
        let report = reap_orphaned_path_schemas_with_workspace_roots(
            &pool,
            args.confirm_drop,
            &workspace_roots,
        )
        .await?;
        if args.json {
            println!("{}", serde_json::to_string_pretty(&report)?);
        } else if report.orphans.is_empty() {
            println!(
                "No orphaned path-derived schemas (registered scanned {}, legacy scanned {})",
                report.registered_scanned, report.legacy_scanned
            );
            print_legacy_scan_errors(&report.legacy_scan_errors);
        } else {
            println!(
                "{} {} orphaned schema(s) of {} scanned (registered reaped {}, legacy reaped {}):",
                if report.dropped {
                    "Reaped"
                } else {
                    "Would reap"
                },
                report.orphans.len(),
                report.scanned,
                report.registered_reaped,
                report.legacy_reaped
            );
            for schema in &report.orphans {
                println!("  {schema}");
            }
            print_legacy_scan_errors(&report.legacy_scan_errors);
            if !report.dropped {
                println!("Re-run with --confirm-drop to drop them.");
            }
        }
        pool.close().await;
        return Ok(());
    }

    if args.test_schemas || args.drop_test_schemas || !args.test_schema.is_empty() {
        let plan = pg_test_schema_cleanup_plan(&pool).await?;
        if args.apply {
            if !args.confirm_drop {
                anyhow::bail!("--apply for test schemas requires --confirm-drop");
            }
            let schemas = if args.drop_test_schemas {
                plan.candidates
                    .iter()
                    .map(|candidate| candidate.schema_name.clone())
                    .collect::<Vec<_>>()
            } else {
                args.test_schema.clone()
            };
            if schemas.is_empty() {
                anyhow::bail!(
                    "--apply for test schemas requires --test-schema or --drop-test-schemas"
                );
            }
            let dropped = apply_pg_test_schema_cleanup(&pool, &schemas).await?;
            if args.json {
                println!("{}", serde_json::to_string_pretty(&dropped)?);
            } else if dropped.is_empty() {
                println!("No test schemas dropped");
            } else {
                println!("Dropped {} test schema(s):", dropped.len());
                for result in dropped {
                    println!("  {}", result.schema_name);
                }
            }
        } else if args.json {
            println!("{}", serde_json::to_string_pretty(&plan)?);
        } else {
            print_test_schema_plan(&plan);
        }
        pool.close().await;
        return Ok(());
    }

    if args.apply {
        if !args.confirm_drop {
            anyhow::bail!("--apply requires --confirm-drop");
        }
        if args.schema.is_empty() && args.allow_unregistered.is_empty() {
            anyhow::bail!("--apply requires at least one --schema or --allow-unregistered");
        }
        let dropped =
            apply_pg_schema_cleanup(&pool, &args.schema, &args.allow_unregistered).await?;
        if args.json {
            println!("{}", serde_json::to_string_pretty(&dropped)?);
        } else if dropped.is_empty() {
            println!("No schemas dropped");
        } else {
            println!("Dropped {} schema(s):", dropped.len());
            for result in dropped {
                println!(
                    "  {}{}",
                    result.schema_name,
                    if result.registered {
                        ""
                    } else {
                        " (explicit allowlist)"
                    }
                );
            }
        }
    } else {
        let plan = pg_schema_cleanup_plan(&pool, args.include_unregistered).await?;
        if args.json {
            println!("{}", serde_json::to_string_pretty(&plan)?);
        } else {
            print_plan(&plan);
        }
    }

    pool.close().await;
    Ok(())
}

fn load_cleanup_config(path: Option<&std::path::Path>) -> anyhow::Result<HarnessConfig> {
    if let Some(path) = path {
        let content = std::fs::read_to_string(path)?;
        let mut config: HarnessConfig = toml::from_str(&content)?;
        if let Some(dir) = path.parent() {
            config.rebase_relative_paths(dir);
        }
        return Ok(config);
    }

    if let Some(discovered) = harness_core::config::dirs::find_config_file() {
        let content = std::fs::read_to_string(&discovered)?;
        let mut config: HarnessConfig = toml::from_str(&content)?;
        if let Some(dir) = discovered.parent() {
            config.rebase_relative_paths(dir);
        }
        return Ok(config);
    }

    Ok(HarnessConfig::default())
}

fn configured_workspace_roots(config: &HarnessConfig) -> Vec<PathBuf> {
    if config.workspace.root.as_os_str().is_empty() {
        Vec::new()
    } else {
        vec![config.workspace.root.clone()]
    }
}

fn print_plan(plan: &PgSchemaCleanupPlan) {
    let mut keep = 0usize;
    let mut drop_candidates = 0usize;
    let mut blocked = 0usize;
    for candidate in &plan.candidates {
        match candidate.action {
            PgSchemaCleanupAction::Keep => keep += 1,
            PgSchemaCleanupAction::DropCandidate => drop_candidates += 1,
            PgSchemaCleanupAction::Blocked => blocked += 1,
        }
    }

    println!("Postgres path-derived schema cleanup dry-run");
    println!("  keep:            {keep}");
    println!("  drop candidates: {drop_candidates}");
    println!("  blocked:         {blocked}");

    for candidate in &plan.candidates {
        let action = match candidate.action {
            PgSchemaCleanupAction::Keep => "KEEP",
            PgSchemaCleanupAction::DropCandidate => "DROP_CANDIDATE",
            PgSchemaCleanupAction::Blocked => "BLOCKED",
        };
        println!(
            "{action}\t{}\ttables={}\testimated_rows={}\t{}",
            candidate.schema_name,
            candidate.table_count,
            candidate.estimated_row_count,
            candidate.reason
        );
    }
}

fn print_test_schema_plan(plan: &PgTestSchemaCleanupPlan) {
    println!("Postgres test schema cleanup dry-run");
    println!("  drop candidates: {}", plan.candidates.len());
    for candidate in &plan.candidates {
        println!(
            "DROP_CANDIDATE\t{}\ttables={}\testimated_rows={}\tknown Harness test schema",
            candidate.schema_name, candidate.table_count, candidate.estimated_row_count
        );
    }
    if !plan.candidates.is_empty() {
        println!("Re-run with --test-schemas --apply --confirm-drop --drop-test-schemas to drop all listed schemas.");
        println!(
            "Or pass --test-schema <name> with --apply --confirm-drop to drop selected schemas."
        );
    }
}

fn print_legacy_scan_errors(errors: &[String]) {
    for error in errors {
        println!("Legacy scan skipped: {error}");
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn configured_workspace_roots_omits_empty_root() {
        let mut config = HarnessConfig::default();
        config.workspace.root = PathBuf::new();

        assert!(configured_workspace_roots(&config).is_empty());
    }

    #[test]
    fn configured_workspace_roots_keeps_non_empty_root() {
        let mut config = HarnessConfig::default();
        config.workspace.root = PathBuf::from("/tmp/harness-workspaces");

        assert_eq!(
            configured_workspace_roots(&config),
            vec![PathBuf::from("/tmp/harness-workspaces")]
        );
    }

    #[test]
    fn parses_test_schema_cleanup_flags() {
        let args = Args::parse_from([
            "harness-pg-schema-cleanup",
            "--test-schemas",
            "--apply",
            "--confirm-drop",
            "--test-schema",
            "event_store_scope_test_1783054126968028000_0",
        ]);

        assert!(args.test_schemas);
        assert!(args.apply);
        assert!(args.confirm_drop);
        assert_eq!(
            args.test_schema,
            vec!["event_store_scope_test_1783054126968028000_0"]
        );
    }
}
