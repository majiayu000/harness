from __future__ import annotations

import json
import os
import subprocess
import sys
from pathlib import Path

import pytest


ROOT = Path(__file__).resolve().parents[1]
CHECKS = ROOT / "checks"
sys.path.insert(0, str(CHECKS))

from github_pr_evidence import (  # noqa: E402
    EvidenceError,
    PR_VIEW_FIELDS,
    build_evidence,
    build_human_authorization,
    collect_review_threads,
    normalize_review_threads,
    parse_github_repo,
)
from pr_gate import evaluate_pr_gate  # noqa: E402


def pr_payload() -> dict[str, object]:
    return {
        "number": 10,
        "state": "OPEN",
        "isDraft": False,
        "headRefOid": "e36d97517d8d0b27faca1abe5e5c63f9f88684d9",
        "mergeStateStatus": "CLEAN",
        "reviewDecision": "APPROVED",
        "body": "Linked Work: #9\n",
        "closingIssuesReferences": [{"number": 9}],
        "statusCheckRollup": [
            {
                "__typename": "CheckRun",
                "name": "workflow-check",
                "status": "COMPLETED",
                "conclusion": "SUCCESS",
                "detailsUrl": "https://github.com/example/specrail/actions/runs/1",
            },
            {
                "__typename": "StatusContext",
                "context": "lint",
                "state": "SUCCESS",
                "targetUrl": "https://ci.example.invalid/lint",
            },
        ],
        "reviews": [
            {"author": {"login": "reviewer"}, "state": "CHANGES_REQUESTED"},
            {"author": {"login": "reviewer"}, "state": "APPROVED"},
            {"author": {"login": "bot"}, "state": "COMMENTED"},
        ],
    }


def threads_payload() -> dict[str, object]:
    return {
        "data": {
            "repository": {
                "pullRequest": {
                    "reviewThreads": {
                        "nodes": [
                            {
                                "id": "PRRT_kwDOExample",
                                "isResolved": True,
                                "isOutdated": False,
                                "comments": {
                                    "nodes": [
                                        {
                                            "url": "https://github.com/example/specrail/pull/10#discussion_r1",
                                            "author": {"login": "reviewer"},
                                        }
                                    ]
                                },
                            }
                        ],
                        "pageInfo": {"hasNextPage": False, "endCursor": None},
                    }
                }
            }
        }
    }


def test_parse_github_repo_requires_owner_repo() -> None:
    assert parse_github_repo("majiayu000/specrail") == ("majiayu000", "specrail")

    with pytest.raises(EvidenceError):
        parse_github_repo("majiayu000/specrail/extra")

    with pytest.raises(EvidenceError):
        parse_github_repo("../specrail")


def test_build_evidence_matches_pr_gate_contract() -> None:
    evidence = build_evidence(
        pr_payload(),
        threads_payload(),
        {
            "actor": "user",
            "source": "chat",
            "summary": "merge approved",
        },
    )

    assert evidence["pr"] == 10
    assert evidence["linked_issue"] == 9
    assert evidence["checks"] == [
        {
            "name": "workflow-check",
            "status": "COMPLETED",
            "conclusion": "SUCCESS",
            "url": "https://github.com/example/specrail/actions/runs/1",
        },
        {
            "name": "lint",
            "status": "COMPLETED",
            "conclusion": "SUCCESS",
            "url": "https://ci.example.invalid/lint",
        },
    ]
    assert evidence["reviews"] == [
        {"author": "reviewer", "state": "APPROVED"},
        {"author": "bot", "state": "COMMENTED"},
    ]
    assert evidence["review_decision"] == "APPROVED"
    assert evidence["review_threads"] == [
        {
            "id": "PRRT_kwDOExample",
            "url": "https://github.com/example/specrail/pull/10#discussion_r1",
            "is_resolved": True,
            "is_outdated": False,
        }
    ]
    assert evaluate_pr_gate(evidence)["decision"] == "allowed"


def test_pr_view_collects_review_decision() -> None:
    assert "reviewDecision" in PR_VIEW_FIELDS


def test_review_decision_blocks_when_required() -> None:
    payload = pr_payload()
    payload["reviewDecision"] = "REVIEW_REQUIRED"

    evidence = build_evidence(
        payload,
        threads_payload(),
        {
            "actor": "user",
            "source": "chat",
            "summary": "merge approved",
        },
    )
    result = evaluate_pr_gate(evidence)

    assert result["decision"] == "blocked"
    assert "reviewDecision blocks merge: REVIEW_REQUIRED" in result["reasons"]


def test_reviews_preserve_blocking_request_through_comment() -> None:
    payload = pr_payload()
    payload["reviews"] = [
        {"author": {"login": "reviewer"}, "state": "CHANGES_REQUESTED"},
        {"author": {"login": "reviewer"}, "state": "COMMENTED"},
    ]

    evidence = build_evidence(payload, threads_payload())

    assert evidence["reviews"] == [{"author": "reviewer", "state": "CHANGES_REQUESTED"}]
    result = evaluate_pr_gate(evidence)
    assert result["decision"] == "blocked"
    assert "changes requested by reviewer" in result["reasons"]


def test_reviews_allow_approval_to_clear_blocking_request() -> None:
    payload = pr_payload()
    payload["reviews"] = [
        {"author": {"login": "reviewer"}, "state": "CHANGES_REQUESTED"},
        {"author": {"login": "reviewer"}, "state": "APPROVED"},
        {"author": {"login": "reviewer"}, "state": "COMMENTED"},
    ]

    evidence = build_evidence(payload, threads_payload())

    assert evidence["reviews"] == [{"author": "reviewer", "state": "APPROVED"}]


def test_build_evidence_without_authorization_needs_human() -> None:
    evidence = build_evidence(pr_payload(), threads_payload())

    assert "human_authorization" not in evidence
    result = evaluate_pr_gate(evidence)
    assert result["decision"] == "needs_human"
    assert "human_authorization" in result["missing"]


def test_build_evidence_accepts_absent_optional_github_fields() -> None:
    payload = pr_payload()
    payload["closingIssuesReferences"] = None
    payload["statusCheckRollup"] = None
    payload["reviews"] = None
    payload["body"] = ""

    evidence = build_evidence(payload, threads_payload())

    assert evidence["linked_issue"] is None
    assert evidence["checks"] == []
    assert evidence["reviews"] == []


def test_build_evidence_fails_closed_on_capped_check_rollup() -> None:
    payload = pr_payload()
    payload["statusCheckRollup"] = [
        {"name": f"check-{index}", "status": "COMPLETED", "conclusion": "SUCCESS"}
        for index in range(100)
    ]

    with pytest.raises(EvidenceError, match="statusCheckRollup may be truncated"):
        build_evidence(payload, threads_payload())


def test_build_evidence_fails_closed_on_capped_reviews() -> None:
    payload = pr_payload()
    payload["reviews"] = [
        {"author": {"login": f"reviewer-{index}"}, "state": "COMMENTED"}
        for index in range(100)
    ]

    with pytest.raises(EvidenceError, match="reviews may be truncated"):
        build_evidence(payload, threads_payload())


def test_build_evidence_parses_linked_issue_from_body_when_not_closing() -> None:
    payload = pr_payload()
    payload["closingIssuesReferences"] = []
    payload["body"] = "## Linked Work\n\nIssue: #123\n"

    evidence = build_evidence(payload, threads_payload())

    assert evidence["linked_issue"] == 123


def test_build_evidence_parses_linked_issue_from_template_bullet() -> None:
    payload = pr_payload()
    payload["closingIssuesReferences"] = []
    payload["body"] = "## Linked Work\n\n- Issue: #123\n"

    evidence = build_evidence(payload, threads_payload())

    assert evidence["linked_issue"] == 123


def test_review_threads_fail_closed_on_truncated_page() -> None:
    payload = threads_payload()
    review_threads = payload["data"]["repository"]["pullRequest"]["reviewThreads"]  # type: ignore[index]
    del review_threads["pageInfo"]
    review_threads["nodes"] = review_threads["nodes"] * 100

    with pytest.raises(EvidenceError, match="pagination state"):
        normalize_review_threads(payload)


def test_review_threads_fail_closed_when_page_info_is_absent() -> None:
    payload = threads_payload()
    review_threads = payload["data"]["repository"]["pullRequest"]["reviewThreads"]  # type: ignore[index]
    del review_threads["pageInfo"]

    with pytest.raises(EvidenceError, match="pagination state"):
        normalize_review_threads(payload)


def test_review_threads_fail_closed_when_more_pages_remain() -> None:
    payload = threads_payload()
    review_threads = payload["data"]["repository"]["pullRequest"]["reviewThreads"]  # type: ignore[index]
    review_threads["pageInfo"] = {"hasNextPage": True, "endCursor": "cursor-1"}

    with pytest.raises(EvidenceError, match="truncated"):
        normalize_review_threads(payload)


def test_review_threads_fail_closed_when_has_next_page_is_missing() -> None:
    payload = threads_payload()
    review_threads = payload["data"]["repository"]["pullRequest"]["reviewThreads"]  # type: ignore[index]
    review_threads["pageInfo"] = {"endCursor": None}

    with pytest.raises(EvidenceError, match="hasNextPage must be a boolean"):
        normalize_review_threads(payload)


def test_review_threads_fail_closed_when_has_next_page_is_null() -> None:
    payload = threads_payload()
    review_threads = payload["data"]["repository"]["pullRequest"]["reviewThreads"]  # type: ignore[index]
    review_threads["pageInfo"] = {"hasNextPage": None, "endCursor": None}

    with pytest.raises(EvidenceError, match="hasNextPage must be a boolean"):
        normalize_review_threads(payload)


def test_collect_review_threads_returns_aggregated_pages(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch
) -> None:
    page_1 = threads_payload()
    page_1_threads = page_1["data"]["repository"]["pullRequest"]["reviewThreads"]  # type: ignore[index]
    page_1_threads["pageInfo"] = {"hasNextPage": True, "endCursor": "cursor-1"}
    page_2 = threads_payload()
    page_2_threads = page_2["data"]["repository"]["pullRequest"]["reviewThreads"]  # type: ignore[index]
    page_2_threads["nodes"][0]["id"] = "PRRT_kwDOExamplePage2"
    page_2_threads["pageInfo"] = {"hasNextPage": False, "endCursor": "cursor-2"}

    bin_dir = tmp_path / "bin"
    bin_dir.mkdir()
    fake_gh = bin_dir / "gh"
    fake_gh.write_text(
        "\n".join(
            [
                "#!/usr/bin/env python3",
                "from __future__ import annotations",
                "import json",
                "import sys",
                f"page_1 = {json.dumps(page_1)!r}",
                f"page_2 = {json.dumps(page_2)!r}",
                "args = sys.argv[1:]",
                "print(page_2 if 'after=cursor-1' in args else page_1)",
            ]
        ),
        encoding="utf-8",
    )
    fake_gh.chmod(0o755)
    monkeypatch.setenv("PATH", f"{bin_dir}{os.pathsep}{os.environ.get('PATH', '')}")

    payload = collect_review_threads("majiayu000", "specrail", 10)
    threads = normalize_review_threads(payload)

    assert [thread["id"] for thread in threads] == [
        "PRRT_kwDOExample",
        "PRRT_kwDOExamplePage2",
    ]


def test_collect_review_threads_fails_when_page_info_is_absent(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch
) -> None:
    payload = threads_payload()
    review_threads = payload["data"]["repository"]["pullRequest"]["reviewThreads"]  # type: ignore[index]
    del review_threads["pageInfo"]

    bin_dir = tmp_path / "bin"
    bin_dir.mkdir()
    fake_gh = bin_dir / "gh"
    fake_gh.write_text(
        "\n".join(
            [
                "#!/usr/bin/env python3",
                "from __future__ import annotations",
                "import json",
                f"payload = {json.dumps(payload)!r}",
                "print(payload)",
            ]
        ),
        encoding="utf-8",
    )
    fake_gh.chmod(0o755)
    monkeypatch.setenv("PATH", f"{bin_dir}{os.pathsep}{os.environ.get('PATH', '')}")

    with pytest.raises(EvidenceError, match="pagination state"):
        collect_review_threads("majiayu000", "specrail", 10)


def test_collect_review_threads_fails_when_has_next_page_is_missing(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch
) -> None:
    payload = threads_payload()
    review_threads = payload["data"]["repository"]["pullRequest"]["reviewThreads"]  # type: ignore[index]
    review_threads["pageInfo"] = {"endCursor": None}

    bin_dir = tmp_path / "bin"
    bin_dir.mkdir()
    fake_gh = bin_dir / "gh"
    fake_gh.write_text(
        "\n".join(
            [
                "#!/usr/bin/env python3",
                "from __future__ import annotations",
                "import json",
                f"payload = {json.dumps(payload)!r}",
                "print(payload)",
            ]
        ),
        encoding="utf-8",
    )
    fake_gh.chmod(0o755)
    monkeypatch.setenv("PATH", f"{bin_dir}{os.pathsep}{os.environ.get('PATH', '')}")

    with pytest.raises(EvidenceError, match="hasNextPage must be a boolean"):
        collect_review_threads("majiayu000", "specrail", 10)


def test_pr_review_gate_schema_matches_collected_evidence_contract() -> None:
    schema = json.loads((ROOT / "schemas" / "pr_review_gate.schema.json").read_text())
    required = set(schema["required"])
    properties = set(schema["properties"])

    assert {
        "pr",
        "state",
        "is_draft",
        "linked_issue",
        "head_sha",
        "merge_state",
        "checks",
        "reviews",
        "review_threads",
    } <= required
    assert "review_decision" in properties
    assert {"readiness_label", "agent_review", "human_review", "verification"} - properties == {
        "readiness_label",
        "agent_review",
        "human_review",
        "verification",
    }


def test_authorization_flags_must_include_actor_and_source() -> None:
    assert build_human_authorization(None, None, None) is None
    assert build_human_authorization("user", "chat", "approved") == {
        "actor": "user",
        "source": "chat",
        "summary": "approved",
    }

    with pytest.raises(EvidenceError):
        build_human_authorization("user", None, None)

    with pytest.raises(EvidenceError):
        build_human_authorization(None, None, "approved")


def test_cli_uses_fake_gh_without_network(tmp_path: Path, monkeypatch: pytest.MonkeyPatch) -> None:
    bin_dir = tmp_path / "bin"
    bin_dir.mkdir()
    fake_gh = bin_dir / "gh"
    fake_gh.write_text(
        "\n".join(
            [
                "#!/usr/bin/env python3",
                "from __future__ import annotations",
                "import json",
                "import sys",
                f"pr_payload = {json.dumps(pr_payload())!r}",
                f"threads_payload = {json.dumps(threads_payload())!r}",
                "args = sys.argv[1:]",
                "if args[:2] == ['pr', 'view']:",
                "    print(pr_payload)",
                "elif args[:2] == ['api', 'graphql']:",
                "    print(threads_payload)",
                "else:",
                "    print('unexpected args: ' + ' '.join(args), file=sys.stderr)",
                "    raise SystemExit(2)",
            ]
        ),
        encoding="utf-8",
    )
    fake_gh.chmod(0o755)
    monkeypatch.setenv("PATH", f"{bin_dir}{os.pathsep}{os.environ.get('PATH', '')}")

    result = subprocess.run(
        [
            sys.executable,
            "checks/github_pr_evidence.py",
            "--github-repo",
            "majiayu000/specrail",
            "--pr",
            "10",
            "--authorization-actor",
            "user",
            "--authorization-source",
            "chat",
            "--authorization-summary",
            "merge approved",
            "--json",
        ],
        cwd=ROOT,
        check=False,
        capture_output=True,
        text=True,
    )

    assert result.returncode == 0, result.stderr
    evidence = json.loads(result.stdout)
    assert evidence["pr"] == 10
    assert evidence["linked_issue"] == 9
    assert evidence["human_authorization"] == {
        "actor": "user",
        "source": "chat",
        "summary": "merge approved",
    }
    assert evaluate_pr_gate(evidence)["decision"] == "allowed"
