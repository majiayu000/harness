from __future__ import annotations

import importlib.util
import re
from pathlib import Path
from unittest import mock

import pytest

from ci_contract_support import parse_workflow


ROOT = Path(__file__).resolve().parents[1]
WORKFLOW = ROOT / ".github" / "workflows" / "eval-nightly.yml"
PREFLIGHT = ROOT / "scripts" / "preflight-eval-nightly.py"
ELIGIBILITY = ROOT / "scripts" / "check-eval-baseline-eligibility.py"


def _load_preflight():
    spec = importlib.util.spec_from_file_location("preflight_eval_nightly", PREFLIGHT)
    assert spec is not None and spec.loader is not None
    module = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(module)
    return module


def _load_eligibility():
    spec = importlib.util.spec_from_file_location("eval_baseline_eligibility", ELIGIBILITY)
    assert spec is not None and spec.loader is not None
    module = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(module)
    return module


def test_eval_nightly_workflow_is_scheduled_isolated_and_bounded() -> None:
    workflow = parse_workflow(WORKFLOW.read_text(encoding="utf-8"))

    assert workflow["on"]["schedule"] == [{"cron": '"17 18 * * *"'}]
    assert workflow["concurrency"] == {
        "group": "eval-nightly",
        "cancel-in-progress": "false",
    }
    job = workflow["jobs"]["eval"]
    assert job["if"] == "${{ vars.HARNESS_EVAL_ENABLED == 'true' }}"
    assert job["runs-on"] == "[self-hosted, harness-eval]"
    assert job["environment"] == "eval-nightly"
    assert job["timeout-minutes"] == "360"
    assert "HARNESS_DATABASE_URL" not in job["env"]
    assert "HARNESS_API_TOKEN" not in job["env"]

    steps = job["steps"]
    commands = "\n".join(
        str(step.get("run", "")) for step in steps if isinstance(step, dict)
    )
    assert "scripts/preflight-eval-nightly.py" in commands
    assert "harness eval run" in commands
    assert "--execute" in commands
    assert "--max-total-tokens" in commands
    assert "harness eval diff" in commands
    assert "--fail-on-new-f-gate" in commands
    assert any(
        step.get("uses")
        == "actions/upload-artifact@ea165f8d65b6e75b540449e92b4886f43607fa02"
        and step.get("if") == "${{ always() }}"
        for step in steps
        if isinstance(step, dict)
    )
    refresh = workflow["jobs"]["refresh-baseline"]
    assert refresh["needs"] == "eval"
    assert "needs.eval.outputs.baseline-eligible == 'true'" in refresh["if"]
    assert refresh["permissions"] == {
        "contents": "write",
        "pull-requests": "write",
    }
    assert any(
        step.get("uses")
        == "peter-evans/create-pull-request@22a9089034f40e5a961c8808d113e2c98fb63676"
        for step in refresh["steps"]
        if isinstance(step, dict)
    )
    action_refs = [
        step["uses"]
        for configured_job in workflow["jobs"].values()
        for step in configured_job["steps"]
        if isinstance(step, dict) and "uses" in step
    ]
    assert action_refs
    assert all(re.fullmatch(r"[^@]+@[0-9a-f]{40}", ref) for ref in action_refs)
    assert "scripts/check-eval-baseline-eligibility.py" in commands
    eligibility_step = next(
        step for step in steps if step.get("name") == "Determine baseline eligibility"
    )
    eligibility_command = str(eligibility_step["run"])
    assert 'echo "eligible=false"' in eligibility_command
    assert '[ "$HARNESS_EVAL_GATE_MODE" = "enforce" ]' in eligibility_command
    assert "exit 1" in eligibility_command


def test_preflight_rejects_missing_database_or_enforced_baseline(tmp_path: Path) -> None:
    preflight = _load_preflight()
    manifest = tmp_path / "manifest.toml"
    manifest.write_text('suite = "test"\n', encoding="utf-8")
    baseline = tmp_path / "latest.json"

    with pytest.raises(preflight.PreflightError, match="HARNESS_DATABASE_URL"):
        preflight.validate_preflight(
            server_url="http://127.0.0.1:9800",
            gate_mode="report-only",
            manifest=manifest,
            baseline=baseline,
            database_url=None,
            api_token=None,
        )

    with pytest.raises(preflight.PreflightError, match="reviewed baseline"):
        preflight.validate_preflight(
            server_url="http://127.0.0.1:9800",
            gate_mode="enforce",
            manifest=manifest,
            baseline=baseline,
            database_url="postgres://example",
            api_token=None,
        )


def test_preflight_requires_online_capable_runtime_host(tmp_path: Path) -> None:
    preflight = _load_preflight()
    manifest = tmp_path / "manifest.toml"
    manifest.write_text('suite = "test"\n', encoding="utf-8")

    with mock.patch.object(
        preflight,
        "_read_json",
        side_effect=[{"status": "ok"}, {"hosts": [{"online": False}]}],
    ):
        with pytest.raises(preflight.PreflightError, match="no online active runtime host"):
            preflight.validate_preflight(
                server_url="http://127.0.0.1:9800",
                gate_mode="report-only",
                manifest=manifest,
                baseline=tmp_path / "latest.json",
                database_url="postgres://example",
                api_token=None,
            )


def test_preflight_accepts_online_capable_runtime_host(tmp_path: Path) -> None:
    preflight = _load_preflight()
    manifest = tmp_path / "manifest.toml"
    manifest.write_text('suite = "test"\n', encoding="utf-8")

    with mock.patch.object(
        preflight,
        "_read_json",
        side_effect=[
            {"status": "ok"},
            {
                "hosts": [
                    {
                        "online": True,
                        "lifecycle": "active",
                        "capabilities": [
                            "eval_resource_limits",
                            "trusted_eval_verifier_v1",
                        ],
                    }
                ]
            },
        ],
    ):
        preflight.validate_preflight(
            server_url="http://127.0.0.1:9800",
            gate_mode="report-only",
            manifest=manifest,
            baseline=tmp_path / "latest.json",
            database_url="postgres://example",
            api_token=None,
        )


def test_baseline_eligibility_requires_complete_report_and_passing_diff() -> None:
    eligibility = _load_eligibility()
    report = {
        "metrics": {
            "pending_cases": 0,
            "skipped_cases": 0,
            "infra_failed_cases": 0,
        },
        "cases": [{"status": "passed"}, {"status": "failed"}],
    }

    eligibility.validate_eligibility(
        report, baseline_present=False, comparison_outcome="skipped"
    )
    eligibility.validate_eligibility(
        report, baseline_present=True, comparison_outcome="success"
    )

    incomplete = {**report, "outcome": "event_persistence_failed"}
    with pytest.raises(eligibility.EligibilityError, match="incomplete"):
        eligibility.validate_eligibility(
            incomplete, baseline_present=False, comparison_outcome="skipped"
        )
    with pytest.raises(eligibility.EligibilityError, match="did not pass comparison"):
        eligibility.validate_eligibility(
            report, baseline_present=True, comparison_outcome="failure"
        )
