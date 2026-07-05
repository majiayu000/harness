#!/usr/bin/env python3
"""Tests for scripts/gc-target.sh."""

from __future__ import annotations

import os
import shutil
import stat
import subprocess
import tempfile
import time
import unittest
from pathlib import Path


SCRIPT_DIR = Path(__file__).parent
SCRIPT = SCRIPT_DIR / "gc-target.sh"


class GcTargetTests(unittest.TestCase):
    def make_repo(self, tmp: Path) -> Path:
        repo = tmp / "repo"
        scripts = repo / "scripts"
        scripts.mkdir(parents=True)
        shutil.copy2(SCRIPT, scripts / "gc-target.sh")
        (scripts / "gc-target.sh").chmod(0o755)
        return repo

    def run_script(
        self,
        repo: Path,
        *args: str,
        cargo_mode: str = "missing",
    ) -> subprocess.CompletedProcess[str]:
        bin_dir = repo / "bin"
        bin_dir.mkdir(exist_ok=True)
        cargo = bin_dir / "cargo"

        if cargo_mode == "missing":
            cargo.write_text("#!/usr/bin/env bash\nexit 1\n", encoding="utf-8")
        elif cargo_mode == "sweep":
            log = repo / "cargo-sweep.log"
            cargo.write_text(
                "#!/usr/bin/env bash\n"
                "printf '%s\\n' \"$*\" >> \""
                + str(log)
                + "\"\n"
                "if [ \"$1\" = sweep ]; then exit 0; fi\n"
                "exit 1\n",
                encoding="utf-8",
            )
        else:
            raise ValueError(f"unknown cargo_mode: {cargo_mode}")

        cargo.chmod(0o755)
        env = os.environ.copy()
        env["PATH"] = f"{bin_dir}{os.pathsep}{env['PATH']}"

        return subprocess.run(
            [str(repo / "scripts" / "gc-target.sh"), *args],
            cwd=repo,
            env=env,
            text=True,
            capture_output=True,
            check=False,
        )

    def write_artifact(self, path: Path, age_days: int) -> None:
        path.parent.mkdir(parents=True, exist_ok=True)
        path.write_text("artifact", encoding="utf-8")
        timestamp = time.time() - age_days * 24 * 60 * 60
        os.utime(path, (timestamp, timestamp))

    def test_help_and_executable_bit(self) -> None:
        mode = SCRIPT.stat().st_mode
        self.assertTrue(mode & stat.S_IXUSR)

        result = subprocess.run(
            [str(SCRIPT), "--help"],
            text=True,
            capture_output=True,
            check=False,
        )

        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertIn("--days N", result.stdout)
        self.assertIn("--dry-run", result.stdout)

    def test_no_target_is_successful_noop(self) -> None:
        with tempfile.TemporaryDirectory() as raw:
            repo = self.make_repo(Path(raw))

            result = self.run_script(repo)

            self.assertEqual(result.returncode, 0, result.stderr)
            self.assertIn("nothing to clean", result.stdout)

    def test_fallback_removes_only_stale_bounded_artifacts(self) -> None:
        with tempfile.TemporaryDirectory() as raw:
            repo = self.make_repo(Path(raw))
            stale = repo / "target" / "debug" / "deps" / "old file.o"
            fresh = repo / "target" / "debug" / "deps" / "fresh.o"
            outside = repo / "target" / "debug" / "other" / "old.o"
            aux_stale = repo / "target" / "cargo-check" / "debug" / "build" / "old.o"
            self.write_artifact(stale, age_days=3)
            self.write_artifact(fresh, age_days=0)
            self.write_artifact(outside, age_days=3)
            self.write_artifact(aux_stale, age_days=3)

            result = self.run_script(repo, "--days", "1")

            self.assertEqual(result.returncode, 0, result.stderr)
            self.assertFalse(stale.exists())
            self.assertTrue(fresh.exists())
            self.assertTrue(outside.exists())
            self.assertFalse(aux_stale.exists())
            self.assertIn("Fallback removed paths:", result.stdout)

    def test_dry_run_reports_without_deleting(self) -> None:
        with tempfile.TemporaryDirectory() as raw:
            repo = self.make_repo(Path(raw))
            stale = repo / "target" / "debug" / "incremental" / "old"
            self.write_artifact(stale, age_days=3)

            result = self.run_script(repo, "--days=1", "--dry-run")

            self.assertEqual(result.returncode, 0, result.stderr)
            self.assertTrue(stale.exists())
            self.assertIn("would remove target/debug/incremental/old", result.stdout)
            self.assertIn("Dry run candidates: 1", result.stdout)

    def test_fallback_is_idempotent(self) -> None:
        with tempfile.TemporaryDirectory() as raw:
            repo = self.make_repo(Path(raw))
            stale = repo / "target" / "debug" / "build" / "old"
            self.write_artifact(stale, age_days=3)

            first = self.run_script(repo, "--days", "1")
            second = self.run_script(repo, "--days", "1")

            self.assertEqual(first.returncode, 0, first.stderr)
            self.assertEqual(second.returncode, 0, second.stderr)
            self.assertIn("Fallback removed paths: 1", first.stdout)
            self.assertIn("Fallback removed paths: 0", second.stdout)

    def test_cargo_sweep_is_preferred_when_available(self) -> None:
        with tempfile.TemporaryDirectory() as raw:
            repo = self.make_repo(Path(raw))
            self.write_artifact(repo / "target" / "debug" / "deps" / "old", age_days=3)

            result = self.run_script(repo, "--days", "9", cargo_mode="sweep")

            self.assertEqual(result.returncode, 0, result.stderr)
            self.assertIn("Using cargo sweep", result.stdout)
            log = (repo / "cargo-sweep.log").read_text(encoding="utf-8")
            self.assertIn("sweep --help", log)
            self.assertIn("sweep --time 9", log)

    def test_invalid_days_are_rejected(self) -> None:
        with tempfile.TemporaryDirectory() as raw:
            repo = self.make_repo(Path(raw))

            result = self.run_script(repo, "--days", "-1")

            self.assertEqual(result.returncode, 2)
            self.assertIn("non-negative integer", result.stderr)


if __name__ == "__main__":
    unittest.main()
