pub(super) const SQL: &str = "DO $$
BEGIN
  IF EXISTS (
    SELECT 1
    FROM workflow_instances
    WHERE definition_id = 'github_issue_pr'
      AND data->'data'->>'project_id' IS NOT NULL
      AND data->'data'->>'repo' IS NOT NULL
      AND data->'data'->>'issue_number' IS NOT NULL
    GROUP BY
      definition_id,
      data->'data'->>'project_id',
      LOWER(data->'data'->>'repo'),
      data->'data'->>'issue_number'
    HAVING COUNT(*) > 1
  ) THEN
    RAISE EXCEPTION 'workflow_instances contains case-colliding GitHub issue identities; resolve duplicates before migration';
  END IF;
END $$;

DROP INDEX IF EXISTS idx_workflow_instances_project_repo_issue_ci;
CREATE UNIQUE INDEX idx_workflow_instances_project_repo_issue_ci
ON workflow_instances (
  definition_id,
  (data->'data'->>'project_id'),
  (LOWER(data->'data'->>'repo')),
  (data->'data'->>'issue_number')
)
WHERE definition_id = 'github_issue_pr'
  AND data->'data'->>'project_id' IS NOT NULL
  AND data->'data'->>'repo' IS NOT NULL
  AND data->'data'->>'issue_number' IS NOT NULL";
