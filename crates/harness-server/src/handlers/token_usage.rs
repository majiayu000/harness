use axum::{extract::State, http::StatusCode, Json};
use serde::Serialize;
use serde_json::Value;
use std::collections::{BTreeMap, HashMap};
use std::path::{Path, PathBuf};
use std::sync::Arc;

use crate::app_state::AppState;

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

fn home_dir() -> PathBuf {
    std::env::var("HOME")
        .ok()
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("."))
}

/// GET /api/token-usage — aggregate token usage from Claude CLI session JSONL files.
///
/// Returns hourly request counts, per-model time series, daily totals,
/// and per-task breakdowns.  Timestamps are taken from each JSONL entry's
/// `timestamp` field (ISO-8601) for accurate bucketing.
pub async fn token_usage(State(state): State<Arc<AppState>>) -> (StatusCode, Json<Value>) {
    let claude_projects_dir = home_dir().join(".claude").join("projects");

    if !claude_projects_dir.is_dir() {
        return (StatusCode::OK, Json(empty_response()));
    }

    let files = collect_session_files(&claude_projects_dir);

    let mut by_day: BTreeMap<String, UsageBucket> = BTreeMap::new();
    let mut by_hour: BTreeMap<String, UsageBucket> = BTreeMap::new();
    // hour -> model -> tokens/requests
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

        let Ok(content) = std::fs::read_to_string(file) else {
            continue;
        };

        for line in content.lines() {
            let Ok(entry) = serde_json::from_str::<Value>(line) else {
                continue;
            };
            let Some(usage) = entry.pointer("/message/usage") else {
                continue;
            };

            let input = usage["input_tokens"].as_u64().unwrap_or(0);
            let output = usage["output_tokens"].as_u64().unwrap_or(0);
            let cache_read = usage["cache_read_input_tokens"].as_u64().unwrap_or(0);
            let cache_create = usage["cache_creation_input_tokens"].as_u64().unwrap_or(0);
            let total_ctx = input + cache_read + cache_create;

            let model = entry
                .pointer("/message/model")
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .to_string();
            // model is consumed per-entry above; no session-level tracking needed.

            // Per-entry timestamp for accurate bucketing.
            let ts = entry["timestamp"].as_str().unwrap_or("");
            let (day, hour) = parse_timestamp(ts);

            // Hourly request count.
            if !hour.is_empty() {
                let hb = by_hour.entry(hour.clone()).or_default();
                hb.request_count += 1;
                hb.input_tokens += input;
                hb.output_tokens += output;
                hb.cache_read_tokens += cache_read;
                hb.cache_create_tokens += cache_create;

                // Model trend (hour × model).
                if !model.is_empty() {
                    let mb = model_trend
                        .entry(hour)
                        .or_default()
                        .entry(model.clone())
                        .or_default();
                    mb.tokens += total_ctx;
                    mb.requests += 1;
                }
            }

            // Daily.
            if !day.is_empty() {
                let db = by_day.entry(day).or_default();
                db.request_count += 1;
                db.input_tokens += input;
                db.output_tokens += output;
                db.cache_read_tokens += cache_read;
                db.cache_create_tokens += cache_create;
            }

            // By model.
            if !model.is_empty() {
                let mb = by_model.entry(model).or_default();
                mb.request_count += 1;
                mb.input_tokens += input;
                mb.output_tokens += output;
                mb.cache_read_tokens += cache_read;
                mb.cache_create_tokens += cache_create;
            }

            sess.input_tokens += input;
            sess.output_tokens += output;
            sess.cache_read_tokens += cache_read;
            sess.cache_create_tokens += cache_create;
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

    // Fill session_count for by_model (each file is one session).
    // Already tracked request_count per model above; session_count
    // would require a separate pass — skip for now.

    let cost = estimate_cost(&totals);

    // Build task_usage with metadata.
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
                    "repo": task_meta.map(|t| t.repo.as_deref().unwrap_or("")),
                    "status": task_meta.map(|t| format!("{:?}", t.status)),
                    "context_tokens": ctx,
                    "output_tokens": usage.output_tokens,
                    "requests": usage.request_count,
                    "cost_usd": estimate_cost(&usage),
                })
            })
            .collect()
    };

    // Collect all model names for consistent legend ordering.
    let mut all_models: Vec<String> = by_model.keys().cloned().collect();
    all_models.sort_by(|a, b| {
        let ta = by_model[a].input_tokens + by_model[a].cache_read_tokens;
        let tb = by_model[b].input_tokens + by_model[b].cache_read_tokens;
        tb.cmp(&ta)
    });

    // Total context (the real number the user cares about).
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

fn empty_response() -> Value {
    serde_json::json!({
        "by_day": {}, "by_hour": {}, "model_trend": {}, "by_model": {},
        "models": [], "totals": UsageBucket::default(),
        "total_context": 0, "total_requests": 0,
        "estimated_cost_usd": 0.0, "task_usage": [], "session_count": 0,
    })
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
fn parse_timestamp(ts: &str) -> (String, String) {
    // "2026-03-26T03:05:56.523Z" → day="2026-03-26", hour="2026-03-26 03:00"
    if ts.len() < 13 {
        return (String::new(), String::new());
    }
    let day = ts[..10].to_string();
    let hour = format!("{} {}:00", &ts[..10], &ts[11..13]);
    (day, hour)
}

fn collect_session_files(claude_projects_dir: &Path) -> Vec<PathBuf> {
    let mut files = Vec::new();
    let Ok(entries) = std::fs::read_dir(claude_projects_dir) else {
        return files;
    };
    for entry in entries.flatten() {
        let name = entry.file_name();
        if !name.to_string_lossy().contains("harness-workspaces") {
            continue;
        }
        let dir = entry.path();
        if !dir.is_dir() {
            continue;
        }
        collect_jsonl_in_dir(&dir, &mut files);
        if let Ok(sub_entries) = std::fs::read_dir(&dir) {
            for sub in sub_entries.flatten() {
                let sub_path = sub.path();
                if sub_path.is_dir() {
                    let subagents_dir = sub_path.join("subagents");
                    if subagents_dir.is_dir() {
                        collect_jsonl_in_dir(&subagents_dir, &mut files);
                    }
                }
            }
        }
    }
    files
}

fn collect_jsonl_in_dir(dir: &Path, out: &mut Vec<PathBuf>) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.extension().and_then(|e| e.to_str()) == Some("jsonl") {
            out.push(path);
        }
    }
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
