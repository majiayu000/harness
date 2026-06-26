from __future__ import annotations

import json
import subprocess
import sys
from pathlib import Path


ROOT = Path(__file__).resolve().parents[1]
CHECKS = ROOT / "checks"
sys.path.insert(0, str(ROOT))
sys.path.insert(0, str(CHECKS))

from check_workflow import validate_spec_packet, validate_task_plan  # noqa: E402
from evaluate import evaluate_adoption_matrix, evaluate_rclean_smoke, evaluate_spec  # noqa: E402
from specrail_lib import validate_json_schemas  # noqa: E402


def write_text(path: Path, text: str) -> None:
    path.parent.mkdir(parents=True, exist_ok=True)
    path.write_text(text, encoding="utf-8")


def test_task_plan_rejects_duplicate_ids(tmp_path: Path) -> None:
    task_path = tmp_path / "specs" / "GH5" / "tasks.md"
    write_text(
        task_path,
        "\n".join(
            [
                "- [ ] `SP5-T001` Owner: `docs` | Done when: done | Verify: review",
                "- [ ] `SP5-T001` Owner: `tests` | Done when: done | Verify: pytest",
            ]
        ),
    )

    errors = validate_task_plan(task_path, "5")

    assert any("duplicate task ID SP5-T001" in error for error in errors)


def test_task_plan_requires_done_when_and_verify(tmp_path: Path) -> None:
    task_path = tmp_path / "specs" / "GH5" / "tasks.md"
    write_text(
        task_path,
        "- [ ] `SP5-T001` Owner: `tests` | Details: malformed task",
    )

    errors = validate_task_plan(task_path, "5")

    assert any("missing Done when:" in error for error in errors)
    assert any("missing Verify:" in error for error in errors)


def test_spec_packet_allows_missing_tasks_md(tmp_path: Path) -> None:
    spec_dir = tmp_path / "specs" / "GH5"
    write_text(spec_dir / "product.md", "GitHub issue: `#5`\n")
    write_text(spec_dir / "tech.md", "GitHub issue: `#5`\n")

    errors = validate_spec_packet(spec_dir)

    assert errors == []


def test_spec_packet_validates_tasks_md_when_present(tmp_path: Path) -> None:
    spec_dir = tmp_path / "specs" / "GH5"
    write_text(spec_dir / "product.md", "GitHub issue: `#5`\n")
    write_text(spec_dir / "tech.md", "GitHub issue: `#5`\n")
    write_text(
        spec_dir / "tasks.md",
        "- [ ] `SP5-T001` Owner: `tests` | Details: malformed task",
    )

    errors = validate_spec_packet(spec_dir)

    assert any("missing Done when:" in error for error in errors)
    assert any("missing Verify:" in error for error in errors)


def test_evaluate_spec_allows_missing_tasks_md(tmp_path: Path) -> None:
    spec_dir = tmp_path / "specs" / "GH5"
    write_text(spec_dir / "product.md", "GitHub issue: `#5`\n")
    write_text(spec_dir / "tech.md", "GitHub issue: `#5`\n")

    checks, errors = evaluate_spec(tmp_path, spec_dir)

    assert errors == []
    assert any(item["id"] == "spec.tasks_optional" and item["status"] == "pass" for item in checks)
    assert not any(item["id"] == "spec.tasks_present" and item["status"] == "fail" for item in checks)


def test_rclean_smoke_requires_all_scenarios(tmp_path: Path) -> None:
    smoke = tmp_path / "examples" / "rclean-smoke.md"
    write_text(
        smoke,
        "\n".join(
            [
                "Scope: read-only. Do not modify `/Users/lifcc/Desktop/code/AI/tool/rclean`.",
                "cargo fmt -- --check",
                "cargo clippy --all-targets --all-features -- -D warnings",
                "cargo test",
                "cargo build --release",
                "rustup run 1.95 cargo build",
                "rustup run 1.95 cargo test",
                "rclean.new_rule_spec_first",
                "rclean.security_boundary_gate",
                "rclean.doc_only_direct",
                "rclean.ci_command_mapping",
                "drafts/rclean-issues-draft-2026-05-25.md",
                "NOT SUBMITTED YET",
            ]
        ),
    )

    checks, errors, warnings = evaluate_rclean_smoke(tmp_path)

    assert any(check["id"] == "rclean_smoke.scenarios_present" for check in checks)
    assert any("rclean.issue_dedupe" in error for error in errors)
    assert any("human review" in warning for warning in warnings)


def test_evaluate_json_contract_for_gh5() -> None:
    result = subprocess.run(
        [
            sys.executable,
            "evaluate.py",
            "--repo",
            ".",
            "--spec-dir",
            "specs/GH5",
            "--format",
            "json",
        ],
        cwd=ROOT,
        check=False,
        capture_output=True,
        text=True,
    )

    assert result.returncode == 0
    payload = json.loads(result.stdout)
    assert {
        "status",
        "repo",
        "spec_dir",
        "checks",
        "artifacts",
        "errors",
        "warnings",
        "next_actions",
    } <= set(payload)
    assert payload["status"] == "needs_human"
    assert payload["errors"] == []
    assert payload["artifacts"]["adoption_matrix"] == "docs/ADOPTION_MATRIX.md"
    assert payload["artifacts"]["adoption_fixture"] == "examples/adoptions/matrix.json"


def test_adoption_matrix_requires_known_pilots(tmp_path: Path) -> None:
    write_text(tmp_path / "docs" / "ADOPTION_MATRIX.md", "rclean\n")
    write_text(
        tmp_path / "examples" / "adoptions" / "matrix.json",
        json.dumps(
            {
                "schema_version": "1.0",
                "levels": ["smoke"],
                "adoptions": [
                    {
                        "id": "rclean",
                        "name": "rclean",
                        "repo": "majiayu000/rclean",
                        "current_level": "smoke",
                        "status": "active",
                        "evidence": [{"kind": "specrail_artifact", "path": "examples/rclean-smoke.md"}],
                        "verified_behaviors": ["smoke"],
                        "next_gap": "add more evidence",
                    }
                ],
            }
        ),
    )
    write_text(tmp_path / "examples" / "rclean-smoke.md", "smoke\n")

    checks, errors, warnings = evaluate_adoption_matrix(tmp_path)

    assert any(check["id"] == "adoption_matrix.required_ids" for check in checks)
    assert any("litellm-rs" in error for error in errors)
    assert warnings == []


def test_adoption_matrix_rejects_non_object_fixture_root(tmp_path: Path) -> None:
    write_text(tmp_path / "docs" / "ADOPTION_MATRIX.md", "matrix\n")
    write_text(tmp_path / "examples" / "adoptions" / "matrix.json", "[]")

    checks, errors, warnings = evaluate_adoption_matrix(tmp_path)

    assert any(check["id"] == "adoption_matrix.fixture_json" for check in checks)
    assert any("JSON root must be an object" in error for error in errors)
    assert warnings == []


def test_json_schema_validator_rejects_non_object_root(tmp_path: Path) -> None:
    write_text(tmp_path / "schemas" / "bad.schema.json", "[]")

    errors = validate_json_schemas(tmp_path)

    assert errors == ["schemas/bad.schema.json: JSON root must be an object"]


def test_adoption_matrix_needs_human_status_affects_checks(tmp_path: Path) -> None:
    write_text(tmp_path / "docs" / "ADOPTION_MATRIX.md", "rclean\n")
    write_text(tmp_path / "examples" / "rclean-smoke.md", "smoke\n")
    write_text(
        tmp_path / "examples" / "adoptions" / "matrix.json",
        json.dumps(
            {
                "schema_version": "1.0",
                "levels": ["smoke", "pr_gate", "spec_packet"],
                "adoptions": [
                    {
                        "id": "rclean",
                        "name": "rclean",
                        "repo": "majiayu000/rclean",
                        "current_level": "smoke",
                        "status": "needs_human",
                        "evidence": [{"kind": "specrail_artifact", "path": "examples/rclean-smoke.md"}],
                        "verified_behaviors": ["smoke"],
                        "next_gap": "review draft issues",
                    },
                    {
                        "id": "litellm-rs",
                        "name": "litellm-rs",
                        "repo": "majiayu000/litellm-rs",
                        "current_level": "pr_gate",
                        "status": "active",
                        "evidence": [{"kind": "github_pr", "repo": "majiayu000/litellm-rs", "number": 718, "url": "https://example.test/pr"}],
                        "verified_behaviors": ["pr gate"],
                        "next_gap": "add fixtures",
                    },
                    {
                        "id": "claude-code-monitor",
                        "name": "Claude-Code-Monitor",
                        "repo": "majiayu000/claude-hub",
                        "current_level": "spec_packet",
                        "status": "active",
                        "evidence": [{"kind": "external_artifact", "repo": "majiayu000/claude-hub", "path": "specs/GH44/product.md"}],
                        "verified_behaviors": ["spec packet"],
                        "next_gap": "decide integration",
                    },
                ],
            }
        ),
    )

    checks, errors, warnings = evaluate_adoption_matrix(tmp_path)

    assert not errors
    assert any(check["id"] == "adoption_matrix.status_needs_human" for check in checks)
    assert any("rclean adoption still needs human review" in warning for warning in warnings)


def test_adoption_matrix_rejects_unsafe_specrail_artifact_paths(tmp_path: Path) -> None:
    write_text(tmp_path / "docs" / "ADOPTION_MATRIX.md", "rclean\n")
    write_text(
        tmp_path / "examples" / "adoptions" / "matrix.json",
        json.dumps(
            {
                "schema_version": "1.0",
                "levels": ["smoke"],
                "adoptions": [
                    {
                        "id": "rclean",
                        "name": "rclean",
                        "repo": "majiayu000/rclean",
                        "current_level": "smoke",
                        "status": "active",
                        "evidence": [{"kind": "specrail_artifact", "path": "../outside.md"}],
                        "verified_behaviors": ["smoke"],
                        "next_gap": "add more evidence",
                    }
                ],
            }
        ),
    )

    checks, errors, _warnings = evaluate_adoption_matrix(tmp_path)

    assert any(check["id"] == "adoption_matrix.local_evidence" for check in checks)
    assert any("must not contain '..'" in error for error in errors)
