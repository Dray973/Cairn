"""Dashboard integration tests: the real window, the real telemetry DLL and a fake engine.

Flows that apply and revert changes run against the in-memory FakeEngine, so nothing on
the machine is modified.
"""

from __future__ import annotations

import threading
import time
import tkinter as tk
from typing import Any

from optimizer.app import BUSY_CLOSE_TITLE, CLOSE_IRREVERSIBLE, CLOSE_JOURNALED, CLOSE_READ, CLOSE_TEXTS
from optimizer.widgets.charts import TimeSeriesChart
from optimizer.widgets.controls import PANEL_WRAP, ROW_WRAP
from optimizer.widgets.monitor import TABLE_ROWS

from .app_support import (
    REVERT_RESTART_TEXT,
    SECTIONS,
    STATUS_UNKNOWN,
    App,
    AppFactory,
    MessageDialog,
    confirm_dialog,
    confirm_dialog_text,
    ctk,
    dialog_text,
    dialogs,
    idle,
    next_dialog,
    pump,
    rows_by_id,
    scanned,
    show_section,
    theme,
)
from .fake_engine import CEIP_TASKS, POLICY_NOTE

TERMINAL_TASK = "Microsoft.WindowsTerminal_8wekyb3d8bbwe\\StartTerminalOnLoginTask"
CEIP_ID = "privacy.ceip_tasks"
# FakeEngine options of a Privacy tweak that turns off the two CEIP_TASKS.
CEIP_TWEAK: dict[str, Any] = {
    "extra_tweaks": [(CEIP_ID, "privacy", True)],
    "scheduled_task_tweaks": {CEIP_ID: CEIP_TASKS},
}


def record_statuses(app: App) -> list[str]:
    """Records every status bar message from now on; a change is followed by a rescan whose
    messages replace the result at once."""
    shown: list[str] = []
    set_status = app.set_status

    def record(text: str, color: str = theme.INK_SECONDARY) -> None:
        shown.append(text)
        set_status(text, color)

    app.set_status = record  # type: ignore[method-assign]
    return shown


def cancel_shown(app: App, dialog: MessageDialog) -> None:
    """Cancels `dialog` once its window has finished appearing.

    A dialog destroyed before its deferred window setup ran (title bar colour, grab, focus)
    can crash Tk the next time the event loop runs; a user never closes one that fast.
    """
    pump(app, 0.3)
    dialog._cancel()


def open_history(app: App, rows: int) -> None:
    show_section(app, "History")
    pump(app, 5.0, until=lambda: app.history_panel.row_count == rows and idle(app))


def resize(app: App, size: str) -> None:
    """Sets the window to `size` ("WxH") and waits until the new layout is drawn."""
    width, height = (int(v) for v in size.split("x"))
    app.geometry(size)
    pump(app, 5.0, until=lambda: (app.winfo_width(), app.winfo_height()) == (width, height))
    pump(app, 0.5)


def top_in(widget: tk.Misc, ancestor: tk.Misc) -> int:
    return widget.winfo_rooty() - ancestor.winfo_rooty()


def assert_inside(widget: tk.Misc, ancestor: tk.Misc) -> None:
    top = top_in(widget, ancestor)
    bottom = top + widget.winfo_height()
    assert 0 <= top and bottom <= ancestor.winfo_height(), (
        f"{widget} spans {top}..{bottom} of {ancestor}, which is {ancestor.winfo_height()} px tall"
    )


def assert_labels_readable(chart: TimeSeriesChart) -> None:
    """The chart's drawn axis labels lie inside the canvas and do not overlap one another."""
    items = [i for i in chart._y_labels + chart._x_labels if chart.itemcget(i, "state") != "hidden"]
    boxes = [chart.bbox(i) for i in items]
    height = chart.winfo_height()
    for box in boxes:
        assert 0 <= box[1] and box[3] <= height, f"label {box} lies outside the {height} px chart"
    for n, a in enumerate(boxes):
        for b in boxes[n + 1 :]:
            overlap = a[0] < b[2] and b[0] < a[2] and a[1] < b[3] and b[1] < a[3]
            assert not overlap, f"labels {a} and {b} overlap"


def close_dialog(app: App) -> MessageDialog:
    """Asks to close the window while an engine call runs and returns the dialog shown."""
    app._request_close()
    dialog = next_dialog(app)
    assert dialog.title_text == BUSY_CLOSE_TITLE
    return dialog


def test_monitor_renders_live_data_at_60_hz(make_app: AppFactory) -> None:
    app, _ = make_app(elevated=False)
    # The rate is that of the running window: the start-up reads and the first scan are done.
    pump(app, 5.0, until=lambda: app._telemetry is not None and scanned(app))
    pump(app, 2.0)
    # Another program can hold the whole machine up for a moment; the frame loop must still
    # keep the rate over a full second of the next few.
    deadline = time.perf_counter() + 6.0
    while app.fps < 50 and time.perf_counter() < deadline:
        pump(app, 0.1)
    assert app.errors == []
    assert app.fps >= 50, f"UI ran at {app.fps:.1f} fps"
    assert app.monitor.cpu.chart.point_count > 10
    assert app.monitor.memory.chart.point_count > 10
    assert app.monitor.cpu.hero.cget("text").endswith("%")
    assert "GB" in app.monitor.memory.hero.cget("text")
    assert app.monitor.processes._cells[0][0].cget("text"), "process table is empty"
    assert len(app.monitor.cpu.bars._bars) >= 1


def test_chart_hover_shows_value_tooltip(make_app: AppFactory) -> None:
    app, _ = make_app(elevated=False)
    pump(app, 5.0, until=lambda: app.monitor.cpu.chart.point_count > 20)
    chart = app.monitor.cpu.chart
    x = chart.winfo_width() - chart.PAD_RIGHT - 5
    chart.event_generate("<Motion>", x=x, y=chart.winfo_height() // 2)
    pump(app, 0.2)
    assert chart.itemcget(chart._tip, "state") == "normal"
    assert chart.itemcget(chart._tip, "text").startswith("CPU ")
    chart.event_generate("<Leave>")
    pump(app, 0.1)
    assert chart.itemcget(chart._tip, "state") == "hidden"

    bars = app.monitor.cpu.bars
    bars.event_generate("<Motion>", x=bars.PAD_X + 2, y=bars.winfo_height() // 2)
    pump(app, 0.2)
    assert bars.itemcget(bars._tip, "text").startswith("Core 0: ")
    assert app.errors == []


ALL_CHART_LABELS = ["100%", "50%", "0%", "60 s ago", "30 s", "now"]


def test_dashboard_fits_the_smallest_window(make_app: AppFactory) -> None:
    app, _ = make_app(elevated=False)
    monitor = app.monitor
    resize(app, "1120x700")
    chart = monitor.cpu.chart
    assert chart.winfo_height() >= 60, "the CPU history keeps a usable height"
    assert chart.shown_labels == ALL_CHART_LABELS
    for card in (monitor.cpu, monitor.memory, monitor.processes):
        for child in card.winfo_children():
            if child.winfo_ismapped():
                assert_inside(child, card)
    assert_labels_readable(chart)
    assert_labels_readable(monitor.memory.chart)

    # The process table drops the rows that do not fit instead of cutting one.
    processes = monitor.processes
    assert 5 <= processes.shown_rows < TABLE_ROWS
    table = processes._cells[0][0].master
    assert table is not None
    for r, row in enumerate(processes._cells):
        for cell in row:
            assert cell.winfo_ismapped() == (r < processes.shown_rows)
            if r < processes.shown_rows:
                assert_inside(cell, table)

    # The default window size gives the charts their room back and shows every row.
    resize(app, "1440x900")
    assert processes.shown_rows == TABLE_ROWS
    assert chart.winfo_height() >= 150
    assert chart.shown_labels == ALL_CHART_LABELS
    assert_labels_readable(chart)
    assert app.errors == []


def test_chart_hides_labels_that_would_overlap(make_app: AppFactory) -> None:
    app, _ = make_app(elevated=False)
    chart = TimeSeriesChart(app, color=theme.CPU, fill=theme.CPU_FILL, label="CPU", height=64)
    # Canvas height -> the labels that fit it.
    expected = {
        64: ALL_CHART_LABELS,
        40: ["100%", "0%"],
        17: [],
    }
    for height, labels in expected.items():
        chart.place(x=20, y=20, width=400, height=height)
        pump(app, 5.0, until=lambda h=height: chart.winfo_height() == h)
        pump(app, 0.1)
        assert chart.shown_labels == labels, f"{height} px"
        assert_labels_readable(chart)
    chart.destroy()
    assert app.errors == []


def test_scan_populates_results_and_mode_status(make_app: AppFactory) -> None:
    app, engine = make_app(elevated=False)
    pump(app, 5.0, until=lambda: app.optimize_list.row_count > 0)
    assert app.optimize_list.row_count == 5
    assert app.apps_list.row_count == 1
    assert "Off" in app.toggles["privacy"].status.cget("text")
    assert engine.calls_named("scan")
    # A standard user cannot change system-wide settings.
    assert app.toggles["privacy"].switch.cget("state") == "disabled"
    assert app.revert_button.cget("state") == "disabled"
    assert app.errors == []


def test_turning_a_mode_on_and_off_goes_through_confirmation(
    make_app: AppFactory,
) -> None:
    app, engine = make_app(elevated=True)
    pump(app, 5.0, until=lambda: app.optimize_list.row_count > 0)

    app.toggles["privacy"].switch.toggle()
    assert not app.toggles["privacy"].switch.get(), "switch moved before confirmation"
    confirm_dialog(app)
    pump(app, 5.0, until=lambda: "On" in app.toggles["privacy"].status.cget("text"))
    assert engine.calls_named("apply_category")[-1] == ("privacy", "try", False)
    assert app.toggles["privacy"].switch.get()

    app.toggles["privacy"].switch.toggle()
    confirm_dialog(app)
    pump(app, 5.0, until=lambda: "Off" in app.toggles["privacy"].status.cget("text"))
    assert engine.calls_named("revert")[-1] == (["privacy.activity_history", "privacy.cortana"], False)
    assert not app.toggles["privacy"].switch.get()
    assert app.errors == []


def test_revert_all_requires_confirmation(make_app: AppFactory) -> None:
    app, engine = make_app(elevated=True)
    pump(app, 5.0, until=lambda: app.optimize_list.row_count > 0)
    engine.applied.update({"privacy.cortana", "gaming.game_mode"})

    app.revert_button.invoke()
    pump(app, 5.0, until=lambda: bool(dialogs(app)))
    cancel = dialogs(app)[-1]
    cancel._cancel()
    pump(app, 0.3)
    assert not [c for c in engine.calls_named("revert_all") if c == (False,)], "reverted without confirmation"

    app.revert_button.invoke()
    confirm_dialog(app)
    pump(app, 5.0, until=lambda: (False,) in engine.calls_named("revert_all"))
    pump(app, 5.0, until=lambda: not engine.applied)
    assert app.errors == []


def test_item_action_applies_one_item(make_app: AppFactory) -> None:
    app, engine = make_app(elevated=True)
    pump(app, 5.0, until=lambda: app.optimize_list.row_count > 0)
    item = next(i for i in app._last_scan["items"] if i["id"] == "appx.Microsoft.BingNews")  # type: ignore[index]
    app._on_item_action(item, "apply")
    confirm_dialog(app)
    pump(app, 5.0, until=lambda: bool(engine.calls_named("apply")))
    assert engine.calls_named("apply")[-1][0] == ["appx.Microsoft.BingNews"]
    assert app.errors == []


def test_all_sections_exist_in_sidebar_order(
    make_app: AppFactory,
) -> None:
    app, _ = make_app(elevated=False)
    assert list(app.nav.items) == [s.name for s in SECTIONS]
    pump(app, 5.0, until=lambda: app.monitor.cpu.chart.point_count > 5)
    show_section(app, "Optimize")
    before = app.monitor.cpu.chart.point_count
    pump(app, 0.8)
    assert app.monitor.cpu.chart.point_count > before, "hidden dashboard stopped recording"
    assert app.errors == []


def test_optimize_filter_and_apps_bulk_remove(make_app: AppFactory) -> None:
    app, engine = make_app(elevated=True)
    pump(app, 5.0, until=lambda: app.optimize_list.row_count > 0)
    assert app.optimize_list.filter_bar is not None
    app.optimize_list._filter_changed("Windows")
    assert [i["category"] for i in app.optimize_list.visible_items()] == ["interface"]
    app.optimize_list._filter_changed("All")
    assert len(app.optimize_list.visible_items()) == 5

    show_section(app, "Apps")
    assert app.apps_list.bulk_button is not None
    app.apps_list.bulk_button.invoke()
    confirm_dialog(app)
    pump(app, 5.0, until=lambda: bool(engine.calls_named("apply")))
    assert engine.calls_named("apply")[-1][0] == ["appx.Microsoft.BingNews"]
    assert app.errors == []


def test_cleanup_tab_opened_during_the_first_scan_measures_once_it_ends(make_app: AppFactory) -> None:
    app, engine = make_app(elevated=True, slow={"scan": 1.0})
    pump(app, 5.0, until=lambda: app._busy)
    show_section(app, "Cleanup")
    show_section(app, "Optimize")
    show_section(app, "Cleanup")
    assert app.cleanup_panel.summary.cget("text") == "Measuring…"
    pump(app, 8.0, until=lambda: app.cleanup_panel.row_count == 4)
    assert len(engine.calls_named("cleanup_scan")) == 1, "reopening the section queues no second scan"
    assert app.errors == []


def test_cleanup_tab_scans_on_open_and_cleans_selection(
    make_app: AppFactory,
) -> None:
    app, engine = make_app(elevated=True)
    pump(app, 5.0, until=lambda: app.optimize_list.row_count > 0)
    show_section(app, "Cleanup")
    pump(app, 5.0, until=lambda: app.cleanup_panel.row_count == 4)
    chosen = [t["id"] for t in app.cleanup_panel.selected_targets()]
    # Default-on targets with data; Recycle Bin is off by default and Chrome is blocked.
    assert chosen == ["user_temp", "windows_temp"]
    app.cleanup_panel.clean_button.invoke()
    confirm_dialog(app)
    pump(app, 5.0, until=lambda: bool(engine.calls_named("cleanup_run")))
    assert engine.calls_named("cleanup_run")[-1][0] == ["user_temp", "windows_temp"]
    pump(app, 2.0, until=lambda: "Freed" in app.status_message.cget("text"))
    assert app.errors == []


def test_startup_tab_toggles_an_entry(make_app: AppFactory) -> None:
    app, engine = make_app(elevated=True)
    pump(app, 5.0, until=lambda: app.optimize_list.row_count > 0)
    show_section(app, "Startup")
    pump(app, 5.0, until=lambda: app.startup_panel.row_count == 3)
    spotify = next(r for r in app.startup_panel._rows if r.entry["name"] == "Spotify")
    spotify.switch.toggle()
    pump(app, 5.0, until=lambda: bool(engine.calls_named("startup_set_enabled")))
    assert engine.calls_named("startup_set_enabled")[-1] == ("user_run:Spotify", False, "skip")
    pump(
        app,
        5.0,
        until=lambda: any(
            r.entry["name"] == "Spotify" and not r.entry["enabled"] for r in app.startup_panel._rows
        ),
    )
    assert app.errors == []


def test_history_tab_groups_changes_and_undoes_one(
    make_app: AppFactory,
) -> None:
    app, engine = make_app(elevated=True)
    engine.applied.update({"privacy.cortana", "gaming.game_mode"})
    pump(app, 5.0, until=lambda: app.optimize_list.row_count > 0 and bool(app._titles))
    show_section(app, "History")
    pump(app, 5.0, until=lambda: app.history_panel.row_count == 2)
    titles = {g.title for g in app.history_panel.groups}
    assert titles == {"Cortana", "Game Mode"}
    group = next(g for g in app.history_panel.groups if g.title == "Cortana")
    app._on_history_undo(group)
    confirm_dialog(app)
    pump(app, 5.0, until=lambda: bool(engine.calls_named("revert_targets")))
    sent = engine.calls_named("revert_targets")[-1][0]
    assert sent["registry"] == [{"hive": "HKLM", "key_path": "Test", "value_name": "privacy.cortana"}]
    pump(app, 5.0, until=lambda: "privacy.cortana" not in engine.applied)
    assert app.errors == []


def test_undo_is_offered_only_for_changes_this_tool_recorded(
    make_app: AppFactory,
) -> None:
    # File extensions were already shown before the tool ran; Cortana was changed by it.
    app, engine = make_app(elevated=True, preset={"interface.file_extensions"})
    engine.applied.add("privacy.cortana")
    pump(app, 5.0, until=lambda: scanned(app))
    rows = rows_by_id(app)

    preset = rows["interface.file_extensions"]
    assert preset.item["state"] == "applied"
    assert preset.action_button is None
    assert preset.tag_label is not None
    assert preset.tag_label.cget("text") == "Already set"
    assert preset.tag_label.cget("text_color") == theme.INK_MUTED

    recorded = rows["privacy.cortana"]
    assert recorded.action_button is not None
    assert recorded.action_button.cget("text") == "Undo"
    assert recorded.tag_label is None

    # An Undo that reaches an item with nothing recorded stops at the dry run.
    app._on_item_action(preset.item, "revert")
    pump(app, 5.0, until=lambda: "nothing recorded" in app.status_message.cget("text"))
    assert engine.calls_named("revert") == [(["interface.file_extensions"], True)]
    assert not dialogs(app)

    # A recorded change is confirmed with the journal actions, then reverted.
    pump(app, 5.0, until=lambda: idle(app))
    app._on_item_action(recorded.item, "revert")
    text = confirm_dialog_text(app)
    assert "delete value: privacy.cortana" in text
    pump(app, 5.0, until=lambda: "privacy.cortana" not in engine.applied)
    assert engine.calls_named("revert")[-1] == (["privacy.cortana"], False)
    assert app.errors == []


def test_item_note_is_shown_as_a_warning(make_app: AppFactory) -> None:
    app, _ = make_app(elevated=True, notes={"privacy.cortana": "Has no effect on this Windows build."})
    pump(app, 5.0, until=lambda: scanned(app))
    rows = rows_by_id(app)
    note = rows["privacy.cortana"].note_label
    assert note is not None
    assert note.cget("text") == "⚠ Has no effect on this Windows build."
    assert note.cget("text_color") == theme.WARNING
    assert rows["gaming.game_mode"].note_label is None
    assert app.errors == []


# As long as the catalog's longest descriptions and scan notes.
LONG_DESCRIPTION = (
    "Sets the diagnostic data policy to Required. Windows 11 Enterprise and Education stop sending "
    "diagnostic data; Home and Pro treat the value as Required and keep sending the required data. "
    "The service that uploads diagnostic data keeps running unless it is turned off as well."
)
LONG_NOTE = (
    "2 scheduled tasks this tool turned off were turned back on; apply again to turn them off. A feature "
    "update of Windows can turn them back on."
)


def test_optimize_list_wraps_text_to_the_width_it_has(make_app: AppFactory) -> None:
    long_ids = ("privacy.cortana", "gaming.game_mode")
    app, _ = make_app(
        elevated=True,
        descriptions=dict.fromkeys(long_ids, LONG_DESCRIPTION),
        notes={"privacy.cortana": LONG_NOTE},
    )
    pump(app, 5.0, until=lambda: scanned(app))
    show_section(app, "Optimize")
    panel = app.optimize_list

    resize(app, "1120x700")
    assert panel.row_wrap < ROW_WRAP, "the text column is narrower than the usual line length"
    assert panel.summary.cget("wraplength") < PANEL_WRAP
    rows = rows_by_id(app)
    assert rows["privacy.cortana"].note_label is not None
    for item_id in long_ids:
        for label in rows[item_id].wrapped_labels:
            assert label.cget("wraplength") == panel.row_wrap
            assert label.winfo_width() >= label.winfo_reqwidth(), f"{item_id}: text is cut off"

    # Rows built by the next scan get the current wrap length.
    app.start_scan()
    pump(app, 5.0, until=lambda: scanned(app))
    rows = rows_by_id(app)
    assert rows["privacy.cortana"].description_label.cget("wraplength") == panel.row_wrap

    # A wide window keeps the usual line length.
    resize(app, "1440x900")
    assert panel.row_wrap == ROW_WRAP
    assert panel.summary.cget("wraplength") == PANEL_WRAP
    for label in rows["privacy.cortana"].wrapped_labels:
        assert label.cget("wraplength") == ROW_WRAP
        assert label.winfo_width() >= label.winfo_reqwidth()
    assert app.errors == []


def test_revert_offers_the_restart_it_needs(make_app: AppFactory) -> None:
    app, engine = make_app(elevated=True, revert_restart="explorer")
    engine.applied.add("interface.file_extensions")
    pump(app, 5.0, until=lambda: scanned(app))
    item = rows_by_id(app)["interface.file_extensions"].item
    app._on_item_action(item, "revert")
    text = confirm_dialog_text(app)
    assert REVERT_RESTART_TEXT["explorer"] in text, "the confirmation names the restart up front"

    pump(
        app,
        5.0,
        until=lambda: any(d.confirm_button.cget("text") == "Restart Explorer now" for d in dialogs(app)),
    )
    dialog = dialogs(app)[-1]
    text = dialog_text(dialog)
    assert "Changes restored" in text
    assert "File Explorer has to restart" in text
    dialog.confirm_button.invoke()
    pump(app, 5.0, until=lambda: bool(engine.calls_named("restart_explorer")))
    assert app.errors == []


def test_turn_off_and_revert_all_confirmations_name_the_restart(
    make_app: AppFactory,
) -> None:
    app, engine = make_app(elevated=True, revert_restart="explorer")
    engine.applied.add("interface.file_extensions")
    pump(app, 5.0, until=lambda: scanned(app))
    toggle = app.toggles["interface"]
    assert toggle.switch.get()

    toggle.switch.toggle()
    dialog = next_dialog(app)
    text = dialog_text(dialog)
    assert "Turn off Clean Interface" in text
    assert REVERT_RESTART_TEXT["explorer"] in text
    dialog._cancel()
    pump(app, 5.0, until=lambda: idle(app) and not dialogs(app))

    app.revert_button.invoke()
    dialog = next_dialog(app)
    text = dialog_text(dialog)
    assert "Revert all changes?" in text
    assert REVERT_RESTART_TEXT["explorer"] in text
    dialog._cancel()
    pump(app, 0.2)
    assert engine.applied == {"interface.file_extensions"}
    assert not [c for c in engine.calls_named("revert") if c[1] is False]
    assert (False,) not in engine.calls_named("revert_all")
    assert app.errors == []


def test_revert_without_restart_needs_no_dialog(make_app: AppFactory) -> None:
    app, engine = make_app(elevated=True)
    engine.applied.add("interface.file_extensions")
    pump(app, 5.0, until=lambda: scanned(app))
    app._on_item_action(rows_by_id(app)["interface.file_extensions"].item, "revert")
    confirm_dialog(app)
    pump(app, 5.0, until=lambda: (["interface.file_extensions"], False) in engine.calls_named("revert"))
    pump(app, 5.0, until=lambda: scanned(app))
    assert not dialogs(app)
    assert not engine.calls_named("restart_explorer")
    assert app.errors == []


def test_standard_user_can_switch_per_user_startup_entries(
    make_app: AppFactory,
) -> None:
    app, engine = make_app(
        elevated=False,
        extra_startup=[("machine_run", "Contoso Updater", True), ("policy_run", "Managed Agent", False)],
    )
    pump(app, 5.0, until=lambda: scanned(app))
    show_section(app, "Startup")
    pump(app, 5.0, until=lambda: app.startup_panel.row_count == 5 and idle(app))
    rows = {r.entry["name"]: r for r in app.startup_panel._rows}

    spotify = rows["Spotify"]
    assert spotify.switch is not None
    assert spotify.switch.cget("state") == "normal"
    assert spotify.hint_label is None
    machine = rows["Contoso Updater"]
    assert machine.switch is not None
    assert machine.switch.cget("state") == "disabled"
    assert machine.hint_label is not None
    assert "administrator" in machine.hint_label.cget("text")
    fixed = rows["Managed Agent"]
    assert fixed.switch is None
    assert fixed.hint_label is not None
    assert fixed.hint_label.cget("text") == POLICY_NOTE, "the engine's reason is shown"

    spotify.switch.toggle()
    pump(app, 5.0, until=lambda: bool(engine.calls_named("startup_set_enabled")))
    assert engine.calls_named("startup_set_enabled") == [("user_run:Spotify", False, "skip")]
    assert not dialogs(app)
    pump(
        app,
        5.0,
        until=lambda: any(
            r.entry["name"] == "Spotify" and not r.entry["enabled"] for r in app.startup_panel._rows
        ),
    )

    # The per-user change can be undone from History without elevation.
    show_section(app, "History")
    pump(app, 5.0, until=lambda: app.history_panel.row_count == 1 and idle(app))
    row = app.history_panel.rows[0]
    assert row.change.title == "Startup app: Spotify (HKCU Run)"
    assert row.undo_button.cget("state") == "normal"
    row.undo_button.invoke()
    confirm_dialog(app)
    pump(app, 5.0, until=lambda: bool(engine.calls_named("revert_targets")))
    pump(
        app,
        5.0,
        until=lambda: any(
            r.entry["name"] == "Spotify" and r.entry["enabled"] for r in app.startup_panel._rows
        ),
    )
    assert app.errors == []


def test_history_names_packaged_startup_tasks_like_the_startup_tab(
    make_app: AppFactory,
) -> None:
    app, engine = make_app(elevated=False, extra_startup=[("packaged_task", "Terminal", True, TERMINAL_TASK)])
    engine.startup_set_enabled(f"packaged_task:{TERMINAL_TASK}", False)
    pump(app, 5.0, until=lambda: scanned(app))

    # History is opened first; the display name comes from the startup list it reads.
    show_section(app, "History")
    pump(app, 5.0, until=lambda: app.history_panel.row_count == 1 and idle(app))
    row = app.history_panel.rows[0]
    assert row.change.title == "Startup app: Terminal (Packaged app)"
    terminal = next(r for r in app.startup_panel._rows if r.entry["name"] == "Terminal")
    assert terminal.entry["location"] == "Packaged app"
    assert not terminal.entry["enabled"]

    # A per-user task, so a standard user can undo it.
    assert row.undo_button.cget("state") == "normal"
    row.undo_button.invoke()
    confirm_dialog(app)
    pump(
        app,
        5.0,
        until=lambda: any(
            r.entry["name"] == "Terminal" and r.entry["enabled"] for r in app.startup_panel._rows
        ),
    )
    sent = engine.calls_named("revert_targets")[-1][0]
    assert sent["registry"][0]["key_path"].endswith("\\SystemAppData\\" + TERMINAL_TASK)
    assert app.errors == []


def test_turning_a_mode_off_keeps_tweaks_that_are_not_recommended(
    make_app: AppFactory,
) -> None:
    app, engine = make_app(elevated=True, extra_tweaks=[("privacy.location", "privacy", False)])
    engine.applied.add("privacy.location")
    pump(app, 5.0, until=lambda: scanned(app))
    toggle = app.toggles["privacy"]
    assert not toggle.switch.get(), "a non-recommended item does not turn the mode on"

    toggle.switch.toggle()
    confirm_dialog(app)
    pump(app, 5.0, until=lambda: "✓ On" in toggle.status.cget("text") and idle(app))

    toggle.switch.toggle()
    text = confirm_dialog_text(app)
    assert "privacy.location" not in text
    pump(app, 5.0, until=lambda: "Off" in toggle.status.cget("text") and idle(app))
    assert engine.calls_named("revert")[-1] == (["privacy.activity_history", "privacy.cortana"], False)
    assert not engine.calls_named("revert_category")
    assert engine.applied == {"privacy.location"}
    assert app.errors == []


def test_partly_on_mode_can_apply_the_rest(make_app: AppFactory) -> None:
    app, engine = make_app(elevated=True)
    engine.applied.add("privacy.activity_history")
    pump(app, 5.0, until=lambda: scanned(app))
    toggle = app.toggles["privacy"]
    assert "Partly on" in toggle.status.cget("text")
    assert toggle.switch.get(), "a partly-on mode shows its switch on"
    assert toggle.partial
    assert toggle.remaining_button.grid_info(), "Apply remaining is shown"
    assert not app.toggles["gaming"].remaining_button.grid_info()

    toggle.remaining_button.invoke()
    text = confirm_dialog_text(app)
    assert "privacy.cortana" in text
    pump(app, 5.0, until=lambda: "✓ On" in toggle.status.cget("text") and idle(app))
    assert engine.calls_named("apply_category")[-1] == ("privacy", "try", False)
    assert engine.applied == {"privacy.activity_history", "privacy.cortana"}
    assert not toggle.remaining_button.grid_info()
    assert app.errors == []


def test_values_already_in_place_do_not_turn_a_mode_on(
    make_app: AppFactory,
) -> None:
    # Cortana and Game Mode were already set before the tool ran: nothing is recorded.
    app, engine = make_app(elevated=True, preset={"privacy.cortana", "gaming.game_mode"})
    pump(app, 5.0, until=lambda: scanned(app))
    privacy = app.toggles["privacy"]
    assert "Partly on" in privacy.status.cget("text")
    assert not privacy.switch.get(), "a value already in place does not turn the switch on"
    assert privacy.remaining_button.grid_info()
    gaming = app.toggles["gaming"]
    assert "✓ On" in gaming.status.cget("text")
    assert gaming.switch.get(), "a mode whose values are all in place is on"

    # With the switch off, flipping it turns the mode on.
    privacy.switch.toggle()
    text = confirm_dialog_text(app)
    assert "Turn on Privacy Mode" in text
    pump(app, 5.0, until=lambda: "✓ On" in privacy.status.cget("text") and idle(app))
    assert privacy.switch.get()
    assert engine.applied == {"privacy.activity_history"}

    # Turning it off restores what the tool changed; the value that was in place stays.
    privacy.switch.toggle()
    text = confirm_dialog_text(app)
    assert "delete value: privacy.activity_history" in text
    assert "delete value: privacy.cortana" not in text
    pump(app, 5.0, until=lambda: "Partly on" in privacy.status.cget("text") and idle(app))
    assert not privacy.switch.get()
    assert not engine.applied

    # A mode with nothing recorded explains that there is nothing to turn off.
    gaming.switch.toggle()
    pump(app, 5.0, until=lambda: "nothing recorded to undo" in app.status_message.cget("text"))
    assert not dialogs(app)
    assert gaming.switch.get()
    assert app.errors == []


def test_partly_on_mode_can_be_turned_off(make_app: AppFactory) -> None:
    app, engine = make_app(elevated=True)
    engine.applied.add("privacy.activity_history")
    pump(app, 5.0, until=lambda: scanned(app))
    toggle = app.toggles["privacy"]
    toggle.switch.toggle()
    assert toggle.switch.get(), "switch moved before confirmation"
    text = confirm_dialog_text(app)
    assert "Turn off Privacy Mode" in text
    pump(app, 5.0, until=lambda: "Off" in toggle.status.cget("text") and idle(app))
    assert engine.calls_named("revert")[-1] == (["privacy.activity_history", "privacy.cortana"], False)
    assert not engine.applied
    assert not toggle.switch.get()
    assert app.errors == []


APPX_ERROR = (
    "Get-AppxPackage : Access is denied.\n"
    "At line:1 char:1\n"
    "+ CategoryInfo          : NotSpecified: (:) [Get-AppxPackage], UnauthorizedAccessException\n"
    "+ FullyQualifiedErrorId : System.UnauthorizedAccessException"
)


def test_scan_warnings_are_shown_in_both_lists(make_app: AppFactory) -> None:
    app, _ = make_app(elevated=True, unreadable={"privacy.cortana"}, appx_inventory_error=APPX_ERROR)
    pump(app, 5.0, until=lambda: scanned(app))

    apps = app.apps_list
    assert apps.row_count == 0
    assert apps.warnings == [f"Store package inventory unavailable: {APPX_ERROR}"]
    assert "⚠ Store package inventory unavailable" in apps.warning_label.cget("text")
    assert "FullyQualifiedErrorId" in apps.warning_label.cget("text"), "the list shows the full text"
    assert apps.warning_label.cget("text_color") == theme.WARNING
    assert apps.warning_label.grid_info()
    assert apps.empty_label is not None
    assert "No known bloatware" not in apps.empty_label.cget("text")
    assert "Access is denied" not in apps.empty_label.cget("text"), "the warning is not repeated"
    assert apps.empty_label.cget("text_color") == theme.WARNING

    optimize = app.optimize_list
    assert optimize.warnings == ["privacy.cortana: cannot read registry value: access denied"]
    assert optimize.warning_label.grid_info()
    row = rows_by_id(app)["privacy.cortana"]
    assert row.warning_labels
    assert row.warning_labels[0].cget("text") == "⚠ cannot read registry value: access denied"
    assert row.warning_labels[0].cget("text_color") == theme.WARNING

    assert "2 warnings" in app.status_message.cget("text")
    assert app.status_message.cget("text_color") == theme.WARNING
    assert app.errors == []


def test_status_bar_shows_the_first_line_of_a_warning(
    make_app: AppFactory,
) -> None:
    app, _ = make_app(elevated=True, appx_inventory_error=APPX_ERROR)
    pump(app, 5.0, until=lambda: scanned(app))
    assert app.status_message.cget("text") == (
        "Scan complete with 1 warning: Store package inventory unavailable: "
        "Get-AppxPackage : Access is denied. …"
    )
    assert app.errors == []


def test_scan_without_warnings_hides_the_warning_line(
    make_app: AppFactory,
) -> None:
    app, _ = make_app(elevated=True)
    pump(app, 5.0, until=lambda: scanned(app))
    for panel in (app.optimize_list, app.apps_list):
        assert panel.warnings == []
        assert not panel.warning_label.grid_info()
    assert app.status_message.cget("text").startswith("Scan complete:")
    assert app.errors == []


def _locked_scan() -> dict[str, Any]:
    raise RuntimeError("database is locked")


def test_failed_rescan_after_a_change_shows_the_mode_status_as_unknown(
    make_app: AppFactory,
) -> None:
    app, engine = make_app(elevated=True)
    pump(app, 5.0, until=lambda: scanned(app))

    engine.scan = _locked_scan  # type: ignore[method-assign]
    toggle = app.toggles["privacy"]
    toggle.switch.toggle()
    confirm_dialog(app)
    pump(app, 5.0, until=lambda: "Scan failed" in app.status_message.cget("text") and idle(app))
    assert engine.applied == {"privacy.activity_history", "privacy.cortana"}
    # The scan from before the change is out of date, so no state is shown.
    assert toggle.status.cget("text") == STATUS_UNKNOWN
    assert app.toggles["gaming"].status.cget("text") == STATUS_UNKNOWN
    assert not toggle.switch.get()
    assert toggle.switch.cget("state") == "normal"
    assert app._last_scan is None

    del engine.scan  # the class method again
    app.start_scan()
    pump(app, 5.0, until=lambda: "✓ On" in toggle.status.cget("text") and idle(app))
    assert toggle.switch.get()
    assert app.errors == []


def test_failed_scan_without_a_change_keeps_the_last_status(
    make_app: AppFactory,
) -> None:
    app, engine = make_app(elevated=True)
    engine.applied.add("privacy.activity_history")
    pump(app, 5.0, until=lambda: scanned(app))
    toggle = app.toggles["privacy"]
    assert "Partly on" in toggle.status.cget("text")

    engine.scan = _locked_scan  # type: ignore[method-assign]
    app.start_scan()
    pump(app, 5.0, until=lambda: "Scan failed" in app.status_message.cget("text") and idle(app))
    assert "Partly on" in toggle.status.cget("text"), "nothing changed since the last scan"
    assert toggle.switch.get()
    assert toggle.switch.cget("state") == "normal"
    assert app.errors == []


def test_a_scan_holds_the_sections_actions_until_it_ends(make_app: AppFactory) -> None:
    app, engine = make_app(elevated=True)
    pump(app, 5.0, until=lambda: scanned(app))
    show_section(app, "Maintenance")
    pump(app, 5.0, until=lambda: app.maintenance_panel.loaded and idle(app) and not app._maintenance_loading)
    turn_on = app.maintenance_panel.primary_button
    assert turn_on.cget("state") == "normal"

    release = threading.Event()
    scan = engine.scan

    def held_scan() -> dict[str, Any]:
        release.wait(10)
        return scan()

    engine.scan = held_scan  # type: ignore[method-assign]
    # A read queued before the scan is shown while the scan still holds the window.
    app.load_maintenance()
    app.start_scan()
    try:
        pump(app, 5.0, until=lambda: not app._maintenance_loading)
        assert app._busy
        assert turn_on.cget("state") == "disabled", "a running scan holds the section's actions"
    finally:
        release.set()
    pump(app, 5.0, until=lambda: scanned(app))
    assert turn_on.cget("state") == "normal", "the actions come back when the scan ends"
    assert app.errors == []


def test_cleanup_confirmation_lists_every_location(make_app: AppFactory) -> None:
    app, engine = make_app(elevated=True)
    pump(app, 5.0, until=lambda: scanned(app))
    show_section(app, "Cleanup")
    pump(app, 5.0, until=lambda: app.cleanup_panel.row_count == 4 and idle(app))
    app.cleanup_panel.clean_button.invoke()
    pump(app, 5.0, until=lambda: bool(dialogs(app)))
    dialog = dialogs(app)[-1]
    text = dialog_text(dialog)
    assert "C:\\Users\\Test\\AppData\\Local\\Temp" in text
    assert "C:\\Windows\\Temp" in text
    assert "last 24 hours are kept" in text
    dialog._cancel()
    pump(app, 0.2)
    assert not engine.calls_named("cleanup_run")
    assert app.errors == []


def test_app_closed_inside_a_test_passes_teardown(make_app: AppFactory) -> None:
    app, _ = make_app(elevated=False)
    pump(app, 5.0, until=lambda: scanned(app))
    app._request_close(force=True)
    assert not app._running
    assert app.errors == []
    # The fixture's teardown must not touch the destroyed window.


def test_busy_close_then_tools_check_order(make_app: AppFactory) -> None:
    app, _ = make_app(elevated=True, delay=0.3)
    assert app.engine is not None and app.engine.busy, "the catalog call is still running"
    checks: list[str] = []

    def tools_allow_close() -> bool:
        checks.append("tools")
        return False  # as if the tools dialog were shown instead

    app._tools_allow_close = tools_allow_close  # type: ignore[method-assign]
    app._request_close()
    dialog = next_dialog(app)
    assert dialog.title_text == BUSY_CLOSE_TITLE
    assert checks == [], "the busy dialog comes before the tools check"

    dialog.confirm_button.invoke()
    assert checks == ["tools"], "confirming the busy dialog continues with the tools check"
    assert app._running, "the tools check kept the window open"
    assert app.errors == []


def test_closing_during_a_read_says_it_is_safe(make_app: AppFactory) -> None:
    app, engine = make_app(elevated=True, slow={"catalog": 1.0, "scan": 1.0})
    assert app.engine is not None and app.engine.busy and not app._busy, "only the catalog read runs"
    dialog = close_dialog(app)
    text = dialog_text(dialog)
    assert CLOSE_TEXTS[CLOSE_READ] in text
    assert "recorded" not in text and "background" not in text
    assert dialog.confirm_button.cget("text") == "Close"
    assert dialog.confirm_button.cget("fg_color") != theme.CRITICAL
    cancel_shown(app, dialog)

    # A scan holds the window busy, and it is a read as well; closing then closes.
    pump(app, 5.0, until=lambda: app._busy and bool(engine.calls_named("scan")))
    dialog = close_dialog(app)
    assert CLOSE_TEXTS[CLOSE_READ] in dialog_text(dialog)
    dialog.confirm_button.invoke()
    assert not app._running
    assert app.errors == []


def test_closing_during_a_change_says_it_is_recorded(make_app: AppFactory) -> None:
    app, engine = make_app(elevated=True, slow={"apply": 1.5, "startup_set_enabled": 1.5})
    pump(app, 5.0, until=lambda: scanned(app))
    app._on_item_action(rows_by_id(app)["privacy.cortana"].item, "apply")
    confirm_dialog(app)
    pump(app, 5.0, until=lambda: bool(engine.calls_named("apply")))
    dialog = close_dialog(app)
    text = dialog_text(dialog)
    assert CLOSE_TEXTS[CLOSE_JOURNALED] in text
    assert "finishes in the background" in text and "History lists" in text
    assert dialog.confirm_button.cget("text") == "Close anyway"
    assert dialog.confirm_button.cget("fg_color") == theme.CRITICAL
    cancel_shown(app, dialog)
    pump(app, 5.0, until=lambda: "privacy.cortana" in engine.applied and scanned(app))

    # A startup toggle is a recorded change too, and holds the window busy while it runs.
    show_section(app, "Startup")
    pump(app, 5.0, until=lambda: app.startup_panel.row_count == 3 and idle(app))
    spotify = next(r for r in app.startup_panel._rows if r.entry["name"] == "Spotify")
    spotify.switch.toggle()
    pump(app, 5.0, until=lambda: bool(engine.calls_named("startup_set_enabled")))
    assert app._busy
    assert app.revert_button.cget("state") == "disabled"
    dialog = close_dialog(app)
    assert CLOSE_TEXTS[CLOSE_JOURNALED] in dialog_text(dialog)
    cancel_shown(app, dialog)
    pump(app, 5.0, until=lambda: idle(app) and "disabled at startup" in app.status_message.cget("text"))
    assert app.revert_button.cget("state") == "normal"
    assert app._running
    assert app.errors == []


def test_closing_during_cleanup_says_it_cannot_be_undone(make_app: AppFactory) -> None:
    app, engine = make_app(elevated=True, slow={"cleanup_run": 1.5})
    pump(app, 5.0, until=lambda: scanned(app))
    show_section(app, "Cleanup")
    pump(app, 5.0, until=lambda: app.cleanup_panel.row_count == 4 and idle(app))
    app.cleanup_panel.clean_button.invoke()
    confirm_dialog(app)
    pump(app, 5.0, until=lambda: bool(engine.calls_named("cleanup_run")))
    dialog = close_dialog(app)
    text = dialog_text(dialog)
    assert f"The cleanup is still running. {CLOSE_TEXTS[CLOSE_IRREVERSIBLE]}" in text
    assert "recorded" not in text and "undo it" not in text
    assert dialog.confirm_button.cget("fg_color") == theme.CRITICAL
    cancel_shown(app, dialog)
    pump(app, 5.0, until=lambda: "Freed" in app.status_message.cget("text"))
    assert app._running
    assert app.errors == []


def test_read_close_dialog_asks_again_once_a_change_started(make_app: AppFactory) -> None:
    app, engine = make_app(elevated=True, slow={"apply_category": 1.5})
    pump(app, 5.0, until=lambda: scanned(app))
    app._on_toggle("privacy", True)
    pump(app, 0.2)
    read_dialog = close_dialog(app)
    assert CLOSE_TEXTS[CLOSE_READ] in dialog_text(read_dialog)

    # The dry run ends while the close dialog is open, and its confirmation starts the change.
    pump(app, 5.0, until=lambda: len(dialogs(app)) == 2)
    mode_dialog = next(d for d in dialogs(app) if d is not read_dialog)
    pump(app, 0.3)
    mode_dialog.confirm_button.invoke()
    pump(app, 5.0, until=lambda: len(engine.calls_named("apply_category")) == 2)
    assert app._busy

    read_dialog.confirm_button.invoke()
    assert app._running, "the change that started meanwhile is asked about first"
    again = next_dialog(app)
    assert again is not read_dialog and again.title_text == BUSY_CLOSE_TITLE
    assert CLOSE_TEXTS[CLOSE_JOURNALED] in dialog_text(again)
    assert again.confirm_button.cget("fg_color") == theme.CRITICAL
    cancel_shown(app, again)
    pump(app, 5.0, until=lambda: scanned(app))
    assert app._running
    assert app.errors == []


def test_acknowledgement_gates_the_confirm_button(make_app: AppFactory) -> None:
    app, _ = make_app(elevated=False)
    confirmed: list[bool] = []
    dialog = MessageDialog(
        app,
        title="Reset something?",
        message="This cannot be undone.",
        confirm_text="Reset",
        cancel_text="Cancel",
        danger=True,
        acknowledge="I understand this cannot be undone.",
        on_confirm=lambda: confirmed.append(True),
    )
    pump(app, 0.1)
    assert dialog.title_text == "Reset something?"
    assert dialog.acknowledge_box is not None
    assert dialog.confirm_button.cget("state") == "disabled"
    dialog._confirm()
    assert confirmed == [], "confirming needs the acknowledgement"

    dialog.acknowledge_box.select()
    dialog._acknowledged()
    assert dialog.confirm_button.cget("state") == "normal"
    dialog.acknowledge_box.deselect()
    dialog._acknowledged()
    assert dialog.confirm_button.cget("state") == "disabled"

    dialog.acknowledge_box.toggle()
    assert dialog.confirm_button.cget("state") == "normal", "ticking the box enables the button"
    dialog.confirm_button.invoke()
    assert confirmed == [True]
    assert not dialogs(app)

    plain = MessageDialog(app, title="Done", message="Finished.")
    assert plain.acknowledge_box is None
    assert plain.confirm_button.cget("state") == "normal"
    cancel_shown(app, plain)
    assert app.errors == []


def test_scheduled_task_tweak_apply_lists_its_tasks(make_app: AppFactory) -> None:
    app, engine = make_app(elevated=True, **CEIP_TWEAK)
    pump(app, 5.0, until=lambda: scanned(app))
    item = rows_by_id(app)[CEIP_ID].item
    assert item["state"] == "not_applied"

    app._on_item_action(item, "apply")
    text = confirm_dialog_text(app)
    for path in CEIP_TASKS:
        assert f"scheduled task {path}: enabled; target disabled" in text
    pump(app, 5.0, until=lambda: bool(engine.calls_named("apply")))
    assert engine.calls_named("apply")[-1] == ([CEIP_ID], "try", False)
    pump(app, 5.0, until=lambda: app.status_journal.cget("text") == "Journal: 2 revertible changes")
    row = rows_by_id(app)[CEIP_ID]
    assert row.item["state"] == "applied"
    assert row.action_button is not None and row.action_button.cget("text") == "Undo"
    assert app.errors == []


def test_scheduled_task_changes_are_listed_and_undone_from_history(make_app: AppFactory) -> None:
    app, engine = make_app(elevated=True, **CEIP_TWEAK)
    engine.applied.add(CEIP_ID)
    pump(app, 5.0, until=lambda: scanned(app) and bool(app._titles))
    # One journal record per task path.
    pump(app, 5.0, until=lambda: app.status_journal.cget("text") == "Journal: 2 revertible changes")

    open_history(app, rows=1)
    row = app.history_panel.rows[0]
    assert row.change.title == "Ceip Tasks", "titled with the catalog title of the tweak"
    assert row.change.kind == "scheduled_task"
    assert row.change.details == [f"scheduled task {p}  ·  was enabled" for p in CEIP_TASKS]
    assert row.undo_button.cget("state") == "normal"

    statuses = record_statuses(app)
    row.undo_button.invoke()
    text = confirm_dialog_text(app)
    assert f"scheduled task {CEIP_TASKS[0]}  ·  was enabled" in text
    pump(app, 5.0, until=lambda: bool(engine.calls_named("revert_targets")))
    sent, dry_run = engine.calls_named("revert_targets")[-1]
    assert sent["scheduled_tasks"] == list(CEIP_TASKS)
    assert sent["registry"] == [] and sent["dns"] == []
    assert dry_run is False
    pump(app, 5.0, until=lambda: CEIP_ID not in engine.applied and idle(app))
    assert "Done: 2 changes restored." in statuses

    # History is visible, so it reloads after the change; the journal is empty again.
    pump(app, 5.0, until=lambda: app.history_panel.row_count == 0 and idle(app))
    pump(app, 5.0, until=lambda: app.status_journal.cget("text") == "Journal: no changes")
    assert app.errors == []


def test_scheduled_task_undo_needs_administrator(make_app: AppFactory) -> None:
    app, engine = make_app(elevated=False, **CEIP_TWEAK)
    engine.applied.add(CEIP_ID)
    pump(app, 5.0, until=lambda: scanned(app) and bool(app._titles))

    open_history(app, rows=1)
    row = app.history_panel.rows[0]
    assert row.change.title == "Ceip Tasks"
    assert row.change.needs_admin
    assert row.undo_button.cget("state") == "disabled"
    labels = [str(w.cget("text")) for w in row.winfo_children() if isinstance(w, ctk.CTkLabel)]
    assert any("undoing it needs administrator rights" in text for text in labels)

    # Reaching the undo anyway offers the relaunch, which is cancelled here, never confirmed.
    app._on_history_undo(row.change)
    dialog = next_dialog(app)
    assert dialog.title_text == "Administrator rights needed"
    cancel_shown(app, dialog)
    pump(app, 0.2)
    assert not engine.calls_named("revert_targets")
    assert engine.applied == {CEIP_ID}
    assert app.errors == []


def test_revert_all_names_scheduled_tasks_and_dns(make_app: AppFactory) -> None:
    app, engine = make_app(elevated=True, **CEIP_TWEAK)
    engine.applied.add(CEIP_ID)
    pump(app, 5.0, until=lambda: scanned(app))

    app.revert_button.invoke()
    dialog = next_dialog(app)
    text = dialog_text(dialog)
    assert "Revert all changes?" in text
    assert "scheduled tasks" in text
    assert "DNS servers" in text
    for path in CEIP_TASKS:
        assert f"enable scheduled task {path}" in text
    assert engine.calls_named("revert_all") == [(True,)], "only the dry run so far"

    cancel_shown(app, dialog)
    pump(app, 0.3)
    assert (False,) not in engine.calls_named("revert_all")
    assert engine.applied == {CEIP_ID}
    assert app.errors == []
