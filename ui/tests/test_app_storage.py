"""Storage section integration tests: the real window with the in-memory FakeEngine.

No drive is tested and no folder is read: jobs exist only inside the fake and are driven with
`storage_progress` and `storage_finish`. File Explorer and the folder picker are replaced by
recorders; administrator dialogs are cancelled, never confirmed.
"""

from __future__ import annotations

import copy
import tkinter as tk
import tkinter.font as tkfont
from tkinter import ttk
from typing import Any

import pytest

from optimizer import system
from optimizer.features.storage import (
    BLOCKED_TITLE,
    CLOSE_SCAN_MESSAGE,
    CLOSE_SPEED_MESSAGE,
    RESULTS_GONE_TEXT,
    SPEED_STOP_TITLE,
    UNSUPPORTED_TEXT,
)
from optimizer.widgets.storage import NO_SCAN_TEXT, PAGES, TREE_STYLE, StoragePanel

from .app_support import (
    ADMIN_DIALOG_TITLE,
    FIT_SIZES,
    App,
    AppFactory,
    FakeEngine,
    assert_section_fits,
    confirm_dialog,
    confirm_dialog_text,
    ctk,
    dialog_text,
    dialogs,
    idle,
    next_dialog,
    pump,
    resize_to,
    scanned,
    show_section,
    theme,
)
from .fake_storage import (
    GIB,
    LARGEST_FILES,
    MIB,
    SCAN_SUMMARY,
    SPEED_RESULT,
    TREE_NODES,
    VOLUMES,
    measurement,
    tree_row,
)

LEFTOVER = {"path": "C:\\CairnSpeedTest-0123abcd", "bytes": GIB, "in_use": False, "other_entries": 0}
BATTERY_NOTE = (
    "The PC is on battery power; Windows may slow the drive to save energy, so results can be lower."
)
SYSTEM_NOTE = "Other programs use C: while the test runs, which can lower the results."
INTEGRITY_NOTE = "Integrity streams are on here; writes include checksums."
FOLDER_ERROR = "the test folder can't be created: Access is denied."


def open_storage(app: App) -> StoragePanel:
    """Waits for the first scan, shows the Storage section and waits until it is loaded."""
    pump(app, 5.0, until=lambda: scanned(app))
    show_section(app, "Storage")
    pump(app, 5.0, until=lambda: app.storage_panel.loaded and idle(app))
    return app.storage_panel


def status(app: App) -> tuple[str, str]:
    return str(app.status_message.cget("text")), str(app.status_message.cget("text_color"))


def inside_section(app: App, widget: Any) -> bool:
    """`widget` is drawn and lies inside the Storage section's frame (1 px tolerance)."""
    frame = app.section_frame("Storage")
    if not widget.winfo_ismapped():
        return False
    x0 = widget.winfo_rootx() - frame.winfo_rootx()
    y0 = widget.winfo_rooty() - frame.winfo_rooty()
    x1, y1 = x0 + widget.winfo_width(), y0 + widget.winfo_height()
    return x0 >= -1 and y0 >= -1 and x1 <= frame.winfo_width() + 1 and y1 <= frame.winfo_height() + 1


def scroll_canvas(widget: Any) -> tk.Canvas:
    """The canvas of the scrollable frame `widget` sits in."""
    while not isinstance(widget, tk.Canvas):
        widget = widget.master
        assert widget is not None, "the widget is not inside a scrollable frame"
    return widget


def start_speed_test(app: App) -> int:
    """Presses Start test, confirms, and returns the job id."""
    app.storage_panel.speed.start_button.invoke()
    confirm_dialog(app)
    pump(app, 5.0, until=lambda: app._storage_job is not None and idle(app))
    assert app._storage_job is not None
    return app._storage_job


def scan(app: App, engine: FakeEngine, path: str | None = None) -> int:
    """Scans the selected location (or `path`, chosen with the folder picker) and finishes it."""
    panel = app.storage_panel
    panel.select_page("Space")
    if path is not None:
        panel.space.add_folder(path)
    panel.space.scan_button.invoke()
    pump(app, 5.0, until=lambda: app._storage_job is not None and idle(app))
    job = app._storage_job
    assert job is not None
    engine.storage_finish(job)
    pump(app, 5.0, until=lambda: app._storage_job is None and app._storage_scan_job == job)
    return job


def folder_scan_result(job: int, root: str) -> dict[str, Any]:
    """The fixture tree as the result of a scan of the folder `root`."""
    summary = {
        **copy.deepcopy(SCAN_SUMMARY),
        "root": root,
        "volume": root[:2],
        "whole_volume": False,
        "not_reached_bytes": None,
    }
    return {
        "kind": "scan",
        "job_id": job,
        "summary": summary,
        "root": {**copy.deepcopy(TREE_NODES[0]), "name": root, "path": root},
        "largest_files": copy.deepcopy(LARGEST_FILES),
        "largest_files_by_size": copy.deepcopy(LARGEST_FILES),
        "warnings": [],
    }


def find_duplicates(app: App, engine: FakeEngine) -> int:
    """Searches the files of the shown scan and finishes the search with the fixture groups."""
    panel = app.storage_panel
    panel.select_page("Duplicates")
    panel.duplicates.find_button.invoke()
    pump(app, 5.0, until=lambda: app._storage_job is not None and idle(app))
    job = app._storage_job
    assert job is not None
    engine.storage_finish(job)
    pump(app, 5.0, until=lambda: app._storage_job is None and bool(panel.duplicates.tree.rows))
    return job


def test_nothing_loads_before_the_section_is_shown_and_it_loads_once(make_app: AppFactory) -> None:
    app, engine = make_app(elevated=True)
    pump(app, 5.0, until=lambda: scanned(app))
    assert not [name for name, _ in engine.calls if name.startswith("storage_")]
    panel = open_storage(app)
    assert [name for name, _ in engine.calls if name.startswith("storage_")] == [
        "storage_volumes",
        "storage_speed_history",
    ]
    assert panel.page == "Speed test"
    assert panel.speed.drive_menu.cget("values") == [
        "C:  Windows  ·  NVMe SSD  ·  611 GB free of 952 GB",
        "D:  Data  ·  SATA HDD  ·  1210 GB free of 1863 GB",
    ]
    assert panel.speed.selected_letter() == "C:"
    assert panel.speed.warning_label.cget("text").startswith("⚠ Writes up to 13 GB to C: the 1 GB test file")
    assert panel.speed.warning_label.cget("text_color") == theme.WARNING
    assert len(panel.speed.history.row_widgets) == 1
    assert panel.speed.cell_text("seq1m_q8t1", "read")[0] == "7,012", "the newest result is shown"
    assert panel.speed.result_label.cget("text").startswith("Showing the result from ")
    assert panel.speed.start_button.cget("state") == "normal"
    assert panel.space.location_menu.cget("values")[0].startswith("C:  Windows")

    # Page switches only regrid; revisiting keeps what was read; Refresh reads again.
    for page in PAGES:
        panel.select_page(page)
        assert panel.page == page
    show_section(app, "History")
    show_section(app, "Storage")
    pump(app, 0.2)
    assert len(engine.calls_named("storage_volumes")) == 1
    panel.refresh_button.invoke()
    pump(app, 5.0, until=lambda: len(engine.calls_named("storage_speed_history")) == 2 and idle(app))
    assert app.errors == []


def test_an_outdated_engine_shows_the_unsupported_note(make_app: AppFactory) -> None:
    app, engine = make_app(elevated=True, unsupported=["storage_volumes"])
    pump(app, 5.0, until=lambda: scanned(app))
    show_section(app, "Storage")
    pump(app, 0.3)
    panel = app.storage_panel
    assert panel.message_label.cget("text") == f"⚠ {UNSUPPORTED_TEXT}"
    assert panel.speed.start_button.cget("state") == "disabled"
    assert panel.space.scan_button.cget("state") == "disabled"
    assert not [name for name, _ in engine.calls if name.startswith("storage_")]
    assert app.errors == []


def test_a_standard_user_gets_the_admin_dialog_and_no_plan(make_app: AppFactory) -> None:
    app, engine = make_app(elevated=False)
    panel = open_storage(app)
    assert panel.speed.start_button.cget("state") == "normal"
    panel.speed.start_button.invoke()
    dialog = next_dialog(app)
    assert dialog.title_text == ADMIN_DIALOG_TITLE
    dialog._cancel()
    pump(app, 0.2)
    assert engine.calls_named("storage_speed_start") == []
    assert app._storage_job is None
    assert app.errors == []


def test_a_speed_test_runs_after_confirmation_and_shows_its_results(make_app: AppFactory) -> None:
    app, engine = make_app(elevated=True)
    panel = open_storage(app)
    panel.speed.start_button.invoke()
    dialog = next_dialog(app)
    text = dialog_text(dialog)
    assert dialog.title_text == "Test the speed of C:?"
    assert "writes up to 13 GB to the drive and uses a little of the SSD's write endurance" in text
    assert "It takes about 2 minutes. You can stop it at any time." in text
    assert "• Other programs use C: while the test runs, which can lower the results." in text
    assert "• The run is recorded in History › Activity; the test leaves nothing behind." in text
    assert engine.calls_named("storage_speed_start") == [("C:", GIB, 3, True)]
    dialog.confirm_button.invoke()
    pump(app, 5.0, until=lambda: app._storage_job is not None and idle(app))
    job = app._storage_job
    assert job is not None
    assert engine.calls_named("storage_speed_start")[-1] == ("C:", GIB, 3, False)
    assert app._running_storage_title() == "Speed test of C:"
    assert panel.speed.start_button.cget("text") == "Stop"
    assert panel.speed.start_button.cget("fg_color") == theme.CRITICAL
    assert panel.space.scan_button.cget("state") == "disabled", "one storage job at a time"
    assert panel.speed.cell_text("seq1m_q8t1", "read")[0] == "–", "the grid is cleared for the new test"

    engine.storage_progress(
        job,
        progress=40.0,
        phase="measuring",
        speed={
            "test": "seq1m_q8t1",
            "direction": "read",
            "run": 1,
            "runs": 3,
            "live_mb_s": 6980.4,
            "done": [measurement("seq1m_q8t1", "read")],
        },
    )
    pump(app, 5.0, until=lambda: panel.speed.cell_text("seq1m_q8t1", "read")[0] == "7,012")
    assert panel.speed.cell_text("seq1m_q8t1", "read")[1] == "6,687 IOPS  ·  1.2 ms"
    assert panel.speed.progress_label.cget("text") == "SEQ1M Q8T1  ·  read  ·  run 1 of 3  ·  6,980 MB/s"
    assert abs(panel.speed.progress_bar.get() - 0.4) < 1e-3
    assert app.status_tool.cget("text") == "◐ Speed test C:  ·  40 %"

    engine.storage_finish(job)
    pump(app, 5.0, until=lambda: app._storage_job is None)
    pump(app, 5.0, until=lambda: len(engine.calls_named("storage_speed_history")) == 2 and idle(app))
    assert status(app) == ("Done: speed test of C: finished in 2 min 41 s.", theme.GOOD)
    assert app.status_tool.cget("text") == ""
    assert panel.speed.result_label.cget("text").startswith("✓ Finished in 2 min 41 s  ·  1 GB test file")
    assert panel.speed.cell_text("rnd4k_q1t1", "write")[0] == "310"
    assert panel.speed.start_button.cget("text") == "Start test"
    assert len(engine.calls_named("storage_volumes")) == 2, "the drives are read again"
    assert not dialogs(app)
    assert app._storage_allow_close(), "nothing runs, so closing needs no question"
    assert app.errors == []


def test_a_running_speed_test_shows_its_own_notes(make_app: AppFactory) -> None:
    app, engine = make_app(elevated=True, storage_history=[{**SPEED_RESULT, "notes": [BATTERY_NOTE]}])
    panel = open_storage(app)
    notes = panel.speed.notes_label
    assert notes.cget("text") == f"• {BATTERY_NOTE}", "the earlier result is shown with its notes"

    job = start_speed_test(app)
    assert notes.cget("text") == f"• {SYSTEM_NOTE}", "the new test's plan notes replace them"
    # Reading the drives and the earlier results again while the test runs keeps them.
    panel.refresh_button.invoke()
    pump(app, 5.0, until=lambda: len(engine.calls_named("storage_speed_history")) == 2 and idle(app))
    assert notes.cget("text") == f"• {SYSTEM_NOTE}"

    result = {"kind": "speed", **SPEED_RESULT, "notes": [SYSTEM_NOTE, INTEGRITY_NOTE]}
    engine.storage_finish(job, result=result)
    pump(app, 5.0, until=lambda: app._storage_job is None)
    pump(app, 5.0, until=lambda: len(engine.calls_named("storage_speed_history")) == 3 and idle(app))
    assert notes.cget("text") == f"• {SYSTEM_NOTE}\n• {INTEGRITY_NOTE}", "the result's notes follow"
    assert app.errors == []


def test_a_drive_that_cannot_be_tested_disables_start_until_another_is_chosen(
    make_app: AppFactory,
) -> None:
    volumes = copy.deepcopy(list(VOLUMES))
    volumes[1].update(read_only=True, speed_test_blocked="D: is read-only")
    app, engine = make_app(elevated=True, storage_volumes=volumes)
    speed = open_storage(app).speed
    assert speed.selected_letter() == "C:"
    assert speed.start_button.cget("state") == "normal"
    assert not speed.blocked_label.winfo_manager()

    assert speed.select_volume("D:")
    assert speed.start_button.cget("state") == "disabled"
    assert speed.blocked_label.winfo_manager() == "grid"
    assert speed.blocked_label.cget("text") == "⚠ D: is read-only"
    # The size and runs menus go through the same handler and keep the refusal.
    speed.size_menu.set("64 MB")
    speed._changed("64 MB")
    assert speed.start_button.cget("state") == "disabled"

    assert speed.select_volume("C:")
    assert speed.start_button.cget("state") == "normal", "the usable drive can be tested at once"
    assert not speed.blocked_label.winfo_manager()
    assert engine.calls_named("storage_speed_start") == []
    assert app.errors == []


def test_a_blocked_plan_explains_and_starts_nothing(make_app: AppFactory) -> None:
    reason = "Optimize Drives is running on C:; wait for it to finish."
    app, engine = make_app(elevated=True, storage_blocked={"speed": reason})
    panel = open_storage(app)
    panel.speed.start_button.invoke()
    dialog = next_dialog(app)
    assert dialog.title_text == BLOCKED_TITLE
    assert reason in dialog_text(dialog)
    dialog.confirm_button.invoke()
    pump(app, 0.2)
    assert engine.calls_named("storage_speed_start") == [("C:", GIB, 3, True)]
    assert status(app)[1] == theme.WARNING
    assert app.errors == []


def test_stopping_a_speed_test_asks_first(make_app: AppFactory) -> None:
    app, engine = make_app(elevated=True)
    panel = open_storage(app)
    job = start_speed_test(app)
    panel.speed.start_button.invoke()
    dialog = next_dialog(app)
    assert dialog.title_text == SPEED_STOP_TITLE
    assert "The test file is deleted. Results measured so far are kept." in dialog_text(dialog)
    dialog._cancel()
    pump(app, 0.2)
    assert engine.calls_named("storage_cancel") == []
    panel.speed.start_button.invoke()
    confirm_dialog(app)
    assert engine.calls_named("storage_cancel") == [(job,)]
    pump(app, 5.0, until=lambda: app._storage_job is None and idle(app))
    assert status(app) == ("The speed test of C: stopped; the test file was deleted.", theme.INK_SECONDARY)
    assert panel.speed.result_label.cget("text").startswith("○ Stopped after ")
    assert app.errors == []


def test_a_test_that_failed_without_a_result_keeps_its_line(make_app: AppFactory) -> None:
    earlier = {
        **copy.deepcopy(SPEED_RESULT),
        "volume": "D:",
        "label": "Data",
        "media": "hdd",
        "started_at": "2026-09-20T08:00:00Z",
        "finished_at": "2026-09-20T08:03:00Z",
    }
    for m in earlier["measurements"]:
        m["mb_s"] = 150.0
    app, engine = make_app(elevated=True, storage_history=[earlier])
    panel = open_storage(app)
    speed = panel.speed
    assert speed.cell_text("seq1m_q8t1", "read")[0] == "150", "the newest earlier result is shown"

    job = start_speed_test(app)
    engine.storage_finish(job, state="failed", error=FOLDER_ERROR)
    pump(app, 5.0, until=lambda: app._storage_job is None)
    dialog = next_dialog(app)
    assert dialog.title_text == "Speed test of C: failed"
    dialog._cancel()
    # The drives and the earlier results are read again after the test, and on Refresh.
    pump(app, 5.0, until=lambda: len(engine.calls_named("storage_speed_history")) == 2 and idle(app))
    panel.refresh_button.invoke()
    pump(app, 5.0, until=lambda: len(engine.calls_named("storage_speed_history")) == 3 and idle(app))
    assert speed.result_label.cget("text") == f"⚠ Failed: {FOLDER_ERROR}"
    assert speed.result_label.cget("text_color") == theme.CRITICAL
    assert speed.cell_text("seq1m_q8t1", "read")[0] == "–", "D:'s earlier result does not fill the grid"
    assert speed.history.copy_button.cget("state") == "disabled"

    # Picking the earlier result shows it.
    _, top, _ = speed.history.row_widgets[0]
    top.event_generate("<Button-1>")
    pump(app, 0.2)
    assert speed.result_label.cget("text").startswith("Showing the result from ")
    assert speed.cell_text("seq1m_q8t1", "read")[0] == "150"
    assert speed.history.copy_button.cget("state") == "normal"
    assert app.errors == []


def test_a_lost_speed_test_keeps_its_line(make_app: AppFactory) -> None:
    app, engine = make_app(elevated=True)
    panel = open_storage(app)
    start_speed_test(app)

    def boom(_now: float) -> None:
        raise RuntimeError("poll failed")

    app._poll_storage = boom  # type: ignore[method-assign]
    lost = "⚠ Lost track of Speed test of C:."
    pump(app, 5.0, until=lambda: panel.speed.result_label.cget("text") == lost)
    panel.refresh_button.invoke()
    pump(app, 5.0, until=lambda: len(engine.calls_named("storage_speed_history")) == 2 and idle(app))
    assert panel.speed.result_label.cget("text") == lost, "the earlier result does not replace it"
    assert panel.speed.cell_text("seq1m_q8t1", "read")[0] == "–"
    assert len(app.errors) == 1 and "poll failed" in app.errors[0]
    app.errors.clear()


def test_scan_tree_sorting_explorer_and_copy(make_app: AppFactory, monkeypatch: pytest.MonkeyPatch) -> None:
    app, engine = make_app(elevated=False)
    panel = open_storage(app)
    chosen = "C:\\Users\\Test\\Videos"
    monkeypatch.setattr(system, "ask_folder", lambda _parent, *, title: chosen)
    panel.select_page("Space")
    panel.space.choose_button.invoke()
    assert panel.space.selected_path() == chosen
    assert panel.space.location_menu.get() == "Folder: C:\\Users\\Test\\Videos"
    panel.space.scan_button.invoke()
    assert not dialogs(app), "a scan only reads, so it starts without a dialog"
    pump(app, 5.0, until=lambda: app._storage_job is not None and idle(app))
    job = app._storage_job
    assert job is not None
    assert engine.calls_named("storage_scan_start") == [(chosen, False)]
    assert panel.space.scan_button.cget("text") == "Stop"
    engine.storage_progress(job, scan={"files": 182_340, "folders": 20_114, "allocated_bytes": 2 * GIB})
    pump(
        app,
        5.0,
        until=lambda: app.status_tool.cget("text") == "◐ Scanning C:\\Users\\Test\\Videos  ·  182,340 files",
    )
    assert panel.space.progress_label.cget("text").startswith(
        "◐ Scanning C:\\Users\\Test\\Videos…  182,340 files"
    )

    engine.storage_finish(job)
    pump(app, 5.0, until=lambda: app._storage_scan_job == job)
    assert status(app) == ("Scan of C: finished: 338 GB in 1,204,332 files.", theme.GOOD)
    assert panel.space.summary_label.cget("text").startswith("✓ C:  ·  338 GB on disk")
    tree = panel.space.tree
    assert tree.tree.get_children("") == ("n0",)
    assert tree.tree.item("n0", "open")
    assert tree.tree.get_children("n0") == ("n1", "n2", "n3", "s0", "n4", "n5")
    assert tree.tree.item("n4", "text") == "Documents and Settings  (link, not followed)"
    assert tree.tree.item("n5", "text") == "System Volume Information  (can't read)"
    assert tree.tree.set("n1", "share").endswith("52 %")
    assert engine.calls_named("storage_scan_children") == [(job, 0, "allocated", 500)]

    tree.open_folder("n1")
    assert engine.calls_named("storage_scan_children")[-1] == (job, 1, "allocated", 500)
    assert tree.tree.get_children("n1") == ("n6",)
    tree.open_folder("n6")
    tree.open_folder("n7")
    assert tree.tree.get_children("n7") == ("f7:0", "s7")

    panel.space._order_changed("Size")
    assert engine.calls_named("storage_scan_children")[-1] == (job, 0, "logical", 500)
    assert tree.tree.get_children("n0")[0] == "n1"

    opened: list[tuple[str, bool]] = []
    monkeypatch.setattr(system, "show_in_explorer", lambda path, *, select: opened.append((path, select)))
    copied: list[str] = []
    app._set_clipboard = copied.append  # type: ignore[method-assign]
    tree.select("n1")
    assert panel.space.open_button.cget("text") == "Open in Explorer"
    panel.space.open_button.invoke()
    panel.space.copy_button.invoke()
    assert status(app) == ("Path copied.", theme.GOOD)
    tree.open_folder("n1")
    tree.open_folder("n6")
    tree.open_folder("n7")
    tree.select("f7:0")
    assert panel.space.open_button.cget("text") == "Show in Explorer"
    panel.space.open_button.invoke()
    assert opened == [("C:\\Users", False), ("C:\\Users\\Test\\Videos\\holiday.mp4", True)]
    assert copied == ["C:\\Users"]
    tree.select("n4")
    assert panel.space.open_button.cget("state") == "disabled", "links are not opened"

    panel.space._view_changed("Largest files")
    assert tree.mode == "largest"
    assert tree.tree.item("l0", "text") == "holiday.mp4"
    assert app.errors == []


def test_folders_that_could_not_be_read_are_not_opened(make_app: AppFactory) -> None:
    app, engine = make_app(elevated=True)
    original = engine.storage_scan_children
    gone = tree_row("folder", "Gone", node=99, path="C:\\Gone", error="it was removed while Cairn scanned")

    def children(job_id: int, node: int, order: str = "allocated", limit: int = 500) -> Any:
        page = original(job_id, node, order, limit)
        if page is not None and node == 0:
            page["children"].append(dict(gone))
        return page

    engine.storage_scan_children = children  # type: ignore[method-assign]
    panel = open_storage(app)
    scan(app, engine)
    tree = panel.space.tree
    assert tree.tree.item("n99", "text") == "Gone  (can't read)"
    assert tree.tree.item("n5", "text") == "System Volume Information  (can't read)"
    # A folder the scan could not list, by an error or by denied access: its path can be
    # copied, but File Explorer is not opened on it.
    for iid in ("n99", "n5"):
        tree.select(iid)
        assert panel.space.open_button.cget("state") == "disabled", iid
        assert panel.space.copy_button.cget("state") == "normal", iid
    tree.select("n1")
    assert panel.space.open_button.cget("state") == "normal"
    assert app.errors == []


def test_a_full_share_bar_and_its_percentage_fit_the_column(make_app: AppFactory) -> None:
    app, engine = make_app(elevated=True)
    open_storage(app)
    scan(app, engine)
    tree = app.storage_panel.space.tree.tree
    text = tree.set("n0", "share")
    assert text.endswith("100 %"), "the scanned folder is the whole"
    font = tkfont.Font(root=app, font=ttk.Style(app).lookup(TREE_STYLE, "font"))
    # The cell keeps a few pixels of padding on each side.
    assert font.measure(text) + 8 <= int(tree.column("share", "width"))
    assert app.errors == []


def test_cleared_scan_results_offer_to_scan_again(make_app: AppFactory) -> None:
    app, engine = make_app(elevated=True)
    panel = open_storage(app)
    job = scan(app, engine)
    engine.storage_forget(job)
    panel.space.tree.open_folder("n1")
    pump(app, 0.2)
    assert status(app) == (RESULTS_GONE_TEXT, theme.WARNING)
    assert panel.space.gone_frame.winfo_manager() == "grid"
    assert panel.space.tree.tree.get_children("") == ()
    assert app._storage_scan_job is None
    assert panel.duplicates.find_button.cget("state") == "disabled"
    panel.space.again_button.invoke()
    pump(app, 5.0, until=lambda: app._storage_job is not None and idle(app))
    assert engine.calls_named("storage_scan_start")[-1] == ("C:\\", False)
    engine.storage_finish(app._storage_job)
    pump(app, 5.0, until=lambda: app._storage_job is None and app._storage_scan_job is not None)
    assert not panel.space.gone_frame.winfo_manager()
    assert app.errors == []


def test_duplicates_need_a_scan_then_show_groups(
    make_app: AppFactory, monkeypatch: pytest.MonkeyPatch
) -> None:
    app, engine = make_app(elevated=False)
    panel = open_storage(app)
    panel.select_page("Duplicates")
    assert panel.duplicates.find_button.cget("state") == "disabled"
    assert panel.duplicates.scope_label.cget("text") == NO_SCAN_TEXT
    scan_id = scan(app, engine)
    assert panel.duplicates.scope_label.cget("text").startswith("In: C:\\  (scanned ")
    panel.select_page("Duplicates")
    panel.duplicates.min_menu.set("10 MB")
    panel.duplicates.find_button.invoke()
    pump(app, 5.0, until=lambda: app._storage_job is not None and idle(app))
    job = app._storage_job
    assert job is not None
    assert engine.calls_named("storage_duplicates_start") == [(scan_id, 10 * MIB, False)]
    assert status(app) == ("Finding duplicate files…", theme.INK_SECONDARY)
    engine.storage_progress(
        job,
        progress=41.0,
        phase="hashing",
        duplicates={"files_total": 7, "bytes_total": 18 * GIB, "bytes_done": 7 * GIB},
    )
    pump(app, 5.0, until=lambda: app.status_tool.cget("text") == "◐ Finding duplicates  ·  41 %")
    assert "7 files in full  ·  7 of 18 GB read" in panel.duplicates.progress_label.cget("text")
    engine.storage_finish(job)
    pump(app, 5.0, until=lambda: app._storage_job is None)
    assert status(app) == ("Found 3 groups of duplicate files.", theme.GOOD)
    assert panel.duplicates.summary_label.cget("text") == (
        "✓ 3 groups of identical files  ·  2.3 GB would be freed by keeping one copy of each"
    )
    tree = panel.duplicates.tree
    assert tree.tree.get_children("") == ("g0", "g1", "g2")
    assert tree.tree.item("g0", "text") == "3 copies  ·  1 GB each  ·  2 GB extra"
    assert tree.tree.item("g0", "open")
    opened: list[tuple[str, bool]] = []
    monkeypatch.setattr(system, "show_in_explorer", lambda path, *, select: opened.append((path, select)))
    tree.select("g0")
    assert panel.duplicates.open_button.cget("state") == "disabled"
    tree.select("g0:1")
    panel.duplicates.open_button.invoke()
    assert opened == [("C:\\Users\\Test\\Desktop\\holiday (1).mp4", True)]
    assert app.errors == []


def test_a_new_scan_clears_the_duplicate_groups_of_the_last(make_app: AppFactory) -> None:
    app, engine = make_app(elevated=False)
    panel = open_storage(app)
    scan(app, engine)
    find_duplicates(app, engine)
    dupes = panel.duplicates
    assert dupes.summary_label.winfo_manager() == "grid"

    photos = "D:\\Photos"
    panel.select_page("Space")
    panel.space.add_folder(photos)
    panel.space.scan_button.invoke()
    pump(app, 5.0, until=lambda: app._storage_job is not None and idle(app))
    job = app._storage_job
    assert job is not None
    engine.storage_finish(job, result=folder_scan_result(job, photos))
    pump(app, 5.0, until=lambda: app._storage_job is None and app._storage_scan_job == job)
    # The groups were found in the files of C:\, not D:\Photos.
    assert dupes.tree.rows == {}
    assert dupes.result is None
    assert not dupes.summary_label.winfo_manager()
    assert not dupes.details_label.winfo_manager()
    assert dupes.scope_label.cget("text").startswith("In: D:\\Photos")
    assert dupes.find_button.cget("state") == "normal", "the new scan can be searched"
    assert app.errors == []


def test_cleared_scan_results_clear_their_duplicate_groups(make_app: AppFactory) -> None:
    app, engine = make_app(elevated=False)
    panel = open_storage(app)
    job = scan(app, engine)
    find_duplicates(app, engine)
    engine.storage_forget(job)
    panel.select_page("Space")
    panel.space.tree.open_folder("n1")
    pump(app, 0.2)
    assert status(app) == (RESULTS_GONE_TEXT, theme.WARNING)
    dupes = panel.duplicates
    assert dupes.scope_label.cget("text") == NO_SCAN_TEXT
    assert dupes.tree.rows == {}
    assert dupes.result is None
    assert not dupes.summary_label.winfo_manager()
    assert dupes.find_button.cget("state") == "disabled"
    assert app.errors == []


def test_removing_a_leftover_is_irreversible_and_reloads(make_app: AppFactory) -> None:
    app, engine = make_app(elevated=True, storage_leftovers={"C:": [LEFTOVER]})
    panel = open_storage(app)
    assert len(panel.speed.leftover_rows) == 1
    _, leftover, button = panel.speed.leftover_rows[0]
    assert leftover["path"] == LEFTOVER["path"]
    assert button is not None
    kinds: list[tuple[bool, str, str]] = []
    original = app._set_busy

    def spy(busy: bool, close: str = "other", *, action: str = "", note: str = "") -> None:
        kinds.append((busy, close, action))
        original(busy, close, action=action, note=note)

    app._set_busy = spy  # type: ignore[method-assign]
    button.invoke()
    text = confirm_dialog_text(app)
    assert "Deletes C:\\CairnSpeedTest-0123abcd and the 1 GB test file in it" in text
    pump(app, 5.0, until=lambda: len(engine.calls_named("storage_volumes")) == 2 and idle(app))
    assert engine.calls_named("storage_remove_leftover") == [(LEFTOVER["path"],)]
    assert (True, "irreversible", "Removing the leftover test file") in kinds
    assert status(app)[1] == theme.GOOD
    assert panel.speed.leftover_rows == []
    assert app.errors == []


def test_a_leftover_in_use_has_no_remove_button(make_app: AppFactory) -> None:
    app, _ = make_app(elevated=True, storage_leftovers={"C:": [{**LEFTOVER, "in_use": True, "bytes": None}]})
    panel = open_storage(app)
    frame, _, button = panel.speed.leftover_rows[0]
    assert button is None
    labels = [w for w in frame.winfo_children() if isinstance(w, ctk.CTkLabel)]
    assert [label.cget("text") for label in labels] == ["◐ Another Cairn window is testing C:."]
    assert app.errors == []


def run_a_tool(app: App) -> None:
    show_section(app, "Tools")
    pump(app, 5.0, until=lambda: app.tools_panel.loaded and idle(app))
    app.tools_panel.rows["sfc_verify"].run_button.invoke()
    confirm_dialog(app)
    pump(app, 5.0, until=lambda: app._tool_job is not None and idle(app))
    show_section(app, "Storage")


def test_closing_with_a_speed_test_asks_after_the_tools_dialog(make_app: AppFactory) -> None:
    app, engine = make_app(elevated=True)
    open_storage(app)
    start_speed_test(app)
    run_a_tool(app)
    pump(app, 5.0, until=lambda: "◐ Speed test C:" in app.status_tool.cget("text"))
    assert app.status_tool.cget("text") == "◐ Check system files  ·  ◐ Speed test C:  ·  0 %"
    app._request_close()
    tools_dialog = next_dialog(app)
    assert tools_dialog.title_text == "Check system files is still running"
    tools_dialog.confirm_button.invoke()
    storage_dialog = next_dialog(app)
    assert storage_dialog.title_text == "The speed test of C: is still running"
    assert CLOSE_SPEED_MESSAGE in dialog_text(storage_dialog)
    assert storage_dialog.confirm_button.cget("text") == "Stop and close"
    assert storage_dialog.confirm_button.cget("fg_color") == theme.ACCENT, "not a danger dialog"
    storage_dialog._cancel()
    pump(app, 0.2)
    assert app._running
    assert engine.calls_named("storage_shutdown") == []

    app._request_close()
    confirm_dialog(app)
    confirm_dialog(app)
    assert not app._running
    assert engine.calls_named("storage_shutdown") == [()]
    assert engine.storage_jobs()[0]["state"] == "cancelled"
    assert app.errors == []


def test_closing_during_a_scan_asks_without_danger(make_app: AppFactory) -> None:
    app, engine = make_app(elevated=True)
    panel = open_storage(app)
    panel.select_page("Space")
    panel.space.scan_button.invoke()
    pump(app, 5.0, until=lambda: app._storage_job is not None and idle(app))
    app._request_close()
    dialog = next_dialog(app)
    assert dialog.title_text == "A storage scan is still running"
    assert CLOSE_SCAN_MESSAGE in dialog_text(dialog)
    assert dialog.confirm_button.cget("text") == "Close"
    assert dialog.confirm_button.cget("fg_color") == theme.ACCENT
    dialog._cancel()
    pump(app, 0.2)
    assert app._running
    # A scan stops at once, without a dialog.
    panel.space.scan_button.invoke()
    pump(app, 5.0, until=lambda: app._storage_job is None and idle(app))
    assert engine.calls_named("storage_cancel")
    assert not dialogs(app)
    assert app.errors == []


def test_a_failing_poll_latches_the_storage_lane_once(make_app: AppFactory) -> None:
    app, _ = make_app(elevated=True)
    panel = open_storage(app)
    panel.select_page("Space")
    panel.space.scan_button.invoke()
    pump(app, 5.0, until=lambda: app._storage_job is not None and idle(app))
    failed: list[int] = []
    original = app._storage_poll_failed

    def boom(_now: float) -> None:
        raise RuntimeError("poll failed")

    def recorded() -> None:
        failed.append(1)
        original()

    app._poll_storage = boom  # type: ignore[method-assign]
    app._storage_poll_failed = recorded  # type: ignore[method-assign]
    pump(app, 0.5)
    assert failed == [1]
    assert "storage" in app._lane_failed
    assert app._storage_job is None
    assert status(app) == ("Lost track of Scan of C:.", theme.CRITICAL)
    assert len(app.errors) == 1 and "poll failed" in app.errors[0]
    app.errors.clear()


@pytest.mark.parametrize("page", PAGES)
def test_every_page_fits_the_window(make_app: AppFactory, page: str) -> None:
    app, engine = make_app(elevated=True, storage_leftovers={"C:": [LEFTOVER]})
    panel = open_storage(app)
    if page != "Speed test":
        scan(app, engine)
    panel.select_page(page)
    assert_section_fits(app, "Storage")
    assert panel.page == page
    if page == "Space":
        assert panel.space.tree.winfo_ismapped()
    assert app.errors == []


def test_the_speed_controls_stay_in_view_while_the_results_scroll(make_app: AppFactory) -> None:
    app, _ = make_app(elevated=True, storage_leftovers={"C:": [LEFTOVER]})
    speed = open_storage(app).speed
    _, _, remove_button = speed.leftover_rows[0]
    controls = {
        "Start test": speed.start_button,
        "Drive": speed.drive_menu,
        "Size": speed.size_menu,
        "Runs": speed.runs_menu,
        "write warning": speed.warning_label,
        "Remove": remove_button,
    }
    geometry = app.geometry()
    app.minsize(900, 560)
    for width, height in FIT_SIZES:
        resize_to(app, width, height)
        for end in (0.0, 1.0):
            scroll_canvas(speed.results_card).yview_moveto(end)
            pump(app, 0.2)
            outside = [name for name, widget in controls.items() if not inside_section(app, widget)]
            assert outside == [], f"{width}x{height}, results scrolled to {end}: {outside}"
        assert inside_section(app, speed.progress_label), "the end of the results can be reached"
    assert not isinstance(speed, ctk.CTkScrollableFrame), "only the results scroll"
    app.geometry(geometry)
    pump(app, 0.3)
    assert app.errors == []


def test_the_tree_is_visible_at_the_smallest_window(make_app: AppFactory) -> None:
    app, engine = make_app(elevated=True)
    panel = open_storage(app)
    scan(app, engine)
    app.minsize(900, 560)
    app.geometry("1120x700")
    pump(app, 0.5)
    assert panel.space.tree.winfo_ismapped()
    assert panel.space.tree.tree.winfo_height() > 60
    assert app.errors == []


def storage_calls(engine: FakeEngine) -> list[str]:
    return [name for name, _ in engine.calls if name.startswith("storage_")]


def test_status_names_a_busy_lane(make_app: AppFactory) -> None:
    app, engine = make_app(elevated=True)
    panel = open_storage(app)
    start_speed_test(app)
    assert panel.space.scan_button.cget("state") == "disabled"
    app._on_scan("C:\\")
    pump(app, 0.2)
    assert status(app) == (
        "Speed test of C: is running; wait for it to finish or stop it.",
        theme.WARNING,
    )
    assert "storage_scan_start" not in storage_calls(engine)
    expected: Any = app._storage_job
    engine.storage_finish(expected)
    pump(app, 5.0, until=lambda: app._storage_job is None and idle(app))
    assert app.errors == []
