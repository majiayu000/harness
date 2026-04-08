use axum::{extract::State, http::StatusCode, Json};
use serde::Serialize;
use serde_json::Value;
use std::collections::{BTreeMap, HashMap};
use std::path::{Path, PathBuf};
use std::sync::Arc;

use crate::http::AppState;

#[derive(Debug, Default, Clone, Serialize)]
struct UsageBucket {
    input_tokens: u64,
    output_tokens: u64,
    cache_read_tokens: u64,
    cache_create_tokens: u64,
    request_count: u64,
    session_count: u64,
}

/// Per-hour, per-model token totals for the trend chart.
#[derive(Debug, Default, Clone, Serialize)]
struct HourModelBucket {
    tokens: u64,
    requests: u64,
}

#[derive(Debug, Clone)]
struct ParsedUsageRecord {
    input: u64,
    output: u64,
    cache_read: u64,
    cache_create: u64,
    model: String,
    day: String,
    hour: String,
}

fn home_dir() -> Result<PathBuf, String> {
    std::env::var("HOME")
        .map(PathBuf::from)
        .map_err(|_| "HOME is not set; cannot locate token usage source".to_string())
}

/// GET /api/token-usage — aggregate token usage from Claude CLI session JSONL files.
///
/// Returns hourly request counts, per-model time series, daily totals,
/// and per-task breakdowns. Timestamps are taken from each JSONL entry's
/// `timestamp` field (ISO-8601) for accurate bucketing.
///
/// This endpoint is intentionally strict: malformed source data returns an
/// explicit error response instead of silently producing partial/empty metrics.
pub async fn token_usage(State(state): State<Arc<AppState>>) -> (StatusCode, Json<Value>) {
    let home = match home_dir() {
        Ok(path) => path,
        Err(err) => return error_response(err),
    };

    let claude_projects_dir = home.join(".claude").join("projects");

    // When --no-session-persistence is active, Claude Code does not write
    // session JSONL files to disk. Distinguish two absence cases:
    //
    //   1. Projects directory absent entirely — potentially a misconfiguration
    //      (wrong $HOME, deleted directory, path regression). Emit a warning
    //      so operators can detect it in logs, and include a diagnostic field
    //      in the response so callers can distinguish from genuine empty usage.
    //
    //   2. Directory present but no JSONL files — the expected steady-state
    //      when --no-session-persistence is active. Silently return empty.
    if !claude_projects_dir.is_dir() {
        tracing::warn!(
            path = %claude_projects_dir.display(),
            "token_usage: session projects directory not found; \
             possible misconfiguration (wrong $HOME, deleted directory, \
             or path regression) — returning empty metrics"
        );
        return missing_dir_response();
    }

    let files = match collect_session_files(&claude_projects_dir) {
        Ok(files) => files,
        Err(err) => return error_response(err),
    };

    if files.is_empty() {
        return empty_usage_response();
    }

    let mut by_day: BTreeMap<String, UsageBucket> = BTreeMap::new();
    let mut by_hour: BTreeMap<String, UsageBucket> = BTreeMap::new();
    let mut model_trend: BTreeMap<String, HashMap<String, HourModelBucket>> = BTreeMap::new();
    let mut by_model: HashMap<String, UsageBucket> = HashMap::new();
    let mut totals = UsageBucket::default();
    let mut task_usage: HashMap<String, UsageBucket> = HashMap::new();

    let all_tasks = state.core.tasks.list_all();
    let task_ids: std::collections::HashSet<String> =
        all_tasks.iter().map(|t| t.id.0.clone()).collect();

    for file in &files {
        let task_id = extract_task_id(file);
        let mut sess = UsageBucket::default();

        let content = match std::fs::read_to_string(file) {
            Ok(content) => content,
            Err(err) => {
                return error_response(format!(
                    "failed to read token usage file {}: {err}",
                    file.display()
                ));
            }
        };

        for (idx, line) in content.lines().enumerate() {
            if line.trim().is_empty() {
                continue;
            }
            let entry: Value = match serde_json::from_str(line) {
                Ok(entry) => entry,
                Err(err) => {
                    return error_response(format!(
                        "invalid JSON in {}:{}: {err}",
                        file.display(),
                        idx + 1
                    ));
                }
            };

            let ctx = format!("{}:{}", file.display(), idx + 1);
            let parsed = match parse_usage_record(&entry, &ctx) {
                Ok(Some(record)) => record,
                Ok(None) => continue,
                Err(err) => return error_response(err),
            };

            let total_ctx = parsed.input + parsed.cache_read + parsed.cache_create;

            let hb = by_hour.entry(parsed.hour.clone()).or_default();
            hb.request_count += 1;
            hb.input_tokens += parsed.input;
            hb.output_tokens += parsed.output;
            hb.cache_read_tokens += parsed.cache_read;
            hb.cache_create_tokens += parsed.cache_create;

            let mb = model_trend
                .entry(parsed.hour.clone())
                .or_default()
                .entry(parsed.model.clone())
                .or_default();
            mb.tokens += total_ctx;
            mb.requests += 1;

            let db = by_day.entry(parsed.day).or_default();
            db.request_count += 1;
            db.input_tokens += parsed.input;
            db.output_tokens += parsed.output;
            db.cache_read_tokens += parsed.cache_read;
            db.cache_create_tokens += parsed.cache_create;

            let by_model_bucket = by_model.entry(parsed.model).or_default();
            by_model_bucket.request_count += 1;
            by_model_bucket.input_tokens += parsed.input;
            by_model_bucket.output_tokens += parsed.output;
            by_model_bucket.cache_read_tokens += parsed.cache_read;
            by_model_bucket.cache_create_tokens += parsed.cache_create;

            sess.input_tokens += parsed.input;
            sess.output_tokens += parsed.output;
            sess.cache_read_tokens += parsed.cache_read;
            sess.cache_create_tokens += parsed.cache_create;
            sess.request_count += 1;
        }

        if sess.request_count == 0 {
            continue;
        }
        sess.session_count = 1;

        if let Some(tid) = &task_id {
            if task_ids.contains(tid) {
                accumulate(task_usage.entry(tid.clone()).or_default(), &sess);
            }
        }

        accumulate(&mut totals, &sess);
    }

    let cost = estimate_cost(&totals);

    let task_usage_vec: Vec<Value> = {
        let mut items: Vec<_> = task_usage.into_iter().collect();
        items.sort_by(|a, b| {
            let ca = a.1.input_tokens + a.1.cache_read_tokens + a.1.cache_create_tokens;
            let cb = b.1.input_tokens + b.1.cache_read_tokens + b.1.cache_create_tokens;
            cb.cmp(&ca)
        });
        items
            .into_iter()
            .take(30)
            .map(|(tid, usage)| {
                let task_meta = all_tasks.iter().find(|t| t.id.0 == tid);
                let ctx = usage.input_tokens + usage.cache_read_tokens + usage.cache_create_tokens;
                serde_json::json!({
                    "task_id": tid,
                    "repo": task_meta.and_then(|t| t.repo.clone()),
                    "status": task_meta.map(|t| format!("{:?}", t.status)),
                    "context_tokens": ctx,
                    "output_tokens": usage.output_tokens,
                    "requests": usage.request_count,
                    "cost_usd": estimate_cost(&usage),
                })
            })
            .collect()
    };

    let mut all_models: Vec<String> = by_model.keys().cloned().collect();
    all_models.sort_by(|a, b| {
        let ta = by_model[a].input_tokens + by_model[a].cache_read_tokens;
        let tb = by_model[b].input_tokens + by_model[b].cache_read_tokens;
        tb.cmp(&ta)
    });

    let total_context = totals.input_tokens + totals.cache_read_tokens + totals.cache_create_tokens;

    let body = serde_json::json!({
        "by_day": by_day,
        "by_hour": by_hour,
        "model_trend": model_trend,
        "by_model": by_model,
        "models": all_models,
        "totals": totals,
        "total_context": total_context,
        "total_requests": totals.request_count,
        "estimated_cost_usd": cost,
        "task_usage": task_usage_vec,
        "session_count": totals.session_count,
    });

    (StatusCode::OK, Json(body))
}

fn error_response(message: String) -> (StatusCode, Json<Value>) {
    tracing::error!("token_usage: {message}");
    (
        StatusCode::INTERNAL_SERVER_ERROR,
        Json(serde_json::json!({ "error": message })),
    )
}

/// Return zeroed metrics with 200 OK when the projects directory is missing.
///
/// Unlike `empty_usage_response`, this includes `"source_dir_missing": true`
/// so callers and monitoring can distinguish "directory absent" (potentially a
/// misconfiguration) from "directory present but no sessions yet" (expected
/// in --no-session-persistence environments).
fn missing_dir_response() -> (StatusCode, Json<Value>) {
    let body = serde_json::json!({
        "by_day": {},
        "by_hour": {},
        "model_trend": {},
        "by_model": {},
        "models": [],
        "totals": UsageBucket::default(),
        "total_context": 0,
        "total_requests": 0,
        "estimated_cost_usd": 0.0,
        "task_usage": [],
        "session_count": 0,
        "source_dir_missing": true,
    });
    (StatusCode::OK, Json(body))
}

/// Return zeroed metrics with 200 OK when no session files are available.
///
/// This is the correct response when `--no-session-persistence` is active:
/// agents do not write JSONL files, so an absent or empty projects directory
/// is expected, not an error.
fn empty_usage_response() -> (StatusCode, Json<Value>) {
    let body = serde_json::json!({
        "by_day": {},
        "by_hour": {},
        "model_trend": {},
        "by_model": {},
        "models": [],
        "totals": UsageBucket::default(),
        "total_context": 0,
        "total_requests": 0,
        "estimated_cost_usd": 0.0,
        "task_usage": [],
        "session_count": 0,
    });
    (StatusCode::OK, Json(body))
}

fn parse_usage_record(entry: &Value, ctx: &str) -> Result<Option<ParsedUsageRecord>, String> {
    let Some(usage) = entry.pointer("/message/usage") else {
        return Ok(None);
    };

    let input = required_u64(usage, "input_tokens", ctx)?;
    let output = required_u64(usage, "output_tokens", ctx)?;
    // Cache fields are optional — Codex agent and older Claude CLI builds
    // report all context as input_tokens without a cache breakdown.
    let cache_read = optional_u64(usage, "cache_read_input_tokens");
    let cache_create = optional_u64(usage, "cache_creation_input_tokens");

    let model = required_str_at_pointer(entry, "/message/model", ctx)?.to_string();
    let ts = required_str_field(entry, "timestamp", ctx)?;
    let (day, hour) = parse_timestamp(ts).map_err(|err| format!("{ctx}: {err}"))?;

    Ok(Some(ParsedUsageRecord {
        input,
        output,
        cache_read,
        cache_create,
        model,
        day,
        hour,
    }))
}

fn required_u64(obj: &Value, key: &str, ctx: &str) -> Result<u64, String> {
    obj.get(key)
        .and_then(Value::as_u64)
        .ok_or_else(|| format!("{ctx}: missing or invalid usage field '{key}'"))
}

fn optional_u64(obj: &Value, key: &str) -> u64 {
    obj.get(key).and_then(Value::as_u64).unwrap_or(0)
}

fn required_str_field<'a>(obj: &'a Value, key: &str, ctx: &str) -> Result<&'a str, String> {
    obj.get(key)
        .and_then(Value::as_str)
        .ok_or_else(|| format!("{ctx}: missing or invalid string field '{key}'"))
}

fn required_str_at_pointer<'a>(
    obj: &'a Value,
    pointer: &str,
    ctx: &str,
) -> Result<&'a str, String> {
    obj.pointer(pointer)
        .and_then(Value::as_str)
        .ok_or_else(|| format!("{ctx}: missing or invalid string field at '{pointer}'"))
}

fn accumulate(dst: &mut UsageBucket, src: &UsageBucket) {
    dst.input_tokens += src.input_tokens;
    dst.output_tokens += src.output_tokens;
    dst.cache_read_tokens += src.cache_read_tokens;
    dst.cache_create_tokens += src.cache_create_tokens;
    dst.request_count += src.request_count;
    dst.session_count += src.session_count;
}

/// Estimate API-equivalent cost at Sonnet 4.6 pricing.
fn estimate_cost(usage: &UsageBucket) -> f64 {
    let has_cache = usage.cache_read_tokens > 0 || usage.cache_create_tokens > 0;
    let (eff_input, eff_cache_read) = if has_cache {
        (usage.input_tokens as f64, usage.cache_read_tokens as f64)
    } else {
        let total = usage.input_tokens as f64;
        (total * 0.10, total * 0.90)
    };
    eff_input / 1e6 * 3.0
        + usage.output_tokens as f64 / 1e6 * 15.0
        + eff_cache_read / 1e6 * 0.30
        + usage.cache_create_tokens as f64 / 1e6 * 3.75
}

/// Parse an ISO-8601 timestamp into (YYYY-MM-DD, YYYY-MM-DD HH:00).
fn parse_timestamp(ts: &str) -> Result<(String, String), String> {
    if ts.len() < 13 {
        return Err(format!("invalid timestamp '{ts}': too short"));
    }
    if ts.as_bytes().get(10) != Some(&b'T') {
        return Err(format!("invalid timestamp '{ts}': missing 'T' separator"));
    }
    let day = &ts[..10];
    let hour = &ts[11..13];
    if !hour.chars().all(|c| c.is_ascii_digit()) {
        return Err(format!("invalid timestamp '{ts}': hour is not numeric"));
    }
    Ok((day.to_string(), format!("{day} {hour}:00")))
}

fn collect_session_files(claude_projects_dir: &Path) -> Result<Vec<PathBuf>, String> {
    let mut files = Vec::new();
    let entries = std::fs::read_dir(claude_projects_dir).map_err(|err| {
        format!(
            "failed to read token usage root directory {}: {err}",
            claude_projects_dir.display()
        )
    })?;

    for entry in entries {
        let entry = entry.map_err(|err| {
            format!(
                "failed to iterate token usage root directory {}: {err}",
                claude_projects_dir.display()
            )
        })?;
        let name = entry.file_name();
        if !name.to_string_lossy().contains("harness-workspaces") {
            continue;
        }
        let dir = entry.path();
        if !dir.is_dir() {
            continue;
        }
        collect_jsonl_in_dir(&dir, &mut files)?;

        let sub_entries = std::fs::read_dir(&dir)
            .map_err(|err| format!("failed to read {}: {err}", dir.display()))?;
        for sub in sub_entries {
            let sub = sub.map_err(|err| format!("failed to iterate {}: {err}", dir.display()))?;
            let sub_path = sub.path();
            if sub_path.is_dir() {
                let subagents_dir = sub_path.join("subagents");
                if subagents_dir.is_dir() {
                    collect_jsonl_in_dir(&subagents_dir, &mut files)?;
                }
            }
        }
    }

    Ok(files)
}

fn collect_jsonl_in_dir(dir: &Path, out: &mut Vec<PathBuf>) -> Result<(), String> {
    let entries = std::fs::read_dir(dir).map_err(|err| {
        format!(
            "failed to read token usage directory {}: {err}",
            dir.display()
        )
    })?;

    for entry in entries {
        let entry = entry.map_err(|err| {
            format!(
                "failed to iterate token usage directory {}: {err}",
                dir.display()
            )
        })?;
        let path = entry.path();
        if path.extension().and_then(|e| e.to_str()) == Some("jsonl") {
            out.push(path);
        }
    }

    Ok(())
}

fn extract_task_id(path: &Path) -> Option<String> {
    path.parent()
        .and_then(|p| p.file_name())
        .and_then(|n| n.to_str())
        .and_then(extract_task_uuid)
        .or_else(|| {
            path.parent()
                .and_then(|p| p.parent())
                .and_then(|p| p.parent())
                .and_then(|p| p.file_name())
                .and_then(|n| n.to_str())
                .and_then(extract_task_uuid)
        })
}

fn extract_task_uuid(name: &str) -> Option<String> {
    let marker = "harness-workspaces-";
    let pos = name.rfind(marker)?;
    let after = &name[pos + marker.len()..];
    if after.len() >= 36 && after.as_bytes()[8] == b'-' && after.as_bytes()[13] == b'-' {
        Some(after[..36].to_string())
    } else {
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_timestamp_rejects_invalid_input() {
        assert!(parse_timestamp("invalid").is_err());
        assert!(parse_timestamp("2026-03-26 03:05:56Z").is_err());
    }

    #[test]
    fn parse_usage_record_treats_cache_fields_as_optional() {
        let entry = serde_json::json!({
            "timestamp": "2026-03-26T03:05:56.523Z",
            "message": {
                "model": "claude-sonnet",
                "usage": {
                    "input_tokens": 100,
                    "output_tokens": 20
                }
            }
        });
        let parsed = parse_usage_record(&entry, "test:1").unwrap().unwrap();
        assert_eq!(parsed.cache_read, 0);
        assert_eq!(parsed.cache_create, 0);
        assert_eq!(parsed.input, 100);
    }

    #[test]
    fn parse_usage_record_accepts_valid_usage_line() {
        let entry = serde_json::json!({
            "timestamp": "2026-03-26T03:05:56.523Z",
            "message": {
                "model": "claude-sonnet",
                "usage": {
                    "input_tokens": 100,
                    "output_tokens": 20,
                    "cache_read_input_tokens": 50,
                    "cache_creation_input_tokens": 10
                }
            }
        });

        let parsed = parse_usage_record(&entry, "test:1").unwrap().unwrap();
        assert_eq!(parsed.input, 100);
        assert_eq!(parsed.output, 20);
        assert_eq!(parsed.cache_read, 50);
        assert_eq!(parsed.cache_create, 10);
        assert_eq!(parsed.day, "2026-03-26");
        assert_eq!(parsed.hour, "2026-03-26 03:00");
    }
}
