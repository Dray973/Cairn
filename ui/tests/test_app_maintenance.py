"""Maintenance section integration tests: the real window with the in-memory FakeEngine.

No task is registered and no run happens: the schedule is a dict inside the fake, and runs
are driven with `maintenance_begin_run`, `maintenance_step` and `maintenance_end_run`, as the
task's own process would write them. Administrator dialogs are cancelled, never confirmed.
"""

from __future__ import annotations

import threading
from datetime import UTC, datetime, timedelta
from typing import Any

import pytest

from optimizer.app import BUSY_CLOSE_TITLE, CLOSE_IRREVERSIBLE, CLOSE_JOURNALED, CLOSE_OTHER, CLOSE_TEXTS
from optimizer.features.maintenance import (
    CANT_TURN_ON_TITLE,
    NOT_TURNED_OFF_TEXT,
    REMOVE_TASK_MESSAGE,
    REMOVE_TASK_TITLE,
    RUN_NOW_TITLE,
    RUNNING_ELSEWHERE_TEXT,
    STARTUP_NOTICE_SECONDS,
    TURN_OFF_TITLE,
    TURN_ON_TITLE,
    UNSUPPORTED_TEXT,
)
from optimizer.widgets.maintenance import (
    ON_BATTERY_TEXT,
    TURN_ON_FIRST_TEXT,
    UNRECORDED_TEXT,
    MaintenancePanel,
)

from . import fake_maintenance
from .app_support import (
    ADMIN_DIALOG_TITLE,
    App,
    AppFactory,
    FakeEngine,
    MessageDialog,
    assert_section_fits,
    confirm_dialog,
    confirm_dialog_text,
    dialog_text,
    dialogs,
    idle,
    next_dialog,
    pump,
    show_section,
    theme,
)
from .fake_maintenance import DEFAULTS, TASK_PATH, run_dict

SECTION = "Maintenance"
REPAIR_HINT = "Windows found damaged system files. Open Tools and run Repair system files."


def recently(days: float = 1.0) -> str:
    return (datetime.now(UTC) - timedelta(days=days)).isoformat(timespec="seconds").replace("+00:00", "Z")


def open_maintenance(app: App) -> MaintenancePanel:
    """Shows the section and waits until its status is loaded."""
    pump(app, 5.0, until=lambda: idle(app))
    show_section(app, SECTION)
    pump(app, 5.0, until=lambda: app.maintenance_panel.loaded and idle(app) and not app._maintenance_loading)
    return app.maintenance_panel


def cancel_shown(app: App, dialog: MessageDialog) -> None:
    """Cancels `dialog` once its window has finished appearing (a dialog destroyed before its
    deferred window setup ran can crash Tk)."""
    pump(app, 0.3)
    dialog._cancel()


def status(app: App) -> tuple[str, str]:
    return str(app.status_message.cget("text")), str(app.status_message.cget("text_color"))


def job_status(app: App) -> str:
    return str(app.status_tool.cget("text"))


def poll(app: App) -> None:
    """Lets the maintenance lane read the run monitor now."""
    app._maintenance_polled = 0.0
    pump(app, 0.2)


def badge(app: App) -> str:
    return app.nav.items[SECTION].badge


def reloads(engine: FakeEngine) -> int:
    return len(engine.calls_named("maintenance_status"))


def test_the_status_loads_only_when_shown_and_again_on_every_visit(make_app: AppFactory) -> None:
    app, engine = make_app(elevated=True)
    pump(app, 1.0, until=lambda: idle(app))
    assert reloads(engine) == 0
    assert engine.calls_named("maintenance_watch") == [(False,)]
    open_maintenance(app)
    assert reloads(engine) == 1
    show_section(app, "Dashboard")
    open_maintenance(app)
    assert reloads(engine) == 2
    assert app.errors == []


def test_reads_asked_for_while_one_runs_are_coalesced(make_app: AppFactory) -> None:
    app, engine = make_app(elevated=True, slow={"maintenance_status": 0.3})
    open_maintenance(app)
    before = reloads(engine)
    app.load_maintenance()
    app.load_maintenance()
    app.load_maintenance()
    pump(app, 5.0, until=lambda: reloads(engine) == before + 2 and not app._maintenance_loading and idle(app))
    pump(app, 0.5)
    assert reloads(engine) == before + 2
    assert app.errors == []


def test_a_standard_user_is_offered_elevation_and_nothing_changes(make_app: AppFactory) -> None:
    app, engine = make_app(elevated=False)
    panel = open_maintenance(app)
    assert panel.hint_label.winfo_manager()
    assert "administrator rights" in str(panel.hint_label.cget("text"))
    panel.primary_button.invoke()
    dialog = next_dialog(app)
    assert dialog.title_text == ADMIN_DIALOG_TITLE
    cancel_shown(app, dialog)
    pump(app, 0.3)
    assert engine.calls_named("maintenance_set_schedule") == []
    assert app.errors == []


def test_turning_on_plans_confirms_saves_and_refreshes(make_app: AppFactory) -> None:
    app, engine = make_app(elevated=True, slow={"maintenance_set_schedule": 0.3})
    panel = open_maintenance(app)
    assert str(panel.primary_button.cget("text")) == "Turn on"
    assert str(panel.state_label.cget("text")) == "○ Off"
    assert not panel.turn_off_button.winfo_manager()
    summaries = len(engine.calls_named("journal_summary"))
    panel.primary_button.invoke()
    dialog = next_dialog(app)
    assert dialog.title_text == TURN_ON_TITLE
    text = dialog_text(dialog)
    assert "every Sunday at 12:00 PM as your account with administrator rights" in text
    assert "Each run permanently deletes the files in the selected locations." in text
    for title in ("Temporary files", "Windows temp folder", "Error reports"):
        assert f"      {title}" in text
    confirm_dialog(app)
    # The confirmation starts the change before the event loop runs again.
    assert app._busy
    assert app._busy_close.action == "Turning on scheduled maintenance"
    assert app._busy_close.kind == "journaled"
    show_section(app, "History")
    exports = len(engine.calls_named("journal_export_json"))
    pump(app, 5.0, until=lambda: idle(app) and len(engine.calls_named("journal_export_json")) > exports)
    config = engine.calls_named("maintenance_set_schedule")[0][0]
    assert engine.calls_named("maintenance_set_schedule") == [(config, True), (config, False)]
    assert config == DEFAULTS
    assert status(app) == ("✓ Scheduled maintenance is on: every Sunday at 12:00 PM.", theme.GOOD)
    assert len(engine.calls_named("journal_summary")) > summaries
    assert "Scheduled maintenance" in {g.title for g in app.history_panel.groups}
    panel = open_maintenance(app)
    assert str(panel.state_label.cget("text")) == "✓ On · every Sunday at 12:00 PM"
    assert str(panel.primary_button.cget("text")) == "Save changes"
    assert panel.turn_off_button.winfo_manager()
    assert "Next run: Sun 4 Oct, 12:00 PM" in str(panel.next_label.cget("text"))
    assert app.errors == []


def test_the_controls_decide_what_is_saved(make_app: AppFactory) -> None:
    app, engine = make_app(elevated=True)
    panel = open_maintenance(app)
    panel.day_menu.set("Friday")
    panel.hour_menu.set("6 PM")
    panel.minute_menu.set(":30")
    panel.target_vars["user_temp"].set(False)
    panel.target_vars["crash_dumps"].set(True)
    panel.dism_var.set(False)
    panel.primary_button.invoke()
    text = confirm_dialog_text(app)
    assert "every Friday at 6:30 PM" in text
    assert "      Crash dumps" in text and "      Temporary files" not in text
    pump(app, 5.0, until=lambda: idle(app) and len(engine.calls_named("maintenance_set_schedule")) == 2)
    assert engine.calls_named("maintenance_set_schedule")[1][0] == {
        "day": "friday",
        "time": "18:30",
        "targets": ["windows_temp", "update_cache", "delivery_optimization", "crash_dumps", "error_reports"],
        "system_file_check": True,
        "component_store_check": False,
    }
    pump(app, 5.0, until=lambda: not app._maintenance_loading and idle(app))
    # The saved schedule is what the controls keep showing.
    assert panel.day_menu.get() == "Friday" and panel.hour_menu.get() == "6 PM"
    assert app.errors == []


def test_a_blocked_schedule_disables_turn_on_and_says_why(make_app: AppFactory) -> None:
    reason = "Scheduled maintenance needs Cairn installed where only administrators can change its files."
    app, engine = make_app(elevated=True, maintenance_blocked=reason)
    panel = open_maintenance(app)
    assert str(panel.primary_button.cget("state")) == "disabled"
    assert panel.warnings_label.winfo_manager()
    assert reason in str(panel.warnings_label.cget("text"))
    # A refusal the plan finds is explained without asking to confirm anything.
    engine.maintenance_blocked = None
    open_maintenance(app)
    assert str(panel.primary_button.cget("state")) == "normal"
    engine.maintenance_blocked = reason
    panel.primary_button.invoke()
    dialog = next_dialog(app)
    assert dialog.title_text == CANT_TURN_ON_TITLE
    assert reason in dialog_text(dialog)
    assert status(app) == (f"⚠ {reason}", theme.WARNING)
    cancel_shown(app, dialog)
    pump(app, 0.3)
    assert engine.calls_named("maintenance_set_schedule") == [(DEFAULTS, True)]
    assert app.errors == []


def test_an_unchanged_schedule_only_says_so(make_app: AppFactory) -> None:
    app, engine = make_app(elevated=True, maintenance_task=DEFAULTS)
    panel = open_maintenance(app)
    panel.primary_button.invoke()
    pump(app, 5.0, until=lambda: idle(app) and bool(engine.calls_named("maintenance_set_schedule")))
    pump(app, 0.3)
    assert dialogs(app) == []
    assert status(app)[0] == "The schedule is already set this way."
    assert engine.calls_named("maintenance_set_schedule") == [(DEFAULTS, True)]
    assert app.errors == []


def test_turning_off_undoes_the_record_and_its_history_group(make_app: AppFactory) -> None:
    app, engine = make_app(elevated=True, maintenance_task=DEFAULTS)
    pump(app, 5.0, until=lambda: idle(app))
    show_section(app, "History")
    pump(app, 5.0, until=lambda: idle(app) and bool(app.history_panel.groups))
    assert "Scheduled maintenance" in {g.title for g in app.history_panel.groups}
    panel = open_maintenance(app)
    panel.turn_off_button.invoke()
    dialog = next_dialog(app)
    assert dialog.title_text == TURN_OFF_TITLE
    confirm_dialog(app)
    pump(app, 5.0, until=lambda: idle(app) and bool(engine.calls_named("revert_targets")))
    assert engine.calls_named("revert_targets") == [({"task_definitions": [TASK_PATH]}, False)]
    pump(app, 5.0, until=lambda: not app._maintenance_loading and idle(app))
    assert status(app) == (
        "✓ Scheduled maintenance is off; its task was removed from Task Scheduler.",
        theme.GOOD,
    )
    assert str(panel.state_label.cget("text")) == "○ Off"
    show_section(app, "History")
    pump(app, 5.0, until=lambda: idle(app))
    pump(app, 0.3)
    assert "Scheduled maintenance" not in {g.title for g in app.history_panel.groups}
    assert app.errors == []


def test_a_failed_turn_off_lists_the_failure_and_keeps_the_schedule(make_app: AppFactory) -> None:
    app, engine = make_app(elevated=True, maintenance_task=DEFAULTS)
    panel = open_maintenance(app)
    requested: list[tuple[dict[str, Any], bool]] = []

    def refused(filter: dict[str, Any], dry_run: bool = False) -> dict[str, Any]:
        requested.append((filter, dry_run))
        return {
            "failures": [{"target": f"task {TASK_PATH}", "error": "Task Scheduler refused (access denied)"}]
        }

    engine.revert_targets = refused  # type: ignore[method-assign]
    panel.turn_off_button.invoke()
    confirm_dialog(app)
    pump(app, 5.0, until=lambda: bool(requested) and bool(dialogs(app)))
    dialog = next_dialog(app)
    assert dialog.title_text == "Scheduled maintenance could not be turned off"
    assert "Task Scheduler refused (access denied)" in dialog_text(dialog)
    assert requested == [({"task_definitions": [TASK_PATH]}, False)]
    assert status(app) == (NOT_TURNED_OFF_TEXT, theme.WARNING)
    cancel_shown(app, dialog)
    pump(app, 5.0, until=lambda: not app._maintenance_loading and idle(app))
    assert engine.maintenance_task == DEFAULTS
    assert str(panel.state_label.cget("text")) == "✓ On · every Sunday at 12:00 PM"
    assert app.errors == []


def test_run_now_follows_the_run_and_announces_its_result(make_app: AppFactory) -> None:
    app, engine = make_app(elevated=True, maintenance_task=DEFAULTS)
    panel = open_maintenance(app)
    assert str(panel.run_now_button.cget("state")) == "normal"
    panel.run_now_button.invoke()
    dialog = next_dialog(app)
    assert dialog.title_text == RUN_NOW_TITLE
    assert str(dialog.confirm_button.cget("fg_color")) == theme.CRITICAL
    confirm_dialog(app)
    pump(app, 5.0, until=lambda: idle(app) and bool(engine.calls_named("maintenance_run_now")))
    assert engine.calls_named("maintenance_watch")[-1] == (True,)
    assert status(app)[0].startswith("◐ Maintenance started in the background")
    assert str(panel.run_state_label.cget("text")) == "◐ Starting…"
    poll(app)
    assert job_status(app) == "◐ Maintenance: Starting…"

    run_id = engine.maintenance_begin_run()
    engine.maintenance_step("system_files", "Checking system files… 45%", 2, 3, 45.0)
    poll(app)
    assert job_status(app) == "◐ Maintenance: Checking system files… 45%"
    assert app._maintenance_running
    assert str(panel.state_label.cget("text")) == "◐ Running…"
    assert panel.progress_bar.winfo_manager()
    assert abs(panel.progress_bar.get() - 0.45) < 0.01
    assert str(panel.run_now_button.cget("state")) == "disabled"

    show_section(app, "Dashboard")
    engine.maintenance_end_run("attention", headline="Freed 1.2 GB", attention=(REPAIR_HINT,))
    poll(app)
    assert job_status(app) == ""
    assert not app._maintenance_running
    text, color = status(app)
    assert text.startswith("⚠ Maintenance ran ") and text.endswith(
        f"found something to look at: {REPAIR_HINT}"
    )
    assert color == theme.WARNING
    assert badge(app) == "⚠"

    panel = open_maintenance(app)
    pump(app, 5.0, until=lambda: bool(engine.calls_named("maintenance_acknowledge")) and idle(app))
    assert engine.calls_named("maintenance_acknowledge") == [(run_id,)]
    assert badge(app) == ""
    assert str(panel.run_state_label.cget("text")).startswith("⚠ Finished ")
    assert panel.open_tools_button.winfo_manager()
    assert REPAIR_HINT in str(panel.hints_label.cget("text"))
    panel.open_tools_button.invoke()
    pump(app, 0.3)
    assert app.current_section == "Tools"
    assert app.errors == []


def test_a_start_that_never_comes_is_reported(make_app: AppFactory) -> None:
    app, engine = make_app(elevated=True, maintenance_task=DEFAULTS)
    panel = open_maintenance(app)
    panel.run_now_button.invoke()
    confirm_dialog(app)
    pump(app, 5.0, until=lambda: idle(app) and bool(engine.calls_named("maintenance_run_now")))
    engine.maintenance_start_times_out()
    poll(app)
    text, color = status(app)
    assert text.startswith("⚠ Maintenance didn't start within a minute")
    assert color == theme.WARNING
    assert job_status(app) == ""
    assert app.errors == []


def test_run_now_needs_the_schedule_and_ac_power(make_app: AppFactory) -> None:
    app, _ = make_app(elevated=True)
    panel = open_maintenance(app)
    assert str(panel.run_now_button.cget("state")) == "disabled"
    assert str(panel.hint_label.cget("text")) == TURN_ON_FIRST_TEXT
    app, _ = make_app(elevated=True, maintenance_task=DEFAULTS, maintenance_on_battery=True)
    panel = open_maintenance(app)
    assert str(panel.run_now_button.cget("state")) == "disabled"
    assert str(panel.hint_label.cget("text")) == ON_BATTERY_TEXT
    assert app.errors == []


def test_an_unrecorded_task_can_only_be_removed(make_app: AppFactory) -> None:
    app, engine = make_app(elevated=True, maintenance_task=DEFAULTS, maintenance_recorded=False)
    panel = open_maintenance(app)
    assert str(panel.state_label.cget("text")) == "⚠ Not recorded by Cairn"
    assert panel.unrecorded_box.winfo_manager()
    assert str(panel.unrecorded_label.cget("text")) == f"⚠ {UNRECORDED_TEXT}"
    assert str(panel.unrecorded_label.cget("text_color")) == theme.WARNING
    assert str(panel.primary_button.cget("state")) == "disabled"
    assert str(panel.run_now_button.cget("state")) == "disabled"
    assert not panel.turn_off_button.winfo_manager()
    panel.remove_button.invoke()
    dialog = next_dialog(app)
    assert dialog.title_text == REMOVE_TASK_TITLE
    assert REMOVE_TASK_MESSAGE in dialog_text(dialog)
    assert str(dialog.confirm_button.cget("fg_color")) == theme.CRITICAL
    confirm_dialog(app)
    pump(app, 5.0, until=lambda: idle(app) and bool(engine.calls_named("maintenance_remove_unrecorded")))
    pump(app, 5.0, until=lambda: not app._maintenance_loading and idle(app))
    assert engine.calls_named("maintenance_remove_unrecorded") == [(False,)]
    assert status(app) == ("✓ The maintenance task was removed from Task Scheduler.", theme.GOOD)
    assert str(panel.state_label.cget("text")) == "○ Off"
    assert not panel.unrecorded_box.winfo_manager()
    assert app.errors == []


def test_an_unseen_run_is_announced_at_start_with_a_badge(make_app: AppFactory) -> None:
    run = run_dict(4, "attention", attention=[REPAIR_HINT], ended_at=recently())
    app, engine = make_app(elevated=True, maintenance_task=DEFAULTS, maintenance_runs=[run])
    pump(app, 5.0, until=lambda: badge(app) == "⚠")
    text, color = status(app)
    assert text.endswith(f"found something to look at: {REPAIR_HINT}")
    assert color == theme.WARNING
    assert engine.calls_named("maintenance_acknowledge") == []
    open_maintenance(app)
    pump(app, 5.0, until=lambda: bool(engine.calls_named("maintenance_acknowledge")) and idle(app))
    assert engine.calls_named("maintenance_acknowledge") == [(4,)]
    assert badge(app) == ""
    assert app.errors == []


def test_a_run_that_never_finished_is_announced_only_until_it_is_seen(make_app: AppFactory) -> None:
    # The run's process ended without finishing: its row still says running and nothing holds
    # the run lock.
    died = dict(run_dict(5, "running", started_at=recently(), ended_at=None, log_path=None), stale=True)
    app, engine = make_app(elevated=True, maintenance_task=DEFAULTS, maintenance_runs=[died])
    pump(app, 8.0, until=lambda: "didn't finish" in status(app)[0])
    text, color = status(app)
    assert text.startswith("○ The maintenance run of ") and color == theme.INK_SECONDARY
    assert badge(app) == ""
    assert engine.calls_named("maintenance_acknowledge") == []
    open_maintenance(app)
    pump(app, 5.0, until=lambda: bool(engine.calls_named("maintenance_acknowledge")) and idle(app))
    assert engine.calls_named("maintenance_acknowledge") == [(5,)]
    closed = engine.maintenance_runs[0]
    assert (closed["state"], closed["stale"], closed["acknowledged"]) == ("interrupted", False, True)
    assert closed["ended_at"] is not None
    # The run is seen now: another visit marks nothing, and the next start stays quiet.
    show_section(app, "Dashboard")
    open_maintenance(app)
    pump(app, 0.5)
    assert engine.calls_named("maintenance_acknowledge") == [(5,)]
    assert app.errors == []
    later, _ = make_app(elevated=True, maintenance_task=DEFAULTS, maintenance_runs=[closed])
    pump(later, 5.0, until=lambda: later._maintenance_known == 5)
    pump(later, STARTUP_NOTICE_SECONDS + 1.0)
    assert later._maintenance_pending is None
    assert "maintenance run" not in status(later)[0]
    assert later.errors == []


def test_marking_an_unfinished_run_leaves_a_run_that_starts_meanwhile_running(
    make_app: AppFactory,
) -> None:
    died = dict(run_dict(5, "running", started_at=recently(), ended_at=None, log_path=None), stale=True)
    app, engine = make_app(elevated=True, maintenance_task=DEFAULTS, maintenance_runs=[died])
    asked, begun = threading.Event(), threading.Event()
    acknowledge = engine.maintenance_acknowledge

    def acknowledge_once_a_run_began(run_id: int) -> bool:
        asked.set()
        begun.wait(5.0)
        return acknowledge(run_id)

    engine.maintenance_acknowledge = acknowledge_once_a_run_began  # type: ignore[method-assign]
    pump(app, 5.0, until=lambda: idle(app))
    show_section(app, SECTION)
    pump(app, 5.0, until=asked.is_set)
    # The task starts a run while the window marks the one that never finished.
    started = engine.maintenance_begin_run()
    begun.set()
    pump(app, 5.0, until=lambda: bool(engine.calls_named("maintenance_acknowledge")) and idle(app))
    pump(app, 5.0, until=lambda: not app._maintenance_loading and idle(app))
    assert engine.calls_named("maintenance_acknowledge") == [(5,)]
    closed, current = engine.maintenance_runs
    assert (closed["id"], closed["state"], closed["acknowledged"]) == (5, "interrupted", True)
    assert (current["id"], current["state"], current["acknowledged"]) == (started, "running", False)
    poll(app)
    assert app._maintenance_running
    assert app._maintenance_blocks_cleanup() == RUNNING_ELSEWHERE_TEXT
    assert app.errors == []


def test_seen_and_old_runs_are_not_announced(make_app: AppFactory) -> None:
    seen = run_dict(2, "failed", ended_at=recently(), acknowledged=True)
    app, _ = make_app(elevated=True, maintenance_task=DEFAULTS, maintenance_runs=[seen])
    pump(app, 2.0)
    assert badge(app) == "" and "Maintenance" not in status(app)[0]
    old = run_dict(3, "failed", ended_at=recently(days=30))
    app, _ = make_app(elevated=True, maintenance_task=DEFAULTS, maintenance_runs=[old])
    pump(app, 2.0)
    assert badge(app) == "" and "Maintenance" not in status(app)[0]
    assert app.errors == []


def test_cleanup_waits_while_maintenance_cleans(make_app: AppFactory) -> None:
    app, engine = make_app(elevated=True, maintenance_task=DEFAULTS)
    pump(app, 5.0, until=lambda: idle(app))
    engine.maintenance_begin_run()
    engine.maintenance_step("cleanup", "Cleaning up…", 1, 3)
    poll(app)
    assert app._maintenance_blocks_cleanup() == RUNNING_ELSEWHERE_TEXT
    app._on_clean([{"id": "user_temp", "title": "Temporary files", "bytes": 1000}])
    pump(app, 0.3)
    assert status(app) == (RUNNING_ELSEWHERE_TEXT, theme.WARNING)
    assert dialogs(app) == []
    assert engine.calls_named("cleanup_run") == []
    engine.maintenance_end_run()
    poll(app)
    assert app._maintenance_blocks_cleanup() is None
    assert app.errors == []


def test_a_failing_poll_latches_the_lane(make_app: AppFactory) -> None:
    app, engine = make_app(elevated=True)
    pump(app, 5.0, until=lambda: idle(app))
    calls: list[int] = []

    def broken() -> dict[str, Any]:
        calls.append(1)
        raise RuntimeError("monitor exploded")

    engine.maintenance_progress = broken  # type: ignore[method-assign]
    poll(app)
    assert "maintenance" in app._lane_failed
    assert len(calls) == 1
    assert any("monitor exploded" in e for e in app.errors)
    poll(app)
    assert len(calls) == 1
    assert job_status(app) == ""
    app.errors.clear()


def test_an_outdated_engine_shows_the_section_as_unavailable(make_app: AppFactory) -> None:
    app, engine = make_app(elevated=True, unsupported=("maintenance_status",))
    pump(app, 5.0, until=lambda: idle(app))
    show_section(app, SECTION)
    pump(app, 0.3)
    panel = app.maintenance_panel
    assert UNSUPPORTED_TEXT in str(panel.state_label.cget("text"))
    assert str(panel.primary_button.cget("state")) == "disabled"
    assert reloads(engine) == 0
    assert app.errors == []


def test_a_status_that_cannot_be_read_is_shown(make_app: AppFactory) -> None:
    app, engine = make_app(elevated=True, maintenance_errors={"maintenance_status": "journal locked"})
    pump(app, 5.0, until=lambda: idle(app))
    show_section(app, SECTION)
    pump(app, 5.0, until=lambda: reloads(engine) == 1 and not app._maintenance_loading and idle(app))
    panel = app.maintenance_panel
    assert not panel.loaded
    assert str(panel.state_label.cget("text")) == "⚠ The maintenance status could not be read: journal locked"
    assert str(panel.primary_button.cget("state")) == "disabled"
    assert app.errors == []


def test_open_log_and_earlier_runs(make_app: AppFactory) -> None:
    runs = [run_dict(1, "completed", acknowledged=True), run_dict(2, "stopped", acknowledged=True)]
    app, engine = make_app(elevated=True, maintenance_task=DEFAULTS, maintenance_runs=runs)
    panel = open_maintenance(app)
    assert str(panel.run_state_label.cget("text")).startswith("○ Stopped early ")
    assert panel.earlier_label.winfo_manager()
    assert "✓ Freed 1.2 GB" in str(panel.earlier_label.cget("text"))
    panel.open_log_button.invoke()
    pump(app, 5.0, until=lambda: idle(app) and bool(engine.calls_named("maintenance_open_log")))
    assert engine.calls_named("maintenance_open_log") == [(2,)]
    assert app.errors == []


def test_a_change_elsewhere_reloads_the_section(make_app: AppFactory) -> None:
    app, engine = make_app(elevated=True, maintenance_task=DEFAULTS)
    open_maintenance(app)
    before = reloads(engine)
    app._maintenance_after_mutation()
    pump(app, 5.0, until=lambda: reloads(engine) > before and idle(app))
    assert app.errors == []


@pytest.mark.parametrize(
    ("button", "call", "calls", "options", "action", "kind"),
    [
        (
            "primary_button",
            "maintenance_set_schedule",
            2,
            {},
            "Turning on scheduled maintenance",
            CLOSE_JOURNALED,
        ),
        (
            "turn_off_button",
            "revert_targets",
            1,
            {"maintenance_task": DEFAULTS},
            "Turning off scheduled maintenance",
            CLOSE_JOURNALED,
        ),
        (
            "run_now_button",
            "maintenance_run_now",
            1,
            {"maintenance_task": DEFAULTS},
            "Starting maintenance",
            CLOSE_OTHER,
        ),
        (
            "remove_button",
            "maintenance_remove_unrecorded",
            1,
            {"maintenance_task": DEFAULTS, "maintenance_recorded": False},
            "Removing the maintenance task",
            CLOSE_IRREVERSIBLE,
        ),
    ],
)
def test_closing_during_an_action_names_it_and_what_closing_means(
    make_app: AppFactory,
    button: str,
    call: str,
    calls: int,
    options: dict[str, Any],
    action: str,
    kind: str,
) -> None:
    app, engine = make_app(elevated=True, slow={call: 1.0}, **options)
    panel = open_maintenance(app)
    getattr(panel, button).invoke()
    confirm_dialog(app)
    pump(app, 5.0, until=lambda: len(engine.calls_named(call)) == calls and app._busy)
    assert str(panel.primary_button.cget("state")) == "disabled"
    app._request_close()
    dialog = next_dialog(app)
    assert dialog.title_text == BUSY_CLOSE_TITLE
    text = dialog_text(dialog)
    assert f"{action} is still running." in text
    assert CLOSE_TEXTS[kind] in text
    cancel_shown(app, dialog)
    pump(app, 5.0, until=lambda: idle(app) and not app._maintenance_loading)
    assert app._running
    assert app.errors == []


def test_the_section_fits(make_app: AppFactory) -> None:
    runs = [
        run_dict(1, "completed", acknowledged=True),
        run_dict(2, "attention", attention=[REPAIR_HINT], acknowledged=True),
    ]
    app, _ = make_app(
        elevated=True,
        maintenance_task=DEFAULTS,
        maintenance_runs=runs,
        maintenance_drift=["it may wake the PC"],
        maintenance_has_battery=True,
    )
    open_maintenance(app)
    assert_section_fits(app, SECTION)
    assert app.errors == []


def test_the_section_fits_with_an_unrecorded_task(make_app: AppFactory) -> None:
    app, _ = make_app(elevated=False, maintenance_task=DEFAULTS, maintenance_recorded=False)
    panel = open_maintenance(app)
    assert panel.unrecorded_box.winfo_manager()
    assert_section_fits(app, SECTION)
    assert app.errors == []


def test_long_location_descriptions_wrap_inside_the_card(
    make_app: AppFactory, monkeypatch: pytest.MonkeyPatch
) -> None:
    # As long as the engine's own cleanup descriptions, which take two lines in the card.
    text = (
        "Update pieces Windows keeps to share with other PCs, removed with the built-in Delivery "
        "Optimization cleanup."
    )
    targets = fake_maintenance.target_dicts
    monkeypatch.setattr(
        fake_maintenance, "target_dicts", lambda: [{**t, "description": text} for t in targets()]
    )
    app, _ = make_app(elevated=True, maintenance_task=DEFAULTS)
    panel = open_maintenance(app)
    pump(app, 0.5)
    labels = [panel.sfc_label, panel.dism_label, *panel._target_labels]
    cut = [
        str(label.cget("text"))[:40]
        for label in labels
        if label._label.winfo_reqwidth() > label.winfo_width() + 1
    ]
    assert cut == []
    assert app.errors == []
