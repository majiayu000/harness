#!/usr/bin/env bash
set -euo pipefail

if [ "${HARNESS_PRE_PUSH_FAKE_CARGO:-}" = "1" ]; then
    command_class="unknown"
    arguments=" $* "
    if [ "${1:-}" = "clippy" ]; then
        command_class="clippy"
    elif [[ "$arguments" == *" -p harness-workflow "* ]]; then
        command_class="workflow"
    elif [[ "$arguments" == *" -p harness-server "* ]]; then
        command_class="server"
    elif [ "${1:-}" = "test" ]; then
        command_class="workspace"
    fi

    is_set() {
        local variable_name="$1"
        if [ "${!variable_name+x}" = "x" ]; then
            printf '1'
        else
            printf '0'
        fi
    }

    matches_expected() {
        local variable_name="$1"
        local expected_name="$2"
        if [ "${!variable_name+x}" = "x" ] &&
            [ "${!expected_name+x}" = "x" ] &&
            [ "${!variable_name}" = "${!expected_name}" ]; then
            printf '1'
        else
            printf '0'
        fi
    }

    xdg_is_global=0
    if [ "${XDG_CONFIG_HOME:-}" = "${HARNESS_PRE_PUSH_GLOBAL_XDG:-}" ]; then
        xdg_is_global=1
    fi

    printf '%s\targv=%s\tdb_set=%s\tdb_matches=%s\tpool_max_set=%s\tpool_max_matches=%s\tpool_timeout_set=%s\tpool_timeout_matches=%s\txdg=%s\txdg_is_global=%s\tgit_index_set=%s\tgit_dir_set=%s\tgit_work_tree_set=%s\n' \
        "$command_class" \
        "$*" \
        "$(is_set HARNESS_DATABASE_URL)" \
        "$(matches_expected HARNESS_DATABASE_URL HARNESS_PRE_PUSH_EXPECTED_DATABASE_URL)" \
        "$(is_set HARNESS_DATABASE_POOL_MAX_CONNECTIONS)" \
        "$(matches_expected HARNESS_DATABASE_POOL_MAX_CONNECTIONS HARNESS_PRE_PUSH_EXPECTED_POOL_MAX)" \
        "$(is_set HARNESS_DATABASE_POOL_ACQUIRE_TIMEOUT_SECS)" \
        "$(matches_expected HARNESS_DATABASE_POOL_ACQUIRE_TIMEOUT_SECS HARNESS_PRE_PUSH_EXPECTED_POOL_TIMEOUT)" \
        "${XDG_CONFIG_HOME:-}" \
        "$xdg_is_global" \
        "$(is_set GIT_INDEX_FILE)" \
        "$(is_set GIT_DIR)" \
        "$(is_set GIT_WORK_TREE)" \
        >>"$HARNESS_PRE_PUSH_LOG"

    if [ "${HARNESS_PRE_PUSH_SIGNAL_CLASS:-}" = "$command_class" ]; then
        kill -TERM "$PPID"
        sleep 1
        exit 0
    fi

    if [ "${HARNESS_PRE_PUSH_FAIL_CLASS:-}" = "$command_class" ]; then
        exit 41
    fi

    exit 0
fi

repo_root="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
hook="$repo_root/.githooks/pre-push"
test_root="$(mktemp -d "${TMPDIR:-/tmp}/harness-pre-push-test.XXXXXX")"
trap 'rm -rf -- "$test_root"' EXIT

fail() {
    echo "test-pre-push-hook: $*" >&2
    exit 1
}

assert_equals() {
    local expected="$1"
    local actual="$2"
    local context="$3"
    if [ "$actual" != "$expected" ]; then
        fail "$context: expected '$expected', got '$actual'"
    fi
}

assert_contains() {
    local file="$1"
    local expected="$2"
    local context="$3"
    if ! grep -Fq -- "$expected" "$file"; then
        fail "$context: missing '$expected'"
    fi
}

assert_not_contains() {
    local file="$1"
    local unexpected="$2"
    local context="$3"
    if grep -Fq -- "$unexpected" "$file"; then
        fail "$context: found unexpected '$unexpected'"
    fi
}

assert_isolation_removed() {
    local case_tmp="$1"
    local leftover
    leftover="$(find "$case_tmp" -mindepth 1 -maxdepth 1 -name 'harness-pre-push.*' -print -quit)"
    if [ -n "$leftover" ]; then
        fail "temporary config root was not removed: $leftover"
    fi
}

[ -x "$hook" ] || fail "$hook is not executable"

fake_bin="$test_root/bin"
mkdir -p "$fake_bin"
ln -s "$repo_root/scripts/test-pre-push-hook.sh" "$fake_bin/cargo"

secret_url="postgres://hook-user:secret-${RANDOM}-${RANDOM}@127.0.0.1:55432/hook-test"
pool_max="17"
pool_timeout="23"

run_case() {
    local name="$1"
    local mode="$2"
    local fail_class="${3:-}"
    local signal_class="${4:-}"
    local expected_status="${5:-0}"
    local case_dir="$test_root/$name"
    local case_tmp="$case_dir/tmp"
    local global_xdg="$case_dir/global-config"
    local log="$case_dir/cargo.log"
    local output="$case_dir/output.log"
    local status

    mkdir -p "$case_tmp" "$global_xdg"
    : >"$log"

    set +e
    if [ "$mode" = "configured" ]; then
        env \
            PATH="$fake_bin:$PATH" \
            TMPDIR="$case_tmp" \
            XDG_CONFIG_HOME="$global_xdg" \
            HARNESS_DATABASE_URL="$secret_url" \
            HARNESS_DATABASE_POOL_MAX_CONNECTIONS="$pool_max" \
            HARNESS_DATABASE_POOL_ACQUIRE_TIMEOUT_SECS="$pool_timeout" \
            HARNESS_PRE_PUSH_FAKE_CARGO=1 \
            HARNESS_PRE_PUSH_LOG="$log" \
            HARNESS_PRE_PUSH_GLOBAL_XDG="$global_xdg" \
            HARNESS_PRE_PUSH_EXPECTED_DATABASE_URL="$secret_url" \
            HARNESS_PRE_PUSH_EXPECTED_POOL_MAX="$pool_max" \
            HARNESS_PRE_PUSH_EXPECTED_POOL_TIMEOUT="$pool_timeout" \
            HARNESS_PRE_PUSH_FAIL_CLASS="$fail_class" \
            HARNESS_PRE_PUSH_SIGNAL_CLASS="$signal_class" \
            GIT_INDEX_FILE="$case_dir/index" \
            GIT_DIR="$case_dir/git-dir" \
            GIT_WORK_TREE="$case_dir/work-tree" \
            "$hook" >"$output" 2>&1
        status=$?
    elif [ "$mode" = "empty" ]; then
        env \
            PATH="$fake_bin:$PATH" \
            TMPDIR="$case_tmp" \
            XDG_CONFIG_HOME="$global_xdg" \
            HARNESS_DATABASE_URL="" \
            HARNESS_DATABASE_POOL_MAX_CONNECTIONS="$pool_max" \
            HARNESS_DATABASE_POOL_ACQUIRE_TIMEOUT_SECS="$pool_timeout" \
            HARNESS_PRE_PUSH_FAKE_CARGO=1 \
            HARNESS_PRE_PUSH_LOG="$log" \
            HARNESS_PRE_PUSH_GLOBAL_XDG="$global_xdg" \
            HARNESS_PRE_PUSH_EXPECTED_DATABASE_URL="$secret_url" \
            HARNESS_PRE_PUSH_EXPECTED_POOL_MAX="$pool_max" \
            HARNESS_PRE_PUSH_EXPECTED_POOL_TIMEOUT="$pool_timeout" \
            HARNESS_PRE_PUSH_FAIL_CLASS="$fail_class" \
            HARNESS_PRE_PUSH_SIGNAL_CLASS="$signal_class" \
            GIT_INDEX_FILE="$case_dir/index" \
            GIT_DIR="$case_dir/git-dir" \
            GIT_WORK_TREE="$case_dir/work-tree" \
            "$hook" >"$output" 2>&1
        status=$?
    else
        env \
            -u HARNESS_DATABASE_URL \
            -u HARNESS_DATABASE_POOL_MAX_CONNECTIONS \
            -u HARNESS_DATABASE_POOL_ACQUIRE_TIMEOUT_SECS \
            PATH="$fake_bin:$PATH" \
            TMPDIR="$case_tmp" \
            XDG_CONFIG_HOME="$global_xdg" \
            HARNESS_PRE_PUSH_FAKE_CARGO=1 \
            HARNESS_PRE_PUSH_LOG="$log" \
            HARNESS_PRE_PUSH_GLOBAL_XDG="$global_xdg" \
            HARNESS_PRE_PUSH_EXPECTED_DATABASE_URL="$secret_url" \
            HARNESS_PRE_PUSH_EXPECTED_POOL_MAX="$pool_max" \
            HARNESS_PRE_PUSH_EXPECTED_POOL_TIMEOUT="$pool_timeout" \
            HARNESS_PRE_PUSH_FAIL_CLASS="$fail_class" \
            HARNESS_PRE_PUSH_SIGNAL_CLASS="$signal_class" \
            GIT_INDEX_FILE="$case_dir/index" \
            GIT_DIR="$case_dir/git-dir" \
            GIT_WORK_TREE="$case_dir/work-tree" \
            "$hook" >"$output" 2>&1
        status=$?
    fi
    set -e

    assert_equals "$expected_status" "$status" "$name status"
    assert_not_contains "$output" "$secret_url" "$name output secret disclosure"
    assert_not_contains "$log" "$secret_url" "$name log secret disclosure"
    assert_isolation_removed "$case_tmp"

    if [ "$expected_status" = "0" ]; then
        assert_contains "$output" "=== pre-push: all checks passed ===" "$name success marker"
    else
        assert_not_contains "$output" "=== pre-push: all checks passed ===" "$name failure marker"
    fi
}

classes() {
    cut -f1 "$1" | paste -sd, -
}

assert_all_git_variables_unset() {
    local log="$1"
    if grep -Evq $'git_index_set=0\tgit_dir_set=0\tgit_work_tree_set=0$' "$log"; then
        fail "git hook variables leaked into a cargo invocation in $log"
    fi
}

run_case db_less_success unset
db_less_log="$test_root/db_less_success/cargo.log"
assert_equals "clippy,workspace,workflow" "$(classes "$db_less_log")" "DB-less command order"
assert_contains "$db_less_log" $'workspace\targv=test --workspace --exclude harness-server --exclude harness-workflow --lib\tdb_set=0' "DB-less workspace command"
assert_contains "$db_less_log" $'workflow\targv=test -p harness-workflow --lib -- --skip runtime::tests::remote_host_lease\tdb_set=0' "DB-less workflow filter"
assert_contains "$db_less_log" $'pool_max_set=0\tpool_max_matches=0\tpool_timeout_set=0' "DB-less pool isolation"
assert_contains "$db_less_log" "xdg_is_global=0" "DB-less XDG isolation"
assert_contains "$test_root/db_less_success/output.log" "deferred 6 runtime::tests::remote_host_lease PostgreSQL tests" "DB-less deferred coverage message"
assert_all_git_variables_unset "$db_less_log"

run_case empty_url_success empty
assert_equals "clippy,workspace,workflow" "$(classes "$test_root/empty_url_success/cargo.log")" "empty URL command order"

run_case db_configured_success configured
db_configured_log="$test_root/db_configured_success/cargo.log"
assert_equals "clippy,workspace,workflow,server" "$(classes "$db_configured_log")" "DB-configured command order"
assert_contains "$db_configured_log" $'workspace\targv=test --workspace --exclude harness-server --exclude harness-workflow --lib\tdb_set=0' "DB-configured isolated workspace command"
assert_contains "$db_configured_log" $'workflow\targv=test -p harness-workflow --lib\tdb_set=1\tdb_matches=1\tpool_max_set=1\tpool_max_matches=1\tpool_timeout_set=1\tpool_timeout_matches=1' "full workflow environment"
assert_contains "$db_configured_log" $'server\targv=test -p harness-server --lib\tdb_set=1\tdb_matches=1\tpool_max_set=1\tpool_max_matches=1\tpool_timeout_set=1\tpool_timeout_matches=1' "server environment"
assert_contains "$db_configured_log" "xdg_is_global=1" "DB-configured XDG preservation"
assert_all_git_variables_unset "$db_configured_log"

run_case fail_clippy unset clippy "" 41
assert_equals "clippy" "$(classes "$test_root/fail_clippy/cargo.log")" "clippy failure stops hook"

run_case fail_workspace unset workspace "" 41
assert_equals "clippy,workspace" "$(classes "$test_root/fail_workspace/cargo.log")" "workspace failure stops hook"

run_case fail_workflow_db_less unset workflow "" 41
assert_equals "clippy,workspace,workflow" "$(classes "$test_root/fail_workflow_db_less/cargo.log")" "DB-less workflow failure stops hook"

run_case fail_workflow_db_configured configured workflow "" 41
assert_equals "clippy,workspace,workflow" "$(classes "$test_root/fail_workflow_db_configured/cargo.log")" "full workflow failure stops hook"

run_case fail_server configured server "" 41
assert_equals "clippy,workspace,workflow,server" "$(classes "$test_root/fail_server/cargo.log")" "server failure stops hook"

run_case signal_workspace unset "" workspace 143
assert_equals "clippy,workspace" "$(classes "$test_root/signal_workspace/cargo.log")" "signal stops hook"

echo "test-pre-push-hook: all cases passed"
