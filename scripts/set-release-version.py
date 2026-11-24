#!/usr/bin/env python3
"""Validate a release tag and set the checkout version for dev builds.

A stable tag must equal the version in Cargo.toml. A dev tag rewrites
Cargo.toml and the root package entry in Cargo.lock for the build only; the
source commit and the repository are not changed.
"""

from __future__ import annotations

import argparse
import re
import sys
from pathlib import Path

STABLE_TAG = re.compile(r"^v(\d+)\.(\d+)\.(\d+)$")
DEV_TAG = re.compile(r"^v(\d+)\.(\d+)\.(\d+)-dev\.(\d+)\.g([0-9a-f]{7,12})$")
VERSION = re.compile(r"^(\d+)\.(\d+)\.(\d+)(?:-([0-9A-Za-z.-]+))?$")


class VersionError(Exception):
    pass


def parse_version(version: str) -> tuple[str, str, str, str | None]:
    match = VERSION.fullmatch(version)
    if match is None:
        raise VersionError(f"invalid version {version!r}")
    major, minor, patch, pre = match.groups()
    return major, minor, patch, pre


def version_sort_key(version: str) -> tuple[object, ...]:
    """Order a dev build after its base stable and before the next stable."""
    major, minor, patch, pre = parse_version(version)
    base = (int(major), int(minor), int(patch))
    if pre is None:
        return (*base, 1, ())
    identifiers = tuple(
        (0, int(part)) if part.isdigit() else (1, part) for part in pre.split(".")
    )
    return (*base, 0, identifiers)


def quoted_field(section: str, name: str) -> re.Match[str] | None:
    return re.search(rf'(?m)^[ \t]*{re.escape(name)}[ \t]*=[ \t]*"([^"]+)"', section)


def package_section(cargo: str) -> re.Match[str]:
    match = re.search(r"(?ms)^\[package\]\s*\n(.*?)(?=^\[|\Z)", cargo)
    if match is None:
        raise VersionError("no [package] section in Cargo.toml")
    return match


def field_span(section: re.Match[str], name: str) -> tuple[str, int, int]:
    field = quoted_field(section.group(1), name)
    if field is None:
        raise VersionError(f"no {name} in [package] section of Cargo.toml")
    return field.group(1), section.start(1) + field.start(1), section.start(1) + field.end(1)


def lock_version(lock: str, name: str) -> tuple[str, int, int]:
    pattern = re.compile(r"(?ms)^\[\[package\]\]\n(.*?)(?=^\[\[package\]\]|\Z)")
    for block in pattern.finditer(lock):
        block_name = quoted_field(block.group(1), "name")
        if block_name is None or block_name.group(1) != name:
            continue
        version = quoted_field(block.group(1), "version")
        if version is None:
            raise VersionError(f"no version for {name!r} in Cargo.lock")
        return (
            version.group(1),
            block.start(1) + version.start(1),
            block.start(1) + version.end(1),
        )
    raise VersionError(f"no Cargo.lock entry for {name!r}")


def replace_span(text: str, start: int, end: int, value: str) -> str:
    return text[:start] + value + text[end:]


def apply_tag(tag: str, root: Path) -> None:
    stable = STABLE_TAG.fullmatch(tag)
    dev = DEV_TAG.fullmatch(tag)
    if stable is None and dev is None:
        raise VersionError(
            f"unsupported tag {tag!r}; expected vMAJOR.MINOR.PATCH or "
            "vMAJOR.MINOR.PATCH-dev.N.gHASH"
        )

    cargo_path = root / "Cargo.toml"
    lock_path = root / "Cargo.lock"
    cargo = cargo_path.read_text(encoding="utf-8")
    lock = lock_path.read_text(encoding="utf-8")

    package = package_section(cargo)
    name, _, _ = field_span(package, "name")
    current, cargo_start, cargo_end = field_span(package, "version")
    locked, lock_start, lock_end = lock_version(lock, name)
    if current != locked:
        raise VersionError(
            f"Cargo.toml version {current} does not match Cargo.lock version {locked}"
        )

    if stable is not None:
        expected = tag[1:]
        if current != expected:
            raise VersionError(
                f"stable tag {tag} requires Cargo.toml version {expected}, found {current}"
            )
        return

    major, minor, patch, count, short = dev.groups()
    target = f"{major}.{minor}.{patch}-dev.{count}.g{short}"
    if current != target:
        current_base = parse_version(current)[:3]
        previous = (major, minor, str(int(patch) - 1))
        if current_base != previous and current_base != (major, minor, patch):
            raise VersionError(
                f"dev tag {tag} cannot be applied to checkout version {current}"
            )

    if current == target:
        return
    cargo = replace_span(cargo, cargo_start, cargo_end, target)
    lock = replace_span(lock, lock_start, lock_end, target)
    cargo_path.write_text(cargo, encoding="utf-8")
    lock_path.write_text(lock, encoding="utf-8")


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("tag", nargs="?", help="release tag to validate or apply")
    parser.add_argument("--tag", dest="tag_flag", help="release tag (alternative to the positional form)")
    parser.add_argument(
        "--root",
        type=Path,
        default=Path(__file__).resolve().parent.parent,
        help="checkout root (default: repository root)",
    )
    args = parser.parse_args()
    tag = args.tag_flag or args.tag
    if tag is None:
        parser.error("a release tag is required")
    if args.tag and args.tag_flag and args.tag != args.tag_flag:
        parser.error("tag given twice with different values")
    try:
        apply_tag(tag, args.root)
    except (VersionError, OSError) as error:
        print(f"error: {error}", file=sys.stderr)
        return 1
    return 0


if __name__ == "__main__":
    sys.exit(main())
