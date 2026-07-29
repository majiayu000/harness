use std::{
    fs,
    path::{Path, PathBuf},
};

const LEGACY_SERVER_LOCAL_REST_DTOS: &[&str] = &[
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
];

#[test]
fn new_rest_dtos_are_not_added_in_server_modules() {
    let manifest_dir = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    let mut discovered = Vec::new();

    for relative_root in ["src/http", "src/handlers"] {
        collect_rest_dtos(
            &manifest_dir,
            &manifest_dir.join(relative_root),
            &mut discovered,
        );
    }

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
        let source = fs::read_to_string(&path)
            .unwrap_or_else(|error| panic!("read {}: {error}", relative_path.display()));
        let file = syn::parse_file(&source)
            .unwrap_or_else(|error| panic!("parse {}: {error}", relative_path.display()));

        for item in file.items {
            let syn::Item::Struct(item_struct) = item else {
                continue;
            };
            let name = item_struct.ident.to_string();
            if name.ends_with("Request") || name.ends_with("Response") {
                discovered.push(format!("{}::{name}", relative_path.display()));
            }
        }
    }
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
