#!/usr/bin/env python3
"""Package the gamepad plugin as an eov plugin package (`.eop`).

An `.eop` is a tar archive with `plugin.toml`, the plugin library and the
`ui/` directory at its root. eov loads the library named after the manifest
id (`lib<id>.so`, `lib<id>.dylib` or `<id>.dll`), so a mismatched file name
is refused here rather than at load time.

The archive is reproducible: entries are sorted and carry fixed owners,
modes and the `SOURCE_DATE_EPOCH` timestamp (0 when unset).

    cargo build --release
    python3 scripts/package_eop.py --output dist/gamepad.eop
"""

from __future__ import annotations

import argparse
import io
import os
import pathlib
import sys
import tarfile
import tempfile
import tomllib


class PackageError(Exception):
    pass


def library_names(plugin_id: str) -> dict[str, str]:
    """The library file name eov expects, per platform."""
    return {
        "linux": f"lib{plugin_id}.so",
        "macos": f"lib{plugin_id}.dylib",
        "windows": f"{plugin_id}.dll",
    }


def current_platform() -> str:
    if sys.platform.startswith("win"):
        return "windows"
    if sys.platform == "darwin":
        return "macos"
    return "linux"


def read_manifest(plugin_root: pathlib.Path) -> dict[str, object]:
    path = plugin_root / "plugin.toml"
    try:
        with path.open("rb") as manifest:
            data = tomllib.load(manifest)
    except FileNotFoundError as error:
        raise PackageError(f"missing plugin manifest: {path}") from error
    except tomllib.TOMLDecodeError as error:
        raise PackageError(f"invalid plugin manifest {path}: {error}") from error
    for key in ("id", "name", "version"):
        if not isinstance(data.get(key), str) or not data[key]:
            raise PackageError(f"plugin.toml has no valid '{key}'")
    return data


def ui_files(plugin_root: pathlib.Path) -> list[pathlib.Path]:
    ui_dir = plugin_root / "ui"
    if not ui_dir.is_dir():
        raise PackageError(f"missing ui directory: {ui_dir}")
    files = []
    for path in sorted(ui_dir.rglob("*")):
        relative = path.relative_to(plugin_root)
        if any(part.startswith(".") or part == "__pycache__" for part in relative.parts):
            continue
        if path.is_symlink():
            raise PackageError(f"symbolic links cannot be packaged: {relative}")
        if path.is_file():
            files.append(path)
    if not any(path.suffix == ".slint" for path in files):
        raise PackageError("the ui directory contains no .slint file")
    return files


def add_file(
    archive: tarfile.TarFile, source: pathlib.Path, name: str, mode: int, mtime: int
) -> None:
    data = source.read_bytes()
    info = tarfile.TarInfo(name)
    info.size = len(data)
    info.mode = mode
    info.mtime = mtime
    info.uid = info.gid = 0
    info.uname = info.gname = ""
    archive.addfile(info, io.BytesIO(data))


def package(
    plugin_root: pathlib.Path, library: pathlib.Path | None, output: pathlib.Path
) -> list[str]:
    """Writes the package; returns its entries."""
    manifest = read_manifest(plugin_root)
    plugin_id = str(manifest["id"])
    names = library_names(plugin_id)
    if library is None:
        library = plugin_root / "target" / "release" / names[current_platform()]
    if not library.is_file():
        raise PackageError(f"missing plugin library: {library} (build it first)")
    if library.name not in names.values():
        raise PackageError(
            f"the library must be named after the plugin id '{plugin_id}' "
            f"({', '.join(sorted(names.values()))}), got {library.name}"
        )
    mtime = int(os.environ.get("SOURCE_DATE_EPOCH", "0"))
    entries: list[tuple[pathlib.Path, str, int]] = [
        (plugin_root / "plugin.toml", "plugin.toml", 0o644),
        (library, library.name, 0o755),
    ]
    entries += [
        (path, path.relative_to(plugin_root).as_posix(), 0o644)
        for path in ui_files(plugin_root)
    ]
    output.parent.mkdir(parents=True, exist_ok=True)
    # Write next to the output and rename, so a failed run leaves no partial
    # package where eov would pick it up.
    handle, temporary = tempfile.mkstemp(
        prefix=f".{output.name}.", suffix=".tmp", dir=output.parent
    )
    os.close(handle)
    try:
        with tarfile.open(temporary, "w", format=tarfile.PAX_FORMAT) as archive:
            for source, name, mode in entries:
                add_file(archive, source, name, mode, mtime)
        # mkstemp creates the file private; a package is meant to be shared.
        umask = os.umask(0)
        os.umask(umask)
        os.chmod(temporary, 0o644 & ~umask)
        os.replace(temporary, output)
    except BaseException:
        pathlib.Path(temporary).unlink(missing_ok=True)
        raise
    return [name for _, name, _ in entries]


def parse_args(argv: list[str] | None) -> argparse.Namespace:
    parser = argparse.ArgumentParser(
        description="Package the gamepad plugin as a .eop tarball"
    )
    parser.add_argument(
        "--plugin-root",
        type=pathlib.Path,
        default=pathlib.Path(__file__).resolve().parents[1],
        help="plugin repository root (default: this repository)",
    )
    parser.add_argument(
        "--library",
        type=pathlib.Path,
        help="compiled plugin library (default: target/release/<platform name>)",
    )
    parser.add_argument("--output", type=pathlib.Path, required=True, help="output .eop path")
    return parser.parse_args(argv)


def main(argv: list[str] | None = None) -> int:
    args = parse_args(argv)
    if args.output.suffix != ".eop":
        print("error: the output file must have the .eop extension", file=sys.stderr)
        return 1
    try:
        entries = package(
            args.plugin_root.resolve(),
            args.library.resolve() if args.library else None,
            args.output.resolve(),
        )
    except PackageError as error:
        print(f"error: {error}", file=sys.stderr)
        return 1
    print(f"wrote {args.output} ({len(entries)} files)")
    for entry in entries:
        print(f"  {entry}")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
