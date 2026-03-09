use axum::http::header;
use axum::response::{Html, IntoResponse};

const CSS: &str = include_str!("../static/dashboard.css");
const JS: &str = include_str!("../static/dashboard.js");

pub async fn index() -> impl IntoResponse {
    Html(format!(
        r#"<!DOCTYPE html>
<html lang="en">
<head>
<meta charset="utf-8">
<meta name="viewport" content="width=device-width, initial-scale=1">
<title>Harness Dashboard</title>
<style>{CSS}</style>
</head>
<body>
<main class="app-shell">
<section class="dashboard-shell">

<header class="hero-card">
  <div class="hero-grid">
    <div>
      <p class="eyebrow">Harness Orchestration</p>
      <h1 class="hero-title">Operations Dashboard</h1>
      <p class="hero-copy">Task pipeline status, agent activity, and review progress.</p>
    </div>
    <div>
      <span id="conn-badge" class="status-badge">Offline</span>
    </div>
  </div>
</header>

<section class="metric-grid">
  <article class="metric-card">
    <p class="metric-label">Total</p>
    <p class="metric-value" id="metric-total">0</p>
  </article>
  <article class="metric-card">
    <p class="metric-label">Running</p>
    <p class="metric-value" id="metric-running">0</p>
  </article>
  <article class="metric-card">
    <p class="metric-label">Done</p>
    <p class="metric-value" id="metric-done">0</p>
  </article>
  <article class="metric-card">
    <p class="metric-label">Failed</p>
    <p class="metric-value" id="metric-failed">0</p>
  </article>
</section>

<section class="section-card">
  <h2 class="section-title">Submit Task</h2>
  <form id="task-form" class="task-form" novalidate>
    <div class="form-group">
      <label class="form-label" for="f-title">Title</label>
      <input id="f-title" class="form-input" type="text" name="title" placeholder="Brief title" required />
    </div>
    <div class="form-group">
      <label class="form-label" for="f-description">Description</label>
      <textarea id="f-description" class="form-input form-textarea" name="description" placeholder="Describe the task in detail..." rows="4" required></textarea>
    </div>
    <div class="form-row">
      <div class="form-group form-group-inline">
        <label class="form-label" for="f-priority">Priority</label>
        <select id="f-priority" class="form-input form-select" name="priority">
          <option value="">Normal</option>
          <option value="1">Urgent</option>
          <option value="2">High</option>
          <option value="3">Medium</option>
        </select>
      </div>
      <button id="task-submit-btn" class="btn-primary" type="submit">Submit Task</button>
    </div>
    <div id="task-form-feedback" class="form-feedback" aria-live="polite"></div>
  </form>
</section>

<section class="section-card">
  <h2 class="section-title">Task Board</h2>
  <div id="board" class="board"></div>
</section>

</section>
</main>
<script>{JS}</script>
</body>
</html>"#
    ))
}

pub async fn favicon() -> impl IntoResponse {
    (
        [(header::CONTENT_TYPE, "image/svg+xml")],
        r#"<svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 100 100"><text y=".9em" font-size="90">&#9881;</text></svg>"#,
    )
}
