"use strict";

const COLUMNS = [
  { key: "pending", label: "Pending" },
  { key: "implementing", label: "Implementing" },
  { key: "agent_review", label: "Agent Review" },
  { key: "waiting", label: "Waiting" },
  { key: "reviewing", label: "Reviewing" },
  { key: "done", label: "Done" },
  { key: "failed", label: "Failed" },
];

const TERMINAL_DEFAULT_LIMIT = 5;
const POLL_INTERVAL_MS = 5000;
const WS_RECONNECT_MS = 3000;

let ws = null;
let pollTimer = null;
let currentTasks = [];
let searchQuery = "";
let statusFilter = "all";
const terminalExpanded = new Set();

function $(sel) { return document.querySelector(sel); }

/** Returns Authorization headers if a token is configured, otherwise {}. */
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

async function fetchTasks() {
  try {
    const resp = await fetch("/tasks", { headers: authHeaders() });
    if (!resp.ok) return;
    currentTasks = await resp.json();
    renderBoard(currentTasks);
    updateMetrics(currentTasks);
    renderPipeline(currentTasks);
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
  } catch {
    // intake api unavailable — leave channel grid empty
  }
}

function updateMetrics(tasks) {
  const counts = {};
  COLUMNS.forEach(c => counts[c.key] = 0);
  tasks.forEach(t => { counts[t.status] = (counts[t.status] || 0) + 1; });

  $("#metric-total").textContent = tasks.length;
  $("#metric-running").textContent =
    (counts.implementing || 0) + (counts.agent_review || 0) + (counts.reviewing || 0);
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

const ACTIVE_KEYS = new Set(["pending", "implementing", "agent_review", "waiting", "reviewing"]);
const TERMINAL_KEYS = new Set(["done", "failed"]);

function applyFilter(tasks) {
  const q = searchQuery.toLowerCase().trim();
  return tasks.filter(t => {
    if (statusFilter !== "all") {
      if (statusFilter === "active" && !ACTIVE_KEYS.has(t.status)) return false;
      if (statusFilter === "done" && t.status !== "done") return false;
      if (statusFilter === "failed" && t.status !== "failed") return false;
    }
    if (!q) return true;
    return (
      (t.description && t.description.toLowerCase().includes(q)) ||
      (t.id && t.id.toLowerCase().includes(q)) ||
      (t.repo && t.repo.toLowerCase().includes(q)) ||
      (t.error && t.error.toLowerCase().includes(q))
    );
  });
}

function renderBoard(tasks) {
  const filtered = applyFilter(tasks);
  const grouped = {};
  COLUMNS.forEach(c => grouped[c.key] = []);
  filtered.forEach(t => {
    if (grouped[t.status]) grouped[t.status].push(t);
  });

  // Sort terminal columns newest first
  TERMINAL_KEYS.forEach(key => {
    grouped[key].sort((a, b) => {
      const da = a.created_at ? new Date(a.created_at).getTime() : 0;
      const db = b.created_at ? new Date(b.created_at).getTime() : 0;
      return db - da;
    });
  });

  const board = $("#board");
  board.innerHTML = "";

  const activeRow = document.createElement("div");
  activeRow.className = "board-active";

  const terminalRow = document.createElement("div");
  terminalRow.className = "board-terminal";

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
      div.innerHTML += `<p class="empty-state">No tasks</p>`;
    } else if (TERMINAL_KEYS.has(col.key)) {
      const isExpanded = terminalExpanded.has(col.key);
      const visible = isExpanded ? items : items.slice(0, TERMINAL_DEFAULT_LIMIT);
      visible.forEach(task => div.appendChild(renderCard(task, col.key)));
      if (items.length > TERMINAL_DEFAULT_LIMIT) {
        const btn = document.createElement("button");
        btn.className = "btn-collapse";
        btn.textContent = isExpanded ? "Show less" : `Show all (${items.length})`;
        btn.addEventListener("click", () => {
          if (terminalExpanded.has(col.key)) {
            terminalExpanded.delete(col.key);
          } else {
            terminalExpanded.add(col.key);
          }
          renderBoard(currentTasks);
        });
        div.appendChild(btn);
      }
    } else {
      items.forEach(task => div.appendChild(renderCard(task, col.key)));
    }

    if (TERMINAL_KEYS.has(col.key)) {
      terminalRow.appendChild(div);
    } else {
      activeRow.appendChild(div);
    }
  });

  board.appendChild(activeRow);
  board.appendChild(terminalRow);
}

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
  if (task.phase && task.phase !== "default") {
    html += `<span class="phase-badge">${escapeHtml(task.phase)}</span>`;
  }
  html += `</div>`;

  // Title: prefer description, fallback to short ID
  const rawTitle = task.description
    ? (task.description.length > 80 ? task.description.slice(0, 80) + "\u2026" : task.description)
    : shortId;
  html += `<div class="task-title">${escapeHtml(rawTitle)}</div>`;

  // Meta row: turn, external_id, time
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
  if (task.phase && task.phase !== "default") body += `<span class="phase-badge">${escapeHtml(task.phase)}</span>`;
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

function connectWebSocket() {
  const proto = location.protocol === "https:" ? "wss:" : "ws:";
  const tok = window.__HARNESS_TOKEN__;
  const url = tok
    ? `${proto}//${location.host}/ws?token=${encodeURIComponent(tok)}`
    : `${proto}//${location.host}/ws`;

  try {
    ws = new WebSocket(url);
  } catch { return; }

  ws.onopen = () => { fetchTasks(); fetchIntake(); };

  ws.onmessage = (event) => {
    try {
      const msg = JSON.parse(event.data);
      const method = msg.method || "";
      if (method.startsWith("turn/") || method.startsWith("task/")) {
        fetchTasks();
        fetchIntake();
      }
    } catch { /* ignore non-JSON */ }
  };

  ws.onclose = () => {
    ws = null;
    setTimeout(connectWebSocket, WS_RECONNECT_MS);
  };

  ws.onerror = () => {
    if (ws) ws.close();
  };
}

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

function initSearchFilter() {
  const searchInput = document.getElementById("search-input");
  const statusSelect = document.getElementById("status-filter");

  if (searchInput) {
    searchInput.addEventListener("input", () => {
      searchQuery = searchInput.value;
      renderBoard(currentTasks);
    });
  }

  if (statusSelect) {
    statusSelect.addEventListener("change", () => {
      statusFilter = statusSelect.value;
      renderBoard(currentTasks);
    });
  }
}

function init() {
  fetchTasks();
  fetchIntake();
  connectWebSocket();
  initForm();
  initSearchFilter();
  document.addEventListener("keydown", (e) => {
    if (e.key === "Escape") closeDetail();
  });
  pollTimer = setInterval(fetchTasks, POLL_INTERVAL_MS);
  setInterval(fetchIntake, POLL_INTERVAL_MS);
}

document.addEventListener("DOMContentLoaded", init);
