//! GET /overview — system-level overview HTML page.
//!
//! Mirrors the inline-CSS/JS pattern used by [`crate::dashboard`]. The static
//! asset in `static/overview.html` is a self-contained prototype produced by
//! Claude Design; it fetches live data from `/api/overview` on load and
//! re-renders every [`OVERVIEW_REFRESH_MS`] ms.
use axum::response::{Html, IntoResponse};

const PAGE: &str = include_str!("../static/overview.html");

pub async fn index() -> impl IntoResponse {
    Html(PAGE)
}
