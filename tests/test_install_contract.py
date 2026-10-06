"""Keep the documented fresh-build prerequisites aligned with repository tooling."""
import json
from pathlib import Path
import unittest


ROOT = Path(__file__).resolve().parents[1]


class InstallContractTests(unittest.TestCase):
    def test_bun_engine_and_install_docs_match_tested_toolchain(self):
        version = (ROOT / ".bun-version").read_text(encoding="utf-8").strip()
        package = json.loads((ROOT / "web/package.json").read_text(encoding="utf-8"))
        self.assertEqual(package["engines"]["bun"], f">={version}")
        readme = (ROOT / "README.md").read_text(encoding="utf-8")
        install = readme.split("## Install", 1)[1].split("## Quickstart", 1)[0]
        server = readme.split("### Server prerequisites", 1)[1].split("\n### ", 1)[0]
        self.assertIn(f"Bun {version}", install)
        self.assertIn(f"Bun {version}", server)

    def test_linux_native_tls_build_prerequisites_are_documented(self):
        readme = (ROOT / "README.md").read_text(encoding="utf-8")
        install = readme.split("## Install", 1)[1].split("## Quickstart", 1)[0]
        for prerequisite in ("Linux", "OpenSSL", "pkg-config", "libssl-dev"):
            self.assertIn(prerequisite, install)


if __name__ == "__main__":
    unittest.main()
