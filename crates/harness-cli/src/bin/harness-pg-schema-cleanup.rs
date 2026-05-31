use clap::Parser;
use harness_core::config::HarnessConfig;
use harness_core::db::{
    apply_pg_schema_cleanup, configure_pg_pool_from_server, pg_open_pool, pg_schema_cleanup_plan,
    reap_orphaned_path_schemas, resolve_database_url, PgSchemaCleanupAction, PgSchemaCleanupPlan,
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
        let report = reap_orphaned_path_schemas(&pool, args.confirm_drop).await?;
        if args.json {
            println!("{}", serde_json::to_string_pretty(&report)?);
        } else if report.orphans.is_empty() {
            println!(
                "No orphaned path-derived schemas (scanned {})",
                report.scanned
            );
        } else {
            println!(
                "{} {} orphaned schema(s) of {} scanned:",
                if report.dropped {
                    "Reaped"
                } else {
                    "Would reap"
                },
                report.orphans.len(),
                report.scanned
            );
            for schema in &report.orphans {
                println!("  {schema}");
            }
            if !report.dropped {
                println!("Re-run with --confirm-drop to drop them.");
            }
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
