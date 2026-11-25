import pathlib
import subprocess
import sys
import tempfile
import unittest

SCRIPT = pathlib.Path(__file__).resolve().parents[1] / "check-repo-hygiene.py"
HOST = "dev" + "-ct-node" + ".example"
DOMAIN = "node" + ".internal"


class CheckRepoHygieneTests(unittest.TestCase):
    def run_check(self, root, allowlist):
        return subprocess.run(
            [
                sys.executable,
                str(SCRIPT),
                "--root",
                str(root),
                "--allowlist",
                str(allowlist),
            ],
            capture_output=True,
            text=True,
        )

    def test_documentation_addresses_pass(self):
        with tempfile.TemporaryDirectory() as tmp:
            root = pathlib.Path(tmp)
            (root / "docs").mkdir()
            (root / "docs" / "tunnel.md").write_text('listen = "192.0.2.1:4098"\n')
            allowlist = root / "allowlist.txt"
            allowlist.write_text("")
            result = self.run_check(root, allowlist)
            self.assertEqual(result.returncode, 0, result.stderr)

    def test_development_hostname_fails(self):
        with tempfile.TemporaryDirectory() as tmp:
            root = pathlib.Path(tmp)
            (root / "notes.md").write_text(f"proxy = {HOST}\n")
            allowlist = root / "allowlist.txt"
            allowlist.write_text("")
            result = self.run_check(root, allowlist)
            self.assertEqual(result.returncode, 1)
            self.assertIn(HOST, result.stderr)

    def test_internal_domain_fails(self):
        with tempfile.TemporaryDirectory() as tmp:
            root = pathlib.Path(tmp)
            (root / "notes.md").write_text(f"host = {DOMAIN}\n")
            allowlist = root / "allowlist.txt"
            allowlist.write_text("")
            result = self.run_check(root, allowlist)
            self.assertEqual(result.returncode, 1)
            self.assertIn(DOMAIN, result.stderr)

    def test_allowlisted_exception_passes(self):
        with tempfile.TemporaryDirectory() as tmp:
            root = pathlib.Path(tmp)
            (root / "docs").mkdir()
            (root / "docs" / "guide.md").write_text(f"host = {DOMAIN}\n")
            allowlist = root / "allowlist.txt"
            allowlist.write_text(f"docs/guide.md {DOMAIN}\n")
            result = self.run_check(root, allowlist)
            self.assertEqual(result.returncode, 0, result.stderr)


if __name__ == "__main__":
    unittest.main()
