//! Embedded React bundle served at `/assets/:filename`.
//!
//! Filenames and bytes come from `OUT_DIR/assets_manifest.rs`, which is
//! written by `build.rs` after `bun run build` produces `web/dist/`.

use axum::{
    extract::Path,
    http::{header, StatusCode},
    response::{IntoResponse, Response},
};

include!(concat!(env!("OUT_DIR"), "/assets_manifest.rs"));

/// Serve one of the hashed assets produced by Vite. Returns 404 for any other
/// filename. `Cache-Control` is long-lived + immutable because filenames carry
/// a content hash.
pub async fn serve(Path(filename): Path<String>) -> Response {
    if filename == ASSET_JS_NAME {
        return respond(ASSET_JS, "application/javascript; charset=utf-8");
    }
    if !ASSET_CSS_NAME.is_empty() && filename == ASSET_CSS_NAME {
        return respond(ASSET_CSS, "text/css; charset=utf-8");
    }
    (StatusCode::NOT_FOUND, "asset not found").into_response()
}

fn respond(bytes: &'static [u8], content_type: &'static str) -> Response {
    (
        [
            (header::CONTENT_TYPE, content_type),
            (header::CACHE_CONTROL, "public, max-age=31536000, immutable"),
        ],
        bytes,
    )
        .into_response()
}

/// The built `index.html` inlined at compile time.
pub fn index_html() -> &'static str {
    INDEX_HTML
}
