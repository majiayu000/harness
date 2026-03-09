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
