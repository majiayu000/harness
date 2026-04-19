use sqlx::postgres::{PgConnectOptions, PgPool, PgPoolOptions};
use std::collections::HashSet;
use std::str::FromStr as _;

use crate::db::Migration;

/// Create a Postgres connection pool for the given DATABASE_URL.
///
/// Uses 8 max connections with a 10-second acquire timeout.
pub async fn pg_open_pool(database_url: &str) -> anyhow::Result<PgPool> {
    let pool = PgPoolOptions::new()
        .max_connections(8)
        .acquire_timeout(std::time::Duration::from_secs(10))
        .connect(database_url)
        .await?;
    Ok(pool)
}

/// Create a Postgres connection pool where every connection has `search_path`
/// set to `schema`. Used to give each store an isolated schema namespace.
///
/// Sets search_path via BOTH the connection `options` startup parameter AND an
/// `after_connect` hook that runs `SET search_path TO <schema>` per connection.
/// The after_connect hook is required for Supabase pgbouncer pooler, which
/// silently strips the startup `options` parameter, causing all DDL to fall
/// back to the default `public` schema and all stores to share a single
/// `schema_migrations` table (migration-number collision bug).
pub async fn pg_open_pool_schematized(database_url: &str, schema: &str) -> anyhow::Result<PgPool> {
    let opts = PgConnectOptions::from_str(database_url)?.options([("search_path", schema)]);
    let schema_for_hook = schema.to_string();
    let pool = PgPoolOptions::new()
        .max_connections(8)
        .acquire_timeout(std::time::Duration::from_secs(10))
        .after_connect(move |conn, _meta| {
            let schema = schema_for_hook.clone();
            Box::pin(async move {
                // Quoted identifier guards against injection from the schema name;
                // schema here is constructed from a hash hex string so quoting is
                // defensive in depth rather than strictly required.
                let stmt = format!("SET search_path TO \"{}\"", schema);
                sqlx::query(&stmt).execute(conn).await?;
                Ok(())
            })
        })
        .connect_with(opts)
        .await?;
    Ok(pool)
}

/// Split a SQL string into individual statements, correctly handling:
/// - Single-quoted string literals (including escaped `''`)
/// - Double-quoted identifiers (including escaped `""`)
/// - Line comments (`--`)
/// - Block comments (`/* */`)
/// - Dollar-quoted strings (`$$...$$` or `$tag$...$tag$`)
fn pg_split_statements(sql: &str) -> Vec<String> {
    let mut statements: Vec<String> = Vec::new();
    let mut current = String::new();
    let mut chars = sql.chars().peekable();

    while let Some(ch) = chars.next() {
        match ch {
            // Line comment: -- ... \n
            '-' if chars.peek() == Some(&'-') => {
                current.push(ch);
                if let Some(c) = chars.next() {
                    current.push(c);
                }
                for c in chars.by_ref() {
                    current.push(c);
                    if c == '\n' {
                        break;
                    }
                }
            }
            // Block comment: /* ... */
            '/' if chars.peek() == Some(&'*') => {
                current.push(ch);
                if let Some(c) = chars.next() {
                    current.push(c);
                }
                loop {
                    match chars.next() {
                        None => break,
                        Some(c) => {
                            current.push(c);
                            if c == '*' && chars.peek() == Some(&'/') {
                                if let Some(c) = chars.next() {
                                    current.push(c);
                                }
                                break;
                            }
                        }
                    }
                }
            }
            // Single-quoted string: '...' with '' escape
            '\'' => {
                current.push(ch);
                loop {
                    match chars.next() {
                        None => break,
                        Some(c) => {
                            current.push(c);
                            if c == '\'' {
                                if chars.peek() == Some(&'\'') {
                                    if let Some(q) = chars.next() {
                                        current.push(q);
                                    }
                                } else {
                                    break;
                                }
                            }
                        }
                    }
                }
            }
            // Double-quoted identifier: "..." with "" escape
            '"' => {
                current.push(ch);
                loop {
                    match chars.next() {
                        None => break,
                        Some(c) => {
                            current.push(c);
                            if c == '"' {
                                if chars.peek() == Some(&'"') {
                                    if let Some(q) = chars.next() {
                                        current.push(q);
                                    }
                                } else {
                                    break;
                                }
                            }
                        }
                    }
                }
            }
            // Dollar-quoted string: $tag$...$tag$ (tag may be empty: $$...$$)
            '$' => {
                let mut tag = String::from('$');
                let mut is_dollar_quote = false;
                let mut speculative: Vec<char> = Vec::new();
                loop {
                    match chars.peek().copied() {
                        Some(c) if c.is_ascii_alphanumeric() || c == '_' => {
                            let Some(c) = chars.next() else {
                                break;
                            };
                            speculative.push(c);
                            tag.push(c);
                        }
                        Some('$') => {
                            if chars.next().is_none() {
                                break;
                            }
                            tag.push('$');
                            is_dollar_quote = true;
                            break;
                        }
                        _ => break,
                    }
                }
                if is_dollar_quote {
                    current.push_str(&tag);
                    let mut body = String::new();
                    loop {
                        match chars.next() {
                            None => {
                                current.push_str(&body);
                                break;
                            }
                            Some(c) => {
                                body.push(c);
                                if body.ends_with(&tag) {
                                    current.push_str(&body);
                                    break;
                                }
                            }
                        }
                    }
                } else {
                    current.push('$');
                    for c in speculative {
                        current.push(c);
                    }
                }
            }
            // Statement separator
            ';' => {
                let stmt = current.trim().to_string();
                if !stmt.is_empty() {
                    statements.push(stmt);
                }
                current.clear();
            }
            c => {
                current.push(c);
            }
        }
    }

    let trailing = current.trim().to_string();
    if !trailing.is_empty() {
        statements.push(trailing);
    }

    statements
}

fn pg_duplicate_column_error(statement: &str, error: &sqlx::Error) -> bool {
    if !statement.to_ascii_uppercase().contains("ADD COLUMN") {
        return false;
    }
    match error {
        sqlx::Error::Database(db_err) => db_err.code().as_deref() == Some("42701"),
        _ => false,
    }
}

/// Runs versioned SQL migrations against a Postgres pool.
///
/// Maintains a `schema_migrations` table to track which versions have been
/// applied. Safe to call on every startup — already-applied versions are
/// skipped. All migrations run inside a transaction (Postgres supports
/// transactional DDL). Duplicate-column errors on `ALTER TABLE ADD COLUMN`
/// are silently ignored for idempotency.
pub struct PgMigrator<'a> {
    pool: &'a PgPool,
    migrations: &'a [Migration],
}

impl<'a> PgMigrator<'a> {
    pub fn new(pool: &'a PgPool, migrations: &'a [Migration]) -> Self {
        Self { pool, migrations }
    }

    pub async fn run(&self) -> anyhow::Result<()> {
        sqlx::query(
            "CREATE TABLE IF NOT EXISTS schema_migrations (
                version     BIGINT PRIMARY KEY,
                description TEXT NOT NULL,
                applied_at  TIMESTAMPTZ NOT NULL DEFAULT CURRENT_TIMESTAMP
            )",
        )
        .execute(self.pool)
        .await?;

        let rows: Vec<(i64,)> =
            sqlx::query_as("SELECT version FROM schema_migrations ORDER BY version ASC")
                .fetch_all(self.pool)
                .await?;
        let applied: HashSet<u32> = rows.into_iter().map(|(v,)| v as u32).collect();

        let mut pending: Vec<&Migration> = self
            .migrations
            .iter()
            .filter(|m| !applied.contains(&m.version))
            .collect();
        pending.sort_by_key(|m| m.version);

        for migration in pending {
            self.apply(migration).await?;
        }
        Ok(())
    }

    async fn apply(&self, migration: &Migration) -> anyhow::Result<()> {
        let mut tx = self.pool.begin().await?;
        for stmt in pg_split_statements(migration.sql) {
            if let Err(e) = sqlx::query(&stmt).execute(&mut *tx).await {
                if pg_duplicate_column_error(&stmt, &e) {
                    continue;
                }
                return Err(anyhow::anyhow!(
                    "migration v{} '{}' failed: {} [sql: {}]",
                    migration.version,
                    migration.description,
                    e,
                    stmt
                ));
            }
        }
        sqlx::query("INSERT INTO schema_migrations (version, description) VALUES ($1, $2)")
            .bind(migration.version as i64)
            .bind(migration.description)
            .execute(&mut *tx)
            .await?;
        tx.commit().await?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::pg_split_statements;

    #[test]
    fn single_statement_no_semicolon() {
        let stmts = pg_split_statements("SELECT 1");
        assert_eq!(stmts, vec!["SELECT 1"]);
    }

    #[test]
    fn two_statements_separated_by_semicolon() {
        let stmts = pg_split_statements("SELECT 1; SELECT 2");
        assert_eq!(stmts, vec!["SELECT 1", "SELECT 2"]);
    }

    #[test]
    fn semicolon_inside_string_literal_not_split() {
        let stmts = pg_split_statements("SELECT ';' AS x");
        assert_eq!(stmts, vec!["SELECT \';\' AS x"]);
    }

    #[test]
    fn semicolon_in_line_comment_not_split() {
        let stmts = pg_split_statements("SELECT 1 -- comment;\n, 2");
        assert_eq!(stmts, vec!["SELECT 1 -- comment;\n, 2"]);
    }

    #[test]
    fn semicolon_in_block_comment_not_split() {
        let stmts = pg_split_statements("SELECT /* block; comment */ 1");
        assert_eq!(stmts, vec!["SELECT /* block; comment */ 1"]);
    }

    #[test]
    fn dollar_quoted_block_not_split() {
        let sql = "DO $$ BEGIN PERFORM 1; END $$";
        let stmts = pg_split_statements(sql);
        assert_eq!(stmts, vec!["DO $$ BEGIN PERFORM 1; END $$"]);
    }

    #[test]
    fn v17_migration_produces_exactly_two_statements() {
        let sql = "CREATE TABLE IF NOT EXISTS task_prompts (\n            id         BIGINT GENERATED ALWAYS AS IDENTITY PRIMARY KEY,\n            task_id    TEXT NOT NULL,\n            UNIQUE(task_id)\n        ); CREATE INDEX IF NOT EXISTS idx_task_prompts_task_id\n           ON task_prompts(task_id)";
        let stmts = pg_split_statements(sql);
        assert_eq!(stmts.len(), 2);
        assert!(stmts[0].starts_with("CREATE TABLE"));
        assert!(stmts[1].starts_with("CREATE INDEX"));
    }
}
