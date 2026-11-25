"""Unit tests for scripts/openapi-diff.py."""

import importlib.util
import json
import subprocess
import sys
import tempfile
import unittest
from pathlib import Path

SCRIPT = Path(__file__).resolve().parents[1] / "openapi-diff.py"
spec = importlib.util.spec_from_file_location("openapi_diff", SCRIPT)
module = importlib.util.module_from_spec(spec)
spec.loader.exec_module(module)


def diff(base, current):
    findings = []
    module.walk("$", base, current, findings)
    return findings


class OpenApiDiffTest(unittest.TestCase):
    def test_field_rename_is_breaking(self):
        base = {"properties": {"cool": {"type": "integer"}}}
        current = {"properties": {"cooldown_seconds": {"type": "integer"}}}
        self.assertEqual(
            diff(base, current),
            ["RENAMED $.properties.cool -> $.properties.cooldown_seconds"],
        )

    def test_path_removal_is_breaking(self):
        base = {"paths": {"/v1/usage": {"get": {}}, "/v1/status": {"get": {}}}}
        current = {"paths": {"/v1/status": {"get": {}}}}
        self.assertEqual(diff(base, current), ["PATH-REMOVED $.paths./v1/usage"])

    def test_tightened_bounds_are_breaking(self):
        base = {"properties": {"keys": {"type": "array", "maxItems": 256}}}
        current = {"properties": {"keys": {"type": "array", "maxItems": 64}}}
        self.assertEqual(
            diff(base, current),
            ["CONSTRAINT-CHANGED $.properties.keys.maxItems: 256 -> 64"],
        )

    def test_new_constraint_is_breaking_and_relaxation_is_not(self):
        base = {"properties": {"keys": {"type": "array"}}}
        tighter = {"properties": {"keys": {"type": "array", "maxItems": 64}}}
        looser = {"properties": {"keys": {"type": "array", "maxItems": 512}}}
        self.assertEqual(
            diff(base, tighter),
            ["CONSTRAINT-CHANGED $.properties.keys.maxItems: absent -> 64"],
        )
        self.assertEqual(diff(tighter, looser), [])
        self.assertEqual(
            diff(looser, tighter),
            ["CONSTRAINT-CHANGED $.properties.keys.maxItems: 512 -> 64"],
        )


    def test_format_and_required_changes_are_breaking(self):
        base = {"properties": {"at": {"type": "string"}}, "required": []}
        current = {
            "properties": {"at": {"type": "string", "format": "date-time"}},
            "required": ["at"],
        }
        self.assertEqual(
            diff(base, current),
            [
                "CONSTRAINT-CHANGED $.properties.at.format: absent -> 'date-time'",
                "NEWLY-REQUIRED $.required[at]",
            ],
        )

    def test_retyped_and_removed_fields_are_breaking(self):
        base = {"properties": {"count": {"type": "integer"}, "gone": {"type": "string"}}}
        current = {"properties": {"count": {"type": "string"}}}
        self.assertEqual(
            diff(base, current),
            [
                "REMOVED $.properties.gone",
                "RETYPED $.properties.count.type: 'integer' -> 'string'",
            ],
        )

    def test_metadata_changes_are_ignored(self):
        base = {"description": "old", "summary": "old"}
        current = {"description": "new", "summary": "new"}
        self.assertEqual(diff(base, current), [])

    def test_cli_reports_breaking_changes(self):
        with tempfile.TemporaryDirectory() as temp:
            root = Path(temp)
            base = root / "base.json"
            current = root / "current.json"
            base.write_text(json.dumps({"paths": {"/v1/usage": {"get": {}}}}), encoding="utf-8")
            current.write_text(json.dumps({"paths": {}}), encoding="utf-8")
            result = subprocess.run(
                [sys.executable, str(SCRIPT), "--base", str(base), "--current", str(current)],
                capture_output=True,
                text=True,
                check=False,
            )
            self.assertEqual(result.returncode, 1)
            self.assertIn("PATH-REMOVED $.paths./v1/usage", result.stdout)


if __name__ == "__main__":
    unittest.main()
