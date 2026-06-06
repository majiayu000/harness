use harness_eval::{
    summarize_pr_repair_benchmark, PrRepairBenchmarkCase, PrRepairBenchmarkInput, QualitySnapshot,
};
use serde_json::Value;
use std::collections::HashSet;
use std::env;
use std::fs;
use std::path::{Path, PathBuf};

#[derive(Default)]
struct Args {
    suite: String,
    snapshots: Vec<PathBuf>,
    cases: Vec<(String, PathBuf)>,
    output: PathBuf,
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

    for path in args.snapshots {
        let case_id = case_id_from_path(&path)?;
        push_case(&mut cases, &mut seen_case_ids, case_id, &path)?;
    }
    for (case_id, path) in args.cases {
        push_case(&mut cases, &mut seen_case_ids, case_id, &path)?;
    }

    let summary = summarize_pr_repair_benchmark(PrRepairBenchmarkInput {
        suite: args.suite,
        cases,
    });
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

    if parsed.suite.is_empty()
        || parsed.output.as_os_str().is_empty()
        || (parsed.snapshots.is_empty() && parsed.cases.is_empty())
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

fn read_case(case_id: String, path: &Path) -> Result<PrRepairBenchmarkCase, String> {
    let body = fs::read_to_string(path)
        .map_err(|err| format!("failed to read {}: {err}", path.display()))?;
    let snapshot: QualitySnapshot = serde_json::from_str(&body)
        .map_err(|err| format!("failed to parse {}: {err}", path.display()))?;
    Ok(PrRepairBenchmarkCase {
        case_id,
        tags: Vec::new(),
        weight: 1,
        snapshot,
    })
}

fn push_case(
    cases: &mut Vec<PrRepairBenchmarkCase>,
    seen_case_ids: &mut HashSet<String>,
    case_id: String,
    path: &Path,
) -> Result<(), String> {
    if !seen_case_ids.insert(case_id.clone()) {
        return Err(format!("duplicate case ID: {case_id}"));
    }
    cases.push(read_case(case_id, path)?);
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
    "Usage: score_pr_repair_benchmark --suite NAME (--snapshot quality_snapshot.json | --case CASE_ID=quality_snapshot.json)... --output benchmark_summary.json".to_string()
}
