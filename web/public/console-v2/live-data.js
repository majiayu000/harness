(function () {
  const H = window.HC;
  const terminalStates = new Set(['done', 'cancelled']);
  const stateAliases = { agent_review: 'local_review_gate', reviewing: 'local_review_gate', queued: 'pending', running: 'implementing', completed: 'done', stalled: 'blocked' };
  const refreshView = () => { for (const notify of window.__dcRegistry?.Root?.subs || []) notify(); };
  const formatInt = (value) => typeof value === 'number' ? Intl.NumberFormat('en-US', { notation: 'compact', maximumFractionDigits: 1 }).format(value) : '—';
  const formatCost = (value) => typeof value === 'number' ? '$' + value.toFixed(2) : '—';
  const age = (value) => {
    if (!value) return '—';
    const seconds = Math.max(0, Math.floor((Date.now() - new Date(value).getTime()) / 1000));
    if (!Number.isFinite(seconds)) return '—';
    if (seconds < 60) return seconds + 's';
    if (seconds < 3600) return Math.floor(seconds / 60) + 'm';
    if (seconds < 86400) return Math.floor(seconds / 3600) + 'h ' + Math.floor(seconds % 3600 / 60) + 'm';
    return Math.floor(seconds / 86400) + 'd';
  };
  const secondsAge = (seconds) => seconds < 60 ? seconds + 's' : seconds < 3600 ? Math.floor(seconds / 60) + 'm' : Math.floor(seconds / 3600) + 'h';
  const state = (value) => stateAliases[value] || value || 'pending';
  const numberFrom = (value) => {
    if (typeof value === 'number') return value;
    const match = String(value || '').match(/(?:#|\/issues\/|\/pull\/)(\d+)(?:\b|$)/) || String(value || '').match(/^(\d+)$/);
    return match ? Number(match[1]) : null;
  };
  const prFrom = (url) => numberFrom(url && url.includes('/pull/') ? url : null);
  const label = (row) => row?.description?.trim() || row?.external_id || row?.task_kind || row?.workflow?.definition_id || row?.id || 'Workflow';

  async function request(path, init, timeoutMs = ['GET', 'HEAD'].includes((init?.method || 'GET').toUpperCase()) ? 15_000 : null) {
    const token = sessionStorage.getItem('harness_token')?.trim();
    const controller = timeoutMs == null ? null : new AbortController();
    const timeout = controller ? setTimeout(() => controller.abort(), timeoutMs) : null;
    try {
      const response = await fetch(path, {
        ...init, ...(controller ? { signal: controller.signal } : {}),
        headers: { Accept: 'application/json', ...(token ? { Authorization: 'Bearer ' + token } : {}), ...(init?.headers || {}) },
      });
      if (response.status === 401) {
        window.parent.postMessage({ type: 'harness:unauthorized' }, location.origin);
        throw new Error('Authentication required');
      }
      if (!response.ok) {
        let detail = path + ' → HTTP ' + response.status;
        try {
          const payload = await response.json();
          if (typeof payload.error === 'string') detail = payload.error;
        } catch { /* Keep the HTTP status when the error is not JSON. */ }
        throw new Error(detail);
      }
      return response.status === 204 ? null : await response.json();
    } finally {
      if (timeout != null) clearTimeout(timeout);
    }
  }

  async function allTasks(active = false, status = null) {
    const rows = [];
    let cursor = null;
    const seen = new Set();
    do {
      const params = new URLSearchParams({ limit: '200' });
      if (active) params.set('active', 'true');
      if (status) params.set('status', status);
      if (cursor) params.set('cursor', cursor);
      const page = await request('/api/workflows/runtime/submissions?' + params);
      rows.push(...(page.data || []));
      cursor = page.page?.has_more ? page.page.next_cursor : null;
      if (cursor && seen.has(cursor)) throw new Error('Task pagination returned a repeated cursor');
      if (cursor) seen.add(cursor);
    } while (cursor);
    return rows;
  }

  function actionInbox(action) {
    if (!action) return null;
    const kind = action.kind === 'failed' ? 'failed' : action.kind === 'blocked' ? 'blocked' : action.kind === 'ready_to_merge' ? 'ready_to_merge' : action.kind === 'awaiting_feedback' ? 'awaiting_feedback' : action.kind === 'driverless' ? 'driverless' : action.kind === 'approval' ? 'approval' : null;
    if (!kind) return null;
    const actions = kind === 'blocked' ? (action.can_unblock ? ['unblock', 'cancel'] : ['cancel'])
      : kind === 'failed' ? (action.can_retry ? ['retry', 'cancel'] : ['cancel'])
      : kind === 'ready_to_merge' ? ['merge'] : kind === 'driverless' ? ['cancel'] : kind === 'approval' ? ['approve', 'deny'] : action.url ? ['open'] : [];
    return {
      kind, reason: action.blocked_reason || action.failure_reason || action.error_kind || kind,
      hint: action.unblock_hint || action.retry_hint || action.next_action || '',
      next: action.next_action || '', actions,
      target: null, ago: secondsAge(action.age_secs || 0), url: action.url, requestId: action.requestId,
    };
  }

  function mapTask(task, byAction, byInvocation, byWorktree) {
    const id = task.workflow_id || task.workflow?.id || task.id;
    const project = H.projects.find(row => row.id === task.project || row.root === task.project || row.id === task.repo || task.repo?.endsWith('/' + row.id));
    const repo = project?.id || task.repo?.split('/').pop() || task.project?.split('/').pop() || '—';
    const issue = numberFrom(task.issue || task.workflow?.issue_number || task.external_id);
    const pr = task.pr || task.workflow?.pr_number || prFrom(task.pr_url);
    const current = state(task.workflow_state || task.workflow?.state || task.status);
    const invocation = byInvocation.get(id);
    const worktree = byWorktree.get(id);
    const action = byAction.get(id);
    const approval = task.pending_approvals?.find(item => item.type === 'approval_request' && item.id && item.approved == null);
    const tokenUsage = H.details?.get(id)?.task?.token_usage;
    const leaseState = { active_leased: 'active', expired_lease: 'expired', missing_lease: 'missing' }[invocation?.lease_state] || invocation?.lease_state || '—';
    return {
      id, submissionId: task.submission_id || task.id, repo, projectId: project?.id || task.project || null, n: issue || pr || '—', pr,
      repository: task.repo || null, issue, dependsOn: task.depends_on || [],
      failureClass: task.failure_kind || invocation?.failure_kind || null,
      approval: approval ? { command: approval.action, scope: 'Submission ' + (task.submission_id || task.id) } : null,
      approvals: (task.pending_approvals || []).filter(item => item.type === 'approval_request' && item.id && item.approved == null),
      ref: issue ? repo + '#' + issue : pr ? repo + '#PR' + pr : repo + ' · ' + String(task.id).slice(0, 8),
      prLabel: pr ? 'PR #' + pr : '—', prUrl: task.pr_url || action?.url || null,
      title: label(task), state: current, from: task.phase || null,
      agent: invocation?.agent_runtime || '—', model: invocation?.model || '—', effort: invocation?.reasoning_effort || '—',
      host: invocation?.lease_owner || '—', turn: task.turn || 0, max: task.max_turns || worktree?.max_turns || '—',
      age: age(task.created_at), obs: invocation?.last_runtime_observation_at ? age(invocation.last_runtime_observation_at) : '—',
      lease: leaseState, tokens: formatInt(tokenUsage?.total_tokens), cost: formatCost(tokenUsage?.cost_usd), file: '', sym: '', crate: repo,
      inbox: approval ? actionInbox({ kind: 'approval', requestId: approval.id, blocked_reason: 'approval_request', unblock_hint: approval.action, next_action: 'Approve or deny request' }) : actionInbox(action), waiting: task.scheduler?.authority_state || '', activity: invocation?.activity || '', taskKind: task.task_kind || '—',
      branch: worktree?.branch || '—', worktree: worktree?.path_short || '—',
      terminal: terminalStates.has(current), ago: age(task.updated_at || task.created_at), score: '—',
    };
  }

  function mapProjects(overview, usage, monitor, registry) {
    const costs = new Map((usage?.tokens_by_project || []).map(row => [row.name, row]));
    const dispatch = new Map((monitor?.activity?.token_dispatch_by_repo || []).map(row => [row.repo, row]));
    const registered = new Map((registry || []).map(row => [row.id, row]));
    return (overview?.projects || []).map(project => {
      const d = dispatch.get(project.id) || {};
      const c = costs.get(project.id);
      return {
        id: project.id, root: project.root, max: registered.get(project.id)?.max_concurrent ?? '—',
        merged: project.merged_24h, score: project.avg_score == null ? '—' : project.avg_score.toFixed(1),
        ruleFail: '—',
        tokens: formatInt(c?.total_tokens ?? project.tokens_24h), cost: formatCost(c?.estimated_cost_usd),
        intake: '—', memory: 0, trend: project.trend || [],
        dispatch: [
          ['implement_issue', d.agent_implement_issue_count || 0],
          ['address_feedback', d.agent_address_feedback_count || 0],
          ['merge_pr', d.agent_merge_pr_count || 0],
          ['dependency_analysis', d.agent_dependency_analysis_count || 0],
          ['skipped · covered', d.agent_skipped_covered_issue_count || 0],
          ['skipped · same fact hash', d.agent_skipped_same_fact_hash_count || 0],
        ],
      };
    });
  }

  function mapHosts(overview, dashboard) {
    const dashboardHosts = new Map((dashboard?.runtime_hosts || []).map(host => [host.id, host]));
    return (overview?.runtimes || []).map(host => {
      const entry = dashboardHosts.get(host.id);
      return {
        id: host.id, name: host.display_name, online: host.online,
        hb: age(host.last_heartbeat_at), leases: host.active_leases, cap: null,
        cpu: host.cpu_pct, ram: host.ram_pct, caps: (host.capabilities || []).join(' · '),
        projects: host.watched_projects, tokens: formatInt(host.tokens_24h),
        projectRoots: entry?.watched_project_roots || [],
        assignmentPressure: entry?.assignment_pressure,
      };
    });
  }

  function mapUsage(usage) {
    const summary = usage?.summary;
    if (!summary) return;
    const { hourly, hourlySource } = H.X.usage;
    const group = (rows) => (rows || []).map(row => [row.name, formatInt(row.total_tokens), formatCost(row.estimated_cost_usd), summary.total_tokens ? Math.round(100 * row.total_tokens / summary.total_tokens) : 0]);
    H.X.usage = {
      tokens: formatInt(summary.total_tokens), cost: formatCost(summary.estimated_cost_usd), turns: summary.request_count,
      cache: summary.total_tokens ? Math.round(100 * (summary.cache_read_input_tokens || 0) / summary.total_tokens) + '%' : '—',
      hourly, hourlySource, quotas: [], byProject: group(usage.tokens_by_project), byAgent: group(usage.tokens_by_agent), byModel: group(usage.tokens_by_model),
    };
  }

  function applyPayloads(payloads) {
    const [tasks, monitor, overview, usage, dashboard, snapshot, worktrees, intake, registry, skills, drafts, tokenUsage, approvals, runtimeSummary] = payloads;
    if (monitor) {
      H.health = {
        status: monitor.health.status, degraded: monitor.health.degraded_subsystems || [],
        uptime: secondsAge(monitor.health.uptime_secs || 0), logs: monitor.health.runtime_log_state,
        logPath: monitor.health.runtime_log_path || '—', retention: snapshot?.runtime_logs?.retention_days ? snapshot.runtime_logs.retention_days + 'd' : '—',
      };
      H.failures = (monitor.failures || []).map(row => ({ family: row.family, sev: row.severity, msg: row.message, count: row.count, repo: row.repo || '—', last: age(row.last_seen), retryable: row.retryable }));
    }
    if (snapshot) {
      const tick = snapshot.retry?.last_tick;
      H.retry = tick ? { at: age(tick.at), checked: tick.checked, retried: tick.retried, stuck: tick.stuck, skipped: tick.skipped } : H.retry;
      H.rate = {
        signalSources: snapshot.rate_limits?.signal_ingestion?.tracked_sources || 0,
        signalLimit: snapshot.rate_limits?.signal_ingestion?.limit_per_minute || 0,
        resetIds: snapshot.rate_limits?.password_reset?.tracked_identifiers || 0,
        resetLimit: snapshot.rate_limits?.password_reset?.limit_per_hour || 0,
      };
    }
    if (runtimeSummary?.summary?.circuit_breakers) H.breakers = runtimeSummary.summary.circuit_breakers.map(b => ({
      profile: b.profile, state: b.state, open: b.state === 'open', failures: b.consecutive ?? '—', window: b.class || '—', opened: '—', next: b.cooldown_until ? new Date(b.cooldown_until).toLocaleTimeString() : '—',
    }));
    if (dashboard) H.maxSlots = dashboard.global?.max_concurrent || 0;
    if (overview) H.projects = mapProjects(overview, usage, monitor, registry);
    else if (registry) H.projects = registry.map(project => ({
      id: project.id, root: project.root, max: project.max_concurrent ?? '—',
      merged: '—', score: '—', ruleFail: '—', tokens: '—', cost: '—',
      intake: '—', memory: 0, trend: [], dispatch: [],
    }));
    if (overview) H.hosts = mapHosts(overview, dashboard);
    if (usage) { mapUsage(usage); H.costNote = usage.cost?.message || ''; if (usage.cost?.configured === false) H.X.usage.cost = 'not priced'; }
    if (tokenUsage?.by_hour) {
      H.X.usage.hourly = Object.keys(tokenUsage.by_hour).sort().slice(-24).map(key => {
        const bucket = tokenUsage.by_hour[key];
        return ((bucket.input_tokens || 0) + (bucket.output_tokens || 0) + (bucket.cache_read_tokens || 0) + (bucket.cache_create_tokens || 0)) / 1_000_000;
      });
      H.X.usage.hourlySource = 'Claude CLI session logs · last 24h';
    }
    if (worktrees) H.worktrees = worktrees;
    if (intake) H.X.intake = (intake.channels || []).map(channel => {
      const webhook = channel.drivers?.webhook, polling = channel.drivers?.polling;
      const degraded = webhook?.degraded || polling?.degraded;
      return {
        k: channel.name, ep: channel.name === 'github' ? 'POST /webhook' : channel.name === 'feishu' ? 'POST /webhook/feishu' : 'POST /api/workflows/runtime/submissions',
        ok: channel.enabled && !degraded, last: !channel.enabled ? 'off' : degraded ? 'degraded' : 'on', n: channel.active, rej: channel.repos?.length ?? '—',
        note: channel.name === 'github' ? 'webhook ' + (webhook?.accepting ? 'accepting' : webhook?.reason || 'off') + ' · polling ' + (polling?.active ? polling.discovery_driver : 'off') : channel.enabled ? 'enabled' : 'disabled',
      };
    });
    if (intake) {
      H.intake = intake;
      H.intakeRecent = (intake.recent_dispatches || []).map(row => ({ src: row.source, ref: row.external_id || row.task_id, kind: row.status, ago: '—' }));
    }
    if (skills) H.X.skills = skills.map(skill => ({
      id: skill.id, name: skill.name, src: String(skill.location?.kind || skill.location || 'project'), uses: skill.usage_count,
      last: age(skill.last_used), gov: skill.governance_status, score: typeof skill.quality_score === 'number' ? (skill.quality_score * 10).toFixed(1) : '—',
    }));
    if (drafts) H.drafts = drafts.map(draft => ({
      id: draft.id, title: draft.artifacts?.[0]?.target_path || String(draft.signal?.signal_type || 'Draft'),
      signal: String(draft.signal?.signal_type || '—'), budget: '—', state: draft.status,
    }));
    if (tasks) {
      const bySubmission = new Map((approvals?.data || []).map(row => [row.submission_id, row.pending_approvals]));
      const byAction = new Map((monitor?.operator_actions || []).map(action => [action.workflow_id, action]));
      for (const stuck of monitor?.stuck_workflows || []) {
        if (!byAction.has(stuck.workflow_id)) byAction.set(stuck.workflow_id, { ...stuck, kind: stuck.state === 'failed' ? 'failed' : 'blocked', next_action: stuck.unblock_hint || stuck.retry_hint || stuck.state });
      }
      for (const driverless of monitor?.driverless_progress || []) {
        if (!byAction.has(driverless.workflow_id)) byAction.set(driverless.workflow_id, { ...driverless, kind: 'driverless', next_action: driverless.provenance_status });
      }
      const byInvocation = new Map();
      // The API orders active and newer invocations first for each workflow.
      for (const invocation of usage?.agent_invocations || []) {
        if (!byInvocation.has(invocation.workflow_id)) byInvocation.set(invocation.workflow_id, invocation);
      }
      const byWorktree = new Map((worktrees || []).filter(row => row.runtime_workflow_id).map(row => [row.runtime_workflow_id, row]));
      const rows = tasks.map(task => mapTask({ ...task, pending_approvals: approvals ? (bySubmission.get(task.id) || []) : (H.details.get(task.workflow_id || task.workflow?.id || task.id)?.task?.pending_approvals || []) }, byAction, byInvocation, byWorktree));
      const previous = new Map([...H.workflows, ...H.history].map(row => [row.id, row]));
      for (const row of rows) {
        const old = previous.get(row.id);
        row.changedAt = old && (old.state !== row.state || old.turn !== row.turn) ? Date.now() : old?.changedAt;
        if (!row.inbox && (row.lease === 'expired' || row.lease === 'missing')) row.inbox = { kind: 'lease', reason: row.lease + ' lease', hint: 'Check the runtime host before taking action', actions: ['cancel'], ago: row.obs };
      }
      const seen = new Set(rows.map(row => row.id));
      for (const action of byAction.values()) {
        if (seen.has(action.workflow_id)) continue;
        rows.push(mapTask({ id: action.task_id || action.workflow_id, workflow_id: action.workflow_id, repo: action.repo, issue: action.issue, pr_url: action.url, description: action.next_action, workflow_state: action.state, created_at: null, updated_at: null }, byAction, byInvocation, byWorktree));
      }
      H.workflows = rows.filter(row => !row.terminal);
      H.history = rows.filter(row => row.terminal || row.state === 'failed').map(row => ({ ...row, terminal: true }));
    }
    H.now = new Date().toLocaleString('en-US', { weekday: 'short', day: 'numeric', month: 'short', hour: '2-digit', minute: '2-digit' });
    refreshView();
  }

  let polling = false;
  let lastSecondaryRefresh = 0;
  let historicalTasks = [];
  let taskSnapshot = null;
  let rpcReady = false;
  let rpcHandshake = null;
  H.details = new Map();
  H.transcripts = new Map();
  H.transcriptFailed = new Set();
  H.contexts = new Map();
  H.events = [];
  H.eventsLoading = true;
  H.loadEvents = async () => {
    H.eventsLoading = true;
    refreshView();
    try {
      const dayAgo = Date.now() - 86400_000;
      const since = H.events[0]?.ts || new Date(dayAgo).toISOString();
      // event_query orders oldest first; limiting that query would hide new actions.
      const incoming = await H.rpc('event_query', { filters: { since } });
      H.events = [...new Map([...H.events, ...incoming].map(event => [event.id, event])).values()]
        .filter(event => new Date(event.ts).getTime() >= dayAgo)
        .sort((a, b) => new Date(b.ts) - new Date(a.ts)).slice(0, 200);
      H.eventsError = null;
    } catch (error) { H.eventsError = error.message || String(error); }
    H.eventsLoading = false;
    refreshView();
  };
  H.recordAction = async (action, workflow, reason, endpoint, status) => {
    await H.rpc('event_log', { event: {
      id: crypto.randomUUID(), ts: new Date().toISOString(), session_id: 'console', hook: 'console_action', tool: action,
      decision: status === 'accepted' ? 'complete' : 'warn', reason: reason || null,
      detail: JSON.stringify({ workflow_id: workflow?.id, ref: workflow?.ref, endpoint, status }),
      metadata: workflow ? { task_id: workflow.submissionId } : null, duration_ms: null,
    } });
    await H.loadEvents();
  };
  H.loadContext = async workflow => {
    const cached = H.contexts.get(workflow.id);
    if (cached?.loading || cached?.at && Date.now() - cached.at < 30_000) return;
    H.contexts.set(workflow.id, { loading: true });
    refreshView();
    try {
      const root = H.projects.find(project => project.id === workflow.projectId)?.root;
      if (!root) throw new Error('Project root unavailable for context preview');
      const result = await H.rpc('context_preview', { request: { thread_id: workflow.submissionId, project: root, task_profile: { task_kind: workflow.taskKind, prompt: workflow.title }, budget_hint: 0 }, supplied_items: [] });
      H.contexts.set(workflow.id, { ...result, at: Date.now(), loading: false });
    } catch (error) { H.contexts.set(workflow.id, { error: error.message || String(error), at: Date.now(), loading: false }); }
    refreshView();
  };
  H.memoryErrors = {};
  H.memoryUnavailable = {};
  H.memoryLoading = {};
  H.memoryRepos = {};
  H.loadMemory = async projectId => {
    if (H.memoryLoading[projectId]) return;
    const repos = [...new Set([...H.workflows, ...H.history].filter(w => w.projectId === projectId && w.repository).map(w => w.repository))];
    if (repos.length !== 1) {
      H.memoryUnavailable[projectId] = repos.length ? 'Multiple repositories recorded for this project; a unique repository is required.' : 'No repository identity recorded in this project’s submissions yet.';
      refreshView();
      return;
    }
    const repo = repos[0];
    H.memoryRepos[projectId] = repo;
    delete H.memoryUnavailable[projectId];
    H.memoryLoading[projectId] = true;
    refreshView();
    try {
      const response = await request('/api/projects/' + encodeURIComponent(repo) + '/memory');
      H.X.memory[projectId] = (response.records || []).map(record => ({
        id: record.id, kind: record.kind, text: typeof record.payload === 'string' ? record.payload : JSON.stringify(record.payload),
        src: record.evidence_ref || '—', age: age(record.created_at), outcome: record.outcome || '—', uses: record.use_count ?? '—',
      }));
      delete H.memoryErrors[projectId];
    } catch (error) { H.memoryErrors[projectId] = error.message || String(error); }
    finally { H.memoryLoading[projectId] = false; refreshView(); }
  };
  H.loadDetails = async (workflow) => {
    const cached = H.details.get(workflow.id);
    if (cached?.loading || cached?.loadedAt && Date.now() - cached.loadedAt < 30_000) return;
    H.details.set(workflow.id, { loading: true });
    const path = '/api/workflows/runtime/submissions/' + encodeURIComponent(workflow.submissionId);
    const requests = [request(path), request(path + '/artifacts'), request(path + '/prompts')];
    if (workflow.terminal) requests.push(request(path + '/proof'));
    const results = await Promise.allSettled(requests);
    let node = null;
    let treeError = null;
    try {
      const projectRoot = H.projects.find(project => project.id === workflow.projectId)?.root;
      for (let offset = 0; ; offset += 100) {
        const params = new URLSearchParams({ detail: 'full', limit: '100', offset: String(offset) });
        if (projectRoot) params.set('project_id', projectRoot);
        const page = await request('/api/workflows/runtime/tree?' + params);
        const walk = (nodes) => {
          for (const entry of nodes || []) {
            if (entry.workflow?.id === workflow.id) return entry;
            const child = walk(entry.children);
            if (child) return child;
          }
          return null;
        };
        node = walk(page.workflows);
        if (node || !page.pagination?.has_more) break;
      }
      if (!node) treeError = 'Workflow timeline and commands are unavailable';
    } catch (error) { treeError = error.message || String(error); }
    const value = (index) => results[index]?.status === 'fulfilled' ? results[index].value : null;
    const errors = results.filter(result => result.status === 'rejected').map(result => result.reason?.message || String(result.reason));
    if (treeError) errors.push(treeError);
    const detail = { task: value(0), artifacts: value(1) || [], prompts: value(2) || [], proof: value(3), node, error: errors.join(' · '), loading: false, loadedAt: Date.now() };
    H.details.set(workflow.id, detail);
    const approval = detail.task?.pending_approvals?.find(item => item.type === 'approval_request' && item.id && item.approved == null);
    if (approval) workflow.inbox = actionInbox({ kind: 'approval', requestId: approval.id, blocked_reason: 'approval_request', unblock_hint: approval.action, next_action: 'Approve or deny request' });
    const usage = detail.task?.token_usage;
    if (usage) {
      workflow.tokens = formatInt(usage.total_tokens);
      workflow.cost = formatCost(usage.cost_usd);
    }
    refreshView();
  };

  H.loadTranscript = async (workflow) => {
    if (H.transcripts.has(workflow.id) && !H.transcriptFailed.has(workflow.id)) return;
    H.transcriptFailed.delete(workflow.id);
    H.transcripts.set(workflow.id, []);
    const token = sessionStorage.getItem('harness_token')?.trim();
    try {
      const response = await fetch('/api/workflows/runtime/submissions/' + encodeURIComponent(workflow.submissionId) + '/stream', {
        headers: { Accept: 'text/event-stream', ...(token ? { Authorization: 'Bearer ' + token } : {}) },
      });
      if (response.status === 401) {
        window.parent.postMessage({ type: 'harness:unauthorized' }, location.origin);
        throw new Error('Authentication required');
      }
      if (!response.ok || !response.body) throw new Error('Transcript stream unavailable');
      const reader = response.body.getReader();
      try {
        const decoder = new TextDecoder();
        let buffer = '';
        while (true) {
          const result = await reader.read();
          if (result.done) throw new Error('Transcript ended before a terminal event; retry to reconnect');
          buffer += decoder.decode(result.value, { stream: true });
          const events = buffer.split('\n\n');
          buffer = events.pop() || '';
          for (const event of events) {
            const data = event.split('\n').find(line => line.startsWith('data: '));
            if (!data) continue;
            try {
              const item = JSON.parse(data.slice(6));
              if (item.type === 'message_delta') H.transcripts.get(workflow.id).push({ t: item.text, c: 'oklch(0.86 0.005 275)' });
              if (item.type === 'error') {
                H.transcripts.get(workflow.id).push({ t: item.message, c: 'var(--fail)' });
                H.transcriptFailed.add(workflow.id);
              }
              refreshView();
              if (item.type === 'done' || item.type === 'error') return;
            } catch { /* Ignore malformed stream events. */ }
          }
        }
      } finally {
        await reader.cancel().catch(() => {});
      }
    } catch (error) {
      H.transcripts.get(workflow.id).push({ t: error.message || String(error), c: 'var(--fail)' });
      H.transcriptFailed.add(workflow.id);
      refreshView();
    }
  };

  function applyTaskSnapshot() {
    if (!taskSnapshot || H.loadError) return;
    const payloads = [...taskSnapshot];
    const activeIds = new Set(payloads[0].map(task => task.id));
    payloads[0] = [...payloads[0], ...historicalTasks.filter(task => !activeIds.has(task.id))];
    applyPayloads(payloads);
  }

  let historyRefreshPending = false;
  async function refreshHistory() {
    if (H.historyLoading) { historyRefreshPending = true; return; }
    H.historyLoading = true;
    H.historyError = null;
    refreshView();
    try {
      historicalTasks = await allTasks(false, 'done,failed,cancelled');
      applyTaskSnapshot();
    } catch (error) {
      H.historyError = error.message || String(error);
    } finally {
      H.historyLoading = false;
      refreshView();
      if (historyRefreshPending) { historyRefreshPending = false; void refreshHistory(); }
    }
  }

  async function refresh(forceSecondary = false) {
    if (polling) return;
    polling = true;
    try {
      const secondaryDue = forceSecondary || Date.now() - lastSecondaryRefresh >= 60_000;
      if (secondaryDue) lastSecondaryRefresh = Date.now();
      const jobs = [
        allTasks(true), request('/api/operator-monitor'), request('/api/overview'), request('/api/usage-monitor'),
        request('/api/dashboard'), request('/api/operator-snapshot'), request('/api/worktrees'), request('/api/intake'),
        request('/projects'), secondaryDue ? H.rpc('skill_list', { query: null }, 15_000) : Promise.resolve(null),
        secondaryDue ? H.rpc('gc_drafts', { project_id: null }, 15_000) : Promise.resolve(null),
        secondaryDue ? request('/api/token-usage') : Promise.resolve(null),
        request('/api/workflows/runtime/approvals'),
        request('/api/workflows/runtime/tree?summary_only=true'),
      ];
      const results = await Promise.allSettled(jobs);
      const failures = results.filter(result => result.status === 'rejected').map(result => result.reason?.message || String(result.reason));
      const critical = [0, 1, 12].filter(index => results[index].status === 'rejected').map(index => results[index].reason?.message || String(results[index].reason));
      H.loadError = critical.length ? critical.join(' · ') : null;
      H.partialError = failures.length ? failures.join(' · ') : null;
      const payloads = results.map(result => result.status === 'fulfilled' ? result.value : null);
      const activeIds = new Set((payloads[0] || []).map(task => task.id));
      const finished = !critical.length && taskSnapshot?.[0].some(task => !activeIds.has(task.id));
      if (!critical.length) {
        // Retain only the inputs needed to remap tasks when history arrives.
        taskSnapshot = [payloads[0], payloads[1], null, payloads[3], null, null, payloads[6], null, null, null, null, null, payloads[12]];
      }
      payloads[0] = null;
      applyPayloads(payloads);
      applyTaskSnapshot();
      if (secondaryDue || finished) void refreshHistory();
      if (secondaryDue) void H.loadEvents();
      if (secondaryDue) await Promise.all(H.projects.map(project => H.loadMemory(project.id)));
      refreshView();
    } catch (error) {
      H.loadError = error.message || String(error);
      refreshView();
    } finally {
      H.loading = false;
      refreshView();
      polling = false;
    }
  }

  H.request = request;
  H.age = age;
  const rpcRequest = async (method, params = {}, timeoutMs = null) => {
    const result = await request('/rpc', { method: 'POST', headers: { 'Content-Type': 'application/json' }, body: JSON.stringify({ jsonrpc: '2.0', id: Date.now(), method, params }) }, timeoutMs);
    if (result.error) throw new Error(result.error.message || 'RPC request failed');
    return result.result;
  };
  H.rpc = async (method, params = {}, timeoutMs = null) => {
    if (!rpcReady) {
      rpcHandshake ||= (async () => {
        try {
          await rpcRequest('initialize', {}, timeoutMs);
          await rpcRequest('initialized', {}, timeoutMs);
        } catch (error) {
          if (error.message !== 'Server already initialized.') throw error;
        }
        rpcReady = true;
      })().finally(() => { rpcHandshake = null; });
      await rpcHandshake;
    }
    return rpcRequest(method, params, timeoutMs);
  };
  H.refresh = refresh;
  window.addEventListener('storage', event => { if (event.key === 'harness_token') { lastSecondaryRefresh = 0; refresh(); } });
  refresh();
  setInterval(refresh, 5000);
})();
