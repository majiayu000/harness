# Harness Multi-Project Batch Workflow

A practical guide for using Harness to orchestrate batch tasks across multiple projects.

## Prerequisites

```bash
# 1. Build the latest harness binary
cargo build --release

# 2. Configure projects in config/default.toml
# Add [[projects]] entries for each project you want to manage
```

```toml
# config/default.toml (append at the end)
[[projects]]
name = "harness"
root = "/Users/apple/Desktop/code/AI/tool/harness"
default = true
max_concurrent = 2

[[projects]]
name = "litellm-rs"
root = "/Users/apple/Desktop/code/AI/gateway/litellm-rs"
max_concurrent = 2

[[projects]]
name = "vibeguard"
root = "/Users/apple/Desktop/code/AI/tool/vibeguard"
max_concurrent = 1
```

```bash
# 3. Start server in a STANDALONE terminal (NOT inside Claude Code)
./target/release/harness serve --transport http --port 9800 --config config/default.toml

# 4. Verify projects registered
curl -s http://127.0.0.1:9800/api/dashboard | python3 -m json.tool
```

## Step 1: Plan Tasks by Priority

Organize tasks into priority waves (P0 → P1 → P2 → ...). Each wave runs in parallel, waves run sequentially.

Example for vibeguard:
- **P0** (open-source basics): CONTRIBUTING.md, CHANGELOG.md, Issue Templates, SECURITY.md, PR Template
- **P1** (distribution): npm publish, systemd support, cross-platform CI, Docker
- **P2** (quality): unit tests, integration tests, CI enhancement

## Step 2: Submit a Batch

### By prompt (for new work)

```bash
curl -s -X POST http://127.0.0.1:9800/tasks \
  -H 'Content-Type: application/json' \
  -d '{
    "project": "/path/to/project",
    "prompt": "Detailed task description...",
    "description": "Short label for logs"
  }'
```

### By issue number (for existing GitHub issues)

```bash
curl -s -X POST http://127.0.0.1:9800/tasks \
  -H 'Content-Type: application/json' \
  -d '{
    "project": "/path/to/project",
    "issue": 45,
    "description": "chore: upgrade rand 0.8 → 0.9"
  }'
```

### Monitor progress

```bash
# Dashboard overview
curl -s http://127.0.0.1:9800/api/dashboard | python3 -c "
import sys,json; d=json.load(sys.stdin)
print(f\"running={d['global']['running']} queued={d['global']['queued']} done={d['global']['done']} failed={d['global']['failed']}\")
for p in d['projects']:
    print(f\"  {p['id']:12s} running={p['tasks']['running']} queued={p['tasks']['queued']}\")
"

# Check PRs created
gh pr list --repo owner/repo --state open
```

## Step 3: Merge PRs (Sequential)

Parallel agent execution creates parallel PRs. These must be merged **sequentially** because each merge updates `main`, making subsequent PRs out-of-date.

### Merge loop pattern

```bash
REPO="owner/repo"

# 1. Pick the first PR to merge (usually the one with fewest conflicts)
gh pr merge <PR_NUMBER> --repo $REPO --merge

# 2. For each remaining PR:
#    a. Get branch name
BRANCH=$(gh pr view <PR_NUMBER> --repo $REPO --json headRefName --jq '.headRefName')

#    b. Rebase onto updated main
cd /path/to/project
git fetch origin main $BRANCH
git checkout $BRANCH
git rebase origin/main
git push --force-with-lease origin $BRANCH

#    c. Wait for CI, then merge
sleep 45
gh pr checks <PR_NUMBER> --repo $REPO
gh pr merge <PR_NUMBER> --repo $REPO --merge
```

### Handle duplicates

If retries created duplicate PRs for the same feature:

```bash
# Close the older duplicate
gh pr close <OLD_PR> --repo $REPO -c "Superseded by #<NEW_PR>"
```

### Handle CI failures after rebase

If a PR fails CI after rebase (usually because its branch predates a CI config change):

```bash
# Rebase to pick up new CI config
git fetch origin main
git checkout <branch>
git rebase origin/main
git push --force-with-lease origin <branch>
# CI will re-run with updated config
```

## Step 4: Close the Wave, Start Next

After all PRs in a wave are merged:

```bash
# Verify all merged
gh pr list --repo $REPO --state all --limit 20

# Submit next wave
# (repeat Step 2 with P1/P2/P3 tasks)
```

## Troubleshooting

### Server logs show `WARN ... unexpected argument`

External CLI (codex/claude) updated its flags. Check:

```bash
codex exec --help | grep -E "\-s|\-a|sandbox"
```

Fix the agent adapter code in `crates/harness-agents/src/codex.rs` or `claude.rs`, rebuild, restart.

### `GITHUB_TOKEN not set` warning

Codex review bot can't auto-post review comments. Non-blocking. To fix:

```bash
GITHUB_TOKEN=ghp_xxx ./target/release/harness serve ...
```

### PRs all show BLOCKED despite CI passing

Check branch protection rules:

```bash
gh api repos/owner/repo/branches/main/protection | python3 -m json.tool
```

Common cause: required status checks reference a CI job name that doesn't exist on older branches. Solution: merge the PR that introduces the new CI workflow first.

### Merge conflicts between parallel PRs

This is expected when parallel agents modify overlapping files. The merge loop pattern in Step 3 handles this. For heavy conflicts, consider setting `max_concurrent = 1` for the project.

## Real Session Example (2026-03-14)

### What was done

1. **Added config-file project registration** — `[[projects]]` in TOML instead of CLI `--project` flags
2. **Fixed Codex CLI 0.114.0 breaking change** — `-a` flag → `-s` flag, value mapping updated
3. **Vibeguard P0** (5 tasks) — CONTRIBUTING.md, CHANGELOG.md, Issue Templates, SECURITY.md, PR Template → 8 PRs created (3 duplicates from retries), 5 merged
4. **Vibeguard P1** (4 tasks) — npm publishing, systemd support, cross-platform CI, Docker → 4 PRs, all merged
5. **Vibeguard P2** (3 tasks) — guard unit tests, MCP integration tests, CI enhancement → in progress
6. **litellm-rs** (2 tasks) — rand upgrade, reqwest upgrade → 2 PRs, CI green

### Key metrics

- Total tasks submitted: 14
- PRs created: 16 (including 3 duplicates)
- PRs merged: 11
- Merge conflicts resolved: 6 (all from parallel execution)
- Time: ~2 hours for P0+P1 complete cycle (submit → merge)

### Lessons learned

- **Parallel execution is fast but creates merge overhead** — 4 parallel agents finish in ~10min, but sequential rebase+merge takes ~20min
- **Always merge the CI-changing PR first** — otherwise branch protection blocks everything
- **Keep task prompts detailed** — vague prompts produce lower-quality results
- **Codex review adds value** — catches real issues (quoting, test coverage) but requires correct CLI flags
