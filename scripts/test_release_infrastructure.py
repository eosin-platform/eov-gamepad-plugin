#!/usr/bin/env python3

from __future__ import annotations

import hashlib
import tempfile
import tomllib
import unittest
from pathlib import Path

from release_infrastructure import (
    ReleaseError,
    artifact_specs,
    render_manifest,
    stage_artifacts,
    validate_manifest,
    validate_source,
)


ROOT = Path(__file__).resolve().parents[1]


def current_version() -> str:
    with (ROOT / "Cargo.toml").open("rb") as manifest:
        return tomllib.load(manifest)["package"]["version"]


def current_environment() -> str:
    with (ROOT / "plugin.toml").open("rb") as manifest:
        return tomllib.load(manifest)["environment"]["version"]


VERSION = current_version()
ENVIRONMENT = current_environment()


class GamepadReleaseInfrastructureTests(unittest.TestCase):
    def test_source_metadata_is_consistent(self) -> None:
        # Read from the manifests, so a version bump never breaks the test.
        self.assertEqual(validate_source(ROOT, VERSION), ENVIRONMENT)
        with self.assertRaises(ReleaseError):
            validate_source(ROOT, "0.0.1")

    def test_manifest_uses_staged_hashes_and_immutable_urls(self) -> None:
        with tempfile.TemporaryDirectory() as temporary_directory:
            root = Path(temporary_directory)
            specs = artifact_specs("gamepad", VERSION)
            artifacts = root / "artifacts"
            for index, spec in enumerate(specs):
                source = artifacts / str(index) / spec.filename
                source.parent.mkdir(parents=True)
                source.write_bytes(spec.filename.encode("ascii"))
                digest = hashlib.sha256(source.read_bytes()).hexdigest()
                (source.parent / f"{spec.filename}.sha256").write_text(
                    f"{digest}  {spec.filename}\n", encoding="ascii"
                )
            staged = stage_artifacts(artifacts, root / "staging", specs)
            content = render_manifest(
                "eosin-platform/eov-gamepad-plugin",
                VERSION,
                ENVIRONMENT,
                specs,
                staged,
                "gamepad",
                "Gamepad",
                "Provides support for using gamepads as input devices",
            )
            validate_manifest(
                tomllib.loads(content),
                "eosin-platform/eov-gamepad-plugin",
                VERSION,
                ENVIRONMENT,
                specs,
            )
            self.assertNotIn("/latest/", content)
            self.assertIn("[manifest]", content)
            self.assertIn("[plugin]", content)
            self.assertIn('id = "gamepad"', content)

    def test_missing_artifact_fails(self) -> None:
        with tempfile.TemporaryDirectory() as temporary_directory:
            root = Path(temporary_directory)
            specs = artifact_specs("gamepad", VERSION)
            with self.assertRaisesRegex(ReleaseError, "missing release artifacts"):
                stage_artifacts(root, root / "staging", specs)


if __name__ == "__main__":
    unittest.main()
