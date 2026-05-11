//! Static registry of builtin skill markdown files.
//!
//! Skills listed here are bundled into the binary via `include_str!` and loaded
//! by `SkillStore::load_builtin`.  Adding a new builtin skill is a one-line
//! change in [`BUILTIN_SKILLS`] plus a markdown file under `skills/`.

/// Builtin skill registry: `(name, content)` pairs.
///
/// `name` is the on-disk filename without the `.md` suffix and is also used as
/// the skill's stable identifier.  `content` is the entire markdown body.
pub(crate) const BUILTIN_SKILLS: &[(&str, &str)] = &[
    ("interview", include_str!("../../../skills/interview.md")),
    ("exec-plan", include_str!("../../../skills/exec-plan.md")),
    ("preflight", include_str!("../../../skills/preflight.md")),
    ("check", include_str!("../../../skills/check.md")),
    ("build-fix", include_str!("../../../skills/build-fix.md")),
    ("review", include_str!("../../../skills/review.md")),
    (
        "cross-review",
        include_str!("../../../skills/cross-review.md"),
    ),
    ("learn", include_str!("../../../skills/learn.md")),
    ("gc", include_str!("../../../skills/gc.md")),
    ("stats", include_str!("../../../skills/stats.md")),
    ("rebase-pr", include_str!("../../../skills/rebase-pr.md")),
];
