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
  const tok = window.__HARNESS_TOKEN__;
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
  if (task.external_id != null) metaParts.push("#" + escapeHtml(String(task.external_id)));
  const timeAgo = relativeTime(task.created_at);
  if (timeAgo) metaParts.push(timeAgo);
  if (metaParts.length > 0) {
    html += `<div class="task-meta">${metaParts.join(" \u00b7 ")}</div>`;
  }

  if (task.pr_url) {
    const match = task.pr_url.match(/\/pull\/(\d+)/);
    const label = match ? "PR #" + match[1] : "PR";
    html += `<div class="task-pr"><a href="${escapeHtml(task.pr_url)}" target="_blank" onclick="event.stopPropagation()">${label}</a></div>`;
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

  let body = `<h2 class="detail-title">${escapeHtml(task.description || task.id || "")}</h2>`;

  body += `<div class="detail-badges">`;
  body += `<span class="state-badge badge-${escapeHtml(status)}">${escapeHtml(status.replace("_", " "))}</span>`;
  if (task.source) body += `<span class="source-badge source-badge-${escapeHtml(task.source)}">${escapeHtml(task.source)}</span>`;
  if (task.repo) body += `<span class="repo-badge">${escapeHtml(task.repo)}</span>`;
  if (task.phase && task.phase !== "implement") body += `<span class="phase-badge">${escapeHtml(task.phase)}</span>`;
  body += `</div>`;

  body += `<table class="detail-table">`;
  body += `<tr><th>ID</th><td><code class="detail-code">${escapeHtml(task.id || "")}</code></td></tr>`;
  if (task.repo) body += `<tr><th>Repo</th><td>${escapeHtml(task.repo)}</td></tr>`;
  if (task.external_id != null) body += `<tr><th>External ID</th><td>${escapeHtml(String(task.external_id))}</td></tr>`;
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
    body += `<a href="${escapeHtml(task.pr_url)}" target="_blank" class="detail-link">${escapeHtml(label)} \u2197</a>`;
    body += `</div>`;
  }

  if (task.error) {
    body += `<div class="detail-section">`;
    body += `<div class="detail-section-title detail-section-error">Error</div>`;
    body += `<pre class="detail-pre detail-pre-error">${escapeHtml(task.error)}</pre>`;
    body += `</div>`;
  }

  document.getElementById("detail-body").innerHTML = body;
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

// --- WebSocket ---

function connectWebSocket() {
  const proto = location.protocol === "https:" ? "wss:" : "ws:";
  const tok = window.__HARNESS_TOKEN__;
  const url = tok
    ? `${proto}//${location.host}/ws?token=${encodeURIComponent(tok)}`
    : `${proto}//${location.host}/ws`;
  try { ws = new WebSocket(url); } catch { return; }
  ws.onopen = () => { fetchTasks(); fetchIntake(); };
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
  ws.onclose = () => { ws = null; setTimeout(connectWebSocket, WS_RECONNECT_MS); };
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

// --- Init ---

function init() {
  initTabs();
  fetchTasks();
  fetchIntake();
  connectWebSocket();
  initForm();
  initHistoryControls();
  document.addEventListener("keydown", (e) => {
    if (e.key === "Escape") closeDetail();
  });
  pollTimer = setInterval(fetchTasks, POLL_INTERVAL_MS);
  setInterval(fetchIntake, POLL_INTERVAL_MS);
}

document.addEventListener("DOMContentLoaded", init);
