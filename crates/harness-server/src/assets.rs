//! Embedded React bundle served at `/assets/:filename`.
//!
//! Filenames and bytes come from `OUT_DIR/assets_manifest.rs`, which is
//! written by `build.rs` after `bun run build` produces `web/dist/`.

use crate::http::rest_contract::PrimitivePath as Path;
use axum::{
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

/// Serve the Console v2 design assets from a fixed allowlist. These filenames
/// are stable, so browsers must revalidate them after a server update.
pub async fn serve_console(Path(filename): Path<String>) -> Response {
    let (bytes, content_type): (&'static [u8], &'static str) = match filename.as_str() {
        "index.html" => (
            include_bytes!(concat!(
                env!("CARGO_MANIFEST_DIR"),
                "/../../web/public/console-v2/index.html"
            )),
            "text/html; charset=utf-8",
        ),
        "support.js" => (
            include_bytes!(concat!(
                env!("CARGO_MANIFEST_DIR"),
                "/../../web/public/console-v2/support.js"
            )),
            "application/javascript; charset=utf-8",
        ),
        "harness-console-data.js" => (
            include_bytes!(concat!(
                env!("CARGO_MANIFEST_DIR"),
                "/../../web/public/console-v2/harness-console-data.js"
            )),
            "application/javascript; charset=utf-8",
        ),
        "harness-console-data-ext.js" => (
            include_bytes!(concat!(
                env!("CARGO_MANIFEST_DIR"),
                "/../../web/public/console-v2/harness-console-data-ext.js"
            )),
            "application/javascript; charset=utf-8",
        ),
        "live-data.js" => (
            include_bytes!(concat!(
                env!("CARGO_MANIFEST_DIR"),
                "/../../web/public/console-v2/live-data.js"
            )),
            "application/javascript; charset=utf-8",
        ),
        "react.production.min.js" => (
            include_bytes!(concat!(
                env!("CARGO_MANIFEST_DIR"),
                "/../../web/public/console-v2/react.production.min.js"
            )),
            "application/javascript; charset=utf-8",
        ),
        "react-dom.production.min.js" => (
            include_bytes!(concat!(
                env!("CARGO_MANIFEST_DIR"),
                "/../../web/public/console-v2/react-dom.production.min.js"
            )),
            "application/javascript; charset=utf-8",
        ),
        "geist-400.ttf" => (
            include_bytes!(concat!(
                env!("CARGO_MANIFEST_DIR"),
                "/../../web/public/console-v2/geist-400.ttf"
            )),
            "font/ttf",
        ),
        "geist-500.ttf" => (
            include_bytes!(concat!(
                env!("CARGO_MANIFEST_DIR"),
                "/../../web/public/console-v2/geist-500.ttf"
            )),
            "font/ttf",
        ),
        "geist-600.ttf" => (
            include_bytes!(concat!(
                env!("CARGO_MANIFEST_DIR"),
                "/../../web/public/console-v2/geist-600.ttf"
            )),
            "font/ttf",
        ),
        "geist-700.ttf" => (
            include_bytes!(concat!(
                env!("CARGO_MANIFEST_DIR"),
                "/../../web/public/console-v2/geist-700.ttf"
            )),
            "font/ttf",
        ),
        "geist-mono-400.ttf" => (
            include_bytes!(concat!(
                env!("CARGO_MANIFEST_DIR"),
                "/../../web/public/console-v2/geist-mono-400.ttf"
            )),
            "font/ttf",
        ),
        "geist-mono-500.ttf" => (
            include_bytes!(concat!(
                env!("CARGO_MANIFEST_DIR"),
                "/../../web/public/console-v2/geist-mono-500.ttf"
            )),
            "font/ttf",
        ),
        "REACT-LICENSE.txt" => (
            include_bytes!(concat!(
                env!("CARGO_MANIFEST_DIR"),
                "/../../web/public/console-v2/REACT-LICENSE.txt"
            )),
            "text/plain; charset=utf-8",
        ),
        "GEIST-LICENSE.txt" => (
            include_bytes!(concat!(
                env!("CARGO_MANIFEST_DIR"),
                "/../../web/public/console-v2/GEIST-LICENSE.txt"
            )),
            "text/plain; charset=utf-8",
        ),
        "GEIST-MONO-LICENSE.txt" => (
            include_bytes!(concat!(
                env!("CARGO_MANIFEST_DIR"),
                "/../../web/public/console-v2/GEIST-MONO-LICENSE.txt"
            )),
            "text/plain; charset=utf-8",
        ),
        _ => return (StatusCode::NOT_FOUND, "asset not found").into_response(),
    };
    (
        [
            (header::CONTENT_TYPE, content_type),
            (header::CACHE_CONTROL, "no-cache"),
        ],
        bytes,
    )
        .into_response()
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

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn console_v2_assets_are_embedded_and_allowlisted() {
        let html = serve_console(Path("index.html".to_string())).await;
        assert_eq!(html.status(), StatusCode::OK);
        assert_eq!(
            html.headers()[header::CONTENT_TYPE],
            "text/html; charset=utf-8"
        );

        let script = serve_console(Path("live-data.js".to_string())).await;
        assert_eq!(script.status(), StatusCode::OK);
        assert_eq!(
            script.headers()[header::CONTENT_TYPE],
            "application/javascript; charset=utf-8"
        );

        let unknown = serve_console(Path("../config.toml".to_string())).await;
        assert_eq!(unknown.status(), StatusCode::NOT_FOUND);
    }
}
