use anyhow::Context;
use harness_core::config::HarnessConfig;
use serde_json::{json, Value};
use std::collections::BTreeMap;
use std::net::IpAddr;
use std::time::Duration;
use url::form_urlencoded;

const REQUEST_TIMEOUT_SECS: u64 = 5;

#[derive(Debug, Default, PartialEq, Eq)]
struct RuntimeTreeSummary {
    workflows: usize,
    workflow_statuses: BTreeMap<String, usize>,
    workflow_scheduler_states: BTreeMap<String, usize>,
    workflow_active_buckets: BTreeMap<String, usize>,
    commands: usize,
    command_statuses: BTreeMap<String, usize>,
    jobs: usize,
    job_statuses: BTreeMap<String, usize>,
    running_job_lease_statuses: BTreeMap<String, usize>,
    activity_outcomes: BTreeMap<String, usize>,
    jobs_without_activity_envelope: usize,
    circuit_breakers: Vec<Value>,
}

pub async fn run(
    config: &HarnessConfig,
    url: Option<String>,
    project_id: Option<String>,
    runtime_limit: i64,
    raw_json: bool,
) -> anyhow::Result<()> {
    let base_url = server_base_url(config, url)?;
    let token = resolve_api_token(config);
    let client = reqwest::Client::builder()
        .timeout(Duration::from_secs(REQUEST_TIMEOUT_SECS))
        .no_proxy()
        .build()
        .context("failed to build HTTP client")?;

    let runtime_path = runtime_tree_path(project_id.as_deref(), runtime_limit);
    let health = get_json(&client, &base_url, "/health", token.as_deref()).await?;
    let queue = get_json(
        &client,
        &base_url,
        "/projects/queue-stats",
        token.as_deref(),
    )
    .await?;
    let operator = get_json(
        &client,
        &base_url,
        "/api/operator-snapshot",
        token.as_deref(),
    )
    .await?;
    let runtime_tree = get_json(&client, &base_url, &runtime_path, token.as_deref()).await?;

    let combined = json!({
        "server_url": base_url,
        "health": health,
        "queue": queue,
        "operator_snapshot": operator,
        "runtime_tree": runtime_tree,
    });

    if raw_json {
        println!("{}", serde_json::to_string_pretty(&combined)?);
    } else {
        print_summary(&combined);
    }
    Ok(())
}

pub(crate) fn resolve_api_token(config: &HarnessConfig) -> Option<String> {
    config
        .server
        .api_token
        .as_deref()
        .map(str::trim)
        .filter(|token| !token.is_empty())
        .map(str::to_owned)
        .or_else(|| {
            std::env::var("HARNESS_API_TOKEN")
                .ok()
                .map(|token| token.trim().to_string())
                .filter(|token| !token.is_empty())
        })
}

pub(crate) fn server_base_url(
    config: &HarnessConfig,
    override_url: Option<String>,
) -> anyhow::Result<String> {
    if let Some(url) = override_url {
        let trimmed = url.trim().trim_end_matches('/');
        if trimmed.is_empty() {
            anyhow::bail!("--url must not be empty");
        }
        if trimmed.starts_with("http://") || trimmed.starts_with("https://") {
            return Ok(trimmed.to_string());
        }
        return Ok(format!("http://{trimmed}"));
    }

    let addr = config.server.http_addr;
    let host = match addr.ip() {
        IpAddr::V4(ip) if ip.is_unspecified() => "127.0.0.1".to_string(),
        IpAddr::V4(ip) => ip.to_string(),
        IpAddr::V6(ip) if ip.is_unspecified() => "[::1]".to_string(),
        IpAddr::V6(ip) => format!("[{ip}]"),
    };
    Ok(format!("http://{}:{}", host, addr.port()))
}

async fn get_json(
    client: &reqwest::Client,
    base_url: &str,
    path: &str,
    token: Option<&str>,
) -> anyhow::Result<Value> {
    let url = format!("{base_url}{path}");
    let mut request = client.get(&url);
    if let Some(token) = token {
        request = request.bearer_auth(token);
    }
    let response = request
        .send()
        .await
        .with_context(|| format!("failed to connect to harness server at {url}"))?;
    let status = response.status();
    if status == reqwest::StatusCode::UNAUTHORIZED {
        anyhow::bail!(
            "server rejected {path} with 401; set server.api_token in config or HARNESS_API_TOKEN"
        );
    }
    if !status.is_success() {
        let body = response.text().await.unwrap_or_default();
        anyhow::bail!("server returned {status} for {path}: {body}");
    }
    response
        .json::<Value>()
        .await
        .with_context(|| format!("server returned invalid JSON for {path}"))
}

fn runtime_tree_path(project_id: Option<&str>, limit: i64) -> String {
    let mut query = form_urlencoded::Serializer::new(String::new());
    query.append_pair("limit", &limit.max(1).to_string());
    query.append_pair("summary_only", "true");
    if let Some(project_id) = project_id.filter(|value| !value.is_empty()) {
        query.append_pair("project_id", project_id);
    }
    format!("/api/workflows/runtime/tree?{}", query.finish())
}

fn print_summary(combined: &Value) {
    let health = &combined["health"];
    let queue = &combined["queue"];
    let operator = &combined["operator_snapshot"];
    let runtime = &combined["runtime_tree"];
    let runtime_summary = summarize_runtime_tree(runtime);

    println!(
        "Harness status: {}",
        health["status"].as_str().unwrap_or("unknown")
    );
    println!(
        "Server: {}",
        combined["server_url"].as_str().unwrap_or("unknown")
    );
    println!("Tasks: {}", number_or_zero(&health["tasks"]));
    println!(
        "Queue: running {}/{} queued {}",
        number_or_zero(&queue["global"]["running"]),
        number_or_zero(&queue["global"]["limit"]),
        number_or_zero(&queue["global"]["queued"])
    );

    let degraded = health["persistence"]["degraded_subsystems"]
        .as_array()
        .map(|items| {
            items
                .iter()
                .filter_map(Value::as_str)
                .collect::<Vec<_>>()
                .join(", ")
        })
        .filter(|items| !items.is_empty());
    if let Some(degraded) = degraded {
        println!("Degraded subsystems: {degraded}");
    }

    let last_tick = &operator["retry"]["last_tick"];
    if last_tick.is_object() {
        println!(
            "Retry tick: checked {} stuck {} retried {} skipped {} at {}",
            number_or_zero(&last_tick["checked"]),
            number_or_zero(&last_tick["stuck"]),
            number_or_zero(&last_tick["retried"]),
            number_or_zero(&last_tick["skipped"]),
            last_tick["at"].as_str().unwrap_or("unknown")
        );
    } else {
        println!("Retry tick: none");
    }
    println!(
        "Stalled tasks: {}",
        operator["retry"]["stalled_tasks"]
            .as_array()
            .map(Vec::len)
            .unwrap_or(0)
    );
    println!(
        "Recent failures: {}",
        operator["recent_failures"]
            .as_array()
            .map(Vec::len)
            .unwrap_or(0)
    );
    println!(
        "Runtime workflows: {} workflows, {} commands, {} jobs",
        runtime_summary.workflows, runtime_summary.commands, runtime_summary.jobs
    );
    print_count_map("Workflow statuses", &runtime_summary.workflow_statuses);
    print_count_map(
        "Workflow scheduler states",
        &runtime_summary.workflow_scheduler_states,
    );
    print_count_map(
        "Workflow active buckets",
        &runtime_summary.workflow_active_buckets,
    );
    print_count_map("Command statuses", &runtime_summary.command_statuses);
    print_count_map("Runtime job statuses", &runtime_summary.job_statuses);
    print_count_map(
        "Running job lease states",
        &runtime_summary.running_job_lease_statuses,
    );
    print_count_map(
        "Activity result outcomes",
        &runtime_summary.activity_outcomes,
    );
    if runtime_summary.jobs_without_activity_envelope > 0 {
        println!(
            "Jobs without activity result envelope: {}",
            runtime_summary.jobs_without_activity_envelope
        );
    }
    print_circuit_breakers(&runtime_summary.circuit_breakers);
}

fn summarize_runtime_tree(tree: &Value) -> RuntimeTreeSummary {
    let mut summary = RuntimeTreeSummary {
        workflows: tree["total_workflows"].as_u64().unwrap_or(0) as usize,
        ..Default::default()
    };
    let has_server_summary = tree["summary"].is_object();
    if has_server_summary {
        summary.workflow_statuses = count_map_from_value(&tree["summary"]["workflow_statuses"]);
        summary.workflow_scheduler_states =
            count_map_from_value(&tree["summary"]["workflow_scheduler_states"]);
        summary.workflow_active_buckets =
            count_map_from_value(&tree["summary"]["workflow_active_buckets"]);
        summary.commands = tree["summary"]["total_commands"].as_u64().unwrap_or(0) as usize;
        summary.jobs = tree["summary"]["total_runtime_jobs"].as_u64().unwrap_or(0) as usize;
        summary.command_statuses = count_map_from_value(&tree["summary"]["command_statuses"]);
        summary.job_statuses = count_map_from_value(&tree["summary"]["runtime_job_statuses"]);
        summary.running_job_lease_statuses =
            count_map_from_value(&tree["summary"]["running_job_lease_statuses"]);
        summary.activity_outcomes = count_map_from_value(&tree["summary"]["activity_outcomes"]);
        summary.jobs_without_activity_envelope = tree["summary"]["jobs_without_activity_envelope"]
            .as_u64()
            .unwrap_or(0) as usize;
        summary.circuit_breakers = tree["summary"]["circuit_breakers"]
            .as_array()
            .cloned()
            .unwrap_or_default();
    }
    if let Some(workflows) = tree["workflows"].as_array() {
        for workflow in workflows {
            summarize_workflow_node(workflow, &mut summary, !has_server_summary);
        }
    }
    summary
}

fn summarize_workflow_node(node: &Value, summary: &mut RuntimeTreeSummary, include_counts: bool) {
    if include_counts {
        if let Some(status) = node["projection"]["status"].as_str() {
            increment(&mut summary.workflow_statuses, status);
        }
        if let Some(authority_state) = node["projection"]["scheduler"]["authority_state"].as_str() {
            increment(&mut summary.workflow_scheduler_states, authority_state);
        }
        if let Some(active_bucket) = node["projection"]["active_bucket"].as_str() {
            increment(&mut summary.workflow_active_buckets, active_bucket);
        }
    }
    if let Some(commands) = node["commands"].as_array() {
        for command in commands {
            if include_counts {
                summary.commands += 1;
                increment(
                    &mut summary.command_statuses,
                    command["status"].as_str().unwrap_or("unknown"),
                );
            }
            if let Some(jobs) = command["runtime_jobs"].as_array() {
                for job in jobs {
                    if include_counts {
                        summary.jobs += 1;
                        increment(
                            &mut summary.job_statuses,
                            job["status"].as_str().unwrap_or("unknown"),
                        );
                        if let Some(lease_state) = job["lease_state"].as_str() {
                            increment(&mut summary.running_job_lease_statuses, lease_state);
                        }
                    }
                    if include_counts {
                        if let Some(outcome) = job["activity_result_envelope"]["outcome"].as_str() {
                            increment(&mut summary.activity_outcomes, outcome);
                        } else {
                            summary.jobs_without_activity_envelope += 1;
                        }
                    }
                }
            }
        }
    }
    if let Some(children) = node["children"].as_array() {
        for child in children {
            summarize_workflow_node(child, summary, include_counts);
        }
    }
}

fn increment(map: &mut BTreeMap<String, usize>, key: &str) {
    *map.entry(key.to_string()).or_insert(0) += 1;
}

fn count_map_from_value(value: &Value) -> BTreeMap<String, usize> {
    value
        .as_object()
        .map(|object| {
            object
                .iter()
                .filter_map(|(key, value)| {
                    value.as_u64().map(|count| (key.clone(), count as usize))
                })
                .collect()
        })
        .unwrap_or_default()
}

fn number_or_zero(value: &Value) -> u64 {
    value.as_u64().unwrap_or(0)
}

fn print_count_map(label: &str, counts: &BTreeMap<String, usize>) {
    if counts.is_empty() {
        return;
    }
    let rendered = counts
        .iter()
        .map(|(key, count)| format!("{key}={count}"))
        .collect::<Vec<_>>()
        .join(", ");
    println!("{label}: {rendered}");
}

fn print_circuit_breakers(circuit_breakers: &[Value]) {
    let rendered = circuit_breakers
        .iter()
        .filter(|breaker| breaker["state"].as_str() != Some("closed"))
        .filter_map(render_circuit_breaker)
        .collect::<Vec<_>>();
    if !rendered.is_empty() {
        println!("Circuit breakers: {}", rendered.join(", "));
    }
}

fn render_circuit_breaker(breaker: &Value) -> Option<String> {
    let profile = breaker["profile"].as_str()?;
    let state = breaker["state"].as_str().unwrap_or("unknown");
    let class = breaker["class"].as_str();
    let consecutive = breaker["consecutive"].as_u64();
    let cooldown_until = breaker["cooldown_until"].as_str();
    let mut parts = Vec::new();
    if let Some(class) = class {
        parts.push(class.to_string());
    }
    if let Some(consecutive) = consecutive {
        parts.push(format!("consecutive={consecutive}"));
    }
    if let Some(cooldown_until) = cooldown_until {
        parts.push(format!("until={cooldown_until}"));
    }
    if parts.is_empty() {
        Some(format!("{profile}:{state}"))
    } else {
        Some(format!("{profile}:{state}({})", parts.join(" ")))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::net::{Ipv4Addr, SocketAddr};

    #[test]
    fn runtime_tree_path_encodes_path_like_project_id() {
        assert_eq!(
            runtime_tree_path(Some("/Users/apple/repo name"), 20),
            "/api/workflows/runtime/tree?limit=20&summary_only=true&project_id=%2FUsers%2Fapple%2Frepo+name"
        );
    }

    #[test]
    fn runtime_tree_path_clamps_limit_and_encodes_project_id() {
        assert_eq!(
            runtime_tree_path(Some("/project-a"), 0),
            "/api/workflows/runtime/tree?limit=1&summary_only=true&project_id=%2Fproject-a"
        );
    }

    #[test]
    fn server_base_url_uses_loopback_for_unspecified_config_addr() {
        let mut config = HarnessConfig::default();
        config.server.http_addr = SocketAddr::from((Ipv4Addr::UNSPECIFIED, 9800));

        let url = server_base_url(&config, None).expect("url should resolve");

        assert_eq!(url, "http://127.0.0.1:9800");
    }

    #[test]
    fn server_base_url_accepts_host_port_override() {
        let config = HarnessConfig::default();

        let url = server_base_url(&config, Some("127.0.0.1:9900/".to_string()))
            .expect("url should resolve");

        assert_eq!(url, "http://127.0.0.1:9900");
    }

    #[test]
    fn summarize_runtime_tree_counts_nested_jobs_and_activity_outcomes() {
        let tree = json!({
            "total_workflows": 2,
            "workflows": [{
                "projection": {
                    "status": "implementing",
                    "scheduler": {
                        "authority_state": "running"
                    },
                    "active_bucket": "running"
                },
                "commands": [{
                    "status": "completed",
                        "runtime_jobs": [{
                            "status": "succeeded",
                            "lease_state": null,
                            "activity_result_envelope": {
                                "outcome": "accepted"
                            }
                    }]
                }],
                "children": [{
                    "projection": {
                        "status": "waiting",
                        "scheduler": {
                            "authority_state": "queued"
                        },
                        "active_bucket": "queued"
                    },
                    "commands": [{
                        "status": "pending",
                        "runtime_jobs": [{
                            "status": "pending",
                            "lease_state": null
                        }]
                    }],
                    "children": []
                }]
            }]
        });

        let summary = summarize_runtime_tree(&tree);

        assert_eq!(summary.workflows, 2);
        assert_eq!(summary.workflow_statuses["implementing"], 1);
        assert_eq!(summary.workflow_statuses["waiting"], 1);
        assert_eq!(summary.workflow_scheduler_states["queued"], 1);
        assert_eq!(summary.workflow_scheduler_states["running"], 1);
        assert_eq!(summary.workflow_active_buckets["queued"], 1);
        assert_eq!(summary.workflow_active_buckets["running"], 1);
        assert_eq!(summary.commands, 2);
        assert_eq!(summary.jobs, 2);
        assert_eq!(summary.command_statuses["completed"], 1);
        assert_eq!(summary.command_statuses["pending"], 1);
        assert_eq!(summary.job_statuses["succeeded"], 1);
        assert_eq!(summary.job_statuses["pending"], 1);
        assert!(summary.running_job_lease_statuses.is_empty());
        assert_eq!(summary.activity_outcomes["accepted"], 1);
        assert_eq!(summary.jobs_without_activity_envelope, 1);
    }

    #[test]
    fn summarize_runtime_tree_prefers_server_totals_when_present() {
        let tree = json!({
            "total_workflows": 2,
            "summary": {
                "workflow_statuses": {
                    "implementing": 3,
                    "waiting": 2
                },
                "workflow_scheduler_states": {
                    "queued": 2,
                    "running": 3
                },
                "workflow_active_buckets": {
                    "queued": 2,
                    "running": 3
                },
                "total_commands": 7,
                "total_runtime_jobs": 42,
                "command_statuses": {
                    "completed": 5,
                    "pending": 2
                },
                "runtime_job_statuses": {
                    "failed": 4,
                    "succeeded": 38
                },
                "running_job_lease_statuses": {
                    "active_leased": 3,
                    "expired_lease": 1
                },
                "activity_outcomes": {
                    "accepted": 36,
                    "repaired_structured_output": 2
                },
                "jobs_without_activity_envelope": 4
            },
            "workflows": [{
                "commands": [{
                    "status": "completed",
                    "runtime_jobs": [{
                        "status": "succeeded",
                        "activity_result_envelope": {
                            "outcome": "accepted"
                        }
                    }]
                }],
                "children": []
            }]
        });

        let summary = summarize_runtime_tree(&tree);

        assert_eq!(summary.workflows, 2);
        assert_eq!(summary.workflow_statuses["implementing"], 3);
        assert_eq!(summary.workflow_statuses["waiting"], 2);
        assert_eq!(summary.workflow_scheduler_states["queued"], 2);
        assert_eq!(summary.workflow_scheduler_states["running"], 3);
        assert_eq!(summary.workflow_active_buckets["queued"], 2);
        assert_eq!(summary.workflow_active_buckets["running"], 3);
        assert_eq!(summary.commands, 7);
        assert_eq!(summary.jobs, 42);
        assert_eq!(summary.command_statuses["completed"], 5);
        assert_eq!(summary.command_statuses["pending"], 2);
        assert_eq!(summary.job_statuses["failed"], 4);
        assert_eq!(summary.job_statuses["succeeded"], 38);
        assert_eq!(summary.running_job_lease_statuses["active_leased"], 3);
        assert_eq!(summary.running_job_lease_statuses["expired_lease"], 1);
        assert_eq!(summary.activity_outcomes["accepted"], 36);
        assert_eq!(summary.activity_outcomes["repaired_structured_output"], 2);
        assert_eq!(summary.jobs_without_activity_envelope, 4);
    }
}
