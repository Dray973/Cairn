"""Tools section integration tests: the real window with the in-memory FakeEngine.

No tool runs: jobs exist only inside the fake and are driven with `tool_emit` and
`tool_finish`. Administrator dialogs are cancelled, never confirmed.
"""

from __future__ import annotations

import time
from typing import Any

from optimizer.app import status_line
from optimizer.features.tools import ALREADY_RUNNING_TEXT, FINISHING_TEXT, UNSUPPORTED_TEXT
from optimizer.widgets.tools import (
    ADMIN_TOOLS_NOTE,
    FLUSH_LINES,
    MAX_OUTPUT_LINES,
    NON_CANCELLABLE_CAPTION,
    RUNNING_NOW,
    ToolsPanel,
)

from .app_support import (
    App,
    AppFactory,
    FakeEngine,
    MessageDialog,
    confirm_dialog,
    confirm_dialog_text,
    ctk,
    dialog_text,
    dialogs,
    idle,
    next_dialog,
    pump,
    scanned,
    show_section,
    theme,
)

SYSTEM_TOOLS = ["sfc_verify", "sfc_scan", "dism_check", "dism_scan", "dism_restore"]
DRIVE_TOOLS = ["drive_optimize", "drive_retrim", "disk_check"]
C_DRIVE = "C:  Windows  ·  SSD  ·  NTFS  ·  611 GB free of 952 GB"
D_DRIVE = "D:  Data  ·  Hard disk  ·  NTFS  ·  1210 GB free of 1863 GB"
DISM_SOURCE_HINT = (
    "Windows couldn't find the files needed to repair the component store (0x800F081F). Check the "
    "internet connection; on managed PCs a policy can block Windows Update as a repair source."
)


def open_tools(app: App) -> ToolsPanel:
    """Waits for the first scan, shows the Tools section and waits until it is loaded."""
    pump(app, 5.0, until=lambda: scanned(app))
    show_section(app, "Tools")
    pump(app, 5.0, until=lambda: app.tools_panel.loaded and idle(app))
    return app.tools_panel


def run_tool(app: App, tool_id: str, volume: str | None = None) -> tuple[int, str]:
    """Presses the tool's button, confirms, and returns the job id and the dialog's text."""
    panel = app.tools_panel
    if volume is not None:
        assert panel.select_volume(volume)
    panel.rows[tool_id].run_button.invoke()
    text = confirm_dialog_text(app)
    pump(app, 5.0, until=lambda: app._tool_job is not None and idle(app))
    assert app._tool_job is not None
    return app._tool_job, text


def finish(app: App, engine: FakeEngine, job: int, **result: Any) -> None:
    engine.tool_finish(job, **result)
    pump(app, 5.0, until=lambda: app._tool_job is None)


def status(app: App) -> tuple[str, str]:
    return str(app.status_message.cget("text")), str(app.status_message.cget("text_color"))


def status_of(text: str, color: str) -> tuple[str, str]:
    """`text` as the one-line status bar shows it (long texts are cut), with its colour."""
    return status_line(text), color


def row_states(panel: ToolsPanel) -> dict[str, str]:
    return {tool_id: str(row.run_button.cget("state")) for tool_id, row in panel.rows.items()}


def link_button(dialog: MessageDialog, text: str) -> ctk.CTkButton:
    return next(w for w in dialog.winfo_children() if isinstance(w, ctk.CTkButton) and w.cget("text") == text)


def test_tools_tab_lists_tools_volumes_and_windows_tools(make_app: AppFactory) -> None:
    app, engine = make_app(elevated=True)
    panel = open_tools(app)
    reads = ["tools_catalog", "tools_volumes", "system_restore_enabled", "tools_windows"]
    assert [name for name, _ in engine.calls if name in reads] == reads, "read once, in order"

    assert list(panel.rows) == SYSTEM_TOOLS + DRIVE_TOOLS
    assert set(row_states(panel).values()) == {"normal"}
    assert not any(row.blocked_label.winfo_manager() for row in panel.rows.values())
    assert panel.rows["sfc_scan"].run_button.cget("text") == "Repair"
    assert "Repairs can't be undone" in panel.rows["sfc_scan"].meta_label.cget("text")
    assert "Can be stopped" in panel.rows["disk_check"].meta_label.cget("text")

    assert panel.volume_menu.cget("values") == [C_DRIVE, D_DRIVE]
    assert panel.selected_volume() == "C:", "the system drive is selected first"
    assert panel.select_volume("d")
    retrim = panel.rows["drive_retrim"]
    assert retrim.run_button.cget("state") == "disabled"
    assert retrim.blocked_label.cget("text") == "⚠ Retrim is for SSDs; this drive is a hard disk"
    assert retrim.blocked_label.cget("text_color") == theme.WARNING
    assert panel.rows["drive_optimize"].run_button.cget("state") == "normal"
    assert panel.select_volume("C:")
    assert not retrim.blocked_label.winfo_manager()
    assert not panel.select_volume("Q:")

    assert set(panel.windows_buttons) == {"task_manager", "event_viewer", "system_protection"}
    assert {b.cget("state") for b in panel.windows_buttons.values()} == {"normal"}
    assert not panel.windows_note.winfo_manager()
    assert panel.restore_caption.cget("text") == "System Protection: On  ·  takes a few seconds to a minute"
    assert panel.restore_button.cget("state") == "normal"
    assert not panel.protection_button.winfo_manager()
    assert panel.explorer_button.cget("state") == "normal"
    assert panel.log_button.cget("state") == "disabled", "no tool has run yet"

    # Revisiting the section keeps what was read; Refresh reads again.
    show_section(app, "History")
    show_section(app, "Tools")
    pump(app, 0.2)
    assert len(engine.calls_named("tools_catalog")) == 1
    panel.refresh_button.invoke()
    pump(app, 5.0, until=lambda: len(engine.calls_named("tools_windows")) == 2 and idle(app))
    pump(app, 0.1)
    assert list(panel.rows) == SYSTEM_TOOLS + DRIVE_TOOLS
    assert app.errors == []


def test_tool_runs_after_confirmation_and_streams_output(make_app: AppFactory) -> None:
    app, engine = make_app(elevated=True)
    panel = open_tools(app)
    job, text = run_tool(app, "dism_scan")
    assert "Run Scan the component store?" in text
    assert (
        "This only checks; nothing is changed. It runs inside Windows and can't be stopped once it starts."
        in text
    )
    assert "Command: dism.exe /Online /Cleanup-Image /ScanHealth" in text
    assert "Usually takes 5–20 minutes" in text
    assert "• Runs to completion; Cairn can't stop it once it starts." in text
    assert engine.calls_named("tools_start") == [("dism_scan", None, True), ("dism_scan", None, False)]
    assert status(app) == ("Running Scan the component store…", theme.INK_SECONDARY)
    assert app._running_tool_title() == "Scan the component store"
    assert panel.state_label.cget("text") == "◐ Running"
    assert panel.progress_label.cget("text") == "Working…"
    assert not panel.progress_bar.winfo_manager(), "no bar until the tool reports a percentage"
    assert not panel.stop_button.winfo_manager()
    assert panel.caption_label.cget("text") == NON_CANCELLABLE_CAPTION
    assert panel.log_button.cget("state") == "normal"
    assert panel.output_text() == "> dism.exe /Online /Cleanup-Image /ScanHealth"

    engine.tool_emit(job, "Deployment Image Servicing and Management tool", "Version: 10.0.26100.1")
    bar = "[==========                 17.9%                          ]"
    engine.tool_emit(job, progress=17.9, progress_line=bar)
    pump(app, 5.0, until=lambda: app.status_tool.cget("text") == "◐ Scan the component store 18%")
    pump(app, 5.0, until=lambda: "Version: 10.0.26100.1" in panel.output_text())
    assert panel.output_text().splitlines() == [
        "> dism.exe /Online /Cleanup-Image /ScanHealth",
        "Deployment Image Servicing and Management tool",
        "Version: 10.0.26100.1",
    ]
    assert panel.progress_label.cget("text") == "18%"
    assert panel.progress_bar.winfo_manager() == "grid"
    assert abs(panel.progress_bar.get() - 0.179) < 1e-3
    assert "17.9%" in panel.progress_line_label.cget("text")
    assert panel.time_label.cget("text").startswith("Running for ")
    assert {tool_id: row.block for tool_id, row in panel.rows.items()} == {
        tool_id: RUNNING_NOW if tool_id == "dism_scan" else "another tool is running"
        for tool_id in SYSTEM_TOOLS + DRIVE_TOOLS
    }
    assert set(row_states(panel).values()) == {"disabled"}

    finish(app, engine, job, summary="No component store damage was found.")
    text, color = status(app)
    assert text.startswith("Done: Scan the component store finished in ") and color == theme.GOOD
    assert app.status_tool.cget("text") == ""
    assert app._running_tool_title() is None
    assert app._last_tool_job == job
    assert panel.state_label.cget("text") == "✓ Finished"
    assert panel.state_label.cget("text_color") == theme.GOOD
    assert panel.result_label.cget("text") == "✓ No component store damage was found."
    assert panel.time_label.cget("text").endswith("exit code 0 (0x00000000)")
    assert panel.progress_label.cget("text") == ""
    assert not dialogs(app), "a successful check needs no dialog"
    assert set(row_states(panel).values()) == {"normal"}
    assert "Deployment Image Servicing" in panel.output_text(), "the output stays after the job"
    assert app._tools_allow_close(), "nothing runs, so closing needs no question"
    assert app.errors == []


def test_tool_plan_cancel_starts_nothing(make_app: AppFactory) -> None:
    app, engine = make_app(elevated=True)
    panel = open_tools(app)
    panel.rows["sfc_scan"].run_button.invoke()
    dialog = next_dialog(app)
    text = dialog_text(dialog)
    assert "Run Repair system files?" in text
    assert "Windows repairs its own files. The repairs can't be undone from History" in text
    assert dialog.confirm_button.cget("fg_color") == theme.CRITICAL, "repairs are confirmed as dangerous"
    dialog._cancel()
    pump(app, 0.3)
    assert engine.calls_named("tools_start") == [("sfc_scan", None, True)]
    assert app._tool_job is None
    assert status(app)[0] == "Repair system files was not started."

    # A read-only check is not styled as dangerous.
    panel.rows["sfc_verify"].run_button.invoke()
    dialog = next_dialog(app)
    assert dialog.confirm_button.cget("fg_color") == theme.ACCENT
    dialog._cancel()
    pump(app, 0.2)
    assert [c for c in engine.calls_named("tools_start") if c[2] is False] == []
    assert app.errors == []


def test_blocked_tool_explains_and_starts_nothing(make_app: AppFactory) -> None:
    reason = (
        "System File Checker is already running (started outside Cairn, or before it was last "
        "closed). Wait for it to finish."
    )
    app, engine = make_app(elevated=True, tool_blocked={"sfc_scan": reason})
    panel = open_tools(app)
    panel.rows["sfc_scan"].run_button.invoke()
    dialog = next_dialog(app)
    text = dialog_text(dialog)
    assert "Repair system files can't run now" in text
    assert reason in text
    assert dialog.acknowledge_box is None and "Cancel" not in text
    assert status(app) == (f"Repair system files can't run now: {reason}", theme.WARNING)
    dialog.confirm_button.invoke()
    pump(app, 0.2)
    assert engine.calls_named("tools_start") == [("sfc_scan", None, True)]
    assert app._tool_job is None
    assert app.errors == []


def test_only_one_tool_runs_at_a_time(make_app: AppFactory) -> None:
    app, engine = make_app(elevated=True)
    panel = open_tools(app)
    job, _ = run_tool(app, "sfc_verify")
    # The running tool's own row says so neutrally; every other row is blocked by it.
    own = panel.rows["sfc_verify"].blocked_label
    assert (own.cget("text"), own.cget("text_color")) == ("◐ running now", theme.INK_SECONDARY)
    others = {
        (row.blocked_label.cget("text"), row.blocked_label.cget("text_color"))
        for tool_id, row in panel.rows.items()
        if tool_id != "sfc_verify"
    }
    assert others == {("⚠ another tool is running", theme.WARNING)}
    assert set(row_states(panel).values()) == {"disabled"}

    app._on_run_tool("dism_check")
    assert status(app) == (ALREADY_RUNNING_TEXT, theme.WARNING)
    pump(app, 0.2)
    assert not dialogs(app)
    assert [c[0] for c in engine.calls_named("tools_start")] == ["sfc_verify", "sfc_verify"]

    finish(app, engine, job, state="completed")
    assert set(row_states(panel).values()) == {"normal"}
    assert not any(row.blocked_label.winfo_manager() for row in panel.rows.values())

    # A later run marks its own row, and a hard disk's retrim reason is still a warning.
    assert panel.select_volume("D:")
    job, _ = run_tool(app, "drive_optimize")
    assert panel.rows["drive_optimize"].blocked_label.cget("text") == "◐ running now"
    assert panel.rows["sfc_verify"].blocked_label.cget("text") == "⚠ another tool is running"
    finish(app, engine, job)
    retrim = panel.rows["drive_retrim"].blocked_label
    assert (retrim.cget("text"), retrim.cget("text_color")) == (
        "⚠ Retrim is for SSDs; this drive is a hard disk",
        theme.WARNING,
    )
    assert app.errors == []


def clipped_row_texts(panel: ToolsPanel) -> list[str]:
    """Row texts that are cut off: wider than the space they get, past the row's edge, or (the
    description) running under the run button."""
    clipped = []
    for tool_id, row in panel.rows.items():
        labels = {"description": row.description_label, "meta": row.meta_label}
        if row.blocked_label.winfo_manager():
            labels["reason"] = row.blocked_label
        for name, label in labels.items():
            need = label._label.winfo_reqwidth()
            if need > label.winfo_width() or label.winfo_x() + need > row.winfo_width():
                clipped.append(f"{tool_id} {name}")
        description = row.description_label
        if description.winfo_x() + description._label.winfo_reqwidth() > row.run_button.winfo_x():
            clipped.append(f"{tool_id} description under the button")
    return clipped


def labels_under(widget: Any) -> list[ctk.CTkLabel]:
    labels = []
    for child in widget.winfo_children():
        if isinstance(child, ctk.CTkLabel):
            labels.append(child)
        labels += labels_under(child)
    return labels


def clipped_card_texts(panel: ToolsPanel) -> list[str]:
    """Shown texts of both cards that are cut off: wider than the space they get, or past the
    card's edge."""
    clipped = []
    for card in (panel.tools_card, panel.output_card):
        right = card.winfo_rootx() + card.winfo_width()
        for label in labels_under(card):
            if not label.winfo_ismapped():
                continue
            need = label._label.winfo_reqwidth()
            if need > label.winfo_width() or label.winfo_rootx() + need > right:
                clipped.append(str(label.cget("text"))[:50])
    return clipped


def assert_texts_fit(app: App, panel: ToolsPanel, widths: tuple[int, ...]) -> None:
    for width in widths:
        app.geometry(f"{width}x700")
        pump(app, 0.5)
        assert clipped_row_texts(panel) == [], f"at {width} px"
        assert clipped_card_texts(panel) == [], f"at {width} px"
        row = panel.rows["sfc_verify"]
        assert row.winfo_width() > 0 and row.description_label.winfo_height() > 0


def test_row_texts_wrap_to_the_width_the_window_gives(make_app: AppFactory) -> None:
    app, engine = make_app(elevated=True)
    app.minsize(900, 600)
    panel = open_tools(app)
    assert panel.select_volume("D:"), "a drive with a reason to show"
    engine._restore_enabled = False
    panel.refresh_button.invoke()
    pump(app, 5.0, until=lambda: panel.restore_caption.cget("text_color") == theme.WARNING and idle(app))
    # A long command line and the caption of a tool that can't be stopped.
    job, _ = run_tool(app, "dism_restore")
    assert panel.caption_label.cget("text") == NON_CANCELLABLE_CAPTION
    assert_texts_fit(app, panel, (1120, 1000, 1057, 1440))

    finish(
        app,
        engine,
        job,
        state="failed",
        exit_code=-2146498529,
        hint=DISM_SOURCE_HINT,
        summary="DISM ended with error 0x800F081F",
        restart_required=True,
    )
    next_dialog(app).confirm_button.invoke()
    assert panel.result_label.winfo_manager() == "grid"
    assert_texts_fit(app, panel, (1120, 1000, 1057, 1440))
    # The wider window gives the texts longer lines than the narrowest.
    assert panel.rows["sfc_verify"].description_label.cget("wraplength") > 380
    assert panel.intro_label.cget("wraplength") > 470
    assert panel.result_label.cget("wraplength") > 500
    assert app.errors == []


def test_card_texts_wrap_for_a_standard_user(make_app: AppFactory) -> None:
    app, _ = make_app(elevated=False)
    app.minsize(900, 600)
    panel = open_tools(app)
    assert panel.windows_note.winfo_manager() == "grid"
    assert panel.restore_caption.cget("text").endswith("  ·  needs administrator rights")
    assert_texts_fit(app, panel, (1000, 1057, 1120, 960, 1440))
    assert panel.explorer_caption.cget("wraplength") > 330
    assert app.errors == []


def test_stop_is_offered_only_for_disk_check(make_app: AppFactory) -> None:
    app, engine = make_app(elevated=True)
    panel = open_tools(app)
    job, _ = run_tool(app, "sfc_verify")
    assert not panel.stop_button.winfo_manager()
    app._on_stop_tool()
    pump(app, 0.2)
    assert not dialogs(app), "a tool that cannot be stopped offers no stop"
    finish(app, engine, job, state="completed")

    job, text = run_tool(app, "disk_check", "C:")
    assert "This only checks; nothing is changed. You can stop it at any time." in text
    assert "Command: chkdsk.exe C:" in text
    assert "C: is in use, so the check runs read-only" in text
    assert engine.calls_named("tools_start")[-1] == ("disk_check", "C:", False)
    assert panel.stop_button.winfo_manager() == "grid"
    assert panel.stop_button.cget("fg_color") == theme.CRITICAL
    assert panel.caption_label.cget("text") == ""

    panel.stop_button.invoke()
    dialog = next_dialog(app)
    text = dialog_text(dialog)
    assert "Stop Check disk (read-only)?" in text
    assert "Nothing has been changed by the check." in text
    dialog._cancel()
    pump(app, 0.2)
    assert engine.calls_named("tools_cancel") == []

    panel.stop_button.invoke()
    confirm_dialog(app)
    assert engine.calls_named("tools_cancel") == [(job,)]
    pump(app, 5.0, until=lambda: app._tool_job is None)
    assert status(app) == ("Check disk (read-only) stopped.", theme.INK_SECONDARY)
    assert panel.state_label.cget("text") == "○ Stopped"
    assert not panel.stop_button.winfo_manager()
    assert not dialogs(app)
    assert app.errors == []


def test_completed_check_is_neutral(make_app: AppFactory) -> None:
    app, engine = make_app(elevated=True)
    panel = open_tools(app)
    job, _ = run_tool(app, "sfc_verify")
    engine.tool_emit(job, "Beginning system scan.  This process will take some time.")
    finish(
        app,
        engine,
        job,
        state="completed",
        summary="System File Checker finished; its result is in the output above.",
    )
    assert status(app) == (
        "Done: Check system files finished; read its result in the output.",
        theme.INK_SECONDARY,
    )
    assert panel.state_label.cget("text") == "– Finished: see the result"
    assert panel.state_label.cget("text_color") == theme.INK_SECONDARY
    assert panel.result_label.cget("text") == (
        "– System File Checker finished; its result is in the output above."
    )
    assert panel.result_label.cget("text_color") == theme.INK_SECONDARY
    pump(app, 0.2)
    assert not dialogs(app)
    assert "Beginning system scan." in panel.output_text(), "lines read with the final state are shown"
    assert app.errors == []


def test_failed_tool_shows_hint_and_summary(make_app: AppFactory) -> None:
    app, engine = make_app(elevated=True)
    panel = open_tools(app)
    job, text = run_tool(app, "dism_restore")
    assert "• Downloads repair files from Windows Update; needs an internet connection." in text
    finish(
        app,
        engine,
        job,
        state="failed",
        exit_code=-2146498529,
        hint=DISM_SOURCE_HINT,
        summary="DISM ended with error 0x800F081F",
    )
    dialog = next_dialog(app)
    text = dialog_text(dialog)
    assert "Repair the component store failed" in text
    assert DISM_SOURCE_HINT in text and "DISM ended with error 0x800F081F" in text
    link_button(dialog, "Open log").invoke()
    pump(app, 5.0, until=lambda: bool(engine.calls_named("tools_open_log")))
    assert engine.calls_named("tools_open_log") == [(job,)]
    dialog.confirm_button.invoke()

    assert status(app) == status_of(f"Repair the component store failed: {DISM_SOURCE_HINT}", theme.CRITICAL)
    assert panel.state_label.cget("text") == "⚠ Failed"
    assert panel.result_label.cget("text") == f"⚠ {DISM_SOURCE_HINT}\nDISM ended with error 0x800F081F"
    assert panel.result_label.cget("text_color") == theme.CRITICAL
    assert panel.time_label.cget("text").endswith("exit code -2146498529 (0x800F081F)")

    # The log of the last job stays reachable from the panel.
    panel.log_button.invoke()
    pump(app, 5.0, until=lambda: len(engine.calls_named("tools_open_log")) == 2)
    assert app.errors == []


def test_attention_and_restart_ask_with_a_dialog(make_app: AppFactory) -> None:
    app, engine = make_app(elevated=True)
    open_tools(app)
    job, _ = run_tool(app, "disk_check", "C:")
    finish(app, engine, job, state="attention", exit_code=3, hint="Check Disk found problems.")
    dialog = next_dialog(app)
    assert "Check disk (read-only) needs attention" in dialog_text(dialog)
    dialog.confirm_button.invoke()
    assert status(app) == ("Check disk (read-only) reported problems; see its output.", theme.WARNING)

    job, _ = run_tool(app, "dism_restore")
    finish(app, engine, job, exit_code=3010, restart_required=True)
    dialog = next_dialog(app)
    text = dialog_text(dialog)
    assert "Repair the component store finished" in text
    assert "Restart Windows to finish this repair." in text
    dialog.confirm_button.invoke()
    text, color = status(app)
    assert text.endswith("Restart Windows to finish this repair.") and color == theme.GOOD
    assert app.errors == []


def test_tool_start_error_is_reported(make_app: AppFactory) -> None:
    error = (
        "Cairn is running inside a Windows job that would stop this repair if Cairn closed "
        "or crashed. Start Cairn from the Start menu or a desktop shortcut and try again."
    )
    app, engine = make_app(elevated=True, tool_start_error=error)
    panel = open_tools(app)
    panel.rows["sfc_scan"].run_button.invoke()
    confirm_dialog(app)
    dialog = next_dialog(app)
    text = dialog_text(dialog)
    assert "Could not start Repair system files" in text
    assert error in text
    dialog.confirm_button.invoke()
    assert status(app) == status_of(f"Could not start Repair system files: {error}", theme.CRITICAL)
    assert app._tool_job is None
    assert app.status_tool.cget("text") == ""
    assert set(row_states(panel).values()) == {"normal"}
    assert engine.calls_named("tools_start")[-1] == ("sfc_scan", None, False)
    assert app.errors == []


def test_tool_output_is_buffered_while_tab_hidden(make_app: AppFactory) -> None:
    app, engine = make_app(elevated=True)
    panel = open_tools(app)
    job, _ = run_tool(app, "sfc_verify")
    show_section(app, "Dashboard")
    polls = len(engine.calls_named("tools_job"))
    engine.tool_emit(job, "first line", "second line", "third line", progress=30.0)
    pump(app, 5.0, until=lambda: app.status_tool.cget("text") == "◐ Check system files 30%")
    pump(app, 5.0, until=lambda: panel.pending_output == 3)
    assert "first line" not in panel.output_text(), "a hidden section only buffers"

    # About two reads a second while hidden.
    pump(app, 1.0)
    hidden_polls = len(engine.calls_named("tools_job")) - polls
    assert hidden_polls <= 6, f"{hidden_polls} reads while hidden"

    show_section(app, "Tools")
    pump(app, 5.0, until=lambda: panel.pending_output == 0)
    assert panel.output_text().splitlines()[1:] == ["first line", "second line", "third line"]
    before = len(engine.calls_named("tools_job"))
    pump(app, 1.0)
    visible_polls = len(engine.calls_named("tools_job")) - before
    assert visible_polls >= 5, f"only {visible_polls} reads while visible"
    assert app.errors == []


def test_other_tabs_work_while_a_tool_runs(make_app: AppFactory) -> None:
    app, engine = make_app(elevated=True)
    open_tools(app)
    run_tool(app, "sfc_verify")
    show_section(app, "Optimize")
    app.toggles["privacy"].switch.toggle()
    confirm_dialog(app)
    pump(app, 5.0, until=lambda: "✓ On" in app.toggles["privacy"].status.cget("text") and idle(app))
    assert engine.calls_named("apply_category")[-1] == ("privacy", "try", False)
    assert app._tool_job is not None, "the tool keeps running"
    assert app.status_tool.cget("text") == "◐ Check system files"

    show_section(app, "Cleanup")
    pump(app, 5.0, until=lambda: app.cleanup_panel.row_count == 4 and idle(app))
    assert app._running_tool_title() == "Check system files"
    assert app.errors == []


def test_closing_with_a_running_tool_asks_first(make_app: AppFactory) -> None:
    app, engine = make_app(elevated=True)
    open_tools(app)
    job, _ = run_tool(app, "sfc_verify")
    app._request_close()
    dialog = next_dialog(app)
    text = dialog_text(dialog)
    assert "Check system files is still running" in text
    assert "keeps running after Cairn closes" in text
    assert "C:\\Test\\tools\\1.raw" in text
    assert dialog.confirm_button.cget("text") == "Close anyway"
    dialog._cancel()
    pump(app, 0.2)
    assert app._running
    assert engine.calls_named("tools_shutdown") == []
    finish(app, engine, job, state="completed")

    run_tool(app, "disk_check", "C:")
    app._request_close()
    dialog = next_dialog(app)
    text = dialog_text(dialog)
    assert "Check disk (read-only) is still running" in text
    assert "Closing stops it. Nothing has been changed by the check." in text
    assert dialog.confirm_button.cget("text") == "Stop and close"
    assert dialog.confirm_button.cget("fg_color") == theme.CRITICAL
    dialog.confirm_button.invoke()
    assert not app._running
    assert engine.calls_named("tools_shutdown") == [()]
    assert engine.tools_jobs()[0]["state"] == "cancelled"
    assert app.errors == []


def test_closing_while_busy_asks_busy_then_tools(make_app: AppFactory) -> None:
    app, engine = make_app(elevated=True, delay=0.2)
    open_tools(app)
    run_tool(app, "sfc_verify")
    app.start_scan()
    assert app._busy
    app._request_close()
    dialog = next_dialog(app)
    assert dialog.title_text == "An operation is still running"
    dialog.confirm_button.invoke()
    dialog = next_dialog(app)
    assert dialog.title_text == "Check system files is still running", "the tools dialog comes second"
    dialog._cancel()
    pump(app, 0.3)
    assert app._running
    assert engine.calls_named("tools_shutdown") == []
    pump(app, 5.0, until=lambda: idle(app))
    assert app.errors == []


def test_a_change_started_under_the_tools_dialog_is_asked_about(make_app: AppFactory) -> None:
    app, engine = make_app(elevated=True, slow={"apply_category": 1.5})
    open_tools(app)
    run_tool(app, "sfc_verify")
    app._request_close()
    tools_dialog = next_dialog(app)
    assert tools_dialog.title_text == "Check system files is still running"

    # A recorded change starts while the tools dialog is open.
    app._set_busy(True, "journaled", action="Privacy Mode")
    pump(app, 0.3)
    tools_dialog.confirm_button.invoke()
    busy_dialog = next_dialog(app)
    assert busy_dialog.title_text == "An operation is still running"
    assert "Privacy Mode is still running." in dialog_text(busy_dialog)
    busy_dialog._cancel()
    pump(app, 0.3)
    assert app._running, "the change was asked about before closing"
    assert engine.calls_named("tools_shutdown") == []
    app._set_busy(False)
    assert app.errors == []


def test_close_texts_follow_detached_state(make_app: AppFactory) -> None:
    app, _ = make_app(elevated=True, tool_detached=False)
    open_tools(app)
    run_tool(app, "sfc_verify")
    app._request_close()
    dialog = next_dialog(app)
    text = dialog_text(dialog)
    assert "Closing Cairn now can end it before it finishes." in text
    assert "Keep Cairn open until it finishes." in text
    assert "raw output" not in text
    assert dialog.confirm_button.cget("text") == "Close anyway"
    assert dialog.confirm_button.cget("fg_color") == theme.CRITICAL
    dialog._cancel()
    pump(app, 0.2)
    assert app._running
    assert app.errors == []


def test_closing_while_a_finished_tool_is_judged_waits_for_it(make_app: AppFactory) -> None:
    app, engine = make_app(elevated=True)
    open_tools(app)
    job, _ = run_tool(app, "dism_check")
    # DISM has exited; the engine is reading the component store state.
    engine.tool_exit(job, 0)
    app._request_close()
    dialog = next_dialog(app)
    text = dialog_text(dialog)
    assert dialog.title_text == "Check the component store is finishing"
    assert FINISHING_TEXT in text
    assert "keeps running" not in text and "raw output" not in text
    assert dialog.confirm_button.cget("text") == "Close"
    assert dialog.confirm_button.cget("fg_color") == theme.ACCENT
    dialog._cancel()
    pump(app, 0.2)
    assert app._running
    assert engine.calls_named("tools_shutdown") == []
    assert app.state() == "normal"

    # The engine waits for the result on the Tk thread, so the window is hidden meanwhile.
    states: list[str] = []
    shutdown = engine.tools_shutdown

    def watched() -> list[dict[str, Any]]:
        states.append(str(app.state()))
        return shutdown()

    engine.tools_shutdown = watched  # type: ignore[method-assign]
    app._request_close()
    next_dialog(app).confirm_button.invoke()
    assert not app._running
    assert states == ["withdrawn"]
    assert engine.calls_named("tools_shutdown") == [()]
    assert engine.tools_jobs()[0]["state"] == "completed", "closing waited for its result"
    assert app.errors == []


def test_restore_point_button(make_app: AppFactory) -> None:
    app, engine = make_app(elevated=True)
    panel = open_tools(app)
    panel.restore_button.invoke()
    pump(app, 5.0, until=lambda: "Restore point" in status(app)[0] and idle(app))
    assert status(app) == ("Restore point #7 created.", theme.GOOD)
    assert engine.calls_named("tools_restore_point") == [("Cairn manual checkpoint",)]
    assert not dialogs(app), "creating a checkpoint needs no confirmation"

    engine._restore_point_error = "The System Restore service is busy."
    panel.restore_button.invoke()
    dialog = next_dialog(app)
    text = dialog_text(dialog)
    assert "The restore point was not created" in text
    assert "The System Restore service is busy." in text
    assert status(app) == (
        "The restore point was not created: The System Restore service is busy.",
        theme.CRITICAL,
    )
    link_button(dialog, "Open System Protection").invoke()
    pump(app, 5.0, until=lambda: bool(engine.calls_named("tools_open_windows")))
    assert engine.calls_named("tools_open_windows") == [("system_protection",)]
    dialog.confirm_button.invoke()

    # With System Protection off the button is disabled and the settings are one click away.
    engine._restore_enabled = False
    panel.refresh_button.invoke()
    pump(app, 5.0, until=lambda: panel.restore_button.cget("state") == "disabled" and idle(app))
    pump(app, 0.1)
    assert panel.restore_caption.cget("text").startswith("⚠ System Protection is off")
    assert panel.restore_caption.cget("text_color") == theme.WARNING
    assert panel.protection_button.winfo_manager() == "grid"
    panel.protection_button.invoke()
    pump(app, 5.0, until=lambda: len(engine.calls_named("tools_open_windows")) == 2)

    # An unreadable System Protection state is shown as unknown and the button stays usable.
    engine._restore_enabled = None
    panel.refresh_button.invoke()
    pump(app, 5.0, until=lambda: panel.restore_caption.cget("text") == "System Protection: unknown")
    pump(app, 5.0, until=lambda: idle(app))
    assert panel.restore_button.cget("state") == "normal"
    assert not panel.protection_button.winfo_manager()
    assert app.errors == []


def test_standard_user_sees_tools_disabled(make_app: AppFactory) -> None:
    app, engine = make_app(elevated=False)
    panel = open_tools(app)
    assert set(row_states(panel).values()) == {"disabled"}
    assert all(row.block == "needs administrator rights" for row in panel.rows.values())
    assert panel.restore_button.cget("state") == "disabled"
    assert panel.restore_caption.cget("text").endswith("needs administrator rights")
    assert panel.windows_buttons["task_manager"].cget("state") == "normal"
    assert panel.windows_buttons["event_viewer"].cget("state") == "disabled"
    assert panel.windows_buttons["system_protection"].cget("state") == "disabled"
    assert panel.windows_note.winfo_manager() == "grid"
    assert panel.windows_note.cget("text") == ADMIN_TOOLS_NOTE
    assert panel.explorer_button.cget("state") == "normal"

    app._on_run_tool("sfc_verify")
    dialog = next_dialog(app)
    assert dialog.title_text == "Administrator rights needed"
    dialog._cancel()
    pump(app, 0.2)
    assert engine.calls_named("tools_start") == []

    app._on_open_windows_tool("event_viewer")
    assert status(app) == (
        "Event Viewer needs administrator rights; restart as administrator to open it.",
        theme.WARNING,
    )
    panel.windows_buttons["task_manager"].invoke()
    pump(app, 5.0, until=lambda: bool(engine.calls_named("tools_open_windows")))
    assert engine.calls_named("tools_open_windows") == [("task_manager",)]
    assert app.errors == []


def test_restart_explorer_from_tools_asks_first(make_app: AppFactory) -> None:
    app, engine = make_app(elevated=True)
    panel = open_tools(app)
    panel.explorer_button.invoke()
    dialog = next_dialog(app)
    text = dialog_text(dialog)
    assert "Restart File Explorer?" in text
    assert "Open File Explorer windows close and the taskbar disappears for a few seconds." in text
    dialog._cancel()
    pump(app, 0.2)
    assert engine.calls_named("restart_explorer") == []

    panel.explorer_button.invoke()
    confirm_dialog(app)
    pump(app, 5.0, until=lambda: status(app)[0] == "File Explorer restarted.")
    assert engine.calls_named("restart_explorer") == [()]
    assert app.errors == []


def test_windows_tool_buttons_open_through_engine(make_app: AppFactory) -> None:
    app, engine = make_app(elevated=True)
    panel = open_tools(app)
    panel.windows_buttons["event_viewer"].invoke()
    pump(app, 5.0, until=lambda: status(app)[0] == "Opened Event Viewer.")
    assert engine.calls_named("tools_open_windows") == [("event_viewer",)]

    def refuse(tool_id: str) -> None:
        engine._record("tools_open_windows", tool_id)
        raise RuntimeError("could not start taskmgr.exe: the file was not found")

    engine.tools_open_windows = refuse  # type: ignore[method-assign]
    panel.windows_buttons["task_manager"].invoke()
    dialog = next_dialog(app)
    assert "Could not open Task Manager" in dialog_text(dialog)
    dialog.confirm_button.invoke()
    assert status(app)[1] == theme.CRITICAL
    assert app.errors == []


def test_lost_job_stops_polling(make_app: AppFactory) -> None:
    app, engine = make_app(elevated=True)
    panel = open_tools(app)
    run_tool(app, "sfc_verify")
    reads: list[int] = []

    def forgotten(job_id: int, after: int = 0) -> None:
        reads.append(job_id)
        return None

    engine.tools_job = forgotten  # type: ignore[method-assign]
    lost = "Lost track of Check system files; it may still be running. Its log is in the tools folder."
    pump(app, 5.0, until=lambda: status(app)[0] == lost)
    assert status(app)[1] == theme.CRITICAL
    assert app._tool_job is None
    assert app.status_tool.cget("text") == ""
    assert panel.result_label.cget("text") == f"⚠ {lost}"
    count = len(reads)
    pump(app, 0.5)
    assert len(reads) == count == 1, "polling stopped after the first unanswered read"
    assert set(row_states(panel).values()) == {"normal"}
    assert app.errors == []


def test_poll_exception_is_reported_once(make_app: AppFactory) -> None:
    app, _ = make_app(elevated=True)
    panel = open_tools(app)
    run_tool(app, "sfc_verify")

    def broken(view: dict[str, Any], *, visible: bool) -> None:
        raise ValueError("the output box is gone")

    panel.update_job = broken  # type: ignore[method-assign]
    pump(app, 5.0, until=lambda: app._tool_poll_failed)
    pump(app, 0.5)
    assert len(app.errors) == 1
    assert "the output box is gone" in app.errors[0]
    assert status(app) == (
        "Lost track of Check system files; it may still be running. Its log is in the tools folder.",
        theme.CRITICAL,
    )
    assert app._tool_job is None
    assert app.status_tool.cget("text") == ""
    app.errors.clear()  # the one expected entry
    assert app.fps > 0, "the frame loop kept running"
    assert app.errors == []


def test_output_flush_is_bounded_per_frame(make_app: AppFactory) -> None:
    app, engine = make_app(elevated=True)
    panel = open_tools(app)
    job, _ = run_tool(app, "sfc_verify")
    assert panel.output_text() == "> sfc.exe /verifyonly"
    flushes: list[tuple[int, float]] = []
    flush = panel.flush

    def timed(limit: int = FLUSH_LINES) -> int:
        started = time.perf_counter()
        inserted = flush(limit)
        flushes.append((inserted, time.perf_counter() - started))
        return inserted

    panel.flush = timed  # type: ignore[method-assign]

    def burst(first: int) -> list[tuple[int, float]]:
        """Emits 3000 lines while the section is hidden, shows it and returns the flushes
        that inserted them."""
        show_section(app, "Dashboard")
        shown = panel.output_text()
        engine.tool_emit(job, *[f"line {i:04d}" for i in range(first, first + 1500)])
        pump(app, 5.0, until=lambda: panel.pending_output == 1500)
        engine.tool_emit(job, *[f"line {i:04d}" for i in range(first + 1500, first + 3000)])
        pump(app, 5.0, until=lambda: panel.pending_output == 3000)
        assert panel.output_text() == shown, "a hidden section only buffers"
        flushes.clear()
        show_section(app, "Tools")
        pump(app, 10.0, until=lambda: panel.pending_output == 0)
        return [entry for entry in flushes if entry[0]]

    # A flush is a fixed amount of work, so a slow one among quick ones was interrupted by
    # the scheduler; the burst is measured again before its time counts.
    emitted = 0
    slowest = 0.0
    for _ in range(3):
        used = burst(emitted)
        emitted += 3000
        assert sum(n for n, _ in used) == 3000
        assert max(n for n, _ in used) <= FLUSH_LINES
        slowest = max(seconds for _, seconds in used)
        if slowest < 0.010:
            break
    assert slowest < 0.010, f"a flush took {slowest * 1000:.1f} ms"
    lines = panel.output_text().splitlines()
    assert len(lines) == MAX_OUTPUT_LINES + 1, "the box keeps 2000 lines below the command line"
    assert lines[0] == "> sfc.exe /verifyonly"
    assert lines[1] == f"line {emitted - MAX_OUTPUT_LINES:04d}"
    assert lines[-1] == f"line {emitted - 1:04d}"
    assert app.errors == []


def test_output_follows_the_end_only_while_scrolled_to_it(make_app: AppFactory) -> None:
    app, engine = make_app(elevated=True)
    panel = open_tools(app)
    job, _ = run_tool(app, "sfc_verify")
    engine.tool_emit(job, *[f"line {i}" for i in range(300)])
    pump(app, 5.0, until=lambda: panel.output_text().endswith("line 299"))
    pump(app, 0.2)
    assert panel.output.yview()[1] >= 0.999, "the view follows new output"

    # Scrolled up to read, the view stays where it is while output keeps arriving.
    panel.output.yview_moveto(0.0)
    pump(app, 0.1)
    engine.tool_emit(job, *[f"more {i}" for i in range(100)])
    pump(app, 5.0, until=lambda: panel.output_text().endswith("more 99"))
    pump(app, 0.2)
    top, bottom = panel.output.yview()
    assert top == 0.0 and bottom < 0.999

    # Back at the end, it follows again.
    panel.output.yview_moveto(1.0)
    pump(app, 0.1)
    engine.tool_emit(job, *[f"last {i}" for i in range(100)])
    pump(app, 5.0, until=lambda: panel.output_text().endswith("last 99"))
    pump(app, 0.2)
    assert panel.output.yview()[1] >= 0.999
    assert app.errors == []


def test_outdated_engine_disables_tools_tab(make_app: AppFactory) -> None:
    app, engine = make_app(elevated=True, unsupported=["tools_catalog"])
    pump(app, 5.0, until=lambda: scanned(app))
    show_section(app, "Tools")
    pump(app, 0.3)
    panel = app.tools_panel
    assert panel.summary.cget("text") == f"⚠ {UNSUPPORTED_TEXT}"
    assert panel.summary.cget("text_color") == theme.WARNING
    assert panel.refresh_button.cget("state") == "disabled"
    assert panel.restore_button.cget("state") == "disabled"
    assert panel.explorer_button.cget("state") == "disabled"
    assert not panel.loaded
    assert not engine.calls_named("tools_volumes")
    app.load_tools()
    pump(app, 0.2)
    assert not engine.calls_named("tools_volumes")
    assert app._tools_allow_close()
    assert app.errors == []
