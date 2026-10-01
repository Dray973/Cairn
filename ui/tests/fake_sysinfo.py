"""System-information part of FakeEngine: `sysinfo_snapshot`.

Mixed into `FakeEngine`, which calls `_init_sysinfo` from its constructor. The snapshot
is a fixed fixture shaped like the engine's (`info`, `summary`, `sections`, `text`), with
section ids in the engine's order and the same row keys; nothing is read from this PC.

Options (`FakeEngine(**options)`):
- `sysinfo_errors`: section id -> message; that section reports the error and no rows,
  as a section the engine could not read. An id the snapshot does not have is a
  `ValueError`.
- `sysinfo_failure`: message of the RuntimeError `sysinfo_snapshot` raises instead.
- `sysinfo_displays_unavailable`: the Displays section carries the remote or locked
  session note and no rows.
"""

from __future__ import annotations

import copy
from collections.abc import Mapping, Sequence
from typing import Any

SYSINFO_COMPUTER_NAME = "TEST-PC"
SYSINFO_TAKEN_AT = "2026-09-25T12:00:00.123456789Z"
SYSINFO_DURATION_MS = 412
SYSINFO_SUMMARY = "Windows 11 Home 25H2  ·  Test CPU 9000  ·  32 GB RAM  ·  Test GPU  ·  1.02 TB NVMe SSD"
SYSINFO_DISPLAYS_NOTE = "Display details are not available in a remote or locked session."
# Keys of every row the engine renders.
SYSINFO_ROW_KEYS = ("label", "value", "level", "note", "fraction", "private")


def _row(
    label: str,
    value: str,
    *,
    level: str = "normal",
    note: str | None = None,
    fraction: float | None = None,
    private: bool = False,
) -> dict[str, Any]:
    return {
        "label": label,
        "value": value,
        "level": level,
        "note": note,
        "fraction": fraction,
        "private": private,
    }


def _section(
    section_id: str,
    title: str,
    rows: list[dict[str, Any]],
    groups: list[dict[str, Any]] | None = None,
) -> dict[str, Any]:
    return {
        "id": section_id,
        "title": title,
        "error": None,
        "note": None,
        "rows": rows,
        "groups": groups or [],
    }


# The sections of a complete snapshot, in the engine's order, with the engine's labels.
SYSINFO_SECTIONS: tuple[dict[str, Any], ...] = (
    _section(
        "windows",
        "Windows",
        [
            _row("Edition", "Windows 11 Home"),
            _row("Version", "25H2"),
            _row("OS build", "26200.6725"),
            _row("Architecture", "64-bit (x64)"),
            _row("Activation", "Activated", level="good"),
            _row("Installed", "2026-03-14", note="Date of the last clean install or feature update"),
            _row(
                "Up time",
                "1 day 18 hours",
                note="Fast Startup is on: shutting down does not reset this; restarting does.",
            ),
            _row("Fast Startup", "On"),
            _row("Computer name", SYSINFO_COMPUTER_NAME, private=True),
        ],
    ),
    _section(
        "processor",
        "Processor",
        [
            _row("Name", "Test CPU 9000"),
            _row("Cores", "20 (8 performance, 12 efficiency)"),
            _row("Logical processors", "20"),
            _row("Base speed", "2.40 GHz"),
            _row("L1 cache", "1.8 MB"),
            _row("L2 cache", "36 MB"),
            _row("L3 cache", "30 MB"),
            _row("Identifier", "Intel64 Family 6 Model 198 Stepping 2"),
        ],
    ),
    _section(
        "memory",
        "Memory",
        [
            _row("Installed", "32 GB"),
            _row("Usable", "31.7 GB"),
            _row("In use", "12 GB (38%)", fraction=0.38),
            _row("Slots", "2 of 4 used"),
            _row("Maximum capacity", "256 GB"),
            # The engine shows the locators "DIMM_A2" and "DIMM_B2" as words.
            _row("DIMM A2", "16 GB DDR5 DIMM  ·  5600 MT/s  ·  Test Memory TM-16G"),
            _row("DIMM B2", "16 GB DDR5 DIMM  ·  5600 MT/s  ·  Test Memory TM-16G"),
        ],
    ),
    _section(
        "graphics",
        "Graphics",
        [],
        [
            {
                "title": "Test GPU",
                "rows": [
                    _row("Manufacturer", "NVIDIA"),
                    _row("Dedicated memory", "16 GB"),
                    _row("Shared memory", "15.9 GB"),
                    _row("Driver version", "32.0.15.8129"),
                    _row("Driver date", "2026-08-01"),
                    _row("Driver provider", "NVIDIA"),
                ],
            }
        ],
    ),
    _section(
        "displays",
        "Displays",
        [_row("Test Monitor", "2560 × 1440  ·  165 Hz  ·  DisplayPort")],
    ),
    _section(
        "board",
        "Motherboard and firmware",
        [
            _row("System", "Test Maker Test Model"),
            _row("Motherboard", "Test Maker TB-1000"),
            _row("Firmware", "UEFI"),
            _row("BIOS", "Test Firmware 1.23"),
            _row("BIOS date", "2026-01-15"),
        ],
    ),
    _section(
        "storage",
        "Storage",
        [],
        [
            {
                "title": "Disk 0: Test NVMe 1TB",
                "rows": [
                    _row("Drive", "1.02 TB  ·  NVMe SSD"),
                    _row("Firmware", "1.0"),
                    _row(
                        "C: Windows",
                        "38.1 GB free of 952 GB  ·  NTFS",
                        level="warning",
                        note="Less than 10% free",
                        fraction=0.96,
                    ),
                ],
            }
        ],
    ),
    _section(
        "security",
        "Security",
        [
            _row("Secure Boot", "On", level="good"),
            _row("TPM", "2.0", level="good"),
            _row("Virtualization", "Enabled in firmware"),
            _row(
                "Memory integrity",
                "Turned on, not running",
                level="warning",
                note="It starts after the next restart.",
            ),
            _row("VBS", "Turned on, not running", level="warning", note="It starts after the next restart."),
        ],
    ),
)
SYSINFO_SECTION_IDS = tuple(s["id"] for s in SYSINFO_SECTIONS)

# Field of the snapshot's `info` each section is read into, with its value when unreadable.
_INFO_FIELDS: dict[str, tuple[tuple[str, Any], ...]] = {
    "windows": (("os", None),),
    "processor": (("cpu", None),),
    "memory": (("memory", None),),
    "graphics": (("gpus", []),),
    "displays": (("displays", []),),
    "board": (("board", None),),
    "storage": (("disks", []), ("volumes", [])),
    "security": (("security", None),),
}

# The plain text: section rows are indented 2 spaces and group rows 4; the values of a
# section start in one column, 23 unless a label needs more (2 spaces at least after it).
_SECTION_INDENT = 2
_GROUP_INDENT = 4
_VALUE_COLUMN = 23
_LABEL_GAP = 2


def _info() -> dict[str, Any]:
    """The `info` part: the raw values the sections were rendered from."""
    return {
        "taken_at": SYSINFO_TAKEN_AT,
        "duration_ms": SYSINFO_DURATION_MS,
        "os": {
            "product_name": "Windows 11 Home",
            "edition_id": "Core",
            "display_version": "25H2",
            "build": 26200,
            "revision": 6725,
            "architecture": "x64",
            "emulated": False,
            "installed_at": "2026-03-14T09:30:00Z",
            "uptime_secs": 151_200,
            "fast_startup": True,
            "activation": "activated",
            "computer_name": SYSINFO_COMPUTER_NAME,
        },
        "cpu": {
            "name": "Test CPU 9000",
            "vendor": "GenuineIntel",
            "identifier": "Intel64 Family 6 Model 198 Stepping 2",
            "packages": 1,
            "cores": 20,
            "logical_processors": 20,
            "performance_cores": 8,
            "efficiency_cores": 12,
            "base_mhz": 2400,
            "cache": {"l1_kib": 1_840, "l2_kib": 36_864, "l3_kib": 30_720},
        },
        "memory": {
            "installed_bytes": 32 * 1024**3,
            "usable_bytes": 34_038_870_016,
            "available_bytes": 21_104_099_328,
            "load_percent": 38,
            "slots": 4,
            "max_capacity_bytes": 256 * 1024**3,
            "modules": [
                {
                    "locator": locator,
                    "bank": bank,
                    "size_bytes": 16 * 1024**3,
                    "kind": "DDR5",
                    "form_factor": "DIMM",
                    "speed_mts": 5600,
                    "configured_mts": 5600,
                    "manufacturer": "Test Memory",
                    "part_number": "TM-16G",
                }
                for locator, bank in (("DIMM_A2", "BANK 0"), ("DIMM_B2", "BANK 1"))
            ],
        },
        "gpus": [
            {
                "name": "Test GPU",
                "vendor": "NVIDIA",
                "vendor_id": 0x10DE,
                "device_id": 0x2D04,
                "dedicated_bytes": 16 * 1024**3,
                "shared_bytes": 17_072_701_440,
                "driver_version": "32.0.15.8129",
                "driver_date": "2026-08-01",
                "driver_provider": "NVIDIA",
                "basic_driver": False,
            }
        ],
        "displays": [
            {
                "name": "Test Monitor",
                "width": 2560,
                "height": 1440,
                "refresh_hz": 165.0,
                "connection": "DisplayPort",
                "built_in": False,
                "primary": True,
                "gpu": "Test GPU",
            }
        ],
        "displays_unavailable": False,
        "board": {
            "system_manufacturer": "Test Maker",
            "system_product": "Test Model",
            "system_family": None,
            "board_manufacturer": "Test Maker",
            "board_product": "TB-1000",
            "bios_vendor": "Test Firmware",
            "bios_version": "1.23",
            "bios_date": "2026-01-15",
            "firmware": "uefi",
        },
        "disks": [
            {
                "number": 0,
                "model": "Test NVMe 1TB",
                "firmware": "1.0",
                "bus": "NVMe",
                "media": "ssd",
                "size_bytes": 1_024_209_543_168,
                "removable": False,
                "system": True,
            }
        ],
        "volumes": [
            {
                "letter": "C:",
                "label": "Windows",
                "file_system": "NTFS",
                "kind": "fixed",
                "size_bytes": 1_022_202_687_488,
                "free_bytes": 40_909_578_240,
                "disk_numbers": [0],
                "system": True,
                "ready": True,
                "not_responding": False,
                "error": None,
            }
        ],
        "security": {
            "firmware": "uefi",
            "secure_boot": "on",
            "tpm_found": True,
            "tpm_version": "2.0",
            "virtualization": "enabled",
            "hypervisor": None,
            "memory_integrity": "not_running",
            "vbs": "not_running",
        },
        "errors": [],
    }


def _value_column(section: Mapping[str, Any]) -> int:
    """Column where the values of `section` start in the text; private rows do not count."""
    placed = [(_SECTION_INDENT, row) for row in section["rows"]] + [
        (_GROUP_INDENT, row) for group in section["groups"] for row in group["rows"]
    ]
    widths = [indent + len(str(row["label"])) + _LABEL_GAP for indent, row in placed if not row["private"]]
    return max([_VALUE_COLUMN, *widths])


def _row_lines(row: Mapping[str, Any], indent: int, column: int) -> list[str]:
    """A row as the engine prints it: the indent, the label padded so the value starts at
    `column`, the value, and the note on its own line under the value."""
    label = str(row["label"])
    lines = [f"{' ' * indent}{label.ljust(column - indent)}{row['value']}"]
    if row.get("note"):
        lines.append(f"{' ' * column}{row['note']}")
    return lines


def sysinfo_text(sections: Sequence[Mapping[str, Any]]) -> str:
    """The copyable text of `sections` in the engine's layout; private rows are left out."""
    lines = [
        "Cairn 0.0.0-test system summary",
        "Captured 2026-09-25 12:00 (UTC+00:00)",
    ]
    for section in sections:
        lines += ["", section["title"]]
        if section.get("note"):
            lines.append(f"  {section['note']}")
        if section.get("error"):
            lines.append(f"  Could not read: {section['error']}")
        column = _value_column(section)
        for row in section["rows"]:
            if not row["private"]:
                lines += _row_lines(row, _SECTION_INDENT, column)
        for group in section["groups"]:
            lines.append(f"  {group['title']}")
            for row in group["rows"]:
                if not row["private"]:
                    lines += _row_lines(row, _GROUP_INDENT, column)
    return "\n".join(lines) + "\n"


SYSINFO_TEXT = sysinfo_text(SYSINFO_SECTIONS)


def sysinfo_fixture(
    errors: Mapping[str, str] | None = None, displays_unavailable: bool = False
) -> dict[str, Any]:
    """A fresh snapshot: every section, minus what `errors` and `displays_unavailable` take out."""
    sections = copy.deepcopy(list(SYSINFO_SECTIONS))
    info = _info()
    for section in sections:
        if section["id"] == "displays" and displays_unavailable:
            section["note"] = SYSINFO_DISPLAYS_NOTE
            section["rows"] = []
            info["displays"] = []
            info["displays_unavailable"] = True
        message = (errors or {}).get(section["id"])
        if message is not None:
            section["error"] = message
            section["rows"] = []
            section["groups"] = []
            for field, empty in _INFO_FIELDS[section["id"]]:
                info[field] = copy.copy(empty)
            info["errors"].append({"section": section["id"], "message": message})
    return {
        "info": info,
        "summary": SYSINFO_SUMMARY,
        "sections": sections,
        "text": sysinfo_text(sections),
    }


class FakeSysinfo:
    """A fixed system snapshot; nothing is read from this PC."""

    sysinfo_errors: dict[str, str]
    sysinfo_failure: str | None
    sysinfo_displays_unavailable: bool

    def _init_sysinfo(self, options: dict[str, Any]) -> None:
        """Pops the system-information options this fake understands from `options`."""
        self.sysinfo_errors = dict(options.pop("sysinfo_errors", None) or {})
        unknown = sorted(set(self.sysinfo_errors) - set(SYSINFO_SECTION_IDS))
        if unknown:
            raise ValueError(f"sysinfo_errors names sections the snapshot does not have: {unknown}")
        self.sysinfo_failure = options.pop("sysinfo_failure", None)
        self.sysinfo_displays_unavailable = bool(options.pop("sysinfo_displays_unavailable", False))

    def sysinfo_snapshot(self) -> dict[str, Any]:
        self._record("sysinfo_snapshot")  # type: ignore[attr-defined]
        if self.sysinfo_failure is not None:
            raise RuntimeError(self.sysinfo_failure)
        return sysinfo_fixture(self.sysinfo_errors, self.sysinfo_displays_unavailable)
