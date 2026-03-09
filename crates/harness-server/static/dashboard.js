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
    updateStatus(true);
  } catch {
    updateStatus(false);
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

  ws.onopen = () => fetchTasks();

  ws.onmessage = (event) => {
    try {
      const msg = JSON.parse(event.data);
      const method = msg.method || "";
      if (method.startsWith("turn/") || method.startsWith("task/")) {
        fetchTasks();
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
    btn.textContent = "Submitting…";

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
  connectWebSocket();
  initForm();
  pollTimer = setInterval(fetchTasks, POLL_INTERVAL_MS);
}

document.addEventListener("DOMContentLoaded", init);
