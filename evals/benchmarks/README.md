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
```

`case_id` is optional and defaults to `owner/repo#issue`. `base_commit` must be
a 7- to 40-character hexadecimal commit prefix or SHA. Every case must include
at least one verification command.

Eval cases are treated as untrusted golden tasks. The manifest parser binds
them to the `container` isolation tier, the `remote_host` runtime kind, an
ephemeral lifecycle, and required cleanup evidence by default. The optional
`[isolation]` table may restate those values, but it cannot downgrade cases to
host execution.
