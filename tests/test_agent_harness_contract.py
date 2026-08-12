from __future__ import annotations

from ci_contract_support import TRUSTED_ROOT, contract_candidate_file


PR_CHECK_WORKFLOW = contract_candidate_file(".github/workflows/pr-check.yml")


def test_shared_agent_rules_have_one_canonical_source() -> None:
    agents = (TRUSTED_ROOT / "AGENTS.md").read_text(encoding="utf-8")
    claude = (TRUSTED_ROOT / "CLAUDE.md").read_text(encoding="utf-8")

    assert "Read and follow `AGENTS.md`" in claude
    assert "canonical source for shared project rules" in claude
    for rule in (
        "Merging requires explicit operator approval",
        "Do not change `Cargo.toml` versions",
        "External review bots are optional advisors",
    ):
        assert rule in agents


def test_enabled_gemini_review_runs_for_new_and_updated_heads() -> None:
    workflow = PR_CHECK_WORKFLOW.read_text(encoding="utf-8")

    assert "GEMINI_REVIEW_ENABLED == 'true'" in workflow
    for action in ("opened", "synchronize", "reopened", "ready_for_review"):
        assert f'"{action}"' in workflow
    assert "body: '/gemini review'" in workflow
