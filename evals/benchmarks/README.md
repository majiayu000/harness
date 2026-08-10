# Runtime Eval Benchmark Manifests

Benchmark manifests live in this directory as TOML files. Each manifest lists
resolved issue cases that the eval driver can replay through the normal
workflow runtime path.

```toml
suite = "harness-core"
default_timeout_secs = 3600

[[cases]]
repo = "majiayu000/harness"
issue = 1437
base_commit = "b308b380"
verify_commands = ["cargo test -p harness-server lifecycle_"]
paths = ["crates/harness-server/src/workflow_runtime_worker.rs"]
risk = "high"
evidence = [
    "https://github.com/majiayu000/harness/issues/1437",
    "specs/GH1437/tasks.md",
]
resolution_prs = [1502]
resolution_commits = ["0123456789abcdef"]
commit_resolution = "resolved"
verdict = "replayable"
```

`case_id` is optional and defaults to `owner/repo#issue`. `base_commit` must be
a 7- to 40-character hexadecimal commit prefix or SHA. Every case must include
at least one single-line verification command.

Historical replay cases can also record structured replay metadata:

- `paths` are repository-relative paths touched or used as acceptance evidence.
- `risk` is `low`, `medium`, or `high`.
- `evidence` lists issue, PR, spec, report, or other single-line evidence
  references.
- `resolution_prs` and `resolution_commits` record the commit-resolution pair.
- `commit_resolution` is `resolved` or `pending`.
- `verdict` is `replayable` or `pending`; pending commit pairs must not be
  marked replayable.
