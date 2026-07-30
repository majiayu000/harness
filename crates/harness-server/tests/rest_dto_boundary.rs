use std::{
    fs,
    path::{Path, PathBuf},
};

const LEGACY_SERVER_LOCAL_REST_DTOS: &[&str] = &[
    "src/github_pr_snapshot.rs::GitHubPrSnapshotGraphQlResponse",
    "src/handlers/projects.rs::RegisterProjectRequest",
    "src/handlers/runtime_hosts.rs::CompleteRuntimeJobRequest",
    "src/handlers/runtime_hosts.rs::RegisterRuntimeHostRequest",
    "src/handlers/runtime_hosts/lease.rs::ClaimRuntimeJobRequest",
    "src/handlers/runtime_hosts/lease.rs::RenewRuntimeJobLeaseRequest",
    "src/handlers/runtime_project_cache.rs::SyncWatchedProjectsRequest",
    "src/handlers/usage_monitor.rs::UsageMonitorResponse",
    "src/handlers/worktrees.rs::WorktreeResponse",
    "src/http/auth_routes.rs::PasswordResetRequest",
    "src/http/misc_routes_runtime_tree.rs::WorkflowRuntimeTreeResponse",
    "src/http/runtime_submission_routes.rs::ApprovalResponse",
    "src/http/signal_routes.rs::IngestSignalRequest",
    "src/http/task_mutation_routes.rs::RuntimeTranscriptReconstructionRequest",
    "src/http/task_mutation_routes.rs::WorkflowRuntimeCancelRequest",
    "src/http/task_mutation_routes.rs::WorkflowRuntimeMergeRequest",
    "src/http/task_mutation_routes.rs::WorkflowRuntimeRecoveryRouteRequest",
    "src/http/task_query_routes.rs::RuntimeSubmissionListResponse",
    "src/http/task_query_routes.rs::RuntimeSubmissionSummaryResponse",
    "src/http/task_query_routes/detail.rs::RuntimeTaskResponse",
    "src/workflow_runtime_submission/runtime_request.rs::CreateTaskRequest",
    "src/workflow_runtime_worker/runtime_execution_queue.rs::RuntimeExecutionQueueRequest",
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
            let syn::Item::Struct(item_struct) = item else {
                continue;
            };
            if is_cfg_test(&item_struct.attrs) {
                continue;
            }
            let name = item_struct.ident.to_string();
            if name.ends_with("Request") || name.ends_with("Response") {
                discovered.push(format!("{path_str}::{name}"));
            }
        }
    }
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

fn is_production_rust_file(path: &Path) -> bool {
    if path.extension().and_then(|ext| ext.to_str()) != Some("rs") {
        return false;
    }
    let Some(name) = path.file_name().and_then(|name| name.to_str()) else {
        return false;
    };
    !name.ends_with("_tests.rs") && !name.starts_with("tests_") && name != "test_fixtures.rs"
}
