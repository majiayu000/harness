# Runtime Eval Baselines

Committed eval baselines are governed records, not mutable run reports. Create
them with `harness eval baseline record` or explicitly migrate a legacy report
with `harness eval baseline migrate`.

Each baseline record binds:

- `suite_digest`
- `stack_id`
- `source_commit`
- `evidence_ids`
- `creator_observation`
- the baseline report digest
- the full record digest

Candidate eval reports must be written outside `evals/baselines/` and compared
against a baseline record with `harness eval diff`. The CLI rejects candidate
report output under this directory so a candidate run cannot overwrite a
committed baseline by using `--output`.

Branch protection for `main` must require a fresh CODEOWNERS review before
changes to this directory can merge. The repository CODEOWNERS file assigns
`/evals/baselines/` to the maintainer owner; baseline updates are therefore
human-reviewed governance changes, not ordinary candidate eval artifacts.
