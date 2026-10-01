"""Storage section: disk speed test, space usage and duplicate files.

The pure helpers at the top format speeds, sizes, dates, progress and summaries. The panel
switches between three pages with a segmented button; a page switch only regrids the pages
and never calls the engine. Job snapshots of the "storage" lane feed the page of the job's
kind through `begin_job`, `update_job` and `finish_job`.

The folder tree and the duplicate groups are `ttk.Treeview`s, the only ttk widgets of the app:
they use the "clam" theme, which is process-wide, with the style `Cairn.Treeview`
(CustomTkinter draws no ttk widgets, so nothing else changes).
"""

from __future__ import annotations

import tkinter as tk
from collections.abc import Callable, Mapping, Sequence
from datetime import datetime
from tkinter import ttk
from typing import Any

import customtkinter as ctk

from .. import APP_NAME, theme
from .controls import fitted_wrap
from .monitor import set_text
from .tools import fmt_duration, size_text, volume_letter

MIB = 1 << 20
GIB = 1 << 30

PAGES = ("Speed test", "Space", "Duplicates")
KIND_SPEED = "speed_test"
KIND_SCAN = "space_scan"
KIND_DUPLICATES = "duplicates"
# Page of each job kind.
KIND_PAGES = {KIND_SPEED: "Speed test", KIND_SCAN: "Space", KIND_DUPLICATES: "Duplicates"}

SIZE_CHOICES = (
    ("64 MB", 64 * MIB),
    ("256 MB", 256 * MIB),
    ("1 GB", GIB),
    ("4 GB", 4 * GIB),
    ("16 GB", 16 * GIB),
)
DEFAULT_SIZE = "1 GB"
RUN_CHOICES = ("1", "3", "5")
DEFAULT_RUNS = "3"
MIN_SIZE_CHOICES = (("1 MB", MIB), ("10 MB", 10 * MIB), ("100 MB", 100 * MIB))
DEFAULT_MIN_SIZE = "1 MB"
TEST_ROWS = (
    ("seq1m_q8t1", "SEQ1M Q8T1"),
    ("seq1m_q1t1", "SEQ1M Q1T1"),
    ("rnd4k_q32t1", "RND4K Q32T1"),
    ("rnd4k_q1t1", "RND4K Q1T1"),
)
TEST_LABELS = dict(TEST_ROWS)
ORDER_CHOICES = (("Size on disk", "allocated"), ("Size", "logical"))
VIEW_CHOICES = ("Folders", "Largest files")
MEDIA_TEXT = {"ssd": "SSD", "hdd": "HDD"}

SPEED_INTRO = (
    "Measures how fast a drive reads and writes, like CrystalDiskMark: sequential 1 MB blocks and random "
    "4 KB blocks, each at a deep and a shallow queue. Speeds are in MB/s (1 MB = 1,000,000 bytes); each "
    "result is the best of the runs."
)
SPACE_INTRO = (
    "Shows what takes up space on a drive or in a folder. Read-only: Cairn doesn't delete anything here. "
    "Open items in File Explorer to review them and delete what you don't need; Explorer moves deleted "
    "files to the Recycle Bin."
)
DUPES_INTRO = (
    "Finds files with identical content among the files of the last scan: first by size, then by "
    "comparing their content. Files in the Windows folder, system files, the Recycle Bin, online-only "
    "OneDrive files and extra hard links are left out. Cairn never deletes files: show copies in File "
    "Explorer and delete the ones you don't need."
)
NO_SCAN_TEXT = "Scan a drive or folder on the Space page first."
RESULTS_GONE_TEXT = "This scan's results were cleared; scan again."
NO_HISTORY_TEXT = "No earlier results."
TREE_HINT = "Double-click a folder to open it here."
NO_DRIVES_TEXT = "No fixed drives found"
LOADING_TEXT = "Reading the drives…"
ENGINE_MISSING_TEXT = "⚠ The engine is not available, so the storage tools can't run."
NOT_REACHED_TEXT = (
    "{size} of used space wasn't reached: NTFS metadata, restore points and shadow copies, and folders "
    "Cairn can't read."
)
PHASE_TEXTS = {
    "pausing": "Pausing between measurements…",
    "cleaning_up": "Deleting the test file…",
    "saving": "Saving the result…",
}
# The eighth blocks from one eighth to seven eighths.
PARTIAL_BLOCKS = "▏▎▍▌▋▊▉"
FULL_BLOCK = "█"
# Rows of the history card.
HISTORY_ROWS = 20
# Duplicate groups shown opened.
OPEN_GROUPS = 5
# Width of the tree's text; the tree style's row height and font are set from these.
TREE_ROW_HEIGHT = 24
TREE_FONT_SIZE = 12
TREE_STYLE = "Cairn.Treeview"
# Room kept around the texts of a card, in logical pixels.
CARD_TEXT_PAD = 2 * 14
INTRO_WRAP = 860
# Least height the speed page asks for its scrolling results; they get the room that is left.
BODY_MIN_HEIGHT = 120


def _font(size: int, weight: str = "normal") -> ctk.CTkFont:
    return ctk.CTkFont(family=theme.FONT_FAMILY, size=size, weight=weight)


# -- pure helpers --------------------------------------------------------------------


def count_text(count: int, noun: str, plural: str | None = None) -> str:
    """ "1 file", "182,340 files"."""
    word = noun if count == 1 else (plural or f"{noun}s")
    return f"{count:,} {word}"


def fmt_mb_s(value: float | None) -> str:
    """Throughput in MB/s: "7,012" from 100, "92.1" from 10, else "1.23"; "–" when unknown."""
    if value is None:
        return "–"
    v = float(value)
    if v >= 100:
        return f"{v:,.0f}"
    if v >= 10:
        return f"{v:.1f}"
    return f"{v:.2f}"


def fmt_iops(value: float | None) -> str:
    """I/O operations per second: "857k" from 10,000, else "3,512"; "–" when unknown."""
    if value is None:
        return "–"
    v = float(value)
    if v >= 10_000:
        return f"{v / 1000:,.0f}k"
    return f"{v:,.0f}"


def fmt_latency(us: float | None) -> str:
    """Average latency: "37 µs" below a millisecond, else "1.2 ms"; "–" when unknown."""
    if us is None:
        return "–"
    v = float(us)
    if v < 1000:
        return f"{v:.0f} µs"
    ms = v / 1000
    return f"{ms:.1f} ms" if ms < 100 else f"{ms:,.0f} ms"


def estimate_text(seconds: float) -> str:
    """ "about a minute", "about 3 minutes", "about 2 hours"."""
    minutes = round(max(0.0, float(seconds)) / 60)
    if minutes <= 1:
        return "about a minute"
    if minutes < 120:
        return f"about {minutes} minutes"
    return f"about {round(minutes / 60)} hours"


def max_write_bytes(size: int, runs: int) -> int:
    """Bytes a speed test may write: the test file, then up to one pass in each of the four
    write measurements of every run."""
    return int(size) * (1 + 4 * int(runs))


def is_hdd(volume: Mapping[str, Any] | None) -> bool:
    return str((volume or {}).get("media") or "").lower() == "hdd"


def write_warning(volume: Mapping[str, Any], size: int, runs: int) -> str:
    """The warning shown under the speed-test controls whenever a drive is chosen."""
    letter = volume_letter(volume)
    text = (
        f"⚠ Writes up to {size_text(max_write_bytes(size, runs))} to {letter} the {size_text(size)} test "
        f"file, then up to {size_text(size)} in each of the {4 * int(runs)} write measurements."
    )
    if not is_hdd(volume):
        text += " On an SSD this uses a little of its write endurance, so don't run it often."
    return text


def disk_text(volume: Mapping[str, Any]) -> str:
    """ "NVMe SSD", "SATA HDD", "SSD"; "" when unknown."""
    media = MEDIA_TEXT.get(str(volume.get("media") or "").lower(), "")
    bus = str(volume.get("bus") or "").strip()
    return " ".join(p for p in (bus, media) if p)


def storage_volume_text(volume: Mapping[str, Any]) -> str:
    """The drive as "C:  Windows  ·  NVMe SSD  ·  611 GB free of 952 GB"."""
    head = volume_letter(volume)
    label = str(volume.get("label") or "").strip()
    if label:
        head += f"  {label}"
    parts = [head]
    disk = disk_text(volume)
    if disk:
        parts.append(disk)
    if volume.get("error"):
        parts.append(f"⚠ {volume['error']}")
    elif volume.get("size_bytes"):
        free = size_text(int(volume.get("free_bytes") or 0))
        parts.append(f"{free} free of {size_text(int(volume['size_bytes']))}")
    return "  ·  ".join(parts)


def short_path(path: str, max_chars: int = 48) -> str:
    """`path` shortened in the middle at a separator: "C:\\Users\\…\\Local\\Packages"."""
    text = str(path)
    if len(text) <= max_chars:
        return text
    parts = text.split("\\")
    head_count = 2 if len(parts) > 3 else 1
    head = "\\".join(parts[:head_count])
    tail: list[str] = []
    for part in reversed(parts[head_count:]):
        candidate = "\\".join([part, *tail])
        if len(head) + 3 + len(candidate) > max_chars:
            break
        tail.insert(0, part)
    if not tail:
        return "…" + text[-(max_chars - 1) :]
    return head + "\\…\\" + "\\".join(tail)


def share_bar(fraction: float, cells: int = 10) -> str:
    """A bar of `cells` full blocks for 1.0, with an eighth-block remainder."""
    value = min(1.0, max(0.0, float(fraction)))
    eighths = round(value * cells * 8)
    full, rest = divmod(eighths, 8)
    return FULL_BLOCK * full + (PARTIAL_BLOCKS[rest - 1] if rest else "")


def share_text(value: int, whole: int) -> str:
    """The bar and percentage of `value` in `whole`: "████▎  42 %"."""
    fraction = value / whole if whole > 0 else 0.0
    return f"{share_bar(fraction)}  {fraction * 100:.0f} %"


def sizes_of(done: int, total: int) -> str:
    """ "3.2 of 18.4 GB": both amounts in the unit of `total`."""
    number, _, unit = size_text(total).partition(" ")
    scale = {"B": 1, "KB": 1024, "MB": MIB, "GB": GIB}.get(unit, 1)
    part = done / scale
    part_text = f"{part:.0f}" if unit in ("B", "KB") else f"{part:.1f}"
    if part_text.endswith(".0"):
        part_text = part_text[:-2]
    return f"{part_text} of {number} {unit}"


def date_text(stamp: str | None, *, with_time: bool = True) -> str:
    """An RFC 3339 time in local time: "28 Sep 2026 14:02" (or "28 Sep 2026"); "" when unknown."""
    if not stamp:
        return ""
    try:
        moment = datetime.fromisoformat(str(stamp)).astimezone()
    except ValueError:
        return str(stamp)
    text = f"{moment.day} {moment:%b %Y}"
    return f"{text} {moment:%H:%M}" if with_time else text


def time_of_day(stamp: str | None) -> str:
    """ "12:03" in local time; "" when unknown."""
    if not stamp:
        return ""
    try:
        return f"{datetime.fromisoformat(str(stamp)).astimezone():%H:%M}"
    except ValueError:
        return ""


def _detail(view: Mapping[str, Any]) -> dict[str, Any]:
    detail = view.get("detail")
    return detail if isinstance(detail, dict) else {}


def job_target(view: Mapping[str, Any]) -> str:
    """What a storage job works on, from its title: "C:" or the scanned folder."""
    title = str(view.get("title") or "")
    for prefix in ("Speed test of ", "Scan of ", "Duplicate search in "):
        if title.startswith(prefix):
            return title[len(prefix) :]
    return title


def speed_progress_text(view: Mapping[str, Any], size_bytes: int | None = None) -> str:
    """The progress line of a running speed test."""
    detail = _detail(view)
    phase = detail.get("phase")
    speed = detail.get("speed") or {}
    if phase == "preparing":
        written = float(speed.get("bytes_written") or 0)
        percent = written / size_bytes * 100 if size_bytes else float(view.get("progress") or 0)
        return f"Preparing the test file…  {min(100.0, percent):.0f} %"
    if phase == "measuring":
        parts = [TEST_LABELS.get(str(speed.get("test")), "Measuring")]
        if speed.get("direction"):
            parts.append(str(speed["direction"]))
        if speed.get("run"):
            parts.append(f"run {speed['run']} of {speed.get('runs') or speed['run']}")
        if speed.get("live_mb_s") is not None:
            parts.append(f"{fmt_mb_s(speed['live_mb_s'])} MB/s")
        return "  ·  ".join(parts)
    return PHASE_TEXTS.get(str(phase), "")


def scan_progress_text(view: Mapping[str, Any]) -> str:
    """The progress line of a running scan."""
    detail = _detail(view)
    target = job_target(view)
    if detail.get("phase") == "summarizing":
        return f"◐ Summarizing the scan of {target}…"
    scan = detail.get("scan") or {}
    text = (
        f"◐ Scanning {target}…  {count_text(int(scan.get('files') or 0), 'file')} in "
        f"{count_text(int(scan.get('folders') or 0), 'folder')}  ·  "
        f"{size_text(int(scan.get('allocated_bytes') or 0))}"
    )
    current = str(scan.get("current") or "")
    if current:
        text += f"  ·  {short_path(current, 48)}"
    return text


def dupes_progress_text(view: Mapping[str, Any]) -> str:
    """The progress line of a running duplicate search."""
    detail = _detail(view)
    block = detail.get("duplicates") or {}
    total = int(block.get("bytes_total") or 0)
    done = int(block.get("bytes_done") or 0)
    percent = done / total * 100 if total else 0.0
    files = int(block.get("files_total") or 0)
    if detail.get("phase") == "hashing":
        return f"◐ Comparing {files:,} files in full  ·  {sizes_of(done, total)} read  ·  {percent:.0f} %"
    return f"◐ Comparing the start and end of {files:,} files that share a size…  {percent:.0f} %"


def storage_status_text(view: Mapping[str, Any]) -> str:
    """The status bar text of a running storage job: "◐ Speed test C:  ·  42 %", "◐ Scanning
    C:  ·  182,340 files", "◐ Finding duplicates  ·  41 %"."""
    kind = view.get("kind")
    progress = view.get("progress")
    percent = f"  ·  {float(progress):.0f} %" if progress is not None else ""
    if kind == KIND_SPEED:
        return f"◐ Speed test {job_target(view)}{percent}"
    if kind == KIND_SCAN:
        scan = _detail(view).get("scan") or {}
        return f"◐ Scanning {job_target(view)}  ·  {count_text(int(scan.get('files') or 0), 'file')}"
    return f"◐ Finding duplicates{percent}"


def scan_summary_lines(result: Mapping[str, Any]) -> list[str]:
    """What a scan found: the headline, then notes on what it could not count."""
    s = result.get("summary") or {}
    target = str(s.get("volume") if s.get("whole_volume") else s.get("root") or "")
    completed = bool(s.get("completed"))
    lines = [
        f"{'✓' if completed else '○'} {target}  ·  {size_text(int(s.get('allocated_bytes') or 0))} on disk "
        f"in {count_text(int(s.get('files') or 0), 'file')} and "
        f"{count_text(int(s.get('folders') or 0), 'folder')}  ·  scanned in "
        f"{fmt_duration(float(s.get('elapsed_ms') or 0))}"
    ]
    not_reached = int(s.get("not_reached_bytes") or 0)
    if s.get("whole_volume") and not_reached > 0:
        lines.append(NOT_REACHED_TEXT.format(size=size_text(not_reached)))
    unreadable = int(s.get("denied_folders") or 0) + int(s.get("unreadable_folders") or 0)
    if unreadable:
        lines.append(f"{count_text(unreadable, 'folder')} can't be read.")
    online = int(s.get("online_only_bytes") or 0)
    if online:
        lines.append(f"Online-only OneDrive files: {size_text(online)}, not stored on this PC.")
    links = int(s.get("hard_links_counted_once") or 0)
    if links:
        verb = "is" if links == 1 else "are"
        lines.append(f"{count_text(links, 'hard link')} {verb} counted once.")
    skipped = int(s.get("links_skipped") or 0)
    if skipped == 1:
        lines.append("1 link to another folder wasn't followed.")
    elif skipped:
        lines.append(f"{skipped:,} links to other folders weren't followed.")
    if not completed:
        lines.append("Stopped: the numbers cover only what was scanned.")
    if s.get("id_limit_reached"):
        lines.append("Too many files to track hard links beyond the first 8 million.")
    if s.get("node_limit_reached"):
        lines.append("Folder limit reached; deeper folders weren't listed.")
    if s.get("big_file_limit_reached"):
        lines.append("Large-file limit reached; later large files are counted with the smaller files.")
    return lines


def dupes_summary_lines(result: Mapping[str, Any]) -> list[str]:
    """What a duplicate search found: the headline, then what it left out."""
    groups = int(result.get("group_count") or 0)
    completed = bool(result.get("completed", True))
    icon = "✓" if completed else "○"
    if groups:
        lines = [
            f"{icon} {count_text(groups, 'group')} of identical files  ·  "
            f"{size_text(int(result.get('wasted_bytes') or 0))} would be freed by keeping one copy of each"
        ]
    else:
        lines = [f"{icon} No duplicate files of {size_text(int(result.get('min_size') or MIB))} or more."]
    unreadable = int(result.get("skipped_in_use") or 0) + int(result.get("skipped_unreadable") or 0)
    changed = int(result.get("skipped_changed") or 0)
    if unreadable and changed:
        lines.append(
            f"{count_text(unreadable, 'file')} couldn't be read (in use or access denied) and {changed:,} "
            "changed since the scan; they were left out."
        )
    elif unreadable:
        lines.append(
            f"{count_text(unreadable, 'file')} couldn't be read (in use or access denied); "
            f"{'it was' if unreadable == 1 else 'they were'} left out."
        )
    elif changed:
        lines.append(
            f"{count_text(changed, 'file')} changed since the scan; "
            f"{'it was' if changed == 1 else 'they were'} left out."
        )
    online = int(result.get("skipped_online_only") or 0)
    if online:
        lines.append(f"{count_text(online, 'online-only file')} {'was' if online == 1 else 'were'} left out.")
    if not completed:
        lines.append("Stopped: only the files compared so far are listed.")
    if groups > len(result.get("groups") or []):
        lines.append(f"Showing the {len(result.get('groups') or []):,} largest groups.")
    return lines


def group_text(group: Mapping[str, Any]) -> str:
    """ "4 copies  ·  1.2 GB each  ·  3.6 GB extra"."""
    count = int(group.get("count") or 0)
    return (
        f"{count_text(count, 'copy', 'copies')}  ·  {size_text(int(group.get('size') or 0))} each  ·  "
        f"{size_text(int(group.get('wasted') or 0))} extra"
    )


def measurement(result: Mapping[str, Any], test: str, direction: str) -> dict[str, Any] | None:
    """The measurement of `test` ("seq1m_q8t1") in `direction` ("read" or "write")."""
    for m in result.get("measurements") or []:
        if m.get("test") == test and m.get("direction") == direction:
            return dict(m)
    return None


def speed_result_text(result: Mapping[str, Any]) -> str:
    """A speed-test result as plain text for the clipboard."""
    letter = str(result.get("volume") or "")
    disk = str(result.get("model") or "").strip()
    if result.get("bus"):
        disk = f"{disk} ({result['bus']})" if disk else str(result["bus"])
    head = f"{APP_NAME} disk speed test  ·  {letter}"
    if disk:
        head += f" {disk}"
    if result.get("file_system"):
        head += f"  ·  {result['file_system']}"
    runs = int(result.get("runs") or 0)
    lines = [
        head,
        f"{date_text(result.get('started_at'))}  ·  "
        f"{size_text(int(result.get('size_bytes') or 0))} test file  ·  "
        f"best of {count_text(runs, 'run')}  ·  MB/s (1 MB = 1,000,000 bytes)",
        "",
        f"{'':<13}{'Read':>10}{'Write':>10}{'Read IOPS':>12}{'Write IOPS':>12}"
        f"{'Read lat.':>11}{'Write lat.':>11}",
    ]
    skipped = set(result.get("skipped") or [])
    for test, label in TEST_ROWS:
        if test in skipped:
            lines.append(f"{label:<13}{'skipped':>10}")
            continue
        read = measurement(result, test, "read") or {}
        write = measurement(result, test, "write") or {}
        lines.append(
            f"{label:<13}{fmt_mb_s(read.get('mb_s')):>10}{fmt_mb_s(write.get('mb_s')):>10}"
            f"{fmt_iops(read.get('iops')):>12}{fmt_iops(write.get('iops')):>12}"
            f"{fmt_latency(read.get('latency_us')):>11}{fmt_latency(write.get('latency_us')):>11}"
        )
    if result.get("error"):
        lines += ["", f"Failed before every measurement ran: {result['error']}"]
    elif not result.get("completed", True):
        lines += ["", "Stopped before every measurement ran."]
    return "\n".join(lines)


def history_row_lines(entry: Mapping[str, Any]) -> tuple[str, str]:
    """The two lines of an earlier result in the history card; a test an error ended early is
    marked "failed", one stopped before every measurement ran "stopped"."""
    first = (
        f"{date_text(entry.get('started_at'))}  ·  {entry.get('volume') or ''}  ·  "
        f"{size_text(int(entry.get('size_bytes') or 0))} × {int(entry.get('runs') or 0)}"
    )
    if entry.get("error"):
        first += "  ·  failed"
    elif not entry.get("completed", True):
        first += "  ·  stopped"

    def mb(test: str, direction: str) -> str:
        found = measurement(entry, test, direction)
        return fmt_mb_s(found.get("mb_s")) if found else "–"

    second = (
        f"SEQ {mb('seq1m_q8t1', 'read')} / {mb('seq1m_q8t1', 'write')}  ·  "
        f"RND4K Q1 {mb('rnd4k_q1t1', 'read')} / {mb('rnd4k_q1t1', 'write')}"
    )
    return first, second


def speed_done_line(view: Mapping[str, Any], result: Mapping[str, Any] | None) -> tuple[str, str]:
    """(text, colour) of the line under the results once a speed test ended."""
    state = view.get("state")
    elapsed = fmt_duration(float(view.get("elapsed_ms") or 0))
    if state == "succeeded":
        runs = int((result or {}).get("runs") or 0)
        parts = [f"✓ Finished in {elapsed}"]
        if result:
            parts += [
                f"{size_text(int(result.get('size_bytes') or 0))} test file",
                f"best of {count_text(runs, 'run')}",
                f"wrote {size_text(int(result.get('bytes_written') or 0))}",
            ]
        return "  ·  ".join(parts), theme.GOOD
    if state == "cancelled":
        return (
            f"○ Stopped after {elapsed}; the test file was deleted. Results measured so far are shown.",
            theme.INK_SECONDARY,
        )
    return f"⚠ Failed: {view.get('summary') or 'the test did not finish'}", theme.CRITICAL


def ensure_tree_style(widget: tk.Misc, scale: float) -> None:
    """Configures the `Cairn.Treeview` style on the "clam" theme at `scale`."""
    style = ttk.Style(widget)
    if style.theme_use() != "clam":
        style.theme_use("clam")
    font = (theme.FONT_FAMILY, -round(TREE_FONT_SIZE * scale))
    style.configure(
        TREE_STYLE,
        background=theme.SURFACE,
        fieldbackground=theme.SURFACE,
        foreground=theme.INK,
        bordercolor=theme.BORDER,
        lightcolor=theme.SURFACE,
        darkcolor=theme.SURFACE,
        borderwidth=0,
        rowheight=round(TREE_ROW_HEIGHT * scale),
        font=font,
    )
    style.configure(
        f"{TREE_STYLE}.Heading",
        background=theme.SURFACE_RAISED,
        foreground=theme.INK_SECONDARY,
        bordercolor=theme.BORDER,
        lightcolor=theme.SURFACE_RAISED,
        darkcolor=theme.SURFACE_RAISED,
        relief="flat",
        font=(theme.FONT_FAMILY, -round(TREE_FONT_SIZE * scale), "bold"),
    )
    style.map(TREE_STYLE, background=[("selected", theme.ACCENT)], foreground=[("selected", theme.INK)])
    style.map(f"{TREE_STYLE}.Heading", background=[("active", theme.BUTTON_NEUTRAL_HOVER)])


# -- small widget helpers ------------------------------------------------------------


def _set_state(widget: Any, enabled: bool) -> None:
    state = "normal" if enabled else "disabled"
    if widget.cget("state") != state:
        widget.configure(state=state)


def _set_color(label: ctk.CTkLabel, color: str) -> None:
    if label.cget("text_color") != color:
        label.configure(text_color=color)


def _show(widget: Any, shown: bool, **grid: Any) -> None:
    """Grids `widget` with `grid` options, or forgets it."""
    if shown and not widget.winfo_manager():
        widget.grid(**grid)
    elif not shown and widget.winfo_manager():
        widget.grid_forget()


def _label(
    master: tk.Misc,
    text: str = "",
    size: int = 11,
    color: str = theme.INK_MUTED,
    *,
    weight: str = "normal",
    **kw: Any,
) -> ctk.CTkLabel:
    options: dict[str, Any] = {"anchor": "w", "justify": "left"}
    options.update(kw)
    return ctk.CTkLabel(master, text=text, font=_font(size, weight), text_color=color, **options)


def _neutral_button(master: tk.Misc, text: str, command: Callable[[], None], **kw: Any) -> ctk.CTkButton:
    options: dict[str, Any] = {
        "height": 30,
        "font": _font(12),
        "fg_color": theme.BUTTON_NEUTRAL,
        "hover_color": theme.BUTTON_NEUTRAL_HOVER,
        "text_color": theme.INK,
    }
    options.update(kw)
    return ctk.CTkButton(master, text=text, command=command, **options)


def _action_button(master: tk.Misc, text: str, command: Callable[[], None], width: int) -> ctk.CTkButton:
    return ctk.CTkButton(
        master,
        text=text,
        command=command,
        width=width,
        height=30,
        font=_font(12, "bold"),
        fg_color=theme.ACCENT,
        hover_color=theme.ACCENT_HOVER,
        text_color=theme.INK,
    )


def _set_action(button: ctk.CTkButton, text: str, *, stop: bool) -> None:
    """Shows the button as its start action or, while its job runs, as "Stop"."""
    color, hover = (theme.CRITICAL, theme.CRITICAL_HOVER) if stop else (theme.ACCENT, theme.ACCENT_HOVER)
    if button.cget("text") != text:
        button.configure(text=text)
    if button.cget("fg_color") != color:
        button.configure(fg_color=color, hover_color=hover)


def _menu(
    master: tk.Misc, values: Sequence[str], width: int, command: Callable[[str], None]
) -> ctk.CTkOptionMenu:
    return ctk.CTkOptionMenu(
        master,
        values=list(values),
        width=width,
        height=28,
        dynamic_resizing=False,
        font=_font(11),
        fg_color=theme.BUTTON_NEUTRAL,
        button_color=theme.BASELINE,
        button_hover_color=theme.INK_MUTED,
        text_color=theme.INK,
        dropdown_fg_color=theme.SURFACE_RAISED,
        dropdown_hover_color=theme.BUTTON_NEUTRAL_HOVER,
        dropdown_text_color=theme.INK,
        dropdown_font=_font(11),
        command=command,
    )


def _segments(
    master: tk.Misc, values: Sequence[str], command: Callable[[str], None], size: int = 11
) -> ctk.CTkSegmentedButton:
    return ctk.CTkSegmentedButton(
        master,
        values=list(values),
        font=_font(size, "bold" if size >= 12 else "normal"),
        height=30 if size >= 12 else 28,
        selected_color=theme.ACCENT,
        selected_hover_color=theme.ACCENT_HOVER,
        unselected_color=theme.SURFACE_RAISED,
        unselected_hover_color=theme.BUTTON_NEUTRAL_HOVER,
        command=command,
    )


def _card(master: tk.Misc) -> ctk.CTkFrame:
    frame = ctk.CTkFrame(
        master, fg_color=theme.SURFACE, corner_radius=10, border_width=1, border_color=theme.BORDER
    )
    frame.grid_columnconfigure(0, weight=1)
    return frame


class _Wraps:
    """Wraps labels to the width of the card they sit in, whenever it changes."""

    def __init__(self, card: ctk.CTkFrame, pad: int = CARD_TEXT_PAD) -> None:
        self.card = card
        self.pad = pad
        self.labels: list[tuple[ctk.CTkLabel, int, int]] = []
        self._last: int | None = None
        card.bind("<Configure>", self._fit, add="+")

    def add(self, label: ctk.CTkLabel, widest: int = INTRO_WRAP, less: int = 0) -> ctk.CTkLabel:
        """Wraps `label` at the card's width less `less` units, at most `widest`."""
        self.labels = [entry for entry in self.labels if entry[0].winfo_exists()]
        self.labels.append((label, widest, less))
        wrap = widest if self._last is None else fitted_wrap(self._last - less, widest)
        label.configure(wraplength=wrap)
        return label

    def _fit(self, event: tk.Event) -> None:
        width = int(event.width / self.card._get_widget_scaling()) - self.pad
        if width == self._last:
            return
        self._last = width
        self.labels = [entry for entry in self.labels if entry[0].winfo_exists()]
        for label, widest, less in self.labels:
            wrap = fitted_wrap(width - less, widest)
            if label.cget("wraplength") != wrap:
                label.configure(wraplength=wrap)


# -- the folder tree -----------------------------------------------------------------


class SizeTree(ctk.CTkFrame):
    """A `ttk.Treeview` with a scrollbar, in one of three modes: "folders" (a folder tree
    whose children are read when a folder opens), "largest" (a flat list of files) and
    "dupes" (duplicate groups with their files).

    Folders and links get the iid `n<node>`; a folder's files `f<node>:<i>`; its smaller files
    and the rest past the page `s<node>` and `m<node>`; a placeholder child `p<node>` marks a
    folder whose children are not read yet. `rows` maps each iid to its row.
    """

    COLUMNS = ("folder", "disk", "size", "share", "files", "modified")
    MODES: dict[str, tuple[tuple[str, str, int], tuple[tuple[str, str, int, str], ...]]] = {
        "folders": (
            ("Name", "w", 330),
            (
                ("disk", "Size on disk", 100, "e"),
                ("size", "Size", 100, "e"),
                # Room for a full bar and "100 %".
                ("share", "Share", 175, "w"),
                ("files", "Files", 90, "e"),
            ),
        ),
        "largest": (
            ("Name", "w", 220),
            (
                ("folder", "Folder", 300, "w"),
                ("disk", "Size on disk", 90, "e"),
                ("size", "Size", 90, "e"),
                ("modified", "Modified", 100, "w"),
            ),
        ),
        "dupes": (
            ("File", "w", 520),
            (("modified", "Modified", 110, "w"), ("disk", "Size on disk", 100, "e")),
        ),
    }

    def __init__(
        self,
        master: tk.Misc,
        *,
        mode: str,
        on_children: Callable[[int], dict[str, Any] | None] | None = None,
        on_select: Callable[[dict[str, Any] | None], None] | None = None,
    ) -> None:
        super().__init__(
            master, fg_color=theme.SURFACE, corner_radius=10, border_width=1, border_color=theme.BORDER
        )
        self._on_children = on_children
        self._on_select = on_select
        self._scale = ctk.ScalingTracker.get_widget_scaling(self)
        self._rows: dict[str, dict[str, Any]] = {}
        self._order = "allocated"
        self.mode = mode
        self.grid_columnconfigure(0, weight=1)
        self.grid_rowconfigure(0, weight=1)
        ensure_tree_style(self, self._scale)
        self.tree = ttk.Treeview(self, columns=self.COLUMNS, style=TREE_STYLE, selectmode="browse", height=6)
        self.tree.grid(row=0, column=0, sticky="nsew", padx=(6, 0), pady=6)
        self.scrollbar = ctk.CTkScrollbar(
            self,
            command=self.tree.yview,
            button_color=theme.BASELINE,
            button_hover_color=theme.INK_MUTED,
        )
        self.scrollbar.grid(row=0, column=1, sticky="ns", padx=(0, 4), pady=6)
        self.tree.configure(yscrollcommand=self.scrollbar.set)
        self.tree.tag_configure("muted", foreground=theme.INK_MUTED)
        self.tree.tag_configure("denied", foreground=theme.INK_MUTED)
        self.tree.bind("<<TreeviewOpen>>", self._opened)
        self.tree.bind("<<TreeviewSelect>>", self._selected)
        self.tree.bind("<Double-Button-1>", self._toggle)
        self.tree.bind("<Return>", self._toggle)
        self.set_mode(mode)

    def _set_scaling(self, new_widget_scaling: float, new_window_scaling: float) -> None:
        super()._set_scaling(new_widget_scaling, new_window_scaling)
        if new_widget_scaling == self._scale:
            return
        self._scale = new_widget_scaling
        ensure_tree_style(self, self._scale)
        self.set_mode(self.mode)

    def _px(self, value: float) -> int:
        return round(value * self._scale)

    @property
    def rows(self) -> dict[str, dict[str, Any]]:
        """Every row shown, by iid."""
        return self._rows

    def set_mode(self, mode: str) -> None:
        """Shows the columns of `mode` ("folders", "largest" or "dupes")."""
        self.mode = mode
        (name, anchor, width), columns = self.MODES[mode]
        self.tree.configure(displaycolumns=[c[0] for c in columns], show="tree headings")
        self.tree.heading("#0", text=name, anchor=anchor)
        self.tree.column("#0", width=self._px(width), minwidth=self._px(120), stretch=True, anchor=anchor)
        for column, heading, col_width, col_anchor in columns:
            self.tree.heading(column, text=heading, anchor=col_anchor)
            self.tree.column(
                column, width=self._px(col_width), minwidth=self._px(50), stretch=False, anchor=col_anchor
            )

    def clear(self) -> None:
        children = self.tree.get_children("")
        if children:
            self.tree.delete(*children)
        self._rows = {}

    # -- rows ----------------------------------------------------------------------

    def _key(self, row: Mapping[str, Any]) -> int:
        return int(row.get(self._order) or 0)

    def _values(self, row: Mapping[str, Any], whole: int) -> dict[str, str]:
        kind = row.get("kind")
        link = kind == "link"
        if link:
            disk = size = share = "–"
        else:
            online = kind == "file" and int(row.get("online_only") or 0) > 0 and not row.get("allocated")
            disk = "online-only" if online else size_text(int(row.get("allocated") or 0))
            size = size_text(int(row.get("logical") or 0))
            share = share_text(self._key(row), whole)
        files = "" if kind in ("file", "link") else f"{int(row.get('files') or 0):,}"
        path = str(row.get("path") or "")
        folder = path.rsplit("\\", 1)[0] if "\\" in path else ""
        return {
            "folder": folder,
            "disk": disk,
            "size": size,
            "share": share,
            "files": files,
            "modified": date_text(row.get("modified"), with_time=False),
        }

    @staticmethod
    def unreadable(row: Mapping[str, Any]) -> bool:
        """A folder the scan could not list (access denied, or another error); shown as
        "(can't read)" and never opened in File Explorer."""
        return bool(row.get("denied") or row.get("error"))

    @staticmethod
    def row_name(row: Mapping[str, Any]) -> str:
        name = str(row.get("name") or "")
        if row.get("kind") == "link":
            return f"{name}  (link, not followed)"
        if SizeTree.unreadable(row):
            return f"{name}  (can't read)"
        return name

    @staticmethod
    def _tags(row: Mapping[str, Any]) -> tuple[str, ...]:
        if row.get("kind") in ("small_files", "more", "link"):
            return ("muted",)
        if SizeTree.unreadable(row):
            return ("denied",)
        return ()

    def _insert(self, parent: str, iid: str, row: dict[str, Any], whole: int, *, open_: bool = False) -> None:
        values = self._values(row, whole)
        self.tree.insert(
            parent,
            "end",
            iid=iid,
            text=self.row_name(row),
            values=[values[c] for c in self.COLUMNS],
            tags=self._tags(row),
            open=open_,
        )
        self._rows[iid] = row
        node = row.get("node")
        if row.get("kind") == "folder" and row.get("has_children") and node is not None and not open_:
            self.tree.insert(iid, "end", iid=f"p{node}", text="…")

    def _insert_page(self, parent_iid: str, page: Mapping[str, Any]) -> None:
        parent = page.get("node") or {}
        node = parent.get("node")
        whole = self._key(parent)
        files = 0
        for row in page.get("children") or []:
            kind = row.get("kind")
            if kind in ("folder", "link"):
                iid = f"n{row.get('node')}"
            elif kind == "small_files":
                iid = f"s{node}"
            elif kind == "more":
                iid = f"m{node}"
            else:
                iid = f"f{node}:{files}"
                files += 1
            self._insert(parent_iid, iid, dict(row), whole)

    def show_folders(self, page: Mapping[str, Any], order: str) -> None:
        """Shows the scanned folder, opened, with the entries of `page`."""
        self._order = order
        self.set_mode("folders")
        self.clear()
        root = dict(page.get("node") or {})
        iid = f"n{root.get('node', 0)}"
        self._insert("", iid, root, self._key(root), open_=True)
        self._insert_page(iid, page)

    def show_largest(self, rows: Sequence[Mapping[str, Any]], order: str) -> None:
        self._order = order
        self.set_mode("largest")
        self.clear()
        whole = sum(self._key(r) for r in rows)
        for i, row in enumerate(rows):
            self._insert("", f"l{i}", dict(row), whole)

    def show_groups(self, groups: Sequence[Mapping[str, Any]]) -> None:
        self.set_mode("dupes")
        self.clear()
        for i, group in enumerate(groups):
            iid = f"g{i}"
            self.tree.insert("", "end", iid=iid, text=group_text(group), open=i < OPEN_GROUPS)
            self._rows[iid] = {"kind": "group", **dict(group)}
            for j, file in enumerate(group.get("files") or []):
                row = {"kind": "file", **dict(file)}
                self.tree.insert(
                    iid,
                    "end",
                    iid=f"{iid}:{j}",
                    text=str(file.get("path") or ""),
                    values=[
                        "",
                        size_text(int(file.get("allocated") or 0)),
                        "",
                        "",
                        "",
                        date_text(file.get("modified"), with_time=False),
                    ],
                )
                self._rows[f"{iid}:{j}"] = row
            more = int(group.get("more_files") or 0)
            if more:
                self.tree.insert(iid, "end", iid=f"{iid}:more", text=f"… and {more:,} more", tags=("muted",))
                self._rows[f"{iid}:more"] = {"kind": "more", "count": more}

    # -- events --------------------------------------------------------------------

    def load_children(self, iid: str) -> bool:
        """Reads the children of the folder `iid` if they are not shown yet; False when the
        scan's results are gone."""
        row = self._rows.get(iid) or {}
        node = row.get("node")
        placeholder = f"p{node}"
        if node is None or not self.tree.exists(placeholder):
            return True
        if self._on_children is None:
            return True
        page = self._on_children(int(node))
        if page is None:
            return False
        if self.tree.exists(placeholder):
            self.tree.delete(placeholder)
        self._insert_page(iid, page)
        return True

    def open_folder(self, iid: str) -> None:
        """Opens the folder `iid`, reading its children first."""
        if self.load_children(iid) and self.tree.exists(iid):
            self.tree.item(iid, open=True)

    def _opened(self, _event: tk.Event) -> None:
        iid = self.tree.focus()
        if iid:
            self.load_children(iid)

    def _toggle(self, _event: tk.Event | None = None) -> str:
        iid = self.tree.focus()
        if iid and self.tree.get_children(iid):
            if self.tree.item(iid, "open"):
                self.tree.item(iid, open=False)
            else:
                self.open_folder(iid)
        return "break"

    def _selected(self, _event: tk.Event | None = None) -> None:
        if self._on_select is not None:
            self._on_select(self.selected_row())

    def select(self, iid: str) -> None:
        """Selects and focuses the row `iid`."""
        self.tree.selection_set(iid)
        self.tree.focus(iid)
        self._selected()

    def selected_row(self) -> dict[str, Any] | None:
        chosen = self.tree.selection()
        return self._rows.get(chosen[0]) if chosen else None


# -- speed test page -----------------------------------------------------------------


class HistoryCard(ctk.CTkFrame):
    """Earlier speed-test results, newest first, as plain Tk labels at the window's scaling;
    clicking one shows it in the results card."""

    def __init__(
        self,
        master: tk.Misc,
        *,
        on_pick: Callable[[dict[str, Any]], None],
        on_copy: Callable[[], None],
    ) -> None:
        super().__init__(
            master, fg_color=theme.SURFACE, corner_radius=10, border_width=1, border_color=theme.BORDER
        )
        self._on_pick = on_pick
        self._scale = ctk.ScalingTracker.get_widget_scaling(self)
        self._entries: list[dict[str, Any]] = []
        self._error: str | None = None
        self.grid_columnconfigure(0, weight=1)
        _label(self, "Earlier results", 13, theme.INK_SECONDARY, weight="bold").grid(
            row=0, column=0, sticky="w", padx=14, pady=(10, 4)
        )
        self.rows_frame = tk.Frame(self, bg=theme.SURFACE, highlightthickness=0, bd=0)
        self.rows_frame.grid(row=1, column=0, sticky="ew", padx=12)
        self.rows_frame.grid_columnconfigure(0, weight=1)
        self.empty_label = _label(self, NO_HISTORY_TEXT, 11, theme.INK_MUTED)
        self.empty_label.grid(row=2, column=0, sticky="w", padx=14)
        self.copy_button = _neutral_button(self, "Copy results", on_copy, width=120, state="disabled")
        self.copy_button.grid(row=3, column=0, sticky="w", padx=14, pady=10)
        self.row_widgets: list[tuple[tk.Frame, tk.Label, tk.Label]] = []
        self._wraps = _Wraps(self)
        self._wraps.add(self.empty_label, 360)

    def _set_scaling(self, new_widget_scaling: float, new_window_scaling: float) -> None:
        super()._set_scaling(new_widget_scaling, new_window_scaling)
        if new_widget_scaling == self._scale:
            return
        self._scale = new_widget_scaling
        self._build_rows()

    def _px(self, value: float) -> int:
        return round(value * self._scale)

    @property
    def entries(self) -> list[dict[str, Any]]:
        return self._entries

    def show(self, entries: Sequence[Mapping[str, Any]], error: str | None = None) -> None:
        self._entries = [dict(e) for e in entries][:HISTORY_ROWS]
        self._error = error
        self._build_rows()

    def _build_rows(self) -> None:
        for frame, _, _ in self.row_widgets:
            frame.destroy()
        self.row_widgets = []
        for i, entry in enumerate(self._entries):
            first, second = history_row_lines(entry)
            frame = tk.Frame(self.rows_frame, bg=theme.SURFACE, highlightthickness=0, bd=0, cursor="hand2")
            frame.grid(row=i, column=0, sticky="ew", pady=(0, self._px(6)))
            top = tk.Label(
                frame,
                text=first,
                font=(theme.FONT_FAMILY, -self._px(11), "bold"),
                fg=theme.INK_SECONDARY,
                bg=theme.SURFACE,
                anchor="w",
                cursor="hand2",
            )
            top.grid(row=0, column=0, sticky="w")
            bottom = tk.Label(
                frame,
                text=second,
                font=(theme.FONT_FAMILY, -self._px(10)),
                fg=theme.INK_MUTED,
                bg=theme.SURFACE,
                anchor="w",
                cursor="hand2",
            )
            bottom.grid(row=1, column=0, sticky="w")
            for widget in (frame, top, bottom):
                widget.bind("<Button-1>", lambda _e, entry=entry: self._on_pick(entry))
            self.row_widgets.append((frame, top, bottom))
        if self._error:
            set_text(self.empty_label, f"Earlier results can't be read: {self._error}")
            _set_color(self.empty_label, theme.CRITICAL)
        else:
            set_text(self.empty_label, NO_HISTORY_TEXT)
            _set_color(self.empty_label, theme.INK_MUTED)
        _show(self.empty_label, bool(self._error) or not self._entries, row=2, column=0, sticky="w", padx=14)

    def set_copy_enabled(self, enabled: bool) -> None:
        _set_state(self.copy_button, enabled)


class SpeedPage(ctk.CTkFrame):
    """The speed test: drive, size and runs, the write warning and leftovers, the results grid
    with its progress, and the earlier results.

    The controls stay in place at the top; the results and the earlier results scroll below
    them when the window is too low for both.
    """

    def __init__(
        self,
        master: tk.Misc,
        *,
        on_start: Callable[[str, int, int], None],
        on_stop: Callable[[], None],
        on_remove_leftover: Callable[[dict[str, Any]], None],
        on_copy: Callable[[str], None],
    ) -> None:
        super().__init__(master, fg_color="transparent")
        self._on_start = on_start
        self._on_stop = on_stop
        self._on_remove = on_remove_leftover
        self._on_copy = on_copy
        self._volumes: list[dict[str, Any]] = []
        self._volume_texts: dict[str, dict[str, Any]] = {}
        self._running = False
        self._job_size: int | None = None
        self._shown: dict[str, Any] | None = None
        # The result line tells how the last test ended (failed or lost) and no result came
        # with it: the earlier results do not replace that line until one is picked.
        self._keep_line = False
        # What `apply_state` was last told, applied again when another drive is chosen.
        self._state: dict[str, Any] = {"ready": False, "enabled": False, "running": None, "stopping": False}
        self.leftover_rows: list[tuple[ctk.CTkFrame, dict[str, Any], ctk.CTkButton | None]] = []
        self.grid_columnconfigure(0, weight=1)
        self.grid_rowconfigure(1, weight=1)
        self._build_controls()
        self._build_body()

    # -- layout --------------------------------------------------------------------

    def _build_controls(self) -> None:
        card = _card(self)
        card.grid(row=0, column=0, sticky="ew", pady=(0, 8))
        self.controls_card = card
        wraps = _Wraps(card)
        _label(card, "Disk speed test", 13, theme.INK_SECONDARY, weight="bold").grid(
            row=0, column=0, sticky="w", padx=14, pady=(10, 2)
        )
        self.intro_label = wraps.add(_label(card, SPEED_INTRO, 11, theme.INK_MUTED))
        self.intro_label.grid(row=1, column=0, sticky="w", padx=14)
        row = ctk.CTkFrame(card, fg_color="transparent")
        row.grid(row=2, column=0, sticky="w", padx=14, pady=(8, 4))
        _label(row, "Drive", 11, theme.INK_SECONDARY).grid(row=0, column=0, padx=(0, 6))
        self.drive_menu = _menu(row, [NO_DRIVES_TEXT], 300, self._changed)
        self.drive_menu.grid(row=0, column=1, padx=(0, 12))
        _label(row, "Size", 11, theme.INK_SECONDARY).grid(row=0, column=2, padx=(0, 6))
        self.size_menu = _menu(row, [t for t, _ in SIZE_CHOICES], 90, self._changed)
        self.size_menu.set(DEFAULT_SIZE)
        self.size_menu.grid(row=0, column=3, padx=(0, 12))
        _label(row, "Runs", 11, theme.INK_SECONDARY).grid(row=0, column=4, padx=(0, 6))
        self.runs_menu = _menu(row, list(RUN_CHOICES), 64, self._changed)
        self.runs_menu.set(DEFAULT_RUNS)
        self.runs_menu.grid(row=0, column=5, padx=(0, 12))
        self.start_button = _action_button(row, "Start test", self._start_or_stop, 110)
        self.start_button.grid(row=0, column=6)
        self.warning_label = wraps.add(_label(card, "", 11, theme.WARNING))
        self.blocked_label = wraps.add(_label(card, "", 11, theme.WARNING))
        self.leftover_box = ctk.CTkFrame(card, fg_color="transparent")
        self.leftover_box.grid_columnconfigure(0, weight=1)
        self._wraps = wraps
        ctk.CTkFrame(card, fg_color="transparent", height=6).grid(row=6, column=0)

    def _build_body(self) -> None:
        scroll = ctk.CTkScrollableFrame(
            self,
            fg_color="transparent",
            corner_radius=0,
            height=BODY_MIN_HEIGHT,
            scrollbar_button_color=theme.BASELINE,
            scrollbar_button_hover_color=theme.INK_MUTED,
        )
        scroll.grid(row=1, column=0, sticky="nsew")
        scroll.grid_columnconfigure(0, weight=1)
        self.results_scroll = scroll
        body = ctk.CTkFrame(scroll, fg_color="transparent")
        body.grid(row=0, column=0, sticky="ew", padx=(0, 6))
        body.grid_columnconfigure(0, weight=3, minsize=480)
        body.grid_columnconfigure(1, weight=2, minsize=280)
        results = _card(body)
        results.grid(row=0, column=0, sticky="nsew", padx=(0, 4))
        self.results_card = results
        wraps = _Wraps(results)
        _label(results, "Results", 13, theme.INK_SECONDARY, weight="bold").grid(
            row=0, column=0, sticky="w", padx=14, pady=(10, 2)
        )
        grid = ctk.CTkFrame(results, fg_color="transparent")
        grid.grid(row=1, column=0, sticky="ew", padx=14)
        grid.grid_columnconfigure((1, 2), weight=1, uniform="speed")
        for column, text in ((1, "Read"), (2, "Write")):
            _label(grid, text, 11, theme.INK_SECONDARY, weight="bold", anchor="e").grid(
                row=0, column=column, sticky="e"
            )
            _label(grid, "MB/s", 10, theme.INK_MUTED, anchor="e", height=14).grid(
                row=1, column=column, sticky="e"
            )
        self.cells: dict[tuple[str, str], tuple[ctk.CTkLabel, ctk.CTkLabel]] = {}
        for i, (test, label) in enumerate(TEST_ROWS):
            _label(grid, label, 12, theme.INK_SECONDARY, weight="bold").grid(
                row=2 + 2 * i, column=0, rowspan=2, sticky="w", pady=(6, 0)
            )
            for column, direction in ((1, "read"), (2, "write")):
                value = _label(grid, "–", 20, theme.INK_MUTED, weight="bold", anchor="e")
                value.grid(row=2 + 2 * i, column=column, sticky="e", pady=(6, 0))
                sub = _label(grid, "", 10, theme.INK_MUTED, anchor="e", height=16)
                sub.grid(row=3 + 2 * i, column=column, sticky="e")
                self.cells[(test, direction)] = (value, sub)
        progress = ctk.CTkFrame(results, fg_color="transparent")
        progress.grid(row=2, column=0, sticky="ew", padx=14, pady=(10, 0))
        progress.grid_columnconfigure(0, weight=1)
        self.progress_bar = ctk.CTkProgressBar(
            progress, mode="determinate", height=8, progress_color=theme.ACCENT, fg_color=theme.BASELINE
        )
        self.progress_bar.set(0)
        self.progress_bar.grid(row=0, column=0, sticky="ew")
        self.progress_label = wraps.add(_label(results, "", 11, theme.INK_SECONDARY))
        self.progress_label.grid(row=3, column=0, sticky="w", padx=14)
        self.result_label = wraps.add(_label(results, "", 11, theme.INK_SECONDARY))
        self.result_label.grid(row=4, column=0, sticky="w", padx=14, pady=(4, 0))
        self.notes_label = wraps.add(_label(results, "", 10, theme.INK_MUTED))
        self.notes_label.grid(row=5, column=0, sticky="w", padx=14, pady=(2, 10))
        self.history = HistoryCard(body, on_pick=self.show_history_entry, on_copy=self._copy)
        self.history.grid(row=0, column=1, sticky="nsew", padx=(4, 0))

    # -- drives and choices ----------------------------------------------------------

    def show_volumes(self, volumes: Sequence[Mapping[str, Any]]) -> None:
        previous = self.selected_letter()
        self._volumes = [dict(v) for v in volumes]
        self._volume_texts = {storage_volume_text(v): v for v in self._volumes}
        if self._volumes:
            self.drive_menu.configure(values=list(self._volume_texts))
            chosen = next((v for v in self._volumes if volume_letter(v) == previous), self._volumes[0])
            self.drive_menu.set(storage_volume_text(chosen))
        else:
            self.drive_menu.configure(values=[NO_DRIVES_TEXT])
            self.drive_menu.set(NO_DRIVES_TEXT)
        self._show_leftovers()
        self._changed()

    def selected_volume(self) -> dict[str, Any] | None:
        return self._volume_texts.get(self.drive_menu.get())

    def selected_letter(self) -> str | None:
        volume = self.selected_volume()
        return volume_letter(volume) if volume is not None else None

    def select_volume(self, letter: str) -> bool:
        wanted = volume_letter({"letter": letter})
        volume = next((v for v in self._volumes if volume_letter(v) == wanted), None)
        if volume is None:
            return False
        self.drive_menu.set(storage_volume_text(volume))
        self._changed()
        return True

    def selected_size(self) -> int:
        return dict(SIZE_CHOICES).get(self.size_menu.get(), GIB)

    def selected_runs(self) -> int:
        try:
            return int(self.runs_menu.get())
        except ValueError:
            return int(DEFAULT_RUNS)

    def _changed(self, _value: str | None = None) -> None:
        volume = self.selected_volume()
        if volume is not None:
            set_text(self.warning_label, write_warning(volume, self.selected_size(), self.selected_runs()))
        _show(self.warning_label, volume is not None, row=3, column=0, sticky="w", padx=14, pady=(4, 0))
        blocked = (volume or {}).get("speed_test_blocked")
        if blocked:
            set_text(self.blocked_label, f"⚠ {blocked}")
        _show(self.blocked_label, bool(blocked), row=4, column=0, sticky="w", padx=14, pady=(2, 0))
        # Whether the chosen drive can be tested decides the Start button.
        self.apply_state(**self._state)

    def _show_leftovers(self) -> None:
        for frame, _, _ in self.leftover_rows:
            frame.destroy()
        self.leftover_rows = []
        for volume in self._volumes:
            letter = volume_letter(volume)
            for leftover in volume.get("leftovers") or []:
                frame = ctk.CTkFrame(self.leftover_box, fg_color="transparent")
                frame.grid(row=len(self.leftover_rows), column=0, sticky="ew", pady=2)
                frame.grid_columnconfigure(0, weight=1)
                button: ctk.CTkButton | None = None
                if leftover.get("in_use"):
                    text, color = f"◐ Another Cairn window is testing {letter}.", theme.INK_SECONDARY
                else:
                    size = leftover.get("bytes")
                    size_part = size_text(int(size)) if size is not None else "size unknown"
                    text = (
                        f"⚠ A test file from a speed test that didn't finish is still on {letter} "
                        f"({size_part} in {leftover.get('path')})."
                    )
                    color = theme.WARNING
                    button = _neutral_button(
                        frame,
                        "Remove",
                        lambda leftover=leftover: self._on_remove(dict(leftover)),
                        width=80,
                        height=26,
                        font=_font(11),
                    )
                    button.grid(row=0, column=1, sticky="e", padx=(8, 0))
                label = self._wraps.add(_label(frame, text, 11, color), less=100)
                label.grid(row=0, column=0, sticky="w")
                self.leftover_rows.append((frame, dict(leftover), button))
        _show(self.leftover_box, bool(self.leftover_rows), row=5, column=0, sticky="ew", padx=14, pady=(4, 0))

    # -- actions and state -------------------------------------------------------------

    def _start_or_stop(self) -> None:
        if self._running:
            self._on_stop()
            return
        letter = self.selected_letter()
        if letter:
            self._on_start(letter, self.selected_size(), self.selected_runs())

    def _copy(self) -> None:
        if self._shown is not None:
            self._on_copy(speed_result_text(self._shown))

    def apply_state(self, *, ready: bool, enabled: bool, running: str | None, stopping: bool) -> None:
        self._state = {"ready": ready, "enabled": enabled, "running": running, "stopping": stopping}
        self._running = running == KIND_SPEED
        if self._running:
            _set_action(self.start_button, "Stopping…" if stopping else "Stop", stop=True)
            _set_state(self.start_button, ready and not stopping)
        else:
            _set_action(self.start_button, "Start test", stop=False)
            volume = self.selected_volume()
            can_start = volume is not None and not volume.get("speed_test_blocked")
            _set_state(self.start_button, ready and enabled and running is None and can_start)
        choosing = ready and not self._running
        _set_state(self.drive_menu, choosing and bool(self._volumes))
        _set_state(self.size_menu, choosing)
        _set_state(self.runs_menu, choosing)
        for _, _, button in self.leftover_rows:
            if button is not None:
                _set_state(button, ready and enabled and running is None)
        self.history.set_copy_enabled(self._shown is not None)

    # -- results -------------------------------------------------------------------

    def _clear_cells(self) -> None:
        for value, sub in self.cells.values():
            set_text(value, "–")
            _set_color(value, theme.INK_MUTED)
            set_text(sub, "")

    def _fill_cells(self, measurements: Sequence[Mapping[str, Any]], skipped: Sequence[str] = ()) -> None:
        self._clear_cells()
        for m in measurements:
            cell = self.cells.get((str(m.get("test")), str(m.get("direction"))))
            if cell is None:
                continue
            value, sub = cell
            set_text(value, fmt_mb_s(m.get("mb_s")))
            _set_color(value, theme.INK)
            set_text(sub, f"{fmt_iops(m.get('iops'))} IOPS  ·  {fmt_latency(m.get('latency_us'))}")
        for test in skipped:
            for direction in ("read", "write"):
                cell = self.cells.get((str(test), direction))
                if cell is not None:
                    set_text(cell[1], "skipped")

    def cell_text(self, test: str, direction: str) -> tuple[str, str]:
        value, sub = self.cells[(test, direction)]
        return str(value.cget("text")), str(sub.cget("text"))

    def set_notes(self, notes: Sequence[str]) -> None:
        set_text(self.notes_label, "\n".join(f"• {n}" for n in notes))

    def begin(self, job: Mapping[str, Any], size_bytes: int | None = None, notes: Sequence[str] = ()) -> None:
        """Clears the results for the test `job` starts and shows its plan notes until its
        result replaces them."""
        self._job_size = size_bytes
        self._shown = None
        self._keep_line = False
        self._clear_cells()
        self.progress_bar.set(0)
        set_text(self.progress_label, speed_progress_text(job, size_bytes) or "Starting…")
        set_text(self.result_label, "")
        self.set_notes(notes)

    def show_progress(self, view: Mapping[str, Any]) -> None:
        progress = view.get("progress")
        if progress is not None:
            value = max(0.0, min(1.0, float(progress) / 100.0))
            if abs(self.progress_bar.get() - value) > 1e-4:
                self.progress_bar.set(value)
        set_text(self.progress_label, speed_progress_text(view, self._job_size))
        speed = _detail(view).get("speed") or {}
        if speed.get("done"):
            self._fill_cells(speed["done"])

    def finish(self, view: Mapping[str, Any], result: Mapping[str, Any] | None) -> None:
        set_text(self.progress_label, "")
        self.progress_bar.set(1.0 if view.get("state") == "succeeded" else self.progress_bar.get())
        if result:
            self._shown = dict(result)
            self._fill_cells(result.get("measurements") or [], result.get("skipped") or [])
            self.set_notes(result.get("notes") or [])
        text, color = speed_done_line(view, result)
        set_text(self.result_label, text)
        _set_color(self.result_label, color)
        self._keep_line = not result

    def lose(self, message: str) -> None:
        set_text(self.progress_label, "")
        set_text(self.result_label, f"⚠ {message}")
        _set_color(self.result_label, theme.CRITICAL)
        self._keep_line = True

    def show_history(self, entries: Sequence[Mapping[str, Any]], error: str | None = None) -> None:
        """Lists the earlier results; the newest is shown while no other result is and no
        test ended without one."""
        self.history.show(entries, error)
        if self._shown is None and not self._keep_line and entries and not self._running:
            self.show_history_entry(dict(entries[0]))

    def show_history_entry(self, entry: dict[str, Any]) -> None:
        """Shows an earlier result in the results card."""
        if self._running:
            return
        self._keep_line = False
        self._shown = dict(entry)
        self._fill_cells(entry.get("measurements") or [], entry.get("skipped") or [])
        self.set_notes(entry.get("notes") or [])
        set_text(self.result_label, f"Showing the result from {date_text(entry.get('started_at'))}.")
        _set_color(self.result_label, theme.INK_SECONDARY)
        self.history.set_copy_enabled(True)

    @property
    def shown_result(self) -> dict[str, Any] | None:
        return self._shown


# -- space page ----------------------------------------------------------------------


class SpacePage(ctk.CTkFrame):
    """The space analyzer: a location, the scan's progress and summary, and the folder tree or
    the largest files with the buttons that show an item in File Explorer."""

    def __init__(
        self,
        master: tk.Misc,
        *,
        on_scan: Callable[[str], None],
        on_stop: Callable[[], None],
        on_choose_folder: Callable[[], None],
        on_children: Callable[[int], dict[str, Any] | None],
        on_open: Callable[[str, bool], None],
        on_copy_path: Callable[[str], None],
        on_order: Callable[[str], None],
    ) -> None:
        super().__init__(master, fg_color="transparent")
        self._on_scan = on_scan
        self._on_stop = on_stop
        self._on_open = on_open
        self._on_copy_path = on_copy_path
        self._on_order = on_order
        self._locations: dict[str, str] = {}
        self._folders: list[str] = []
        self._running = False
        self._result: dict[str, Any] | None = None
        self._page: dict[str, Any] | None = None
        self._last_root: str | None = None
        self.order = "allocated"
        self.view = "Folders"
        self.grid_columnconfigure(0, weight=1)
        self.grid_rowconfigure(3, weight=1)

        card = _card(self)
        card.grid(row=0, column=0, sticky="ew")
        wraps = _Wraps(card)
        _label(card, "Space usage", 13, theme.INK_SECONDARY, weight="bold").grid(
            row=0, column=0, sticky="w", padx=14, pady=(10, 2)
        )
        self.intro_label = wraps.add(_label(card, SPACE_INTRO, 11, theme.INK_MUTED))
        self.intro_label.grid(row=1, column=0, sticky="w", padx=14)
        row = ctk.CTkFrame(card, fg_color="transparent")
        row.grid(row=2, column=0, sticky="w", padx=14, pady=(8, 4))
        _label(row, "Location", 11, theme.INK_SECONDARY).grid(row=0, column=0, padx=(0, 6))
        self.location_menu = _menu(row, [NO_DRIVES_TEXT], 300, lambda _v: None)
        self.location_menu.grid(row=0, column=1, padx=(0, 8))
        self.choose_button = _neutral_button(row, "Choose folder…", on_choose_folder, width=120)
        self.choose_button.grid(row=0, column=2, padx=(0, 8))
        self.scan_button = _action_button(row, "Scan", self._scan_or_stop, 90)
        self.scan_button.grid(row=0, column=3)
        self.progress_label = wraps.add(_label(card, "", 11, theme.INK_SECONDARY))
        self.progress_bar = ctk.CTkProgressBar(
            card, mode="determinate", height=8, progress_color=theme.ACCENT, fg_color=theme.BASELINE
        )
        self.progress_bar.set(0)
        self.summary_label = wraps.add(_label(card, "", 12, theme.INK))
        self.details_label = wraps.add(_label(card, "", 11, theme.INK_MUTED))
        self.notes_label = wraps.add(_label(card, "", 10, theme.INK_MUTED))
        ctk.CTkFrame(card, fg_color="transparent", height=6).grid(row=8, column=0)

        modes = ctk.CTkFrame(self, fg_color="transparent")
        modes.grid(row=1, column=0, sticky="ew", pady=(8, 4))
        modes.grid_columnconfigure(1, weight=1)
        self.view_bar = _segments(modes, VIEW_CHOICES, self._view_changed)
        self.view_bar.set("Folders")
        self.view_bar.grid(row=0, column=0, sticky="w")
        _label(modes, "Sort by", 11, theme.INK_SECONDARY).grid(row=0, column=2, padx=(0, 6))
        self.order_bar = _segments(modes, [t for t, _ in ORDER_CHOICES], self._order_changed)
        self.order_bar.set(ORDER_CHOICES[0][0])
        self.order_bar.grid(row=0, column=3, sticky="e")

        self.gone_frame = ctk.CTkFrame(self, fg_color="transparent")
        self.gone_frame.grid_columnconfigure(0, weight=1)
        self.gone_label = _label(self.gone_frame, f"⚠ {RESULTS_GONE_TEXT}", 11, theme.WARNING)
        self.gone_label.grid(row=0, column=0, sticky="w")
        self.again_button = _neutral_button(
            self.gone_frame, "Scan again", self._scan_again, width=100, height=28
        )
        self.again_button.grid(row=0, column=1, sticky="e")

        self.tree = SizeTree(self, mode="folders", on_children=on_children, on_select=self._row_selected)
        self.tree.grid(row=3, column=0, sticky="nsew")

        buttons = ctk.CTkFrame(self, fg_color="transparent")
        buttons.grid(row=4, column=0, sticky="ew", pady=(6, 0))
        buttons.grid_columnconfigure(2, weight=1)
        self.open_button = _neutral_button(
            buttons, "Open in Explorer", self._open, width=140, state="disabled"
        )
        self.open_button.grid(row=0, column=0, padx=(0, 8))
        self.copy_button = _neutral_button(buttons, "Copy path", self._copy_path, width=100, state="disabled")
        self.copy_button.grid(row=0, column=1, padx=(0, 8))
        self.hint_label = _label(buttons, TREE_HINT, 10, theme.INK_MUTED)
        self.hint_label.grid(row=0, column=2, sticky="w")

    # -- locations -----------------------------------------------------------------

    def show_volumes(self, volumes: Sequence[Mapping[str, Any]]) -> None:
        previous = self.selected_path()
        self._locations = {}
        for volume in volumes:
            if volume.get("scan_blocked"):
                continue
            self._locations[storage_volume_text(volume)] = volume_letter(volume) + "\\"
        for folder in self._folders:
            self._locations[f"Folder: {short_path(folder, 48)}"] = folder
        self._refresh_menu(previous)

    def _refresh_menu(self, wanted: str | None) -> None:
        values = list(self._locations) or [NO_DRIVES_TEXT]
        self.location_menu.configure(values=values)
        chosen = next((t for t, p in self._locations.items() if p == wanted), values[0])
        self.location_menu.set(chosen)

    def add_folder(self, path: str) -> None:
        """Adds a chosen folder to the locations and selects it."""
        if path not in self._folders:
            self._folders.insert(0, path)
            self._locations[f"Folder: {short_path(path, 48)}"] = path
        self._refresh_menu(path)

    def selected_path(self) -> str | None:
        return self._locations.get(self.location_menu.get())

    def _scan_or_stop(self) -> None:
        if self._running:
            self._on_stop()
            return
        path = self.selected_path()
        if path:
            self._on_scan(path)

    def _scan_again(self) -> None:
        path = self._last_root or self.selected_path()
        if path:
            self._on_scan(path)

    # -- state ---------------------------------------------------------------------

    def apply_state(self, *, ready: bool, enabled: bool, running: str | None, stopping: bool) -> None:
        self._running = running == KIND_SCAN
        if self._running:
            _set_action(self.scan_button, "Stopping…" if stopping else "Stop", stop=True)
            _set_state(self.scan_button, ready and not stopping)
        else:
            _set_action(self.scan_button, "Scan", stop=False)
            _set_state(self.scan_button, ready and enabled and running is None and bool(self._locations))
        _set_state(self.location_menu, ready and not self._running and bool(self._locations))
        _set_state(self.choose_button, ready and not self._running)
        _set_state(self.again_button, ready and enabled and running is None)
        self._row_selected(self.tree.selected_row())

    def _row_selected(self, row: Mapping[str, Any] | None) -> None:
        kind = (row or {}).get("kind")
        path = (row or {}).get("path")
        openable = bool(path) and kind in ("folder", "file") and not SizeTree.unreadable(row or {})
        set_text(self.open_button, "Show in Explorer" if kind == "file" else "Open in Explorer")
        _set_state(self.open_button, openable)
        _set_state(self.copy_button, bool(path))

    def _open(self) -> None:
        row = self.tree.selected_row()
        if row and row.get("path"):
            self._on_open(str(row["path"]), row.get("kind") == "file")

    def _copy_path(self) -> None:
        row = self.tree.selected_row()
        if row and row.get("path"):
            self._on_copy_path(str(row["path"]))

    def _view_changed(self, value: str) -> None:
        self.view = value
        self._show_tree()

    def _order_changed(self, value: str) -> None:
        self.order = dict(ORDER_CHOICES).get(value, "allocated")
        self._on_order(self.order)

    # -- the job -------------------------------------------------------------------

    def begin(self, job: Mapping[str, Any], notes: Sequence[str] = ()) -> None:
        target = job_target(job)
        if target:
            # A whole-drive scan is titled with the bare letter ("C:"); its folder is "C:\".
            self._last_root = target + "\\" if len(target) == 2 and target.endswith(":") else target
        set_text(self.progress_label, scan_progress_text(job))
        _show(self.progress_label, True, row=3, column=0, sticky="w", padx=14, pady=(4, 0))
        indeterminate = job.get("progress") is None
        self.progress_bar.configure(mode="indeterminate" if indeterminate else "determinate")
        if indeterminate:
            self.progress_bar.start()
        else:
            self.progress_bar.set(0)
        _show(self.progress_bar, True, row=4, column=0, sticky="ew", padx=14, pady=(4, 0))
        self.set_notes(notes)

    def set_notes(self, notes: Sequence[str]) -> None:
        set_text(self.notes_label, "\n".join(f"• {n}" for n in notes))
        _show(self.notes_label, bool(notes), row=7, column=0, sticky="w", padx=14, pady=(2, 0))

    def show_progress(self, view: Mapping[str, Any]) -> None:
        set_text(self.progress_label, scan_progress_text(view))
        progress = view.get("progress")
        if progress is not None:
            if self.progress_bar.cget("mode") != "determinate":
                self.progress_bar.stop()
                self.progress_bar.configure(mode="determinate")
            self.progress_bar.set(max(0.0, min(1.0, float(progress) / 100.0)))

    def _end_progress(self) -> None:
        if self.progress_bar.cget("mode") == "indeterminate":
            self.progress_bar.stop()
        _show(self.progress_label, False)
        _show(self.progress_bar, False)

    def finish(self, view: Mapping[str, Any], result: Mapping[str, Any] | None) -> None:
        self._end_progress()
        if result is None:
            state = view.get("state")
            if state == "failed":
                self._summary(
                    [f"⚠ Failed: {view.get('summary') or 'the scan did not finish'}"], theme.CRITICAL
                )
            elif state == "cancelled":
                self._summary(["○ Stopped: the scan's results were not kept."], theme.INK_SECONDARY)

    def lose(self, message: str) -> None:
        self._end_progress()
        self._summary([f"⚠ {message}"], theme.CRITICAL)

    def _summary(self, lines: Sequence[str], color: str = theme.INK) -> None:
        set_text(self.summary_label, lines[0] if lines else "")
        _set_color(self.summary_label, color)
        _show(self.summary_label, bool(lines), row=5, column=0, sticky="w", padx=14, pady=(4, 0))
        rest = "\n".join(lines[1:])
        set_text(self.details_label, rest)
        _show(self.details_label, bool(rest), row=6, column=0, sticky="w", padx=14)

    def show_scan(self, result: Mapping[str, Any], page: Mapping[str, Any] | None) -> None:
        self._result = dict(result)
        self._page = dict(page) if page else None
        summary = result.get("summary") or {}
        self._last_root = str(summary.get("root") or self._last_root or "")
        self._summary(scan_summary_lines(result))
        self.set_notes(result.get("warnings") or [])
        _show(self.gone_frame, False)
        self._show_tree()

    def show_page(self, page: Mapping[str, Any]) -> None:
        """Shows the scanned folder's entries again, as `page` sorts them."""
        self._page = dict(page)
        self._show_tree()

    def _show_tree(self) -> None:
        if self._result is None:
            self.tree.clear()
            return
        if self.view == "Largest files":
            key = "largest_files" if self.order == "allocated" else "largest_files_by_size"
            self.tree.show_largest(self._result.get(key) or [], self.order)
        elif self._page is not None:
            self.tree.show_folders(self._page, self.order)
        else:
            self.tree.clear()
        self._row_selected(None)

    def clear_scan(self) -> None:
        """The scan's results are gone: clears the tree and offers to scan again."""
        self._result = None
        self._page = None
        self.tree.clear()
        self._summary([])
        _show(self.gone_frame, True, row=2, column=0, sticky="ew", pady=(0, 4))
        self._row_selected(None)

    @property
    def result(self) -> dict[str, Any] | None:
        return self._result


# -- duplicates page -----------------------------------------------------------------


class DuplicatesPage(ctk.CTkFrame):
    """The duplicate finder: a minimum size, the search's progress and summary, and the groups
    of identical files."""

    def __init__(
        self,
        master: tk.Misc,
        *,
        on_find: Callable[[int], None],
        on_stop: Callable[[], None],
        on_open: Callable[[str, bool], None],
        on_copy_path: Callable[[str], None],
    ) -> None:
        super().__init__(master, fg_color="transparent")
        self._on_find = on_find
        self._on_stop = on_stop
        self._on_open = on_open
        self._on_copy_path = on_copy_path
        self._running = False
        self._scan_available = False
        self._result: dict[str, Any] | None = None
        self.grid_columnconfigure(0, weight=1)
        self.grid_rowconfigure(1, weight=1)

        card = _card(self)
        card.grid(row=0, column=0, sticky="ew", pady=(0, 8))
        wraps = _Wraps(card)
        _label(card, "Duplicate files", 13, theme.INK_SECONDARY, weight="bold").grid(
            row=0, column=0, sticky="w", padx=14, pady=(10, 2)
        )
        self.intro_label = wraps.add(_label(card, DUPES_INTRO, 11, theme.INK_MUTED))
        self.intro_label.grid(row=1, column=0, sticky="w", padx=14)
        row = ctk.CTkFrame(card, fg_color="transparent")
        row.grid(row=2, column=0, sticky="ew", padx=14, pady=(8, 4))
        row.grid_columnconfigure(3, weight=1)
        _label(row, "Minimum size", 11, theme.INK_SECONDARY).grid(row=0, column=0, padx=(0, 6))
        self.min_menu = _menu(row, [t for t, _ in MIN_SIZE_CHOICES], 90, lambda _v: None)
        self.min_menu.set(DEFAULT_MIN_SIZE)
        self.min_menu.grid(row=0, column=1, padx=(0, 8))
        self.find_button = _action_button(row, "Find duplicates", self._find_or_stop, 130)
        self.find_button.grid(row=0, column=2, padx=(0, 10))
        self.scope_label = wraps.add(_label(row, NO_SCAN_TEXT, 11, theme.INK_MUTED), less=340)
        self.scope_label.grid(row=0, column=3, sticky="w")
        self.progress_label = wraps.add(_label(card, "", 11, theme.INK_SECONDARY))
        self.progress_bar = ctk.CTkProgressBar(
            card, mode="determinate", height=8, progress_color=theme.ACCENT, fg_color=theme.BASELINE
        )
        self.progress_bar.set(0)
        self.summary_label = wraps.add(_label(card, "", 12, theme.INK))
        self.details_label = wraps.add(_label(card, "", 11, theme.INK_MUTED))
        ctk.CTkFrame(card, fg_color="transparent", height=6).grid(row=7, column=0)

        self.tree = SizeTree(self, mode="dupes", on_select=self._row_selected)
        self.tree.grid(row=1, column=0, sticky="nsew")
        buttons = ctk.CTkFrame(self, fg_color="transparent")
        buttons.grid(row=2, column=0, sticky="ew", pady=(6, 0))
        self.open_button = _neutral_button(
            buttons, "Show in Explorer", self._open, width=140, state="disabled"
        )
        self.open_button.grid(row=0, column=0, padx=(0, 8))
        self.copy_button = _neutral_button(buttons, "Copy path", self._copy_path, width=100, state="disabled")
        self.copy_button.grid(row=0, column=1)

    def selected_min_size(self) -> int:
        return dict(MIN_SIZE_CHOICES).get(self.min_menu.get(), MIB)

    def _find_or_stop(self) -> None:
        if self._running:
            self._on_stop()
        elif self._scan_available:
            self._on_find(self.selected_min_size())

    def set_scope(self, root: str | None, finished_at: str | None = None) -> None:
        """The scan the search compares, or None when there is none."""
        self._scan_available = bool(root)
        if root:
            when = time_of_day(finished_at)
            set_text(
                self.scope_label, f"In: {short_path(root, 40)}" + (f"  (scanned {when})" if when else "")
            )
        else:
            set_text(self.scope_label, NO_SCAN_TEXT)

    @property
    def scan_available(self) -> bool:
        return self._scan_available

    def apply_state(self, *, ready: bool, enabled: bool, running: str | None, stopping: bool) -> None:
        self._running = running == KIND_DUPLICATES
        if self._running:
            _set_action(self.find_button, "Stopping…" if stopping else "Stop", stop=True)
            _set_state(self.find_button, ready and not stopping)
        else:
            _set_action(self.find_button, "Find duplicates", stop=False)
            _set_state(self.find_button, ready and enabled and running is None and self._scan_available)
        _set_state(self.min_menu, ready and not self._running)
        self._row_selected(self.tree.selected_row())

    def _row_selected(self, row: Mapping[str, Any] | None) -> None:
        is_file = (row or {}).get("kind") == "file" and bool((row or {}).get("path"))
        _set_state(self.open_button, is_file)
        _set_state(self.copy_button, is_file)

    def _open(self) -> None:
        row = self.tree.selected_row()
        if row and row.get("kind") == "file" and row.get("path"):
            self._on_open(str(row["path"]), True)

    def _copy_path(self) -> None:
        row = self.tree.selected_row()
        if row and row.get("path"):
            self._on_copy_path(str(row["path"]))

    def begin(self, job: Mapping[str, Any]) -> None:
        set_text(self.progress_label, dupes_progress_text(job))
        _show(self.progress_label, True, row=3, column=0, sticky="w", padx=14, pady=(4, 0))
        self.progress_bar.set(0)
        _show(self.progress_bar, True, row=4, column=0, sticky="ew", padx=14, pady=(4, 0))

    def show_progress(self, view: Mapping[str, Any]) -> None:
        set_text(self.progress_label, dupes_progress_text(view))
        progress = view.get("progress")
        if progress is not None:
            self.progress_bar.set(max(0.0, min(1.0, float(progress) / 100.0)))

    def _end_progress(self) -> None:
        _show(self.progress_label, False)
        _show(self.progress_bar, False)

    def _summary(self, lines: Sequence[str], color: str = theme.INK) -> None:
        set_text(self.summary_label, lines[0] if lines else "")
        _set_color(self.summary_label, color)
        _show(self.summary_label, bool(lines), row=5, column=0, sticky="w", padx=14, pady=(4, 0))
        rest = "\n".join(lines[1:])
        set_text(self.details_label, rest)
        _show(self.details_label, bool(rest), row=6, column=0, sticky="w", padx=14)

    def finish(self, view: Mapping[str, Any], result: Mapping[str, Any] | None) -> None:
        self._end_progress()
        if result is None and view.get("state") == "failed":
            self._summary([f"⚠ Failed: {view.get('summary') or 'the search did not finish'}"], theme.CRITICAL)

    def lose(self, message: str) -> None:
        self._end_progress()
        self._summary([f"⚠ {message}"], theme.CRITICAL)

    def show_result(self, result: Mapping[str, Any]) -> None:
        self._result = dict(result)
        self._summary(dupes_summary_lines(result))
        self.tree.show_groups(result.get("groups") or [])
        self._row_selected(None)

    def clear(self) -> None:
        self._result = None
        self.tree.clear()
        self._summary([])
        self._row_selected(None)

    @property
    def result(self) -> dict[str, Any] | None:
        return self._result


# -- the panel -----------------------------------------------------------------------


class StoragePanel(ctk.CTkFrame):
    """The Storage section: a page switch and a "Refresh drives" button above the three pages.

    `show` fills the drives and the speed-test history; the owner feeds the running job
    through `begin_job`, `update_job` and `finish_job`. A running job of any kind disables the
    start buttons of the other pages; its own page's button becomes "Stop".
    """

    def __init__(
        self,
        master: tk.Misc,
        *,
        on_refresh: Callable[[], None],
        on_start_speed: Callable[[str, int, int], None],
        on_stop: Callable[[], None],
        on_remove_leftover: Callable[[dict[str, Any]], None],
        on_copy_speed: Callable[[str], None],
        on_scan: Callable[[str], None],
        on_choose_folder: Callable[[], None],
        on_find_duplicates: Callable[[int], None],
        on_children: Callable[[int], dict[str, Any] | None],
        on_open: Callable[[str, bool], None],
        on_copy_path: Callable[[str], None],
        on_order: Callable[[str], None],
    ) -> None:
        super().__init__(master, fg_color="transparent")
        self.loaded = False
        self._loading = False
        self._engine_ready = True
        self._unsupported = False
        self._elevated = False
        self._actions_enabled = True
        self._job: dict[str, Any] | None = None
        self._stopping = False
        self._page = PAGES[0]
        self.grid_columnconfigure(0, weight=1)
        self.grid_rowconfigure(2, weight=1)

        header = ctk.CTkFrame(self, fg_color="transparent")
        header.grid(row=0, column=0, sticky="ew", pady=(0, 6))
        header.grid_columnconfigure(1, weight=1)
        self.page_bar = _segments(header, PAGES, self.select_page, size=12)
        self.page_bar.set(PAGES[0])
        self.page_bar.grid(row=0, column=0, sticky="w")
        self.refresh_button = _neutral_button(header, "Refresh drives", on_refresh, width=120)
        self.refresh_button.grid(row=0, column=2, sticky="e")
        self.message_label = _label(self, "", 11, theme.INK_MUTED)
        self._message_wraps = _Wraps(self, pad=0)
        self._message_wraps.add(self.message_label)

        host = ctk.CTkFrame(self, fg_color="transparent")
        host.grid(row=2, column=0, sticky="nsew")
        host.grid_columnconfigure(0, weight=1)
        host.grid_rowconfigure(0, weight=1)
        self._host = host
        self._speed = SpeedPage(
            host,
            on_start=on_start_speed,
            on_stop=on_stop,
            on_remove_leftover=on_remove_leftover,
            on_copy=on_copy_speed,
        )
        self._space = SpacePage(
            host,
            on_scan=on_scan,
            on_stop=on_stop,
            on_choose_folder=on_choose_folder,
            on_children=on_children,
            on_open=on_open,
            on_copy_path=on_copy_path,
            on_order=on_order,
        )
        self._duplicates = DuplicatesPage(
            host, on_find=on_find_duplicates, on_stop=on_stop, on_open=on_open, on_copy_path=on_copy_path
        )
        self._pages: dict[str, Any] = {
            "Speed test": self._speed,
            "Space": self._space,
            "Duplicates": self._duplicates,
        }
        self._speed.grid(row=0, column=0, sticky="nsew")
        self._apply_state()

    # -- pages ---------------------------------------------------------------------

    @property
    def page(self) -> str:
        return self._page

    @property
    def speed(self) -> SpeedPage:
        return self._speed

    @property
    def space(self) -> SpacePage:
        return self._space

    @property
    def duplicates(self) -> DuplicatesPage:
        return self._duplicates

    def select_page(self, name: str) -> None:
        """Shows page `name`; only regrids, never calls the engine."""
        if name not in self._pages or name == self._page:
            if self.page_bar.get() != self._page:
                self.page_bar.set(self._page)
            return
        self._pages[name].grid(row=0, column=0, sticky="nsew")
        self._pages[self._page].grid_forget()
        self._page = name
        if self.page_bar.get() != name:
            self.page_bar.set(name)

    # -- loading -------------------------------------------------------------------

    def _message(self, text: str, color: str = theme.INK_MUTED) -> None:
        set_text(self.message_label, text)
        _set_color(self.message_label, color)
        _show(self.message_label, bool(text), row=1, column=0, sticky="w", padx=4, pady=(0, 6))

    def set_loading(self) -> None:
        self._loading = True
        self._message(LOADING_TEXT)
        self._apply_state()

    def show(
        self,
        volumes: Sequence[Mapping[str, Any]],
        history: Sequence[Mapping[str, Any]],
        *,
        engine_ready: bool,
        elevated: bool,
        history_error: str | None = None,
    ) -> None:
        self.loaded = True
        self._loading = False
        self._engine_ready = engine_ready
        self._elevated = elevated
        self._message("")
        self._speed.show_volumes(volumes)
        self._space.show_volumes(volumes)
        self._speed.show_history(history, history_error)
        self._apply_state()

    def show_error(self, message: str) -> None:
        self._loading = False
        self._message(f"⚠ Could not read the drives: {message}", theme.CRITICAL)
        self._apply_state()

    def set_unsupported(self, text: str) -> None:
        self._unsupported = True
        self._message(f"⚠ {text}", theme.WARNING)
        self._apply_state()

    def set_engine_ready(self, ready: bool) -> None:
        self._engine_ready = ready
        if not ready:
            self._message(ENGINE_MISSING_TEXT, theme.WARNING)
        self._apply_state()

    def set_actions_enabled(self, enabled: bool) -> None:
        if enabled != self._actions_enabled:
            self._actions_enabled = enabled
            self._apply_state()

    def _apply_state(self) -> None:
        ready = self._engine_ready and not self._unsupported
        running = str(self._job.get("kind")) if self._job else None
        options = {"ready": ready, "enabled": self._actions_enabled and not self._loading, "running": running}
        for page in (self._speed, self._space, self._duplicates):
            page.apply_state(**options, stopping=self._stopping)
        _set_state(self.refresh_button, ready and not self._loading)

    # -- the job -------------------------------------------------------------------

    @property
    def job(self) -> dict[str, Any] | None:
        return self._job

    def begin_job(
        self, snapshot: Mapping[str, Any], *, notes: Sequence[str] = (), size_bytes: int | None = None
    ) -> None:
        self._job = dict(snapshot)
        self._stopping = False
        kind = snapshot.get("kind")
        if kind == KIND_SPEED:
            self._speed.begin(snapshot, size_bytes, notes)
        elif kind == KIND_SCAN:
            self._space.begin(snapshot, notes)
        elif kind == KIND_DUPLICATES:
            self._duplicates.begin(snapshot)
        self._apply_state()

    def update_job(self, snapshot: Mapping[str, Any], *, visible: bool) -> None:
        self._job = dict(snapshot)
        stopping = bool(snapshot.get("cancel_requested"))
        kind = snapshot.get("kind")
        if visible:
            if kind == KIND_SPEED:
                self._speed.show_progress(snapshot)
            elif kind == KIND_SCAN:
                self._space.show_progress(snapshot)
            elif kind == KIND_DUPLICATES:
                self._duplicates.show_progress(snapshot)
        if stopping != self._stopping:
            self._stopping = stopping
            self._apply_state()

    def set_stopping(self) -> None:
        self._stopping = True
        self._apply_state()

    def finish_job(self, snapshot: Mapping[str, Any], result: Mapping[str, Any] | None) -> None:
        kind = snapshot.get("kind")
        self._job = None
        self._stopping = False
        if kind == KIND_SPEED:
            self._speed.finish(snapshot, result)
        elif kind == KIND_SCAN:
            self._space.finish(snapshot, result)
        elif kind == KIND_DUPLICATES:
            self._duplicates.finish(snapshot, result)
        self._apply_state()

    def lose_job(self, message: str) -> None:
        kind = (self._job or {}).get("kind")
        self._job = None
        self._stopping = False
        page = {KIND_SPEED: self._speed, KIND_SCAN: self._space, KIND_DUPLICATES: self._duplicates}.get(
            str(kind)
        )
        if page is not None:
            page.lose(message)
        self._apply_state()

    # -- results -------------------------------------------------------------------

    def show_scan(self, result: Mapping[str, Any], root_page: Mapping[str, Any] | None) -> None:
        """Shows a newly finished scan. It replaces the scan the shown duplicate groups were
        found in, so those groups are cleared."""
        self._duplicates.clear()
        self._space.show_scan(result, root_page)

    def clear_scan(self) -> None:
        """The scan's results are gone: clears the tree and the duplicate groups found in it."""
        self._space.clear_scan()
        self._duplicates.clear()
        self._duplicates.set_scope(None)
        self._apply_state()

    def show_duplicates(self, result: Mapping[str, Any]) -> None:
        self._duplicates.show_result(result)

    def stop_animations(self) -> None:
        """Stops the moving progress bar of a folder scan, before the window closes."""
        bar = self._space.progress_bar
        if bar.cget("mode") == "indeterminate":
            bar.stop()

    def add_scan_folder(self, path: str) -> None:
        """Adds a chosen folder to the Space page's locations and selects it."""
        self._space.add_folder(path)
        self._apply_state()

    def set_scan_available(self, root: str | None, finished_at: str | None = None) -> None:
        self._duplicates.set_scope(root, finished_at)
        self._apply_state()
