#!/usr/bin/env python3

from __future__ import annotations

import os
import tarfile
import tempfile
import unittest
from pathlib import Path
from unittest import mock

import eov_ref
from package_eop import PackageError, library_names, package

ROOT = Path(__file__).resolve().parents[1]


def fake_plugin(root: Path, plugin_id: str = "gamepad") -> None:
    (root / "ui" / "icons").mkdir(parents=True)
    (root / "plugin.toml").write_text(
        f'id = "{plugin_id}"\nname = "Gamepad"\nversion = "1.0.0"\n', encoding="utf-8"
    )
    (root / "ui" / "window.slint").write_text("export component W {}\n", encoding="utf-8")
    (root / "ui" / "icons" / "pad.svg").write_text("<svg/>", encoding="utf-8")
    (root / "ui" / ".swap").write_text("junk", encoding="utf-8")


def translate(root: Path, catalogs: str = "translations", tags: tuple[str, ...] = ()) -> Path:
    """Declares the plugin translated and writes a catalog per tag, beside
    the template and files that are not catalogs."""
    with (root / "plugin.toml").open("a", encoding="utf-8") as manifest:
        manifest.write(f"\n[i18n]\ncatalogs = '{catalogs}'\n")
    folder = root / catalogs
    folder.mkdir(parents=True, exist_ok=True)
    (folder / ".gitkeep").write_text("", encoding="utf-8")
    (folder / "gamepad.pot").write_text('msgid ""\nmsgstr ""\n', encoding="utf-8")
    for tag in tags:
        messages = folder / tag / "LC_MESSAGES"
        messages.mkdir(parents=True)
        (messages / "gamepad.po").write_text(
            f'msgid ""\nmsgstr ""\n"Language: {tag}\\n"\n', encoding="utf-8"
        )
        (messages / "notes.txt").write_text("not a catalog", encoding="utf-8")
        (messages / "gamepad.pot").write_text("a template", encoding="utf-8")
        (folder / tag / "stray.po").write_text("not in LC_MESSAGES", encoding="utf-8")
    return folder


BASE_ENTRIES = ["plugin.toml", "libgamepad.so", "ui/icons/pad.svg", "ui/window.slint"]


class PackageTests(unittest.TestCase):
    def test_packages_the_manifest_library_and_ui(self) -> None:
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            fake_plugin(root)
            library = root / "libgamepad.so"
            library.write_bytes(b"\x7fELF")
            output = root / "dist" / "gamepad.eop"
            entries = package(root, library, output)
            self.assertEqual(
                entries,
                ["plugin.toml", "libgamepad.so", "ui/icons/pad.svg", "ui/window.slint"],
            )
            with tarfile.open(output) as archive:
                members = {m.name: m for m in archive.getmembers()}
            self.assertEqual(sorted(members), sorted(entries))
            self.assertEqual(members["libgamepad.so"].mode, 0o755)
            # Readable like any other new file (not private like mkstemp's).
            umask = os.umask(0)
            os.umask(umask)
            self.assertEqual(
                output.stat().st_mode & 0o777,
                0o644 & ~umask,
                "the package does not get the usual permissions of a new file",
            )
            self.assertEqual(members["plugin.toml"].uid, 0)
            self.assertFalse(any(name.startswith("ui/.") for name in members))

    def test_packages_are_reproducible(self) -> None:
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            fake_plugin(root)
            library = root / "gamepad.dll"
            library.write_bytes(b"MZ")
            first, second = root / "a.eop", root / "b.eop"
            with mock.patch.dict(os.environ, {"SOURCE_DATE_EPOCH": "1700000000"}):
                package(root, library, first)
                os.utime(library, (0, 0))
                package(root, library, second)
            self.assertEqual(first.read_bytes(), second.read_bytes())

    def test_the_library_must_be_named_after_the_id(self) -> None:
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            fake_plugin(root)
            library = root / "libsomething.so"
            library.write_bytes(b"")
            with self.assertRaisesRegex(PackageError, "named after the plugin id"):
                package(root, library, root / "x.eop")
            with self.assertRaisesRegex(PackageError, "missing plugin library"):
                package(root, root / "libgamepad.so", root / "x.eop")
            self.assertFalse((root / "x.eop").exists())
            self.assertEqual(library_names("gamepad")["macos"], "libgamepad.dylib")

    def test_the_catalogs_are_packaged_but_not_the_template(self) -> None:
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            fake_plugin(root)
            translate(root, tags=("pt-BR", "de"))
            library = root / "libgamepad.so"
            library.write_bytes(b"")
            output = root / "dist" / "gamepad.eop"
            entries = package(root, library, output)
            self.assertEqual(
                entries,
                BASE_ENTRIES
                + [
                    "translations/de/LC_MESSAGES/gamepad.po",
                    "translations/pt-BR/LC_MESSAGES/gamepad.po",
                ],
            )
            with tarfile.open(output) as archive:
                member = archive.getmember("translations/pt-BR/LC_MESSAGES/gamepad.po")
                self.assertEqual(member.mode, 0o644)
                catalog = archive.extractfile(member)
                assert catalog is not None
                self.assertIn(b"Language: pt-BR", catalog.read())

    def test_a_translated_plugin_without_catalogs_is_packaged(self) -> None:
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            fake_plugin(root)
            # Declared, with only the template and .gitkeep so far.
            translate(root)
            library = root / "libgamepad.so"
            library.write_bytes(b"")
            self.assertEqual(package(root, library, root / "a.eop"), BASE_ENTRIES)
            # Or without the folder at all.
            for path in sorted((root / "translations").iterdir()):
                path.unlink()
            (root / "translations").rmdir()
            self.assertEqual(package(root, library, root / "b.eop"), BASE_ENTRIES)

    def test_the_catalogs_folder_comes_from_the_manifest(self) -> None:
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            fake_plugin(root)
            translate(root, catalogs="locale", tags=("fr",))
            library = root / "libgamepad.so"
            library.write_bytes(b"")
            entries = package(root, library, root / "x.eop")
            self.assertEqual(entries, BASE_ENTRIES + ["locale/fr/LC_MESSAGES/gamepad.po"])

    def test_the_catalogs_folder_must_stay_inside_the_plugin(self) -> None:
        for bad in ["../elsewhere", "/etc", "a/../../b", "C:\\catalogs", "\\\\server\\share"]:
            with tempfile.TemporaryDirectory() as directory:
                root = Path(directory)
                fake_plugin(root)
                with (root / "plugin.toml").open("a", encoding="utf-8") as manifest:
                    manifest.write(f"\n[i18n]\ncatalogs = '{bad}'\n")
                library = root / "libgamepad.so"
                library.write_bytes(b"")
                with self.assertRaisesRegex(PackageError, "inside the plugin", msg=bad):
                    package(root, library, root / "x.eop")
                self.assertFalse((root / "x.eop").exists())

    def test_the_real_manifest_and_ui_are_packageable(self) -> None:
        with tempfile.TemporaryDirectory() as directory:
            library = Path(directory) / "libgamepad.so"
            library.write_bytes(b"")
            entries = package(ROOT, library, Path(directory) / "gamepad.eop")
            self.assertIn("ui/gamepad-window.slint", entries)
            self.assertIn("ui/icons/gamepad.svg", entries)
            # Catalogs, if any yet; never the template or .gitkeep.
            translations = [e for e in entries if e.startswith("translations/")]
            for entry in translations:
                self.assertRegex(entry, r"^translations/[^/]+/LC_MESSAGES/gamepad\.po$")


class EovRefTests(unittest.TestCase):
    def test_the_ref_comes_from_cargo_metadata(self) -> None:
        self.assertEqual(eov_ref.resolve(ROOT, None), eov_ref.configured_ref(ROOT))
        self.assertEqual(eov_ref.resolve(ROOT, " v1.2.3 "), "v1.2.3")

    def test_overrides_are_validated(self) -> None:
        for bad in ["v1; rm -rf /", "$(id)", "../x", "a..b", "-x", "x/"]:
            with self.assertRaises(eov_ref.RefError, msg=bad):
                eov_ref.resolve(ROOT, bad)


if __name__ == "__main__":
    unittest.main()
