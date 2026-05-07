//! SQL statement splitter for the Postgres migrator.
//!
//! Lives in a separate module so `db_pg.rs` stays under the project's per-file
//! line limit. Used by `PgMigrator::apply` to chunk a multi-statement migration
//! body into individual statements before submitting them to `sqlx::query`.

/// Split a Postgres SQL string into individual statements, correctly handling:
/// - Single-quoted string literals (with `''` escape)
/// - Double-quoted identifiers (with `""` escape)
/// - Line comments (`-- ... \n`)
/// - Block comments (`/* ... */`)
/// - Dollar-quoted strings (`$$...$$` or `$tag$...$tag$`), including PL/pgSQL bodies
///
/// Naive `split(';')` would crash mid-migration on any embedded semicolon
/// inside the constructs above, so this splitter walks the string with a
/// small state machine instead.
pub(crate) fn pg_split_statements(sql: &str) -> Vec<String> {
    let mut statements: Vec<String> = Vec::new();
    let mut current = String::new();
    let mut chars = sql.chars().peekable();

    while let Some(ch) = chars.next() {
        match ch {
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
            '/' if chars.peek() == Some(&'*') => {
                current.push(ch);
                if let Some(c) = chars.next() {
                    current.push(c);
                }
                while let Some(c) = chars.next() {
                    current.push(c);
                    if c == '*' && chars.peek() == Some(&'/') {
                        if let Some(c2) = chars.next() {
                            current.push(c2);
                        }
                        break;
                    }
                }
            }
            '\'' => {
                current.push(ch);
                while let Some(c) = chars.next() {
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
            '"' => {
                current.push(ch);
                while let Some(c) = chars.next() {
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
            '$' => {
                let mut tag = String::from('$');
                let mut speculative: Vec<char> = Vec::new();
                let mut is_dollar_quote = false;
                loop {
                    match chars.peek().copied() {
                        Some(c) if c.is_ascii_alphanumeric() || c == '_' => {
                            let Some(c) = chars.next() else { break };
                            speculative.push(c);
                            tag.push(c);
                        }
                        Some('$') => {
                            if let Some(c) = chars.next() {
                                tag.push(c);
                            }
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
            ';' => {
                let stmt = current.trim().to_string();
                if !stmt.is_empty() {
                    statements.push(stmt);
                }
                current.clear();
            }
            c => current.push(c),
        }
    }

    let trailing = current.trim().to_string();
    if !trailing.is_empty() {
        statements.push(trailing);
    }
    statements
}

#[cfg(test)]
mod tests {
    use super::pg_split_statements;

    #[test]
    fn single_statement_no_semicolon() {
        assert_eq!(pg_split_statements("SELECT 1"), vec!["SELECT 1"]);
    }

    #[test]
    fn two_statements_separated_by_semicolon() {
        assert_eq!(
            pg_split_statements("SELECT 1; SELECT 2"),
            vec!["SELECT 1", "SELECT 2"]
        );
    }

    #[test]
    fn semicolon_inside_string_literal_not_split() {
        assert_eq!(
            pg_split_statements("SELECT ';' AS x"),
            vec!["SELECT ';' AS x"]
        );
    }

    #[test]
    fn escaped_quote_inside_string_literal_preserved() {
        assert_eq!(
            pg_split_statements("SELECT 'it''s; ok' AS x"),
            vec!["SELECT 'it''s; ok' AS x"]
        );
    }

    #[test]
    fn semicolon_inside_double_quoted_identifier_not_split() {
        assert_eq!(
            pg_split_statements("SELECT 1 AS \"weird;name\""),
            vec!["SELECT 1 AS \"weird;name\""]
        );
    }

    #[test]
    fn semicolon_in_line_comment_not_split() {
        assert_eq!(
            pg_split_statements("SELECT 1 -- trailing; comment\n, 2"),
            vec!["SELECT 1 -- trailing; comment\n, 2"]
        );
    }

    #[test]
    fn semicolon_in_block_comment_not_split() {
        assert_eq!(
            pg_split_statements("SELECT /* ; */ 1; SELECT 2"),
            vec!["SELECT /* ; */ 1", "SELECT 2"]
        );
    }

    #[test]
    fn dollar_quoted_block_with_inner_semicolons_preserved() {
        let sql = "DO $$ BEGIN PERFORM 1; PERFORM 2; END $$";
        assert_eq!(pg_split_statements(sql), vec![sql.to_string()]);
    }

    #[test]
    fn tagged_dollar_quote_preserved() {
        let sql = "CREATE FUNCTION f() RETURNS void AS $body$ BEGIN PERFORM 1; END $body$ LANGUAGE plpgsql";
        assert_eq!(pg_split_statements(sql), vec![sql.to_string()]);
    }

    #[test]
    fn lone_dollar_does_not_open_a_quote() {
        assert_eq!(pg_split_statements("SELECT $1"), vec!["SELECT $1"]);
    }

    #[test]
    fn empty_input_returns_empty_vec() {
        assert!(pg_split_statements("").is_empty());
        assert!(pg_split_statements("   ;\n;\t").is_empty());
    }

    #[test]
    fn trailing_semicolon_does_not_emit_empty_statement() {
        assert_eq!(pg_split_statements("SELECT 1;"), vec!["SELECT 1"]);
    }

    #[test]
    fn create_table_then_index_split_into_two() {
        let sql = "CREATE TABLE t (id BIGINT); CREATE INDEX i ON t(id)";
        let stmts = pg_split_statements(sql);
        assert_eq!(stmts.len(), 2);
        assert!(stmts[0].starts_with("CREATE TABLE"));
        assert!(stmts[1].starts_with("CREATE INDEX"));
    }
}
