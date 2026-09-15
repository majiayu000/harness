use super::*;

/// A selected file owns the flow and activity instructions. Project settings
/// supply execution parameters and per-activity validation commands.
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct SharedWorkflow {
    definition: WorkflowDefinitionPolicy,
    activities: BTreeMap<String, WorkflowActivityPolicy>,
}

pub(super) fn resolve(
    project_root: &Path,
    project_policy: &serde_yaml::Value,
    value: &mut serde_yaml::Value,
    body: &mut String,
    sources: &mut Vec<WorkflowSourceObservation>,
) -> anyhow::Result<()> {
    let Some(file) = value.get("workflow").and_then(|v| v.get("file")) else {
        return Ok(());
    };
    if file.is_null() {
        return Ok(());
    }
    let file = file
        .as_str()
        .filter(|s| !s.trim().is_empty())
        .ok_or_else(|| anyhow::anyhow!("workflow.file must be a non-empty path"))?;
    let path = project_root.join(file);
    let loaded = read_workflow_file(&path)?.ok_or_else(|| {
        anyhow::anyhow!("selected workflow file does not exist: {}", path.display())
    })?;
    let mut shared: SharedWorkflow = serde_yaml::from_value(loaded.front_matter)
        .map_err(|e| anyhow::anyhow!("invalid selected workflow {}: {e}", path.display()))?;
    if value.get("definition").is_some_and(|v| !v.is_null()) {
        anyhow::bail!("workflow.file cannot be combined with an inline definition");
    }
    let overrides: BTreeMap<String, WorkflowActivityPolicy> = value
        .get("activities")
        .cloned()
        .map(serde_yaml::from_value)
        .transpose()
        .map_err(|error| anyhow::anyhow!("invalid selected workflow activity overrides: {error}"))?
        .unwrap_or_default();
    if let Some(activities) = project_policy
        .get("activities")
        .and_then(serde_yaml::Value::as_mapping)
    {
        for (name, policy) in activities {
            let name = name
                .as_str()
                .ok_or_else(|| anyhow::anyhow!("activity names must be strings"))?;
            if !shared.activities.contains_key(name) {
                anyhow::bail!(
                    "project config references activity '{name}' absent from the selected workflow"
                );
            }
            if policy.get("prompt").is_some() || policy.get("agent_contract").is_some() {
                anyhow::bail!("activity '{name}' prompt and agent_contract belong in workflow.file; project config may override validation");
            }
        }
    }
    // Selecting a workflow replaces inherited activity policy. Only validation
    // commands are project parameters; prompts and agent contracts belong to
    // the selected workflow.
    for (name, policy) in overrides {
        let Some(activity) = shared.activities.get_mut(&name) else {
            continue;
        };
        if value["activities"][&name].get("validation").is_some() {
            activity.validation = policy.validation;
        }
    }
    value["definition"] = serde_yaml::to_value(&shared.definition)?;
    value["activities"] = serde_yaml::to_value(&shared.activities)?;
    value["workflow"]["id"] = serde_yaml::to_value(&shared.definition.id)?;
    if !loaded.body.is_empty() {
        *body = if body.is_empty() {
            loaded.body
        } else {
            format!("{}\n\nProject instructions:\n{}", loaded.body, body)
        };
    }
    sources.push(WorkflowSourceObservation {
        role: WorkflowSourceRole::SelectedWorkflow,
        path: workflow_path_identity(&path),
        content_sha256: loaded.content_sha256,
    });
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    const FLOW: &str = "---\ndefinition:\n  id: shared_review\n  initial: reviewing\n  states:\n    reviewing: {activity: inspect, on_success: done}\n    blocked: {progress: operator_gate}\n  terminal: {done: succeeded, failed: failed, cancelled: cancelled}\nactivities:\n  inspect:\n    prompt: Inspect without editing files.\n    validation: [default-check]\n---\nShared review instructions.\n";

    #[test]
    fn shared_workflow_keeps_flow_and_project_configuration_separate() -> anyhow::Result<()> {
        let root = tempfile::tempdir()?;
        std::fs::write(root.path().join("review.md"), FLOW)?;
        let mut documents = Vec::new();
        for (project, validation) in [("rust", "cargo check"), ("web", "bun test")] {
            let dir = root.path().join(project);
            std::fs::create_dir(&dir)?;
            std::fs::write(dir.join("WORKFLOW.md"), format!(
                "---\nworkflow:\n  file: ../review.md\nsource:\n  repo: owner/{project}\nactivities:\n  inspect:\n    validation: [{validation}]\n---\nProject {project} instructions.\n"
            ))?;
            let doc = load_workflow_document_with_base(&dir, None)?;
            assert_eq!(doc.config.definition.as_ref().unwrap().id, "shared_review");
            assert_eq!(doc.config.workflow.id.as_deref(), Some("shared_review"));
            assert_eq!(
                doc.config.activities["inspect"].prompt.as_deref(),
                Some("Inspect without editing files.")
            );
            assert_eq!(
                doc.config.activities["inspect"].validation,
                vec![validation]
            );
            assert!(doc.prompt_template.contains("Shared review instructions."));
            assert!(doc
                .prompt_template
                .contains(&format!("Project {project} instructions.")));
            assert!(doc
                .sources
                .iter()
                .any(|s| s.role == WorkflowSourceRole::SelectedWorkflow));
            documents.push(doc);
        }
        assert_ne!(
            documents[0].config.source.repo,
            documents[1].config.source.repo
        );
        std::fs::write(
            root.path().join("other.md"),
            FLOW.replace("shared_review", "other_review"),
        )?;
        std::fs::write(
            root.path().join("web/WORKFLOW.md"),
            "---\nworkflow: {file: ../other.md}\n---\n",
        )?;
        assert_eq!(
            load_workflow_document_with_base(&root.path().join("web"), None)?
                .config
                .workflow
                .id
                .as_deref(),
            Some("other_review")
        );
        Ok(())
    }

    #[test]
    fn selected_workflow_preserves_explicit_central_precedence() -> anyhow::Result<()> {
        let root = tempfile::tempdir()?;
        std::fs::write(root.path().join("flow.md"), FLOW)?;
        let base = root.path().join("base.md");
        std::fs::write(
            root.path().join("WORKFLOW.md"),
            "---\nworkflow: {file: flow.md}\n---\n",
        )?;
        std::fs::write(&base, "---\ndefinition: {}\n---\n")?;
        let error = load_workflow_document_with_base(root.path(), Some(&base))
            .expect_err("inherited inline definitions must not silently disappear");
        assert!(error.to_string().contains("inline definition"), "{error}");

        std::fs::write(
            &base,
            "---\nactivities:\n  inspect:\n    validation: [central-check]\n---\n",
        )?;
        let doc = load_workflow_document_with_base(root.path(), Some(&base))?;
        assert_eq!(
            doc.config.activities["inspect"].validation,
            vec!["central-check"]
        );
        assert_eq!(
            doc.config.activities["inspect"].prompt.as_deref(),
            Some("Inspect without editing files.")
        );

        std::fs::write(root.path().join("WORKFLOW.md"),
            "---\nworkflow: {file: flow.md}\nactivities:\n  inspect:\n    validation: [project-check]\n---\n")?;
        let doc = load_workflow_document_with_base(root.path(), Some(&base))?;
        assert_eq!(
            doc.config.activities["inspect"].validation,
            vec!["project-check"]
        );
        Ok(())
    }

    #[test]
    fn selected_workflow_rejects_malformed_activity_overrides() -> anyhow::Result<()> {
        let root = tempfile::tempdir()?;
        std::fs::write(root.path().join("flow.md"), FLOW)?;
        for activities in [
            "[inspect]",
            "{inspect: false}",
            "{inspect: {validation: false}}",
        ] {
            std::fs::write(
                root.path().join("WORKFLOW.md"),
                format!("---\nworkflow: {{file: flow.md}}\nactivities: {activities}\n---\n"),
            )?;
            let error = load_workflow_document_with_base(root.path(), None)
                .expect_err("malformed project activities must not disappear during selection");
            assert!(error.to_string().contains("activity overrides"), "{error}");
        }
        Ok(())
    }

    #[test]
    fn selected_workflow_errors_do_not_fall_back_to_builtin_work() -> anyhow::Result<()> {
        let root = tempfile::tempdir()?;
        std::fs::write(root.path().join("flow.md"), FLOW)?;
        for (config, expected) in [
            ("workflow: {file: missing.md}", "does not exist"),
            (
                "workflow: {file: flow.md}\ndefinition: {}",
                "inline definition",
            ),
            (
                "workflow: {file: flow.md}\nactivities:\n  inspect: {prompt: replace}",
                "belong in workflow.file",
            ),
            (
                "workflow: {file: flow.md}\nactivities:\n  typo: {validation: []}",
                "absent from the selected workflow",
            ),
        ] {
            std::fs::write(
                root.path().join("WORKFLOW.md"),
                format!("---\n{config}\n---\n"),
            )?;
            let error = load_workflow_document_with_base(root.path(), None).unwrap_err();
            assert!(error.to_string().contains(expected), "{error}");
        }
        Ok(())
    }
}
