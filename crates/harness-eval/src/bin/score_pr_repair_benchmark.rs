use harness_eval::{
    summarize_pr_repair_benchmark, EvalRunMode, PrRepairBenchmarkCase, PrRepairBenchmarkInput,
    QualitySnapshot,
};
use serde::Deserialize;
use serde_json::Value;
use std::collections::HashSet;
use std::env;
use std::fs;
use std::path::{Path, PathBuf};

#[derive(Default)]
struct Args {
    suite: String,
    manifests: Vec<PathBuf>,
    snapshots: Vec<PathBuf>,
    cases: Vec<(String, PathBuf)>,
    output: PathBuf,
}

#[derive(Deserialize)]
struct BenchmarkManifest {
    suite: Option<String>,
    cases: Vec<BenchmarkManifestCase>,
}

#[derive(Deserialize)]
struct BenchmarkManifestCase {
    #[serde(alias = "id")]
    case_id: String,
    #[serde(alias = "path", alias = "snapshot_path")]
    snapshot: PathBuf,
    #[serde(default)]
    tags: Vec<String>,
    #[serde(default = "default_manifest_case_weight")]
    weight: u32,
}

fn main() {
    if let Err(err) = run() {
        eprintln!("{err}");
        std::process::exit(2);
    }
}

fn run() -> Result<(), String> {
    let args = parse_args(env::args().skip(1))?;
    let mut cases = Vec::new();
    let mut seen_case_ids = HashSet::new();
    let mut manifest_suite = None::<String>;

    for path in args.manifests {
        let manifest = read_manifest(&path)?;
        if args.suite.is_empty() {
            merge_manifest_suite(&mut manifest_suite, manifest.suite.as_deref(), &path)?;
        }
        for case in manifest.cases {
            let snapshot_path = resolve_manifest_snapshot_path(&path, &case.snapshot);
            push_case_with_metadata(
                &mut cases,
                &mut seen_case_ids,
                case.case_id,
                &snapshot_path,
                case.tags,
                case.weight,
            )?;
        }
    }

    for path in args.snapshots {
        let case_id = case_id_from_path(&path)?;
        push_case(&mut cases, &mut seen_case_ids, case_id, &path)?;
    }
    for (case_id, path) in args.cases {
        push_case(&mut cases, &mut seen_case_ids, case_id, &path)?;
    }

    let suite = if args.suite.is_empty() {
        manifest_suite
            .ok_or_else(|| "--suite is required when no manifest suite is provided".to_string())?
    } else {
        args.suite
    };

    let summary = summarize_pr_repair_benchmark(PrRepairBenchmarkInput { suite, cases });
    write_json(
        &args.output,
        &serde_json::to_value(summary).map_err(|err| err.to_string())?,
    )
}

fn parse_args<I>(mut args: I) -> Result<Args, String>
where
    I: Iterator<Item = String>,
{
    let mut parsed = Args::default();
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--suite" => parsed.suite = required_value(&mut args, "--suite")?,
            "--manifest" | "--suite-manifest" => parsed
                .manifests
                .push(PathBuf::from(required_value(&mut args, "--manifest")?)),
            "--snapshot" => {
                parsed
                    .snapshots
                    .push(PathBuf::from(required_value(&mut args, "--snapshot")?));
            }
            "--case" => parsed
                .cases
                .push(parse_case(&required_value(&mut args, "--case")?)?),
            "--output" => parsed.output = PathBuf::from(required_value(&mut args, "--output")?),
            "-h" | "--help" => return Err(usage()),
            _ => return Err(format!("unknown argument: {arg}\n{}", usage())),
        }
    }

    if parsed.output.as_os_str().is_empty()
        || (parsed.manifests.is_empty() && parsed.snapshots.is_empty() && parsed.cases.is_empty())
    {
        return Err(usage());
    }

    Ok(parsed)
}

fn parse_case(value: &str) -> Result<(String, PathBuf), String> {
    let Some((case_id, path)) = value.split_once('=') else {
        return Err("--case must use CASE_ID=PATH".to_string());
    };
    let case_id = case_id.trim();
    let path = path.trim();
    if case_id.is_empty() || path.is_empty() {
        return Err("--case must use non-empty CASE_ID=PATH".to_string());
    }
    Ok((case_id.to_string(), PathBuf::from(path)))
}

fn required_value<I>(args: &mut I, flag: &str) -> Result<String, String>
where
    I: Iterator<Item = String>,
{
    args.next()
        .filter(|value| !value.trim().is_empty())
        .ok_or_else(|| format!("{flag} requires a value"))
}

fn read_manifest(path: &Path) -> Result<BenchmarkManifest, String> {
    let body = fs::read_to_string(path)
        .map_err(|err| format!("failed to read manifest {}: {err}", path.display()))?;
    serde_json::from_str(&body)
        .map_err(|err| format!("failed to parse manifest {}: {err}", path.display()))
}

fn merge_manifest_suite(
    current: &mut Option<String>,
    suite: Option<&str>,
    path: &Path,
) -> Result<(), String> {
    let Some(suite) = suite.map(str::trim).filter(|suite| !suite.is_empty()) else {
        return Ok(());
    };
    match current {
        Some(existing) if existing != suite => Err(format!(
            "manifest {} uses suite {suite:?}, which conflicts with suite {existing:?}",
            path.display()
        )),
        Some(_) => Ok(()),
        None => {
            *current = Some(suite.to_string());
            Ok(())
        }
    }
}

fn resolve_manifest_snapshot_path(manifest_path: &Path, snapshot: &Path) -> PathBuf {
    if snapshot.is_absolute() {
        return snapshot.to_path_buf();
    }
    manifest_path
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty())
        .unwrap_or_else(|| Path::new("."))
        .join(snapshot)
}

fn read_case(
    case_id: String,
    path: &Path,
    tags: Vec<String>,
    weight: u32,
) -> Result<PrRepairBenchmarkCase, String> {
    let body = fs::read_to_string(path)
        .map_err(|err| format!("failed to read {}: {err}", path.display()))?;
    let raw_snapshot: Value = serde_json::from_str(&body)
        .map_err(|err| format!("failed to parse {}: {err}", path.display()))?;
    match raw_snapshot.get("run_mode").and_then(Value::as_str) {
        Some("live_run") => {}
        Some("collect_only") => {
            return Err(format!(
                "collect-only snapshot {} cannot be used as a live PR repair benchmark case",
                path.display()
            ));
        }
        Some(value) => {
            return Err(format!(
                "snapshot {} has unsupported run_mode {value:?}; expected live_run",
                path.display()
            ));
        }
        None => {
            return Err(format!(
                "snapshot {} must include explicit run_mode=\"live_run\" for live PR repair benchmarks",
                path.display()
            ));
        }
    }
    let snapshot: QualitySnapshot = serde_json::from_value(raw_snapshot)
        .map_err(|err| format!("failed to parse {}: {err}", path.display()))?;
    if snapshot.run_mode != EvalRunMode::LiveRun {
        return Err(format!(
            "snapshot {} must use run_mode=\"live_run\" for live PR repair benchmarks",
            path.display()
        ));
    }
    Ok(PrRepairBenchmarkCase {
        case_id,
        tags,
        weight,
        snapshot,
    })
}

fn push_case(
    cases: &mut Vec<PrRepairBenchmarkCase>,
    seen_case_ids: &mut HashSet<String>,
    case_id: String,
    path: &Path,
) -> Result<(), String> {
    push_case_with_metadata(cases, seen_case_ids, case_id, path, Vec::new(), 1)
}

fn push_case_with_metadata(
    cases: &mut Vec<PrRepairBenchmarkCase>,
    seen_case_ids: &mut HashSet<String>,
    case_id: String,
    path: &Path,
    tags: Vec<String>,
    weight: u32,
) -> Result<(), String> {
    if !seen_case_ids.insert(case_id.clone()) {
        return Err(format!("duplicate case ID: {case_id}"));
    }
    cases.push(read_case(case_id, path, tags, weight)?);
    Ok(())
}

fn write_json(path: &Path, value: &Value) -> Result<(), String> {
    let body = serde_json::to_string_pretty(value).map_err(|err| err.to_string())?;
    if let Some(parent) = path
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty())
    {
        fs::create_dir_all(parent)
            .map_err(|err| format!("failed to create directory {}: {err}", parent.display()))?;
    }
    fs::write(path, format!("{body}\n"))
        .map_err(|err| format!("failed to write {}: {err}", path.display()))
}

fn case_id_from_path(path: &Path) -> Result<String, String> {
    path.file_stem()
        .and_then(|value| value.to_str())
        .filter(|value| !value.trim().is_empty())
        .map(ToString::to_string)
        .ok_or_else(|| format!("failed to derive case id from {}", path.display()))
}

fn usage() -> String {
    "Usage: score_pr_repair_benchmark [--suite NAME] (--manifest suite.json | --snapshot quality_snapshot.json | --case CASE_ID=quality_snapshot.json)... --output benchmark_summary.json".to_string()
}

fn default_manifest_case_weight() -> u32 {
    1
}
