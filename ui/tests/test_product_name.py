"""The product is Cairn: no source, script, manifest or document of the repository says the old
name in user text or comments (the identifier `PCOptimizer` of internal names stays)."""

from __future__ import annotations

import re
from pathlib import Path

ROOT = Path(__file__).resolve().parents[2]
GLOBS = (
    "ui/optimizer/**/*.py",
    "ui/tests/**/*.py",
    "crates/**/*.rs",
    "crates/**/Cargo.toml",
    "Cargo.toml",
    "CMakeLists.txt",
    "telemetry/**/*.txt",
    "telemetry/**/*.cpp",
    "telemetry/**/*.h",
    "scripts/*.ps1",
    "scripts/*.py",
    ".vscode/*.json",
    "installer/*.iss",
    "README.md",
    "requirements*.txt",
    "ui/pyproject.toml",
)
# The old name, also split across comment lines; built from parts so this file does not match.
OLD_NAME = re.compile("PC" + r"[\s/!#;*]+" + "Optimizer")


def source_files() -> list[Path]:
    files: set[Path] = set()
    for pattern in GLOBS:
        files.update(p for p in ROOT.glob(pattern) if p.is_file())
    return sorted(files)


def test_the_scan_covers_the_sources() -> None:
    names = {p.relative_to(ROOT).as_posix() for p in source_files()}
    for expected in ("ui/optimizer/app.py", "crates/core/src/lib.rs", "Cargo.toml", "ui/pyproject.toml"):
        assert expected in names
    assert not any(n.startswith(("target/", "build/", ".venv/")) for n in names)


def test_the_pattern_finds_the_old_name() -> None:
    old = "PC" + " " + "Optimizer"
    assert OLD_NAME.search(f"Welcome to {old}.")
    assert OLD_NAME.search("PC\n/// Optimizer")
    assert not OLD_NAME.search("HKCU\\Software\\" + "PC" + "Optimizer\\SelfTest")


def test_no_source_uses_the_old_name() -> None:
    found = []
    for path in source_files():
        text = path.read_text(encoding="utf-8", errors="replace")
        for match in OLD_NAME.finditer(text):
            line = text.count("\n", 0, match.start()) + 1
            found.append(f"{path.relative_to(ROOT).as_posix()}:{line}")
    assert found == []
