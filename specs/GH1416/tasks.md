# GH1416 Tasks

Issue: `#1416`
Product spec: `specs/GH1416/product.md`
Tech spec: `specs/GH1416/tech.md`

## Status

- [ ] `SP1416-T001` Owner: `config` | Done when: `allow_unauthenticated` (default false) exists in server HTTP config and empty/whitespace `api_token` values normalize to none at load | Verify: `cargo test --package harness-server config`
- [ ] `SP1416-T002` Owner: `http-auth` | Done when: startup resolves an explicit auth mode — enforced with token, open with opt-in plus a prominent warning, and refusal to start with an actionable error naming `api_token`, `HARNESS_API_TOKEN`, and `allow_unauthenticated` when neither is set | Verify: `cargo test --package harness-server http::auth`
- [ ] `SP1416-T003` Owner: `http-auth` | Done when: `api_auth_middleware` branches on the resolved auth mode, keeps the constant-time compare and exempt-path behavior unchanged, and warns when the opt-in is ignored because a token is set | Verify: `cargo test --package harness-server http::auth`
- [ ] `SP1416-T004` Owner: `tests` | Done when: the config matrix (token / empty token / opt-in / both) and all three startup configurations are covered by unit and integration tests | Verify: `cargo test --package harness-server`
- [ ] `SP1416-T005` Owner: `docs` | Done when: README and usage docs describe the fail-closed default, the opt-in, and the release-note upgrade callout with both recovery paths | Verify: `test -s README.md`

## Parallelization

Single lane — the change concentrates in `http/auth.rs`, config, and the `http/mod.rs` startup path; splitting would create shared-file conflicts.

## Verification

- `cargo test --package harness-server http::auth`
- `RUSTFLAGS="-Dwarnings" cargo check --workspace --all-targets`
- Manual: run `harness serve` under all three configurations and capture the startup output in the implementation PR body.

## Handoff Notes

- Do not touch webhook/signals HMAC paths or the exempt route list.
- The startup error text is part of the acceptance criteria; treat its wording as contract.
