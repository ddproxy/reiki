#!/usr/bin/env python3
"""Compare two OpenAPI documents and report breaking changes.

Breaking: REMOVED, RENAMED, PATH-REMOVED, RETYPED, NEWLY-REQUIRED and
CONSTRAINT-CHANGED (tightened bounds, formats or constants). Exits non-zero when
breaking changes are found. Intended as a warning in CI until 1.0.
"""
from __future__ import annotations

import argparse
import json
import subprocess
import sys
from pathlib import Path

# Metadata text is documentation, not a contract obligation.
METADATA_KEYS = {"description", "summary", "example", "examples", "title"}

# A tightened bound excludes answers the old contract allowed. A relaxed one does not.
UPPER_BOUNDS = {"maxItems", "maxLength", "maximum", "exclusiveMaximum"}
LOWER_BOUNDS = {"minItems", "minLength", "minimum", "exclusiveMinimum"}
FORMAT_KEYS = {"format", "pattern", "const"}
CONSTRAINT_KEYS = UPPER_BOUNDS | LOWER_BOUNDS | FORMAT_KEYS


def load_json(path: str) -> dict:
    return json.loads(Path(path).read_text(encoding="utf-8"))


def load_main(ref: str, path: str) -> dict | None:
    try:
        text = subprocess.check_output(["git", "show", f"{ref}:{path}"], text=True, stderr=subprocess.DEVNULL)
    except Exception:
        return None
    return json.loads(text)


def type_name(schema) -> str:
    if not isinstance(schema, dict):
        return type(schema).__name__
    if "$ref" in schema:
        return schema["$ref"]
    t = schema.get("type")
    if isinstance(t, list):
        return "|".join(sorted(str(x) for x in t))
    if isinstance(t, str):
        if t == "array":
            return f"array[{type_name(schema.get('items', {}))}]"
        return t
    if "enum" in schema:
        return "enum"
    if "anyOf" in schema:
        return "anyOf"
    if "oneOf" in schema:
        return "oneOf"
    return "unknown"


def constraint_tightened(key: str, base, current) -> bool:
    if key in UPPER_BOUNDS:
        return isinstance(current, (int, float)) and (
            not isinstance(base, (int, float)) or current < base
        )
    if key in LOWER_BOUNDS:
        return isinstance(current, (int, float)) and (
            not isinstance(base, (int, float)) or current > base
        )
    return base != current


def walk(prefix: str, base, current, breaking: list[str]) -> None:
    if isinstance(base, dict) and isinstance(current, dict):
        base_keys = set(base.keys())
        cur_keys = set(current.keys())
        added = cur_keys - base_keys
        removed = base_keys - cur_keys
        for key in sorted(removed):
            if key in METADATA_KEYS or key in CONSTRAINT_KEYS:
                continue
            # A removed field with the same schema under a new name is a rename, not a
            # compatibility-safe removal: old readers will not recognise the new field.
            matches = [new for new in sorted(added) if base[key] == current[new]]
            if len(matches) == 1:
                breaking.append(f"RENAMED {prefix}.{key} -> {prefix}.{matches[0]}")
            elif prefix == "$.paths" and key.startswith("/"):
                breaking.append(f"PATH-REMOVED {prefix}.{key}")
            else:
                breaking.append(f"REMOVED {prefix}.{key}")
        for key in sorted(added & CONSTRAINT_KEYS):
            breaking.append(
                f"CONSTRAINT-CHANGED {prefix}.{key}: absent -> {current[key]!r}"
            )
        for key in sorted(base_keys & cur_keys):
            child = f"{prefix}.{key}"
            if key == "required":
                if isinstance(base[key], list) and isinstance(current[key], list):
                    for req in sorted(set(current[key]) - set(base[key])):
                        breaking.append(f"NEWLY-REQUIRED {child}[{req}]")
                continue
            if key == "type" or key == "$ref" or key == "enum":
                b = base[key]
                c = current[key]
                if b != c:
                    breaking.append(f"RETYPED {child}: {b!r} -> {c!r}")
                continue
            if key in CONSTRAINT_KEYS:
                b = base[key]
                c = current[key]
                if constraint_tightened(key, b, c):
                    breaking.append(f"CONSTRAINT-CHANGED {child}: {b!r} -> {c!r}")
                continue
            walk(child, base[key], current[key], breaking)
        return
    if isinstance(base, list) and isinstance(current, list):
        # For parameter lists, compare by name+in when possible.
        if all(isinstance(x, dict) and "name" in x and "in" in x for x in base + current):
            bmap = {(x.get("in"), x.get("name")): x for x in base}
            cmap = {(x.get("in"), x.get("name")): x for x in current}
            for k in sorted(bmap.keys() - cmap.keys()):
                breaking.append(f"REMOVED {prefix}[{k[0]}:{k[1]}]")
            for k in sorted(bmap.keys() & cmap.keys()):
                walk(f"{prefix}[{k[0]}:{k[1]}]", bmap[k], cmap[k], breaking)
            return
        for i, (b, c) in enumerate(zip(base, current)):
            walk(f"{prefix}[{i}]", b, c, breaking)
        for i in range(len(current), len(base)):
            breaking.append(f"REMOVED {prefix}[{i}]")
        return
    if base != current:
        # Value change at leaf: treat type-like changes as retyped, otherwise ignore
        # unless it's a contract-significant scalar we know about.
        if prefix.endswith(".type") or prefix.endswith(".$ref"):
            breaking.append(f"RETYPED {prefix}: {base!r} -> {current!r}")


def main() -> int:
    ap = argparse.ArgumentParser()
    ap.add_argument("--base", help="Base OpenAPI JSON file")
    ap.add_argument("--current", required=True, help="Current OpenAPI JSON file")
    ap.add_argument("--base-ref", default="origin/main", help="Git ref for base file when --base omitted")
    ap.add_argument("--base-path", default="tests/golden/openapi.json", help="Path in git ref for base file")
    args = ap.parse_args()

    current = load_json(args.current)
    if args.base:
        base = load_json(args.base)
    else:
        base = load_main(args.base_ref, args.base_path)
        if base is None:
            print(f"openapi-diff: no base found at {args.base_ref}:{args.base_path}; skipping")
            return 0

    breaking: list[str] = []
    walk("$", base, current, breaking)
    # de-dup
    seen = set()
    uniq = []
    for item in breaking:
        if item not in seen:
            seen.add(item)
            uniq.append(item)

    if not uniq:
        print("openapi-diff: no breaking changes detected")
        return 0
    print(f"openapi-diff: {len(uniq)} breaking change(s)")
    for item in uniq:
        print(item)
    return 1


if __name__ == "__main__":
    raise SystemExit(main())
