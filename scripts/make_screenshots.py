"""Takes the README screenshots in docs/screenshots.

The real window runs with the in-memory engine of the UI tests (`ui/tests/fake_engine.py`) and
the real telemetry DLL. The tweak catalog, the cleanup locations and the starter profiles are read
from the engine's Rust sources, so those sections show Cairn's own texts; everything else comes
from the tests' generic fixtures (TEST-PC, Contoso, Fabrikam, C:\\Users\\Test). No engine reads or
changes anything on this PC. The process helpers of `optimizer.system` (relaunching, bringing a
window to the front, message boxes, File Explorer, folders, URIs and file dialogs) and the
clipboard are replaced by stubs that fail the run when reached. As a second layer, every engine
result passes through a scrubber that replaces this PC's user and computer names, MAC addresses
and public IPv4 addresses; the Dashboard's process table lists only Windows' own processes; and
every text the window shows is checked for those identifiers before a picture is taken. The
demo's times are wall-clock times, so the pictures show them unchanged in every time zone.

The window runs on a separate desktop that is never shown, so nothing appears on the screen or
takes the keyboard focus, and each picture is the window's own drawing of itself (PrintWindow).
`--visible` shows the window on the screen instead and copies the pictures from there; nothing
may cover the window then. A run takes about a minute. Run it from the repository root with the
display at 100 % scaling, so that each picture is 1440 x 900 pixels:

    .venv\\Scripts\\python.exe scripts\\make_screenshots.py [--visible] [--out FOLDER]

The pictures are written (to docs/screenshots unless `--out` names another folder) only when
every section was shown without an error.
"""

from __future__ import annotations

import argparse
import ast
import ctypes
import gc
import ipaddress
import json
import os
import re
import sys
import time
import tkinter as tk
import traceback
from collections.abc import Callable, Iterator
from ctypes import wintypes
from dataclasses import dataclass
from datetime import UTC, datetime
from pathlib import Path
from tkinter import ttk
from typing import Any

ROOT = Path(__file__).resolve().parents[1]
UI = ROOT / "ui"
RUST = ROOT / "crates" / "core" / "src"
OUT = ROOT / "docs" / "screenshots"

# The engine guards of the test runs. Nothing here loads the real engine; if anything did, it
# could neither change this PC nor open the user's journal.
for _guard in (
    "OPTIMIZER_FORBID_RESTORE_POINT",
    "OPTIMIZER_FORBID_DRIVE_TESTS",
    "OPTIMIZER_FORBID_UPDATE_SEARCH",
    "OPTIMIZER_FORBID_APP_INSTALLS",
):
    os.environ[_guard] = "1"
os.environ["OPTIMIZER_DATA_DIR"] = str(ROOT / "target" / "test-data")
sys.path.insert(0, str(UI))

import customtkinter as ctk  # noqa: E402
from PIL import Image, ImageGrab  # noqa: E402

from optimizer import __version__, system, theme  # noqa: E402
from optimizer.app import App  # noqa: E402
from optimizer.bridge.engine import EngineBridge  # noqa: E402
from optimizer.bridge.telemetry import TEL_MAX_TOP_PROCESSES, Telemetry  # noqa: E402
from optimizer.features import system_info  # noqa: E402
from optimizer.widgets import monitor  # noqa: E402
from optimizer.widgets.dialogs import MessageDialog  # noqa: E402
from tests import (  # noqa: E402
    fake_health,
    fake_maintenance,
    fake_network,
    fake_profiles,
    fake_storage,
    fake_sysinfo,
)
from tests.fake_engine import FakeEngine  # noqa: E402

WIDTH, HEIGHT = 1440, 900
PLACE = "+240+60"
# The Dashboard's charts cover the last 60 s; its picture is taken once they are full.
TELEMETRY_SECONDS = 62.0
# Widgets whose text must be fully visible unless they sit in a scrollable frame.
FIT_TYPES = (ctk.CTkLabel, ctk.CTkButton, ctk.CTkSwitch, ctk.CTkCheckBox, ctk.CTkOptionMenu)
# The dialog that offers to restart Cairn elevated; confirming it would ask for a UAC prompt.
ADMIN_DIALOG_TITLE = "Administrator rights needed"
# The desktop the window runs on unless --visible: it is never shown.
SHOTS_DESKTOP = "CairnScreenshots"
GENERIC_ALL = 0x10000000
# PrintWindow flag: the window's whole content as drawn, also when it is not on the screen.
PW_RENDERFULLCONTENT = 0x2


class ShotError(RuntimeError):
    """The run cannot produce trustworthy pictures."""


# -- the engine's Rust sources -----------------------------------------------------------------

# Comments, char literals and string literals (plain and raw) of Rust source text.
_RUST_SKIPPED = re.compile(
    r"//[^\n]*|/\*.*?\*/|'(?:[^'\\\n]|\\.)'|r(#*)\".*?\"\1|\"(?:[^\"\\]|\\.)*\"", re.DOTALL
)
_RUST_STRING = re.compile(r"r(#*)\"(.*?)\"\1|\"((?:[^\"\\]|\\.)*)\"", re.DOTALL)
_RUST_ESCAPES = {"n": "\n", "t": "\t", "r": "\r", "0": "\0", "\\": "\\", '"': '"', "'": "'"}
_CLOSING = {"{": "}", "[": "]", "(": ")"}


def _unescape(match: re.Match[str]) -> str:
    escape = match.group(1)
    if escape[0] == "\n":
        # A line continuation: the line break and the next line's indentation are dropped.
        return ""
    if escape.startswith("u{"):
        return chr(int(escape[2:-1], 16))
    return _RUST_ESCAPES.get(escape, escape)


def rust_string(source: str) -> str:
    """The value of one Rust string literal, plain or raw."""
    match = _RUST_STRING.fullmatch(source.strip())
    if match is None:
        raise ValueError(f"not a string literal: {source.strip()[:60]!r}")
    if match.group(3) is None:
        return match.group(2)
    return re.sub(r"\\(\n\s*|u\{[0-9a-fA-F]+\}|.)", _unescape, match.group(3), flags=re.DOTALL)


def snake_variant(source: str) -> str:
    """`Enum::Variant` as its serde name: `RestartNeed::SignOut` is "sign_out"."""
    name = source.strip().rsplit("::", 1)[-1]
    return re.sub(r"(?<!^)(?=[A-Z])", "_", name).lower()


@dataclass(frozen=True)
class Rust:
    """Rust source text and the same text with comments and literals blanked out, so brackets
    and commas can be matched on the second while values are read from the first."""

    text: str
    masked: str

    @classmethod
    def read(cls, *parts: str) -> Rust:
        text = RUST.joinpath(*parts).read_text(encoding="utf-8").replace("\r\n", "\n")
        return cls(text, _RUST_SKIPPED.sub(lambda m: re.sub(r"[^\n]", " ", m.group(0)), text))

    def part(self, start: int, end: int) -> Rust:
        return Rust(self.text[start:end], self.masked[start:end])

    def closing(self, start: int) -> int:
        """Offset of the bracket that closes the one at `start`."""
        opening = self.masked[start]
        depth = 0
        for offset in range(start, len(self.masked)):
            char = self.masked[offset]
            if char == opening:
                depth += 1
            elif char == _CLOSING[opening]:
                depth -= 1
                if depth == 0:
                    return offset
        raise ValueError(f"unbalanced {opening!r}")

    def array(self, name: str) -> Rust:
        """The elements of the array constant or static `name`."""
        match = re.search(rf"\b(?:const|static)\s+{name}\s*:[^=]*=\s*&\[", self.masked)
        if match is None:
            raise ValueError(f"no array {name}")
        return self.part(match.end(), self.closing(match.end() - 1))

    def literals(self, name: str) -> list[Rust]:
        """The bodies of the struct literals `name { … }`."""
        return [
            self.part(m.end(), self.closing(m.end() - 1))
            for m in re.finditer(rf"\b{name}\s*\{{", self.masked)
        ]

    def function(self, name: str) -> Rust:
        """The body of function `name`."""
        match = re.search(rf"\bfn\s+{name}\b[^{{]*\{{", self.masked)
        if match is None:
            raise ValueError(f"no function {name}")
        return self.part(match.end(), self.closing(match.end() - 1))

    def _depth(self, end: int) -> int:
        depth = 0
        for char in self.masked[:end]:
            if char in "{[(":
                depth += 1
            elif char in "}])":
                depth -= 1
        return depth

    def _value_end(self, start: int) -> int:
        depth = 0
        for offset in range(start, len(self.masked)):
            char = self.masked[offset]
            if char in "{[(":
                depth += 1
            elif char in "}])":
                if depth == 0:
                    return offset
                depth -= 1
            elif char == "," and depth == 0:
                return offset
        return len(self.masked)

    def field(self, name: str) -> Rust:
        """The source of the value of field `name` at the top level of a struct body."""
        for match in re.finditer(rf"\b{name}\s*:(?!:)", self.masked):
            if self._depth(match.start()) == 0:
                return self.part(match.end(), self._value_end(match.end()))
        raise ValueError(f"no field {name}")

    def string(self, name: str) -> str:
        return rust_string(self.field(name).text)

    def word(self, name: str) -> str:
        return self.field(name).text.strip()

    def strings(self, name: str) -> list[str]:
        """The string literals of an array field such as `tweaks: &["a", "b"]`."""
        return [rust_string(m.group(0)) for m in _RUST_STRING.finditer(self.field(name).text)]

    def calls(self, function: str) -> list[list[str]]:
        """The arguments of every call of `function`, as source text."""
        calls = []
        for match in re.finditer(rf"\b{function}\s*\(", self.masked):
            body = self.part(match.end(), self.closing(match.end() - 1))
            args, start = [], 0
            while start < len(body.masked):
                end = body._value_end(start)
                if body.text[start:end].strip():
                    args.append(body.text[start:end].strip())
                start = end + 1
            calls.append(args)
        return calls

    def string_constants(self) -> dict[str, str]:
        """Every `const NAME: &str = "…";` of the file."""
        constants = {}
        for match in re.finditer(r"\bconst\s+(\w+)\s*:\s*&(?:'static\s+)?str\s*=", self.masked):
            end = self.masked.index(";", match.end())
            constants[match.group(1)] = rust_string(self.text[match.end() : end])
        return constants


@dataclass(frozen=True)
class CatalogTweak:
    id: str
    category: str
    title: str
    description: str
    risk: str
    default_on: bool
    restart: str
    requires: str | None
    # (hive, key path, value name, the value the tweak writes as source text, "" for text data)
    registry: tuple[tuple[str, str, str, str], ...]
    services: tuple[str, ...]
    tasks: tuple[str, ...]

    @property
    def targets(self) -> list[str]:
        """Journal targets, worded as the engine's catalog lists them."""
        return (
            [f"{hive}\\{key}\\{value}" for hive, key, value, _ in self.registry]
            + [f"service {name}" for name in self.services]
            + [f"scheduled task {path}" for path in self.tasks]
        )


@dataclass(frozen=True)
class CleanupLocation:
    id: str
    title: str
    description: str
    requires_admin: bool
    default_on: bool
    keeps_recent: bool


@dataclass(frozen=True)
class Starter:
    id: str
    name: str
    description: str
    tweaks: tuple[str, ...]
    apps: tuple[str, ...]


@dataclass(frozen=True)
class Sources:
    tweaks: tuple[CatalogTweak, ...]
    # Requirement serde name -> why a tweak with it is not offered.
    requirements: dict[str, str]
    cleanup: tuple[CleanupLocation, ...]
    starters: tuple[Starter, ...]


def _hive(source: str) -> str:
    hive = source.strip()
    if hive not in ("HKLM", "HKCU"):
        raise ValueError(f"unknown hive {hive!r}")
    return hive


def _text_or_constant(source: str, constants: dict[str, str]) -> str:
    source = source.strip()
    return constants[source] if re.fullmatch(r"\w+", source) else rust_string(source)


def read_catalog() -> tuple[tuple[CatalogTweak, ...], dict[str, str]]:
    """The tweaks of `debloat/catalog.rs` and its requirement texts."""
    source = Rust.read("debloat", "catalog.rs")
    constants = source.string_constants()
    array = source.array("TWEAKS")
    tweaks = []
    for body in array.literals("Tweak"):
        actions = body.field("actions")
        registry = [
            (
                _hive(args[0]),
                _text_or_constant(args[1], constants),
                _text_or_constant(args[2], constants),
                args[3] if function == "dword" else "",
            )
            for function in ("dword", "sz", "flags_sz_clear")
            for args in actions.calls(function)
        ]
        requires = body.word("requires")
        tweaks.append(
            CatalogTweak(
                id=body.string("id"),
                category=snake_variant(body.word("category")),
                title=body.string("title"),
                description=body.string("description"),
                risk=snake_variant(body.word("risk")),
                default_on=body.word("default_on") == "true",
                restart=snake_variant(body.word("restart")),
                requires=None if requires == "None" else snake_variant(requires.removesuffix(")")),
                registry=tuple(registry),
                services=tuple(rust_string(args[0]) for args in actions.calls("service")),
                tasks=tuple(_text_or_constant(args[0], constants) for args in actions.calls("disable_task")),
            )
        )
    if len(tweaks) != len(re.findall(r"\bTweak\s*\{", array.masked)) or len(tweaks) < 40:
        raise ShotError(f"read {len(tweaks)} tweaks from debloat/catalog.rs; its layout changed")
    requirements = {
        snake_variant(m.group(1)): rust_string(m.group(2))
        for m in re.finditer(
            r"Requirement::(\w+)\s*=>\s*\{?\s*(\"(?:[^\"\\]|\\.)*\")", source.function("missing_text").text
        )
    }
    return tuple(tweaks), requirements


def read_cleanup() -> tuple[CleanupLocation, ...]:
    """The cleanup locations of `cleanup/mod.rs`."""
    array = Rust.read("cleanup", "mod.rs").array("DEFS")
    return tuple(
        CleanupLocation(
            id=body.string("id"),
            title=body.string("title"),
            description=body.string("description"),
            requires_admin=body.word("requires_admin") == "true",
            default_on=body.word("default_on") == "true",
            keeps_recent=body.word("keeps_recent") == "true",
        )
        for body in array.literals("Def")
    )


def read_starters() -> tuple[Starter, ...]:
    """The starter profiles of `profiles/starters.rs`."""
    array = Rust.read("profiles", "starters.rs").array("STARTERS")
    return tuple(
        Starter(
            id=body.string("id"),
            name=body.string("name"),
            description=body.string("description"),
            tweaks=tuple(body.strings("tweaks")),
            apps=tuple(body.strings("apps")),
        )
        for body in array.literals("Starter")
    )


def read_sources() -> Sources:
    try:
        tweaks, requirements = read_catalog()
        sources = Sources(tweaks, requirements, read_cleanup(), read_starters())
    except (OSError, ValueError, KeyError) as exc:
        raise ShotError(f"could not read the engine's sources: {exc}") from exc
    if len(sources.cleanup) < 8 or len(sources.starters) < 3 or "office" not in sources.requirements:
        raise ShotError("the cleanup locations, starter profiles or requirements changed their layout")
    return sources


# -- demo data ---------------------------------------------------------------------------------


def utc(local: str) -> str:
    """Wall-clock time `local` ("2026-09-28 10:02") of this PC's time zone as an RFC 3339 UTC
    time, which the window shows as `local` again."""
    moment = datetime.strptime(local, "%Y-%m-%d %H:%M").astimezone()
    return moment.astimezone(UTC).isoformat(timespec="seconds").replace("+00:00", "Z")


# Times of the demo journal, oldest first.
WHEN = {
    "privacy": "2026-09-27 19:02",
    "gaming": "2026-09-27 19:04",
    "startup": "2026-09-28 10:12",
    "dns": "2026-09-28 10:14",
    "windows_update": "2026-09-28 10:20",
    "maintenance": "2026-09-28 11:02",
}
# Tweaks of the Gaming category that the demo journal holds besides Privacy Mode.
GAMING_APPLIED = ("gaming.game_mode", "gaming.background_recording")
# Rows of History's activity log, oldest first: (time, operation, target, outcome).
ACTIVITY = (
    ("2026-09-27 19:02", "set_service_start_type", "service DiagTrack", "applied"),
    ("2026-09-27 19:06", "cleanup", "user_temp", "cleaned"),
    ("2026-09-27 19:06", "cleanup", "windows_temp", "cleaned"),
    ("2026-09-27 19:06", "cleanup", "update_cache", "cleaned"),
    ("2026-09-27 19:06", "cleanup", "delivery_optimization", "cleaned"),
    ("2026-09-28 10:14", "set_dns_servers", "IPv4 DNS servers of Wi-Fi", "applied"),
    ("2026-09-28 10:40", "disk_speed_test", "C:", "started"),
    ("2026-09-28 10:42", "disk_speed_test", "C:", "succeeded"),
    ("2026-09-28 10:51", "app_upgrade", "Contoso.Editor", "started"),
    ("2026-09-28 10:51", "app_upgrade", "Contoso.Editor", "succeeded"),
    ("2026-09-28 10:52", "app_upgrade", "Fabrikam.Player", "started"),
    ("2026-09-28 10:53", "app_upgrade", "Fabrikam.Player", "succeeded"),
    ("2026-09-28 11:20", "tool", "dism_scan", "succeeded"),
)
# Times the fixtures' readings were taken.
SYSTEM_READ_AT = "2026-09-28 11:31"
CHECKUP_AT = "2026-09-28 11:32"
APPS_CHECKED_AT = "2026-09-28 11:33"
SPEED_TEST_AT = ("2026-09-28 10:40", "2026-09-28 10:42")
SCAN_AT = ("2026-09-28 11:34", "2026-09-28 11:35")
MAINTENANCE_RUN_AT = ("2026-09-27 12:00", "2026-09-27 12:14")
# Cleanup sizes, file counts and folders by location id.
CLEANUP_FOUND: dict[str, tuple[int, int, tuple[str, ...]]] = {
    "user_temp": (2_576_980_378, 18_204, ("C:\\Users\\Test\\AppData\\Local\\Temp",)),
    "windows_temp": (325_058_560, 1_204, ("C:\\Windows\\Temp",)),
    "update_cache": (1_181_116_006, 96, ("C:\\Windows\\SoftwareDistribution\\Download",)),
    "delivery_optimization": (
        650_117_120,
        42,
        (
            "C:\\Windows\\ServiceProfiles\\NetworkService\\AppData\\Local\\Microsoft\\Windows"
            "\\DeliveryOptimization\\Cache",
        ),
    ),
    "crash_dumps": (
        429_916_160,
        6,
        (
            "C:\\Users\\Test\\AppData\\Local\\CrashDumps",
            "C:\\Windows\\Minidump",
            "C:\\Windows\\LiveKernelReports",
        ),
    ),
    "error_reports": (
        50_331_648,
        31,
        (
            "C:\\ProgramData\\Microsoft\\Windows\\WER\\ReportArchive",
            "C:\\ProgramData\\Microsoft\\Windows\\WER\\ReportQueue",
        ),
    ),
    "thumbnail_cache": (230_686_720, 12, ("C:\\Users\\Test\\AppData\\Local\\Microsoft\\Windows\\Explorer",)),
    "recycle_bin": (1_825_361_101, 88, ("C:\\$Recycle.Bin",)),
    "shader_cache": (566_231_040, 3_412, ("C:\\Users\\Test\\AppData\\Local\\D3DSCache",)),
    "browser_chrome": (
        398_458_880,
        2_861,
        ("C:\\Users\\Test\\AppData\\Local\\Google\\Chrome\\User Data\\Default\\Cache",),
    ),
    "browser_edge": (
        272_629_760,
        1_940,
        ("C:\\Users\\Test\\AppData\\Local\\Microsoft\\Edge\\User Data\\Default\\Cache",),
    ),
    "browser_firefox": (
        118_489_088,
        904,
        ("C:\\Users\\Test\\AppData\\Local\\Mozilla\\Firefox\\Profiles\\abcd1234.default-release\\cache2",),
    ),
}
# Security checkup texts, worded as the engine words them on a well-kept PC.
SECURITY_CHECKS: dict[str, dict[str, Any]] = {
    "antivirus": {"summary": "Microsoft Defender Antivirus is on"},
    "security_intelligence": {"summary": "Updated today"},
    "threat_actions": {"summary": "Nothing waiting"},
    "remote_assistance": {
        "detail": "Remote Assistance lets someone you invite view or control this PC. Scammers often ask "
        "people to send an invitation."
    },
    "smb1": {"summary": "Not installed"},
    "windows_update": {"summary": "Updates installed 3 days ago"},
    "update_restart": {"summary": "No restart pending"},
    "encryption": {
        "title": "Device encryption",
        "detail": "If this PC is lost or stolen, anyone can read its files by moving the drive to another "
        "computer.",
        "fixes": [fake_health.uri_fix("Open Device encryption", "ms-settings:deviceencryption")],
    },
    "tpm": {"summary": "TPM 2.0"},
    "admin_account": {"summary": "Administrator, with Administrator protection"},
    "builtin_accounts": {"summary": "Administrator and Guest are disabled"},
    "auto_sign_in": {"summary": "Off"},
    "smartscreen": {"summary": "Warns before unrecognised apps run"},
    "file_extensions": {
        "detail": "With extensions hidden, a program named “invoice.pdf.exe” looks like “invoice.pdf”."
    },
}
# Generic names for the network fixture's adapters.
ADAPTER_NAMES = {
    fake_network.WIFI: {"description": "Contoso Wi-Fi 6E Adapter"},
    fake_network.ETHERNET: {"description": "Fabrikam 2.5GbE Controller"},
    fake_network.PROTON_VPN: {"name": "Contoso VPN", "description": "Contoso VPN Tunnel"},
}
# What the demo tool run prints: DISM checking the component store.
TOOL_ID = "dism_scan"
TOOL_OUTPUT = (
    "Deployment Image Servicing and Management tool",
    "Version: 10.0.26100.5074",
    "",
    "Image Version: 10.0.26200.6725",
    "",
    "[==========================100.0%==========================] No component store corruption detected.",
    "The operation completed successfully.",
)
TOOL_SUMMARY = "No component store damage was found."
TOOL_SECONDS = 408.0
# Folders of the space scan fixture that the Storage picture shows opened (tree item ids).
STORAGE_OPENED = ("n1", "n6", "n7")


def _upgrade(package: str, name: str, installed: str, available: str) -> dict[str, Any]:
    return {
        "id": package,
        "name": name,
        "installed": installed,
        "available": available,
        "source": "winget",
        "explicit_only": False,
        "selectable": True,
        "note": None,
    }


APP_UPGRADES = (
    _upgrade("Contoso.Editor", "Contoso Editor", "1.2.0", "1.3.0"),
    _upgrade("Fabrikam.Player", "Fabrikam Player", "2.0.0", "2.1.0"),
    _upgrade("Northwind.Notes", "Northwind Notes", "4.2.0", "4.3.1"),
    _upgrade("Litware.Dev", "Litware Dev", "3.8.2", "3.9.0"),
    _upgrade("Tailspin.Media", "Tailspin Media", "5.1.0", "5.2.0"),
    _upgrade("Contoso.Browser", "Contoso Browser", "128.0.1", "129.0.2"),
    _upgrade("Fabrikam.Chat", "Fabrikam Chat", "0.9.12", "0.9.14"),
)


class DemoEngine(FakeEngine):
    """The tests' FakeEngine with Cairn's tweak catalog, cleanup locations and starter profiles,
    and a journal that holds a few days of changes."""

    def __init__(self, sources: Sources, **options: Any) -> None:
        missing = {t.id: sources.requirements[t.requires] for t in sources.tweaks if t.requires == "office"}
        super().__init__(elevated=True, missing=missing, **options)
        self.sources = sources
        self.catalog_tweaks = {t.id: t for t in sources.tweaks}
        self.tweaks = [(t.id, t.category, t.default_on) for t in sources.tweaks]
        for adapter in self.network_adapters:
            adapter.update(ADAPTER_NAMES.get(adapter["id"], {}))

    def version(self) -> str:
        return __version__

    def scan(self) -> dict[str, Any]:
        return {**super().scan(), "duration_ms": 384}

    def _items(self) -> list[dict[str, Any]]:
        items = super()._items()
        for item in items:
            tweak = self.catalog_tweaks.get(item["id"])
            if tweak is not None:
                item.update(
                    title=tweak.title, description=tweak.description, risk=tweak.risk, restart=tweak.restart
                )
        return items

    def _targets(self, item_id: str) -> list[str]:
        tweak = self.catalog_tweaks.get(item_id)
        return tweak.targets if tweak is not None else super()._targets(item_id)

    def _profile_tweak_rows(self, profile: dict[str, Any]) -> list[dict[str, Any]]:
        rows = super()._profile_tweak_rows(profile)
        for row in rows:
            tweak = self.catalog_tweaks.get(row["key"].removeprefix("tweak:"))
            if tweak is not None:
                row.update(title=tweak.title, risk=tweak.risk, restart=tweak.restart)
        return rows

    def sysinfo_snapshot(self) -> dict[str, Any]:
        """The fixture snapshot, with the system drive and the security features as the other
        sections show them."""
        snapshot = super().sysinfo_snapshot()
        for section in snapshot["sections"]:
            rows = section["rows"] + [row for group in section["groups"] for row in group["rows"]]
            for row in rows:
                if row["label"] == "C: Windows":
                    row.update(
                        value="611 GB free of 952 GB  ·  NTFS",
                        level="normal",
                        note="Windows is installed here",
                        fraction=0.36,
                    )
                elif row["label"] == "Memory integrity":
                    row.update(value="On", level="good", note=None)
                elif row["label"] == "VBS":
                    row.update(value="Running", level="good", note=None)
        info = snapshot["info"]
        info["volumes"][0]["free_bytes"] = 611 * 1024**3
        info["security"].update(memory_integrity="running", vbs="running")
        snapshot["text"] = fake_sysinfo.sysinfo_text(snapshot["sections"])
        return snapshot

    def _tweak_rows(self) -> tuple[list[dict[str, Any]], list[dict[str, Any]], list[dict[str, Any]]]:
        """Registry, service and scheduled-task records of the applied tweaks."""
        registry: list[dict[str, Any]] = []
        services: list[dict[str, Any]] = []
        tasks: list[dict[str, Any]] = []
        for tweak_id in sorted(self.applied):
            tweak = self.catalog_tweaks.get(tweak_id)
            if tweak is None:
                continue
            when = utc(WHEN["gaming" if tweak_id in GAMING_APPLIED else "privacy"])
            common = {"session_id": 1, "recorded_at": when, "active": True, "reverted_at": None}
            for hive, key, value, written in tweak.registry:
                # A policy value is usually not set before; a switch held the other state.
                policy = key.lower().startswith("software\\policies")
                switch = {"0": 1, "1": 0}.get(written)
                original = None if policy or switch is None else {"type": "Dword", "value": switch}
                registry.append(
                    {
                        **common,
                        "target": f"{hive}\\{key}\\{value}",
                        "hive": hive,
                        "key_path": key,
                        "value_name": value,
                        "key_existed": not policy,
                        "value_existed": original is not None,
                        "original": original,
                    }
                )
            for name in tweak.services:
                services.append(
                    {
                        **common,
                        "target": f"service {name}",
                        "name": name,
                        "start_type": "Automatic",
                        "was_running": True,
                    }
                )
            for path in tweak.tasks:
                tasks.append(
                    {**common, "target": f"scheduled task {path}", "path": path, "was_enabled": True}
                )
        return registry, services, tasks

    def journal_export_json(self) -> str:
        export = json.loads(super().journal_export_json())
        registry, services, tasks = self._tweak_rows()
        for row in export["registry"]:
            # The fixture's stand-in records of tweaks live under `HKLM\Test`.
            if row["key_path"] == "Test":
                continue
            kind = "windows_update" if "windowsupdate" in row["key_path"].lower() else "startup"
            row["recorded_at"] = utc(WHEN[kind])
            registry.append(row)
        for rows in (registry, services, tasks):
            for index, row in enumerate(rows, start=1):
                row["id"] = index
        for row in export["dns"]:
            row["recorded_at"] = utc(WHEN["dns"])
        for row in export["task_definitions"]:
            row["recorded_at"] = utc(WHEN["maintenance"])
        export.update(registry=registry, services=services, scheduled_tasks=tasks)
        export["ops"] = [
            {
                "id": n,
                "session_id": None,
                "ts": utc(at),
                "op": op,
                "target": target,
                "outcome": outcome,
                "detail": None,
            }
            for n, (at, op, target, outcome) in enumerate(ACTIVITY, start=1)
        ]
        return json.dumps(export)

    def journal_summary(self) -> dict[str, Any]:
        summary = super().journal_summary()
        export = json.loads(DemoEngine.journal_export_json(self))
        summary["registry_active"] = len(export["registry"])
        summary["services_active"] = len(export["services"])
        summary["scheduled_tasks_active"] = len(export["scheduled_tasks"])
        return summary

    def cleanup_scan(self) -> dict[str, Any]:
        self._record("cleanup_scan")
        targets = []
        for location in self.sources.cleanup:
            size, files, paths = CLEANUP_FOUND.get(location.id, (0, 0, ()))
            targets.append(
                {
                    "id": location.id,
                    "title": location.title,
                    "description": location.description,
                    "requires_admin": location.requires_admin,
                    "default_on": location.default_on,
                    "bytes": size,
                    "files": files,
                    "blocked_reason": None,
                    "paths": list(paths),
                    "recent_files_kept": location.keeps_recent,
                }
            )
        return {"targets": targets, "total_bytes": sum(t["bytes"] for t in targets), "duration_ms": 96}


def use_product_texts(sources: Sources) -> None:
    """Gives the fixtures Cairn's starter profiles and cleanup descriptions, and the demo's
    times."""
    descriptions = {c.id: c.description for c in sources.cleanup}
    maintenance_targets = fake_maintenance.target_dicts

    def target_dicts() -> list[dict[str, Any]]:
        targets = maintenance_targets()
        for target in targets:
            target["description"] = descriptions.get(target["id"], target["description"])
        return targets

    fake_maintenance.target_dicts = target_dicts
    starters = []
    for starter in sources.starters:
        profile: dict[str, Any] = {
            "format": fake_profiles.FORMAT,
            "schema": fake_profiles.SCHEMA,
            "name": starter.name,
            "description": starter.description,
        }
        if starter.tweaks:
            profile["tweaks"] = list(starter.tweaks)
        if starter.apps:
            profile["apps"] = list(starter.apps)
        starters.append(
            {
                "id": starter.id,
                "name": starter.name,
                "description": starter.description,
                "counts": fake_profiles.counts_of(profile),
                "text": fake_profiles.profile_text(profile),
            }
        )
    fake_profiles.FAKE_STARTERS = tuple(starters)
    fake_sysinfo.SYSINFO_TAKEN_AT = utc(SYSTEM_READ_AT)
    fake_health.TAKEN_AT = utc(CHECKUP_AT)
    fake_storage.STARTED_AT, fake_storage.FINISHED_AT = (utc(t) for t in SCAN_AT)


def build_engine(sources: Sources) -> DemoEngine:
    """The demo engine, its journal filled the way the window itself would fill it."""
    use_product_texts(sources)
    started, finished = (utc(t) for t in SPEED_TEST_AT)
    run_started, run_ended = (utc(t) for t in MAINTENANCE_RUN_AT)
    engine = DemoEngine(
        sources,
        health_check_overrides=SECURITY_CHECKS,
        storage_history=[{**fake_storage.SPEED_RESULT, "started_at": started, "finished_at": finished}],
        upgrades=list(APP_UPGRADES),
        installed=[u["id"] for u in APP_UPGRADES],
        edition="Core",
        now=utc(APPS_CHECKED_AT),
        maintenance_task=dict(fake_maintenance.DEFAULTS),
        maintenance_runs=[
            fake_maintenance.run_dict(1, started_at=run_started, ended_at=run_ended, acknowledged=True)
        ],
        maintenance_task_result=0,
    )
    privacy = [
        t.id
        for t in sources.tweaks
        if t.category == "privacy" and t.default_on and t.id not in engine.missing
    ]
    engine.applied.update(privacy + list(GAMING_APPLIED))
    engine.startup_set_enabled("user_run:Discord", False)
    engine.network_set_dns(fake_network.WIFI, "cloudflare")
    engine.updates_wu_set("active_hours", [8, 23])
    return engine


# -- the second layer: scrubbing and checking -------------------------------------------------

MAC = re.compile(r"\b[0-9A-Fa-f]{2}(?:[-:][0-9A-Fa-f]{2}){5}\b")
IPV4 = re.compile(r"(?<![\w.])(?:\d{1,3}\.){3}\d{1,3}(?!\.?\d)")
SCRUBBED_MAC = "XX-XX-XX-XX-XX-XX"
SCRUBBED_IP = "203.0.113.53"
# Windows' own processes; the Dashboard's process table shows only these and this window's.
WINDOWS_PROCESSES = frozenset(
    name.lower()
    for name in (
        "System", "Registry", "Memory Compression", "Secure System", "smss.exe", "csrss.exe",
        "wininit.exe", "winlogon.exe", "services.exe", "lsass.exe", "lsaiso.exe", "svchost.exe",
        "dwm.exe", "explorer.exe", "sihost.exe", "taskhostw.exe", "ctfmon.exe", "RuntimeBroker.exe",
        "SearchHost.exe", "SearchIndexer.exe", "SearchProtocolHost.exe", "SearchFilterHost.exe",
        "StartMenuExperienceHost.exe", "ShellExperienceHost.exe", "ShellHost.exe", "TextInputHost.exe",
        "MsMpEng.exe", "NisSrv.exe", "MpDefenderCoreService.exe", "SecurityHealthService.exe",
        "SecurityHealthSystray.exe", "smartscreen.exe", "SgrmBroker.exe", "WmiPrvSE.exe",
        "audiodg.exe", "fontdrvhost.exe", "conhost.exe", "dllhost.exe", "spoolsv.exe", "WUDFHost.exe",
        "dasHost.exe", "MoUsoCoreWorker.exe", "TiWorker.exe", "TrustedInstaller.exe",
        "backgroundTaskHost.exe", "ApplicationFrameHost.exe", "SystemSettings.exe", "LockApp.exe",
        "Widgets.exe", "WidgetService.exe", "UserOOBEBroker.exe", "CompPkgSrv.exe",
    )
)  # fmt: skip


def _private_ip(text: str) -> bool:
    try:
        address = ipaddress.IPv4Address(text)
    except ValueError:
        # Not an address at all, such as a version number with a part above 255.
        return True
    return address.is_private or address.is_loopback or address.is_link_local


class Scrubber:
    """Finds and replaces this PC's identifiers in text: the user and computer names (read
    from the environment at run time), MAC addresses and public IPv4 addresses. Values the
    demo data itself holds (the fixture adapters' MAC addresses, the public DNS presets) pass."""

    def __init__(self, allowed: set[str]) -> None:
        self.allowed = allowed
        self.names: list[tuple[str, re.Pattern[str], str]] = []
        for kind, variable, replacement in (
            ("user name", "USERNAME", "Test"),
            ("computer name", "COMPUTERNAME", "TEST-PC"),
        ):
            value = os.environ.get(variable, "").strip()
            if len(value) >= 3:
                pattern = re.compile(rf"(?<![A-Za-z0-9]){re.escape(value)}(?![A-Za-z0-9])", re.IGNORECASE)
                self.names.append((kind, pattern, replacement))

    def _foreign_mac(self, text: str) -> bool:
        return text.upper().replace(":", "-") not in self.allowed

    def _foreign_ip(self, text: str) -> bool:
        return text not in self.allowed and not _private_ip(text)

    def hits(self, text: str) -> list[str]:
        """Kinds of identifiers in `text`; the values themselves are never reported."""
        found = [kind for kind, pattern, _ in self.names if pattern.search(text)]
        if any(self._foreign_mac(m.group(0)) for m in MAC.finditer(text)):
            found.append("MAC address")
        if any(self._foreign_ip(m.group(0)) for m in IPV4.finditer(text)):
            found.append("public IPv4 address")
        return found

    def text(self, text: str) -> str:
        for _, pattern, replacement in self.names:
            text = pattern.sub(replacement, text)
        text = MAC.sub(lambda m: SCRUBBED_MAC if self._foreign_mac(m.group(0)) else m.group(0), text)
        return IPV4.sub(lambda m: SCRUBBED_IP if self._foreign_ip(m.group(0)) else m.group(0), text)

    def value(self, value: Any) -> Any:
        if isinstance(value, str):
            return self.text(value)
        if isinstance(value, dict):
            return {k: self.value(v) for k, v in value.items()}
        if isinstance(value, list):
            return [self.value(v) for v in value]
        if isinstance(value, tuple):
            return tuple(self.value(v) for v in value)
        return value


def scrub_engine(engine: DemoEngine, scrubber: Scrubber) -> None:
    """Passes every result of the engine's public functions through `scrubber`."""
    for name in dir(engine):
        function = getattr(engine, name)
        if name.startswith("_") or not callable(function):
            continue

        def scrubbed(*args: Any, _function: Callable[..., Any] = function, **kwargs: Any) -> Any:
            return scrubber.value(_function(*args, **kwargs))

        setattr(engine, name, scrubbed)


def show_windows_processes_only(scrubber: Scrubber) -> None:
    """The Dashboard's process table lists Windows' own processes and this window's only."""
    top_processes = monitor.top_processes
    own = os.getpid()

    def windows_processes(snapshot: Any) -> list[Any]:
        return [
            p
            for p in top_processes(snapshot)
            if (p.pid == own or p.name.lower() in WINDOWS_PROCESSES) and not scrubber.hits(p.name)
        ]

    monitor.top_processes = windows_processes


class WideTelemetry(Telemetry):
    """The telemetry DLL with its longest process list, so Windows' own processes still fill
    the Dashboard's table once every other one is left out."""

    def start(
        self,
        sample_interval_ms: int = 16,
        process_interval_ms: int = 500,
        top_process_count: int = TEL_MAX_TOP_PROCESSES,
    ) -> None:
        super().start(sample_interval_ms, process_interval_ms, top_process_count)


def process_helpers() -> tuple[str, ...]:
    """The `optimizer.system` helpers the UI tests replace with failing stubs
    (`PROCESS_HELPERS` in ui/tests/conftest.py), read from that file so both lists stay one."""
    tree = ast.parse((UI / "tests" / "conftest.py").read_text(encoding="utf-8"))
    for node in tree.body:
        if isinstance(node, ast.Assign) and any(
            isinstance(t, ast.Name) and t.id == "PROCESS_HELPERS" for t in node.targets
        ):
            names = tuple(ast.literal_eval(node.value))
            missing = [n for n in names if not callable(getattr(system, n, None))]
            if missing:
                raise ShotError(f"optimizer.system has no {', '.join(missing)}")
            return names
    raise ShotError("PROCESS_HELPERS is missing from ui/tests/conftest.py")


class Refusals:
    """Stubs for the process helpers and the clipboard: each records its name and raises."""

    def __init__(self) -> None:
        self.reached: list[str] = []

    def stub(self, name: str) -> Callable[..., Any]:
        def refuse(*_args: Any, **_kwargs: Any) -> Any:
            self.reached.append(name)
            raise RuntimeError(f"the screenshot run reached {name}")

        return refuse

    def install(self) -> None:
        for name in process_helpers():
            setattr(system, name, self.stub(name))
        system_info.write_clipboard_text = self.stub("write_clipboard_text")


def _widgets(widget: tk.Misc) -> Iterator[tk.Misc]:
    for child in widget.winfo_children():
        yield child
        yield from _widgets(child)


def _texts(widget: tk.Misc) -> list[str]:
    """The texts `widget` shows."""
    texts = []
    try:
        texts.append(str(widget.cget("text")))
    except (tk.TclError, ValueError, AttributeError):
        pass
    if isinstance(widget, tk.Text):
        texts.append(widget.get("1.0", "end"))
    elif isinstance(widget, ttk.Treeview):
        stack = list(widget.get_children(""))
        while stack:
            item = stack.pop()
            texts.append(str(widget.item(item, "text")))
            texts.extend(str(v) for v in widget.item(item, "values"))
            stack.extend(widget.get_children(item))
    elif isinstance(widget, ctk.CTkOptionMenu):
        texts.append(str(widget.get()))
        texts.extend(str(v) for v in widget.cget("values") or ())
    return texts


def _fit_widgets(widget: tk.Misc) -> Iterator[tk.Misc]:
    """The widgets under `widget` whose text must fit, skipping scrollable frames' content."""
    for child in widget.winfo_children():
        if isinstance(child, ctk.CTkScrollableFrame):
            continue
        if isinstance(child, FIT_TYPES):
            yield child
        else:
            yield from _fit_widgets(child)


def _short_text(widget: tk.Misc) -> str:
    try:
        return str(widget.cget("text"))[:50]
    except (tk.TclError, ValueError):
        return ""


def layout_problems(app: App, section: str) -> list[str]:
    """Shown labels, buttons, switches, check boxes and option menus of `section`, outside
    scrollable frames, that stick out of the section or get less width than their text needs."""
    frame = app.section_frame(section)
    left, top = frame.winfo_rootx(), frame.winfo_rooty()
    width, height = frame.winfo_width(), frame.winfo_height()
    problems = []
    for widget in _fit_widgets(frame):
        if not widget.winfo_ismapped():
            continue
        x0, y0 = widget.winfo_rootx() - left, widget.winfo_rooty() - top
        x1, y1 = x0 + widget.winfo_width(), y0 + widget.winfo_height()
        text = _short_text(widget)
        if x0 < -1 or y0 < -1 or x1 > width + 1 or y1 > height + 1:
            problems.append(f"{type(widget).__name__} {text!r} sticks out of {section}")
        inner = widget._label if isinstance(widget, ctk.CTkLabel) else widget
        if inner.winfo_reqwidth() > widget.winfo_width() + 1:
            problems.append(f"{type(widget).__name__} {text!r} is cut off")
    return problems


def open_dialogs(app: App) -> list[tk.Toplevel]:
    return [w for w in app.winfo_children() if isinstance(w, tk.Toplevel) and w.winfo_exists()]


def check_window(app: App, section: str, scrubber: Scrubber) -> None:
    """Refuses a picture of a window with a dialog, an error, a clipped label or an identifier
    of this PC."""
    dialogs = open_dialogs(app)
    if dialogs:
        raise ShotError(f"{section}: a dialog is open: {dialogs[0].title()!r}")
    if app.errors:
        raise ShotError(f"{section}: the window reported errors")
    if str(app.status_message.cget("text_color")) in (theme.CRITICAL, theme.WARNING):
        raise ShotError(f"{section}: the status bar warns: {app.status_message.cget('text')!r}")
    problems = layout_problems(app, section)
    if problems:
        raise ShotError(f"{section}: " + "; ".join(problems))
    for widget in _widgets(app):
        for text in _texts(widget):
            hits = scrubber.hits(text)
            if hits:
                raise ShotError(f"{section}: {type(widget).__name__} shows a {hits[0]} of this PC")


# -- driving the window -----------------------------------------------------------------------


def pump(app: App, seconds: float, until: Callable[[], bool] | None = None, what: str = "") -> None:
    deadline = time.perf_counter() + seconds
    while time.perf_counter() < deadline:
        app.update()
        if until is not None and until():
            return
        time.sleep(0.004)
    if until is not None:
        raise ShotError(f"timed out waiting for {what}")


def idle(app: App) -> bool:
    return app.engine is not None and not app.engine.busy and not app._busy


def settle(app: App) -> None:
    """Waits until no engine call is queued or running, then for the redraws."""
    pump(app, 15.0, until=lambda: idle(app), what="the engine")
    pump(app, 0.8)


def show(app: App, section: str) -> None:
    app.show_section(section)
    settle(app)


def confirm(app: App, what: str) -> None:
    """Confirms the dialog the window opened for `what`; never the administrator relaunch."""
    pump(app, 15.0, until=lambda: bool(open_dialogs(app)), what=f"the dialog of {what}")
    dialog = open_dialogs(app)[-1]
    if not isinstance(dialog, MessageDialog) or dialog.title_text == ADMIN_DIALOG_TITLE:
        raise ShotError(f"{what} opened an unexpected dialog: {dialog.title()!r}")
    dialog.confirm_button.invoke()
    pump(app, 15.0, until=lambda: not open_dialogs(app), what=f"the dialog of {what} to close")


def prepare_security(app: App, engine: DemoEngine) -> None:
    app.show_section("Security")
    pump(app, 15.0, until=lambda: app._update_scan_active, what="the Windows Update check")
    engine.update_scan_finish(updates=[])
    pump(app, 15.0, until=lambda: not app._update_scan_active, what="the end of the Windows Update check")
    settle(app)
    button = app.security_panel.passed_button
    if str(button.cget("text")).startswith("Show"):
        button.invoke()
        settle(app)


def prepare_optimize(app: App, engine: DemoEngine) -> None:
    pump(app, 15.0, until=lambda: app._last_scan is not None, what="the scan")
    show(app, "Optimize")


def prepare_cleanup(app: App, engine: DemoEngine) -> None:
    app.show_section("Cleanup")
    pump(app, 15.0, until=lambda: app.cleanup_panel.scanned, what="the cleanup measurement")
    settle(app)


def prepare_storage(app: App, engine: DemoEngine) -> None:
    app.show_section("Storage")
    panel = app.storage_panel
    pump(app, 15.0, until=lambda: panel.loaded, what="the drives")
    panel.select_page("Space")
    settle(app)
    panel.space.scan_button.invoke()
    pump(app, 15.0, until=lambda: app._storage_job is not None, what="the scan to start")
    engine.storage_finish(app._storage_job)
    pump(app, 15.0, until=lambda: app._storage_scan_job is not None, what="the scan result")
    settle(app)
    # Users, the user's folder and its Videos folder, as double-clicks open them.
    for folder in STORAGE_OPENED:
        panel.space.tree.open_folder(folder)
        settle(app)


def prepare_updates(app: App, engine: DemoEngine) -> None:
    app.show_section("Updates")
    pump(app, 15.0, until=lambda: app._updates_job is not None, what="the check for app updates")
    engine.updates_finish_scan(app._updates_job)
    pump(app, 15.0, until=lambda: app._updates_job is None, what="the end of the check")
    settle(app)


def prepare_tools(app: App, engine: DemoEngine) -> None:
    show(app, "Tools")
    panel = app.tools_panel
    pump(app, 15.0, until=lambda: panel.loaded, what="the tools")
    panel.rows[TOOL_ID].run_button.invoke()
    confirm(app, "the tool")
    pump(app, 15.0, until=lambda: app._tool_job is not None and idle(app), what="the tool to start")
    job = app._tool_job
    engine.tool_emit(job, *TOOL_OUTPUT, progress=100.0)
    pump(app, 1.0)
    engine.tool_exit(job, 0)
    # The run took as long as a real one.
    engine._tool_jobs[job].started = time.monotonic() - TOOL_SECONDS
    engine.tool_finish(job, summary=TOOL_SUMMARY)
    pump(app, 15.0, until=lambda: app._tool_job is None, what="the tool to finish")
    settle(app)


def prepare_history(app: App, engine: DemoEngine) -> None:
    app.show_section("History")
    pump(app, 15.0, until=lambda: app.history_panel.row_count > 0, what="the journal")
    settle(app)


def prepare_profiles(app: App, engine: DemoEngine) -> None:
    show(app, "Profiles")
    panel = app.profiles_panel
    card = next(c for c in panel.starter_cards if c.starter_id == "gaming")
    card.preview_button.invoke()
    pump(
        app,
        15.0,
        until=lambda: panel.mode == "plan" and idle(app) and not app._profile_loading,
        what="the starter preview",
    )
    settle(app)


def prepare_dashboard(app: App, engine: DemoEngine) -> None:
    # A scan as the System Scan button starts it, so the status bar reports one.
    app.start_scan()
    settle(app)
    show(app, "Dashboard")
    pump(app, 1.0)


# (picture name, section, preparation); the Dashboard comes last, once its charts are full.
SHOTS: tuple[tuple[str, str, Callable[[App, DemoEngine], None]], ...] = (
    ("system", "System", lambda app, _engine: show(app, "System")),
    ("security", "Security", prepare_security),
    ("optimize", "Optimize", prepare_optimize),
    ("network", "Network", lambda app, _engine: show(app, "Network")),
    ("cleanup", "Cleanup", prepare_cleanup),
    ("updates", "Updates", prepare_updates),
    ("storage", "Storage", prepare_storage),
    ("maintenance", "Maintenance", lambda app, _engine: show(app, "Maintenance")),
    ("tools", "Tools", prepare_tools),
    ("history", "History", prepare_history),
    ("profiles", "Profiles", prepare_profiles),
    ("dashboard", "Dashboard", prepare_dashboard),
)


def grab_screen(app: App) -> Image.Image:
    """The window's client area as the screen shows it (--visible)."""
    app.lift()
    app.focus_force()
    pump(app, 0.5)
    x, y = app.winfo_rootx(), app.winfo_rooty()
    return ImageGrab.grab(bbox=(x, y, x + app.winfo_width(), y + app.winfo_height()), all_screens=True)


class _BitmapInfoHeader(ctypes.Structure):
    _fields_ = [
        ("biSize", wintypes.DWORD),
        ("biWidth", wintypes.LONG),
        ("biHeight", wintypes.LONG),
        ("biPlanes", wintypes.WORD),
        ("biBitCount", wintypes.WORD),
        ("biCompression", wintypes.DWORD),
        ("biSizeImage", wintypes.DWORD),
        ("biXPelsPerMeter", wintypes.LONG),
        ("biYPelsPerMeter", wintypes.LONG),
        ("biClrUsed", wintypes.DWORD),
        ("biClrImportant", wintypes.DWORD),
    ]


class _BitmapInfo(ctypes.Structure):
    _fields_ = [("bmiHeader", _BitmapInfoHeader), ("bmiColors", wintypes.DWORD * 1)]


def _user32() -> Any:
    user32 = ctypes.WinDLL("user32", use_last_error=True)
    user32.CreateDesktopW.argtypes = [
        wintypes.LPCWSTR,
        wintypes.LPCWSTR,
        ctypes.c_void_p,
        wintypes.DWORD,
        wintypes.DWORD,
        ctypes.c_void_p,
    ]
    user32.CreateDesktopW.restype = wintypes.HANDLE
    user32.SetThreadDesktop.argtypes = [wintypes.HANDLE]
    user32.SetThreadDesktop.restype = wintypes.BOOL
    user32.GetWindowRect.argtypes = [wintypes.HWND, ctypes.POINTER(wintypes.RECT)]
    user32.GetWindowRect.restype = wintypes.BOOL
    user32.GetDC.argtypes = [wintypes.HWND]
    user32.GetDC.restype = wintypes.HDC
    user32.ReleaseDC.argtypes = [wintypes.HWND, wintypes.HDC]
    user32.ReleaseDC.restype = ctypes.c_int
    user32.PrintWindow.argtypes = [wintypes.HWND, wintypes.HDC, wintypes.UINT]
    user32.PrintWindow.restype = wintypes.BOOL
    return user32


def _gdi32() -> Any:
    gdi32 = ctypes.WinDLL("gdi32", use_last_error=True)
    gdi32.CreateCompatibleDC.argtypes = [wintypes.HDC]
    gdi32.CreateCompatibleDC.restype = wintypes.HDC
    gdi32.CreateCompatibleBitmap.argtypes = [wintypes.HDC, ctypes.c_int, ctypes.c_int]
    gdi32.CreateCompatibleBitmap.restype = wintypes.HBITMAP
    gdi32.SelectObject.argtypes = [wintypes.HDC, wintypes.HGDIOBJ]
    gdi32.SelectObject.restype = wintypes.HGDIOBJ
    gdi32.DeleteObject.argtypes = [wintypes.HGDIOBJ]
    gdi32.DeleteObject.restype = wintypes.BOOL
    gdi32.DeleteDC.argtypes = [wintypes.HDC]
    gdi32.DeleteDC.restype = wintypes.BOOL
    gdi32.GetDIBits.argtypes = [
        wintypes.HDC,
        wintypes.HBITMAP,
        wintypes.UINT,
        wintypes.UINT,
        ctypes.c_void_p,
        ctypes.POINTER(_BitmapInfo),
        wintypes.UINT,
    ]
    gdi32.GetDIBits.restype = ctypes.c_int
    return gdi32


def use_hidden_desktop() -> None:
    """Moves this thread to a desktop that is never shown, so the window and its dialogs open
    there: nothing appears on the screen or takes the keyboard focus. A thread can move only
    while it owns no window, so this runs before the window is created. The desktop stays open
    for the life of the process and goes with it."""
    user32 = _user32()
    desktop = user32.CreateDesktopW(SHOTS_DESKTOP, None, None, 0, GENERIC_ALL, None)
    if not desktop or not user32.SetThreadDesktop(desktop):
        raise ShotError(
            f"the hidden desktop could not be used (error {ctypes.get_last_error()}); "
            "run with --visible to take the pictures on the screen"
        )


def grab_window(app: App) -> Image.Image:
    """The window's client area as the window draws itself (PrintWindow), which works on a
    desktop that is never shown."""
    pump(app, 0.5)
    user32, gdi32 = _user32(), _gdi32()
    hwnd = int(app.wm_frame(), 16)
    rect = wintypes.RECT()
    if not user32.GetWindowRect(hwnd, ctypes.byref(rect)):
        raise ShotError(f"the window's position could not be read (error {ctypes.get_last_error()})")
    width, height = rect.right - rect.left, rect.bottom - rect.top
    # A top-down 32-bit copy of the bitmap: blue, green, red and an unused byte per pixel.
    info = _BitmapInfo()
    info.bmiHeader.biSize = ctypes.sizeof(_BitmapInfoHeader)
    info.bmiHeader.biWidth, info.bmiHeader.biHeight = width, -height
    info.bmiHeader.biPlanes, info.bmiHeader.biBitCount = 1, 32
    pixels = ctypes.create_string_buffer(width * height * 4)
    screen = user32.GetDC(None)
    if not screen:
        raise ShotError("no device context for the pictures")
    memory = bitmap = None
    try:
        memory = gdi32.CreateCompatibleDC(screen)
        bitmap = gdi32.CreateCompatibleBitmap(screen, width, height)
        if not memory or not bitmap:
            raise ShotError(f"no {width}x{height} bitmap for the picture")
        previous = gdi32.SelectObject(memory, bitmap)
        try:
            if not user32.PrintWindow(hwnd, memory, PW_RENDERFULLCONTENT):
                raise ShotError(f"the window could not draw itself (error {ctypes.get_last_error()})")
            if gdi32.GetDIBits(memory, bitmap, 0, height, pixels, ctypes.byref(info), 0) != height:
                raise ShotError("the picture could not be read from its bitmap")
        finally:
            gdi32.SelectObject(memory, previous)
    finally:
        if bitmap:
            gdi32.DeleteObject(bitmap)
        if memory:
            gdi32.DeleteDC(memory)
        user32.ReleaseDC(None, screen)
    image = Image.frombuffer("RGB", (width, height), pixels, "raw", "BGRX", 0, 1)
    left, top = app.winfo_rootx() - rect.left, app.winfo_rooty() - rect.top
    return image.crop((left, top, left + app.winfo_width(), top + app.winfo_height()))


def run(
    grab: Callable[[App], Image.Image] = grab_window, telemetry_seconds: float = TELEMETRY_SECONDS
) -> dict[str, Image.Image]:
    """Shows every section and returns its picture; ShotError when any check fails."""
    sources = read_sources()
    refusals = Refusals()
    refusals.install()
    engine = build_engine(sources)
    allowed = {ip for preset in fake_network.DNS_PRESETS for ip in preset["ipv4"]}
    allowed |= {a["mac"].upper() for a in fake_network.ADAPTERS if a["mac"]}
    scrubber = Scrubber(allowed)
    scrub_engine(engine, scrubber)
    show_windows_processes_only(scrubber)

    images: dict[str, Image.Image] = {}
    problems: list[str] = []
    gc.disable()
    with system.TimerResolution():
        started = time.perf_counter()
        app = App(
            telemetry_factory=WideTelemetry,
            engine=EngineBridge(module=engine),  # type: ignore[arg-type]
            elevated=True,
        )
        try:
            app.clipboard_clear = refusals.stub("clipboard_clear")  # type: ignore[method-assign]
            app.clipboard_append = refusals.stub("clipboard_append")  # type: ignore[method-assign]
            app.geometry(f"{WIDTH}x{HEIGHT}{PLACE}")
            app.attributes("-topmost", True)
            pump(app, 4.0)
            if (app.winfo_width(), app.winfo_height()) != (WIDTH, HEIGHT):
                raise ShotError(
                    f"the window is {app.winfo_width()}x{app.winfo_height()} pixels, not {WIDTH}x{HEIGHT}; "
                    "set the display scaling to 100 %"
                )
            for name, section, prepare in SHOTS:
                if section == "Dashboard":
                    pump(app, max(0.0, telemetry_seconds - (time.perf_counter() - started)))
                prepare(app, engine)
                if app.state() != "normal":
                    raise ShotError(f"{section}: the window is {app.state()}, not shown")
                check_window(app, section, scrubber)
                image = grab(app)
                if image.size != (WIDTH, HEIGHT):
                    raise ShotError(f"{section}: the picture is {image.size[0]}x{image.size[1]} pixels")
                darkest, brightest = image.convert("L").getextrema()
                if darkest == brightest:
                    raise ShotError(f"{section}: the picture is blank (a locked screen or a covered window)")
                images[name] = image.convert("RGB")
        except ShotError as exc:
            problems.append(str(exc))
        except Exception:  # noqa: BLE001 - any failure ends the run with its traceback
            problems.append(traceback.format_exc())
        finally:
            try:
                app._request_close(force=True)
            except Exception:  # noqa: BLE001 - the window is closing anyway
                problems.append(traceback.format_exc())
            if app.engine is not None:
                app.engine.shutdown(wait=True)
            gc.collect()
            gc.enable()
    problems += [f"the window reported an error:\n{error}" for error in app.errors]
    problems += [f"the run reached {name}" for name in refusals.reached]
    if "optimizer_engine" in sys.modules:
        problems.append("the real engine was loaded")
    if problems:
        raise ShotError("\n".join(problems))
    return images


def main(argv: list[str] | None = None) -> int:
    parser = argparse.ArgumentParser(description="Takes the README screenshots.")
    parser.add_argument(
        "--visible",
        action="store_true",
        help="show the window on the screen and copy the pictures from there",
    )
    parser.add_argument(
        "--out", type=Path, default=OUT, metavar="FOLDER", help="folder for the pictures (docs/screenshots)"
    )
    args = parser.parse_args(argv)
    try:
        if not args.visible:
            use_hidden_desktop()
        images = run(grab_screen if args.visible else grab_window)
    except ShotError as exc:
        print(f"make_screenshots: no pictures were written:\n{exc}", file=sys.stderr)
        return 1
    out = args.out.resolve()
    out.mkdir(parents=True, exist_ok=True)
    for name, image in images.items():
        path = out / f"{name}.png"
        image.save(path, optimize=True)
        shown = path.relative_to(ROOT) if path.is_relative_to(ROOT) else path
        print(f"{shown}: {image.width}x{image.height}, {path.stat().st_size // 1024} KB")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
