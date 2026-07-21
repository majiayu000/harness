#!/usr/bin/env bash
set -euo pipefail
umask 077

die() {
  echo "ERROR: $*" >&2
  exit 2
}

need_tool() {
  local env_name="$1"
  local name="$2"
  local fallback="/opt/homebrew/opt/libpq/bin/$2"
  if [[ -n "${!env_name:-}" ]]; then
    printf '%s\n' "${!env_name}"
    return
  fi
  if command -v "$name" >/dev/null 2>&1; then
    command -v "$name"
    return
  fi
  if [[ -x "$fallback" ]]; then
    printf '%s\n' "$fallback"
    return
  fi
  die "$name not found; set $env_name"
}

validate_phase1_table() {
  local table="$1"
  [[ "$table" =~ ^(thread_db|task_db|eval_store|review_store|h[0-9a-f]{16})\.[a-z_]+$ ]] \
    || die "refusing unexpected table name: $table"
}

write_schema_bootstrap() {
  local tables_file="$1"
  local output_file="$2"
  local schemas=()
  local table schema
  while IFS= read -r table; do
    validate_phase1_table "$table"
    schemas+=("${table%%.*}")
  done < "$tables_file"
  printf '%s\n' "${schemas[@]}" | sort -u | while IFS= read -r schema; do
    printf 'CREATE SCHEMA IF NOT EXISTS "%s";\n' "$schema"
  done > "$output_file"
}

write_row_count_verification() {
  local tables_file="$1"
  local output_file="$2"
  local first=1
  local table schema name
  {
    printf 'COPY (\n'
    while IFS= read -r table; do
      validate_phase1_table "$table"
      schema="${table%%.*}"
      name="${table#*.}"
      if [[ "$first" -eq 0 ]]; then
        printf 'UNION ALL\n'
      fi
      printf 'SELECT '\''%s'\''::text AS "table", count(*)::bigint AS rows FROM "%s"."%s"\n' \
        "$table" "$schema" "$name"
      first=0
    done < "$tables_file"
    printf ') TO STDOUT WITH (FORMAT csv, DELIMITER E'\''\\t'\'', HEADER true);\n'
  } > "$output_file"
}

write_restore_readme() {
  local output_file="$1"
  local created_at="$2"
  local lock_wait_timeout="$3"
  local table_count="$4"
  cat > "$output_file" <<EOF
# Restore phase1 archive

This archive contains Harness Phase 1 contraction data captured before code
removal. It may include shared schemas (\`thread_db\`, \`task_db\`, \`eval_store\`,
\`review_store\`) and path-derived \`h<hex>\` schemas when they contain matching
thread, task, eval, or review tables.

Restore only into an empty scratch database. The custom dump intentionally
contains only selected tables, so create its validated schema containers first:

\`\`\`bash
: "\${SCRATCH_DATABASE_URL:?set SCRATCH_DATABASE_URL to an empty scratch database}"
psql -X --set ON_ERROR_STOP=1 --dbname "\$SCRATCH_DATABASE_URL" \\
  --file schema-bootstrap.sql
pg_restore --exit-on-error --no-owner --no-acl \\
  --dbname "\$SCRATCH_DATABASE_URL" phase1-data.dump
psql -Xq --set ON_ERROR_STOP=1 --dbname "\$SCRATCH_DATABASE_URL" \\
  --file verify-row-counts.sql > restored_table_counts.tsv
diff -u table_counts.tsv restored_table_counts.tsv
\`\`\`

The restore is verified only when \`pg_restore\` exits zero and the row-count
diff is empty. Do not restore directly into a live Harness database. Do not
pass \`--clean\`. Use \`table_counts.tsv\` and \`phase1-data.list\` to confirm
expected objects first.

Created at: $created_at
Lock wait timeout: $lock_wait_timeout
Table count: $table_count
EOF
}

self_test() {
  local test_dir
  test_dir="$(mktemp -d "${TMPDIR:-/tmp}/archive-phase1-data-test.XXXXXX")"
  printf '%s\n' \
    'eval_store.eval_runs' \
    'task_db.tasks' \
    'task_db.workspace_leases' > "$test_dir/tables.txt"
  write_schema_bootstrap "$test_dir/tables.txt" "$test_dir/schema-bootstrap.sql"
  write_row_count_verification "$test_dir/tables.txt" "$test_dir/verify-row-counts.sql"
  write_restore_readme "$test_dir/RESTORE.md" '2026-07-21T00:00:00Z' '5s' '3'
  [[ "$(wc -l < "$test_dir/schema-bootstrap.sql" | tr -d ' ')" == 2 ]]
  grep -Fqx 'CREATE SCHEMA IF NOT EXISTS "eval_store";' "$test_dir/schema-bootstrap.sql"
  grep -Fqx 'CREATE SCHEMA IF NOT EXISTS "task_db";' "$test_dir/schema-bootstrap.sql"
  grep -Fq 'FROM "task_db"."tasks"' "$test_dir/verify-row-counts.sql"
  grep -Fq 'DELIMITER E'\''\t'\''' "$test_dir/verify-row-counts.sql"
  grep -Fq -- '--file schema-bootstrap.sql' "$test_dir/RESTORE.md"
  grep -Fq 'diff -u table_counts.tsv restored_table_counts.tsv' "$test_dir/RESTORE.md"
  rm -r "$test_dir"
  printf 'archive-phase1-data self-test passed\n'
}

if [[ "${1:-}" == "--self-test" ]]; then
  self_test
  exit 0
fi
[[ "$#" -eq 0 ]] || die "usage: $0 [--self-test]"

[[ -n "${HARNESS_DATABASE_URL:-}" ]] || die "HARNESS_DATABASE_URL is required"

PSQL="$(need_tool PSQL psql)"
PG_DUMP="$(need_tool PG_DUMP pg_dump)"
PG_RESTORE="$(need_tool PG_RESTORE pg_restore)"
ARCHIVE_ROOT="${ARCHIVE_ROOT:-archives}"
ARCHIVE_DATE="${ARCHIVE_DATE:-$(date -u +%Y%m%dT%H%M%SZ)}"
OUT_DIR="${ARCHIVE_DIR:-$ARCHIVE_ROOT/phase1-$ARCHIVE_DATE}"
LOCK_WAIT_TIMEOUT="${PG_DUMP_LOCK_WAIT_TIMEOUT:-5s}"

mkdir -p "$ARCHIVE_ROOT"
[[ ! -e "$OUT_DIR" ]] || die "archive already exists: $OUT_DIR"
mkdir "$OUT_DIR"

psql_cmd=("$PSQL" -XAt --set ON_ERROR_STOP=1 "$HARNESS_DATABASE_URL")
TABLE_SQL="
SELECT table_schema || '.' || table_name
FROM information_schema.tables
WHERE table_type = 'BASE TABLE'
  AND (
    (
      table_schema IN ('thread_db', 'task_db', 'eval_store', 'review_store')
      AND table_name IN (
        'schema_migrations', 'threads', 'tasks', 'task_artifacts',
        'task_checkpoints', 'task_prompts', 'workspace_leases',
        'eval_runs', 'eval_artifacts', 'quality_snapshots', 'review_findings'
      )
    )
    OR (
      table_schema ~ '^h[0-9a-f]{16}$'
      AND table_name IN (
        'schema_migrations', 'threads', 'tasks', 'task_artifacts',
        'task_checkpoints', 'task_prompts', 'workspace_leases',
        'eval_runs', 'eval_artifacts', 'quality_snapshots', 'review_findings'
      )
    )
  )
ORDER BY table_schema, table_name"

tables_file="$OUT_DIR/tables.txt"
if ! "${psql_cmd[@]}" -c "$TABLE_SQL" > "$tables_file"; then
  die "failed to query phase1 tables"
fi
mapfile -t tables < "$tables_file"
[[ "${#tables[@]}" -gt 0 ]] || die "no phase1 tables found"
required_tables=(thread_db.threads task_db.tasks)
for required_table in "${required_tables[@]}"; do
  found=0
  for table in "${tables[@]}"; do
    if [[ "$table" == "$required_table" ]]; then
      found=1
      break
    fi
  done
  [[ "$found" -eq 1 ]] || die "required table is missing: $required_table"
done

dump_args=()
{
  printf 'table\trows\n'
  for table in "${tables[@]}"; do
    validate_phase1_table "$table"
    IFS=. read -r schema name <<<"$table"
    count="$("${psql_cmd[@]}" -c "SELECT count(*) FROM \"$schema\".\"$name\"")"
    printf '%s\t%s\n' "$table" "$count"
    dump_args+=(--table="$table")
  done
} > "$OUT_DIR/table_counts.tsv"

write_schema_bootstrap "$tables_file" "$OUT_DIR/schema-bootstrap.sql"
write_row_count_verification "$tables_file" "$OUT_DIR/verify-row-counts.sql"

"$PG_DUMP" \
  --format=custom \
  --no-owner \
  --no-acl \
  --lock-wait-timeout="$LOCK_WAIT_TIMEOUT" \
  "${dump_args[@]}" \
  --file="$OUT_DIR/phase1-data.dump" \
  "$HARNESS_DATABASE_URL"

"$PG_RESTORE" --list "$OUT_DIR/phase1-data.dump" > "$OUT_DIR/phase1-data.list"

write_restore_readme \
  "$OUT_DIR/RESTORE.md" \
  "$(date -u +%Y-%m-%dT%H:%M:%SZ)" \
  "$LOCK_WAIT_TIMEOUT" \
  "${#tables[@]}"

printf 'Archive written: %s\n' "$OUT_DIR"
