use axum::http::header;
use axum::response::{Html, IntoResponse};

pub async fn index() -> impl IntoResponse {
    Html(crate::assets::index_html())
}

pub async fn favicon() -> impl IntoResponse {
    (
        [(header::CONTENT_TYPE, "image/svg+xml")],
        r#"<svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 100 100"><text y=".9em" font-size="90">&#9881;</text></svg>"#,
    )
}
