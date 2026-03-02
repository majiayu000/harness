---
paths: "**/*.rs"
---

# Rust Quality Rules

## RS-01: No `.unwrap()` outside tests (high)
Use `?` operator with proper error types. `unwrap()` only in `#[cfg(test)]`.

## RS-02: No `.clone()` to satisfy borrow checker (medium)
Restructure code to use references. Clone only when ownership transfer is needed.

## RS-03: No `Box<dyn Error>` in libraries (medium)
Use concrete error types with `thiserror`. `anyhow` for applications only.

## RS-04: No `unsafe` without justification (critical)
Safe abstractions first. Document safety invariants.

## RS-05: No blocking in async context (high)
Use `spawn_blocking` for CPU-bound work.

## RS-06: No `Arc<Mutex<_>>` overuse (medium)
Prefer message passing and channels.

## RS-07: No manual `Drop` without RAII (medium)
Use struct destructors for cleanup.

## RS-08: No `String` everywhere (medium)
Use `&str`, `Cow<str>`, or domain-specific newtypes.
