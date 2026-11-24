"""Unit tests for scripts/set-release-version.py."""

import importlib.util
import subprocess
import sys
import tempfile
import unittest
from pathlib import Path

SCRIPT = Path(__file__).resolve().parents[1] / "set-release-version.py"
spec = importlib.util.spec_from_file_location("set_release_version", SCRIPT)
module = importlib.util.module_from_spec(spec)
spec.loader.exec_module(module)


def write_checkout(root: Path, version: str = "0.15.0") -> None:
    (root / "Cargo.toml").write_text(
        "[workspace]\nmembers = []\n\n"
        "[package]\nname = \"reiki\"\n"
        f"version = \"{version}\"\nedition = \"2024\"\n",
        encoding="utf-8",
    )
    (root / "Cargo.lock").write_text(
        "# generated\nversion = 3\n\n"
        "[[package]]\nname = \"reiki\"\n"
        f"version = \"{version}\"\n\n"
        "[[package]]\nname = \"other\"\n"
        f"version = \"{version}\"\n",
        encoding="utf-8",
    )


class SetReleaseVersionTest(unittest.TestCase):
    def run_script(self, root: Path, tag: str) -> subprocess.CompletedProcess[str]:
        return subprocess.run(
            [sys.executable, str(SCRIPT), "--tag", tag, "--root", str(root)],
            capture_output=True,
            text=True,
            check=False,
        )

    def test_stable_tag_mismatch_refused(self) -> None:
        with tempfile.TemporaryDirectory() as temp:
            root = Path(temp)
            write_checkout(root)
            result = self.run_script(root, "v0.15.1")
            self.assertEqual(result.returncode, 1)
            self.assertIn("stable tag v0.15.1", result.stderr)
            self.assertIn('version = "0.15.0"', (root / "Cargo.toml").read_text())
            self.assertIn('version = "0.15.0"', (root / "Cargo.lock").read_text())

    def test_stable_tag_accepted(self) -> None:
        with tempfile.TemporaryDirectory() as temp:
            root = Path(temp)
            write_checkout(root)
            result = self.run_script(root, "v0.15.0")
            self.assertEqual(result.returncode, 0, result.stderr)
            self.assertIn('version = "0.15.0"', (root / "Cargo.toml").read_text())

    def test_positional_tag_accepted(self) -> None:
        with tempfile.TemporaryDirectory() as temp:
            root = Path(temp)
            write_checkout(root)
            result = subprocess.run(
                [sys.executable, str(SCRIPT), "v0.15.0", "--root", str(root)],
                capture_output=True,
                text=True,
                check=False,
            )
            self.assertEqual(result.returncode, 0, result.stderr)

    def test_dev_rewrite_correct(self) -> None:
        with tempfile.TemporaryDirectory() as temp:
            root = Path(temp)
            write_checkout(root)
            result = self.run_script(root, "v0.15.1-dev.3.gdeadbee")
            self.assertEqual(result.returncode, 0, result.stderr)
            cargo = (root / "Cargo.toml").read_text()
            lock = (root / "Cargo.lock").read_text()
            self.assertIn('version = "0.15.1-dev.3.gdeadbee"', cargo)
            self.assertIn('version = "0.15.1-dev.3.gdeadbee"', lock)
            self.assertIn('name = "other"\nversion = "0.15.0"', lock)

    def test_dev_base_mismatch_refused(self) -> None:
        with tempfile.TemporaryDirectory() as temp:
            root = Path(temp)
            write_checkout(root)
            result = self.run_script(root, "v0.16.0-dev.3.gdeadbee")
            self.assertEqual(result.returncode, 1)
            self.assertIn("cannot be applied", result.stderr)

    def test_dev_ordering(self) -> None:
        key = module.version_sort_key
        versions = [
            "0.15.0",
            "0.15.1-dev.1.gdeadbeef",
            "0.15.1-dev.10.gcafebabe",
            "0.15.1",
        ]
        self.assertEqual(versions, sorted(versions, key=key))
        self.assertLess(key("0.15.0"), key("0.15.1-dev.3.gdeadbee"))
        self.assertLess(key("0.15.1-dev.3.gdeadbee"), key("0.15.1"))


if __name__ == "__main__":
    unittest.main()
