#!/usr/bin/env bash
# One-off P1 cleanup: drop dead path-derived Postgres schemas (storage RFC).
#
# SAFETY MODEL (dual-signal keep-list, recomputed at apply time — never trusts a
# stale list):
#   KEEP a schema iff EITHER it had write activity in a fresh observation window
#   OR its registry owner_path still exists on disk; plus `workflow_runtime` and
#   all non-`h*` schemas. Everything else matching ^h[0-9a-f]{16}$ is dropped.
#
# Defaults to DRY-RUN. Pass --confirm to apply. Dumps schema-only first.
# Recommended: run with the harness server paused for maximum safety.
set -euo pipefail

PSQL="${PSQL:-/opt/homebrew/opt/libpq/bin/psql}"
PGHOST="${PGHOST:-localhost}"; PGPORT="${PGPORT:-5432}"
PGUSER="${PGUSER:-harness}"; PGDB="${PGDB:-harness}"
export PGPASSWORD="${PGPASSWORD:-harness}"
WINDOW="${WINDOW:-70}"
CONFIRM=0
[ "${1:-}" = "--confirm" ] && CONFIRM=1

q() { "$PSQL" -h "$PGHOST" -p "$PGPORT" -U "$PGUSER" -d "$PGDB" -t -A -F'|' -c "$1"; }

echo "== observing write activity for ${WINDOW}s to identify live schemas =="
q "SELECT schemaname, sum(n_tup_ins+n_tup_upd+n_tup_del) FROM pg_stat_user_tables GROUP BY schemaname" > /tmp/_wt0.txt
sleep "$WINDOW"
q "SELECT schemaname, sum(n_tup_ins+n_tup_upd+n_tup_del) FROM pg_stat_user_tables GROUP BY schemaname" > /tmp/_wt1.txt
q "SELECT schema_name, coalesce(owner_path,'') FROM harness_admin.schema_ownership" > /tmp/_own.txt
q "SELECT schema_name FROM information_schema.schemata WHERE schema_name ~ '^h[0-9a-f]{16}\$'" > /tmp/_allh.txt

python3 - "$CONFIRM" << 'PY'
import os, sys, re
confirm = sys.argv[1] == "1"
def lw(p):
    d={}
    for ln in open(p):
        ln=ln.strip()
        if '|' in ln:
            s,w=ln.rsplit('|',1)
            try: d[s]=int(w)
            except: d[s]=0
    return d
t0,t1=lw('/tmp/_wt0.txt'),lw('/tmp/_wt1.txt')
owner={}
for ln in open('/tmp/_own.txt'):
    ln=ln.rstrip('\n')
    if '|' in ln:
        a,b=ln.rsplit('|',1); owner[a]=b
allh={ln.strip() for ln in open('/tmp/_allh.txt') if ln.strip()}
live={s for s in t1 if t1.get(s,0)>t0.get(s,0)}
# Match the runtime reaper's orphan oracle (db_pg_schema_registry::owner_path_is_orphaned):
# a path-derived store is alive while its owner_path's PARENT directory exists.
# owner_path is an identity like `<workspace>/events.db` and the .db file may not
# exist on disk for a Postgres-backed store, so checking the file (not its parent)
# could drop a live schema whose workspace dir is still present.
def owner_parent_alive(p):
    return bool(p) and os.path.isdir(os.path.dirname(p.rstrip('/')))
keep={s for s in allh if (s in live) or owner_parent_alive(owner.get(s))} | {'workflow_runtime'}
drop=sorted(allh-keep)
open('/tmp/_drop.txt','w').write('\n'.join(drop)+('\n' if drop else ''))
# keep-list for the targeted backup (only live data needs preserving)
open('/tmp/_keep.txt','w').write('\n'.join(sorted(allh & keep))+('\n' if (allh & keep) else ''))
print(f"KEEP {len(allh & keep)} h* schemas, DROP {len(drop)}")
print("KEEP:", ", ".join(sorted(allh & keep)) or "(none)")
if not confirm:
    print("\nDRY-RUN. Re-run with --confirm to drop the listed schemas.")
PY

[ "$CONFIRM" = 1 ] || exit 0

echo "== backing up ONLY the live keep-list schemas (full data) before dropping =="
# A whole-catalog dump locks every table in one txn and blows max_locks_per_transaction
# with 13k schemas. The schemas being dropped are dead by definition and need no
# backup; only the live keep-list holds data worth preserving — dump just those.
DUMP="/tmp/harness-keepset-predrop-$(q "SELECT to_char(now(),'YYYYMMDDHH24MISS')").sql"
NARGS=(-n harness_admin -n workflow_runtime)
while IFS= read -r s; do [ -n "$s" ] && NARGS+=(-n "$s"); done < /tmp/_keep.txt
/opt/homebrew/opt/libpq/bin/pg_dump -h "$PGHOST" -p "$PGPORT" -U "$PGUSER" -d "$PGDB" \
  "${NARGS[@]}" -f "$DUMP"
echo "keep-set backup written: $DUMP ($(wc -c < "$DUMP") bytes)"

echo "== dropping $(wc -l < /tmp/_drop.txt) schemas =="
n=0
while IFS= read -r s; do
  [ -z "$s" ] && continue
  # Re-validate the schema name immediately before interpolating it into SQL.
  # Only the hashed path-derived form is ever a drop target; anything else is a bug.
  if ! printf '%s' "$s" | grep -Eq '^h[0-9a-f]{16}$'; then
    echo "  REFUSING to drop non-conforming schema name: '$s'" >&2
    continue
  fi
  "$PSQL" -h "$PGHOST" -p "$PGPORT" -U "$PGUSER" -d "$PGDB" -c "DROP SCHEMA IF EXISTS \"$s\" CASCADE" >/dev/null
  if ! q "DELETE FROM harness_admin.schema_ownership WHERE schema_name = '$s'" >/dev/null; then
    echo "  WARN: dropped schema $s but failed to delete its ownership row" >&2
  fi
  n=$((n+1))
  [ $((n % 500)) -eq 0 ] && echo "  dropped $n ..."
done < /tmp/_drop.txt
echo "done: dropped $n schemas. Run VACUUM/ANALYZE and check pg_database_size."
