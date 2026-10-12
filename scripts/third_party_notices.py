"""Writes the license notices of the Rust crates that Cairn's programs are built from.

Reads `cargo metadata --format-version 1 --locked` for the workspace and follows the normal
dependencies (not the build or development ones) of the shipped packages for
x86_64-pc-windows-msvc: Cairn.exe (`cairn`), optctl.exe and cairn-maintenance.exe (`optctl`) and
the engine module (`optimizer_engine`). For every crate that is not one of Cairn's own it writes
the name, version, license expression and repository, then the license and notice files the
crate ships in its package, read from cargo's registry folder. Identical texts are written once,
under the names of every crate that ships them. The file names no folder of this PC.

Run it from the repository with the development interpreter (it needs only the standard library
and cargo):

    .venv\\Scripts\\python.exe scripts\\third_party_notices.py --out build\\Rust-THIRD-PARTY.txt

`scripts\\build_release.ps1` writes the stage's `licenses\\Rust-THIRD-PARTY.txt` with it.

Exit code 0: written; 1: cargo metadata failed, or a crate ships no license file or one that
cannot be read (nothing is written then).
"""

from __future__ import annotations

import argparse
import json
import re
import subprocess
import sys
from collections.abc import Iterable, Mapping
from dataclasses import dataclass, field
from pathlib import Path
from typing import Any

ROOT = Path(__file__).resolve().parents[1]
TARGET = "x86_64-pc-windows-msvc"
# The workspace packages whose programs and libraries are shipped.
SHIPPED = ("cairn", "optctl", "optimizer_engine")
# File names of license and notice texts at the top of a crate's package.
LICENSE_FILE = re.compile(r"^(licen[cs]e|copying|notice|copyright)", re.IGNORECASE)
RULE = "=" * 78


class NoticeError(Exception):
    """The notices cannot be written; the message says why."""


@dataclass
class Crate:
    name: str
    version: str
    license: str
    repository: str
    folder: Path
    license_file: str | None = None
    texts: list[tuple[str, str]] = field(default_factory=list)

    @property
    def label(self) -> str:
        return f"{self.name} {self.version}"


def cargo_metadata(manifest: Path) -> dict[str, Any]:
    """The metadata of the workspace of `manifest`, resolved for TARGET from Cargo.lock."""
    command = [
        "cargo",
        "metadata",
        "--format-version",
        "1",
        "--locked",
        "--filter-platform",
        TARGET,
        "--manifest-path",
        str(manifest),
    ]
    try:
        done = subprocess.run(command, capture_output=True, check=False, timeout=600)
    except (OSError, subprocess.SubprocessError) as error:
        raise NoticeError(f"cargo metadata could not run: {error}") from error
    if done.returncode != 0:
        detail = done.stderr.decode("utf-8", errors="replace").strip().splitlines()
        raise NoticeError(f"cargo metadata failed: {detail[-1] if detail else done.returncode}")
    return json.loads(done.stdout.decode("utf-8"))


def shipped_crates(metadata: Mapping[str, Any], shipped: Iterable[str] = SHIPPED) -> list[Crate]:
    """The registry crates the shipped packages depend on through normal dependencies, by name
    and version."""
    packages = {package["id"]: package for package in metadata["packages"]}
    nodes = {node["id"]: node for node in metadata["resolve"]["nodes"]}
    wanted = set(shipped)
    roots = [pid for pid, package in packages.items() if package["name"] in wanted]
    missing = wanted - {packages[pid]["name"] for pid in roots}
    if missing:
        raise NoticeError(f"the workspace has no package {', '.join(sorted(missing))}")
    seen: set[str] = set()
    stack = list(roots)
    while stack:
        pid = stack.pop()
        if pid in seen:
            continue
        seen.add(pid)
        for dep in nodes[pid]["deps"]:
            if any(kind.get("kind") is None for kind in dep["dep_kinds"]):
                stack.append(dep["pkg"])
    crates = []
    for pid in seen:
        package = packages[pid]
        if package.get("source") is None:
            continue
        crates.append(
            Crate(
                name=package["name"],
                version=package["version"],
                license=package.get("license") or "(none declared)",
                repository=package.get("repository") or "-",
                folder=Path(package["manifest_path"]).parent,
                license_file=package.get("license_file"),
            )
        )
    return sorted(crates, key=lambda c: (c.name.lower(), version_key(c.version)))


def version_key(version: str) -> tuple[int, ...]:
    return tuple(int(part) if part.isdigit() else 0 for part in re.split(r"[.+-]", version))


def read_texts(crate: Crate) -> list[tuple[str, str]]:
    """The license and notice files of `crate` as (file name, text), by file name."""
    try:
        names = {
            entry.name
            for entry in crate.folder.iterdir()
            if entry.is_file() and LICENSE_FILE.match(entry.name)
        }
    except OSError as error:
        reason = error.strerror or str(error)
        raise NoticeError(f"{crate.label}: its package cannot be read ({reason})") from error
    if crate.license_file:
        names.add(crate.license_file)
    if not names:
        raise NoticeError(f"{crate.label} ships no license file")
    texts = []
    for name in sorted(names, key=str.lower):
        try:
            data = (crate.folder / name).read_bytes()
        except OSError as error:
            reason = error.strerror or str(error)
            raise NoticeError(f"{crate.label}: {name} cannot be read ({reason})") from error
        text = data.decode("utf-8", errors="replace").replace("\r\n", "\n").strip("\n")
        texts.append((Path(name).name, text))
    return texts


def render(crates: list[Crate], version: str) -> str:
    """The notices file."""
    lines = [
        f"Third-party notices: the Rust crates of Cairn {version}",
        "",
        "Cairn's programs (Cairn.exe, optctl.exe, cairn-maintenance.exe) and its engine module",
        "(optimizer_engine.pyd) are built from these crates of crates.io, as Cargo.lock resolves",
        f"them for {TARGET}, the crates that generate code while the programs are compiled",
        "included. The license and notice files of each crate follow, as its package ships them;",
        "a text that several crates ship is written once, under all of their names.",
        "",
        "Crates",
        "------",
        "",
    ]
    width = max((len(crate.label) for crate in crates), default=0)
    license_width = max((len(crate.license) for crate in crates), default=0)
    for crate in crates:
        lines.append(f"{crate.label.ljust(width)}  {crate.license.ljust(license_width)}  {crate.repository}")
    groups: dict[str, list[str]] = {}
    titles: dict[str, str] = {}
    for crate in crates:
        for name, text in crate.texts:
            groups.setdefault(text, []).append(crate.label)
            titles.setdefault(text, name)
    for text, labels in groups.items():
        lines += ["", "", RULE, f"{titles[text]}: {', '.join(labels)}", RULE, "", text]
    return "\n".join(lines) + "\n"


def workspace_version(metadata: Mapping[str, Any]) -> str:
    for package in metadata["packages"]:
        if package["name"] == "cairn" and package.get("source") is None:
            return str(package["version"])
    raise NoticeError("the workspace has no package cairn")


def notices(metadata: Mapping[str, Any]) -> tuple[str, int]:
    """The notices file for `metadata` and the number of crates it names."""
    crates = shipped_crates(metadata)
    for crate in crates:
        crate.texts = read_texts(crate)
    return render(crates, workspace_version(metadata)), len(crates)


def main(argv: list[str] | None = None) -> int:
    parser = argparse.ArgumentParser(description=__doc__.splitlines()[0])
    parser.add_argument("--out", type=Path, required=True, help="file to write")
    parser.add_argument(
        "--manifest-path", type=Path, default=ROOT / "Cargo.toml", help="the workspace's Cargo.toml"
    )
    parser.add_argument(
        "--metadata",
        type=Path,
        help="read the output of cargo metadata from this file instead of running cargo",
    )
    args = parser.parse_args(argv)
    try:
        if args.metadata is not None:
            metadata = json.loads(args.metadata.read_text(encoding="utf-8"))
        else:
            metadata = cargo_metadata(args.manifest_path)
        text, count = notices(metadata)
    except (NoticeError, OSError, ValueError, KeyError) as error:
        print(f"third_party_notices.py: {error}", file=sys.stderr)
        return 1
    args.out.parent.mkdir(parents=True, exist_ok=True)
    args.out.write_text(text, encoding="utf-8", newline="\n")
    print(f"wrote the notices of {count} crates to {args.out}")
    return 0


if __name__ == "__main__":
    sys.exit(main())
