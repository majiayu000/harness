use std::path::PathBuf;
use std::process::Command;

fn main() {
    let workspace_root: PathBuf = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .and_then(|p| p.parent())
        .expect("workspace root")
        .to_path_buf();
    let web_dir: PathBuf = workspace_root.join("web");
    let sdk_dir: PathBuf = workspace_root.join("sdk").join("typescript");

    // Trigger rerun whenever web source changes.
    for rel in [
        "package.json",
        "bun.lock",
        "tsconfig.json",
        "vite.config.ts",
        "tailwind.config.ts",
        "postcss.config.js",
        "index.html",
        "src",
    ] {
        println!("cargo:rerun-if-changed={}/{}", web_dir.display(), rel);
    }

    // The web bundle depends on sdk/typescript (web/package.json uses
    // file:../sdk/typescript and web/package.json "prebuild" rebuilds the
    // SDK). Without these triggers, editing only SDK source could leave a
    // stale embedded bundle until an unrelated web/ edit forces rerun.
    for rel in ["package.json", "tsconfig.json", "src"] {
        println!("cargo:rerun-if-changed={}/{}", sdk_dir.display(), rel);
    }

    // Skip the build when `HARNESS_SKIP_WEB_BUILD=1` so Rust-only hacking
    // during development doesn't pay the bundle cost. The check at the bottom
    // of this file still fails if dist/ is missing and the skip flag is off.
    if std::env::var("HARNESS_SKIP_WEB_BUILD").ok().as_deref() != Some("1") {
        run(&["bun", "install", "--frozen-lockfile"], &web_dir);
        run(&["bun", "run", "build"], &web_dir);
    }

    let dist = web_dir.join("dist");
    let index_html = dist.join("index.html");
    if !index_html.exists() {
        panic!(
            "web/dist/index.html missing at {:?}. Either run `bun run build` in web/ \
             or unset HARNESS_SKIP_WEB_BUILD.",
            index_html
        );
    }

    // Parse <script src="/assets/index.<hash>.js"> and <link href="/assets/index.<hash>.css">
    // from dist/index.html and emit a Rust manifest consumed by src/assets.rs.
    let html = std::fs::read_to_string(&index_html).expect("read index.html");
    let js =
        find_asset(&html, "/assets/", ".js").expect("no .js asset referenced in dist/index.html");
    let css = find_asset(&html, "/assets/", ".css");

    let out_dir = std::path::PathBuf::from(std::env::var_os("OUT_DIR").expect("OUT_DIR"));
    let manifest = out_dir.join("assets_manifest.rs");

    let mut body = String::new();
    body.push_str(&format!("pub const ASSET_JS_NAME: &str = \"{}\";\n", js));
    body.push_str(&format!(
        "pub const ASSET_JS: &[u8] = include_bytes!(\"{}\");\n",
        rust_lit(&dist.join("assets").join(&js))
    ));
    if let Some(css) = css {
        body.push_str(&format!("pub const ASSET_CSS_NAME: &str = \"{}\";\n", css));
        body.push_str(&format!(
            "pub const ASSET_CSS: &[u8] = include_bytes!(\"{}\");\n",
            rust_lit(&dist.join("assets").join(&css))
        ));
    } else {
        body.push_str("pub const ASSET_CSS_NAME: &str = \"\";\n");
        body.push_str("pub const ASSET_CSS: &[u8] = b\"\";\n");
    }
    body.push_str(&format!(
        "pub const INDEX_HTML: &str = include_str!(\"{}\");\n",
        rust_lit(&index_html)
    ));

    std::fs::write(&manifest, body).expect("write assets_manifest.rs");
}

/// Render a filesystem path as a Rust string literal safe to embed inside
/// `include_bytes!`/`include_str!`. On Windows `Path::display()` emits
/// backslashes which Rust interprets as escape sequences; normalize to
/// forward slashes so the generated manifest compiles on every platform.
fn rust_lit(path: &std::path::Path) -> String {
    path.display().to_string().replace('\\', "/")
}

fn run(cmd: &[&str], dir: &std::path::Path) {
    let status = Command::new(cmd[0])
        .args(&cmd[1..])
        .current_dir(dir)
        .status()
        .unwrap_or_else(|e| panic!("failed to invoke `{}` — install bun? ({})", cmd[0], e));
    if !status.success() {
        panic!("`{}` exited with {}", cmd.join(" "), status);
    }
}

/// Extract the first asset filename from `dist/index.html` whose href/src
/// matches the given prefix and ends with the given suffix. Scans all
/// occurrences of `prefix` and returns the filename (between prefix and the
/// next `"`) when that filename ends with `suffix`.
fn find_asset(html: &str, prefix: &str, suffix: &str) -> Option<String> {
    let mut cursor = 0;
    while let Some(rel) = html[cursor..].find(prefix) {
        let start = cursor + rel + prefix.len();
        let rest = &html[start..];
        let end = rest.find('"')?;
        let name = &rest[..end];
        if name.ends_with(suffix) {
            return Some(name.to_string());
        }
        cursor = start + end;
    }
    None
}
