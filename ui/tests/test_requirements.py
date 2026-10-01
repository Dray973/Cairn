"""The Python requirement files: the release build installs hashed exact pins
(`requirements-runtime.txt`), the development environment installs the same pins without
hashes plus the development tools (`requirements.txt`, `requirements-dev.txt`). Reads files
only."""

from __future__ import annotations

import re
from pathlib import Path

ROOT = Path(__file__).resolve().parents[2]
RUNTIME = ROOT / "requirements-runtime.txt"
DEVELOPMENT = ROOT / "requirements.txt"
TOOLS = ROOT / "requirements-dev.txt"
RUNTIME_PACKAGES = {"customtkinter", "darkdetect", "packaging", "pillow"}
PIN = re.compile(r"^([A-Za-z0-9_.-]+)==([0-9][0-9A-Za-z.+-]*)$")
HASH = re.compile(r"^--hash=sha256:[0-9a-f]{64}$")


def logical_lines(path: Path) -> list[str]:
    """Requirement lines of `path`: comments removed, continuation lines joined."""
    lines: list[str] = []
    pending = ""
    for raw in path.read_text(encoding="utf-8").splitlines():
        text = re.sub(r"(^|\s)#.*$", "", raw).rstrip()
        if text.endswith("\\"):
            pending += text[:-1] + " "
            continue
        text = (pending + text).strip()
        pending = ""
        if text:
            lines.append(text)
    assert not pending, f"{path.name} ends with a continuation"
    return lines


def pins(lines: list[str]) -> dict[str, str]:
    found: dict[str, str] = {}
    for line in lines:
        if line.startswith("-"):
            continue
        match = PIN.match(line.split()[0])
        assert match, f"not an exact pin: {line}"
        found[match.group(1).lower()] = match.group(2)
    return found


def test_runtime_pins_are_exact_and_hashed() -> None:
    lines = logical_lines(RUNTIME)
    assert set(pins(lines)) == RUNTIME_PACKAGES
    for line in lines:
        name, *options = line.split()
        assert PIN.match(name), line
        assert options, f"{name} has no hash"
        assert all(HASH.match(option) for option in options), line
    assert not any(line.startswith("-r") for line in lines), "nothing unhashed is pulled in"


def test_development_pins_equal_the_runtime_pins_without_hashes() -> None:
    text = DEVELOPMENT.read_text(encoding="utf-8")
    assert "--hash" not in text
    lines = logical_lines(DEVELOPMENT)
    assert pins(lines) == pins(logical_lines(RUNTIME))
    assert "-r requirements-dev.txt" in lines


def test_development_tools_are_listed_once() -> None:
    lines = logical_lines(TOOLS)
    names = [re.split(r"[<>=!~\s]", line, maxsplit=1)[0].lower() for line in lines]
    assert names == ["debugpy", "ruff", "pytest", "pytest-timeout"]
    assert not RUNTIME_PACKAGES & set(names)
    assert "--hash" not in TOOLS.read_text(encoding="utf-8")


def test_no_file_names_an_unused_package() -> None:
    for path in (RUNTIME, DEVELOPMENT, TOOLS):
        assert "maturin" not in path.read_text(encoding="utf-8").lower(), path.name
