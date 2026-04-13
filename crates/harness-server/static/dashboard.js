"use strict";

const COLUMNS = [
  { key: "pending", label: "Pending" },
  { key: "implementing", label: "Implementing" },
  { key: "agent_review", label: "Agent Review" },
  { key: "waiting", label: "Waiting" },
  { key: "reviewing", label: "Reviewing" },
];

const HISTORY_PAGE_SIZE = 20;
const POLL_INTERVAL_MS = 5000;
const WS_RECONNECT_MS = 3000;

let ws = null;
let pollTimer = null;
let currentTasks = [];
let historyPage = 0;
let historyQuery = "";
let historyFilter = "all";

function $(sel) { return document.querySelector(sel); }

function authHeaders() {
  const tok = sessionStorage.getItem("harness_token");
  return tok ? { "Authorization": "Bearer " + tok } : {};
}

function relativeTime(ts) {
  if (!ts) return null;
  const date = new Date(ts);
  if (isNaN(date.getTime())) return null;
  const diff = Math.floor((Date.now() - date.getTime()) / 1000);
  if (diff < 60) return "just now";
  if (diff < 3600) return Math.floor(diff / 60) + "m ago";
  if (diff < 86400) return Math.floor(diff / 3600) + "h ago";
  return Math.floor(diff / 86400) + "d ago";
}

function githubUrl(repo, externalId) {
  if (!repo) return null;
  const base = "https://github.com/" + repo;
  if (!externalId) return base;
  const s = String(externalId);
  const m = s.match(/^(issue|pr)[:#]?(\d+)$/i);
  if (m) {
    const kind = m[1].toLowerCase() === "pr" ? "pull" : "issues";
    return base + "/" + kind + "/" + m[2];
  }
  if (/^\d+$/.test(s)) return base + "/issues/" + s;
  return base;
}

// --- Data fetching ---

async function fetchTasks() {
  try {
    const resp = await fetch("/tasks", { headers: authHeaders() });
    if (!resp.ok) return;
    currentTasks = await resp.json();
    renderBoard(currentTasks);
    updateMetrics(currentTasks);
    renderPipeline(currentTasks);
    renderHistory();
    updateStatus(true);
  } catch {
    updateStatus(false);
  }
}

async function fetchIntake() {
  try {
    const resp = await fetch("/api/intake", { headers: authHeaders() });
    if (!resp.ok) return;
    const data = await resp.json();
    renderIntakeChannels(data.channels || []);
  } catch {}
}

async function fetchDashboardSummary() {
  try {
    const resp = await fetch("/api/dashboard", { headers: authHeaders() });
    if (!resp.ok) return;
    const data = await resp.json();
    renderRuntimeHosts(data.runtime_hosts || []);
  } catch {}
}

// --- Metrics ---

function updateMetrics(tasks) {
  const counts = {};
  tasks.forEach(t => { counts[t.status] = (counts[t.status] || 0) + 1; });
  $("#metric-total").textContent = tasks.length;
  $("#metric-running").textContent =
    (counts.implementing || 0) + (counts.agent_review || 0) +
    (counts.reviewing || 0) + (counts.waiting || 0);
  $("#metric-done").textContent = counts.done || 0;
  $("#metric-failed").textContent = counts.failed || 0;
}

function updateStatus(connected) {
  const badge = $("#conn-badge");
  if (connected) {
    badge.textContent = "Live";
    badge.className = "status-badge status-badge-live";
  } else {
    badge.textContent = "Offline";
    badge.className = "status-badge";
  }
}

// --- Pipeline ---

function renderPipeline(tasks) {
  const row = document.getElementById("pipeline-row");
  if (!row) return;
  const stages = [
    { keys: ["pending"], label: "Queued" },
    { keys: ["implementing", "agent_review", "waiting"], label: "Building" },
    { keys: ["reviewing"], label: "Review" },
    { keys: ["done"], label: "Done" },
    { keys: ["failed"], label: "Failed" },
  ];
  const counts = {};
  tasks.forEach(t => { counts[t.status] = (counts[t.status] || 0) + 1; });
  row.innerHTML = stages.map((s, i) => {
    const count = s.keys.reduce((acc, k) => acc + (counts[k] || 0), 0);
    const arrow = i > 0 ? '<span class="pipeline-arrow">&#8594;</span>' : "";
    return arrow +
      `<div class="pipeline-step">` +
        `<span class="pipeline-count">${count}</span>` +
        `<span class="pipeline-label">${s.label}</span>` +
      `</div>`;
  }).join("");
}

// --- Intake channels ---

function renderIntakeChannels(channels) {
  const grid = document.getElementById("channel-grid");
  if (!grid) return;
  grid.innerHTML = "";
  channels.forEach(ch => {
    const enabled = Boolean(ch.enabled);
    const card = document.createElement("div");
    card.className = "channel-card" + (enabled ? " channel-card-enabled" : " channel-card-disabled");
    let detail = "";
    if (ch.repo) detail = ch.repo;
    else if (ch.keyword) detail = "keyword: " + ch.keyword;
    card.innerHTML =
      `<div class="channel-name">${escapeHtml(ch.name)}</div>` +
      (detail ? `<div class="channel-detail">${escapeHtml(detail)}</div>` : "") +
      `<div class="channel-status">${enabled ? "enabled" : "disabled"}</div>` +
      `<div class="channel-active">${escapeHtml(String(ch.active))} active</div>`;
    grid.appendChild(card);
  });
}

function renderRuntimeHosts(hosts) {
  const grid = document.getElementById("runtime-host-grid");
  if (!grid) return;
  grid.innerHTML = "";
  if (!hosts.length) {
    grid.innerHTML = '<p class="empty-state">No runtime hosts registered</p>';
    return;
  }
  hosts.forEach((host) => {
    const online = Boolean(host.online);
    const card = document.createElement("div");
    card.className = "channel-card" + (online ? " channel-card-enabled" : " channel-card-disabled");
    const caps = Array.isArray(host.capabilities) ? host.capabilities.join(", ") : "";
    const watched = Number(host.watched_projects || 0);
    const heartbeat = relativeTime(host.last_heartbeat_at) || "unknown";
    card.innerHTML =
      `<div class="channel-name">${escapeHtml(host.display_name || host.id || "runtime-host")}</div>` +
      `<div class="channel-detail">${escapeHtml(caps || "no capabilities")}</div>` +
      `<div class="channel-status">${online ? "online" : "offline"}</div>` +
      `<div class="channel-active">${escapeHtml(String(watched))} watched \u00b7 ${escapeHtml(heartbeat)}</div>`;
    grid.appendChild(card);
  });
}

// --- Active board (only active tasks) ---

const ACTIVE_KEYS = new Set(["pending", "implementing", "agent_review", "waiting", "reviewing"]);

function renderBoard(tasks) {
  const active = tasks.filter(t => ACTIVE_KEYS.has(t.status));
  const grouped = {};
  COLUMNS.forEach(c => grouped[c.key] = []);
  active.forEach(t => {
    if (grouped[t.status]) grouped[t.status].push(t);
  });

  const board = $("#board");
  board.innerHTML = "";

  if (active.length === 0) {
    board.innerHTML = '<p class="empty-state" style="padding:1rem">No active tasks</p>';
    return;
  }

  const row = document.createElement("div");
  row.className = "board-active";

  COLUMNS.forEach(col => {
    const items = grouped[col.key];
    const div = document.createElement("div");
    div.className = "column";
    div.innerHTML =
      `<div class="column-header col-border-${col.key}">` +
        `<span class="column-title">${col.label}</span>` +
        `<span class="column-count">${items.length}</span>` +
      `</div>`;
    if (items.length === 0) {
      div.innerHTML += '<p class="empty-state">No tasks</p>';
    } else {
      items.forEach(task => div.appendChild(renderCard(task, col.key)));
    }
    row.appendChild(div);
  });

  board.appendChild(row);
}

// --- History tab (done + failed, paginated) ---

function getFilteredHistory() {
  const terminal = currentTasks.filter(t => t.status === "done" || t.status === "failed");
  terminal.sort((a, b) => {
    const da = a.created_at ? new Date(a.created_at).getTime() : 0;
    const db = b.created_at ? new Date(b.created_at).getTime() : 0;
    return db - da;
  });

  const q = historyQuery.toLowerCase().trim();
  return terminal.filter(t => {
    if (historyFilter !== "all" && t.status !== historyFilter) return false;
    if (!q) return true;
    return (
      (t.description && t.description.toLowerCase().includes(q)) ||
      (t.id && t.id.toLowerCase().includes(q)) ||
      (t.repo && t.repo.toLowerCase().includes(q)) ||
      (t.error && t.error.toLowerCase().includes(q))
    );
  });
}

function renderHistory() {
  const list = document.getElementById("history-list");
  const pager = document.getElementById("history-pager");
  if (!list || !pager) return;

  const filtered = getFilteredHistory();
  const totalPages = Math.max(1, Math.ceil(filtered.length / HISTORY_PAGE_SIZE));
  if (historyPage >= totalPages) historyPage = totalPages - 1;
  if (historyPage < 0) historyPage = 0;

  const start = historyPage * HISTORY_PAGE_SIZE;
  const page = filtered.slice(start, start + HISTORY_PAGE_SIZE);

  list.innerHTML = "";
  if (page.length === 0) {
    list.innerHTML = '<p class="empty-state">No tasks match</p>';
  } else {
    page.forEach(task => list.appendChild(renderCard(task, task.status)));
  }

  // Pager
  if (totalPages <= 1) {
    pager.innerHTML = `<span class="pager-info">${filtered.length} tasks</span>`;
    return;
  }

  let html = "";
  html += `<button class="pager-btn" ${historyPage === 0 ? "disabled" : ""} data-page="${historyPage - 1}">&laquo; Prev</button>`;
  html += `<span class="pager-info">Page ${historyPage + 1} / ${totalPages} (${filtered.length} tasks)</span>`;
  html += `<button class="pager-btn" ${historyPage >= totalPages - 1 ? "disabled" : ""} data-page="${historyPage + 1}">Next &raquo;</button>`;
  pager.innerHTML = html;

  pager.querySelectorAll(".pager-btn").forEach(btn => {
    btn.addEventListener("click", () => {
      historyPage = parseInt(btn.dataset.page, 10);
      renderHistory();
    });
  });
}

// --- Card rendering (shared) ---

function renderCard(task, status) {
  const card = document.createElement("div");
  card.className = "task-card task-card-clickable";

  const shortId = (task.id && task.id.length > 8) ? task.id.slice(0, 8) : (task.id || "");

  let html = `<div class="task-card-header">`;
  html += `<span class="state-badge badge-${escapeHtml(status)}">${escapeHtml(status.replace("_", " "))}</span>`;
  if (task.source) {
    html += `<span class="source-badge source-badge-${escapeHtml(task.source)}">${escapeHtml(task.source)}</span>`;
  }
  if (task.repo) {
    html += `<span class="repo-badge">${escapeHtml(task.repo)}</span>`;
  }
  if (task.phase && task.phase !== "implement") {
    html += `<span class="phase-badge">${escapeHtml(task.phase)}</span>`;
  }
  html += `</div>`;

  const rawTitle = task.description
    ? (task.description.length > 80 ? task.description.slice(0, 80) + "\u2026" : task.description)
    : shortId;
  html += `<div class="task-title">${escapeHtml(rawTitle)}</div>`;

  const metaParts = [];
  if (task.turn > 0) metaParts.push("Turn " + task.turn);
  if (task.external_id != null) {
    const extCardUrl = githubUrl(task.repo, task.external_id);
    const extLabel = "#" + escapeHtml(String(task.external_id));
    if (extCardUrl) {
      metaParts.push(`<a href="${escapeHtml(extCardUrl)}" target="_blank" class="task-ext-link" onclick="event.stopPropagation()">${extLabel}</a>`);
    } else {
      metaParts.push(extLabel);
    }
  }
  const timeAgo = relativeTime(task.created_at);
  if (timeAgo) metaParts.push(timeAgo);
  if (metaParts.length > 0) {
    html += `<div class="task-meta">${metaParts.join(" \u00b7 ")}</div>`;
  }

  if (task.pr_url) {
    const match = task.pr_url.match(/\/pull\/(\d+)/);
    const label = match ? "PR #" + match[1] : "PR";
    const safeCardUrl = (task.pr_url.startsWith("https://") || task.pr_url.startsWith("http://")) ? escapeHtml(task.pr_url) : "#";
    html += `<div class="task-pr"><a href="${safeCardUrl}" target="_blank" onclick="event.stopPropagation()">${label}</a></div>`;
  }

  if (task.error) {
    const shortErr = task.error.length > 80 ? task.error.slice(0, 80) + "\u2026" : task.error;
    html += `<div class="task-error">${escapeHtml(shortErr)}</div>`;
  }

  card.innerHTML = html;
  card.addEventListener("click", () => showDetail(task));
  return card;
}

// --- Detail panel ---

function showDetail(task) {
  let panel = document.getElementById("detail-panel");
  if (!panel) {
    panel = document.createElement("div");
    panel.id = "detail-panel";
    panel.className = "detail-panel";
    panel.innerHTML =
      `<div class="detail-overlay" id="detail-overlay"></div>` +
      `<div class="detail-sheet" id="detail-sheet">` +
        `<button class="detail-close" id="detail-close" aria-label="Close">&times;</button>` +
        `<div id="detail-body" class="detail-body"></div>` +
      `</div>`;
    document.body.appendChild(panel);
    document.getElementById("detail-overlay").addEventListener("click", closeDetail);
    document.getElementById("detail-close").addEventListener("click", closeDetail);
  }

  const status = task.status || "unknown";
  const timeAgo = relativeTime(task.created_at);
  const createdStr = task.created_at ? new Date(task.created_at).toLocaleString() : null;

  const titleText = escapeHtml(task.description || task.id || "");
  const titleUrl = githubUrl(task.repo, task.external_id);
  let body = titleUrl
    ? `<h2 class="detail-title"><a href="${escapeHtml(titleUrl)}" target="_blank" class="detail-title-link">${titleText} \u2197</a></h2>`
    : `<h2 class="detail-title">${titleText}</h2>`;

  body += `<div class="detail-badges">`;
  body += `<span class="state-badge badge-${escapeHtml(status)}">${escapeHtml(status.replace("_", " "))}</span>`;
  if (task.source) body += `<span class="source-badge source-badge-${escapeHtml(task.source)}">${escapeHtml(task.source)}</span>`;
  if (task.repo) body += `<span class="repo-badge">${escapeHtml(task.repo)}</span>`;
  if (task.phase && task.phase !== "implement") body += `<span class="phase-badge">${escapeHtml(task.phase)}</span>`;
  const ACTIVE_STATUSES = ["pending", "awaiting_deps", "implementing", "agent_review", "waiting", "reviewing"];
  if (ACTIVE_STATUSES.includes(status)) {
    body += `<button class="cancel-btn" data-task-id="${escapeHtml(task.id || "")}">Cancel</button>`;
  }
  body += `</div>`;

  body += `<table class="detail-table">`;
  body += `<tr><th>ID</th><td><code class="detail-code">${escapeHtml(task.id || "")}</code></td></tr>`;
  if (task.repo) {
    const repoUrl = "https://github.com/" + escapeHtml(task.repo);
    body += `<tr><th>Repo</th><td><a href="${repoUrl}" target="_blank" class="detail-link">${escapeHtml(task.repo)} \u2197</a></td></tr>`;
  }
  if (task.external_id != null) {
    const extUrl = githubUrl(task.repo, task.external_id);
    const extText = escapeHtml(String(task.external_id));
    body += extUrl
      ? `<tr><th>External ID</th><td><a href="${escapeHtml(extUrl)}" target="_blank" class="detail-link">${extText} \u2197</a></td></tr>`
      : `<tr><th>External ID</th><td>${extText}</td></tr>`;
  }
  if (task.source) body += `<tr><th>Source</th><td>${escapeHtml(task.source)}</td></tr>`;
  if (task.phase) body += `<tr><th>Phase</th><td>${escapeHtml(task.phase)}</td></tr>`;
  if (task.turn > 0) body += `<tr><th>Turn</th><td>${escapeHtml(String(task.turn))}</td></tr>`;
  if (createdStr) {
    const timeLabel = timeAgo ? timeAgo + " (" + createdStr + ")" : createdStr;
    body += `<tr><th>Created</th><td>${escapeHtml(timeLabel)}</td></tr>`;
  }
  body += `</table>`;

  if (task.description) {
    body += `<div class="detail-section">`;
    body += `<div class="detail-section-title">Prompt / Description</div>`;
    body += `<pre class="detail-pre">${escapeHtml(task.description)}</pre>`;
    body += `</div>`;
  }

  if (task.pr_url) {
    const match = task.pr_url.match(/\/pull\/(\d+)/);
    const label = match ? "PR #" + match[1] : "View PR";
    body += `<div class="detail-section">`;
    body += `<div class="detail-section-title">Pull Request</div>`;
    const safeUrl = (task.pr_url.startsWith("https://") || task.pr_url.startsWith("http://")) ? escapeHtml(task.pr_url) : "#";
    body += `<a href="${safeUrl}" target="_blank" class="detail-link">${escapeHtml(label)} \u2197</a>`;
    body += `</div>`;
  }

  if (task.error) {
    body += `<div class="detail-section">`;
    body += `<div class="detail-section-title detail-section-error">Error</div>`;
    body += `<pre class="detail-pre detail-pre-error">${escapeHtml(task.error)}</pre>`;
    body += `</div>`;
  }

  document.getElementById("detail-body").innerHTML = body;

  const cancelBtn = document.querySelector(".cancel-btn[data-task-id]");
  if (cancelBtn) {
    cancelBtn.addEventListener("click", async () => {
      const taskId = cancelBtn.dataset.taskId;
      if (!confirm(`Cancel task ${taskId}?`)) return;
      try {
        const resp = await fetch(`/tasks/${encodeURIComponent(taskId)}/cancel`, { method: "POST" });
        if (!resp.ok) {
          const body = await resp.json().catch(() => ({}));
          alert(`Cancel failed: ${body.error || resp.status}`);
          return;
        }
        closeDetail();
        fetchTasks();
      } catch (e) {
        alert(`Cancel failed: ${e}`);
      }
    });
  }

  panel.classList.add("detail-panel-open");
  document.body.style.overflow = "hidden";
}

function closeDetail() {
  const panel = document.getElementById("detail-panel");
  if (panel) {
    panel.classList.remove("detail-panel-open");
    document.body.style.overflow = "";
  }
}

function escapeHtml(str) {
  const d = document.createElement("div");
  d.textContent = str;
  return d.innerHTML;
}

// --- Tabs ---

function initTabs() {
  const bar = document.getElementById("tab-bar");
  if (!bar) return;
  bar.addEventListener("click", (e) => {
    const btn = e.target.closest(".tab-btn");
    if (!btn) return;
    const tab = btn.dataset.tab;
    bar.querySelectorAll(".tab-btn").forEach(b => b.classList.remove("tab-btn-active"));
    btn.classList.add("tab-btn-active");
    document.querySelectorAll(".tab-panel").forEach(p => p.classList.remove("tab-panel-active"));
    const panel = document.getElementById("tab-" + tab);
    if (panel) panel.classList.add("tab-panel-active");
  });
}

// --- Token auth prompt ---

function showTokenPrompt(errorMsg) {
  let overlay = document.getElementById("token-auth-overlay");
  if (!overlay) {
    overlay = document.createElement("div");
    overlay.id = "token-auth-overlay";
    overlay.style.cssText =
      "position:fixed;inset:0;background:rgba(0,0,0,.6);display:flex;" +
      "align-items:center;justify-content:center;z-index:1000";
    overlay.innerHTML =
      '<div style="background:var(--surface,#1e1e2e);border:1px solid var(--border,#333);' +
      'border-radius:12px;padding:2rem;min-width:320px;max-width:90vw">' +
        '<h2 style="margin:0 0 .5rem;font-size:1.1rem">Harness Dashboard</h2>' +
        '<p style="margin:0 0 1rem;color:var(--muted,#888);font-size:.875rem">' +
          'Enter your API token to connect.</p>' +
        '<div id="token-auth-error" style="display:none;color:#ef4444;font-size:.875rem;' +
          'margin-bottom:.75rem"></div>' +
        '<input id="token-auth-input" type="password" placeholder="API token"' +
          ' autocomplete="current-password"' +
          ' style="width:100%;box-sizing:border-box;margin-bottom:.75rem;' +
            'padding:.5rem .75rem;border-radius:6px;' +
            'background:var(--surface2,#2a2a3e);border:1px solid var(--border,#333);' +
            'color:inherit;font-size:.9rem" />' +
        '<button id="token-auth-btn"' +
          ' style="padding:.5rem 1.25rem;border-radius:6px;border:none;' +
            'background:var(--accent,#3b82f6);color:#fff;cursor:pointer;font-size:.9rem">' +
          'Connect</button>' +
      '</div>';
    document.body.appendChild(overlay);
    const btn = document.getElementById("token-auth-btn");
    const input = document.getElementById("token-auth-input");
    function submit() {
      sessionStorage.setItem("harness_token", input.value);
      overlay.style.display = "none";
      connectWebSocket();
    }
    btn.addEventListener("click", submit);
    input.addEventListener("keydown", (e) => { if (e.key === "Enter") submit(); });
  }
  overlay.style.display = "flex";
  const errEl = document.getElementById("token-auth-error");
  if (errorMsg) {
    errEl.textContent = errorMsg;
    errEl.style.display = "";
  } else {
    errEl.style.display = "none";
  }
  const input = document.getElementById("token-auth-input");
  input.value = "";
  setTimeout(() => input.focus(), 0);
}

// --- WebSocket ---

function connectWebSocket() {
  const proto = location.protocol === "https:" ? "wss:" : "ws:";
  const url = `${proto}//${location.host}/ws`;
  try { ws = new WebSocket(url); } catch { return; }
  ws.onopen = () => {
    const tok = sessionStorage.getItem("harness_token");
    if (tok) {
      ws.send(JSON.stringify({ type: "auth", token: tok }));
    }
    fetchTasks();
    fetchIntake();
  };
  ws.onmessage = (event) => {
    try {
      const msg = JSON.parse(event.data);
      const method = msg.method || "";
      if (method.startsWith("turn/") || method.startsWith("task/")) {
        fetchTasks();
        fetchIntake();
      }
    } catch {}
  };
  ws.onclose = (event) => {
    ws = null;
    if (event.code === 4401) {
      showTokenPrompt("Invalid token. Please try again.");
    } else {
      setTimeout(connectWebSocket, WS_RECONNECT_MS);
    }
  };
  ws.onerror = () => { if (ws) ws.close(); };
}

// --- Form ---

function showFeedback(el, msg, type) {
  el.textContent = msg;
  el.className = `form-feedback form-feedback-${type}`;
}

function initForm() {
  const form = document.getElementById("task-form");
  if (!form) return;
  const feedback = document.getElementById("task-form-feedback");
  const btn = document.getElementById("task-submit-btn");

  form.addEventListener("submit", async (e) => {
    e.preventDefault();
    const title = form.elements["title"].value.trim();
    const description = form.elements["description"].value.trim();
    if (!title || !description) {
      showFeedback(feedback, "Title and description are required.", "error");
      return;
    }
    feedback.textContent = "";
    feedback.className = "form-feedback";
    btn.disabled = true;
    btn.textContent = "Submitting\u2026";
    const prompt = `${title}\n\n${description}`;
    try {
      const resp = await fetch("/tasks", {
        method: "POST",
        headers: { "Content-Type": "application/json", ...authHeaders() },
        body: JSON.stringify({ prompt }),
      });
      if (!resp.ok) {
        const text = await resp.text();
        throw new Error(`${resp.status}: ${text}`);
      }
      form.reset();
      showFeedback(feedback, "Task submitted successfully.", "success");
      fetchTasks();
    } catch (err) {
      showFeedback(feedback, `Error: ${err.message}`, "error");
    } finally {
      btn.disabled = false;
      btn.textContent = "Submit Task";
    }
  });
}

// --- History search/filter ---

function initHistoryControls() {
  const search = document.getElementById("history-search");
  const filter = document.getElementById("history-filter");
  if (search) {
    search.addEventListener("input", () => {
      historyQuery = search.value;
      historyPage = 0;
      renderHistory();
    });
  }
  if (filter) {
    filter.addEventListener("change", () => {
      historyFilter = filter.value;
      historyPage = 0;
      renderHistory();
    });
  }
}

// --- Token Usage ---

const MODEL_COLORS = [
  "#3b82f6", "#22c55e", "#f59e0b", "#a855f7", "#ef4444",
  "#06b6d4", "#ec4899", "#84cc16", "#f97316", "#6366f1",
];

function fmtNum(n) {
  if (n >= 1e9) return (n / 1e9).toFixed(1) + "B";
  if (n >= 1e6) return (n / 1e6).toFixed(1) + "M";
  if (n >= 1e3) return (n / 1e3).toFixed(1) + "K";
  return String(n);
}
function fmtTokens(n) { return n.toLocaleString(); }

async function fetchTokenUsage() {
  try {
    const resp = await fetch("/api/token-usage", { headers: authHeaders() });
    if (!resp.ok) {
      let message = `HTTP ${resp.status}`;
      try {
        const errorBody = await resp.json();
        if (errorBody && typeof errorBody.error === "string" && errorBody.error.length > 0) {
          message = errorBody.error;
        }
      } catch {}
      console.error("token usage fetch failed:", message);
      renderTokenError(message);
      return;
    }
    const data = await resp.json();
    renderTokenMetrics(data);
    if (data.source_dir_missing) {
      // Inject the diagnostic warning into the chart container instead of
      // calling renderRequestChart, which would immediately replace this
      // message with "No data" when by_hour is empty.
      const chart = document.getElementById("tok-req-chart");
      if (chart) {
        chart.innerHTML =
          '<p class="empty-state" style="color:#f59e0b">' +
          "&#9888; Session directory (~/.claude/projects) not found &mdash; " +
          "check $HOME configuration, or expected when using --no-session-persistence" +
          "</p>";
      }
    } else {
      renderRequestChart(data.by_hour || {});
    }
    renderModelTrend(data.model_trend || {}, data.models || []);
    renderTokenDayTable(data.by_day || {});
    renderTokenModelTable(data.by_model || {});
    renderTokenTaskTable(data.task_usage || []);
  } catch (err) {
    const message = err && err.message ? err.message : String(err);
    console.error("token usage fetch failed:", message);
    renderTokenError(message);
  }
}

function renderTokenError(message) {
  const metricIds = [
    "tok-requests",
    "tok-avg-req",
    "tok-sessions",
    "tok-context",
    "tok-output",
    "tok-cost",
  ];
  metricIds.forEach((id) => {
    const el = document.getElementById(id);
    if (el) el.textContent = "ERR";
  });

  const safe = escapeHtml(message || "token usage unavailable");
  const chartIds = ["tok-req-chart", "tok-model-chart"];
  chartIds.forEach((id) => {
    const el = document.getElementById(id);
    if (el) el.innerHTML = `<p class="empty-state">Error: ${safe}</p>`;
  });

  const tableBodies = {
    "tok-day-body": 5,
    "tok-model-body": 5,
    "tok-task-body": 6,
  };
  Object.entries(tableBodies).forEach(([id, cols]) => {
    const body = document.getElementById(id);
    if (body) body.innerHTML = `<tr><td colspan="${cols}">Error: ${safe}</td></tr>`;
  });
}

function renderTokenMetrics(data) {
  const el = (id, val) => { const e = document.getElementById(id); if (e) e.textContent = val; };
  el("tok-requests", fmtNum(data.total_requests || 0));
  el("tok-context", fmtNum(data.total_context || 0));
  el("tok-output", fmtNum((data.totals || {}).output_tokens || 0));
  el("tok-cost", "$" + (data.estimated_cost_usd || 0).toFixed(2));
  // Avg requests per hour (from by_hour data)
  const hours = Object.keys(data.by_hour || {});
  const avgReq = hours.length > 0 ? Math.round((data.total_requests || 0) / hours.length) : 0;
  el("tok-avg-req", fmtNum(avgReq));
  el("tok-sessions", fmtNum(data.session_count || 0));
}

// --- Request count bar chart (SVG) ---
function renderRequestChart(byHour) {
  const el = document.getElementById("tok-req-chart");
  if (!el) return;
  const hours = Object.keys(byHour).sort();
  if (hours.length === 0) { el.innerHTML = '<p class="empty-state">No data</p>'; return; }
  // Show last 48 hours max.
  const recent = hours.slice(-48);
  const vals = recent.map(h => byHour[h].request_count || 0);
  const max = Math.max(...vals, 1);
  const total = vals.reduce((a, b) => a + b, 0);
  const avg = Math.round(total / vals.length);

  const W = 760, H = 180, pad = 40, barGap = 1;
  const barW = Math.max(2, (W - pad * 2) / vals.length - barGap);
  const scaleY = (v) => (H - pad - 10) * (v / max);

  let bars = "", labels = "";
  const labelStep = Math.max(1, Math.floor(vals.length / 12));
  vals.forEach((v, i) => {
    const x = pad + i * (barW + barGap);
    const bh = scaleY(v);
    const y = H - pad - bh;
    bars += `<rect x="${x}" y="${y}" width="${barW}" height="${bh}" rx="2" fill="#3b82f6" opacity="0.85">` +
      `<title>${recent[i].slice(11,16)}: ${v} requests</title></rect>`;
    if (i % labelStep === 0) {
      labels += `<text x="${x + barW/2}" y="${H - pad + 14}" text-anchor="middle" fill="var(--muted)" font-size="10">${recent[i].slice(11,16)}</text>`;
    }
  });

  // Y-axis labels.
  let yLabels = "";
  for (let i = 0; i <= 4; i++) {
    const v = Math.round(max * i / 4);
    const y = H - pad - scaleY(v);
    yLabels += `<text x="${pad - 4}" y="${y + 3}" text-anchor="end" fill="var(--muted)" font-size="10">${fmtNum(v)}</text>`;
    yLabels += `<line x1="${pad}" x2="${W - 10}" y1="${y}" y2="${y}" stroke="var(--line)" stroke-dasharray="3,3"/>`;
  }

  const summary = `<text x="${W - 10}" y="16" text-anchor="end" fill="var(--muted)" font-size="12">Total: <tspan font-weight="600" fill="var(--ink)">${fmtNum(total)}</tspan>  Avg/h: <tspan font-weight="600" fill="var(--ink)">${avg}</tspan></text>`;

  el.innerHTML = `<svg viewBox="0 0 ${W} ${H}" style="width:100%;height:auto">${summary}${yLabels}${bars}${labels}</svg>`;
}

// --- Model usage trend (SVG area chart) ---
function renderModelTrend(modelTrend, models) {
  const el = document.getElementById("tok-model-chart");
  if (!el) return;
  const hours = Object.keys(modelTrend).sort().slice(-48);
  if (hours.length < 2 || models.length === 0) { el.innerHTML = '<p class="empty-state">No data</p>'; return; }

  const topModels = models.slice(0, 8);

  // Build series: model -> [values per hour].
  const series = {};
  topModels.forEach(m => { series[m] = hours.map(h => (modelTrend[h] || {})[m]?.tokens || 0); });

  const maxVal = Math.max(...hours.map((_, i) => topModels.reduce((s, m) => s + (series[m][i] || 0), 0)), 1);

  const W = 760, H = 200, pad = 50;
  const xStep = (W - pad - 10) / (hours.length - 1);
  const scaleY = (v) => (H - pad - 20) * (v / maxVal);

  let paths = "", legendHtml = "";

  // Draw each model as a filled area.
  topModels.forEach((m, mi) => {
    const color = MODEL_COLORS[mi % MODEL_COLORS.length];
    const pts = series[m];
    let d = "";
    pts.forEach((v, i) => {
      const x = pad + i * xStep;
      const y = H - pad - scaleY(v);
      d += (i === 0 ? "M" : "L") + `${x.toFixed(1)},${y.toFixed(1)}`;
    });
    // Line.
    paths += `<path d="${d}" fill="none" stroke="${color}" stroke-width="2" opacity="0.9"/>`;
    // Fill.
    const last = pad + (pts.length - 1) * xStep;
    paths += `<path d="${d}L${last.toFixed(1)},${H - pad}L${pad},${H - pad}Z" fill="${color}" opacity="0.12"/>`;
    // Legend.
    legendHtml += `<span class="tok-legend-item"><span class="tok-legend-dot" style="background:${color}"></span>${escapeHtml(m)}</span>`;
  });

  // X-axis labels.
  let labels = "";
  const labelStep = Math.max(1, Math.floor(hours.length / 10));
  hours.forEach((h, i) => {
    if (i % labelStep === 0) {
      const x = pad + i * xStep;
      labels += `<text x="${x}" y="${H - pad + 14}" text-anchor="middle" fill="var(--muted)" font-size="10">${h.slice(11,16)}</text>`;
    }
  });

  // Y-axis.
  let yLabels = "";
  for (let i = 0; i <= 4; i++) {
    const v = Math.round(maxVal * i / 4);
    const y = H - pad - scaleY(v);
    yLabels += `<text x="${pad - 4}" y="${y + 3}" text-anchor="end" fill="var(--muted)" font-size="10">${fmtNum(v)}</text>`;
    yLabels += `<line x1="${pad}" x2="${W - 10}" y1="${y}" y2="${y}" stroke="var(--line)" stroke-dasharray="3,3"/>`;
  }

  el.innerHTML = `<svg viewBox="0 0 ${W} ${H}" style="width:100%;height:auto">${yLabels}${paths}${labels}</svg>` +
    `<div class="tok-legend">${legendHtml}</div>`;
}

function estimateCost(v) {
  const hasCache = (v.cache_read_tokens || 0) > 0 || (v.cache_create_tokens || 0) > 0;
  let inp = v.input_tokens || 0, cr = v.cache_read_tokens || 0;
  if (!hasCache && inp > 0) { cr = inp * 0.9; inp = inp * 0.1; }
  return inp / 1e6 * 3 + (v.output_tokens || 0) / 1e6 * 15
       + cr / 1e6 * 0.30 + (v.cache_create_tokens || 0) / 1e6 * 3.75;
}

function renderTokenDayTable(byDay) {
  const body = document.getElementById("tok-day-body");
  if (!body) return;
  const days = Object.keys(byDay).sort().reverse();
  body.innerHTML = days.map(d => {
    const v = byDay[d];
    const ctx = (v.input_tokens||0) + (v.cache_read_tokens||0) + (v.cache_create_tokens||0);
    const cost = estimateCost(v);
    return `<tr><td>${escapeHtml(d)}</td><td>${fmtNum(v.request_count||0)}</td>` +
      `<td>${fmtNum(ctx)}</td><td>${fmtNum(v.output_tokens||0)}</td>` +
      `<td>$${cost.toFixed(2)}</td></tr>`;
  }).join("");
}

function renderTokenModelTable(byModel) {
  const body = document.getElementById("tok-model-body");
  if (!body) return;
  const models = Object.entries(byModel).sort((a, b) => {
    const ca = (a[1].input_tokens||0) + (a[1].cache_read_tokens||0);
    const cb = (b[1].input_tokens||0) + (b[1].cache_read_tokens||0);
    return cb - ca;
  });
  body.innerHTML = models.map(([name, v]) => {
    const ctx = (v.input_tokens||0) + (v.cache_read_tokens||0) + (v.cache_create_tokens||0);
    const cost = estimateCost(v);
    return `<tr><td>${escapeHtml(name)}</td><td>${fmtNum(v.request_count||0)}</td>` +
      `<td>${fmtNum(ctx)}</td><td>${fmtNum(v.output_tokens||0)}</td>` +
      `<td>$${cost.toFixed(2)}</td></tr>`;
  }).join("");
}

function renderTokenTaskTable(tasks) {
  const body = document.getElementById("tok-task-body");
  if (!body) return;
  body.innerHTML = tasks.slice(0, 20).map(t => {
    return `<tr><td><code>${escapeHtml((t.task_id||"").slice(0,8))}</code></td>` +
      `<td>${escapeHtml(t.repo||"")}</td><td>${escapeHtml(t.status||"")}</td>` +
      `<td>${fmtNum(t.requests||0)}</td><td>${fmtNum(t.context_tokens||0)}</td>` +
      `<td>$${(t.cost_usd||0).toFixed(2)}</td></tr>`;
  }).join("");
}

// --- Init ---

function initTokenAuth() {
  if (sessionStorage.getItem("harness_token") === null) {
    showTokenPrompt(null);
  } else {
    connectWebSocket();
  }
}

function init() {
  initTabs();
  fetchTasks();
  fetchIntake();
  fetchDashboardSummary();
  fetchTokenUsage();
  initTokenAuth();
  initForm();
  initHistoryControls();
  document.addEventListener("keydown", (e) => {
    if (e.key === "Escape") closeDetail();
  });
  pollTimer = setInterval(fetchTasks, POLL_INTERVAL_MS);
  setInterval(fetchIntake, POLL_INTERVAL_MS);
  setInterval(fetchDashboardSummary, POLL_INTERVAL_MS);
  setInterval(fetchTokenUsage, POLL_INTERVAL_MS);
}

document.addEventListener("DOMContentLoaded", init);
