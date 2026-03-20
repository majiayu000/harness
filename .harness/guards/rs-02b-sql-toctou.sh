#!/usr/bin/env sh
# RS-02B: Detect SQL-layer TOCTOU — SELECT check followed by INSERT on the
# same table within the same function, without a transaction or INSERT OR IGNORE.
#
# The pattern:
#   let existing = sqlx::query("SELECT ... FROM table WHERE ...").fetch_optional(...).await?;
#   if existing.is_none() {
#       sqlx::query("INSERT INTO table ...").execute(...).await?;
#   }
#
# Fix: Use INSERT OR IGNORE + UNIQUE constraint, or wrap in a transaction with
# a single INSERT ... ON CONFLICT clause.
#
# Output format: FILE:LINE:RS-02B:MESSAGE
# Exit 0 on pass, exit 1 if violations found.

project_root="${1:-}"
if [ -z "${project_root}" ]; then
  exit 0
fi

tmpfile=$(mktemp)

# Detect SELECT ... FROM <table> followed by INSERT INTO <table> within 15 lines
# in the same Rust file. Excludes INSERT OR IGNORE (the correct pattern).
find "${project_root}" -name "*.rs" \
  ! -path "*/target/*" \
  ! -path "*/.git/*" \
  ! -name "*_test.rs" \
  ! -path "*/tests/*" \
  -print0 2>/dev/null \
| xargs -0 awk '
  FNR == 1 {
    select_table = ""
    select_line = 0
  }
  /SELECT.*FROM[[:space:]]+[A-Za-z_]+/ {
    s = $0
    gsub(/.*FROM[[:space:]]+/, "", s)
    gsub(/[^A-Za-z_].*/, "", s)
    if (s != "") {
      select_table = s
      select_line = FNR
    }
  }
  /INSERT[[:space:]]+OR[[:space:]]+IGNORE/ {
    # Correct pattern — reset tracking
    select_table = ""
    select_line = 0
  }
  /INSERT[[:space:]]+INTO[[:space:]]+[A-Za-z_]+/ && !/INSERT[[:space:]]+OR/ {
    s = $0
    gsub(/.*INSERT[[:space:]]+INTO[[:space:]]+/, "", s)
    gsub(/[^A-Za-z_].*/, "", s)
    if (s == select_table && select_line > 0 && (FNR - select_line) <= 15) {
      print FILENAME ":" select_line ":RS-02B:SQL TOCTOU — SELECT FROM " s " then INSERT INTO " s " without INSERT OR IGNORE; use unique constraint + INSERT OR IGNORE or ON CONFLICT"
      select_table = ""
      select_line = 0
    }
  }
  # Reset on function boundary
  /^fn / || /^pub fn / || /^async fn / || /^pub async fn / {
    select_table = ""
    select_line = 0
  }
' 2>/dev/null >> "${tmpfile}"

if [ -s "${tmpfile}" ]; then
  cat "${tmpfile}"
  rm -f "${tmpfile}"
  exit 1
fi

rm -f "${tmpfile}"
exit 0
