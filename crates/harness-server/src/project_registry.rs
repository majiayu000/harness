use anyhow::Context;
use harness_core::db::{Db, DbEntity};
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};
use std::sync::Arc;

pub const DEFAULT_PROJECT_ID: &str = "default";

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ResolvedProject {
    pub id: String,
    pub root: PathBuf,
    pub max_concurrent: Option<u32>,
    pub default_agent: Option<String>,
    pub active: bool,
}

impl ResolvedProject {
    fn from_project(project: Project) -> Self {
        Self {
            id: project.id,
            root: project.root,
            max_concurrent: project.max_concurrent,
            default_agent: project.default_agent,
            active: project.active,
        }
    }
}

fn canonicalize_root(root: &Path) -> anyhow::Result<PathBuf> {
    root.canonicalize()
        .with_context(|| format!("failed to canonicalize project root '{}'", root.display()))
}

fn canonical_root_alias(root: &Path) -> String {
    root.to_string_lossy().into_owned()
}

fn clone_project_root(project: &Project) -> Project {
    Project {
        id: project.id.clone(),
        root: project.root.clone(),
        max_concurrent: project.max_concurrent,
        default_agent: project.default_agent.clone(),
        active: project.active,
        created_at: project.created_at.clone(),
    }
}

fn matches_alias(project: &Project, alias: &Path) -> bool {
    project.root == alias || project.root.ends_with(alias)
}

fn find_exact_root_match<'a>(projects: &'a [Project], root: &Path) -> Option<&'a Project> {
    projects.iter().find(|project| project.root == root)
}

fn canonical_or_raw_root_key(path: &Path) -> String {
    canonicalize_root(path)
        .map(|root| canonical_root_alias(&root))
        .unwrap_or_else(|_| path.to_string_lossy().into_owned())
}

fn resolve_configured_limit_key(
    projects: &[Project],
    default_root: &Path,
    key: &str,
) -> anyhow::Result<String> {
    if key == DEFAULT_PROJECT_ID {
        return Ok(resolve_default_from_projects(projects, default_root)?.id);
    }

    if let Some(project) = resolve_from_projects(projects, key)? {
        return Ok(project.id);
    }

    Ok(canonical_or_raw_root_key(Path::new(key)))
}

fn map_duplicate_root_conflict(existing: &[Project], project: &Project) -> anyhow::Error {
    if let Some(existing_project) = existing.iter().find(|entry| entry.root == project.root) {
        duplicate_root_error(&project.id, &project.root, &existing_project.id)
    } else {
        duplicate_root_error(&project.id, &project.root, "unknown")
    }
}

fn invalid_project_id(id: &str) -> anyhow::Error {
    anyhow::anyhow!("project id '{id}' conflicts with a canonical root alias")
}

fn duplicate_root_error(id: &str, root: &Path, existing_id: &str) -> anyhow::Error {
    anyhow::anyhow!(
        "project '{id}' reuses canonical root '{}' already registered to '{existing_id}'",
        root.display()
    )
}

fn ambiguous_alias_error(alias: &Path, first_id: &str, second_id: &str) -> anyhow::Error {
    anyhow::anyhow!(
        "project alias '{}' is ambiguous between '{first_id}' and '{second_id}'",
        alias.display()
    )
}

fn duplicate_alias_error(id: &str, root: &Path, existing_id: &str) -> anyhow::Error {
    anyhow::anyhow!(
        "project '{id}' root '{}' conflicts with registered id '{existing_id}'",
        root.display()
    )
}

fn find_alias_match<'a>(
    projects: &'a [Project],
    alias: &Path,
) -> anyhow::Result<Option<&'a Project>> {
    let mut matched: Option<&Project> = None;
    for project in projects {
        if matches_alias(project, alias) {
            if let Some(existing) = matched {
                return Err(ambiguous_alias_error(alias, &existing.id, &project.id));
            }
            matched = Some(project);
        }
    }
    Ok(matched)
}

fn ensure_unique_identity(
    existing: &[Project],
    project: &Project,
    canonical_root: &Path,
) -> anyhow::Result<()> {
    if existing
        .iter()
        .any(|entry| entry.id != project.id && entry.id == canonical_root_alias(canonical_root))
    {
        return Err(invalid_project_id(&project.id));
    }

    for entry in existing {
        if entry.id == project.id {
            continue;
        }
        if entry.root == canonical_root {
            return Err(duplicate_root_error(&project.id, canonical_root, &entry.id));
        }
        if entry.id == canonical_root_alias(canonical_root) {
            return Err(duplicate_alias_error(
                &project.id,
                canonical_root,
                &entry.id,
            ));
        }
        // Inverse check: new project's ID must not equal an existing project's
        // canonical root alias, or resolve_project_input would hijack path
        // lookups for that existing project to the new one.
        if project.id == canonical_root_alias(&entry.root) {
            return Err(invalid_project_id(&project.id));
        }
    }

    Ok(())
}

fn normalize_project(project: Project) -> anyhow::Result<Project> {
    let canonical_root = canonicalize_root(&project.root)?;
    if project.id == canonical_root_alias(&canonical_root) {
        return Err(invalid_project_id(&project.id));
    }
    Ok(Project {
        root: canonical_root,
        ..project
    })
}

fn resolve_from_projects(projects: &[Project], key: &str) -> anyhow::Result<Option<Project>> {
    if let Some(project) = projects.iter().find(|project| project.id == key) {
        return Ok(Some(clone_project_root(project)));
    }

    let alias_path = Path::new(key);
    if alias_path.as_os_str().is_empty() {
        return Ok(None);
    }

    find_alias_match(projects, alias_path).map(|project| project.map(clone_project_root))
}

fn resolve_default_from_projects(
    projects: &[Project],
    default_root: &Path,
) -> anyhow::Result<ResolvedProject> {
    if let Some(project) = projects
        .iter()
        .find(|project| project.id == DEFAULT_PROJECT_ID)
    {
        return Ok(ResolvedProject::from_project(clone_project_root(project)));
    }

    if let Some(project) = find_alias_match(projects, default_root)? {
        return Ok(ResolvedProject::from_project(clone_project_root(project)));
    }

    Ok(ResolvedProject {
        id: DEFAULT_PROJECT_ID.to_string(),
        root: canonicalize_root(default_root)?,
        max_concurrent: None,
        default_agent: None,
        active: true,
    })
}

pub fn canonical_project_key(id: &str) -> String {
    id.to_string()
}

pub fn default_project_record(root: PathBuf) -> Project {
    Project {
        id: DEFAULT_PROJECT_ID.to_string(),
        root,
        max_concurrent: None,
        default_agent: None,
        active: true,
        created_at: chrono::Utc::now().to_rfc3339(),
    }
}

pub fn startup_project_record(id: String, root: PathBuf) -> Project {
    Project {
        id,
        root,
        max_concurrent: None,
        default_agent: None,
        active: true,
        created_at: chrono::Utc::now().to_rfc3339(),
    }
}

pub fn resolve_configured_project_limits(
    projects: &[Project],
    default_root: &Path,
    limits: &harness_core::config::misc::PerProjectConcurrencyLimits,
) -> anyhow::Result<std::collections::HashMap<String, usize>> {
    let mut resolved = std::collections::HashMap::new();

    // For the Typed variant, by_id keys are already project IDs — copy directly
    // without resolution.  Skip this for the Legacy variant because by_id() and
    // legacy() return the *same* underlying map there; copying first and then
    // resolving again in the legacy branch would leave raw path keys alongside
    // their resolved counterparts.
    if limits.legacy().is_none() {
        resolved.extend(limits.by_id().iter().map(|(k, v)| (k.clone(), *v)));
    }

    if let Some(legacy) = limits.legacy() {
        for (key, value) in legacy {
            let resolved_key = resolve_configured_limit_key(projects, default_root, key)?;
            resolved.entry(resolved_key).or_insert(*value);
        }
    }

    if let Some(by_root) = limits.by_root() {
        for (key, value) in by_root {
            let resolved_key = resolve_configured_limit_key(projects, default_root, key)?;
            resolved.entry(resolved_key).or_insert(*value);
        }
    }

    Ok(resolved)
}

pub fn resolve_project_input(
    projects: &[Project],
    requested: Option<&Path>,
    default_root: &Path,
) -> anyhow::Result<ResolvedProject> {
    match requested {
        None => resolve_default_from_projects(projects, default_root),
        Some(path) => {
            // Resolve relative paths against default_root, not the process CWD,
            // so behaviour is deterministic regardless of where the binary was
            // launched from.
            let full_path: std::borrow::Cow<'_, Path> = if path.is_absolute() {
                std::borrow::Cow::Borrowed(path)
            } else {
                std::borrow::Cow::Owned(default_root.join(path))
            };

            // Only treat the literal string "default" as the default-project
            // sentinel when it is not an actual directory on disk.  A relative
            // directory named "default" must go through normal path resolution.
            if path == Path::new(DEFAULT_PROJECT_ID) && !full_path.is_dir() {
                return resolve_default_from_projects(projects, default_root);
            }

            let raw = path.to_string_lossy();
            if let Some(project) = projects.iter().find(|project| project.id == raw) {
                return Ok(ResolvedProject::from_project(clone_project_root(project)));
            }

            let canonical_root = canonicalize_root(&full_path)?;
            if let Some(project) = find_exact_root_match(projects, &canonical_root) {
                return Ok(ResolvedProject::from_project(clone_project_root(project)));
            }

            Ok(ResolvedProject {
                id: canonical_root_alias(&canonical_root),
                root: canonical_root,
                max_concurrent: None,
                default_agent: None,
                active: true,
            })
        }
    }
}

fn project_list_alias_guard(projects: &[Project], project: &Project) -> anyhow::Result<()> {
    let canonical_root = canonicalize_root(&project.root)?;
    ensure_unique_identity(projects, project, &canonical_root)
}

/// A registered project with its root path and optional config overrides.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Project {
    pub id: String,
    pub root: PathBuf,
    #[serde(default)]
    pub max_concurrent: Option<u32>,
    #[serde(default)]
    pub default_agent: Option<String>,
    #[serde(default = "default_active")]
    pub active: bool,
    pub created_at: String,
}

fn default_active() -> bool {
    true
}

impl DbEntity for Project {
    fn table_name() -> &'static str {
        "projects"
    }

    fn id(&self) -> &str {
        &self.id
    }

    fn create_table_sql() -> &'static str {
        "CREATE TABLE IF NOT EXISTS projects (
            id         TEXT PRIMARY KEY,
            data       TEXT NOT NULL,
            created_at TEXT NOT NULL DEFAULT (datetime('now')),
            updated_at TEXT NOT NULL DEFAULT (datetime('now'))
        )"
    }
}

/// Registry of projects backed by SQLite. Survives server restarts.
pub struct ProjectRegistry {
    db: Db<Project>,
}

impl ProjectRegistry {
    async fn ensure_indexes(&self) -> anyhow::Result<()> {
        // Deduplicate rows with the same canonical root before enforcing the
        // unique constraint so that upgrading from an older DB that allowed
        // duplicate roots does not cause a startup outage.  Keep the row with
        // the lowest rowid (oldest entry) for each root value.
        let delete_result = sqlx::query(
            "DELETE FROM projects WHERE rowid NOT IN (\
                SELECT MIN(rowid) FROM projects \
                GROUP BY json_extract(data, '$.root')\
            )",
        )
        .execute(self.db.pool())
        .await?;
        let deleted = delete_result.rows_affected();
        if deleted > 0 {
            tracing::warn!(
                deleted_rows = deleted,
                "removed duplicate project rows during startup deduplication; \
                 only the oldest row per canonical root was kept"
            );
        }

        sqlx::query(
            "CREATE UNIQUE INDEX IF NOT EXISTS idx_projects_canonical_root \
             ON projects(json_extract(data, '$.root'))",
        )
        .execute(self.db.pool())
        .await?;
        Ok(())
    }

    pub async fn open(path: &std::path::Path) -> anyhow::Result<Arc<Self>> {
        let db = Db::open(path).await?;
        let registry = Arc::new(Self { db });
        registry.ensure_indexes().await?;
        Ok(registry)
    }

    /// Register or update a project.
    pub async fn register(&self, project: Project) -> anyhow::Result<()> {
        let project = normalize_project(project)?;
        let existing = self.db.list().await?;
        project_list_alias_guard(&existing, &project)?;
        match self.db.upsert(&project).await {
            Ok(()) => Ok(()),
            Err(error) => {
                let message = error.to_string();
                if message.contains("idx_projects_canonical_root")
                    || message.contains("json_extract(data, '$.root')")
                {
                    return Err(map_duplicate_root_conflict(&existing, &project));
                }
                Err(error)
            }
        }
    }

    /// List all registered projects ordered by creation time (newest first).
    pub async fn list(&self) -> anyhow::Result<Vec<Project>> {
        self.db.list().await
    }

    /// Get a project by ID or canonical root alias.
    pub async fn get(&self, id: &str) -> anyhow::Result<Option<Project>> {
        let projects = self.list().await?;
        resolve_from_projects(&projects, id)
    }

    /// Remove a project by ID. Returns `true` if it existed.
    pub async fn remove(&self, id: &str) -> anyhow::Result<bool> {
        self.db.delete(id).await
    }

    /// Resolve a project ID or alias to its root path. Returns `None` if not found.
    pub async fn resolve_path(&self, id: &str) -> anyhow::Result<Option<PathBuf>> {
        Ok(self.get(id).await?.map(|p| p.root))
    }

    pub async fn resolve_project(
        &self,
        requested: Option<&Path>,
        default_root: &Path,
    ) -> anyhow::Result<ResolvedProject> {
        let projects = self.list().await?;
        resolve_project_input(&projects, requested, default_root)
    }

    pub async fn resolve_limits(
        &self,
        default_root: &Path,
        limits: &harness_core::config::misc::PerProjectConcurrencyLimits,
    ) -> anyhow::Result<std::collections::HashMap<String, usize>> {
        let projects = self.list().await?;
        resolve_configured_project_limits(&projects, default_root, limits)
    }
}

impl Project {
    pub fn resolved(&self) -> ResolvedProject {
        ResolvedProject::from_project(self.clone())
    }
}

impl ResolvedProject {
    pub fn task_queue_key(&self) -> String {
        canonical_project_key(&self.id)
    }
}

pub fn resolve_project_with_registry(
    registry: Option<&ProjectRegistry>,
    requested: Option<&Path>,
    default_root: &Path,
) -> anyhow::Result<ResolvedProject> {
    match registry {
        Some(_) => anyhow::bail!("async registry resolution required"),
        None => resolve_project_input(&[], requested, default_root),
    }
}

pub async fn resolve_project_with_optional_registry(
    registry: Option<&ProjectRegistry>,
    requested: Option<&Path>,
    default_root: &Path,
) -> anyhow::Result<ResolvedProject> {
    match registry {
        Some(registry) => registry.resolve_project(requested, default_root).await,
        None => resolve_project_input(&[], requested, default_root),
    }
}

pub fn project_queue_overrides(
    project: &ResolvedProject,
    configured_limit: Option<usize>,
) -> Option<usize> {
    project
        .max_concurrent
        .map(|limit| limit as usize)
        .or(configured_limit)
}

pub fn ensure_project_is_active(project: &ResolvedProject) -> anyhow::Result<()> {
    if project.active {
        Ok(())
    } else {
        Err(anyhow::anyhow!("project '{}' is inactive", project.id))
    }
}

pub fn resolve_project_default_agent(project: &ResolvedProject) -> Option<&str> {
    project.default_agent.as_deref()
}

pub fn resolve_project_root_alias(project: &ResolvedProject) -> String {
    canonical_root_alias(&project.root)
}

pub fn canonical_project_root(root: &Path) -> anyhow::Result<PathBuf> {
    canonicalize_root(root)
}

pub fn canonical_project_root_string(root: &Path) -> anyhow::Result<String> {
    Ok(canonical_root_alias(&canonicalize_root(root)?))
}

pub fn is_default_project_id(id: &str) -> bool {
    id == DEFAULT_PROJECT_ID
}

pub fn default_project_id() -> &'static str {
    DEFAULT_PROJECT_ID
}

pub fn normalize_project_record(project: Project) -> anyhow::Result<Project> {
    normalize_project(project)
}

pub fn resolve_project_alias_in_memory(
    projects: &[Project],
    key: &str,
) -> anyhow::Result<Option<Project>> {
    resolve_from_projects(projects, key)
}

pub fn validate_project_identity_uniqueness(
    existing: &[Project],
    project: &Project,
) -> anyhow::Result<()> {
    let canonical_root = canonicalize_root(&project.root)?;
    ensure_unique_identity(existing, project, &canonical_root)
}

pub fn resolve_default_project_in_memory(
    projects: &[Project],
    default_root: &Path,
) -> anyhow::Result<ResolvedProject> {
    resolve_default_from_projects(projects, default_root)
}

pub fn queue_limit_overrides(
    project: &ResolvedProject,
    configured_limit: Option<usize>,
) -> Option<usize> {
    project_queue_overrides(project, configured_limit)
}

pub fn project_identity_alias(project: &Project) -> String {
    canonical_root_alias(&project.root)
}

pub fn canonical_project_alias(root: &Path) -> String {
    canonical_root_alias(root)
}

pub fn canonicalize_project_root(root: &Path) -> anyhow::Result<PathBuf> {
    canonicalize_root(root)
}

pub fn resolved_project_from_project(project: Project) -> ResolvedProject {
    ResolvedProject::from_project(project)
}

pub fn clone_project(project: &Project) -> Project {
    clone_project_root(project)
}

pub fn project_matches_alias(project: &Project, alias: &Path) -> bool {
    matches_alias(project, alias)
}

pub fn resolve_projects_configured_limits(
    projects: &[Project],
    default_root: &Path,
    limits: &harness_core::config::misc::PerProjectConcurrencyLimits,
) -> anyhow::Result<std::collections::HashMap<String, usize>> {
    resolve_configured_project_limits(projects, default_root, limits)
}

pub fn resolve_requested_project(
    projects: &[Project],
    requested: Option<&Path>,
    default_root: &Path,
) -> anyhow::Result<ResolvedProject> {
    resolve_project_input(projects, requested, default_root)
}

pub fn default_project(root: PathBuf) -> Project {
    default_project_record(root)
}

pub fn startup_project(id: String, root: PathBuf) -> Project {
    startup_project_record(id, root)
}

pub fn validate_project_activity(project: &ResolvedProject) -> anyhow::Result<()> {
    ensure_project_is_active(project)
}

pub fn project_default_agent(project: &ResolvedProject) -> Option<&str> {
    resolve_project_default_agent(project)
}

pub fn project_queue_key(project: &ResolvedProject) -> String {
    project.task_queue_key()
}

pub fn project_root_alias(project: &ResolvedProject) -> String {
    resolve_project_root_alias(project)
}

pub fn canonical_root_key(root: &Path) -> anyhow::Result<String> {
    canonical_project_root_string(root)
}

pub fn canonical_id_key(id: &str) -> String {
    canonical_project_key(id)
}

pub fn project_id_conflicts_with_alias(id: &str, root: &Path) -> bool {
    id == canonical_root_alias(root)
}

pub fn resolve_project_alias(
    projects: &[Project],
    alias: &Path,
) -> anyhow::Result<Option<Project>> {
    find_alias_match(projects, alias).map(|project| project.map(clone_project_root))
}

pub fn resolved_project_default_id() -> &'static str {
    DEFAULT_PROJECT_ID
}

pub fn resolved_project_key(project: &ResolvedProject) -> &str {
    &project.id
}

pub fn resolved_project_root(project: &ResolvedProject) -> &Path {
    &project.root
}

pub fn resolved_project_is_active(project: &ResolvedProject) -> bool {
    project.active
}

pub fn resolved_project_limit(project: &ResolvedProject) -> Option<u32> {
    project.max_concurrent
}

pub fn resolved_project_agent(project: &ResolvedProject) -> Option<&str> {
    project.default_agent.as_deref()
}

pub fn projects_by_id(projects: &[Project]) -> std::collections::HashMap<String, Project> {
    projects
        .iter()
        .cloned()
        .map(|project| (project.id.clone(), project))
        .collect()
}

pub fn project_aliases(projects: &[Project]) -> std::collections::HashMap<String, String> {
    projects
        .iter()
        .map(|project| (canonical_root_alias(&project.root), project.id.clone()))
        .collect()
}

pub fn resolved_project_for_queue(project: &ResolvedProject) -> (&str, &Path) {
    (&project.id, &project.root)
}

pub fn active_project_or_error(project: ResolvedProject) -> anyhow::Result<ResolvedProject> {
    ensure_project_is_active(&project)?;
    Ok(project)
}

pub fn apply_project_overrides(
    project: &ResolvedProject,
    configured_limit: Option<usize>,
) -> Option<usize> {
    project_queue_overrides(project, configured_limit)
}

pub fn resolved_project_from_parts(
    id: String,
    root: PathBuf,
    max_concurrent: Option<u32>,
    default_agent: Option<String>,
    active: bool,
) -> ResolvedProject {
    ResolvedProject {
        id,
        root,
        max_concurrent,
        default_agent,
        active,
    }
}

pub fn default_project_resolved(root: PathBuf) -> ResolvedProject {
    ResolvedProject {
        id: DEFAULT_PROJECT_ID.to_string(),
        root,
        max_concurrent: None,
        default_agent: None,
        active: true,
    }
}

pub fn resolved_project_from_optional(
    project: Option<Project>,
    default_root: &Path,
) -> anyhow::Result<ResolvedProject> {
    match project {
        Some(project) => Ok(project.resolved()),
        None => Ok(default_project_resolved(canonicalize_root(default_root)?)),
    }
}

pub fn project_root_matches(project: &ResolvedProject, root: &Path) -> bool {
    project.root == root
}

pub fn resolved_project_display_name(project: &ResolvedProject) -> &str {
    &project.id
}

pub fn default_project_display_name() -> &'static str {
    DEFAULT_PROJECT_ID
}

pub fn resolved_project_created_from_default(root: PathBuf) -> ResolvedProject {
    default_project_resolved(root)
}

pub fn project_id_matches(project: &ResolvedProject, id: &str) -> bool {
    project.id == id
}

pub fn resolved_project_to_project(project: &ResolvedProject, created_at: String) -> Project {
    Project {
        id: project.id.clone(),
        root: project.root.clone(),
        max_concurrent: project.max_concurrent,
        default_agent: project.default_agent.clone(),
        active: project.active,
        created_at,
    }
}

pub fn project_identity(project: &ResolvedProject) -> &str {
    &project.id
}

pub fn project_execution_root(project: &ResolvedProject) -> &Path {
    &project.root
}

pub fn configured_project_default_agent(project: &ResolvedProject) -> Option<&str> {
    project.default_agent.as_deref()
}

pub fn configured_project_max_concurrent(project: &ResolvedProject) -> Option<u32> {
    project.max_concurrent
}

pub fn resolved_project_summary(project: &ResolvedProject) -> (&str, &Path, bool) {
    (&project.id, &project.root, project.active)
}

pub fn project_key_for_logs(project: &ResolvedProject) -> &str {
    &project.id
}

pub fn project_root_for_logs(project: &ResolvedProject) -> &Path {
    &project.root
}

pub fn resolve_project_or_default(
    projects: &[Project],
    requested: Option<&Path>,
    default_root: &Path,
) -> anyhow::Result<ResolvedProject> {
    resolve_project_input(projects, requested, default_root)
}

pub fn configured_queue_key(id: &str) -> String {
    canonical_project_key(id)
}

pub fn default_project_name() -> &'static str {
    DEFAULT_PROJECT_ID
}

pub fn default_project_root_key(root: &Path) -> anyhow::Result<String> {
    canonical_project_root_string(root)
}

pub fn resolved_project_limit_override(project: &ResolvedProject) -> Option<usize> {
    project.max_concurrent.map(|value| value as usize)
}

pub fn canonical_root_alias_string(root: &Path) -> String {
    canonical_root_alias(root)
}

pub fn project_root_alias_string(project: &Project) -> String {
    canonical_root_alias(&project.root)
}

pub fn maybe_resolve_project(projects: &[Project], key: &str) -> anyhow::Result<Option<Project>> {
    resolve_from_projects(projects, key)
}

pub fn maybe_resolve_requested_project(
    projects: &[Project],
    requested: Option<&Path>,
    default_root: &Path,
) -> anyhow::Result<ResolvedProject> {
    resolve_project_input(projects, requested, default_root)
}

pub fn resolve_project_limit_map(
    projects: &[Project],
    default_root: &Path,
    limits: &harness_core::config::misc::PerProjectConcurrencyLimits,
) -> anyhow::Result<std::collections::HashMap<String, usize>> {
    resolve_configured_project_limits(projects, default_root, limits)
}

pub fn canonical_project_identifier(id: &str) -> String {
    canonical_project_key(id)
}

pub fn resolved_project_identifier(project: &ResolvedProject) -> &str {
    &project.id
}

pub fn resolved_project_canonical_root(project: &ResolvedProject) -> &Path {
    &project.root
}

pub fn resolved_project_default_agent_name(project: &ResolvedProject) -> Option<&str> {
    project.default_agent.as_deref()
}

pub fn resolved_project_concurrency(project: &ResolvedProject) -> Option<u32> {
    project.max_concurrent
}

pub fn resolved_project_active(project: &ResolvedProject) -> bool {
    project.active
}

pub fn default_resolved_project(root: PathBuf) -> ResolvedProject {
    default_project_resolved(root)
}

pub fn is_default_project(project: &ResolvedProject) -> bool {
    project.id == DEFAULT_PROJECT_ID
}

pub fn resolve_project_id_or_alias(
    projects: &[Project],
    key: &str,
) -> anyhow::Result<Option<Project>> {
    resolve_from_projects(projects, key)
}

pub fn ensure_distinct_project_identity(
    existing: &[Project],
    project: &Project,
) -> anyhow::Result<()> {
    let canonical_root = canonicalize_root(&project.root)?;
    ensure_unique_identity(existing, project, &canonical_root)
}

pub fn canonical_project_path(root: &Path) -> anyhow::Result<PathBuf> {
    canonicalize_root(root)
}

pub fn canonical_project_path_string(root: &Path) -> anyhow::Result<String> {
    canonical_project_root_string(root)
}

pub fn canonical_project_alias_string_for_project(project: &Project) -> String {
    canonical_root_alias(&project.root)
}

pub fn resolve_registered_project(
    projects: &[Project],
    requested: Option<&Path>,
    default_root: &Path,
) -> anyhow::Result<ResolvedProject> {
    resolve_project_input(projects, requested, default_root)
}

pub fn queue_identity(project: &ResolvedProject) -> &str {
    &project.id
}

pub fn execution_root(project: &ResolvedProject) -> &Path {
    &project.root
}

pub fn registry_project(project: Project) -> ResolvedProject {
    project.resolved()
}

pub fn registry_project_key(project: &ResolvedProject) -> String {
    canonical_project_key(&project.id)
}

pub fn registry_project_root_alias(project: &ResolvedProject) -> String {
    canonical_root_alias(&project.root)
}

pub fn registry_default_project(root: PathBuf) -> Project {
    default_project_record(root)
}

pub fn registry_startup_project(id: String, root: PathBuf) -> Project {
    startup_project_record(id, root)
}

pub fn resolved_project_active_or_err(project: &ResolvedProject) -> anyhow::Result<()> {
    ensure_project_is_active(project)
}

pub fn resolved_project_limit_or_config(
    project: &ResolvedProject,
    configured_limit: Option<usize>,
) -> Option<usize> {
    project_queue_overrides(project, configured_limit)
}

pub fn configured_limit_map(
    projects: &[Project],
    default_root: &Path,
    limits: &harness_core::config::misc::PerProjectConcurrencyLimits,
) -> anyhow::Result<std::collections::HashMap<String, usize>> {
    resolve_configured_project_limits(projects, default_root, limits)
}

pub fn project_registry_default_id() -> &'static str {
    DEFAULT_PROJECT_ID
}

pub fn project_registry_canonical_id(id: &str) -> String {
    canonical_project_key(id)
}

pub fn project_registry_canonical_root(root: &Path) -> anyhow::Result<PathBuf> {
    canonicalize_root(root)
}

pub fn project_registry_canonical_root_alias(root: &Path) -> String {
    canonical_root_alias(root)
}

pub fn project_registry_resolved_project(project: Project) -> ResolvedProject {
    project.resolved()
}

pub fn project_registry_default_project(root: PathBuf) -> Project {
    default_project_record(root)
}

pub fn project_registry_startup_project(id: String, root: PathBuf) -> Project {
    startup_project_record(id, root)
}

pub fn project_registry_resolve_project(
    projects: &[Project],
    requested: Option<&Path>,
    default_root: &Path,
) -> anyhow::Result<ResolvedProject> {
    resolve_project_input(projects, requested, default_root)
}

pub fn project_registry_resolve_limits(
    projects: &[Project],
    default_root: &Path,
    limits: &harness_core::config::misc::PerProjectConcurrencyLimits,
) -> anyhow::Result<std::collections::HashMap<String, usize>> {
    resolve_configured_project_limits(projects, default_root, limits)
}

pub fn project_registry_validate_activity(project: &ResolvedProject) -> anyhow::Result<()> {
    ensure_project_is_active(project)
}

pub fn project_registry_queue_key(project: &ResolvedProject) -> String {
    canonical_project_key(&project.id)
}

pub fn project_registry_execution_root(project: &ResolvedProject) -> &Path {
    &project.root
}

pub fn project_registry_default_agent(project: &ResolvedProject) -> Option<&str> {
    project.default_agent.as_deref()
}

pub fn project_registry_limit(project: &ResolvedProject) -> Option<u32> {
    project.max_concurrent
}

pub fn project_registry_activity(project: &ResolvedProject) -> bool {
    project.active
}

pub fn project_registry_root_alias(project: &ResolvedProject) -> String {
    canonical_root_alias(&project.root)
}

pub fn project_registry_identity(project: &ResolvedProject) -> &str {
    &project.id
}

pub fn project_registry_resolve_alias(
    projects: &[Project],
    alias: &Path,
) -> anyhow::Result<Option<Project>> {
    resolve_project_alias(projects, alias)
}

pub fn project_registry_validate_uniqueness(
    existing: &[Project],
    project: &Project,
) -> anyhow::Result<()> {
    validate_project_identity_uniqueness(existing, project)
}

pub fn project_registry_config_limits(
    projects: &[Project],
    default_root: &Path,
    limits: &harness_core::config::misc::PerProjectConcurrencyLimits,
) -> anyhow::Result<std::collections::HashMap<String, usize>> {
    resolve_configured_project_limits(projects, default_root, limits)
}

pub fn project_registry_default_resolved(root: PathBuf) -> ResolvedProject {
    default_project_resolved(root)
}

pub fn project_registry_is_default(project: &ResolvedProject) -> bool {
    project.id == DEFAULT_PROJECT_ID
}

pub fn project_registry_root_matches(project: &ResolvedProject, root: &Path) -> bool {
    project.root == root
}

pub fn project_registry_id_matches(project: &ResolvedProject, id: &str) -> bool {
    project.id == id
}

pub fn project_registry_resolve_or_default(
    projects: &[Project],
    requested: Option<&Path>,
    default_root: &Path,
) -> anyhow::Result<ResolvedProject> {
    resolve_project_input(projects, requested, default_root)
}

pub fn project_registry_queue_limit_override(
    project: &ResolvedProject,
    configured_limit: Option<usize>,
) -> Option<usize> {
    project_queue_overrides(project, configured_limit)
}

pub fn project_registry_display_name(project: &ResolvedProject) -> &str {
    &project.id
}

pub fn project_registry_default_name() -> &'static str {
    DEFAULT_PROJECT_ID
}

pub fn project_registry_resolved_key(project: &ResolvedProject) -> &str {
    &project.id
}

pub fn project_registry_resolved_root(project: &ResolvedProject) -> &Path {
    &project.root
}

pub fn project_registry_resolved_agent(project: &ResolvedProject) -> Option<&str> {
    project.default_agent.as_deref()
}

pub fn project_registry_resolved_limit(project: &ResolvedProject) -> Option<u32> {
    project.max_concurrent
}

pub fn project_registry_resolved_active(project: &ResolvedProject) -> bool {
    project.active
}

pub fn project_registry_resolved_parts(project: &ResolvedProject) -> (&str, &Path, bool) {
    (&project.id, &project.root, project.active)
}

pub fn project_registry_resolved_project_for_queue(project: &ResolvedProject) -> (&str, &Path) {
    (&project.id, &project.root)
}

pub fn project_registry_resolved_from_optional(
    project: Option<Project>,
    default_root: &Path,
) -> anyhow::Result<ResolvedProject> {
    resolved_project_from_optional(project, default_root)
}

pub fn project_registry_resolved_from_parts(
    id: String,
    root: PathBuf,
    max_concurrent: Option<u32>,
    default_agent: Option<String>,
    active: bool,
) -> ResolvedProject {
    resolved_project_from_parts(id, root, max_concurrent, default_agent, active)
}

pub fn project_registry_resolved_summary(project: &ResolvedProject) -> (&str, &Path, bool) {
    (&project.id, &project.root, project.active)
}

pub fn project_registry_resolved_queue_key(project: &ResolvedProject) -> String {
    canonical_project_key(&project.id)
}

pub fn project_registry_resolved_root_alias(project: &ResolvedProject) -> String {
    canonical_root_alias(&project.root)
}

pub fn project_registry_resolved_default_agent_name(project: &ResolvedProject) -> Option<&str> {
    project.default_agent.as_deref()
}

pub fn project_registry_resolved_limit_override(project: &ResolvedProject) -> Option<usize> {
    project.max_concurrent.map(|value| value as usize)
}

pub fn project_registry_default_project_id() -> &'static str {
    DEFAULT_PROJECT_ID
}

/// Check that `canonical_root` falls under at least one of the
/// `allowed_project_roots`.  Each allowlist entry is canonicalized before the
/// prefix check so that relative paths and symlinks in the config don't cause
/// false 403s.  Returns `Ok(())` when the allowlist is empty (i.e. no
/// restriction configured).
pub fn check_allowed_roots(
    canonical_root: &std::path::Path,
    allowed: &[std::path::PathBuf],
) -> Result<(), String> {
    if allowed.is_empty() {
        return Ok(());
    }
    let matched = allowed.iter().any(|base| {
        base.canonicalize()
            .map(|canon_base| canonical_root.starts_with(&canon_base))
            .unwrap_or(false)
    });
    if !matched {
        return Err("project root is not under an allowed base directory".to_string());
    }
    Ok(())
}

/// Validate that a path is an existing directory and a git repository.
pub fn validate_project_root(root: &std::path::Path) -> Result<(), String> {
    if !root.is_dir() {
        return Err(format!("root is not a directory: {}", root.display()));
    }
    // Check for .git directory or .git file (worktree case)
    if !root.join(".git").exists() {
        return Err(format!(
            "root is not a git repository (no .git found): {}",
            root.display()
        ));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    static CWD_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

    /// RAII guard that restores the process working directory on drop.
    struct CwdGuard(std::path::PathBuf);
    impl Drop for CwdGuard {
        fn drop(&mut self) {
            if let Err(e) = std::env::set_current_dir(&self.0) {
                eprintln!(
                    "CwdGuard: failed to restore working directory to {}: {e}",
                    self.0.display()
                );
            }
        }
    }

    fn test_project(root: PathBuf, id: &str) -> Project {
        Project {
            id: id.to_string(),
            root,
            max_concurrent: None,
            default_agent: None,
            active: true,
            created_at: "2026-01-01T00:00:00Z".to_string(),
        }
    }

    #[tokio::test]
    async fn register_and_get_roundtrip() -> anyhow::Result<()> {
        let dir = tempfile::tempdir()?;
        let registry = ProjectRegistry::open(&dir.path().join("projects.db")).await?;
        let project_root = tempfile::tempdir()?;
        let canonical_root = project_root.path().canonicalize()?;

        let project = test_project(canonical_root.clone(), "my-project");
        registry.register(project.clone()).await?;

        let loaded = registry
            .get("my-project")
            .await?
            .expect("project should exist");
        assert_eq!(loaded.id, "my-project");
        assert_eq!(loaded.root, canonical_root);
        assert!(loaded.active);
        Ok(())
    }

    #[tokio::test]
    async fn list_returns_all_projects() -> anyhow::Result<()> {
        let dir = tempfile::tempdir()?;
        let registry = ProjectRegistry::open(&dir.path().join("projects.db")).await?;
        let root_dirs: Vec<_> = (0..3)
            .map(|_| tempfile::tempdir())
            .collect::<Result<_, _>>()?;

        for (i, root_dir) in root_dirs.iter().enumerate() {
            registry
                .register(test_project(
                    root_dir.path().canonicalize()?,
                    &format!("p{i}"),
                ))
                .await?;
        }

        let all = registry.list().await?;
        assert_eq!(all.len(), 3);
        Ok(())
    }

    #[tokio::test]
    async fn remove_returns_true_when_found() -> anyhow::Result<()> {
        let dir = tempfile::tempdir()?;
        let registry = ProjectRegistry::open(&dir.path().join("projects.db")).await?;
        let project_root = tempfile::tempdir()?;

        registry
            .register(test_project(
                project_root.path().canonicalize()?,
                "to-delete",
            ))
            .await?;

        assert!(registry.remove("to-delete").await?);
        assert!(registry.get("to-delete").await?.is_none());
        Ok(())
    }

    #[tokio::test]
    async fn remove_returns_false_when_missing() -> anyhow::Result<()> {
        let dir = tempfile::tempdir()?;
        let registry = ProjectRegistry::open(&dir.path().join("projects.db")).await?;
        assert!(!registry.remove("nonexistent").await?);
        Ok(())
    }

    #[tokio::test]
    async fn resolve_path_returns_root() -> anyhow::Result<()> {
        let dir = tempfile::tempdir()?;
        let registry = ProjectRegistry::open(&dir.path().join("projects.db")).await?;
        let project_root = tempfile::tempdir()?;
        let canonical_root = project_root.path().canonicalize()?;

        registry
            .register(test_project(canonical_root.clone(), "harness"))
            .await?;

        let path = registry.resolve_path("harness").await?;
        assert_eq!(path, Some(canonical_root));
        assert!(registry.resolve_path("unknown").await?.is_none());
        Ok(())
    }

    #[tokio::test]
    async fn resolving_by_root_alias_returns_registered_project() -> anyhow::Result<()> {
        let dir = tempfile::tempdir()?;
        let registry = ProjectRegistry::open(&dir.path().join("projects.db")).await?;
        let root_dir = tempfile::tempdir()?;
        let canonical_root = root_dir.path().canonicalize()?;

        registry
            .register(Project {
                id: "harness".to_string(),
                root: canonical_root.clone(),
                max_concurrent: Some(2),
                default_agent: Some("claude".to_string()),
                active: true,
                created_at: "2026-01-01T00:00:00Z".to_string(),
            })
            .await?;

        let loaded = registry
            .get(&canonical_root.to_string_lossy())
            .await?
            .expect("root alias should resolve to project");
        assert_eq!(loaded.id, "harness");
        assert_eq!(loaded.root, canonical_root);
        assert_eq!(loaded.max_concurrent, Some(2));
        assert_eq!(loaded.default_agent.as_deref(), Some("claude"));
        Ok(())
    }

    #[tokio::test]
    async fn registering_duplicate_canonical_root_is_rejected() -> anyhow::Result<()> {
        let dir = tempfile::tempdir()?;
        let registry = ProjectRegistry::open(&dir.path().join("projects.db")).await?;
        let root_dir = tempfile::tempdir()?;
        let canonical_root = root_dir.path().canonicalize()?;

        registry
            .register(Project {
                id: "alpha".to_string(),
                root: canonical_root.clone(),
                max_concurrent: None,
                default_agent: None,
                active: true,
                created_at: "2026-01-01T00:00:00Z".to_string(),
            })
            .await?;

        let err = registry
            .register(Project {
                id: "beta".to_string(),
                root: canonical_root,
                max_concurrent: None,
                default_agent: None,
                active: true,
                created_at: "2026-01-01T00:00:00Z".to_string(),
            })
            .await
            .expect_err("duplicate root should be rejected");
        assert!(err.to_string().contains("canonical root"));
        Ok(())
    }

    #[tokio::test]
    async fn survives_reopen() -> anyhow::Result<()> {
        let dir = tempfile::tempdir()?;
        let db_path = dir.path().join("projects.db");
        let project_root = tempfile::tempdir()?;
        let canonical_root = project_root.path().canonicalize()?;

        {
            let registry = ProjectRegistry::open(&db_path).await?;
            registry
                .register(Project {
                    id: "persistent".to_string(),
                    root: canonical_root.clone(),
                    max_concurrent: Some(2),
                    default_agent: Some("claude".to_string()),
                    active: true,
                    created_at: "2026-01-01T00:00:00Z".to_string(),
                })
                .await?;
        }

        let registry = ProjectRegistry::open(&db_path).await?;
        let loaded = registry
            .get("persistent")
            .await?
            .expect("should survive reopen");
        assert_eq!(loaded.root, canonical_root);
        assert_eq!(loaded.max_concurrent, Some(2));
        assert_eq!(loaded.default_agent.as_deref(), Some("claude"));
        Ok(())
    }

    #[tokio::test]
    async fn resolve_requested_project_supports_default_identity_aliases() -> anyhow::Result<()> {
        let dir = tempfile::tempdir()?;
        let registry = ProjectRegistry::open(&dir.path().join("projects.db")).await?;
        let default_root = tempfile::tempdir()?;
        let canonical_default_root = default_root.path().canonicalize()?;

        let implicit = registry.resolve_project(None, default_root.path()).await?;
        let default_id = registry
            .resolve_project(Some(Path::new(DEFAULT_PROJECT_ID)), default_root.path())
            .await?;
        let default_alias = registry
            .resolve_project(Some(canonical_default_root.as_path()), default_root.path())
            .await?;

        assert_eq!(implicit.id, DEFAULT_PROJECT_ID);
        assert_eq!(implicit.root, canonical_default_root);
        assert_eq!(default_id, implicit);
        assert_eq!(
            default_alias.id,
            canonical_root_alias(&canonical_default_root)
        );
        assert_eq!(default_alias.root, canonical_default_root);
        Ok(())
    }

    #[tokio::test]
    async fn resolve_limits_maps_root_aliases_to_project_ids() -> anyhow::Result<()> {
        let dir = tempfile::tempdir()?;
        let registry = ProjectRegistry::open(&dir.path().join("projects.db")).await?;
        let default_root = tempfile::tempdir()?;
        let project_root = tempfile::tempdir()?;
        let canonical_project_root = project_root.path().canonicalize()?;

        registry
            .register(Project {
                id: "named".to_string(),
                root: canonical_project_root.clone(),
                max_concurrent: None,
                default_agent: None,
                active: true,
                created_at: "2026-01-01T00:00:00Z".to_string(),
            })
            .await?;

        let mut limits = harness_core::config::misc::PerProjectConcurrencyLimits::default();
        limits.by_id_mut().insert("named".to_string(), 2);
        if let harness_core::config::misc::PerProjectConcurrencyLimits::Typed(typed) = &mut limits {
            typed
                .by_root
                .insert(canonical_project_root.to_string_lossy().into_owned(), 3);
        }

        let resolved = registry
            .resolve_limits(default_root.path(), &limits)
            .await?;
        assert_eq!(resolved.get("named"), Some(&2));
        Ok(())
    }

    #[tokio::test]
    async fn stale_registered_root_does_not_break_other_project_lookups() -> anyhow::Result<()> {
        let dir = tempfile::tempdir()?;
        let registry = ProjectRegistry::open(&dir.path().join("projects.db")).await?;
        let live_root = tempfile::tempdir()?;
        let deleted_root = tempfile::tempdir()?;
        let live_canonical_root = live_root.path().canonicalize()?;
        let deleted_canonical_root = deleted_root.path().canonicalize()?;

        registry
            .register(test_project(live_canonical_root.clone(), "live-project"))
            .await?;
        registry
            .register(test_project(deleted_canonical_root, "stale-project"))
            .await?;
        drop(deleted_root);

        let loaded = registry
            .get("live-project")
            .await?
            .expect("live project should still resolve");
        assert_eq!(loaded.root, live_canonical_root);
        Ok(())
    }

    #[tokio::test]
    async fn unregistered_directory_keeps_its_own_canonical_queue_identity() -> anyhow::Result<()> {
        let dir = tempfile::tempdir()?;
        let registry = ProjectRegistry::open(&dir.path().join("projects.db")).await?;
        let default_root = tempfile::tempdir()?;
        let repo_root = tempfile::tempdir()?;
        let canonical_repo_root = repo_root.path().canonicalize()?;

        let resolved = registry
            .resolve_project(Some(canonical_repo_root.as_path()), default_root.path())
            .await?;

        assert_eq!(resolved.root, canonical_repo_root);
        assert_eq!(resolved.id, canonical_repo_root.to_string_lossy());
        Ok(())
    }

    #[tokio::test]
    async fn resolve_limits_maps_by_root_entries_for_registered_projects() -> anyhow::Result<()> {
        let dir = tempfile::tempdir()?;
        let registry = ProjectRegistry::open(&dir.path().join("projects.db")).await?;
        let default_root = tempfile::tempdir()?;
        let project_root = tempfile::tempdir()?;
        let canonical_project_root = project_root.path().canonicalize()?;

        registry
            .register(test_project(canonical_project_root.clone(), "named"))
            .await?;

        let mut limits = harness_core::config::misc::PerProjectConcurrencyLimits::default();
        if let harness_core::config::misc::PerProjectConcurrencyLimits::Typed(typed) = &mut limits {
            typed
                .by_root
                .insert(canonical_project_root.to_string_lossy().into_owned(), 3);
        }

        let resolved = registry
            .resolve_limits(default_root.path(), &limits)
            .await?;
        assert_eq!(resolved.get("named"), Some(&3));
        Ok(())
    }

    #[tokio::test]
    async fn relative_directory_input_does_not_resolve_to_registered_suffix_alias(
    ) -> anyhow::Result<()> {
        let _lock = CWD_LOCK.lock().unwrap();
        let _cwd = CwdGuard(std::env::current_dir()?);

        let project_root = tempfile::tempdir()?;
        let canonical_project_root = project_root.path().canonicalize()?;
        let nested = canonical_project_root.join("foo");
        std::fs::create_dir(&nested)?;
        // Set CWD so that the test mirrors a realistic server launch context;
        // resolve_project_input now anchors relative paths to default_root
        // rather than CWD, so this does not affect the outcome.
        std::env::set_current_dir(project_root.path())?;

        let registered_root = tempfile::tempdir()?;
        let registered_root = registered_root.path().join("parent").join("foo");
        std::fs::create_dir_all(&registered_root)?;
        let registered_root = registered_root.canonicalize()?;

        let project = resolve_project_input(
            &[test_project(registered_root, "registered")],
            Some(Path::new("foo")),
            project_root.path(),
        )?;

        assert_eq!(project.root, nested.canonicalize()?);
        assert_eq!(project.id, nested.to_string_lossy());
        let result: anyhow::Result<()> = Ok(());
        result
    }

    #[tokio::test]
    async fn resolve_limits_keeps_unregistered_raw_root_entries() -> anyhow::Result<()> {
        let default_root = tempfile::tempdir()?;
        let unregistered_root = tempfile::tempdir()?;
        let canonical_unregistered_root = unregistered_root.path().canonicalize()?;
        let projects = vec![test_project(
            default_root.path().canonicalize()?,
            DEFAULT_PROJECT_ID,
        )];

        let mut limits = harness_core::config::misc::PerProjectConcurrencyLimits::Legacy(
            std::collections::HashMap::new(),
        );
        if let harness_core::config::misc::PerProjectConcurrencyLimits::Legacy(legacy) = &mut limits
        {
            legacy.insert(
                canonical_unregistered_root.to_string_lossy().into_owned(),
                4,
            );
        }

        let resolved = resolve_configured_project_limits(&projects, default_root.path(), &limits)?;
        assert_eq!(
            resolved.get(&canonical_unregistered_root.to_string_lossy().into_owned()),
            Some(&4)
        );
        Ok(())
    }

    #[tokio::test]
    async fn resolve_limits_keeps_unregistered_by_root_entries() -> anyhow::Result<()> {
        let default_root = tempfile::tempdir()?;
        let unregistered_root = tempfile::tempdir()?;
        let canonical_unregistered_root = unregistered_root.path().canonicalize()?;
        let projects = vec![test_project(
            default_root.path().canonicalize()?,
            DEFAULT_PROJECT_ID,
        )];

        let mut limits = harness_core::config::misc::PerProjectConcurrencyLimits::default();
        if let harness_core::config::misc::PerProjectConcurrencyLimits::Typed(typed) = &mut limits {
            typed.by_root.insert(
                canonical_unregistered_root.to_string_lossy().into_owned(),
                5,
            );
        }

        let resolved = resolve_configured_project_limits(&projects, default_root.path(), &limits)?;
        assert_eq!(
            resolved.get(&canonical_unregistered_root.to_string_lossy().into_owned()),
            Some(&5)
        );
        Ok(())
    }

    #[tokio::test]
    async fn register_rejects_duplicate_root_under_concurrency() -> anyhow::Result<()> {
        let dir = tempfile::tempdir()?;
        let registry = ProjectRegistry::open(&dir.path().join("projects.db")).await?;
        let root_dir = tempfile::tempdir()?;
        let canonical_root = root_dir.path().canonicalize()?;

        let first = registry.register(test_project(canonical_root.clone(), "alpha"));
        let second = registry.register(test_project(canonical_root, "beta"));
        let (first_result, second_result) = tokio::join!(first, second);

        let success_count = [first_result.is_ok(), second_result.is_ok()]
            .into_iter()
            .filter(|ok| *ok)
            .count();
        assert_eq!(success_count, 1);

        let failure = first_result
            .err()
            .or_else(|| second_result.err())
            .expect("one registration must fail");
        assert!(failure.to_string().contains("canonical root"));

        let projects = registry.list().await?;
        assert_eq!(projects.len(), 1);
        Ok(())
    }
}
