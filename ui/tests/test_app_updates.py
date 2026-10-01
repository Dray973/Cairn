"""The Updates section in the real window with a FakeEngine: lazy loading, the update check, the
update and install flows with their dialogs, the job strip, closing while apps are updated, the
install list editor and the Windows Update settings with their undo.

No test confirms the administrator dialog; process helpers are replaced by recorders.
"""

from __future__ import annotations

import time
from typing import Any

import pytest

from optimizer import system
from optimizer.features.updates import (
    AGREEMENTS_NOTE,
    ALL_INSTALLED_TEXT,
    HOME_DEFER_TEXT,
    INSTALL_CONFIRM_MESSAGE,
    LOST_TEXT,
    MAX_BATCH_ITEMS,
    OTHER_USER_TEXT,
    RESTORE_POINT_TIP,
    TOO_MANY_TEXT,
    UNSUPPORTED_TEXT,
    UPDATE_CONFIRM_MESSAGE,
    UPDATES_POLL_HIDDEN_HZ,
    UPDATES_POLL_HZ,
    USER_UNKNOWN_TEXT,
    WINGET_MISSING_TEXT,
)
from optimizer.widgets.updates import (
    APP_INSTALLER_STORE_URI,
    DUPLICATE_TEXT,
    INVALID_ID_TEXT,
    LIGHT_ROWS_ABOVE,
    MAX_NAME_CHARS,
    NAME_MISSING_TEXT,
    RETRY_LEFT_OUT_TEXT,
    STORE_UPDATES_URI,
    VIEWS,
    UpdateRow,
    UpdatesPanel,
    pause_text,
)

from .app_support import (
    ADMIN_DIALOG_TITLE,
    App,
    AppFactory,
    assert_section_fits,
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
from .fake_updates import PAUSE_LIMIT_TEXT, UX_SETTINGS

UNAVAILABLE_TITLE = "Administrator rights not available"
# The engine's message for winget's INSTALLER_PROHIBITS_ELEVATION (0x8A150056), the longest of
# its fixed messages.
ELEVATION_REFUSED_TEXT = (
    "This app can't be updated or installed by an administrator app. Update it from the app itself or "
    "the Microsoft Store."
)
ELEVATION_REFUSED_CODE = 0x8A150056 - (1 << 32)
# The engine's message for UPDATE_INSTALL_TECHNOLOGY_MISMATCH (0x8A15008E), a failure the engine
# marks as one a retry can't change.
MISMATCH_TEXT = (
    "winget can't update this copy because it was installed another way. It may update itself; otherwise "
    "get the update from its publisher."
)
MISMATCH_CODE = 0x8A15008E - (1 << 32)
APP_OPEN_TEXT = "The app is open. Close it and try again."
APP_OPEN_CODE = 0x8A150101 - (1 << 32)
INSTALLER_FAILED_TEXT = "The app's own installer failed. If the app is open, close it and try again."
INSTALLER_FAILED_CODE = 0x8A150049 - (1 << 32)
ALREADY_INSTALLED_CODE = 0x8A150061 - (1 << 32)
SOURCES_UNREACHABLE_TEXT = "winget couldn't reach its sources. Check your internet connection."
LONG_NAME = "Contoso Editor with a product name that runs on well past the width of its column"
# A package id has no spaces, so a long one is broken inside the word.
LONG_ID = "Contoso.EditorWithAPackageIdentifierLongerThanItsColumn.Community"
# The window sizes between which the lists are narrowest and widest.
LIST_SIZES = ((1120, 700), (1000, 600))
ROWS = [
    {
        "id": "Contoso.Editor",
        "name": "Contoso Editor",
        "installed": "1.2.0",
        "available": "1.3.0",
        "source": "winget",
        "explicit_only": False,
        "selectable": True,
        "note": None,
    },
    {
        "id": "Fabrikam.Tool",
        "name": "Fabrikam Tool",
        "installed": "1.0",
        "available": "2.0",
        "source": "winget",
        "explicit_only": True,
        "selectable": True,
        "note": None,
    },
    {
        "id": "9NTAILSPIN0001",
        "name": "Tailspin Notes",
        "installed": "1.0.0.0",
        "available": "1.1.0.0",
        "source": "msstore",
        "explicit_only": False,
        "selectable": True,
        "note": None,
    },
    {
        "id": "Microsoft.AppInstaller",
        "name": "App Installer",
        "installed": "1.28",
        "available": "1.29",
        "source": "winget",
        "explicit_only": False,
        "selectable": False,
        "note": "Updated by the Microsoft Store.",
    },
]
EDITOR = {
    "id": "Contoso.Editor",
    "source": "winget",
    "name": "Contoso Editor",
    "from": "1.2.0",
    "to": "1.3.0",
}
PLAYER = {
    "id": "Fabrikam.Player",
    "source": "winget",
    "name": "Fabrikam Player",
    "from": "2.0.0",
    "to": "2.1.0",
}


def open_updates(app: App) -> UpdatesPanel:
    """Waits for the window's first scan, whose result replaces the status bar text, then shows
    the Updates section as a click on its sidebar row does."""
    pump(app, 5.0, until=lambda: scanned(app))
    show_section(app, "Updates")
    return app.updates_panel


def status(app: App) -> tuple[str, str]:
    return str(app.status_message.cget("text")), str(app.status_message.cget("text_color"))


def job_status(app: App) -> str:
    return str(app.status_tool.cget("text"))


def starts(engine: Any) -> list[tuple[Any, ...]]:
    return engine.calls_named("updates_start")


def running_job(app: App, kind: str = "scan") -> int:
    pump(app, 5.0, until=lambda: app._updates_job is not None and app._updates_job_kind == kind)
    assert app._updates_job is not None
    return app._updates_job


def finish_check(app: App, engine: Any, **options: Any) -> int:
    """Waits for the running check, finishes it in the fake engine and waits for its rows."""
    job = running_job(app)
    before = app._updates_scan
    engine.updates_finish_scan(job, **options)
    pump(app, 5.0, until=lambda: app._updates_job is None and app._updates_scan is not before)
    return job


def switch_view(app: App, view: str) -> None:
    app.updates_panel._switched(VIEWS[view])
    pump(app, 0.1)


def confirm_batch(app: App) -> str:
    """Ticks the acknowledgement of the next dialog, confirms it and returns its text."""
    dialog = next_dialog(app)
    assert dialog.title_text != ADMIN_DIALOG_TITLE
    text = dialog_text(dialog)
    assert dialog.confirm_button.cget("state") == "disabled", "the acknowledgement comes first"
    assert dialog.acknowledge_box is not None
    dialog.acknowledge_box.toggle()
    dialog.confirm_button.invoke()
    return text


def start_update_batch(app: App, engine: Any) -> int:
    """Checks, updates both default rows and returns the running batch's job id."""
    panel = open_updates(app)
    finish_check(app, engine)
    panel.apps.selected_button.invoke()
    confirm_batch(app)
    return running_job(app, "upgrade")


def record_uris(monkeypatch: pytest.MonkeyPatch) -> list[str]:
    opened: list[str] = []
    monkeypatch.setattr(system, "open_uri", opened.append)
    return opened


def wu_rows(app: App) -> dict[str, Any]:
    return app.updates_panel.windows.rows


def open_windows_update(app: App) -> UpdatesPanel:
    panel = open_updates(app)
    switch_view(app, "windows")
    pump(app, 5.0, until=lambda: app._updates_wu is not None and not app._updates_wu_loading and idle(app))
    return panel


# -- loading ---------------------------------------------------------------------------


def test_nothing_is_read_before_the_section_is_shown(make_app: AppFactory) -> None:
    app, engine = make_app(elevated=True)
    pump(app, 5.0, until=lambda: idle(app) and f"HKLM\\{UX_SETTINGS}\\PauseUpdatesExpiryTime" in app._titles)
    pump(app, 0.3)
    names = {name for name, _ in engine.calls if name.startswith("updates_")}
    assert names == {"updates_wu_catalog"}, "only the static History titles are read at start"
    assert app._titles[f"HKLM\\{UX_SETTINGS}\\PauseUpdatesExpiryTime"] == "Windows Update: pause"
    assert app.errors == []


def test_the_first_visit_checks_once(make_app: AppFactory) -> None:
    app, engine = make_app(elevated=True)
    panel = open_updates(app)
    running_job(app)
    assert engine.calls_named("updates_winget_status") == [()]
    assert starts(engine) == [("scan", None, False)]
    assert str(panel.apps.summary.cget("text")).startswith("Checking for app updates…")
    pump(app, 5.0, until=lambda: job_status(app) == "◐ Checking for app updates…")
    assert app._running_job_title(network_only=True) is None, "a check does not hold up the network tools"

    show_section(app, "Dashboard")
    show_section(app, "Updates")
    pump(app, 0.3)
    assert len(starts(engine)) == 1, "the running check is followed, not started again"
    finish_check(app, engine)
    show_section(app, "Dashboard")
    show_section(app, "Updates")
    pump(app, 0.3)
    assert len(starts(engine)) == 1, "a later visit does not check again"
    assert engine.calls_named("updates_winget_status") == [()]
    assert job_status(app) == ""
    assert app.errors == []


def test_a_finished_check_lists_its_rows_with_the_default_selection(
    make_app: AppFactory, monkeypatch: pytest.MonkeyPatch
) -> None:
    opened = record_uris(monkeypatch)
    app, engine = make_app(elevated=True, upgrades=ROWS)
    panel = open_updates(app)
    finish_check(app, engine)
    rows = panel.apps.rows
    assert [r.row["id"] for r in rows] == [r["id"] for r in ROWS]
    assert {r.row["id"] for r in rows if r.selected} == {"Contoso.Editor"}
    assert panel.apps.selected_button.cget("text") == "Update selected (1)"
    assert panel.apps.all_button.cget("text") == "Update all (1)"
    assert str(panel.apps.summary.cget("text")).startswith("4 updates available  ·  checked ")
    assert str(panel.apps.summary.cget("text")).endswith("  ·  winget 1.29.380")
    installer = rows[3]
    assert installer.check.cget("state") == "disabled"
    assert installer.note_label.cget("text") == "Updated by the Microsoft Store."
    assert "picked by name" in rows[1].note_label.cget("text")
    store = rows[2]
    assert store.store_link is not None
    store.store_link.invoke()
    assert opened == [STORE_UPDATES_URI]
    assert not panel.job.winfo_manager(), "the job strip hides after a clean check"

    rows[1].check.toggle()
    assert panel.apps.selected_button.cget("text") == "Update selected (2)"
    assert panel.apps.all_button.cget("text") == "Update all (1)"
    assert app.errors == []


@pytest.mark.parametrize(
    ("availability", "text", "store"),
    [
        ("missing", WINGET_MISSING_TEXT, True),
        ("other_user", OTHER_USER_TEXT, False),
        ("user_unknown", USER_UNKNOWN_TEXT, False),
    ],
)
def test_winget_that_cannot_be_used_is_explained(
    make_app: AppFactory, monkeypatch: pytest.MonkeyPatch, availability: str, text: str, store: bool
) -> None:
    opened = record_uris(monkeypatch)
    app, engine = make_app(elevated=True, winget=availability)
    panel = open_updates(app)
    pump(app, 5.0, until=lambda: app._updates_status is not None and idle(app))
    pump(app, 0.2)
    assert starts(engine) == []
    assert panel.apps.message.winfo_manager()
    assert panel.apps.message_label.cget("text") == text
    assert bool(panel.apps.store_button.winfo_manager()) is store
    assert panel.apps.selected_button.cget("state") == "disabled"
    assert panel.apps.all_button.cget("state") == "disabled"
    if store:
        panel.apps.store_button.invoke()
        assert opened == [APP_INSTALLER_STORE_URI]
        panel.apps.message_check.invoke()
        pump(app, 5.0, until=lambda: len(engine.calls_named("updates_winget_status")) == 2)
    assert app.errors == []


def test_an_outdated_winget_asks_for_a_newer_app_installer(make_app: AppFactory) -> None:
    app, engine = make_app(elevated=True, winget="outdated", winget_version="1.5.0")
    panel = open_updates(app)
    finish_check(app, engine)
    assert panel.apps.message.winfo_manager()
    assert str(panel.apps.message_label.cget("text")).startswith("winget 1.5.0 is too old; Cairn needs 1.6.0")
    assert panel.apps.store_button.winfo_manager()
    assert panel.apps.rows == []
    assert app.errors == []


def test_an_engine_without_the_section_shows_only_the_notice(make_app: AppFactory) -> None:
    app, engine = make_app(elevated=True, unsupported=["updates_winget_status"])
    panel = open_updates(app)
    pump(app, 0.3)
    assert panel.unsupported
    assert panel.unsupported_label.cget("text") == UNSUPPORTED_TEXT
    assert not panel.switch.winfo_manager()
    assert [n for n, _ in engine.calls if n.startswith("updates_") and n != "updates_wu_catalog"] == []
    assert app.errors == []


# -- updating ------------------------------------------------------------------------


def test_a_standard_user_is_asked_for_administrator_rights_and_nothing_is_planned(
    make_app: AppFactory,
) -> None:
    app, engine = make_app(elevated=False)
    panel = open_updates(app)
    finish_check(app, engine)
    assert panel.apps.selected_button.cget("state") == "normal"
    panel.apps.selected_button.invoke()
    dialog = next_dialog(app)
    assert dialog.title_text in (ADMIN_DIALOG_TITLE, UNAVAILABLE_TITLE)
    dialog._cancel()
    pump(app, 0.2)
    assert [c for c in starts(engine) if c[0] != "scan"] == []
    assert app.errors == []


def test_updates_are_planned_confirmed_started_followed_and_checked_again(make_app: AppFactory) -> None:
    app, engine = make_app(elevated=True)
    panel = open_updates(app)
    finish_check(app, engine)
    panel.apps.selected_button.invoke()
    dialog = next_dialog(app)
    assert dialog.title_text == "Update 2 apps?"
    assert dialog.confirm_button.cget("text") == "Update 2 apps"
    assert dialog.confirm_button.cget("fg_color") == theme.CRITICAL
    text = confirm_batch(app)
    for part in (
        UPDATE_CONFIRM_MESSAGE,
        "Contoso Editor  ·  1.2.0 → 1.3.0",
        "Fabrikam Player  ·  2.0.0 → 2.1.0",
        AGREEMENTS_NOTE,
        RESTORE_POINT_TIP,
    ):
        assert part in text
    job = running_job(app, "upgrade")
    assert starts(engine)[1:] == [("upgrade", [EDITOR, PLAYER], True), ("upgrade", [EDITOR, PLAYER], False)]
    assert [r.status_text for r in panel.apps.rows] == ["○ Waiting", "○ Waiting"]
    assert app._running_job_title(network_only=True) == "App updates"
    assert panel.job.stop_button.cget("text") == "Stop after this app"

    engine.updates_item(job, 0, "running")
    pump(app, 5.0, until=lambda: panel.apps.rows[0].status_text == "◐ Updating…")
    pump(app, 5.0, until=lambda: job_status(app) == "◐ Updating apps: 1 of 2  ·  Contoso Editor")
    line = "Updating Contoso Editor (1 of 2)  ·  12.0 MB / 32.5 MB"
    engine.updates_progress(job, 18.5, line, "12.0 MB / 32.5 MB")
    pump(app, 5.0, until=lambda: panel.apps.rows[0].status_text == "◐ Updating…  ·  12.0 MB / 32.5 MB")
    assert panel.job.label.cget("text") == line
    engine.updates_emit(job, "== Contoso Editor (Contoso.Editor): updating 1.2.0 → 1.3.0 (1 of 2) ==")
    engine.updates_item(job, 0, "succeeded", exit_code=0)
    engine.updates_item(
        job, 1, "failed", exit_code=-1978334975, message="The app is open. Close it and try again."
    )
    summaries = len(engine.calls_named("journal_summary"))
    engine.updates_finish(job, "attention", "1 updated · 1 failed")
    pump(app, 5.0, until=lambda: len(starts(engine)) == 4)
    assert starts(engine)[3] == ("scan", None, False), "a check follows every batch"
    assert panel.job.result_text == "✓ 1 updated  ·  ⚠ 1 failed"
    assert status(app) == ("Done: ✓ 1 updated  ·  ⚠ 1 failed", theme.WARNING)
    pump(app, 5.0, until=lambda: len(engine.calls_named("journal_summary")) > summaries)
    assert app._running_job_title(network_only=True) is None

    finish_check(app, engine, upgrades=[ROWS[0] | {"id": "Fabrikam.Player", "name": "Fabrikam Player"}])
    assert panel.job.result_text == "✓ 1 updated  ·  ⚠ 1 failed", "the batch's line stays after the check"
    assert panel.job.label.cget("text") == "Update 2 apps", "with the batch's title"
    assert panel.job.winfo_manager()
    note = panel.apps.rows[0].note_label.cget("text")
    assert note == "Last attempt: The app is open. Close it and try again."

    panel.apps.check_button.invoke()
    pump(app, 5.0, until=lambda: app._updates_job is not None)
    assert panel.job.result_text == "", "the job after the batch's check clears the batch's line"
    finish_check(app, engine)
    assert not panel.job.winfo_manager(), "the job strip hides after a clean check"
    assert app.errors == []


def test_update_all_takes_the_rows_winget_updates_on_its_own(make_app: AppFactory) -> None:
    app, engine = make_app(elevated=True, upgrades=ROWS)
    panel = open_updates(app)
    finish_check(app, engine)
    for row in panel.apps.rows[1:3]:
        row.check.toggle()
    panel.apps.all_button.invoke()
    dialog = next_dialog(app)
    assert dialog.title_text == "Update 1 app?"
    dialog._cancel()
    pump(app, 5.0, until=lambda: idle(app))
    plan = starts(engine)[-1]
    assert plan[0] == "upgrade" and [i["id"] for i in plan[1]] == ["Contoso.Editor"] and plan[2] is True
    assert status(app)[0] == "Nothing was started."
    assert app.errors == []


def finish_batch(app: App, engine: Any, job: int, state: str, summary: str) -> None:
    """Ends a batch in the fake engine and waits for the check that follows it to start."""
    checks = sum(1 for call in starts(engine) if call[0] == "scan")
    engine.updates_finish(job, state, summary)
    pump(app, 5.0, until=lambda: sum(1 for call in starts(engine) if call[0] == "scan") > checks)


def test_update_all_leaves_out_an_app_whose_failure_a_retry_cannot_change(make_app: AppFactory) -> None:
    app, engine = make_app(elevated=True)
    job = start_update_batch(app, engine)
    panel = app.updates_panel
    engine.updates_item(job, 0, "failed", exit_code=MISMATCH_CODE, message=MISMATCH_TEXT, retry=False)
    engine.updates_item(job, 1, "failed", exit_code=APP_OPEN_CODE, message=APP_OPEN_TEXT)
    finish_batch(app, engine, job, "attention", "2 failed")
    finish_check(app, engine)
    editor, player = panel.apps.rows
    assert not editor.selected, "trying again can't change its failure"
    assert player.selected, "an app that was open may update now"
    assert editor.note_label.cget("text") == f"Last attempt: {MISMATCH_TEXT}  ·  {RETRY_LEFT_OUT_TEXT}"
    assert player.note_label.cget("text") == f"Last attempt: {APP_OPEN_TEXT}"
    assert panel.apps.all_button.cget("text") == "Update all (1)"
    assert [i["id"] for i in panel.apps.all_items()] == ["Fabrikam.Player"]

    panel.apps.all_button.invoke()
    dialog = next_dialog(app)
    assert dialog.title_text == "Update 1 app?"
    dialog._cancel()
    pump(app, 5.0, until=lambda: idle(app))
    assert starts(engine)[-1] == ("upgrade", [PLAYER], True)

    editor.check.toggle()
    assert panel.apps.selected_button.cget("text") == "Update selected (2)", "it can still be picked"
    assert panel.apps.all_button.cget("text") == "Update all (1)"
    assert app.errors == []


def test_the_batch_keeps_its_output_and_log_while_its_check_follows(make_app: AppFactory) -> None:
    app, engine = make_app(elevated=True)
    job = start_update_batch(app, engine)
    strip = app.updates_panel.job
    engine.updates_emit(job, "== Contoso Editor (Contoso.Editor): updating 1.2.0 → 1.3.0 (1 of 2) ==")
    engine.updates_emit(job, "Installer failed with exit code: 1603")
    engine.updates_item(job, 0, "failed", exit_code=INSTALLER_FAILED_CODE, message=INSTALLER_FAILED_TEXT)
    engine.updates_item(job, 1, "succeeded", exit_code=0)
    finish_batch(app, engine, job, "attention", "1 updated · 1 failed")
    check = running_job(app)
    assert check != job
    engine.updates_emit(check, "Installed package is not available from any source: Contoso Thing")
    finish_check(app, engine)
    assert strip.result_text == "✓ 1 updated  ·  ⚠ 1 failed"
    assert strip.shows_batch_result

    strip.output_button.invoke()
    pump(app, 5.0, until=lambda: strip.pending_output == 0)
    text = strip.output.get("1.0", "end")
    batch_line = text.index("Installer failed with exit code: 1603")
    separator = text.index("== Check for app updates ==")
    assert batch_line < separator < text.index("Installed package is not available from any source")

    strip.log_button.invoke()
    pump(app, 5.0, until=lambda: bool(engine.calls_named("updates_open_log")))
    assert engine.calls_named("updates_open_log") == [(job,)], "Open log opens the batch's transcript"

    # The job after that check starts without the batch's line, output and log.
    app.updates_panel.apps.check_button.invoke()
    pump(app, 5.0, until=lambda: app._updates_job not in (None, check))
    later = app._updates_job
    assert strip.result_text == "" and not strip.shows_batch_result
    pump(app, 0.2)
    assert strip.output.get("1.0", "end").strip() == ""
    strip.log_button.invoke()
    pump(app, 5.0, until=lambda: len(engine.calls_named("updates_open_log")) == 2)
    assert engine.calls_named("updates_open_log")[-1] == (later,)
    assert app.errors == []


def test_more_apps_than_one_batch_takes_are_refused_before_a_plan(make_app: AppFactory) -> None:
    rows = [
        ROWS[0] | {"id": f"Contoso.App{n}", "name": f"Contoso App {n}"} for n in range(MAX_BATCH_ITEMS + 1)
    ]
    app, engine = make_app(elevated=True, upgrades=rows)
    panel = open_updates(app)
    finish_check(app, engine)
    panel.apps.all_button.invoke()
    assert status(app) == (TOO_MANY_TEXT.format(limit=MAX_BATCH_ITEMS), theme.WARNING)
    assert status(app)[0] == "Pick at most 200 apps at a time."
    pump(app, 0.2)
    assert dialogs(app) == []
    assert [c for c in starts(engine) if c[0] != "scan"] == []
    assert app.errors == []


def test_stop_after_this_app_asks_the_engine(make_app: AppFactory) -> None:
    app, engine = make_app(elevated=True)
    job = start_update_batch(app, engine)
    engine.updates_item(job, 0, "running")
    panel = app.updates_panel
    pump(app, 5.0, until=lambda: panel.apps.rows[0].status_text == "◐ Updating…")
    panel.job.stop_button.invoke()
    assert engine.calls_named("updates_cancel") == [(job,)]
    assert panel.job.label.cget("text") == "Stopping after Contoso Editor…"
    assert panel.job.stop_button.cget("state") == "disabled"
    engine.updates_item(job, 0, "succeeded", exit_code=0)
    engine.updates_item(job, 1, "not_started")
    engine.updates_finish(job, "cancelled")
    pump(app, 5.0, until=lambda: len(starts(engine)) == 4)
    assert panel.job.result_text == "✓ 1 updated  ·  1 not started"
    assert app.errors == []


@pytest.mark.parametrize("ending", ["failed", "stopped"])
def test_a_check_clears_the_line_of_the_check_before_it(make_app: AppFactory, ending: str) -> None:
    app, engine = make_app(elevated=True)
    panel = open_updates(app)
    first = running_job(app)
    if ending == "failed":
        engine.updates_finish_scan(first, error=SOURCES_UNREACHABLE_TEXT)
        line = f"⚠ {SOURCES_UNREACHABLE_TEXT}"
    else:
        panel.job.stop_button.invoke()
        engine.updates_finish(first, "cancelled")
        line = "○ The check for app updates was stopped."
    pump(app, 5.0, until=lambda: app._updates_job is None)
    assert panel.job.result_text == line
    assert panel.job.winfo_manager(), "the strip stays to show why the check ended"

    panel.apps.check_button.invoke()
    pump(app, 5.0, until=lambda: app._updates_job not in (None, first))
    assert panel.job.result_text == "", "the next check starts without the earlier check's line"
    assert not panel.job.result_label.winfo_manager()
    finish_check(app, engine)
    assert str(panel.apps.summary.cget("text")).startswith("2 updates available")
    assert panel.job.result_text == ""
    assert not panel.job.winfo_manager(), "the job strip hides after a clean check"
    assert app.errors == []


def fits(widget: Any) -> bool:
    """Whether `widget` got at least the width its content asks for."""
    return bool(widget.winfo_reqwidth() <= widget.winfo_width())


def wraps_inside(label: Any) -> bool:
    """Whether a label wraps its text and gets the width the wrapped text asks for; without a
    wrap length a label grows with its text."""
    return bool(label.winfo_fpixels(label.cget("wraplength")) > 0) and fits(label)


def right_edge(widget: Any) -> int:
    return int(widget.winfo_rootx() + widget.winfo_width())


@pytest.mark.parametrize("count", [2, LIGHT_ROWS_ABOVE + 1])
def test_a_long_failure_message_wraps_in_its_row(make_app: AppFactory, count: int) -> None:
    upgrades = [ROWS[0] | {"id": f"Contoso.App{n}", "name": f"Contoso App {n}"} for n in range(count)]
    app, engine = make_app(elevated=True, upgrades=upgrades)
    panel = open_updates(app)
    finish_check(app, engine)
    row = panel.apps.rows[0]
    assert row.light is (count > LIGHT_ROWS_ABOVE)
    panel.apps.selected_button.invoke()
    confirm_batch(app)
    job = running_job(app, "upgrade")
    engine.updates_item(job, 0, "failed", exit_code=ELEVATION_REFUSED_CODE, message=ELEVATION_REFUSED_TEXT)
    pump(app, 5.0, until=lambda: row.status_text == f"⚠ Failed: {ELEVATION_REFUSED_TEXT}")
    app.minsize(900, 560)
    for width, height in LIST_SIZES:
        resize_to(app, width, height)
        size = f"at {width}x{height}"
        assert fits(row), f"the row needs {row.winfo_reqwidth()} px of its {row.winfo_width()} {size}"
        names = row.name_label.master
        assert row.name_label.winfo_reqwidth() <= names.winfo_width(), f"the name is squeezed {size}"
        status = row.status_label
        assert wraps_inside(status), f"the status does not wrap in its column {size}"
        assert right_edge(status) <= right_edge(row), f"the status is cut {size}"
        assert right_edge(row.version_label) <= status.winfo_rootx(), f"the status covers the versions {size}"
    assert app.errors == []


@pytest.mark.parametrize("count", [2, LIGHT_ROWS_ABOVE + 1])
def test_a_long_name_and_id_wrap_beside_the_fixed_columns(make_app: AppFactory, count: int) -> None:
    upgrades = [ROWS[0] | {"id": f"{LONG_ID}{n}", "name": f"{LONG_NAME} {n}"} for n in range(count)]
    app, engine = make_app(elevated=True, upgrades=upgrades)
    panel = open_updates(app)
    finish_check(app, engine)
    row = panel.apps.rows[0]
    assert row.light is (count > LIGHT_ROWS_ABOVE)
    scale = ctk.ScalingTracker.get_widget_scaling(row)
    columns = (
        (row.version_label, UpdateRow.VERSION_WIDTH),
        (row.source_label, UpdateRow.SOURCE_WIDTH),
        (row.status_label, UpdateRow.STATUS_WIDTH),
    )
    app.minsize(900, 560)
    for width, height in LIST_SIZES:
        resize_to(app, width, height)
        size = f"at {width}x{height}"
        assert fits(row), f"the row needs {row.winfo_reqwidth()} px of its {row.winfo_width()} {size}"
        names = row.name_label.master
        for label in (row.name_label, row.id_label):
            assert label.winfo_reqwidth() <= names.winfo_width(), f"{label.cget('text')!r} is cut {size}"
        for label, column in columns:
            assert label.winfo_reqwidth() <= round(column * scale), f"a column is wider than {column} {size}"
    assert app.errors == []


def test_a_job_the_engine_forgot_is_reported_lost(make_app: AppFactory) -> None:
    app, engine = make_app(elevated=True)
    open_updates(app)
    job = running_job(app)
    engine.updates_lose(job)
    pump(app, 5.0, until=lambda: app._updates_job is None)
    assert status(app) == (LOST_TEXT, theme.CRITICAL)
    assert app.updates_panel.job.label.cget("text") == LOST_TEXT
    assert job_status(app) == ""
    assert app.errors == []


def job_reads(app: App, engine: Any, seconds: float) -> tuple[int, float]:
    """How often the job was read while the window ran, and how long that really took: on a
    busy machine one `update()` runs several late frames and returns after the deadline."""
    before = len(engine.calls_named("updates_job"))
    started = time.perf_counter()
    pump(app, seconds)
    return len(engine.calls_named("updates_job")) - before, time.perf_counter() - started


def test_a_hidden_section_polls_its_job_slowly(make_app: AppFactory) -> None:
    app, engine = make_app(elevated=True)
    open_updates(app)
    job = running_job(app)
    show_section(app, "Dashboard")
    pump(app, 0.3)
    hidden, seconds = job_reads(app, engine, 1.0)
    assert 1 <= hidden <= UPDATES_POLL_HIDDEN_HZ * seconds + 2, (
        f"{hidden} reads in {seconds:.2f} s while hidden"
    )
    show_section(app, "Updates")
    shown, seconds = job_reads(app, engine, 1.0)
    assert shown > 2 * UPDATES_POLL_HIDDEN_HZ * seconds, f"{shown} reads in {seconds:.2f} s while shown"
    assert shown <= UPDATES_POLL_HZ * seconds + 2, f"{shown} reads in {seconds:.2f} s while shown"
    assert engine.calls_named("updates_job")[-1][0] == job
    assert app.errors == []


def test_output_is_inserted_at_most_200_lines_at_a_time(make_app: AppFactory) -> None:
    app, engine = make_app(elevated=True)
    panel = open_updates(app)
    job = running_job(app)
    strip = panel.job
    strip.append([f"line {n}" for n in range(450)])
    assert strip.flush() == 200
    assert strip.pending_output == 250
    strip.output_button.invoke()
    assert strip.output_shown and strip.output.winfo_manager()
    assert strip.output_button.cget("text") == "Hide output"
    engine.updates_emit(job, "from the engine")
    pump(app, 5.0, until=lambda: strip.pending_output == 0 and app._updates_after == 1)
    text = strip.output.get("1.0", "end").splitlines()
    assert text[0] == "line 0" and text[449] == "line 449" and text[450] == "from the engine"
    assert app.errors == []


# -- closing -----------------------------------------------------------------------


def test_closing_during_a_batch_asks_and_stops_after_the_tools(make_app: AppFactory) -> None:
    app, engine = make_app(elevated=True)
    job = start_update_batch(app, engine)
    engine.updates_item(job, 0, "running", detached=True)
    pump(app, 5.0, until=lambda: "Updating apps: 1 of 2" in job_status(app))
    app._request_close()
    dialog = next_dialog(app)
    assert dialog.title_text == "Apps are still being updated"
    assert (
        "Cairn is updating Contoso Editor (1 of 2). If you close now, Contoso Editor finishes on its own, "
        "the other 1 aren't started, and Contoso Editor's result won't appear in the Activity log."
    ) in dialog_text(dialog)
    assert dialog.confirm_button.cget("text") == "Close anyway"
    dialog._cancel()
    pump(app, 0.2)
    assert app._running
    assert engine.calls_named("updates_shutdown") == []

    app._request_close()
    next_dialog(app).confirm_button.invoke()
    assert not app._running
    names = [name for name, _ in engine.calls]
    assert names.index("updates_shutdown") > names.index("tools_shutdown")
    assert engine.updates_jobs()[0]["state"] == "cancelled"
    assert app.errors == []


def test_closing_while_an_installer_runs_inside_cairn_warns(make_app: AppFactory) -> None:
    app, engine = make_app(elevated=True)
    job = start_update_batch(app, engine)
    engine.updates_item(job, 1, "running", detached=False)
    pump(app, 5.0, until=lambda: "Updating apps: 2 of 2" in job_status(app))
    app._request_close()
    dialog = next_dialog(app)
    text = dialog_text(dialog)
    assert "Closing Cairn now can stop Fabrikam Player's installer midway" in text
    assert dialog.confirm_button.cget("fg_color") == theme.CRITICAL
    dialog._cancel()
    pump(app, 0.2)
    assert app._running
    assert app.errors == []


def test_closing_during_a_check_stops_it_without_asking(make_app: AppFactory) -> None:
    app, engine = make_app(elevated=True)
    open_updates(app)
    running_job(app)
    app._request_close()
    assert not app._running, "a check needs no dialog"
    assert engine.calls_named("updates_shutdown") == [()]
    assert engine.updates_jobs()[0]["state"] == "cancelled"
    assert app.errors == []


def test_network_reset_waits_for_a_batch(make_app: AppFactory) -> None:
    app, engine = make_app(elevated=True)
    start_update_batch(app, engine)
    app._on_network_reset()
    assert status(app) == (
        "Wait for App updates to finish before resetting the network stack.",
        theme.WARNING,
    )
    assert engine.calls_named("network_reset") == []
    assert app.errors == []


# -- installing and the install list ----------------------------------------------------


def open_install(app: App, engine: Any) -> UpdatesPanel:
    panel = open_updates(app)
    switch_view(app, "install")
    finish_check(app, engine)
    pump(app, 5.0, until=lambda: app._updates_apps is not None and len(panel.install.tiles) == 6)
    return panel


def tile(panel: UpdatesPanel, package: str) -> Any:
    return next(t for t in panel.install.tiles if t.app["id"] == package)


def test_installs_skip_the_apps_already_installed(make_app: AppFactory) -> None:
    app, engine = make_app(elevated=True, installed=["Contoso.Editor"])
    panel = open_install(app, engine)
    summary = str(panel.install.summary.cget("text"))
    assert (
        summary == "Pick apps to install. Apps already on this PC are skipped.  ·  1 of 6 already installed"
    )
    editor = tile(panel, "Contoso.Editor")
    assert editor.badge.cget("text") == "✓ Installed"
    assert editor.check.cget("state") == "disabled"
    assert panel.install.install_button.cget("state") == "disabled"
    tile(panel, "Contoso.Browser").check.toggle()
    tile(panel, "Tailspin.Media").check.toggle()
    assert panel.install.install_button.cget("text") == "Install selected (2)"
    panel.install.install_button.invoke()
    dialog = next_dialog(app)
    assert dialog.title_text == "Install 2 apps?"
    text = confirm_batch(app)
    assert INSTALL_CONFIRM_MESSAGE in text
    assert "Contoso Browser  ·  Contoso.Browser" in text
    job = running_job(app, "install")
    items = [
        {"id": "Contoso.Browser", "source": "winget", "name": "Contoso Browser"},
        {"id": "Tailspin.Media", "source": "winget", "name": "Tailspin Media"},
    ]
    assert starts(engine)[-2:] == [("install", items, True), ("install", items, False)]
    assert app._running_job_title() == "App installs"
    engine.updates_item(job, 0, "running")
    engine.updates_progress(job, 22.5, "Installing Contoso Browser (1 of 2)  ·  45%", "45%")
    browser = tile(panel, "Contoso.Browser")
    pump(app, 5.0, until=lambda: browser.badge.cget("text") == "◐ Installing…  ·  45%")
    assert app.errors == []


def test_a_selection_of_installed_apps_starts_nothing(make_app: AppFactory) -> None:
    app, engine = make_app(elevated=True, installed=["Contoso.Editor"])
    panel = open_install(app, engine)
    tile(panel, "Contoso.Editor").var.set(1)
    app._on_updates_install()
    assert status(app)[0] == ALL_INSTALLED_TEXT
    assert [c for c in starts(engine) if c[0] == "install"] == []
    assert dialogs(app) == []
    assert app.errors == []


def test_an_app_its_install_found_installed_stays_marked_installed(make_app: AppFactory) -> None:
    # winget's list of installed apps leaves out Fabrikam Chat, so the check never reports it.
    app, engine = make_app(elevated=True, installed=["Contoso.Editor"])
    panel = open_install(app, engine)
    tile(panel, "Fabrikam.Chat").check.toggle()
    panel.install.install_button.invoke()
    confirm_batch(app)
    job = running_job(app, "install")
    engine.updates_item(
        job, 0, "already_installed", exit_code=ALREADY_INSTALLED_CODE, message="Already installed."
    )
    finish_batch(app, engine, job, "succeeded", "1 already installed")
    finish_check(app, engine)
    chat = tile(panel, "Fabrikam.Chat")
    assert chat.badge.cget("text") == "✓ Installed"
    assert chat.check.cget("state") == "disabled"
    assert str(panel.install.summary.cget("text")).endswith("  ·  2 of 6 already installed")
    assert app._updates_installed_ids() == ["Contoso.Editor", "Fabrikam.Chat"]
    assert app.errors == []


def tiles_fit(panel: UpdatesPanel, size: str) -> None:
    """Both tiles of the two-app list hold their texts and share the list's width."""
    left, right = panel.install.tiles
    for one in (left, right):
        assert fits(one), f"a tile needs {one.winfo_reqwidth()} px of its {one.winfo_width()} {size}"
        for label in (one.name_label, one.id_label, one.badge):
            assert fits(label) and right_edge(label) <= right_edge(one), f"a text is cut {size}"
        assert right_edge(one.name_label) <= one.badge.winfo_rootx(), f"the status covers the name {size}"
        if one.remove.winfo_manager():
            assert right_edge(one.name_label) <= one.remove.winfo_rootx(), f"Remove covers the name {size}"
    assert abs(left.winfo_width() - right.winfo_width()) <= 1, f"the tiles differ in width {size}"
    assert left.winfo_width() + right.winfo_width() <= panel.install.body.winfo_width()


def test_a_long_failure_message_and_a_long_name_wrap_in_their_tiles(make_app: AppFactory) -> None:
    long_name = "Fabrikam Browser with a product name that is as long as the install list takes"
    assert MAX_NAME_CHARS - 5 < len(long_name) <= MAX_NAME_CHARS
    apps = [
        {"id": "Contoso.Browser", "name": "Contoso Browser", "category": "browsers", "source": "winget"},
        {"id": "Fabrikam.Browser", "name": long_name, "category": "browsers", "source": "winget"},
    ]
    app, engine = make_app(elevated=True, installed=[], app_list=apps)
    panel = open_updates(app)
    switch_view(app, "install")
    finish_check(app, engine)
    pump(app, 5.0, until=lambda: len(panel.install.tiles) == 2)
    app.minsize(900, 560)
    resize_to(app, *LIST_SIZES[0])
    tiles_fit(panel, "without a status")
    panel.install.edit_button.invoke()
    pump(app, 0.4)
    assert tile(panel, "Fabrikam.Browser").remove.winfo_manager()
    tiles_fit(panel, "while editing")
    panel.install.edit_button.invoke()

    tile(panel, "Contoso.Browser").check.toggle()
    panel.install.install_button.invoke()
    confirm_batch(app)
    job = running_job(app, "install")
    engine.updates_item(job, 0, "failed", exit_code=ELEVATION_REFUSED_CODE, message=ELEVATION_REFUSED_TEXT)
    failed = tile(panel, "Contoso.Browser")
    pump(app, 5.0, until=lambda: failed.badge.cget("text") == f"⚠ Failed: {ELEVATION_REFUSED_TEXT}")
    for width, height in LIST_SIZES:
        resize_to(app, width, height)
        tiles_fit(panel, f"at {width}x{height}")
    assert app.errors == []


def saves(engine: Any) -> list[tuple[Any, ...]]:
    return engine.calls_named("updates_save_app_list")


def test_the_install_list_is_edited_in_place(make_app: AppFactory) -> None:
    app, engine = make_app(elevated=True)
    panel = open_install(app, engine)
    view = panel.install
    view.edit_button.invoke()
    assert view.editing and view.form.winfo_manager()
    assert view.edit_button.cget("text") == "Done"

    view.id_entry.insert(0, "not an id")
    view.name_entry.insert(0, "Something")
    view.add_button.invoke()
    assert view.error.cget("text") == INVALID_ID_TEXT
    view.id_entry.delete(0, "end")
    view.id_entry.insert(0, "contoso.browser")
    view.add_button.invoke()
    assert view.error.cget("text") == DUPLICATE_TEXT.format(name="Contoso Browser")
    view.id_entry.delete(0, "end")
    view.name_entry.delete(0, "end")
    view.id_entry.insert(0, "Wingtip.Notes")
    view.add_button.invoke()
    assert view.error.cget("text") == NAME_MISSING_TEXT
    pump(app, 0.2)
    assert saves(engine) == []

    view.name_entry.insert(0, "Wingtip Notes")
    view.category_menu.set("Productivity")
    view.add_button.invoke()
    pump(app, 5.0, until=lambda: len(view.tiles) == 7)
    added = saves(engine)[0][0]
    assert len(added) == 7
    assert added[-1] == {
        "id": "Wingtip.Notes",
        "name": "Wingtip Notes",
        "category": "productivity",
        "source": "winget",
    }
    assert app._updates_apps is not None and app._updates_apps["custom"] is True
    assert view.error.cget("text") == ""

    tile(panel, "Litware.Dev").remove.invoke()
    pump(app, 5.0, until=lambda: len(view.tiles) == 6)
    assert [a["id"] for a in saves(engine)[1][0]][-1] == "Wingtip.Notes"
    assert "Litware.Dev" not in [a["id"] for a in saves(engine)[1][0]]

    view.reset_link.invoke()
    pump(app, 5.0, until=lambda: len(saves(engine)) == 3 and not app._updates_apps["custom"])
    assert saves(engine)[2] == (None,)
    assert status(app)[0] == "The default install list is back."
    view.edit_button.invoke()
    assert not view.form.winfo_manager()
    assert app.errors == []


# -- Windows Update ----------------------------------------------------------------


def test_windows_update_is_read_on_every_visit(make_app: AppFactory) -> None:
    app, engine = make_app(elevated=True)
    panel = open_windows_update(app)
    assert len(engine.calls_named("updates_wu_state")) == 1
    assert set(wu_rows(app)) == {
        "pause",
        "active_hours",
        "exclude_drivers",
        "defer_feature",
        "restart_notify",
    }
    assert wu_rows(app)["pause"].state_text == "○ Updates are on"
    labels = [w.cget("text") for w in panel.windows.status_block.winfo_children()]
    assert labels == ["Windows 11 Pro 25H2  ·  build 26200.9457"]
    show_section(app, "Dashboard")
    show_section(app, "Updates")
    pump(app, 5.0, until=lambda: len(engine.calls_named("updates_wu_state")) == 2 and idle(app))
    assert app.errors == []


def test_pausing_is_journaled_titled_in_history_and_undone(make_app: AppFactory) -> None:
    app, engine = make_app(elevated=True)
    open_windows_update(app)
    pause = wu_rows(app)["pause"]
    pause.pause_menu.set("2 weeks")
    pause.pause_button.invoke()
    pump(app, 5.0, until=lambda: bool(engine.calls_named("updates_wu_set")))
    assert engine.calls_named("updates_wu_set") == [("pause", 14, "skip", False)]
    pump(app, 5.0, until=lambda: wu_rows(app)["pause"].state_text.startswith("◐ Paused until") and idle(app))
    assert status(app) == ("✓ Windows Update: pause changed", theme.GOOD)
    assert wu_rows(app)["pause"].undo_link is not None
    assert wu_rows(app)["pause"].pause_button.cget("text") == "Extend"

    show_section(app, "History")
    pump(app, 5.0, until=lambda: app.history_panel.row_count == 1 and idle(app))
    change = app.history_panel.rows[0].change
    assert change.title == "Windows Update: pause"
    assert len(change.filter["registry"]) == 6
    assert change.needs_admin

    open_updates(app)
    pump(app, 5.0, until=lambda: not app._updates_wu_loading and idle(app))
    undo = wu_rows(app)["pause"].undo_link
    assert undo is not None
    undo.invoke()
    pump(app, 5.0, until=lambda: bool(engine.calls_named("revert_targets")))
    targets, dry_run = engine.calls_named("revert_targets")[-1]
    assert dry_run is False
    assert [t["value_name"] for t in targets["registry"]] == [
        "PauseFeatureUpdatesStartTime",
        "PauseQualityUpdatesStartTime",
        "PauseUpdatesStartTime",
        "PauseFeatureUpdatesEndTime",
        "PauseQualityUpdatesEndTime",
        "PauseUpdatesExpiryTime",
    ]
    pump(app, 5.0, until=lambda: wu_rows(app)["pause"].state_text == "○ Updates are on" and idle(app))
    assert wu_rows(app)["pause"].undo_link is None
    assert engine._wu_active_count() == 0
    assert app.errors == []


def test_resume_of_a_pause_set_elsewhere_is_a_change(make_app: AppFactory) -> None:
    until = "2026-10-01T00:00:00Z"
    values = {"PauseUpdatesExpiryTime": until, "PauseQualityUpdatesEndTime": until}
    app, engine = make_app(elevated=True, wu_values=values)
    open_windows_update(app)
    row = wu_rows(app)["pause"]
    assert row.state_text.endswith("set outside Cairn")
    row.resume_button.invoke()
    pump(app, 5.0, until=lambda: bool(engine.calls_named("updates_wu_set")))
    assert engine.calls_named("updates_wu_set") == [("pause", None, "skip", False)]
    assert engine.calls_named("revert_targets") == []
    pump(app, 5.0, until=lambda: wu_rows(app)["pause"].state_text == "○ Updates are on" and idle(app))
    assert app.errors == []


def pause_for(app: App, choice: str) -> None:
    """Pauses updates for the `choice` of the Pause menu and waits for the row drawn after it."""
    row = wu_rows(app)["pause"]
    row.pause_menu.set(choice)
    row.pause_button.invoke()
    pump(app, 5.0, until=lambda: wu_rows(app)["pause"].pause_button.cget("text") == "Extend")
    pump(app, 5.0, until=lambda: not app._updates_wu_loading and idle(app))


def test_extend_adds_the_chosen_time_to_the_running_pause(make_app: AppFactory) -> None:
    app, engine = make_app(elevated=True)
    open_windows_update(app)
    pause_for(app, "1 week")
    row = wu_rows(app)["pause"]
    assert row.pause_menu.get() == "1 week", "the menu shows its first choice again"
    row.pause_button.invoke()
    pump(app, 5.0, until=lambda: len(engine.calls_named("updates_wu_set")) == 2)
    pump(app, 5.0, until=lambda: not app._updates_wu_loading and idle(app))
    assert engine.calls_named("updates_wu_set") == [("pause", 7, "skip", False), ("pause", 7, "skip", False)]
    assert engine._wu_values["PauseUpdatesStartTime"] == "2026-09-25T10:00:00Z", "the pause keeps its start"
    assert engine._wu_values["PauseUpdatesExpiryTime"] == "2026-10-09T10:00:00Z", "14 days after its start"
    assert wu_rows(app)["pause"].state_text == pause_text({"paused": True, "until": "2026-10-09T10:00:00Z"})
    assert app.errors == []


def test_extend_at_the_five_week_limit_is_refused_and_keeps_the_date(make_app: AppFactory) -> None:
    app, engine = make_app(elevated=True)
    open_windows_update(app)
    pause_for(app, "5 weeks")
    shown = wu_rows(app)["pause"].state_text
    assert shown == pause_text({"paused": True, "until": "2026-10-30T10:00:00Z"})
    wu_rows(app)["pause"].pause_button.invoke()
    dialog = next_dialog(app)
    assert dialog.title_text == "Could not change Windows Update"
    assert PAUSE_LIMIT_TEXT in dialog_text(dialog)
    dialog._cancel()
    pump(app, 5.0, until=lambda: not app._updates_wu_loading and idle(app))
    assert engine.calls_named("updates_wu_set") == [("pause", 35, "skip", False), ("pause", 7, "skip", False)]
    assert engine._wu_values["PauseUpdatesExpiryTime"] == "2026-10-30T10:00:00Z"
    assert wu_rows(app)["pause"].state_text == shown, "the pause still ends when it did"
    assert app.errors == []


def test_switching_off_a_setting_that_is_back_at_its_original_is_a_change(make_app: AppFactory) -> None:
    app, engine = make_app(elevated=True, wu_values={"RestartNotificationsAllowed2": 1})
    open_windows_update(app)

    def toggle() -> Any:
        """Flips the switch and returns the row drawn from the state read afterwards."""
        reads = len(engine.calls_named("updates_wu_state"))
        wu_rows(app)["restart_notify"].switch.toggle()
        pump(app, 5.0, until=lambda: len(engine.calls_named("updates_wu_state")) > reads)
        pump(app, 5.0, until=lambda: not app._updates_wu_loading and idle(app))
        return wu_rows(app)["restart_notify"]

    assert wu_rows(app)["restart_notify"].switch.get() == 1
    row = toggle()
    assert row.switch.get() == 0 and row.undo_link is not None
    row = toggle()
    assert row.switch.get() == 1
    assert row.undo_link is None, "the value is what it was before Cairn changed it"
    assert engine._wu_active_count() == 1, "its record is still active"

    row = toggle()
    assert engine.calls_named("revert_targets") == [], "Undo would put back the value that is there"
    assert engine.calls_named("updates_wu_set") == [
        ("restart_notify", False, "skip", False),
        ("restart_notify", True, "skip", False),
        ("restart_notify", False, "skip", False),
    ]
    assert row.switch.get() == 0 and row.state_text == "○ Off"
    assert row.undo_link is not None
    assert engine._wu_values == {}
    assert app.errors == []


def test_switching_off_a_setting_cairn_turned_on_is_its_undo(make_app: AppFactory) -> None:
    app, engine = make_app(elevated=True)
    open_windows_update(app)
    wu_rows(app)["exclude_drivers"].switch.toggle()
    pump(app, 5.0, until=lambda: wu_rows(app)["exclude_drivers"].state_text == "✓ On" and idle(app))
    assert engine._wu_active_count() == 1
    wu_rows(app)["exclude_drivers"].switch.toggle()
    pump(app, 5.0, until=lambda: bool(engine.calls_named("revert_targets")))
    pump(app, 5.0, until=lambda: wu_rows(app)["exclude_drivers"].state_text == "○ Off" and idle(app))
    assert engine.calls_named("updates_wu_set") == [("exclude_drivers", True, "skip", False)]
    assert engine._wu_active_count() == 0, "the record is gone with the change"
    assert app.errors == []


def test_home_cannot_delay_feature_updates(make_app: AppFactory) -> None:
    app, _ = make_app(elevated=True, edition="Core")
    open_windows_update(app)
    row = wu_rows(app)["defer_feature"]
    assert row.defer_menu.cget("state") == "disabled"
    assert row.defer_button.cget("state") == "disabled"
    assert row.reason_label.cget("text") == HOME_DEFER_TEXT
    assert "Windows 11 Home" in str(app.updates_panel.windows.status_block.winfo_children()[0].cget("text"))
    assert app.errors == []


def test_delaying_feature_updates_on_pro_is_confirmed(make_app: AppFactory) -> None:
    app, engine = make_app(elevated=True)
    open_windows_update(app)
    row = wu_rows(app)["defer_feature"]
    row.defer_menu.set("90 days")
    row.defer_button.invoke()
    dialog = next_dialog(app)
    assert dialog.title_text == "Delay feature updates by 90 days?"
    assert "managed by your organization" in dialog_text(dialog)
    dialog._cancel()
    pump(app, 0.2)
    assert engine.calls_named("updates_wu_set") == []
    row = wu_rows(app)["defer_feature"]
    row.defer_menu.set("90 days")
    row.defer_button.invoke()
    next_dialog(app).confirm_button.invoke()
    pump(app, 5.0, until=lambda: bool(engine.calls_named("updates_wu_set")) and idle(app))
    assert engine.calls_named("updates_wu_set") == [("defer_feature", 90, "skip", False)]
    pump(app, 5.0, until=lambda: wu_rows(app)["defer_feature"].state_text == "◐ Delayed by 90 days")
    assert app.errors == []


def test_active_hours_are_checked_before_any_call(make_app: AppFactory) -> None:
    app, engine = make_app(elevated=True)
    open_windows_update(app)
    row = wu_rows(app)["active_hours"]
    row.start_menu.set("08:00")
    row.end_menu.set("03:00")
    row.set_button.invoke()
    assert row.error.cget("text") == "Active hours can span at most 18 hours."
    row.end_menu.set("08:00")
    row.set_button.invoke()
    assert row.error.cget("text") == "Start and end must differ."
    pump(app, 0.2)
    assert engine.calls_named("updates_wu_set") == []
    row.end_menu.set("17:00")
    row.set_button.invoke()
    pump(app, 5.0, until=lambda: bool(engine.calls_named("updates_wu_set")))
    assert engine.calls_named("updates_wu_set") == [("active_hours", [8, 17], "skip", False)]
    pump(app, 5.0, until=lambda: wu_rows(app)["active_hours"].state_text == "✓ 08:00–17:00" and idle(app))
    assert app.errors == []


def test_a_failed_change_is_reported_and_the_switch_goes_back(make_app: AppFactory) -> None:
    app, engine = make_app(elevated=True, wu_fail="restart_notify")
    open_windows_update(app)
    wu_rows(app)["restart_notify"].switch.toggle()
    dialog = next_dialog(app)
    assert dialog.title_text == "Could not change Windows Update"
    dialog._cancel()
    pump(app, 5.0, until=lambda: idle(app) and not app._updates_wu_loading)
    assert wu_rows(app)["restart_notify"].switch.get() == 0
    assert status(app)[1] == theme.CRITICAL
    assert app.errors == []


def test_a_standard_user_cannot_change_windows_update(make_app: AppFactory) -> None:
    app, engine = make_app(elevated=False)
    open_windows_update(app)
    wu_rows(app)["exclude_drivers"].switch.toggle()
    dialog = next_dialog(app)
    assert dialog.title_text in (ADMIN_DIALOG_TITLE, UNAVAILABLE_TITLE)
    dialog._cancel()
    pump(app, 0.2)
    assert engine.calls_named("updates_wu_set") == []
    assert wu_rows(app)["exclude_drivers"].switch.get() == 0, "the switch shows the system again"
    assert app.errors == []


def test_revert_all_reads_windows_update_again(make_app: AppFactory) -> None:
    app, engine = make_app(elevated=True)
    open_windows_update(app)
    wu_rows(app)["restart_notify"].switch.toggle()
    pump(app, 5.0, until=lambda: wu_rows(app)["restart_notify"].state_text == "✓ On" and idle(app))
    reads = len(engine.calls_named("updates_wu_state"))
    app._on_revert_all()
    dialog = next_dialog(app)
    assert dialog.title_text == "Revert all changes?"
    dialog.confirm_button.invoke()
    pump(app, 5.0, until=lambda: bool(engine.calls_named("revert_all")) and idle(app))
    pump(app, 5.0, until=lambda: len(engine.calls_named("updates_wu_state")) > reads and idle(app))
    pump(app, 5.0, until=lambda: wu_rows(app)["restart_notify"].state_text == "○ Off")
    for extra in dialogs(app):
        extra._cancel()
    assert app.errors == []


# -- layout ------------------------------------------------------------------------


def test_app_updates_fit_with_the_job_strip(make_app: AppFactory) -> None:
    app, engine = make_app(elevated=True, upgrades=ROWS)
    open_updates(app)
    finish_check(app, engine)
    app.updates_panel.apps.check_button.invoke()
    running_job(app)
    assert app.updates_panel.job.winfo_manager() and len(app.updates_panel.apps.rows) == 4
    assert_section_fits(app, "Updates")
    assert app.errors == []


def test_install_apps_fit(make_app: AppFactory) -> None:
    app, engine = make_app(elevated=True, winget="missing")
    open_updates(app)
    switch_view(app, "install")
    pump(app, 5.0, until=lambda: app._updates_status is not None and idle(app))
    assert app.updates_panel.install.message.winfo_manager(), "the longest layout: the list and the message"
    assert_section_fits(app, "Updates")
    assert app.errors == []


def test_windows_update_fits(make_app: AppFactory) -> None:
    app, _ = make_app(
        elevated=True, wu_policy={"WUServer": "http://wsus.contoso.test"}, wu_service="disabled"
    )
    open_windows_update(app)
    assert_section_fits(app, "Updates")
    assert app.errors == []
