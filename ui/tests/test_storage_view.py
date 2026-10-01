"""Pure helpers of the Storage section: number and size formats, the write warning, progress
lines, summaries, the clipboard text and the history rows. No window is created."""

from __future__ import annotations

from datetime import datetime
from typing import Any

import pytest

from optimizer.features.storage import speed_confirm_message
from optimizer.widgets.storage import (
    GIB,
    MIB,
    count_text,
    date_text,
    dupes_progress_text,
    dupes_summary_lines,
    estimate_text,
    fmt_iops,
    fmt_latency,
    fmt_mb_s,
    group_text,
    history_row_lines,
    job_target,
    max_write_bytes,
    scan_progress_text,
    scan_summary_lines,
    share_bar,
    share_text,
    short_path,
    sizes_of,
    speed_done_line,
    speed_progress_text,
    speed_result_text,
    storage_status_text,
    storage_volume_text,
    time_of_day,
    write_warning,
)

from . import fake_storage
from .fake_engine import FakeEngine
from .fake_jobs import host_snapshot


def local(stamp: str, fmt: str) -> str:
    moment = datetime.fromisoformat(stamp).astimezone()
    return f"{moment.day} {moment:{fmt}}"


def job(kind: str, title: str, **detail: Any) -> dict[str, Any]:
    return host_snapshot(id=1, lane="storage", kind=kind, title=title, detail=detail)


def test_throughput_iops_and_latency_formats() -> None:
    assert fmt_mb_s(7012.3) == "7,012"
    assert fmt_mb_s(100) == "100"
    assert fmt_mb_s(92.14) == "92.1"
    assert fmt_mb_s(1.234) == "1.23"
    assert fmt_mb_s(None) == "–"
    assert fmt_iops(857_275) == "857k"
    assert fmt_iops(3512) == "3,512"
    assert fmt_iops(None) == "–"
    assert fmt_latency(37.2) == "37 µs"
    assert fmt_latency(1195) == "1.2 ms"
    assert fmt_latency(123_456) == "123 ms"
    assert fmt_latency(None) == "–"


def test_estimates_and_counts() -> None:
    assert estimate_text(30) == "about a minute"
    assert estimate_text(89) == "about a minute"
    assert estimate_text(180) == "about 3 minutes"
    assert estimate_text(7200) == "about 2 hours"
    assert count_text(1, "file") == "1 file"
    assert count_text(182_340, "file") == "182,340 files"
    assert count_text(4, "copy", "copies") == "4 copies"


def test_max_write_bytes_agrees_with_the_engine_plan() -> None:
    assert max_write_bytes(GIB, 3) == 13 * GIB
    engine = FakeEngine(elevated=True)
    for size, runs in ((64 * MIB, 1), (GIB, 3), (16 * GIB, 5)):
        plan = engine.storage_speed_start("C:", size, runs, dry_run=True)["plan"]
        assert plan["max_write_bytes"] == max_write_bytes(size, runs)


def test_write_warning_drops_the_endurance_sentence_on_a_hard_disk() -> None:
    ssd, hdd = fake_storage.VOLUMES
    assert write_warning(ssd, GIB, 3) == (
        "⚠ Writes up to 13 GB to C: the 1 GB test file, then up to 1 GB in each of the 12 write "
        "measurements. On an SSD this uses a little of its write endurance, so don't run it often."
    )
    assert write_warning(hdd, 64 * MIB, 1) == (
        "⚠ Writes up to 320 MB to D: the 64 MB test file, then up to 64 MB in each of the 4 write "
        "measurements."
    )


def test_volume_texts_and_paths() -> None:
    ssd, hdd = fake_storage.VOLUMES
    assert storage_volume_text(ssd) == "C:  Windows  ·  NVMe SSD  ·  611 GB free of 952 GB"
    assert storage_volume_text(hdd) == "D:  Data  ·  SATA HDD  ·  1210 GB free of 1863 GB"
    assert storage_volume_text({**ssd, "error": "cannot read the drive"}).endswith("⚠ cannot read the drive")
    assert short_path("C:\\Users\\Test\\Videos", 48) == "C:\\Users\\Test\\Videos"
    assert short_path("C:\\Users\\Test\\AppData\\Local\\Packages", 30) == "C:\\Users\\…\\Local\\Packages"
    long = "C:\\Users\\Test\\AppData\\Local\\Packages\\Contoso.App_0123456789abc\\LocalCache"
    assert short_path(long, 48) == "C:\\Users\\…\\Contoso.App_0123456789abc\\LocalCache"
    assert len(short_path("C:\\" + "x" * 80, 20)) == 20


def test_share_bars_use_eighth_blocks() -> None:
    assert share_bar(0) == ""
    assert share_bar(-1) == ""
    assert share_bar(1) == "█" * 10
    assert share_bar(3) == "█" * 10
    assert share_bar(0.5) == "█████"
    assert share_bar(1 / 80) == "▏"
    assert share_bar(5 / 80) == "▋"
    assert share_bar(7 / 80) == "▉"
    assert share_text(42, 100) == "████▎  42 %"
    assert share_text(5, 0) == "  0 %"
    assert sizes_of(int(3.2 * GIB), int(18.4 * GIB)) == "3.2 of 18.4 GB"
    assert sizes_of(0, 512 * 1024) == "0 of 512 KB"


def test_speed_progress_lines_for_every_phase() -> None:
    title = "Speed test of C:"
    preparing = job("speed_test", title, phase="preparing", speed={"bytes_written": 348 * MIB})
    assert speed_progress_text(preparing, GIB) == "Preparing the test file…  34 %"
    measuring = job(
        "speed_test",
        title,
        phase="measuring",
        speed={"test": "seq1m_q8t1", "direction": "read", "run": 2, "runs": 3, "live_mb_s": 6980.4},
    )
    assert speed_progress_text(measuring) == "SEQ1M Q8T1  ·  read  ·  run 2 of 3  ·  6,980 MB/s"
    assert speed_progress_text(job("speed_test", title, phase="pausing")) == "Pausing between measurements…"
    assert speed_progress_text(job("speed_test", title, phase="cleaning_up")) == "Deleting the test file…"
    assert speed_progress_text(job("speed_test", title, phase="saving")) == "Saving the result…"
    assert speed_progress_text(job("speed_test", title, phase="done")) == ""


def test_scan_and_duplicates_progress_lines() -> None:
    long = "C:\\Users\\Test\\AppData\\Local\\Packages\\Contoso.App_0123456789abc\\LocalCache"
    scanning = job(
        "space_scan",
        "Scan of C:",
        phase="scanning",
        scan={"files": 182_340, "folders": 20_114, "allocated_bytes": int(64.2 * GIB), "current": long},
    )
    assert scan_progress_text(scanning) == (
        "◐ Scanning C:…  182,340 files in 20,114 folders  ·  64.2 GB  ·  "
        "C:\\Users\\…\\Contoso.App_0123456789abc\\LocalCache"
    )
    assert scan_progress_text(job("space_scan", "Scan of C:", phase="summarizing")) == (
        "◐ Summarizing the scan of C:…"
    )
    sampling = job(
        "duplicates",
        "Duplicate search in C:\\",
        phase="sampling",
        duplicates={"files_total": 12_480, "bytes_total": 1000, "bytes_done": 410},
    )
    assert dupes_progress_text(sampling) == (
        "◐ Comparing the start and end of 12,480 files that share a size…  41 %"
    )
    hashing = job(
        "duplicates",
        "Duplicate search in C:\\",
        phase="hashing",
        duplicates={"files_total": 1204, "bytes_total": int(18.4 * GIB), "bytes_done": int(3.2 * GIB)},
    )
    assert dupes_progress_text(hashing) == "◐ Comparing 1,204 files in full  ·  3.2 of 18.4 GB read  ·  17 %"


def test_status_bar_texts_and_targets() -> None:
    speed = {**job("speed_test", "Speed test of C:"), "progress": 42.0}
    assert storage_status_text(speed) == "◐ Speed test C:  ·  42 %"
    scan = job("space_scan", "Scan of C:", scan={"files": 182_340})
    assert storage_status_text(scan) == "◐ Scanning C:  ·  182,340 files"
    dupes = {**job("duplicates", "Duplicate search in C:\\"), "progress": 41.0}
    assert storage_status_text(dupes) == "◐ Finding duplicates  ·  41 %"
    assert job_target(job("space_scan", "Scan of C:\\Users\\Test")) == "C:\\Users\\Test"


def summary(**fields: Any) -> dict[str, Any]:
    return {"summary": {**fake_storage.SCAN_SUMMARY, **fields}}


def test_scan_summary_lines() -> None:
    assert scan_summary_lines(summary()) == [
        "✓ C:  ·  338 GB on disk in 1,204,332 files and 214,009 folders  ·  scanned in 48 s",
        "3 GB of used space wasn't reached: NTFS metadata, restore points and shadow copies, and folders "
        "Cairn can't read.",
        "1 folder can't be read.",
        "38,402 hard links are counted once.",
        "1 link to another folder wasn't followed.",
    ]
    lines = scan_summary_lines(
        summary(
            whole_volume=False,
            root="C:\\Users\\Test",
            completed=False,
            denied_folders=20,
            unreadable_folders=3,
            online_only_bytes=int(12.4 * GIB),
            hard_links_counted_once=1,
            links_skipped=5,
            id_limit_reached=True,
            node_limit_reached=True,
        )
    )
    assert lines == [
        "○ C:\\Users\\Test  ·  338 GB on disk in 1,204,332 files and 214,009 folders  ·  scanned in 48 s",
        "23 folders can't be read.",
        "Online-only OneDrive files: 12.4 GB, not stored on this PC.",
        "1 hard link is counted once.",
        "5 links to other folders weren't followed.",
        "Stopped: the numbers cover only what was scanned.",
        "Too many files to track hard links beyond the first 8 million.",
        "Folder limit reached; deeper folders weren't listed.",
    ]


def test_duplicate_summary_lines() -> None:
    result = dict(fake_storage.DUPES_RESULT)
    assert dupes_summary_lines(result) == [
        "✓ 3 groups of identical files  ·  2.3 GB would be freed by keeping one copy of each"
    ]
    none = {**result, "group_count": 0, "groups": [], "wasted_bytes": 0}
    assert dupes_summary_lines(none) == ["✓ No duplicate files of 1 MB or more."]
    skipped = {**result, "skipped_in_use": 20, "skipped_unreadable": 12, "skipped_changed": 118}
    assert dupes_summary_lines(skipped)[1] == (
        "32 files couldn't be read (in use or access denied) and 118 changed since the scan; they were "
        "left out."
    )
    assert dupes_summary_lines({**result, "skipped_changed": 1})[1] == (
        "1 file changed since the scan; it was left out."
    )
    stopped = {**result, "completed": False, "group_count": 1500}
    lines = dupes_summary_lines(stopped)
    assert lines[0].startswith("○ 1,500 groups of identical files")
    assert lines[-2:] == [
        "Stopped: only the files compared so far are listed.",
        "Showing the 3 largest groups.",
    ]
    assert group_text(fake_storage.DUPLICATE_GROUPS[0]) == "3 copies  ·  1 GB each  ·  2 GB extra"


def test_speed_result_text_lays_out_a_table() -> None:
    text = speed_result_text({"kind": "speed", **fake_storage.SPEED_RESULT})
    lines = text.splitlines()
    assert lines[0] == "Cairn disk speed test  ·  C: Test NVMe SSD (NVMe)  ·  NTFS"
    assert lines[1] == (
        f"{local(fake_storage.STARTED_AT, '%b %Y %H:%M')}  ·  1 GB test file  ·  best of 3 runs  ·  "
        "MB/s (1 MB = 1,000,000 bytes)"
    )
    assert lines[2] == ""
    assert lines[3].split() == [
        "Read",
        "Write",
        "Read",
        "IOPS",
        "Write",
        "IOPS",
        "Read",
        "lat.",
        "Write",
        "lat.",
    ]
    assert lines[4].split() == ["SEQ1M", "Q8T1", "7,012", "6,345", "6,687", "6,051", "1.2", "ms", "1.3", "ms"]
    assert lines[6].split()[:6] == ["RND4K", "Q32T1", "3,511", "2,890", "857k", "706k"]
    assert len(lines) == 8
    stopped = speed_result_text({**fake_storage.SPEED_RESULT, "completed": False, "skipped": ["rnd4k_q1t1"]})
    assert "RND4K Q1T1      skipped" in stopped
    assert stopped.endswith("Stopped before every measurement ran.")


IO_ERROR = "the drive reported an I/O error: The request failed because of a device error."


def failed_partial_result() -> dict[str, Any]:
    """A test whose first write failed after the reads were measured, as the history keeps it."""
    reads = [m for m in fake_storage.SPEED_RESULT["measurements"] if m["direction"] == "read"]
    return {**fake_storage.SPEED_RESULT, "completed": False, "error": IO_ERROR, "measurements": reads}


def test_a_failed_result_is_not_called_stopped() -> None:
    text = speed_result_text(failed_partial_result())
    lines = text.splitlines()
    assert lines[4].split()[:4] == ["SEQ1M", "Q8T1", "7,012", "–"], "the reads, no writes"
    assert lines[-2:] == ["", f"Failed before every measurement ran: {IO_ERROR}"]
    assert "Stopped" not in text
    first, second = history_row_lines(failed_partial_result())
    assert first == f"{local(fake_storage.STARTED_AT, '%b %Y %H:%M')}  ·  C:  ·  1 GB × 3  ·  failed"
    assert second == "SEQ 7,012 / –  ·  RND4K Q1 92.1 / –"


def test_history_rows_and_dates() -> None:
    first, second = history_row_lines(fake_storage.SPEED_RESULT)
    assert first == f"{local(fake_storage.STARTED_AT, '%b %Y %H:%M')}  ·  C:  ·  1 GB × 3"
    assert second == "SEQ 7,012 / 6,345  ·  RND4K Q1 92.1 / 310"
    stopped, _ = history_row_lines({**fake_storage.SPEED_RESULT, "completed": False})
    assert stopped.endswith("  ·  stopped")
    assert date_text("2026-09-28T12:02:00Z", with_time=False) == local("2026-09-28T12:02:00+00:00", "%b %Y")
    assert date_text(None) == ""
    assert date_text("not a date") == "not a date"
    noon = datetime.fromisoformat("2026-09-28T12:03:00+00:00").astimezone()
    assert time_of_day("2026-09-28T12:03:00Z") == f"{noon:%H:%M}"


@pytest.mark.parametrize(
    ("state", "start"),
    [
        ("succeeded", "✓ Finished in 2 min 41 s  ·  1 GB test file  ·  best of 3 runs  ·  wrote 13 GB"),
        ("cancelled", "○ Stopped after 2 min 41 s; the test file was deleted."),
        ("failed", "⚠ Failed: C: ran out of space"),
    ],
)
def test_speed_done_lines(state: str, start: str) -> None:
    view = host_snapshot(state=state, elapsed_ms=161_000, summary="C: ran out of space")
    text, _ = speed_done_line(view, fake_storage.SPEED_RESULT if state == "succeeded" else None)
    assert text.startswith(start)


def test_the_confirmation_states_what_the_test_writes() -> None:
    plan = FakeEngine(elevated=True).storage_speed_start("C:", GIB, 3, dry_run=True)["plan"]
    message = speed_confirm_message("C:", dict(fake_storage.VOLUMES[0]), plan, GIB, 3)
    assert message.startswith("Cairn writes a 1 GB test file to C: (Windows), measures how fast")
    assert "This writes up to 13 GB to the drive and uses a little of the SSD's write endurance." in message
    assert "It takes about 2 minutes. You can stop it at any time." in message
    hdd = speed_confirm_message("D:", dict(fake_storage.VOLUMES[1]), {"media": "hdd"}, 64 * MIB, 1)
    assert "This writes up to 320 MB to the drive. It takes" in hdd
