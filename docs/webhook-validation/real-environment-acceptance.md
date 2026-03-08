# GitHub Webhook Real-Environment Acceptance (FUT-101)

## Scope

This runbook validates **real GitHub webhook callbacks** for `pull_request_review` driven Linear state migration and idempotency stability.

In-scope for FUT-101:
- Real callback evidence (not local mock only)
- Transition chain checks for `Human Review -> Rework` and `Human Review -> Merging`
- Duplicate delivery idempotency checks
- Signature/error writeback evidence via real webhook delivery records

Out-of-scope for FUT-101:
- Re-implementing FUT-80 webhook parsing/endpoint logic

## Prerequisites

- `gh` authenticated for target repo (`repo` scope required)
- `LINEAR_API_KEY` exported
- `jq`, `curl`
- A **non-author reviewer identity** for true `CHANGES_REQUESTED` / `APPROVED` review submissions on the PR under test

Optional (deeper delivery inspection):
- `admin:repo_hook` scope to fetch per-delivery headers/payload and trigger API redelivery

## Evidence Collector

Use the repository script:

```bash
scripts/collect_webhook_acceptance_evidence.sh \
  --repo majiayu000/harness \
  --team-key FUT \
  --webhook-id 599474464 \
  --delivery-limit 60 \
  --canary FUT-103
```

Output directory format:

- `docs/webhook-validation/evidence-<UTC timestamp>/`

Key artifacts:

- `github-deliveries.json`: real delivery statuses (`202`/`503` etc.)
- `github-duplicate-guids.json`: repeated `guid` attempts (idempotency input)
- `linear-hr-to-rework.jsonl`: historical `Human Review -> Rework` events
- `linear-hr-to-merging.jsonl`: historical `Human Review -> Merging` events
- `linear-canary-state.json`: current canary issue state/history snapshot
- `summary.md`: acceptance gate summary

## Real Callback Validation Flow

1. Prepare canary issue/PR pair (title/branch contains target Linear identifier, e.g. `FUT-103`).
2. Set canary issue to `Human Review`.
3. From **non-author reviewer** account, submit:
   - a `CHANGES_REQUESTED` review
   - an `APPROVED` review
4. Run evidence collector immediately after each review submission.
5. Validate:
   - `Human Review -> Rework` observed for the changes-requested callback
   - `Human Review -> Merging` observed for the approve callback
6. For idempotency, redeliver same delivery (or use existing duplicate `guid`) and verify no extra state flap for the target issue.

## Acceptance Checklist Mapping

- Reproducible steps + evidence: collector command + evidence folder
- `Human Review -> Rework`: must have concrete row in collector output and canary/target issue history
- `Human Review -> Merging`: must have concrete row in collector output and canary/target issue history
- Duplicate delivery idempotency: same `guid` repeated without extra state oscillation
- Signature/error writeback: delivery status/response evidence captured in `github-deliveries.json`

## 2026-03-08 Snapshot (This Attempt)

Evidence folder:
- `docs/webhook-validation/evidence-20260308T011027Z/`

Observed facts:
- Latest real deliveries for hook `599474464` returned `503 Invalid HTTP Response`
- Historical `Human Review -> Rework` entries exist (`13`)
- Historical `Human Review -> Merging` entries found: `0`
- Duplicate delivery `guid` observed (`d1be72be-1a3b-11f1-98ae-3157e02427b4`) with mixed attempts (`202` then `503`)
- Canary issue `FUT-103` stayed in `Human Review` with no new state history during this attempt

Interpretation:
- Real callback path is currently degraded (`503`), so fresh acceptance for required transition paths cannot be completed in this run.
- Existing data is sufficient to prove real delivery activity and historical `Rework` path presence, but insufficient to satisfy `Merging` acceptance coverage.
