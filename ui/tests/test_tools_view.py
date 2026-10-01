"""Pure helpers of the Tools section: durations, drive and job texts, why a tool cannot run, and the
output buffer. No window is created and no native module is loaded."""

from __future__ import annotations

from typing import Any

from optimizer import theme
from optimizer.features.tools import confirm_message
from optimizer.widgets.tools import (
    IDLE_HINT_MS,
    RUNNING_NOW,
    STATE_STYLE,
    OutputBuffer,
    duration_text,
    fmt_duration,
    meta_text,
    result_text,
    row_block,
    sentence,
    size_text,
    skipped_text,
    state_style,
    time_text,
    tool_status_text,
    volume_letter,
    volume_text,
)

from .fake_tools import TOOLS, VOLUMES, catalog_entry

GIB = 1024**3


def test_fmt_duration() -> None:
    assert fmt_duration(0) == "0 s"
    assert fmt_duration(999) == "0 s"
    assert fmt_duration(45_000) == "45 s"
    assert fmt_duration(59_999) == "59 s"
    assert fmt_duration(60_000) == "1 min"
    assert fmt_duration((12 * 60 + 5) * 1000) == "12 min 5 s"
    assert fmt_duration((14 * 60 + 2) * 1000) == "14 min 2 s"
    assert fmt_duration(3600_000) == "1 h"
    assert fmt_duration((63 * 60 + 40) * 1000) == "1 h 3 min"
    assert fmt_duration(-5) == "0 s"


def test_state_style_has_an_icon_and_colour_per_state() -> None:
    assert STATE_STYLE["running"] == ("◐ Running", theme.INK_SECONDARY)
    assert STATE_STYLE["succeeded"] == ("✓ Finished", theme.GOOD)
    assert STATE_STYLE["completed"] == ("– Finished: see the result", theme.INK_SECONDARY)
    assert STATE_STYLE["attention"] == ("⚠ Needs attention", theme.WARNING)
    assert STATE_STYLE["failed"] == ("⚠ Failed", theme.CRITICAL)
    assert STATE_STYLE["cancelled"] == ("○ Stopped", theme.INK_MUTED)
    assert state_style("exploded") == ("– exploded", theme.INK_MUTED)
    assert state_style(None)[1] == theme.INK_MUTED


def test_volume_text_names_drive_media_file_system_and_space() -> None:
    c, d = (dict(v) for v in VOLUMES)
    assert volume_text(c) == "C:  Windows  ·  SSD  ·  NTFS  ·  611 GB free of 952 GB"
    assert volume_text(d) == "D:  Data  ·  Hard disk  ·  NTFS  ·  1210 GB free of 1863 GB"
    bare = {
        "letter": "E",
        "label": "",
        "file_system": "exFAT",
        "size_bytes": 64 * GIB,
        "free_bytes": 0,
        "media": "unknown",
    }
    assert volume_text(bare) == "E:  ·  exFAT  ·  0 B free of 64 GB"
    part = dict(c, free_bytes=611 * GIB + GIB // 2, size_bytes=952 * GIB + GIB // 4)
    assert volume_text(part) == "C:  Windows  ·  SSD  ·  NTFS  ·  611.5 GB free of 952.2 GB"
    assert size_text(512 * 1024**2) == "512 MB"
    assert size_text(1536 * 1024**2) == "1.5 GB"
    assert size_text(900) == "900 B"
    broken = dict(c, error="The device is not ready.")
    assert volume_text(broken) == "C:  Windows  ·  SSD  ·  NTFS  ·  ⚠ The device is not ready."
    for letter in ("c", "C:", "c:\\", " C:\\ "):
        assert volume_letter({"letter": letter}) == "C:"
    assert volume_letter({"letter": ""}) == ""


def test_duration_text_reads_as_a_phrase() -> None:
    assert duration_text("usually 10–30 minutes") == "Usually 10–30 minutes"
    assert duration_text("under a minute") == "Usually under a minute"
    assert duration_text("about a minute", "Usually takes") == "Usually takes about a minute"
    assert duration_text("usually 10–60 minutes; needs internet", "Usually takes") == (
        "Usually takes 10–60 minutes; needs internet"
    )
    assert duration_text("SSD: about a minute; hard disk: minutes to hours") == (
        "SSD: about a minute; hard disk: minutes to hours"
    )
    assert duration_text("  ") == ""


def test_meta_text_covers_the_four_kinds_of_tool() -> None:
    assert meta_text(catalog_entry("disk_check")) == (
        "Usually a few minutes; longer on large hard disks  ·  Can be stopped  ·  Read-only"
    )
    assert meta_text(catalog_entry("sfc_verify")) == (
        "Usually 10–30 minutes  ·  Runs to completion  ·  Read-only"
    )
    assert meta_text(catalog_entry("sfc_scan")) == (
        "Usually 10–30 minutes  ·  Runs to completion  ·  Repairs can't be undone"
    )
    assert meta_text(catalog_entry("drive_retrim")) == (
        "Usually about a minute  ·  Runs to completion  ·  Nothing to undo"
    )
    assert meta_text({"cancellable": False}) == "Runs to completion  ·  Read-only"


def _block(tool_id: str, volume: dict[str, Any] | None, **state: Any) -> str | None:
    options: dict[str, Any] = {"engine_ready": True, "elevated": True, "job_running": False}
    options.update(state)
    return row_block(catalog_entry(tool_id), volume, **options)


def test_row_block_reasons_in_order() -> None:
    c, d = (dict(v) for v in VOLUMES)
    assert _block("sfc_verify", None) is None
    assert _block("disk_check", c) is None
    assert _block("drive_retrim", c) is None
    assert _block("drive_retrim", d) == "Retrim is for SSDs; this drive is a hard disk"
    assert _block("drive_optimize", d) is None
    assert _block("disk_check", None) == "no drive selected"
    assert _block("sfc_verify", d) is None, "tools without a drive ignore the selected one"
    broken = dict(d, error="The device is not ready.")
    assert _block("drive_optimize", broken) == "The device is not ready."

    # The first reason that applies wins.
    everything = {"engine_ready": False, "elevated": False, "job_running": True}
    assert _block("drive_retrim", d, **everything) == "engine unavailable"
    assert _block("drive_retrim", d, elevated=False, job_running=True) == "needs administrator rights"
    assert _block("drive_retrim", d, job_running=True) == "another tool is running"
    assert _block("disk_check", None, job_running=True) == "another tool is running"
    assert _block("drive_retrim", broken) == "The device is not ready.", "a read error wins over the rule"

    # The running tool's own row says it is running; the others that another tool is.
    assert _block("sfc_verify", None, job_running=True, running_tool="sfc_verify") == RUNNING_NOW
    assert _block("sfc_scan", None, job_running=True, running_tool="sfc_verify") == "another tool is running"
    assert _block("disk_check", c, job_running=True, running_tool="disk_check") == RUNNING_NOW
    assert _block("disk_check", c, running_tool="disk_check") is None, "only while a job runs"
    assert _block("sfc_verify", None, elevated=False, job_running=True, running_tool="sfc_verify") == (
        "needs administrator rights"
    )


def test_time_text_while_running_and_when_finished() -> None:
    running = {"state": "running", "elapsed_ms": 125_000, "idle_ms": 3_000}
    assert time_text(running) == "Running for 2 min 5 s  ·  last output 3 s ago"
    quiet = dict(running, idle_ms=IDLE_HINT_MS)
    assert time_text(quiet) == (
        "Running for 2 min 5 s  ·  last output 5 min ago\n"
        "Some steps print nothing for a long time; the tool is still working."
    )
    assert "still working" not in time_text(dict(running, idle_ms=IDLE_HINT_MS - 1))
    finished = {"state": "succeeded", "elapsed_ms": 842_000, "exit_code": 0, "exit_code_hex": "0x00000000"}
    assert time_text(finished) == "Finished in 14 min 2 s  ·  exit code 0 (0x00000000)"
    failed = {"state": "failed", "elapsed_ms": 1_000, "exit_code": -2146498529}
    assert time_text(failed) == "Finished in 1 s  ·  exit code -2146498529 (0x800F081F)"
    stopped = {"state": "cancelled", "elapsed_ms": 9_000, "exit_code": None}
    assert time_text(stopped) == "Stopped after 9 s"


def test_output_buffer_keeps_order_and_marks_lost_lines() -> None:
    buffer = OutputBuffer(cap=5)
    assert buffer.pending == 0
    assert buffer.take(10) == []
    buffer.feed(["a", "b"])
    buffer.feed(["e", "f"], skipped=2)
    assert buffer.pending == 5
    assert buffer.take(2) == ["a", "b"]
    assert buffer.take(10) == ["… 2 earlier lines are only in the log file", "e", "f"]
    assert buffer.pending == 0

    # Lines pushed out of a full buffer become one note at the front.
    buffer.feed([f"line {i}" for i in range(8)])
    assert buffer.pending == 6
    assert buffer.take(3) == ["… 3 earlier lines are only in the log file", "line 3", "line 4"]
    assert buffer.take(0) == []
    assert buffer.take(10) == ["line 5", "line 6", "line 7"]

    # A pushed-out note adds its count to the note that replaces it.
    buffer.feed(["x"], skipped=1500)
    buffer.feed([f"y{i}" for i in range(5)])
    assert buffer.take(1) == ["… 1,501 earlier lines are only in the log file"]
    assert buffer.take(10) == ["y0", "y1", "y2", "y3", "y4"]

    buffer.feed(["z"], skipped=4)
    buffer.clear()
    assert buffer.pending == 0
    assert buffer.take(5) == []

    # A single lost line is named in the singular.
    assert skipped_text(1) == "… 1 earlier line is only in the log file"
    buffer.feed(["p"], skipped=1)
    assert buffer.take(5) == ["… 1 earlier line is only in the log file", "p"]


def test_status_result_and_confirmation_texts() -> None:
    assert tool_status_text("Check system files", 45.0) == "◐ Check system files 45%"
    assert tool_status_text("Check system files", 17.9) == "◐ Check system files 18%"
    assert tool_status_text("Check system files", None) == "◐ Check system files"
    assert result_text({"hint": "Try again.", "summary": "Done.", "restart_required": True}) == (
        "Try again.\nDone.\nRestart Windows to finish this repair."
    )
    assert result_text({"hint": None, "summary": None}) == ""
    assert sentence("needs administrator rights") == "Needs administrator rights."
    assert sentence("Already said.") == "Already said."

    messages = {t[0]: confirm_message(catalog_entry(t[0])) for t in TOOLS}
    assert messages["sfc_scan"] == messages["dism_restore"]
    assert messages["sfc_scan"].startswith("Windows repairs its own files.")
    assert messages["drive_optimize"].startswith("Windows optimizes the drive.")
    assert messages["disk_check"] == "This only checks; nothing is changed. You can stop it at any time."
    assert messages["sfc_verify"] == (
        "This only checks; nothing is changed. It runs inside Windows and can't be stopped once it starts."
    )


def test_fake_catalog_follows_the_tool_table() -> None:
    catalog = [catalog_entry(t[0]) for t in TOOLS]
    assert [t["id"] for t in catalog] == [
        "sfc_verify",
        "sfc_scan",
        "dism_check",
        "dism_scan",
        "dism_restore",
        "drive_optimize",
        "drive_retrim",
        "disk_check",
    ]
    assert all(t["requires_admin"] for t in catalog)
    assert [t["id"] for t in catalog if t["cancellable"]] == ["disk_check"]
    assert [t["id"] for t in catalog if t["requires_detach"]] == ["sfc_scan", "dism_restore"]
    assert [t["id"] for t in catalog if t["needs_volume"]] == ["drive_optimize", "drive_retrim", "disk_check"]
