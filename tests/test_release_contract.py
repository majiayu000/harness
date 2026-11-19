"""Keep the release workflow and its documentation aligned with the repository."""
from pathlib import Path
import re
import unittest


ROOT = Path(__file__).resolve().parents[1]
# A manual backfill checks out the requested tag and packages the build with
# that tag's own scripts/package-release.sh. v0.7.2 is the first tag that has it.
FIRST_PACKAGEABLE_TAG = (0, 7, 2)


def release_tags(text):
    return [tuple(map(int, tag)) for tag in re.findall(r"\bv(\d+)\.(\d+)\.(\d+)\b", text)]


class ReleaseContractTests(unittest.TestCase):
    def test_backfill_docs_only_offer_tags_the_workflow_can_package(self):
        workflow = (ROOT / ".github/workflows/release-binaries.yml").read_text(encoding="utf-8")
        releasing = (ROOT / "RELEASING.md").read_text(encoding="utf-8")
        tag_input = workflow.split("workflow_dispatch:", 1)[1].split("\npermissions:", 1)[0]
        backfill = releasing.split("To backfill an existing release", 1)[1].split("\n\n", 1)[0]
        for source, text in (("workflow tag input", tag_input), ("RELEASING.md", backfill)):
            with self.subTest(source):
                tags = release_tags(text)
                self.assertEqual([tag for tag in tags if tag < FIRST_PACKAGEABLE_TAG], [])
                self.assertIn(FIRST_PACKAGEABLE_TAG, tags)

    def test_changelog_links_cover_every_released_version(self):
        changelog = (ROOT / "CHANGELOG.md").read_text(encoding="utf-8")
        versions = re.findall(r"^## \[(\d+\.\d+\.\d+)\]", changelog, re.MULTILINE)
        links = dict(re.findall(r"^\[([^\]]+)\]: (\S+)$", changelog, re.MULTILINE))
        repo = "https://github.com/majiayu000/harness"
        self.assertEqual(links.get("Unreleased"), f"{repo}/compare/v{versions[0]}...HEAD")
        for version, previous in zip(versions, versions[1:]):
            with self.subTest(version):
                self.assertEqual(links.get(version), f"{repo}/compare/v{previous}...v{version}")
        self.assertEqual(links.get(versions[-1]), f"{repo}/releases/tag/v{versions[-1]}")


if __name__ == "__main__":
    unittest.main()
