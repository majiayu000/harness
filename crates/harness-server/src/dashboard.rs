use axum::extract::State;
use axum::http::header;
use axum::response::{Html, IntoResponse};
use std::sync::Arc;

const CSS: &str = include_str!("../static/dashboard.css");
const JS: &str = include_str!("../static/dashboard.js");

pub async fn index(State(state): State<Arc<crate::http::AppState>>) -> impl IntoResponse {
    // When API auth is configured, inject the token as a JS variable so the
    // browser client can attach it to fetch requests and the WebSocket URL.
    // The dashboard HTML itself is exempt from auth (it contains no sensitive
    // data), and the individual API endpoints remain protected.
    let token_script = match crate::http::auth::resolve_api_token(&state.core.server.config.server)
    {
        Some(tok) => format!("<script>window.__HARNESS_TOKEN__={:?};</script>", tok),
        None => String::new(),
    };
    Html(format!(
        r#"<!DOCTYPE html>
<html lang="en">
<head>
<meta charset="utf-8">
<meta name="viewport" content="width=device-width, initial-scale=1">
<title>Harness Dashboard</title>
<style>{CSS}</style>
{token_script}</head>
<body>
<main class="app-shell">
<section class="dashboard-shell">

<header class="hero-card">
  <div class="hero-grid">
    <div>
      <p class="eyebrow">Harness Orchestration</p>
      <h1 class="hero-title">Operations Dashboard</h1>
    </div>
    <div class="hero-right">
      <span id="conn-badge" class="status-badge">Offline</span>
    </div>
  </div>
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
</header>

<nav class="tab-bar" id="tab-bar">
  <button class="tab-btn tab-btn-active" data-tab="board">Active</button>
  <button class="tab-btn" data-tab="history">History</button>
  <button class="tab-btn" data-tab="channels">Channels</button>
  <button class="tab-btn" data-tab="submit">Submit</button>
</nav>

<div id="tab-board" class="tab-panel tab-panel-active">
  <div id="board" class="board"></div>
</div>

<div id="tab-history" class="tab-panel">
  <div class="search-bar">
    <input id="history-search" class="form-input search-input" type="search" placeholder="Search done/failed tasks\u2026" />
    <select id="history-filter" class="form-input form-select filter-select">
      <option value="all">All</option>
      <option value="done">Done</option>
      <option value="failed">Failed</option>
    </select>
  </div>
  <div id="history-list" class="history-list"></div>
  <div id="history-pager" class="pager"></div>
</div>

<div id="tab-channels" class="tab-panel">
  <div id="pipeline-row" class="pipeline-row"></div>
  <div id="channel-grid" class="channel-grid"></div>
</div>

<div id="tab-submit" class="tab-panel">
  <div class="section-card">
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
        <button id="task-submit-btn" class="btn-primary" type="submit">Submit Task</button>
      </div>
      <div id="task-form-feedback" class="form-feedback" aria-live="polite"></div>
    </form>
  </div>
</div>

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
