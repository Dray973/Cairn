"""Pure helpers of the System section and the shape of the FakeEngine's system snapshot.

No window is created and the clipboard is never touched.
"""

from __future__ import annotations

import ctypes
from collections.abc import Callable
from ctypes import wintypes
from typing import Any

import pytest

from optimizer import theme
from optimizer.features import system_info
from optimizer.widgets.history import local_time
from optimizer.widgets.sysinfo import (
    LEVEL_STYLE,
    PRIVACY_NOTE,
    SECTION_COLUMNS,
    _fraction,
    _mappings,
    break_long_words,
    meta_text,
    place_sections,
    row_style,
)

from .fake_engine import FakeEngine
from .fake_sysinfo import (
    SYSINFO_COMPUTER_NAME,
    SYSINFO_DISPLAYS_NOTE,
    SYSINFO_ROW_KEYS,
    SYSINFO_SECTION_IDS,
    SYSINFO_TAKEN_AT,
    SYSINFO_TEXT,
    sysinfo_text,
)

ENGINE_SECTION_IDS = (
    "windows",
    "processor",
    "memory",
    "graphics",
    "displays",
    "board",
    "storage",
    "security",
)
ENGINE_SECTION_TITLES = (
    "Windows",
    "Processor",
    "Memory",
    "Graphics",
    "Displays",
    "Motherboard and firmware",
    "Storage",
    "Security",
)


def test_row_style_pairs_each_level_with_an_icon_and_a_colour() -> None:
    assert row_style("good") == ("✓ ", theme.GOOD)
    assert row_style("warning") == ("⚠ ", theme.WARNING)
    assert row_style("normal") == ("", theme.INK)
    assert row_style(None) == ("", theme.INK)
    assert row_style("critical") == ("", theme.INK), "an unknown level reads as normal"
    assert set(LEVEL_STYLE) == {"good", "warning"}


def test_place_sections_follows_the_column_table() -> None:
    placed = dict(zip(ENGINE_SECTION_IDS, place_sections(ENGINE_SECTION_IDS), strict=True))
    assert placed == {
        "windows": (0, 0),
        "processor": (0, 1),
        "memory": (0, 2),
        "graphics": (0, 3),
        "board": (1, 0),
        "displays": (1, 1),
        "storage": (1, 2),
        "security": (1, 3),
    }
    assert place_sections(["security", "windows", "board"]) == [(1, 1), (0, 0), (1, 0)]
    assert place_sections([]) == []


def test_unknown_sections_go_to_the_column_with_fewer_cards() -> None:
    # Known: windows left, board right; then ties go left.
    assert place_sections(["windows", "extra", "board", "other", "more"]) == [
        (0, 0),
        (0, 1),
        (1, 0),
        (1, 1),
        (0, 2),
    ]
    assert place_sections(["windows", "processor", "new"]) == [(0, 0), (0, 1), (1, 0)]
    assert place_sections(["new"]) == [(0, 0)]


def test_meta_text_names_the_read_time_the_duration_and_the_privacy_note() -> None:
    stamp = "2026-09-25T12:00:00Z"
    assert meta_text({"info": {"taken_at": stamp, "duration_ms": 412}}) == (
        f"Read {local_time(stamp)} in 0.4 s  ·  {PRIVACY_NOTE}"
    )
    # chrono writes nanoseconds; the time is still converted to local time.
    assert local_time(SYSINFO_TAKEN_AT) != SYSINFO_TAKEN_AT
    assert meta_text({"info": {"taken_at": SYSINFO_TAKEN_AT, "duration_ms": 1500}}) == (
        f"Read {local_time(SYSINFO_TAKEN_AT)} in 1.5 s  ·  {PRIVACY_NOTE}"
    )
    # A read of a few milliseconds is not shown as "0.0 s".
    for duration in (0, 16, 49):
        assert meta_text({"info": {"taken_at": stamp, "duration_ms": duration}}) == (
            f"Read {local_time(stamp)} in under 0.1 s  ·  {PRIVACY_NOTE}"
        )
    assert " in 0.1 s  ·  " in meta_text({"info": {"taken_at": stamp, "duration_ms": 50}})
    assert " in 12.3 s  ·  " in meta_text({"info": {"taken_at": stamp, "duration_ms": 12_345}})
    for duration in (-5, True, "412", float("nan"), float("inf")):
        assert meta_text({"info": {"taken_at": stamp, "duration_ms": duration}}) == (
            f"Read {local_time(stamp)}  ·  {PRIVACY_NOTE}"
        ), duration
    assert meta_text({"info": {"taken_at": stamp}}) == f"Read {local_time(stamp)}  ·  {PRIVACY_NOTE}"
    assert meta_text({"info": {"taken_at": stamp, "duration_ms": None}}).endswith(f"  ·  {PRIVACY_NOTE}")
    assert meta_text({}) == PRIVACY_NOTE
    assert meta_text({"info": None}) == PRIVACY_NOTE
    assert "computer name" in PRIVACY_NOTE


def test_fake_snapshot_has_the_engine_shape() -> None:
    snapshot = FakeEngine().sysinfo_snapshot()
    assert set(snapshot) == {"info", "summary", "sections", "text"}
    sections = snapshot["sections"]
    assert tuple(s["id"] for s in sections) == ENGINE_SECTION_IDS == SYSINFO_SECTION_IDS
    assert tuple(s["title"] for s in sections) == ENGINE_SECTION_TITLES
    assert set(SECTION_COLUMNS) == set(ENGINE_SECTION_IDS)
    private = []
    for section in sections:
        assert set(section) == {"id", "title", "error", "note", "rows", "groups"}
        assert section["error"] is None
        assert section["note"] is None
        for group in section["groups"]:
            assert set(group) == {"title", "rows"}
        rows = section["rows"] + [r for g in section["groups"] for r in g["rows"]]
        assert rows, f"{section['id']} has no rows"
        for row in rows:
            assert tuple(row) == SYSINFO_ROW_KEYS
            assert row["level"] in {"normal", "good", "warning"}
            assert row["fraction"] is None or 0.0 <= row["fraction"] <= 1.0
            if row["private"]:
                private.append((row["label"], row["value"]))
    assert private == [("Computer name", SYSINFO_COMPUTER_NAME)]
    assert snapshot["text"] == SYSINFO_TEXT
    assert snapshot["text"].startswith("Cairn ")
    assert snapshot["text"].endswith("\n")
    assert SYSINFO_COMPUTER_NAME not in snapshot["text"], "the copied text leaves out the computer name"
    assert snapshot["info"]["taken_at"] == SYSINFO_TAKEN_AT
    assert snapshot["summary"].count("  ·  ") >= 3


def test_fake_text_follows_the_engine_layout() -> None:
    lines = SYSINFO_TEXT.splitlines()
    assert lines[0] == "Cairn 0.0.0-test system summary"
    assert lines[1].startswith("Captured ")
    # Each section starts after a blank line.
    titles = [lines[i + 1] for i, line in enumerate(lines[:-1]) if line == ""]
    assert titles == list(ENGINE_SECTION_TITLES)
    # Section rows: 2 spaces, the label padded to 21, the value.
    assert "  Edition              Windows 11 Home" in lines
    # Group rows: 4 spaces, the label padded to 19; the note sits under the value.
    row = lines.index("    C: Windows         38.1 GB free of 952 GB  ·  NTFS")
    assert lines[row - 3] == "  Disk 0: Test NVMe 1TB"
    assert lines[row + 1] == " " * 23 + "Less than 10% free"
    assert "Computer name" not in SYSINFO_TEXT
    # Memory slots read as words, as the engine renders their locators.
    assert "  DIMM A2              16 GB DDR5 DIMM  ·  5600 MT/s  ·  Test Memory TM-16G" in lines


def _row(label: str, value: str, *, note: str | None = None, private: bool = False) -> dict[str, Any]:
    return {
        "label": label,
        "value": value,
        "level": "normal",
        "note": note,
        "fraction": None,
        "private": private,
    }


def test_fake_text_moves_a_sections_values_right_for_a_long_label() -> None:
    sections = [
        {
            "id": "memory",
            "title": "Memory",
            "error": None,
            "note": None,
            "rows": [_row("Installed", "32 GB"), _row("Controller0 ChannelA DIMM0", "16 GB DDR5")],
            "groups": [],
        },
        {
            "id": "storage",
            "title": "Storage",
            "error": None,
            "note": None,
            "rows": [],
            "groups": [
                {
                    "title": "Disk 0: Test",
                    "rows": [
                        _row("Drive", "1 TB"),
                        _row("D: Photos and home videos", "1 GB free of 2 GB", note="Less than 10% free"),
                    ],
                }
            ],
        },
        {
            "id": "windows",
            "title": "Windows",
            "error": None,
            "note": None,
            "rows": [
                _row("Edition", "Windows 11 Home"),
                _row("A private label longer than the column", "hidden", private=True),
            ],
            "groups": [],
        },
    ]
    lines = sysinfo_text(sections).splitlines()
    # 2 + 26 + 2 and 4 + 25 + 2: every value of the section starts in the same column.
    assert "  Installed" + " " * 19 + "32 GB" in lines
    assert "  Controller0 ChannelA DIMM0  16 GB DDR5" in lines
    assert "    Drive" + " " * 22 + "1 TB" in lines
    row = lines.index("    D: Photos and home videos  1 GB free of 2 GB")
    assert lines[row + 1] == " " * 31 + "Less than 10% free"
    # A private row is not printed and leaves the column alone.
    assert "  Edition              Windows 11 Home" in lines


def _fits_in(columns: int) -> Callable[[str], bool]:
    """A stand-in for a font measure: one column per character."""
    return lambda text: len(text) <= columns


def test_break_long_words_breaks_after_the_last_separator_that_fits() -> None:
    assert break_long_words("Controller0-ChannelA-DIMM1", _fits_in(21)) == "Controller0-ChannelA-\nDIMM1"
    assert break_long_words("Controller0-ChannelA-DIMM1", _fits_in(12)) == "Controller0-\nChannelA-\nDIMM1"
    assert break_long_words("P0_CHANNEL_A/DIMM_1", _fits_in(13)) == "P0_CHANNEL_A/\nDIMM_1"
    assert break_long_words(r"C:\Volumes\Backup", _fits_in(11)) == "C:\\Volumes\\\nBackup"
    # Only the word that does not fit is broken; the others are left for Tk to wrap.
    assert break_long_words("Slot Controller0-ChannelA-DIMM1 left", _fits_in(21)) == (
        "Slot Controller0-ChannelA-\nDIMM1 left"
    )


def test_break_long_words_leaves_what_fits_or_cannot_break() -> None:
    assert break_long_words("ChannelA DIMM1", _fits_in(21)) == "ChannelA DIMM1"
    assert break_long_words("Controller0-ChannelA-DIMM1", _fits_in(26)) == "Controller0-ChannelA-DIMM1"
    # No separator, or none early enough: Tk breaks it.
    assert break_long_words("Averyveryverylongwordwithoutbreaks", _fits_in(10)) == (
        "Averyveryverylongwordwithoutbreaks"
    )
    assert break_long_words("Averyverylongword-x", _fits_in(10)) == "Averyverylongword-x"
    # A separator at the very end is no place to break.
    assert break_long_words("Averyverylongword-", _fits_in(10)) == "Averyverylongword-"
    # The part after the last break that fits may still be too wide.
    assert break_long_words("ab-Averyverylongword", _fits_in(10)) == "ab-\nAveryverylongword"
    assert break_long_words("", _fits_in(0)) == ""
    assert break_long_words("  a  ", _fits_in(1)) == "  a  "


def test_fake_snapshot_reports_failed_sections_and_the_displays_note() -> None:
    engine = FakeEngine(
        sysinfo_errors={"graphics": "DXGI is not available"}, sysinfo_displays_unavailable=True
    )
    snapshot = engine.sysinfo_snapshot()
    sections = {s["id"]: s for s in snapshot["sections"]}
    assert sections["graphics"]["error"] == "DXGI is not available"
    assert sections["graphics"]["rows"] == []
    assert sections["graphics"]["groups"] == []
    assert sections["displays"]["note"] == SYSINFO_DISPLAYS_NOTE
    assert sections["displays"]["rows"] == []
    assert sections["displays"]["error"] is None
    assert all(s["note"] is None for sid, s in sections.items() if sid != "displays")
    assert sections["processor"]["rows"], "other sections still render"
    assert snapshot["info"]["gpus"] == []
    assert snapshot["info"]["displays_unavailable"] is True
    assert snapshot["info"]["errors"] == [{"section": "graphics", "message": "DXGI is not available"}]
    assert "  Could not read: DXGI is not available\n" in snapshot["text"]
    assert f"  {SYSINFO_DISPLAYS_NOTE}\n" in snapshot["text"]
    # Every call returns a fresh fixture.
    snapshot["sections"][0]["rows"].clear()
    assert engine.sysinfo_snapshot()["sections"][0]["rows"]
    assert len(engine.calls_named("sysinfo_snapshot")) == 2


def test_fake_snapshot_failure_and_unknown_options() -> None:
    engine = FakeEngine(sysinfo_failure="access denied")
    with pytest.raises(RuntimeError, match="access denied"):
        engine.sysinfo_snapshot()
    assert engine.calls_named("sysinfo_snapshot") == [()]
    with pytest.raises(TypeError, match="sysinfo_error"):
        FakeEngine(sysinfo_error={"graphics": "x"})
    with pytest.raises(ValueError, match="gpu"):
        FakeEngine(sysinfo_errors={"gpu": "x", "graphics": "y"})


def test_fraction_is_a_finite_share_between_zero_and_one() -> None:
    assert _fraction(0.38) == 0.38
    assert _fraction(1) == 1.0
    assert _fraction(0) == 0.0
    assert _fraction(7) == 1.0
    assert _fraction(-0.5) == 0.0
    for value in (None, "0.5", True, False, float("nan"), float("inf"), 10**400, [0.5]):
        assert _fraction(value) is None, value


def test_mappings_keeps_only_the_mappings_of_a_list() -> None:
    row = {"label": "A"}
    assert _mappings([row, "junk", None, 3, {"label": "B"}]) == [row, {"label": "B"}]
    assert _mappings((row,)) == [row]
    for value in (None, 5, "rows", row):
        assert _mappings(value) == [], value


def test_clipboard_functions_have_explicit_prototypes() -> None:
    """Binds the Win32 functions the native clipboard writer uses; none of them is called."""
    api = system_info._clipboard_api()
    assert tuple(api.open_clipboard.argtypes) == (wintypes.HWND,)
    assert api.open_clipboard.restype is wintypes.BOOL
    assert tuple(api.empty_clipboard.argtypes) == ()
    assert tuple(api.set_clipboard_data.argtypes) == (wintypes.UINT, wintypes.HANDLE)
    assert api.set_clipboard_data.restype is ctypes.c_void_p
    assert tuple(api.close_clipboard.argtypes) == ()
    assert tuple(api.global_alloc.argtypes) == (wintypes.UINT, ctypes.c_size_t)
    assert api.global_alloc.restype is ctypes.c_void_p
    assert tuple(api.global_lock.argtypes) == (wintypes.HGLOBAL,)
    assert api.global_lock.restype is ctypes.c_void_p
    assert api.global_unlock.restype is wintypes.BOOL
    assert tuple(api.global_free.argtypes) == (wintypes.HGLOBAL,)
    assert system_info.CF_UNICODETEXT == 13
    assert system_info.GMEM_MOVEABLE == 0x0002
    # The prototypes live on private library objects, not on the shared `ctypes.windll` ones.
    assert api.open_clipboard is not ctypes.windll.user32.OpenClipboard
