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

const POLL_INTERVAL_MS = 5000;
const WS_RECONNECT_MS = 3000;

let ws = null;
let pollTimer = null;

function $(sel) { return document.querySelector(sel); }

async function fetchTasks() {
  try {
    const resp = await fetch("/tasks");
    if (!resp.ok) return;
    const tasks = await resp.json();
    renderBoard(tasks);
    updateMetrics(tasks);
    renderPipeline(tasks);
    updateStatus(true);
  } catch {
    updateStatus(false);
  }
}

async function fetchIntake() {
  try {
    const resp = await fetch("/api/intake");
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
      `<div class="channel-active">${ch.active} active</div>`;

    grid.appendChild(card);
  });
}

function renderBoard(tasks) {
  const grouped = {};
  COLUMNS.forEach(c => grouped[c.key] = []);
  tasks.forEach(t => {
    if (grouped[t.status]) grouped[t.status].push(t);
  });

  const board = $("#board");
  board.innerHTML = "";

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
    } else {
      items.forEach(task => div.appendChild(renderCard(task, col.key)));
    }

    board.appendChild(div);
  });
}

function renderCard(task, status) {
  const card = document.createElement("div");
  card.className = "task-card";

  const shortId = task.id.length > 8 ? task.id.slice(0, 8) : task.id;

  let html = `<span class="state-badge badge-${status}">${status}</span>`;

  if (task.source) {
    html += `<span class="source-badge source-badge-${escapeHtml(task.source)}">${escapeHtml(task.source)}</span>`;
  }

  html += `<div class="task-id" title="${task.id}">${shortId}</div>`;

  if (task.turn > 0) {
    html += `<div class="task-turn">Turn ${task.turn}</div>`;
  }

  if (task.pr_url) {
    const match = task.pr_url.match(/\/pull\/(\d+)/);
    const label = match ? `PR #${match[1]}` : "PR";
    html += `<div class="task-pr"><a href="${escapeHtml(task.pr_url)}" target="_blank">${label}</a></div>`;
  }

  if (task.error) {
    html += `<div class="task-error" title="${escapeHtml(task.error)}">${escapeHtml(task.error)}</div>`;
  }

  card.innerHTML = html;
  return card;
}

function escapeHtml(str) {
  const d = document.createElement("div");
  d.textContent = str;
  return d.innerHTML;
}

function connectWebSocket() {
  const proto = location.protocol === "https:" ? "wss:" : "ws:";
  const url = `${proto}//${location.host}/ws`;

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
        headers: { "Content-Type": "application/json" },
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

function init() {
  fetchTasks();
  fetchIntake();
  connectWebSocket();
  initForm();
  pollTimer = setInterval(fetchTasks, POLL_INTERVAL_MS);
  setInterval(fetchIntake, POLL_INTERVAL_MS);
}

document.addEventListener("DOMContentLoaded", init);
