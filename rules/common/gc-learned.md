---
paths: "**/*.rs"
---

# GC Learned Rules — Staging Area

Rules extracted by the GC Learn pipeline land here first.
After review, they should be merged into the appropriate domain file:

- State machine / workflow rules → `common/coding-style.md`
- Rust-specific rules → `rust/quality.md`
- Security rules → `common/security.md`
- Patterns that need automated detection → `.harness/guards/` as shell scripts

Once merged, remove the entry from this file. Originals are archived in `docs/gc-learned-<date>.md`.

---

<!-- No pending rules. Next GC Learn output will be appended here. -->
