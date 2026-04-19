use axum::response::{Html, IntoResponse};

pub async fn index() -> impl IntoResponse {
    Html(crate::assets::index_html())
}
