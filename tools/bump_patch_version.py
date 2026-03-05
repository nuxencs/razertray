#!/usr/bin/env python3
"""Bump patch version in Cargo.toml and matching root package in Cargo.lock."""

from __future__ import annotations

import re
import sys
from pathlib import Path

PACKAGE_SECTION_RE = re.compile(r"(?ms)^\[package\]\n(?P<body>.*?)(?=^\[|\Z)")
VERSION_LINE_RE = re.compile(r'(?m)^version\s*=\s*"(?P<v>\d+)\.(?P<w>\d+)\.(?P<x>\d+)"\s*$')
LOCK_ROOT_RE = re.compile(
    r'(?ms)(\[\[package\]\]\nname = "razertray"\nversion = ")(?P<v>\d+)\.(?P<w>\d+)\.(?P<x>\d+)(?P<suffix>")'
)


def bump_patch(version: str) -> str:
    major, minor, patch = (int(part) for part in version.split("."))
    return f"{major}.{minor}.{patch + 1}"


def update_cargo_toml(text: str) -> tuple[str, str, str]:
    section_match = PACKAGE_SECTION_RE.search(text)
    if not section_match:
        raise RuntimeError("failed to locate [package] section in Cargo.toml")

    body_start = section_match.start("body")
    body_end = section_match.end("body")
    body = section_match.group("body")

    version_match = VERSION_LINE_RE.search(body)
    if not version_match:
        raise RuntimeError("failed to locate package version in Cargo.toml")

    old_version = ".".join(
        [version_match.group("v"), version_match.group("w"), version_match.group("x")]
    )
    new_version = bump_patch(old_version)
    new_body = VERSION_LINE_RE.sub(f'version = "{new_version}"', body, count=1)
    new_text = text[:body_start] + new_body + text[body_end:]
    return old_version, new_version, new_text


def update_cargo_lock(text: str, old_version: str, new_version: str) -> str:
    lock_match = LOCK_ROOT_RE.search(text)
    if not lock_match:
        raise RuntimeError('failed to locate root package "razertray" in Cargo.lock')

    found_version = ".".join(
        [lock_match.group("v"), lock_match.group("w"), lock_match.group("x")]
    )
    if found_version != old_version:
        raise RuntimeError(
            "Cargo.lock root package version mismatch: "
            f"expected {old_version}, found {found_version}"
        )

    return LOCK_ROOT_RE.sub(
        lambda match: f'{match.group(1)}{new_version}{match.group("suffix")}',
        text,
        count=1,
    )


def main() -> int:
    if len(sys.argv) != 3:
        print("usage: bump_patch_version.py <Cargo.toml> <Cargo.lock>", file=sys.stderr)
        return 2

    cargo_toml_path = Path(sys.argv[1]).resolve()
    cargo_lock_path = Path(sys.argv[2]).resolve()

    cargo_toml_text = cargo_toml_path.read_text(encoding="utf-8")
    old_version, new_version, new_cargo_toml_text = update_cargo_toml(cargo_toml_text)

    cargo_lock_text = cargo_lock_path.read_text(encoding="utf-8")
    new_cargo_lock_text = update_cargo_lock(cargo_lock_text, old_version, new_version)

    cargo_toml_path.write_text(new_cargo_toml_text, encoding="utf-8")
    cargo_lock_path.write_text(new_cargo_lock_text, encoding="utf-8")

    print(new_version)
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
