use harness_eval::{pr_repair_eval_input_from_values, score_pr_repair_eval};
use serde_json::Value;
use std::env;
use std::fs;
use std::path::PathBuf;

#[derive(Default)]
struct Args {
    repo: String,
    pr_number: u64,
    baseline: PathBuf,
    final_pr: PathBuf,
    submission: Option<PathBuf>,
    task_detail: Option<PathBuf>,
    baseline_collected_at: String,
    final_collected_at: String,
    input_output: Option<PathBuf>,
    snapshot_output: PathBuf,
}

fn main() {
    if let Err(err) = run() {
        eprintln!("{err}");
        std::process::exit(2);
    }
}

fn run() -> Result<(), String> {
    let args = parse_args(env::args().skip(1))?;
    let baseline = read_json(&args.baseline)?;
    let final_pr = read_json(&args.final_pr)?;
    let submission = args.submission.as_ref().map(read_json).transpose()?;
    let task_detail = args.task_detail.as_ref().map(read_json).transpose()?;
    let input = pr_repair_eval_input_from_values(
        &args.repo,
        args.pr_number,
        &args.baseline_collected_at,
        &args.final_collected_at,
        &baseline,
        &final_pr,
        submission.as_ref(),
        task_detail.as_ref(),
    );
    let snapshot = score_pr_repair_eval(input.clone()).map_err(|err| err.to_string())?;

    if let Some(path) = args.input_output {
        write_json(
            &path,
            &serde_json::to_value(input).map_err(|err| err.to_string())?,
        )?;
    }
    write_json(
        &args.snapshot_output,
        &serde_json::to_value(snapshot).map_err(|err| err.to_string())?,
    )?;
    Ok(())
}

fn parse_args<I>(mut args: I) -> Result<Args, String>
where
    I: Iterator<Item = String>,
{
    let mut parsed = Args::default();
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--repo" => parsed.repo = required_value(&mut args, "--repo")?,
            "--pr" => {
                parsed.pr_number = required_value(&mut args, "--pr")?
                    .parse::<u64>()
                    .map_err(|_| "--pr must be a number".to_string())?;
            }
            "--baseline" => {
                parsed.baseline = PathBuf::from(required_value(&mut args, "--baseline")?)
            }
            "--final" => parsed.final_pr = PathBuf::from(required_value(&mut args, "--final")?),
            "--submission" => {
                parsed.submission = Some(PathBuf::from(required_value(&mut args, "--submission")?));
            }
            "--task-detail" => {
                parsed.task_detail =
                    Some(PathBuf::from(required_value(&mut args, "--task-detail")?));
            }
            "--baseline-collected-at" => {
                parsed.baseline_collected_at =
                    required_value(&mut args, "--baseline-collected-at")?;
            }
            "--final-collected-at" => {
                parsed.final_collected_at = required_value(&mut args, "--final-collected-at")?;
            }
            "--input-output" => {
                parsed.input_output =
                    Some(PathBuf::from(required_value(&mut args, "--input-output")?));
            }
            "--snapshot-output" => {
                parsed.snapshot_output =
                    PathBuf::from(required_value(&mut args, "--snapshot-output")?);
            }
            "-h" | "--help" => {
                return Err(usage());
            }
            _ => return Err(format!("unknown argument: {arg}\n{}", usage())),
        }
    }

    if parsed.repo.is_empty()
        || parsed.pr_number == 0
        || parsed.baseline.as_os_str().is_empty()
        || parsed.final_pr.as_os_str().is_empty()
        || parsed.baseline_collected_at.is_empty()
        || parsed.final_collected_at.is_empty()
        || parsed.snapshot_output.as_os_str().is_empty()
    {
        return Err(usage());
    }

    Ok(parsed)
}

fn required_value<I>(args: &mut I, flag: &str) -> Result<String, String>
where
    I: Iterator<Item = String>,
{
    args.next()
        .filter(|value| !value.trim().is_empty())
        .ok_or_else(|| format!("{flag} requires a value"))
}

fn read_json(path: &PathBuf) -> Result<Value, String> {
    let body = fs::read_to_string(path)
        .map_err(|err| format!("failed to read {}: {err}", path.display()))?;
    serde_json::from_str(&body).map_err(|err| format!("failed to parse {}: {err}", path.display()))
}

fn write_json(path: &PathBuf, value: &Value) -> Result<(), String> {
    let body = serde_json::to_string_pretty(value).map_err(|err| err.to_string())?;
    fs::write(path, format!("{body}\n"))
        .map_err(|err| format!("failed to write {}: {err}", path.display()))
}

fn usage() -> String {
    "Usage: score_pr_repair --repo OWNER/REPO --pr N --baseline baseline_pr.json --final final_pr.json --baseline-collected-at RFC3339 --final-collected-at RFC3339 --snapshot-output quality_snapshot.json [--submission submission.json --task-detail task_detail_final.json --input-output pr_repair_eval_input.json]".to_string()
}
