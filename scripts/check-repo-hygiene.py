#!/usr/bin/env python3
"""Fail when tracked files contain internal hostnames or domains."""

from __future__ import annotations

import argparse
import pathlib
import re
import subprocess
import sys

HOST_PREFIX = "dev" + "-ct-"
RULES = (
    (
        "development hostname",
        re.compile(r"\b" + re.escape(HOST_PREFIX) + r"[A-Za-z0-9.-]+\b", re.I),
    ),
    (
        "internal domain",
        re.compile(
            r"\b[A-Za-z0-9-]+(?:\.[A-Za-z0-9-]+)*\.(?:corp|internal|intranet|lan|local)\b",
            re.I,
        ),
    ),
)
SKIP_DIRS = {".git", "node_modules", "target"}


def tracked_files(root: pathlib.Path) -> list[pathlib.Path]:
    try:
        result = subprocess.run(
            ["git", "-C", str(root), "ls-files", "-z"],
            check=True,
            capture_output=True,
        )
    except (FileNotFoundError, subprocess.CalledProcessError):
        return [
            path
            for path in root.rglob("*")
            if path.is_file()
            and not SKIP_DIRS.intersection(path.relative_to(root).parts)
        ]
    return [root / name for name in result.stdout.decode().split("\0") if name]


def load_allowlist(path: pathlib.Path) -> set[tuple[str, str]]:
    entries: set[tuple[str, str]] = set()
    for number, line in enumerate(path.read_text().splitlines(), 1):
        text = line.split("#", 1)[0].strip()
        if not text:
            continue
        parts = text.split()
        if len(parts) != 2:
            raise ValueError(f"{path}:{number}: expected 'path match'")
        entries.add((parts[0], parts[1].lower()))
    return entries


def scan(root: pathlib.Path, allowlist: pathlib.Path) -> list[tuple[str, int, str, str]]:
    allowed = load_allowlist(allowlist)
    findings: set[tuple[str, int, str, str]] = set()
    for path in tracked_files(root):
        if path == allowlist:
            continue
        try:
            data = path.read_bytes()
        except OSError:
            continue
        if b"\0" in data:
            continue
        relative = path.relative_to(root).as_posix()
        for number, line in enumerate(data.decode(errors="replace").splitlines(), 1):
            for label, pattern in RULES:
                for match in pattern.finditer(line):
                    text = match.group(0)
                    if (relative, text.lower()) in allowed:
                        continue
                    findings.add((relative, number, label, text))
    return sorted(findings)


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument(
        "--root",
        type=pathlib.Path,
        default=pathlib.Path(__file__).resolve().parent.parent,
        help="repository to scan",
    )
    parser.add_argument(
        "--allowlist",
        type=pathlib.Path,
        help="exception file (defaults to scripts/check-repo-hygiene.allowlist)",
    )
    args = parser.parse_args()
    root = args.root.resolve()
    allowlist = (args.allowlist or root / "scripts/check-repo-hygiene.allowlist").resolve()
    try:
        findings = scan(root, allowlist)
    except (OSError, ValueError) as error:
        print(f"repo hygiene: {error}", file=sys.stderr)
        return 2
    for relative, number, label, text in findings:
        print(f"{relative}:{number}: {label}: {text}", file=sys.stderr)
    if findings:
        print(
            "repo hygiene: internal names found; add a justified exception only when required",
            file=sys.stderr,
        )
        return 1
    print("repo hygiene: no internal hostnames or domains")
    return 0


if __name__ == "__main__":
    sys.exit(main())
