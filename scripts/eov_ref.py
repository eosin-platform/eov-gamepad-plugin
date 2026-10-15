#!/usr/bin/env python3
"""Print the eov reference this plugin builds against.

The plugin compiles against `eov-plugin-api` from the eov repository
(`../../plugin_api`). CI and release builds check out the eov release named
by `[package.metadata.eov] ref` in Cargo.toml instead of eov's moving default
branch, so unreleased API changes cannot leak into plugin builds (GPD-8,
UPD-6). The `EOV_REF` environment variable overrides it (workflow input);
it is validated, never interpolated into a shell.

Output (for `$GITHUB_OUTPUT`): `ref=<ref>`
"""

from __future__ import annotations

import argparse
import os
import re
import sys
import tomllib
from pathlib import Path

REF_RE = re.compile(r"^[A-Za-z0-9][A-Za-z0-9._/-]{0,99}$")


class RefError(Exception):
    pass


def validate_ref(ref: str) -> str:
    if REF_RE.fullmatch(ref) is None or ".." in ref or ref.endswith((".lock", "/")):
        raise RefError(f"not a valid git ref: {ref!r}")
    return ref


def configured_ref(root: Path) -> str:
    try:
        with (root / "Cargo.toml").open("rb") as manifest:
            data = tomllib.load(manifest)
    except (OSError, tomllib.TOMLDecodeError) as error:
        raise RefError(f"cannot read Cargo.toml: {error}") from error
    ref = data.get("package", {}).get("metadata", {}).get("eov", {}).get("ref")
    if not isinstance(ref, str):
        raise RefError("Cargo.toml has no [package.metadata.eov] ref")
    return validate_ref(ref)


def resolve(root: Path, override: str | None) -> str:
    if override:
        return validate_ref(override.strip())
    return configured_ref(root)


def main(argv: list[str] | None = None) -> int:
    parser = argparse.ArgumentParser(description=__doc__.splitlines()[0])
    parser.add_argument("--root", type=Path, default=Path(__file__).resolve().parents[1])
    args = parser.parse_args(argv)
    try:
        ref = resolve(args.root, os.environ.get("EOV_REF"))
    except RefError as error:
        print(f"error: {error}", file=sys.stderr)
        return 1
    print(f"ref={ref}")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
