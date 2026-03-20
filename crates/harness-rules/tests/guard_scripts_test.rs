/// Integration tests: run each new guard script against synthetic fixtures and
/// verify that violations are detected (positive case) and clean code passes
/// (negative case).
///
/// Guards are located at `.harness/guards/` relative to the workspace root.
/// Fixtures are written to a tempdir for each test.
use std::path::{Path, PathBuf};
use std::process::Command;

// ---------------------------------------------------------------------------
// Helper utilities
// ---------------------------------------------------------------------------

fn workspace_root() -> PathBuf {
    // CARGO_MANIFEST_DIR points to crates/harness-rules; go up two levels.
    let manifest = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    manifest
        .parent()
        .and_then(|p| p.parent())
        .expect("failed to locate workspace root from CARGO_MANIFEST_DIR")
        .to_path_buf()
}

fn guard_path(name: &str) -> PathBuf {
    workspace_root().join(".harness").join("guards").join(name)
}

/// Run a guard script with `project_root` as the argument.
/// Returns (stdout, exit_code).
fn run_guard(script: &Path, project_root: &Path) -> (String, i32) {
    let output = Command::new("bash")
        .arg(script)
        .arg(project_root)
        .output()
        .unwrap_or_else(|e| panic!("failed to run guard {:?}: {}", script, e));
    let stdout = String::from_utf8_lossy(&output.stdout).into_owned();
    let code = output.status.code().unwrap_or(-1);
    (stdout, code)
}

/// Assert a guard finds at least one violation (stdout non-empty, exit 1).
fn assert_violation(script: &Path, project_root: &Path, rule_id: &str) {
    let (stdout, code) = run_guard(script, project_root);
    assert!(
        stdout.contains(rule_id),
        "guard {:?} expected to find {} violation but stdout was:\n{}",
        script.file_name().unwrap(),
        rule_id,
        stdout,
    );
    assert_eq!(
        code,
        1,
        "guard {:?} should exit 1 when violations found, got {}",
        script.file_name().unwrap(),
        code
    );
}

/// Assert a guard finds no violations (stdout empty, exit 0).
fn assert_clean(script: &Path, project_root: &Path) {
    let (stdout, code) = run_guard(script, project_root);
    assert!(
        stdout.trim().is_empty(),
        "guard {:?} expected no violations but got:\n{}",
        script.file_name().unwrap(),
        stdout,
    );
    assert_eq!(
        code,
        0,
        "guard {:?} should exit 0 on clean code, got {}",
        script.file_name().unwrap(),
        code
    );
}

// ---------------------------------------------------------------------------
// RS-01: nested lock acquisition
// ---------------------------------------------------------------------------

#[test]
fn rs01_detects_nested_lock_acquisition() {
    let dir = tempfile::tempdir().unwrap();
    let src = dir.path().join("src");
    std::fs::create_dir_all(&src).unwrap();
    std::fs::write(
        src.join("lib.rs"),
        r#"
fn bad(a: &RwLock<u32>, b: &RwLock<u32>) {
    let _x = a.read().unwrap();
    let _y = b.write().unwrap(); // nested — risk of deadlock
}
"#,
    )
    .unwrap();
    assert_violation(&guard_path("rs-01-nested-locks.sh"), dir.path(), "RS-01");
}

#[test]
fn rs01_passes_single_lock_acquisition() {
    let dir = tempfile::tempdir().unwrap();
    let src = dir.path().join("src");
    std::fs::create_dir_all(&src).unwrap();
    std::fs::write(
        src.join("lib.rs"),
        r#"
fn good(a: &RwLock<u32>) -> u32 {
    *a.read().unwrap()
}
"#,
    )
    .unwrap();
    assert_clean(&guard_path("rs-01-nested-locks.sh"), dir.path());
}

// ---------------------------------------------------------------------------
// RS-02: TOCTOU get() + insert()
// ---------------------------------------------------------------------------

#[test]
fn rs02_detects_toctou_get_then_insert() {
    let dir = tempfile::tempdir().unwrap();
    let src = dir.path().join("src");
    std::fs::create_dir_all(&src).unwrap();
    std::fs::write(
        src.join("cache.rs"),
        r#"
fn populate(map: &mut HashMap<u32, String>, key: u32) {
    if map.get(&key).is_none() {
        map.insert(key, "value".to_string()); // TOCTOU
    }
}
"#,
    )
    .unwrap();
    assert_violation(&guard_path("rs-02-toctou.sh"), dir.path(), "RS-02");
}

#[test]
fn rs02_passes_entry_api() {
    let dir = tempfile::tempdir().unwrap();
    let src = dir.path().join("src");
    std::fs::create_dir_all(&src).unwrap();
    std::fs::write(
        src.join("cache.rs"),
        r#"
fn populate(map: &mut HashMap<u32, String>, key: u32) {
    map.entry(key).or_insert_with(|| "value".to_string());
}
"#,
    )
    .unwrap();
    assert_clean(&guard_path("rs-02-toctou.sh"), dir.path());
}

// ---------------------------------------------------------------------------
// RS-02B: SQL TOCTOU — SELECT then INSERT on same table
// ---------------------------------------------------------------------------

#[test]
fn rs02b_detects_sql_select_then_insert() {
    let dir = tempfile::tempdir().unwrap();
    let src = dir.path().join("src");
    std::fs::create_dir_all(&src).unwrap();
    std::fs::write(
        src.join("store.rs"),
        r#"
async fn persist(pool: &SqlitePool, id: &str) -> Result<()> {
    let existing = sqlx::query("SELECT id FROM findings WHERE rule_id = ?")
        .bind(id)
        .fetch_optional(pool)
        .await?;
    if existing.is_none() {
        sqlx::query("INSERT INTO findings (rule_id) VALUES (?)")
            .bind(id)
            .execute(pool)
            .await?;
    }
    Ok(())
}
"#,
    )
    .unwrap();
    assert_violation(&guard_path("rs-02b-sql-toctou.sh"), dir.path(), "RS-02B");
}

#[test]
fn rs02b_passes_insert_or_ignore() {
    let dir = tempfile::tempdir().unwrap();
    let src = dir.path().join("src");
    std::fs::create_dir_all(&src).unwrap();
    std::fs::write(
        src.join("store.rs"),
        r#"
async fn persist(pool: &SqlitePool, id: &str) -> Result<()> {
    sqlx::query("INSERT OR IGNORE INTO findings (rule_id) VALUES (?)")
        .bind(id)
        .execute(pool)
        .await?;
    Ok(())
}
"#,
    )
    .unwrap();
    assert_clean(&guard_path("rs-02b-sql-toctou.sh"), dir.path());
}

// ---------------------------------------------------------------------------
// RS-10: silent Result discard
// ---------------------------------------------------------------------------

#[test]
fn rs10_detects_let_underscore_discard() {
    let dir = tempfile::tempdir().unwrap();
    let src = dir.path().join("src");
    std::fs::create_dir_all(&src).unwrap();
    std::fs::write(
        src.join("db.rs"),
        r#"
fn run(conn: &Conn) {
    let _ = conn.execute("UPDATE items SET done = 1");
}
"#,
    )
    .unwrap();
    assert_violation(&guard_path("rs-10-silent-result.sh"), dir.path(), "RS-10");
}

#[test]
fn rs10_passes_explicit_error_handling() {
    let dir = tempfile::tempdir().unwrap();
    let src = dir.path().join("src");
    std::fs::create_dir_all(&src).unwrap();
    std::fs::write(
        src.join("db.rs"),
        r#"
fn run(conn: &Conn) -> Result<()> {
    conn.execute("UPDATE items SET done = 1")?;
    Ok(())
}
"#,
    )
    .unwrap();
    assert_clean(&guard_path("rs-10-silent-result.sh"), dir.path());
}

// ---------------------------------------------------------------------------
// RS-12: dual system for same responsibility
// ---------------------------------------------------------------------------

#[test]
fn rs12_detects_task_and_todo_coexistence() {
    let dir = tempfile::tempdir().unwrap();
    let src = dir.path().join("src");
    std::fs::create_dir_all(&src).unwrap();
    std::fs::write(
        src.join("task.rs"),
        r#"
pub struct TaskItem { pub id: u32 }
"#,
    )
    .unwrap();
    std::fs::write(
        src.join("todo.rs"),
        r#"
pub struct TodoItem { pub id: u32 }
"#,
    )
    .unwrap();
    assert_violation(&guard_path("rs-12-dual-system.sh"), dir.path(), "RS-12");
}

#[test]
fn rs12_passes_single_domain_model() {
    let dir = tempfile::tempdir().unwrap();
    let src = dir.path().join("src");
    std::fs::create_dir_all(&src).unwrap();
    std::fs::write(
        src.join("task.rs"),
        r#"
pub struct TaskItem { pub id: u32 }
pub struct TaskList { pub items: Vec<TaskItem> }
"#,
    )
    .unwrap();
    assert_clean(&guard_path("rs-12-dual-system.sh"), dir.path());
}

// ---------------------------------------------------------------------------
// SEC-03: XSS via innerHTML
// ---------------------------------------------------------------------------

#[test]
fn sec03_detects_innerhtml_assignment() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(
        dir.path().join("app.js"),
        r#"
function render(data) {
    element.innerHTML = data.userContent; // XSS risk
}
"#,
    )
    .unwrap();
    assert_violation(&guard_path("sec-03-xss.sh"), dir.path(), "SEC-03");
}

#[test]
fn sec03_passes_sanitized_innerhtml() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(
        dir.path().join("app.js"),
        r#"
function render(data) {
    element.innerHTML = DOMPurify.sanitize(data.userContent);
}
"#,
    )
    .unwrap();
    assert_clean(&guard_path("sec-03-xss.sh"), dir.path());
}

// ---------------------------------------------------------------------------
// SEC-04: unauthenticated API endpoints
// ---------------------------------------------------------------------------

#[test]
fn sec04_detects_unauthenticated_route() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(
        dir.path().join("routes.js"),
        r#"
router.get('/admin/users', (req, res) => {
    res.json(db.getUsers());
});
"#,
    )
    .unwrap();
    assert_violation(&guard_path("sec-04-unauth-api.sh"), dir.path(), "SEC-04");
}

#[test]
fn sec04_passes_authenticated_route() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(
        dir.path().join("routes.js"),
        r#"
router.get('/admin/users', authenticate, (req, res) => {
    res.json(db.getUsers());
});
"#,
    )
    .unwrap();
    assert_clean(&guard_path("sec-04-unauth-api.sh"), dir.path());
}

// ---------------------------------------------------------------------------
// SEC-07: path traversal
// ---------------------------------------------------------------------------

#[test]
fn sec07_detects_path_traversal_in_js() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(
        dir.path().join("server.js"),
        r#"
app.get('/file', (req, res) => {
    const filePath = path.join(__dirname, req.query.name);
    res.sendFile(filePath);
});
"#,
    )
    .unwrap();
    assert_violation(
        &guard_path("sec-07-path-traversal.sh"),
        dir.path(),
        "SEC-07",
    );
}

#[test]
fn sec07_passes_validated_path() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(
        dir.path().join("server.js"),
        r#"
app.get('/file', (req, res) => {
    const filePath = path.resolve(__dirname, 'static', req.query.name);
    if (!filePath.startsWith(path.resolve(__dirname, 'static'))) {
        return res.status(403).send('Forbidden');
    }
    res.sendFile(filePath);
});
"#,
    )
    .unwrap();
    assert_clean(&guard_path("sec-07-path-traversal.sh"), dir.path());
}
