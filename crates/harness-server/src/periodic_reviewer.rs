use crate::http::AppState;
use chrono::{DateTime, Utc};
use harness_core::{
    config::misc::ReviewConfig,
    config::project::{load_project_config, ReviewType},
    types::EventFilters,
};
use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::Mutex;
use tokio::time::sleep;
use tokio::time::Instant;

/// Per-project information resolved once at startup and reused each tick.
struct ProjectInfo {
    /// Unique project name (used as watermark key suffix and log identifier).
    name: String,
    /// Absolute path to the project root.
    root: PathBuf,
    /// Review focus type resolved from `.harness/config.toml`.
    review_type: ReviewType,
}

/// Mutable state shared between review ticks.
///
/// Both fields are combined into a single `Mutex` to eliminate the RS-01
/// nested-lock risk that arises from holding two separate mutexes in the same
/// function.  Every acquisition of this lock is short (no `.await` while
/// holding it) and sequential — locks are never held concurrently.
#[derive(Default)]
struct ReviewState {
    /// Local fallback timestamp guards against stale deduplication when the
    /// EventStore write fails.  Updated atomically after every successful task
    /// enqueue so the next cycle always sees a fresh lower-bound even if the
    /// DB is unavailable.
    fallback_ts: Option<DateTime<Utc>>,
    /// Handle for the currently active polling task.  The handle is dropped
    /// (without abort) when a new cycle starts so the old task can still run
    /// to completion and persist its findings (issue #448).
    poll_handle: Option<tokio::task::JoinHandle<()>>,
}

/// Spawn the periodic review loop as a background task.
///
/// If review is disabled in config the loop returns immediately without
/// spawning anything; no resources are consumed.
/// Returns the EventStore watermark key for a project.
///
/// Encoding the root path prevents stale watermarks from leaking across a
/// root re-point: if a registered project is updated (via registry upsert) to
/// point at a different repository, the new root gets its own key and reviews
/// start from scratch instead of inheriting the previous root's history.
fn project_hook_key(name: &str, root: &std::path::Path) -> String {
    // Percent-encode '%' first (to avoid double-encoding), then '@', so that
    // project names containing '@' cannot be confused with the name/root
    // separator.  Root paths are absolute filesystem paths on macOS/Linux and
    // do not require encoding — the first '@' after the colon is unambiguously
    // the separator once the name is encoded.
    let encoded_name = name.replace('%', "%25").replace('@', "%40");
    format!("periodic_review:{}@{}", encoded_name, root.display())
}

pub fn start(state: Arc<AppState>, config: ReviewConfig) {
    if !config.enabled {
        tracing::debug!("scheduler: periodic review disabled, review_loop not started");
        return;
    }

    tokio::spawn(async move {
        review_loop(state, config).await;
    });
}

fn startup_project_delay(
    run_on_startup: bool,
    interval: Duration,
    last_review_ts: Option<DateTime<Utc>>,
    now: DateTime<Utc>,
) -> Duration {
    let interval_chrono =
        chrono::Duration::from_std(interval).unwrap_or_else(|_| chrono::Duration::hours(24));
    match last_review_ts {
        Some(last_ts) => {
            let elapsed = now.signed_duration_since(last_ts);
            if elapsed >= interval_chrono {
                if run_on_startup {
                    Duration::ZERO
                } else {
                    interval
                }
            } else {
                (interval_chrono - elapsed).to_std().unwrap_or(interval)
            }
        }
        None => {
            if run_on_startup {
                Duration::ZERO
            } else {
                interval
            }
        }
    }
}

struct StartupDelayOutcome {
    delay: Duration,
    last_review_ts: Option<DateTime<Utc>>,
    overdue: bool,
    elapsed: Option<chrono::Duration>,
    reused_startup_delay: bool,
}

fn startup_delay_outcome(
    run_on_startup: bool,
    interval: Duration,
    last_review_ts: Option<DateTime<Utc>>,
    now: DateTime<Utc>,
) -> StartupDelayOutcome {
    let interval_chrono =
        chrono::Duration::from_std(interval).unwrap_or_else(|_| chrono::Duration::hours(24));
    let elapsed = last_review_ts.map(|ts| now.signed_duration_since(ts));
    let overdue = elapsed.is_some_and(|elapsed| elapsed >= interval_chrono);
    let delay = startup_project_delay(run_on_startup, interval, last_review_ts, now);
    StartupDelayOutcome {
        delay,
        last_review_ts,
        overdue,
        elapsed,
        reused_startup_delay: false,
    }
}

async fn startup_delay_for_project(
    state: &Arc<AppState>,
    project: &ProjectInfo,
    run_on_startup: bool,
    interval: Duration,
    now: DateTime<Utc>,
) -> StartupDelayOutcome {
    let hook_key = project_hook_key(&project.name, &project.root);
    let last_review_ts = last_review_timestamp(state, &hook_key).await;
    startup_delay_outcome(run_on_startup, interval, last_review_ts, now)
}

fn startup_delay_for_rescanned_project_outcome(
    startup_delay_by_name: &HashMap<String, Duration>,
    project_name: &str,
    run_on_startup: bool,
    interval: Duration,
    last_review_ts: Option<DateTime<Utc>>,
    now: DateTime<Utc>,
) -> StartupDelayOutcome {
    match startup_delay_by_name.get(project_name).copied() {
        Some(delay) => StartupDelayOutcome {
            delay,
            last_review_ts,
            overdue: false,
            elapsed: last_review_ts.map(|ts| now.signed_duration_since(ts)),
            reused_startup_delay: true,
        },
        None => startup_delay_outcome(run_on_startup, interval, last_review_ts, now),
    }
}

fn startup_rescan_remaining_delay(outcome: &StartupDelayOutcome, min_delay: Duration) -> Duration {
    if outcome.reused_startup_delay {
        outcome.delay.saturating_sub(min_delay)
    } else {
        outcome.delay
    }
}

async fn startup_delay_for_rescanned_project(
    state: &Arc<AppState>,
    startup_delay_by_name: &HashMap<String, Duration>,
    project: &ProjectInfo,
    run_on_startup: bool,
    interval: Duration,
    now: DateTime<Utc>,
) -> StartupDelayOutcome {
    let hook_key = project_hook_key(&project.name, &project.root);
    let last_review_ts = last_review_timestamp(state, &hook_key).await;
    startup_delay_for_rescanned_project_outcome(
        startup_delay_by_name,
        &project.name,
        run_on_startup,
        interval,
        last_review_ts,
        now,
    )
}

async fn review_loop(state: Arc<AppState>, config: ReviewConfig) {
    let interval = config.effective_interval();

    // Resolve all projects to review once at startup.
    let projects = collect_projects(&state).await;
    // Per-project state map: each project gets its own ReviewState mutex.
    let mut state_map: HashMap<String, Arc<Mutex<ReviewState>>> = projects
        .iter()
        .map(|p| (p.name.clone(), Arc::new(Mutex::new(ReviewState::default()))))
        .collect();

    tracing::info!(
        project_count = projects.len(),
        "periodic_review watermark keys migrated; first run will treat all projects as overdue"
    );

    // Brief delay to let the server fully initialize before checking.
    sleep(Duration::from_secs(5)).await;

    // On startup, compute the per-project remaining time and the global minimum.
    // Only projects whose individual delay falls within the minimum sleep window
    // are run on the first tick; others wait for the regular interval loop.
    let mut min_delay = interval;
    let mut project_delays: Vec<Duration> = Vec::with_capacity(projects.len());
    for project in &projects {
        let outcome =
            startup_delay_for_project(&state, project, config.run_on_startup, interval, Utc::now())
                .await;
        match outcome.last_review_ts {
            Some(last_ts) => {
                if outcome.overdue {
                    if config.run_on_startup {
                        tracing::info!(
                            project = %project.name,
                            last_review = %last_ts,
                            elapsed_hours = outcome.elapsed.map(|e| e.num_hours()).unwrap_or_default(),
                            "scheduler: periodic review overdue, triggering now"
                        );
                    } else {
                        tracing::info!(
                            project = %project.name,
                            last_review = %last_ts,
                            next_in_secs = outcome.delay.as_secs(),
                            "scheduler: startup review suppressed for overdue project; deferring until next interval"
                        );
                    }
                } else {
                    tracing::info!(
                        project = %project.name,
                        last_review = %last_ts,
                        next_in_secs = outcome.delay.as_secs(),
                        "scheduler: periodic review not yet due, sleeping"
                    );
                }
            }
            None => {
                if config.run_on_startup {
                    tracing::info!(
                        project = %project.name,
                        "scheduler: no prior periodic review found, triggering now"
                    );
                } else {
                    tracing::info!(
                        project = %project.name,
                        next_in_secs = outcome.delay.as_secs(),
                        "scheduler: startup review suppressed for first run; deferring until next interval"
                    );
                }
            }
        }
        project_delays.push(outcome.delay);
        if outcome.delay < min_delay {
            min_delay = outcome.delay;
        }
    }

    if !min_delay.is_zero() {
        sleep(min_delay).await;
    }

    // Re-collect after the startup sleep to pick up projects registered via
    // `POST /projects` during the sleep window (5 s init + min_delay).
    // Build a delay-by-name lookup first so projects from the original
    // snapshot keep their already-computed startup delay. Projects newly
    // discovered during the sleep window recompute their delay from the
    // current watermark on demand.
    let startup_delay_by_name: HashMap<String, Duration> = projects
        .iter()
        .zip(project_delays.iter())
        .map(|(p, &d)| (p.name.clone(), d))
        .collect();
    let projects = collect_projects(&state).await;
    // Ensure state_map covers any projects that appeared during the sleep.
    for p in &projects {
        state_map
            .entry(p.name.clone())
            .or_insert_with(|| Arc::new(Mutex::new(ReviewState::default())));
    }

    // Build next_run_map incrementally so each project's schedule is anchored
    // to its own tick-completion time.  A single startup_now captured before
    // first-pass processing would place startup_now + interval in the past for
    // slow guard scans / queue waits, triggering an immediate re-review and
    // avoidable quota spikes when the main loop starts.
    let mut next_run_map: HashMap<String, Instant> = HashMap::new();

    // First tick: only run projects that became due within the startup sleep window.
    // Projects whose remaining time exceeds min_delay are deferred to the regular
    // interval loop, avoiding unnecessary task/quota spikes on restart.
    for project in &projects {
        let outcome = startup_delay_for_rescanned_project(
            &state,
            &startup_delay_by_name,
            project,
            config.run_on_startup,
            interval,
            Utc::now(),
        )
        .await;
        let remaining_delay = startup_rescan_remaining_delay(&outcome, min_delay);
        if !remaining_delay.is_zero() {
            tracing::debug!(
                project = %project.name,
                remaining_secs = remaining_delay.as_secs(),
                "scheduler: project not yet due at startup, deferring to next scheduled tick"
            );
            // Anchor deferred schedule relative to now (after sleep elapsed).
            next_run_map.insert(project.name.clone(), Instant::now() + remaining_delay);
            continue;
        }
        let review_state = state_map[&project.name].clone();
        if let Err(e) = tick::run_review_tick(&state, &config, &review_state, project).await {
            tracing::error!(
                project = %project.name,
                "scheduler: review tick failed: {e}"
            );
        }
        // Anchor to Instant::now() post-tick so slow guard scans or queue
        // waits never place next_run in the past before the main loop starts.
        next_run_map.insert(project.name.clone(), Instant::now() + interval);
    }

    // Track last-known root per project name so root re-pointing (registry
    // upsert to a different path) is detected each tick and triggers eviction.
    let mut root_by_name: HashMap<String, PathBuf> = projects
        .iter()
        .map(|p| (p.name.clone(), p.root.clone()))
        .collect();

    loop {
        // Re-collect on each tick to pick up projects registered at runtime
        // via `POST /projects` without requiring a server restart.
        let current_projects = collect_projects(&state).await;
        let current_names: std::collections::HashSet<&str> =
            current_projects.iter().map(|p| p.name.as_str()).collect();

        // Prune stale entries for projects that were removed since last tick.
        // Without this, a removed project's expired deadline stays in
        // next_run_map; once it passes, saturating_duration_since returns 0
        // and the loop spins with sleep(0) until restart.
        next_run_map.retain(|name, _| current_names.contains(name.as_str()));
        state_map.retain(|name, _| current_names.contains(name.as_str()));
        root_by_name.retain(|name, _| current_names.contains(name.as_str()));

        // Detect root changes: if a project was re-pointed to a different
        // repository via a registry upsert, evict its stale in-memory state
        // and schedule so the new repo's history is not silently skipped.
        for p in &current_projects {
            if let Some(old_root) = root_by_name.get(&p.name) {
                if *old_root != p.root {
                    tracing::info!(
                        project = %p.name,
                        old_root = %old_root.display(),
                        new_root = %p.root.display(),
                        "scheduler: project root changed, resetting review state and schedule"
                    );
                    state_map.remove(&p.name);
                    next_run_map.remove(&p.name);
                }
            }
            root_by_name.insert(p.name.clone(), p.root.clone());
        }

        for p in &current_projects {
            state_map
                .entry(p.name.clone())
                .or_insert_with(|| Arc::new(Mutex::new(ReviewState::default())));
            next_run_map
                .entry(p.name.clone())
                .or_insert_with(Instant::now);
        }

        // Sleep only until the earliest project becomes due, not a full interval.
        let sleep_dur = next_run_map
            .values()
            .copied()
            .min()
            .map(|earliest| earliest.saturating_duration_since(Instant::now()))
            .unwrap_or(interval);
        sleep(sleep_dur).await;

        let tick_now = Instant::now();
        for project in &current_projects {
            if next_run_map.get(&project.name).copied().unwrap_or(tick_now) <= tick_now {
                let review_state = state_map[&project.name].clone();
                if let Err(e) = tick::run_review_tick(&state, &config, &review_state, project).await
                {
                    tracing::error!(
                        project = %project.name,
                        "scheduler: review tick failed: {e}"
                    );
                }
                next_run_map.insert(project.name.clone(), Instant::now() + interval);
            }
        }
    }
}

/// Resolve all projects that should be periodically reviewed.
///
/// Merges two sources:
/// 1. `[[projects]]` entries from `server.config` (static config file).
/// 2. Projects registered at runtime via CLI `--project` flags or the HTTP
///    `POST /projects` API (stored in `project_registry`).
///
/// When neither source has entries, falls back to a single entry derived from
/// `state.core.project_root` for backward compatibility.
async fn collect_projects(state: &Arc<AppState>) -> Vec<ProjectInfo> {
    let config_projects = &state.core.server.config.projects;

    // Build a lookup from startup_projects so we can resolve the effective path
    // for each config entry.  `startup_projects` is populated by the CLI layer
    // (`serve.rs`) and already incorporates two things that `config.projects`
    // does not: (a) canonicalized paths, and (b) `--project name=path` CLI
    // overrides that replace same-named config entries.  Using it here ensures
    // that a CLI override is honoured and the old config path is never reviewed.
    let startup_path_by_name: HashMap<&str, &PathBuf> = state
        .core
        .server
        .startup_projects
        .iter()
        .map(|project| (project.name.as_str(), &project.root))
        .collect();

    // Pre-fetch live registry roots so that a runtime `POST /projects` upsert
    // (same ID, new root) is honoured when resolving config project roots.
    // Without this, config projects would always use the stale config/startup
    // root while normal task routing resolves via the registry, creating
    // split-brain execution against different repositories.
    //
    // Priority: registry (live) > startup CLI flag > config file (static).
    let registry_root_by_id: HashMap<String, PathBuf> =
        if let Some(registry) = state.core.project_registry.as_deref() {
            match registry.list().await {
                Ok(projects) => projects
                    .into_iter()
                    .filter(|p| p.active)
                    .map(|p| (p.id.clone(), p.root.clone()))
                    .collect(),
                Err(e) => {
                    tracing::warn!(
                        "scheduler: failed to pre-fetch registry project roots; \
                         config/startup roots will be used as fallback: {e}"
                    );
                    HashMap::new()
                }
            }
        } else {
            HashMap::new()
        };

    // Track roots and names already covered to deduplicate config vs registry entries.
    let mut seen_roots: std::collections::HashSet<PathBuf> = std::collections::HashSet::new();
    let mut seen_names: std::collections::HashSet<String> = std::collections::HashSet::new();
    let mut result: Vec<ProjectInfo> = Vec::new();

    if config_projects.is_empty() && state.core.server.startup_projects.is_empty() {
        // Backward-compat: seed with the default project root.
        let root = state.core.project_root.clone();
        let name = root
            .file_name()
            .and_then(|n| n.to_str())
            .unwrap_or("default")
            .to_string();
        let review_type = load_project_config(&root)
            .map(|cfg| cfg.review_type)
            .unwrap_or_default();
        seen_names.insert(name.clone());
        seen_roots.insert(root.clone());
        result.push(ProjectInfo {
            name,
            root,
            review_type,
        });
    } else {
        result.reserve(config_projects.len());
        for entry in config_projects {
            // Resolve effective root with priority: registry (live) > startup CLI > config.
            // Consulting the registry first ensures that a runtime `POST /projects`
            // upsert (same ID, new root) is honoured — the registry is the
            // authoritative live-state source, while startup/config are static snapshots.
            let effective_root: PathBuf = registry_root_by_id
                .get(entry.name.as_str())
                .cloned()
                .or_else(|| {
                    startup_path_by_name
                        .get(entry.name.as_str())
                        .map(|p| (*p).clone())
                })
                .unwrap_or_else(|| entry.root.clone());

            if !effective_root.exists() {
                tracing::warn!(
                    project = %entry.name,
                    root = %effective_root.display(),
                    "scheduler: project root does not exist on disk, skipping"
                );
                continue;
            }
            let project_cfg = match load_project_config(&effective_root) {
                Ok(cfg) => cfg,
                Err(e) => {
                    tracing::warn!(
                        project = %entry.name,
                        error = %e,
                        "scheduler: failed to load project config, skipping"
                    );
                    continue;
                }
            };
            // Respect explicit opt-out via `.harness/config.toml` [review] enabled = false.
            // Note: `review.enabled = false` disables both PR auto-review and periodic
            // health checks.  Use debug level — this is a deliberate opt-out, not an error,
            // and collect_projects() is called on every scheduler tick.
            if project_cfg.review.as_ref().and_then(|r| r.enabled) == Some(false) {
                tracing::debug!(
                    project = %entry.name,
                    "scheduler: `review.enabled = false` suppresses periodic health checks \
                     in addition to PR auto-review; skipping project"
                );
                continue;
            }
            // Insert both raw and canonical forms so that relative/absolute
            // aliasing of the same path does not produce duplicate entries.
            let canonical_entry_root = effective_root
                .canonicalize()
                .unwrap_or_else(|_| effective_root.clone());
            // Skip if this name or root was already added (handles duplicate
            // [[projects]] entries with the same name or same physical path).
            if seen_names.contains(&entry.name)
                || seen_roots.contains(&effective_root)
                || seen_roots.contains(&canonical_entry_root)
            {
                tracing::warn!(
                    project = %entry.name,
                    root = %effective_root.display(),
                    "scheduler: duplicate config project (same name or root), skipping"
                );
                continue;
            }
            seen_names.insert(entry.name.clone());
            seen_roots.insert(effective_root.clone());
            seen_roots.insert(canonical_entry_root);
            result.push(ProjectInfo {
                name: entry.name.clone(),
                root: effective_root,
                review_type: project_cfg.review_type,
            });
        }
    }

    // Also include projects registered at runtime via CLI flags or the HTTP API.
    // These are stored in project_registry and are not reflected in server.config.projects.
    if let Some(registry) = state.core.project_registry.as_deref() {
        match registry.list().await {
            Ok(reg_projects) => {
                for proj in reg_projects {
                    if !proj.active {
                        continue;
                    }
                    // Skip roots already covered by config entries (check both raw
                    // and canonical forms to handle symlinked paths).
                    let canonical = proj
                        .root
                        .canonicalize()
                        .unwrap_or_else(|_| proj.root.clone());
                    if seen_roots.contains(&proj.root) || seen_roots.contains(&canonical) {
                        continue;
                    }
                    if !proj.root.exists() {
                        tracing::warn!(
                            project = %proj.id,
                            root = %proj.root.display(),
                            "scheduler: registry project root does not exist on disk, skipping"
                        );
                        continue;
                    }
                    let (review_type, opted_out) = match load_project_config(&proj.root) {
                        Ok(cfg) => {
                            let opted_out =
                                cfg.review.as_ref().and_then(|r| r.enabled) == Some(false);
                            (cfg.review_type, opted_out)
                        }
                        Err(e) => {
                            tracing::warn!(
                                project = %proj.id,
                                error = %e,
                                "scheduler: failed to load registry project config, skipping"
                            );
                            continue;
                        }
                    };
                    if opted_out {
                        tracing::debug!(
                            project = %proj.id,
                            "scheduler: `review.enabled = false` suppresses periodic health checks \
                             in addition to PR auto-review; skipping registry project"
                        );
                        continue;
                    }
                    if seen_names.contains(proj.id.as_str()) {
                        continue;
                    }
                    result.push(ProjectInfo {
                        name: proj.id.clone(),
                        root: proj.root.clone(),
                        review_type,
                    });
                    // Mark both raw and canonical forms as seen so that a second
                    // registry entry for the same physical root is skipped.
                    seen_names.insert(proj.id.clone());
                    seen_roots.insert(proj.root.clone());
                    seen_roots.insert(canonical);
                }
            }
            Err(e) => {
                tracing::warn!(
                    "scheduler: failed to list registry projects; \
                     CLI/API-registered projects will be excluded from this review cycle: {e}"
                );
            }
        }
    }

    // P1-1: Also include startup_projects entries not already covered by config
    // or registry, in case the registry registration failed at startup.
    for project in state.core.server.startup_projects.iter() {
        if seen_names.contains(project.name.as_str()) {
            continue;
        }
        let canonical = project
            .root
            .canonicalize()
            .unwrap_or_else(|_| project.root.clone());
        if seen_roots.contains(&project.root) || seen_roots.contains(&canonical) {
            continue;
        }
        if !project.root.exists() {
            tracing::warn!(
                project = %project.name,
                root = %project.root.display(),
                "scheduler: startup project root does not exist on disk, skipping"
            );
            continue;
        }
        let (review_type, opted_out) = match load_project_config(&project.root) {
            Ok(cfg) => {
                let opted_out = cfg.review.as_ref().and_then(|r| r.enabled) == Some(false);
                (cfg.review_type, opted_out)
            }
            Err(e) => {
                tracing::warn!(
                    project = %project.name,
                    error = %e,
                    "scheduler: failed to load startup project config, skipping"
                );
                continue;
            }
        };
        if opted_out {
            tracing::debug!(
                project = %project.name,
                "scheduler: `review.enabled = false` suppresses periodic health checks \
                 in addition to PR auto-review; skipping startup project"
            );
            continue;
        }
        seen_names.insert(project.name.clone());
        seen_roots.insert(project.root.clone());
        seen_roots.insert(canonical);
        result.push(ProjectInfo {
            name: project.name.clone(),
            root: project.root.clone(),
            review_type,
        });
    }

    result
}

/// Query the most recent watermark for the given `hook_key` from the EventStore.
///
/// The key is namespaced per project: `"periodic_review:<project_name>"`.
async fn last_review_timestamp(state: &Arc<AppState>, hook_key: &str) -> Option<DateTime<Utc>> {
    let events = state
        .observability
        .events
        .query(&EventFilters {
            hook: Some(hook_key.to_string()),
            ..EventFilters::default()
        })
        .await
        .ok()?;
    events.iter().map(|e| e.ts).max()
}

#[path = "periodic_reviewer/commit_gate.rs"]
mod commit_gate;
#[path = "periodic_reviewer/tick.rs"]
mod tick;
#[path = "periodic_reviewer/tick_helpers.rs"]
mod tick_helpers;

#[cfg(test)]
#[path = "periodic_reviewer/tests.rs"]
mod tests;
