use std::{
    fs,
    path::{Path, PathBuf},
};

const LEGACY_SERVER_LOCAL_REST_DTOS: &[&str] = &[
    "src/github_pr_snapshot.rs::GitHubPrSnapshotGraphQlResponse",
    "src/handlers/cross_review.rs::CrossReviewResult",
    "src/handlers/operator_monitor.rs::FailureGroup",
    "src/handlers/operator_monitor.rs::LegacyQueueCounts",
    "src/handlers/operator_monitor.rs::OperatorAction",
    "src/handlers/operator_monitor.rs::OperatorActivity",
    "src/handlers/operator_monitor.rs::OperatorHealth",
    "src/handlers/operator_monitor.rs::OperatorMonitorPayload",
    "src/handlers/operator_monitor.rs::StuckWorkflow",
    "src/handlers/operator_monitor.rs::WorktreeSummary",
    "src/handlers/operator_monitor/activity.rs::RuntimeWorkflowCounts",
    "src/handlers/operator_monitor/activity.rs::SourceActivity",
    "src/handlers/operator_monitor/driverless_progress.rs::DriverlessProgressEvidence",
    "src/handlers/preflight.rs::PreflightResult",
    "src/handlers/projects.rs::RegisterProjectRequest",
    "src/handlers/reconcile.rs::ReconcileParams",
    "src/handlers/runtime_hosts.rs::CompleteRuntimeJobRequest",
    "src/handlers/runtime_hosts.rs::RegisterRuntimeHostRequest",
    "src/handlers/runtime_hosts/lease.rs::ClaimRuntimeJobRequest",
    "src/handlers/runtime_hosts/lease.rs::RenewRuntimeJobLeaseRequest",
    "src/handlers/runtime_project_cache.rs::SyncProjectItem",
    "src/handlers/runtime_project_cache.rs::SyncWatchedProjectsRequest",
    "src/handlers/skills.rs::GovernanceTransition",
    "src/handlers/skills.rs::GovernanceView",
    "src/handlers/skills.rs::StaleEntry",
    "src/handlers/token_usage.rs::HourModelBucket",
    "src/handlers/token_usage.rs::UsageBucket",
    "src/handlers/usage_monitor.rs::ActiveCount",
    "src/handlers/usage_monitor.rs::AgentInvocation",
    "src/handlers/usage_monitor.rs::CostConfig",
    "src/handlers/usage_monitor.rs::ModelPrice",
    "src/handlers/usage_monitor.rs::UsageDiagnostics",
    "src/handlers/usage_monitor.rs::UsageMonitorQuery",
    "src/handlers/usage_monitor.rs::UsageMonitorResponse",
    "src/handlers/usage_monitor.rs::UsageSummary",
    "src/handlers/usage_monitor.rs::UsageWindow",
    "src/handlers/usage_monitor_aggregate.rs::UsageGroup",
    "src/handlers/usage_monitor_candidate.rs::CandidateUsageAttribution",
    "src/handlers/usage_monitor_candidate.rs::CandidateUsageGroup",
    "src/handlers/usage_monitor_candidate.rs::CandidateUsageRow",
    "src/handlers/usage_monitor_local_usage.rs::CcstatsSessionRow",
    "src/handlers/usage_monitor_local_usage.rs::LocalUsageModelSummary",
    "src/handlers/usage_monitor_local_usage.rs::LocalUsageSourceSummary",
    "src/handlers/usage_monitor_process.rs::AgentProcess",
    "src/handlers/worktrees.rs::WorktreeResponse",
    "src/http/auth_routes.rs::PasswordResetRequest",
    "src/http/background/auto_recovery.rs::AutoRecoveryState",
    "src/http/misc_routes_runtime_tree.rs::WorkflowRuntimeTreeDetail",
    "src/http/misc_routes_runtime_tree.rs::WorkflowRuntimeTreePagination",
    "src/http/misc_routes_runtime_tree.rs::WorkflowRuntimeTreeQuery",
    "src/http/misc_routes_runtime_tree.rs::WorkflowRuntimeTreeResponse",
    "src/http/misc_routes_runtime_tree.rs::WorkflowRuntimeTreeSummary",
    "src/http/misc_routes_runtime_tree_nodes.rs::WorkflowRuntimeCommandNode",
    "src/http/misc_routes_runtime_tree_nodes.rs::WorkflowRuntimeJobNode",
    "src/http/misc_routes_runtime_tree_nodes.rs::WorkflowRuntimeTreeNode",
    "src/http/misc_routes_runtime_tree_nodes.rs::WorkflowRuntimeTreeProjection",
    "src/http/runtime_submission_routes.rs::ApprovalResponse",
    "src/http/runtime_submission_routes.rs::RuntimeSubmissionArtifact",
    "src/http/runtime_submission_routes.rs::RuntimeSubmissionPrompt",
    "src/http/signal_routes.rs::IngestSignalRequest",
    "src/http/state.rs::GitHubTokenDispatchCounterSnapshot",
    "src/http/task_mutation_routes.rs::RuntimeTranscriptReconstructionRequest",
    "src/http/task_mutation_routes.rs::WorkflowRuntimeCancelRequest",
    "src/http/task_mutation_routes.rs::WorkflowRuntimeMergeRequest",
    "src/http/task_mutation_routes.rs::WorkflowRuntimeRecoveryRouteRequest",
    "src/http/task_query_routes.rs::RuntimeSubmissionListCounts",
    "src/http/task_query_routes.rs::RuntimeSubmissionListPage",
    "src/http/task_query_routes.rs::RuntimeSubmissionListParams",
    "src/http/task_query_routes.rs::RuntimeSubmissionListResponse",
    "src/http/task_query_routes.rs::RuntimeSubmissionSummaryResponse",
    "src/http/task_query_routes/detail.rs::RuntimeTaskResponse",
    "src/http/workflow_routes.rs::IssueWorkflowByIssueQuery",
    "src/http/workflow_routes.rs::IssueWorkflowByPrQuery",
    "src/http/workflow_routes.rs::ProjectWorkflowByProjectQuery",
    "src/runtime_hosts.rs::RuntimeHostInfo",
    "src/runtime_hosts.rs::RuntimeHostLifecycle",
    "src/runtime_hosts.rs::TaskClaimResult",
    "src/runtime_project_cache.rs::HostProjectCacheSnapshot",
    "src/runtime_project_cache.rs::WatchedProject",
    "src/workflow_runtime_submission/runtime_request.rs::CreateTaskRequest",
];

#[test]
fn new_rest_dtos_are_not_added_in_server_modules() {
    let manifest_dir = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    let mut discovered = Vec::new();

    collect_rest_dtos(&manifest_dir, &manifest_dir.join("src"), &mut discovered);

    discovered.sort();
    let mut expected = LEGACY_SERVER_LOCAL_REST_DTOS.to_vec();
    expected.sort();

    assert_eq!(
        discovered, expected,
        "new REST request/response DTOs must be defined in harness-protocol::rest, not server-local HTTP modules; update this legacy allowlist only when a DTO is migrated out of harness-server"
    );
}

fn collect_rest_dtos(manifest_dir: &Path, dir: &Path, discovered: &mut Vec<String>) {
    for entry in fs::read_dir(dir).unwrap_or_else(|error| panic!("read {}: {error}", dir.display()))
    {
        let entry =
            entry.unwrap_or_else(|error| panic!("read entry in {}: {error}", dir.display()));
        let path = entry.path();
        if path.is_dir() {
            if path.file_name().is_some_and(|name| name == "tests") {
                continue;
            }
            collect_rest_dtos(manifest_dir, &path, discovered);
            continue;
        }
        if !is_production_rust_file(&path) {
            continue;
        }
        let relative_path = path.strip_prefix(manifest_dir).unwrap_or(&path);
        let path_str = relative_path.to_string_lossy().replace('\\', "/");
        let source =
            fs::read_to_string(&path).unwrap_or_else(|error| panic!("read {path_str}: {error}"));
        let file =
            syn::parse_file(&source).unwrap_or_else(|error| panic!("parse {path_str}: {error}"));

        for item in file.items {
            let (name, attrs) = match item {
                syn::Item::Struct(item_struct) => {
                    (item_struct.ident.to_string(), item_struct.attrs)
                }
                syn::Item::Enum(item_enum) => (item_enum.ident.to_string(), item_enum.attrs),
                _ => continue,
            };
            if is_cfg_test(&attrs) {
                continue;
            }
            if is_rest_dto_candidate(&path_str, &name, &attrs) {
                discovered.push(format!("{path_str}::{name}"));
            }
        }
    }
}

fn is_rest_dto_candidate(path_str: &str, name: &str, attrs: &[syn::Attribute]) -> bool {
    if !has_serde_derive(attrs) {
        return false;
    }
    let has_wire_name = name.ends_with("Request") || name.ends_with("Response");
    has_wire_name || is_rest_boundary_module(path_str)
}

fn is_rest_boundary_module(path_str: &str) -> bool {
    path_str.starts_with("src/http/")
        || path_str.starts_with("src/handlers/")
        || path_str == "src/runtime_hosts.rs"
        || path_str == "src/runtime_project_cache.rs"
}

fn is_cfg_test(attrs: &[syn::Attribute]) -> bool {
    attrs.iter().any(|attr| {
        let syn::Meta::List(list) = &attr.meta else {
            return false;
        };
        attr.path().is_ident("cfg")
            && list
                .tokens
                .to_string()
                .split(|ch: char| !ch.is_ascii_alphanumeric() && ch != '_')
                .any(|token| token == "test")
    })
}

fn has_serde_derive(attrs: &[syn::Attribute]) -> bool {
    attrs.iter().any(|attr| {
        let syn::Meta::List(list) = &attr.meta else {
            return false;
        };
        attr.path().is_ident("derive")
            && list
                .tokens
                .to_string()
                .split(|ch: char| !ch.is_ascii_alphanumeric() && ch != '_')
                .any(|token| matches!(token, "Serialize" | "Deserialize"))
    })
}

fn is_production_rust_file(path: &Path) -> bool {
    if path.extension().and_then(|ext| ext.to_str()) != Some("rs") {
        return false;
    }
    let Some(name) = path.file_name().and_then(|name| name.to_str()) else {
        return false;
    };
    !name.ends_with("_tests.rs") && !name.starts_with("tests_") && name != "test_fixtures.rs"
}
