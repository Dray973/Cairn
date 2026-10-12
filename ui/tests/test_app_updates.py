"""The Updates section in the real window with a FakeEngine: lazy loading, the update check, the
update and install flows with their dialogs, the job strip, closing while apps are updated, the
install list in sections with its Find box and editor, the honest install results, and the
Windows Update settings with their undo.

No test confirms the administrator dialog; process helpers are replaced by recorders.
"""

from __future__ import annotations

import json
import time
from typing import Any

import pytest

from optimizer import system
from optimizer.features.updates import (
    AGREEMENTS_NOTE,
    ALL_INSTALLED_TEXT,
    DEBLOAT_REMOVED_NOTE,
    DEPENDENCIES_NOTE,
    HOME_DEFER_TEXT,
    INSTALL_CONFIRM_MESSAGE,
    LOST_TEXT,
    MAX_BATCH_ITEMS,
    OTHER_USER_TEXT,
    RESTORE_POINT_TIP,
    TOO_MANY_TEXT,
    UNLISTED_TEXT,
    UNSUPPORTED_TEXT,
    UPDATE_CONFIRM_MESSAGE,
    UPDATES_POLL_HIDDEN_HZ,
    UPDATES_POLL_HZ,
    USER_UNKNOWN_TEXT,
    WINGET_MISSING_TEXT,
)
from optimizer.widgets import updates as updates_view
from optimizer.widgets.updates import (
    APP_INSTALLER_STORE_URI,
    CATEGORY_TITLES,
    DUPLICATE_TEXT,
    INVALID_ID_TEXT,
    LIGHT_ROWS_ABOVE,
    MAX_FIND_RESULTS,
    MAX_NAME_CHARS,
    NAME_MISSING_TEXT,
    REPORTED_TEXT,
    RETRY_LEFT_OUT_TEXT,
    STORE_UPDATES_URI,
    VIEWS,
    UpdateRow,
    UpdatesPanel,
    day_text,
    pause_text,
)

from .app_support import (
    ADMIN_DIALOG_TITLE,
    FIT_SIZES,
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
from .fake_updates import DEFAULT_APPS, PAUSE_LIMIT_TEXT, UX_SETTINGS

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


def open_install(app: App, engine: Any, tiles: int = len(DEFAULT_APPS)) -> UpdatesPanel:
    """Shows Install apps, finishes the first check and waits for the list's `tiles` (a list of
    up to 40 apps opens every section, so all its tiles are built)."""
    panel = open_updates(app)
    switch_view(app, "install")
    finish_check(app, engine)
    pump(app, 5.0, until=lambda: app._updates_apps is not None and len(panel.install.tiles) == tiles)
    return panel


def open_long_list(app: App, engine: Any) -> UpdatesPanel:
    """Shows Install apps with a list of more than 40 apps, after the first check."""
    panel = open_updates(app)
    switch_view(app, "install")
    finish_check(app, engine)
    pump(app, 5.0, until=lambda: app._updates_apps is not None and bool(panel.install.sections))
    return panel


def tile(panel: UpdatesPanel, package: str) -> Any:
    return next(t for t in panel.install.tiles if t.app["id"] == package)


def many_apps(count: int = 130) -> list[dict[str, str]]:
    """A list of `count` generic apps over the nine categories, with three that test the Find
    box: two whose names hold the word "zip" and one that holds it only inside a word."""
    apps = [
        {"id": "Contoso.Zip", "name": "Contoso Zip", "category": "utilities", "source": "winget"},
        {"id": "Fabrikam.NanoZip", "name": "NanoZip", "category": "utilities", "source": "winget"},
        {"id": "Tailspin.Unzipped", "name": "Unzipped", "category": "utilities", "source": "winget"},
    ]
    keys = list(CATEGORY_TITLES)
    for n in range(count - len(apps)):
        apps.append(
            {
                "id": f"Contoso.App{n:03}",
                "name": f"Contoso App {n:03}",
                "category": keys[n % len(keys)],
                "source": "winget",
            }
        )
    return apps


def count_tiles(monkeypatch: pytest.MonkeyPatch) -> list[Any]:
    """Records every AppTile the install list builds from now on."""
    built: list[Any] = []

    class CountedTile(updates_view.AppTile):
        def __init__(self, *args: Any, **kwargs: Any) -> None:
            super().__init__(*args, **kwargs)
            built.append(self)

    monkeypatch.setattr(updates_view, "AppTile", CountedTile)
    return built


def gridded(tiles: list[Any]) -> list[str]:
    return [t.app["id"] for t in tiles if t.winfo_manager()]


def shown_tiles(panel: UpdatesPanel) -> list[str]:
    """Ids of the tiles that are gridded inside a section frame that is gridded, in list order."""
    view = panel.install
    open_frames = {key for key, s in view.sections.items() if s.frame is not None and s.frame.winfo_manager()}
    return [t.app["id"] for t in view.tiles if t.app["category"] in open_frames and t.winfo_manager()]


def test_installs_skip_the_apps_already_installed(make_app: AppFactory) -> None:
    app, engine = make_app(elevated=True, installed=["Contoso.Editor"])
    panel = open_install(app, engine)
    summary = str(panel.install.summary.cget("text"))
    assert (
        summary == "Pick apps to install. Apps already on this PC are skipped.  ·  1 of 9 already installed"
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
    assert DEPENDENCIES_NOTE in text, "runtimes winget adds get no rows of their own"
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
    tile(panel, "Contoso.Browser").check.toggle()
    assert panel.install.install_button.cget("text") == "Install selected (1)"
    # The next check finds the ticked app installed, as when it was installed elsewhere.
    panel.install.check_button.invoke()
    finish_check(app, engine, installed=["Contoso.Editor", "Contoso.Browser"])
    browser = tile(panel, "Contoso.Browser")
    assert browser.badge.cget("text") == "✓ Installed"
    assert browser.check.cget("state") == "disabled" and browser.var.get() == 0
    assert panel.install.install_button.cget("text") == "Install selected (0)"
    app._on_updates_install()
    assert status(app)[0] == ALL_INSTALLED_TEXT
    assert [c for c in starts(engine) if c[0] == "install"] == []
    assert dialogs(app) == []
    assert app.errors == []


def test_an_app_its_install_found_installed_stays_marked_installed(make_app: AppFactory) -> None:
    # winget's list of installed apps leaves out Fabrikam Chat, but winget found it by its id
    # (already installed), so the check that does not list it changes nothing.
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
    assert str(panel.install.summary.cget("text")).endswith("  ·  2 of 9 already installed")
    assert app._updates_installed_ids() == ["Contoso.Editor", "Fabrikam.Chat"]
    assert app._updates_last_results["fabrikam.chat"]["state"] == "already_installed"
    assert app.errors == []


def test_an_install_windows_does_not_list_is_never_marked_installed(make_app: AppFactory) -> None:
    # winget reports success, but the check after the batch doesn't find the app, as when an
    # installer hands its work to another process and exits at once.
    app, engine = make_app(elevated=True, installed=["Contoso.Editor"])
    panel = open_install(app, engine)
    tile(panel, "Fabrikam.Chat").check.toggle()
    panel.install.install_button.invoke()
    confirm_batch(app)
    job = running_job(app, "install")
    engine.updates_item(job, 0, "succeeded", exit_code=0)
    chat = tile(panel, "Fabrikam.Chat")
    pump(app, 5.0, until=lambda: chat.badge.cget("text") == f"◐ {REPORTED_TEXT}")
    assert chat.var.get() == 0, "an app winget installed isn't left ticked for a second install"
    finish_batch(app, engine, job, "succeeded", "1 installed")
    assert chat.badge.cget("text") == f"◐ {REPORTED_TEXT}", "winget's report until a check ends"
    assert status(app) == ("Done: ✓ 1 installed", theme.GOOD)
    finish_check(app, engine)
    assert chat.badge.cget("text") == f"◐ {UNLISTED_TEXT}"
    assert chat.check.cget("state") == "normal", "it can be picked again"
    assert chat.var.get() == 0 and panel.install.install_button.cget("text") == "Install selected (0)"
    assert app._updates_last_results["fabrikam.chat"]["state"] == "succeeded_unlisted"
    assert app._updates_installed_ids() == ["Contoso.Editor"]
    assert str(panel.install.summary.cget("text")).endswith("  ·  1 of 9 already installed")
    assert panel.job.result_text == "◐ 1 not listed yet"
    assert panel.job.result_label.cget("text_color") == theme.WARNING
    assert status(app) == ("Done: ◐ 1 not listed yet", theme.WARNING)

    # Check again finds it: from then on it is installed.
    panel.install.check_button.invoke()
    finish_check(app, engine, installed=["Contoso.Editor", "Fabrikam.Chat"])
    assert chat.badge.cget("text") == "✓ Installed"
    assert chat.check.cget("state") == "disabled"
    assert app._updates_last_results["fabrikam.chat"]["state"] == "succeeded"
    assert app._updates_installed_ids() == ["Contoso.Editor", "Fabrikam.Chat"]
    assert status(app) == ("Done: ✓ 1 installed", theme.GOOD)
    assert app.errors == []


def test_an_install_that_needs_a_restart_is_installed_only_once_a_check_lists_it(
    make_app: AppFactory,
) -> None:
    app, engine = make_app(elevated=True, installed=["Contoso.Editor"])
    panel = open_install(app, engine)
    tile(panel, "Fabrikam.Chat").check.toggle()
    panel.install.install_button.invoke()
    confirm_batch(app)
    job = running_job(app, "install")
    engine.updates_item(job, 0, "restart_required", exit_code=3010, message="Restart Windows to finish.")
    finish_batch(app, engine, job, "succeeded", "restart needed for 1")
    finish_check(app, engine)
    chat = tile(panel, "Fabrikam.Chat")
    assert chat.badge.cget("text") == "⚠ Restart Windows to finish", "the check does not list it"
    assert chat.var.get() == 0 and chat.check.cget("state") == "normal"
    assert app._updates_installed_ids() == ["Contoso.Editor"]
    assert str(panel.install.summary.cget("text")).endswith("  ·  1 of 9 already installed")

    panel.install.check_button.invoke()
    finish_check(app, engine, installed=["Contoso.Editor", "Fabrikam.Chat"])
    assert chat.badge.cget("text") == "✓ Installed"
    assert app._updates_installed_ids() == ["Contoso.Editor", "Fabrikam.Chat"]
    assert app.errors == []


def test_a_pending_restart_after_an_install_batch_is_shown(make_app: AppFactory) -> None:
    # The installer exited cleanly, but Windows' pending-restart flags turned on during the batch,
    # which winget's exit code does not carry: the app's tile and the batch's line say so.
    app, engine = make_app(elevated=True, installed=["Contoso.Editor"])
    panel = open_install(app, engine)
    tile(panel, "Fabrikam.Chat").check.toggle()
    panel.install.install_button.invoke()
    confirm_batch(app)
    job = running_job(app, "install")
    engine.updates_item(job, 0, "succeeded", exit_code=0)
    note = "Windows reports a pending restart; restart Windows to finish"
    checks = sum(1 for call in starts(engine) if call[0] == "scan")
    engine.updates_finish(job, "attention", f"1 installed · {note}.", pending_restart=True)
    chat = tile(panel, "Fabrikam.Chat")
    pump(app, 5.0, until=lambda: chat.badge.cget("text") == f"◐ {REPORTED_TEXT}  ·  ⚠ {note}.")
    assert chat.badge.cget("text_color") == theme.WARNING
    pump(app, 5.0, until=lambda: sum(1 for call in starts(engine) if call[0] == "scan") > checks)
    assert panel.job.result_text == f"✓ 1 installed  ·  ⚠ {note}"
    assert panel.job.result_label.cget("text_color") == theme.WARNING
    assert status(app) == (f"Done: ✓ 1 installed  ·  ⚠ {note}", theme.WARNING)

    # The check after the batch lists the app; the batch's line keeps the restart note.
    finish_check(app, engine, installed=["Contoso.Editor", "Fabrikam.Chat"])
    assert chat.badge.cget("text") == "✓ Installed"
    assert panel.job.result_text == f"✓ 1 installed  ·  ⚠ {note}"
    assert panel.job.result_label.cget("text_color") == theme.WARNING
    assert app.errors == []


def test_the_check_after_a_batch_keeps_a_newer_status_message(make_app: AppFactory) -> None:
    app, engine = make_app(elevated=True, installed=["Contoso.Editor"])
    panel = open_install(app, engine)
    tile(panel, "Fabrikam.Chat").check.toggle()
    panel.install.install_button.invoke()
    confirm_batch(app)
    job = running_job(app, "install")
    engine.updates_item(job, 0, "succeeded", exit_code=0)
    finish_batch(app, engine, job, "succeeded", "1 installed")
    app.set_status("The install list was saved.")
    finish_check(app, engine)
    assert panel.job.result_text == "◐ 1 not listed yet"
    assert status(app)[0] == "The install list was saved.", "the status bar keeps the newer message"
    assert app.errors == []


def test_an_app_removed_after_its_install_is_no_longer_marked_installed(make_app: AppFactory) -> None:
    # The check after the batch finds the app; it is then removed in Settings › Apps, so the
    # next check doesn't list it.
    app, engine = make_app(elevated=True, installed=["Contoso.Editor"])
    panel = open_install(app, engine)
    tile(panel, "Fabrikam.Chat").check.toggle()
    panel.install.install_button.invoke()
    confirm_batch(app)
    job = running_job(app, "install")
    engine.updates_item(job, 0, "succeeded", exit_code=0)
    finish_batch(app, engine, job, "succeeded", "1 installed")
    finish_check(app, engine, installed=["Contoso.Editor", "Fabrikam.Chat"])
    chat = tile(panel, "Fabrikam.Chat")
    assert chat.badge.cget("text") == "✓ Installed"
    assert app._updates_unverified == set()

    panel.install.check_button.invoke()
    finish_check(app, engine, installed=["Contoso.Editor"])
    assert chat.badge.cget("text") == "", "neither ✓ Installed nor winget's earlier report"
    assert chat.check.cget("state") == "normal" and chat.var.get() == 0
    assert app._updates_installed_ids() == ["Contoso.Editor"]
    assert str(panel.install.summary.cget("text")).endswith("  ·  1 of 9 already installed")
    assert app.errors == []


def test_a_failed_check_proves_nothing(make_app: AppFactory) -> None:
    app, engine = make_app(elevated=True, installed=["Contoso.Editor"])
    panel = open_install(app, engine)
    tile(panel, "Fabrikam.Chat").check.toggle()
    panel.install.install_button.invoke()
    confirm_batch(app)
    job = running_job(app, "install")
    engine.updates_item(job, 0, "succeeded", exit_code=0)
    finish_batch(app, engine, job, "succeeded", "1 installed")
    finish_check(app, engine, error="winget couldn't reach its sources. Check your internet connection.")
    assert app._updates_last_results["fabrikam.chat"]["state"] == "succeeded"
    chat = tile(panel, "Fabrikam.Chat")
    assert chat.badge.cget("text") == f"◐ {REPORTED_TEXT}", "winget's own report, never ✓ Installed"
    assert chat.check.cget("state") == "normal" and chat.var.get() == 0
    assert app._updates_installed_ids() == ["Contoso.Editor"]
    assert app._updates_unverified == {"fabrikam.chat"}, "the next good check decides"
    assert app.errors == []


def test_a_check_that_could_not_read_the_installed_apps_proves_nothing(make_app: AppFactory) -> None:
    # The check ran, but winget's list of installed apps could not be read.
    app, engine = make_app(elevated=True, installed=["Contoso.Editor"])
    panel = open_install(app, engine)
    tile(panel, "Fabrikam.Chat").check.toggle()
    panel.install.install_button.invoke()
    confirm_batch(app)
    job = running_job(app, "install")
    engine.updates_item(job, 0, "succeeded", exit_code=0)
    finish_batch(app, engine, job, "succeeded", "1 installed")
    finish_check(app, engine, inventory_complete=False)
    chat = tile(panel, "Fabrikam.Chat")
    assert chat.badge.cget("text") == f"◐ {REPORTED_TEXT}", "a list that was not read proves nothing"
    assert app._updates_last_results["fabrikam.chat"]["state"] == "succeeded"
    assert app._updates_unverified == {"fabrikam.chat"}, "the next good check decides"
    assert app.errors == []


def appx_removal(family: str, stamp: str) -> dict[str, Any]:
    """An active Appx entry of the journal export, as a Debloat removal leaves it."""
    name, publisher = family.split("_")
    return {
        "id": 1,
        "session_id": 1,
        "recorded_at": stamp,
        "package_full_name": f"{name}_1.0.0.0_x64__{publisher}",
        "package_family": family,
        "install_location": "C:\\Program Files\\WindowsApps\\Package",
        "all_users": False,
        "active": True,
        "reverted_at": None,
    }


def test_installing_an_app_cairn_removed_names_the_removal(make_app: AppFactory) -> None:
    apps = [
        {"id": "Microsoft.Teams", "name": "Microsoft Teams", "category": "chat", "source": "winget"},
        {"id": "Contoso.Browser", "name": "Contoso Browser", "category": "browsers", "source": "winget"},
    ]
    app, engine = make_app(elevated=True, installed=[], app_list=apps)
    stamp = "2026-09-20T10:00:00+00:00"
    export = engine.journal_export_json

    def with_removal() -> str:
        journal = json.loads(export())
        journal["appx"] = [appx_removal("MSTeams_8wekyb3d8bbwe", stamp)]
        return json.dumps(journal)

    engine.journal_export_json = with_removal  # type: ignore[method-assign]
    panel = open_install(app, engine, tiles=2)
    tile(panel, "Microsoft.Teams").check.toggle()
    tile(panel, "Contoso.Browser").check.toggle()
    panel.install.install_button.invoke()
    dialog = next_dialog(app)
    text = dialog_text(dialog)
    note = DEBLOAT_REMOVED_NOTE.format(date=day_text(stamp))
    assert f"Microsoft Teams  ·  {note}" in text
    assert text.count("removed this app") == 1, "only the app Debloat also lists"
    assert "Contoso Browser  ·  Contoso.Browser" in text
    dialog._cancel()
    pump(app, 5.0, until=lambda: idle(app))
    assert len(engine.calls_named("journal_export_json")) >= 1
    assert app.errors == []


def test_an_app_without_a_removal_record_needs_no_journal_read(make_app: AppFactory) -> None:
    app, engine = make_app(elevated=True, installed=["Contoso.Editor"])
    panel = open_install(app, engine)
    reads = len(engine.calls_named("journal_export_json"))
    tile(panel, "Contoso.Browser").check.toggle()
    panel.install.install_button.invoke()
    text = dialog_text(next_dialog(app))
    assert "removed this app" not in text
    assert len(engine.calls_named("journal_export_json")) == reads
    dialogs(app)[-1]._cancel()
    assert app.errors == []


def test_an_own_list_says_how_many_default_apps_it_lacks(make_app: AppFactory) -> None:
    apps = [
        dict(DEFAULT_APPS[0]),
        {"id": "Wingtip.Notes", "name": "Wingtip Notes", "category": "productivity", "source": "winget"},
    ]
    app, engine = make_app(elevated=True, installed=[], app_list=apps)
    panel = open_install(app, engine, tiles=2)
    summary = str(panel.install.summary.cget("text"))
    assert summary.endswith(
        "The default list has 8 apps your list doesn't. Restore the default list to see them; your own "
        "additions are then removed."
    )
    panel.install.edit_button.invoke()
    panel.install.reset_link.invoke()
    pump(app, 5.0, until=lambda: app._updates_apps is not None and not app._updates_apps["custom"])
    pump(app, 5.0, until=lambda: len(panel.install.tiles) == len(DEFAULT_APPS))
    assert "The default list has" not in str(panel.install.summary.cget("text"))
    assert app.errors == []


# -- the install list in sections -------------------------------------------------------


def test_a_long_list_starts_closed_and_builds_a_section_when_it_opens(
    make_app: AppFactory, monkeypatch: pytest.MonkeyPatch
) -> None:
    built = count_tiles(monkeypatch)
    apps = many_apps()
    app, engine = make_app(elevated=True, installed=["Contoso.App002"], app_list=apps)
    panel = open_long_list(app, engine)
    view = panel.install
    assert built == [] and view.tiles == [], "no tile exists while every section is closed"
    assert list(view.sections) == list(CATEGORY_TITLES)
    gaming = [a for a in apps if a["category"] == "gaming"]
    header = view.sections["gaming"].header
    assert header.cget("text") == f"▸ Gaming  ·  {len(gaming)} apps  ·  1 installed"
    header.invoke()
    assert [t.app["id"] for t in built] == [a["id"] for a in gaming], "opening builds exactly its tiles"
    assert header.cget("text").startswith("▾ Gaming")
    pump(app, 0.2)
    assert gridded(built) == [a["id"] for a in gaming]
    assert [t.cell for t in built] == [(i // 2, i % 2) for i in range(len(gaming))]
    frame = view.sections["gaming"].frame
    assert frame is not None and frame.winfo_manager()

    header.invoke()
    pump(app, 0.2)
    assert not frame.winfo_manager(), "closing hides the tiles"
    assert all(t.winfo_exists() for t in built), "and keeps them"
    header.invoke()
    pump(app, 0.2)
    assert len(built) == len(gaming), "reopening builds nothing"
    assert frame.winfo_manager() and gridded(built) == [a["id"] for a in gaming]

    view.check_button.invoke()
    finish_check(app, engine, installed=["Contoso.App002", "Contoso.App011"])
    assert len(built) == len(gaming), "a check that changes no app builds nothing"
    assert view.sections["gaming"].expanded, "an opened section stays open"
    assert tile(panel, "Contoso.App011").badge.cget("text") == "✓ Installed"
    assert header.cget("text") == f"▾ Gaming  ·  {len(gaming)} apps  ·  2 installed"
    assert app.errors == []


def test_ticks_and_statuses_live_in_the_list_not_in_the_tiles(make_app: AppFactory) -> None:
    apps = many_apps()
    app, engine = make_app(elevated=True, installed=[], app_list=apps)
    panel = open_long_list(app, engine)
    view = panel.install
    view.toggle_section("gaming")
    gaming = view.tiles[0]
    gaming.check.toggle()
    view.toggle_section("gaming")
    view.toggle_section("media")
    media = next(t for t in view.tiles if t.app["category"] == "media")
    media.check.toggle()
    assert view.install_button.cget("text") == "Install selected (2)", "the count spans the sections"
    assert view.sections["gaming"].header.cget("text").endswith("  ·  1 selected")
    view.toggle_section("gaming")
    assert gaming.var.get() == 1, "a closed section keeps its ticks"
    assert [i["id"] for i in view.selected_items()] == [gaming.app["id"], media.app["id"]]

    developer = next(a for a in apps if a["category"] == "developer")
    view.set_items([{"id": developer["id"], "state": "failed", "message": APP_OPEN_TEXT}])
    assert not any(t.app["id"] == developer["id"] for t in view.tiles)
    view.toggle_section("developer")
    assert tile(panel, developer["id"]).badge.cget("text") == f"⚠ Failed: {APP_OPEN_TEXT}"
    assert app.errors == []


def test_edit_list_shows_remove_links_without_building_tiles_again(
    make_app: AppFactory, monkeypatch: pytest.MonkeyPatch
) -> None:
    built = count_tiles(monkeypatch)
    app, engine = make_app(elevated=True, installed=[], app_list=many_apps())
    panel = open_long_list(app, engine)
    view = panel.install
    view.toggle_section("chat")
    count = len(built)
    assert count > 0
    view.edit_button.invoke()
    pump(app, 0.2)
    assert view.form.winfo_manager()
    assert all(t.remove.winfo_manager() for t in view.tiles)
    view.edit_button.invoke()
    pump(app, 0.2)
    assert not view.form.winfo_manager()
    assert not any(t.remove.winfo_manager() for t in view.tiles)
    assert len(built) == count, "Edit list builds no tile"
    assert app.errors == []


def test_removing_an_app_rebuilds_only_its_section(
    make_app: AppFactory, monkeypatch: pytest.MonkeyPatch
) -> None:
    built = count_tiles(monkeypatch)
    apps = many_apps()
    app, engine = make_app(elevated=True, installed=[], app_list=apps)
    panel = open_long_list(app, engine)
    view = panel.install
    view.toggle_section("gaming")
    view.toggle_section("media")
    gaming = [t for t in view.tiles if t.app["category"] == "gaming"]
    media = [t for t in view.tiles if t.app["category"] == "media"]
    view.edit_button.invoke()
    gaming[0].remove.invoke()
    pump(app, 5.0, until=lambda: len(app._updates_apps["apps"]) == len(apps) - 1 and idle(app))
    pump(app, 0.2)
    assert [t for t in view.tiles if t.app["category"] == "media"] == media, "the other sections stay"
    rebuilt = [t for t in view.tiles if t.app["category"] == "gaming"]
    assert len(rebuilt) == len(gaming) - 1
    assert not set(rebuilt) & set(gaming), "the changed section is built again"
    assert len(built) == len(gaming) + len(media) + len(rebuilt)
    assert app.errors == []


def test_the_find_box_shows_only_the_matching_tiles(
    make_app: AppFactory, monkeypatch: pytest.MonkeyPatch
) -> None:
    built = count_tiles(monkeypatch)
    apps = many_apps()
    app, engine = make_app(elevated=True, installed=[], app_list=apps)
    panel = open_long_list(app, engine)
    view = panel.install
    view.toggle_section("media")
    media = len(built)
    view.find.set_query("zip")
    pump(app, 0.2)
    assert shown_tiles(panel) == ["Contoso.Zip", "Fabrikam.NanoZip"]
    assert [t.app["id"] for t in built[media:]] == ["Contoso.Zip", "Fabrikam.NanoZip"], (
        "a search builds only the tiles it shows"
    )
    assert [k for k, s in view.sections.items() if s.header.winfo_manager()] == ["utilities"]
    assert view.find_note.winfo_manager() and view.find_note.cget("text") == "2 apps match."

    view.find.set_query("contoso")
    pump(app, 0.2)
    total = sum(1 for a in apps if a["id"].startswith("Contoso."))
    assert len(shown_tiles(panel)) == MAX_FIND_RESULTS
    assert view.find_note.cget("text") == (
        f"{total} apps match. The first {MAX_FIND_RESULTS} are shown; type more to narrow the search."
    )
    view.find.set_query("nothing like it")
    pump(app, 0.2)
    assert shown_tiles(panel) == []
    assert view.find_note.cget("text") == "No app on the list matches your search."

    view.find.clear()
    pump(app, 0.2)
    assert not view.find_note.winfo_manager()
    assert all(s.header.winfo_manager() for s in view.sections.values())
    assert [k for k, s in view.sections.items() if s.expanded] == ["media"], "the sections are as before"
    assert app.errors == []


def test_an_install_opens_the_sections_of_its_apps(make_app: AppFactory) -> None:
    app, engine = make_app(elevated=True, installed=[], app_list=many_apps())
    panel = open_long_list(app, engine)
    view = panel.install
    view.find.set_query("zip")
    tile(panel, "Contoso.Zip").check.toggle()
    view.find.clear()
    view.toggle_section("gaming")
    next(t for t in view.tiles if t.app["category"] == "gaming").check.toggle()
    view.toggle_section("gaming")
    assert not view.sections["utilities"].expanded and not view.sections["gaming"].expanded
    view.find.set_query("contoso app")
    view.install_button.invoke()
    confirm_batch(app)
    running_job(app, "install")
    pump(app, 0.2)
    assert view.find.query == "", "the search ends"
    assert [k for k, s in view.sections.items() if s.expanded] == ["gaming", "utilities"]
    assert tile(panel, "Contoso.Zip").badge.cget("text") == "○ Waiting"
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
    pump(app, 5.0, until=lambda: len(view.tiles) == len(DEFAULT_APPS) + 1)
    added = saves(engine)[0][0]
    assert len(added) == len(DEFAULT_APPS) + 1
    assert added[-1] == {
        "id": "Wingtip.Notes",
        "name": "Wingtip Notes",
        "category": "productivity",
        "source": "winget",
    }
    assert app._updates_apps is not None and app._updates_apps["custom"] is True
    assert view.error.cget("text") == ""
    assert tile(panel, "Wingtip.Notes").remove.winfo_manager(), "a tile built while editing offers Remove"

    view.category_menu.set("Security and privacy")
    view.id_entry.insert(0, "Contoso.Vault")
    view.name_entry.insert(0, "Contoso Vault")
    view.add_button.invoke()
    pump(app, 5.0, until=lambda: len(view.tiles) == len(DEFAULT_APPS) + 2)
    assert saves(engine)[1][0][-1]["category"] == "security"
    tile(panel, "Contoso.Vault").remove.invoke()
    pump(app, 5.0, until=lambda: len(view.tiles) == len(DEFAULT_APPS) + 1)

    tile(panel, "Litware.Dev").remove.invoke()
    pump(app, 5.0, until=lambda: len(view.tiles) == len(DEFAULT_APPS))
    assert [a["id"] for a in saves(engine)[3][0]][-1] == "Wingtip.Notes"
    assert "Litware.Dev" not in [a["id"] for a in saves(engine)[3][0]]

    view.reset_link.invoke()
    pump(app, 5.0, until=lambda: len(saves(engine)) == 5 and not app._updates_apps["custom"])
    assert saves(engine)[4] == (None,)
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


def test_install_apps_fit_with_a_long_list_and_a_search(make_app: AppFactory) -> None:
    app, engine = make_app(elevated=True, installed=["Contoso.App002"], app_list=many_apps())
    panel = open_long_list(app, engine)
    view = panel.install
    view.find.set_query("zip")
    tile(panel, "Contoso.Zip").check.toggle()
    pump(app, 0.2)
    assert view.find.clear_shown and view.find_note.winfo_manager()
    assert view.sections["utilities"].expanded, "the search opened a section"
    assert view.install_button.cget("text") == "Install selected (1)"
    assert_section_fits(app, "Updates")
    assert app.errors == []


def test_install_apps_fit_with_the_editor_open(make_app: AppFactory) -> None:
    app, engine = make_app(elevated=True)
    panel = open_install(app, engine)
    view = panel.install
    view.edit_button.invoke()
    view.category_menu.set("Security and privacy")
    view.show_error(INVALID_ID_TEXT)
    pump(app, 0.2)
    assert_section_fits(app, "Updates")
    # The menu keeps its width at every size; the narrowest window leaves the form the least room.
    width, height = min(FIT_SIZES)
    resize_to(app, width, height)
    menu = view.category_menu
    assert menu.get() == "Security and privacy"
    assert fits(menu._text_label), f"'Security and privacy' is cut in the category menu at {width}x{height}"
    assert right_edge(view.add_button) <= right_edge(view.form), f"Add sticks out at {width}x{height}"
    assert app.errors == []


def test_windows_update_fits(make_app: AppFactory) -> None:
    app, _ = make_app(
        elevated=True, wu_policy={"WUServer": "http://wsus.contoso.test"}, wu_service="disabled"
    )
    open_windows_update(app)
    assert_section_fits(app, "Updates")
    assert app.errors == []
