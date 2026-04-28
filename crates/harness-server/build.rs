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
    let dist = web_dir.join("dist");
    let index_html = dist.join("index.html");
    let out_dir = std::path::PathBuf::from(std::env::var_os("OUT_DIR").expect("OUT_DIR"));
    let manifest = out_dir.join("assets_manifest.rs");

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
        if let Some(bun) = find_bun() {
            let bun_str = bun.to_str().unwrap_or("bun");
            let build_err = try_run(&[bun_str, "install", "--frozen-lockfile"], &web_dir)
                .and_then(|_| try_run(&[bun_str, "run", "build"], &web_dir))
                .err();
            if let Some(e) = build_err {
                if index_html.exists() {
                    println!(
                        "cargo:warning=bun invocation failed ({}); reusing existing web/dist bundle",
                        e
                    );
                } else if can_fall_back_to_stub_bundle() {
                    println!(
                        "cargo:warning=bun invocation failed ({}); embedding stub bundle for non-release build",
                        e
                    );
                    write_stub_manifest(&manifest);
                    return;
                } else {
                    panic!("bun build failed in release mode: {}", e);
                }
            }
        } else if index_html.exists() {
            println!(
                "cargo:warning=bun unavailable; reusing existing web/dist bundle from {}",
                dist.display()
            );
        } else if can_fall_back_to_stub_bundle() {
            println!(
                "cargo:warning=bun unavailable and web/dist is missing; embedding a stub dashboard bundle for non-release build"
            );
            write_stub_manifest(&manifest);
            return;
        } else {
            panic!(
                "bun is required to build harness-server in release mode; install bun or prebuild web/dist"
            );
        }
    }

    if !index_html.exists() {
        panic!(
            "web/dist/index.html missing at {:?}. Either run `bun run build` in web/ \
             or unset HARNESS_SKIP_WEB_BUILD.",
            index_html
        );
    }

    write_dist_manifest(&out_dir, &manifest, &dist, &index_html);
}

fn write_dist_manifest(
    out_dir: &std::path::Path,
    manifest: &std::path::Path,
    dist: &std::path::Path,
    index_html: &std::path::Path,
) {
    // Parse <script src="/assets/index.<hash>.js"> and <link href="/assets/index.<hash>.css">
    // from dist/index.html and emit a Rust manifest consumed by src/assets.rs.
    let html = std::fs::read_to_string(index_html).expect("read index.html");
    let js =
        find_asset(&html, "/assets/", ".js").expect("no .js asset referenced in dist/index.html");
    let css = find_asset(&html, "/assets/", ".css");
    let embedded_dir = out_dir.join("embedded-web");
    let embedded_assets_dir = embedded_dir.join("assets");
    std::fs::create_dir_all(&embedded_assets_dir).expect("create embedded asset dir");

    let embedded_index_html = embedded_dir.join("index.html");
    std::fs::copy(index_html, &embedded_index_html).expect("copy embedded index.html");

    let embedded_js = embedded_assets_dir.join(&js);
    std::fs::copy(dist.join("assets").join(&js), &embedded_js).expect("copy embedded js");

    let mut body = String::new();
    body.push_str(&format!("pub const ASSET_JS_NAME: &str = \"{}\";\n", js));
    body.push_str(&format!(
        "pub const ASSET_JS: &[u8] = include_bytes!(\"{}\");\n",
        rust_lit(&embedded_js)
    ));
    if let Some(css) = css {
        let embedded_css = embedded_assets_dir.join(&css);
        std::fs::copy(dist.join("assets").join(&css), &embedded_css).expect("copy embedded css");
        body.push_str(&format!("pub const ASSET_CSS_NAME: &str = \"{}\";\n", css));
        body.push_str(&format!(
            "pub const ASSET_CSS: &[u8] = include_bytes!(\"{}\");\n",
            rust_lit(&embedded_css)
        ));
    } else {
        body.push_str("pub const ASSET_CSS_NAME: &str = \"\";\n");
        body.push_str("pub const ASSET_CSS: &[u8] = b\"\";\n");
    }
    body.push_str(&format!(
        "pub const INDEX_HTML: &str = include_str!(\"{}\");\n",
        rust_lit(&embedded_index_html)
    ));

    std::fs::write(manifest, body).expect("write assets_manifest.rs");
}

fn write_stub_manifest(manifest: &std::path::Path) {
    let body = concat!(
        "pub const ASSET_JS_NAME: &str = \"index.stub.js\";\n",
        "pub const ASSET_JS: &[u8] = b\"console.warn('Harness UI stub bundle loaded; install bun and rebuild to restore the full dashboard.');\";\n",
        "pub const ASSET_CSS_NAME: &str = \"\";\n",
        "pub const ASSET_CSS: &[u8] = b\"\";\n",
        "pub const INDEX_HTML: &str = r#\"<!doctype html><html lang=\\\"en\\\"><head><meta charset=\\\"utf-8\\\"><meta name=\\\"viewport\\\" content=\\\"width=device-width, initial-scale=1\\\"><title>Harness</title><style>body{margin:0;background:#111827;color:#f9fafb;font:16px/1.6 ui-monospace,SFMono-Regular,Menlo,monospace}main{max-width:48rem;margin:0 auto;padding:3rem 1.5rem}h1{font-size:1.5rem;margin:0 0 1rem}p{margin:0 0 1rem}</style></head><body><main><h1>Harness UI unavailable</h1><p>This non-release build was compiled without bun, so the generated web bundle was replaced with a stub page.</p><p>Install bun and rebuild to restore the full dashboard UI.</p></main><script src=\\\"/assets/index.stub.js\\\"></script></body></html>\"#;\n"
    );
    std::fs::write(manifest, body).expect("write stub assets_manifest.rs");
}

/// Render a filesystem path as a Rust string literal safe to embed inside
/// `include_bytes!`/`include_str!`. On Windows `Path::display()` emits
/// backslashes which Rust interprets as escape sequences; normalize to
/// forward slashes so the generated manifest compiles on every platform.
fn rust_lit(path: &std::path::Path) -> String {
    path.display().to_string().replace('\\', "/")
}

/// Return the path to the `bun` binary. Cargo build scripts may run with a
/// restricted PATH that omits shell-profile additions (e.g. `~/.bun/bin`),
/// so fall back to the default install location before giving up.
fn find_bun() -> Option<PathBuf> {
    if let Some(bun) = std::env::var_os("BUN") {
        let candidate = PathBuf::from(bun);
        if command_works(&candidate) {
            return Some(candidate);
        }
    }

    for candidate in [
        PathBuf::from("bun"),
        PathBuf::from("/opt/homebrew/bin/bun"),
        PathBuf::from("/usr/local/bin/bun"),
    ] {
        if command_works(&candidate) {
            return Some(candidate);
        }
    }

    if let Some(home) = std::env::var_os("HOME") {
        let candidate = PathBuf::from(home).join(".bun/bin/bun");
        if command_works(&candidate) {
            return Some(candidate);
        }
    }

    None
}

fn command_works(cmd: &std::path::Path) -> bool {
    Command::new(cmd)
        .arg("--version")
        .status()
        .map(|status| status.success())
        .unwrap_or(false)
}

fn can_fall_back_to_stub_bundle() -> bool {
    std::env::var("PROFILE").ok().as_deref() != Some("release")
}

fn try_run(cmd: &[&str], dir: &std::path::Path) -> Result<(), String> {
    let status = Command::new(cmd[0])
        .args(&cmd[1..])
        .current_dir(dir)
        .status()
        .map_err(|e| format!("failed to invoke `{}` — install bun? ({})", cmd[0], e))?;
    if !status.success() {
        return Err(format!("`{}` exited with {}", cmd.join(" "), status));
    }
    Ok(())
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
