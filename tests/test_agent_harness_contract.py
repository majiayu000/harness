from __future__ import annotations

from ci_contract_support import TRUSTED_ROOT


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
