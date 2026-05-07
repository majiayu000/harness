# Use when: rebasing a PR branch onto main and the rebase produces conflicts. Triggers on: rebase origin/main, rebase the PR, rebase --continue, rebase --skip, rebase conflict, conflicting and rebase, manual resolution required, rebase the conflicting

<!-- trigger-patterns: rebase origin/main, rebase the PR, rebase --continue, rebase --skip, rebase conflict, conflicting and rebase, manual resolution required, rebase the conflicting -->

## Trigger
Use when a `git rebase` against the base branch surfaces one or more conflicts that the default "abort and bail" path would otherwise drop as `manual resolution required`.

## Input
- PR number, head branch, and base branch (resolved via `gh pr view`).
- Path to an isolated worktree, e.g. `/tmp/harness-rebase-<pr>`.
- Read access to the GitHub repo (for divergence stats and commit fingerprints).

## Procedure

### 1. Pre-flight
Before starting the rebase, gather the data the conflict triage relies on:

- Count divergent commits ahead of base:
  `git log --oneline origin/$BASE..HEAD`
- List overlapping files with base:
  `git diff --name-only origin/$BASE...HEAD`
- Fingerprint each commit on the branch with `(commit_message_hash, diff_size_bytes, touched_paths)`. Store the list — duplicate-skip and stray-skip both consult it.
- Note the PR / issue scope (title, labels) to detect commits whose messages are off-theme (stray candidates).

### 2. First pass
Run the rebase and capture the first conflict point if any:

```
git rebase "origin/$BASE"
```

If clean: skip to step 5 (Verify).
If conflicting: capture the conflict report:

- Conflicting files: `git diff --name-only --diff-filter=U`
- For each conflicting file capture the conflicting hunks (lines between `<<<<<<<` and `>>>>>>>`).
- Identify the offending commit being replayed: `git rev-parse REBASE_HEAD` and `git log -1 --format=%H%n%s%n%b REBASE_HEAD`.

### 3. Triage each conflict

For each conflict, classify and act in order:

#### a. HEAD identical to incoming → auto-resolve
Both sides produced byte-identical content for the conflicting region. Take either side and continue.

```
git checkout --theirs <path>   # or --ours, both produce the same bytes
git add <path>
git rebase --continue
```

#### b. HEAD supersedes (HEAD has the strictly newer revision of the same lines)
HEAD already includes a later iteration of the same change (e.g., the PR has a post-Gemini-fix commit that rewrote the same lines the replayed commit is touching). Keep HEAD's version verbatim.

```
git checkout --ours <path>
git add <path>
git rebase --continue
```

Document the decision: which HEAD commit supersedes which incoming commit.

#### c. Stray commit → `git rebase --skip`
The replayed commit is unrelated to the PR/issue scope (mismatched message theme, clearly an audit-findings or unrelated cleanup commit dropped in by a previous flaky run, no association with the PR title or labels).

```
git rebase --skip
```

Record the dropped commit SHA + subject so the final report can list it.

#### d. Duplicate commit → `git rebase --skip`
Compare the offending commit's fingerprint against commits already applied on HEAD (or already present in `origin/$BASE`). High overlap = duplicate. Heuristics:

- Identical commit message hash, OR
- Diff size and touched paths overlap > 80% with an already-applied commit, OR
- `git cherry origin/$BASE HEAD` reports the commit as `-` (already in upstream).

```
git rebase --skip
```

Record the dropped commit SHA + which already-applied commit it duplicates.

#### e. Otherwise → escalate with structured report
If none of (a)-(d) apply, the conflict is a genuine semantic conflict. Do NOT force a side. Abort cleanly and emit a structured report:

```
git rebase --abort
```

Emit a JSON-shaped report on stdout:

```
{
  "classification": "semantic_conflict",
  "pr": <pr_number>,
  "base": "<base_branch>",
  "head": "<head_branch>",
  "conflicting_files": ["<path>", ...],
  "conflicting_hunks": [
    {"path": "<path>", "ours_lines": "Lstart-Lend", "theirs_lines": "Lstart-Lend", "summary": "<one-line>"}
  ],
  "offending_commits": [{"sha": "<hex>", "subject": "<msg>"}]
}
```

End the agent run with that report — do NOT print the bare `manual resolution required` string. Future automation needs the file + hunk list to plan a next step.

### 4. Fallback: cherry-pick onto fresh main
If step 3 escalated *and* the orchestrator has explicitly enabled the cherry-pick fallback (label, config flag, or follow-up task), attempt a clean rebuild:

1. Reset the PR worktree to `origin/$BASE`:
   `git checkout -B "$BRANCH" "origin/$BASE"`
2. Walk the original commit list filtered through the duplicate-skip and stray-skip fingerprints from step 1, keeping only the "clean" commits.
3. `git cherry-pick <sha>` each remaining commit in order. If any cherry-pick conflicts:
   - Re-run the same triage rules from step 3 (HEAD-identical / HEAD-supersedes / stray / duplicate / escalate).
   - On semantic conflict, `git cherry-pick --abort` and emit the same structured report (extend with `"phase": "cherry_pick_fallback"`).
4. If every clean commit cherry-picks successfully, treat the result as the rebased branch and continue at step 5.

### 5. Verify
Before pushing, run the repo-appropriate build sanity check:

- Rust workspace: `cargo check --workspace --all-targets`
- TypeScript: `npx tsc --noEmit`
- Go: `go build ./...`
- Python: `python -m compileall <package>` or `pytest --collect-only` for syntax/import sanity.

If verification fails, do NOT push. Emit a structured report with `"classification": "verify_failed"` and the build error excerpt.

### 6. Push
Force-push only with lease safety:

```
git push --force-with-lease "$HEAD_REPO_URL" HEAD:"$BRANCH"
```

Report the new commit count (`git log --oneline origin/$BASE..HEAD | wc -l`) and the new base SHA (`git rev-parse origin/$BASE`).

## Output Format
End the run with one terminal token on its own line:

- `REBASE_PUSHED` — rebase or cherry-pick fallback succeeded, verify passed, force-push completed.
- `REBASE_SKIPPED` — branch was already up to date with base; nothing to do.
- `REBASE_CONFLICT` — escalation report emitted; manual review required.
- `REBASE_VERIFY_FAILED` — rebase resolved but build/test sanity check failed; report attached.

When emitting `REBASE_CONFLICT` or `REBASE_VERIFY_FAILED`, the *previous* lines of output MUST contain the structured report described above so the orchestrator can plan a next step without re-running the rebase.

## Constraints
- Never run `git checkout` or `git stash` in the main repository working tree — use `/tmp/harness-rebase-<pr>` worktree only.
- Never push without `--force-with-lease`.
- Never silently drop a commit. Every `--skip` decision must be logged with the dropped SHA, subject, and the rule (stray vs duplicate) that classified it.
- Never claim success without running step 5 verification in this session.
- If three consecutive triage attempts fail to make progress (no `--continue` and no `--skip` was applicable), stop and escalate per step 3e — this enforces the W-02 back-off.
