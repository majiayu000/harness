# Console v3 integration

The dashboard uses the `Harness Console v3.dc.html` design from the September 26
export. Its theme and 827 inline styles are retained. The existing
`/console-v2/index.html` asset URL remains the dashboard entry point; it now
renders v3. Production loads no example workflows, usage, events, or memories.
The data scripts load once, in order, before the design runtime so cold loads
cannot reset shared state while the dashboard is rendering.

## Data and actions

| Surface | Source and behavior |
| --- | --- |
| Home / Fleet / History | Paginated runtime submissions, operator monitor, usage monitor and pending approvals. Active polling runs every five seconds; history loads independently every minute and after observed completion. |
| Approvals | Every pending request is listed. Confirmation uses its submission ID and request ID, then rechecks that request before sending accept/reject. |
| Bulk actions | Eligibility comes from the operator monitor. A shared reason is required. Each eligible workflow receives one request; partial failures remain visible. |
| Dependencies / failures | Submission `depends_on` links and `failure_kind`; retry/unblock eligibility is supplied by the server, not a client classification table. |
| Events / operator activity | `event_query` re-reads the 24-hour window to include late commits and clock skew; the newest 200 events are shown. Workflow action results use `event_log` with `hook=console_action`. These are client-reported results, not tamper-proof server audit evidence. No localStorage audit or fabricated events are used. |
| Prompts / artifacts / proof | Submission detail endpoints; actual prompt text and artifact content can be inspected. |
| Context | `context_preview` composes a current preview using the server budget. It is not a reconstruction of a previously executed turn; recorded prompts remain separate. |
| Memory | Project memory records, using the repository identity recorded in submissions, including outcome/use count; deletion uses the memory endpoint. |
| Intake / breakers / system | Intake channel driver status and recent dispatches, runtime tree summary, operator snapshot and registered hosts. |
| Worktree cleanup | Copies a shell-quoted `git worktree remove` command for a server-classified orphan. Does not execute deletion or add `--force`. |

Requests retain session-token Bearer authentication, unauthorized notification,
read timeouts, and stale-state/error reporting. Mutations do not inherit the short polling deadline. Failed detail requests retry at most
once per 30 seconds. Workflow data is preserved when an essential poll fails.
An action-log error does not turn an accepted mutation into a failed mutation or
silently replay it.

## Deliberate limits

- Browser WebSocket clients cannot send the Bearer header required by `/ws`.
  Authenticated polling remains active and is labeled in the UI. No credential
  is added to a WebSocket URL and server authentication is not relaxed.
- Provider quotas, eval execution, rule auto-fix, and exec-policy evaluation do
  not have dashboard endpoints. The UI reports that limitation. Rule check
  results are populated after a real check, not seeded with a rule catalog.
- The transcript reconstruction endpoint requires a runtime job and supplied
  content; it is not an automatic recovery endpoint. The dashboard retries the
  actual transcript stream after a failure instead of inventing reconstructed
  output.
- Hourly usage is labeled as Claude CLI session logs. Aggregate attribution is
  supplied by the usage monitor; absent prices remain unpriced.
- The context preview, event window and displayed submission attempts are
  limited to their stated sources. Large-dataset performance is not established
  by functional tests.

## Verification

`web/src/consoleV3.test.js` covers the nine view bindings, exact approval identity,
stale approvals, bulk eligibility/reasons/partial failures, action-log failures,
dependency cycles, quoted cleanup paths, onboarding, and mobile view selection.
`consoleLiveData.test.js` covers the adapter's pagination and failure behavior.
`consoleStyle.test.js` pins the v3 export's original theme and inline styles.

Run `cd web && bun run test && bun run build`; build `harness-cli` to verify the
embedded dashboard and use a disposable PostgreSQL database for live smoke tests.
