use super::HarnessServer;
use anyhow::Context;
use harness_workflow::runtime::DeclarativeWorkflowDefinition;
use std::collections::BTreeSet;
use std::path::{Path, PathBuf};

impl HarnessServer {
    pub(super) fn register_startup_declarative_workflows(&self) -> anyhow::Result<()> {
        let definitions = self.compile_startup_declarative_workflows()?;
        harness_workflow::runtime::register_declarative_workflow_definitions(definitions)
            .context("failed to atomically register startup declarative workflow definitions")
    }

    fn compile_startup_declarative_workflows(
        &self,
    ) -> anyhow::Result<Vec<DeclarativeWorkflowDefinition>> {
        compile_declarative_workflows(startup_project_roots(self))
    }
}

fn startup_project_roots(server: &HarnessServer) -> Vec<PathBuf> {
    let mut seen = BTreeSet::new();
    let mut roots = Vec::new();
    for root in std::iter::once(&server.config.server.project_root)
        .chain(server.startup_projects.iter().map(|project| &project.root))
    {
        let identity = root.canonicalize().unwrap_or_else(|_| root.clone());
        if seen.insert(identity) {
            roots.push(root.clone());
        }
    }
    roots
}

fn compile_declarative_workflows(
    project_roots: impl IntoIterator<Item = PathBuf>,
) -> anyhow::Result<Vec<DeclarativeWorkflowDefinition>> {
    let mut definitions = Vec::new();
    for project_root in project_roots {
        let document = harness_core::config::workflow::load_workflow_document(&project_root)
            .with_context(|| {
                format!(
                    "failed to load workflow configuration for startup project '{}'",
                    project_root.display()
                )
            })?;
        let Some(policy) = document.config.definition.as_ref() else {
            continue;
        };
        let definition = harness_workflow::runtime::build_declarative_definition(
            policy,
            &document.config.activities,
        )
        .with_context(|| definition_context(&project_root, document.source_path.as_deref()))?;
        definitions.push(definition);
    }
    Ok(definitions)
}

fn definition_context(project_root: &Path, source_path: Option<&str>) -> String {
    format!(
        "invalid declarative workflow for startup project '{}' from '{}'",
        project_root.display(),
        source_path.unwrap_or("<no WORKFLOW.md>")
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use harness_agents::registry::AgentRegistry;
    use harness_core::config::{HarnessConfig, ProjectEntry};
    use harness_workflow::runtime::WorkflowDefinitionRegistry;

    fn write_definition(root: &Path, id: &str, activity: &str) -> anyhow::Result<()> {
        std::fs::create_dir_all(root)?;
        std::fs::write(
            root.join("WORKFLOW.md"),
            format!(
                r#"---
definition:
  id: {id}
  initial: reviewing
  states:
    reviewing:
      activity: {activity}
      on_success: done
      on_failure: failed
      on_signal:
        cancel: cancelled
    blocked:
      progress: operator_gate
  terminal:
    done: succeeded
    failed: failed
    cancelled: cancelled
  recovery_targets: [reviewing]
activities:
  {activity}:
    prompt: Review the submitted content.
---
Run the declared review workflow.
"#
            ),
        )?;
        Ok(())
    }

    fn server_for(root: &Path) -> HarnessServer {
        let mut config = HarnessConfig::default();
        config.server.project_root = root.to_path_buf();
        HarnessServer::new(
            config,
            crate::thread_manager::ThreadManager::new(),
            AgentRegistry::new("test"),
        )
    }

    #[test]
    fn startup_compiles_each_unique_managed_project_before_registration() -> anyhow::Result<()> {
        let sandbox = tempfile::tempdir()?;
        let first = sandbox.path().join("first");
        let second = sandbox.path().join("second");
        write_definition(&first, "startup_docs_review", "review_docs")?;
        write_definition(&second, "startup_release_review", "review_release")?;
        let mut server = server_for(&first);
        server.startup_projects = vec![
            ProjectEntry {
                name: "duplicate-default".to_string(),
                root: first.clone(),
                default: true,
                default_agent: None,
                max_concurrent: None,
            },
            ProjectEntry {
                name: "release".to_string(),
                root: second,
                default: false,
                default_agent: None,
                max_concurrent: None,
            },
        ];

        let definitions = server.compile_startup_declarative_workflows()?;

        assert_eq!(definitions.len(), 2);
        assert_eq!(definitions[0].policy().id, "startup_docs_review");
        assert_eq!(definitions[1].policy().id, "startup_release_review");
        Ok(())
    }

    #[test]
    fn invalid_later_project_yields_no_partial_registration_batch() -> anyhow::Result<()> {
        let sandbox = tempfile::tempdir()?;
        let first = sandbox.path().join("first");
        let second = sandbox.path().join("second");
        write_definition(&first, "startup_atomic_valid", "review")?;
        write_definition(&second, "startup_atomic_invalid", "missing_policy")?;
        std::fs::write(
            second.join("WORKFLOW.md"),
            std::fs::read_to_string(second.join("WORKFLOW.md"))?.replace(
                "activities:\n  missing_policy:",
                "activities:\n  other_activity:",
            ),
        )?;

        let error = compile_declarative_workflows([first, second])
            .expect_err("unknown activity in the later project must abort compilation");

        let detail = format!("{error:#}");
        assert!(detail.contains("startup_atomic_invalid"));
        assert!(detail.contains("missing_policy"));
        let registry = WorkflowDefinitionRegistry::new();
        assert!(registry.definition("startup_atomic_valid").is_none());
        Ok(())
    }

    #[test]
    fn duplicate_definition_ids_are_rejected_atomically() -> anyhow::Result<()> {
        let sandbox = tempfile::tempdir()?;
        let first = sandbox.path().join("first");
        let second = sandbox.path().join("second");
        write_definition(&first, "startup_duplicate_id", "review")?;
        write_definition(&second, "startup_duplicate_id", "review")?;
        let definitions = compile_declarative_workflows([first, second])?;
        let mut registry = WorkflowDefinitionRegistry::new();

        let error = registry
            .register_declarative_current_batch(definitions)
            .expect_err("duplicate process-global ids must fail");

        assert!(error.to_string().contains("already registered"));
        assert!(registry.definition("startup_duplicate_id").is_none());
        Ok(())
    }
}
