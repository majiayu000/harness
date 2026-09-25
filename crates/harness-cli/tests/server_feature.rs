//! Guard: harness-server stays optional behind the default `server` feature.

#[test]
fn harness_server_is_optional_behind_default_server_feature() {
    let cargo = include_str!(concat!(env!("CARGO_MANIFEST_DIR"), "/Cargo.toml"));
    assert!(
        cargo.contains("harness-server = { workspace = true, optional = true }"),
        "harness-server must remain an optional dependency"
    );
    assert!(
        cargo.contains("default = [\"server\"]"),
        "server must stay on by default so cargo install / harness serve keep working"
    );
    assert!(
        cargo.contains("server = [\"dep:harness-server\"]"),
        "the server feature must be the only way to link harness-server"
    );
}
