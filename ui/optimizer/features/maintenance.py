"""Maintenance section: the weekly scheduled maintenance task.

Turning maintenance on records the task in the journal before Task Scheduler gets it, so
turning it off is an undo (`revert_targets` with the task path, like History and Revert All);
the runs themselves happen in the task's own process outside the window and are only logged.
A run in progress is followed from the frame loop through the "maintenance" job lane: the
engine's run monitor keeps the latest observation, which the lane reads once a second without
waiting. Runs are out of process, so closing the window never waits for them.
"""

from __future__ import annotations

import logging
from concurrent.futures import Future
from datetime import datetime, timedelta
from typing import TYPE_CHECKING, Any

from .. import theme
from ..widgets.dialogs import MessageDialog
from ..widgets.maintenance import (
    INTRO_TEXT,
    IRREVERSIBLE_TEXT,
    ON_BATTERY_TEXT,
    READ_ONLY_TEXT,
    TURN_ON_FIRST_TEXT,
    UNRECORDED_TEXT,
    MaintenancePanel,
    day_label,
    local_time,
    needs_attention,
    notice_for,
    plan_details,
    run_state,
    schedule_text,
    status_bar_text,
    time_text,
)

if TYPE_CHECKING:
    import customtkinter as ctk

    from ..bridge.engine import EngineBridge
    from ..widgets.sidebar import Sidebar

log = logging.getLogger(__name__)

SECTION = "Maintenance"
MAINTENANCE_SECTION = SECTION
UNSUPPORTED_TEXT = "The engine is out of date: rebuild and deploy it to use this section."
NO_ENGINE_TEXT = "The engine is not loaded, so scheduled maintenance can't be read."
TURN_ON_TITLE = "Turn on scheduled maintenance?"
SAVE_TITLE = "Save the maintenance schedule?"
TURN_ON_MESSAGE = (
    "Cairn adds a task to Windows Task Scheduler that runs every {day} at {time} as your "
    "account with administrator rights, when the PC is idle and plugged in. Turning maintenance off, or "
    "undoing it in History, removes the task. Each run permanently deletes the files in the selected "
    "locations."
)
CANT_TURN_ON_TITLE = "Scheduled maintenance can't be turned on"
TURN_OFF_TITLE = "Turn off scheduled maintenance?"
TURN_OFF_MESSAGE = (
    "Cairn removes its task from Task Scheduler. Finished runs stay listed in History's "
    "activity log. A run in progress finishes on its own."
)
RUN_NOW_TITLE = "Run maintenance now?"
RUN_NOW_MESSAGE = (
    "Maintenance starts now in the background, even while you use the PC, and permanently "
    "deletes the files in the selected locations. It keeps running if you close Cairn. Checking system "
    "files usually takes 10–30 minutes."
)
REMOVE_TASK_TITLE = "Remove the maintenance task?"
REMOVE_TASK_MESSAGE = (
    "This deletes the task from Task Scheduler. Cairn has no record of creating it, so "
    "this can't be undone from History; turn maintenance on again to create a new one."
)
RUNNING_ELSEWHERE_TEXT = "⚠ Scheduled maintenance is cleaning right now; try again when it finishes."
UNCHANGED_TEXT = "The schedule is already set this way."
TURNED_OFF_TEXT = "✓ Scheduled maintenance is off; its task was removed from Task Scheduler."
NOT_TURNED_OFF_TEXT = "⚠ Scheduled maintenance could not be turned off."
STARTED_TEXT = "◐ Maintenance started in the background; it keeps running if you close Cairn."
REMOVED_TEXT = "✓ The maintenance task was removed from Task Scheduler."
START_TIMED_OUT_TEXT = (
    "⚠ Maintenance didn't start within a minute. Task Scheduler may still be waiting; check Last run "
    "in Maintenance later."
)
# How often the lane reads the run monitor.
MAINTENANCE_POLL_HZ = 1.0
# A finished, unseen run is announced at start while it ended this recently.
NOTICE_DAYS = 14
# At start the announcement waits this long, and until the window's first scan ended.
STARTUP_NOTICE_SECONDS = 2.0

__all__ = [
    "CANT_TURN_ON_TITLE",
    "INTRO_TEXT",
    "IRREVERSIBLE_TEXT",
    "MAINTENANCE_POLL_HZ",
    "ON_BATTERY_TEXT",
    "READ_ONLY_TEXT",
    "REMOVE_TASK_MESSAGE",
    "REMOVE_TASK_TITLE",
    "RUNNING_ELSEWHERE_TEXT",
    "RUN_NOW_MESSAGE",
    "RUN_NOW_TITLE",
    "SAVE_TITLE",
    "SECTION",
    "TURN_OFF_MESSAGE",
    "TURN_OFF_TITLE",
    "TURN_ON_FIRST_TEXT",
    "TURN_ON_MESSAGE",
    "TURN_ON_TITLE",
    "UNRECORDED_TEXT",
    "UNSUPPORTED_TEXT",
    "MaintenanceFeature",
]


def _recent(run: dict[str, Any], now: datetime | None = None) -> bool:
    """Whether a run ended within `NOTICE_DAYS`."""
    ended = local_time(run.get("ended_at") or run.get("started_at"))
    if ended is None:
        return False
    return (now or datetime.now()) - ended <= timedelta(days=NOTICE_DAYS)


class MaintenanceFeature:
    """State, widgets and flows of the Maintenance section, mixed into `App`.

    Uses these members of the window: `engine`, `elevated`, `nav`, `section_visible`,
    `show_section`, `set_status`, `set_job_status`, `_guard`, `_set_busy`, `_show_error`,
    `_refresh_journal`, `load_history` and `_busy`.
    """

    maintenance_panel: MaintenancePanel
    _maintenance_supported: bool
    _maintenance_status: dict[str, Any] | None
    _maintenance_loading: bool
    _maintenance_reload: bool
    _maintenance_polled: float
    _maintenance_seen: int | None
    _maintenance_known: int | None
    _maintenance_noticed: set[int]
    _maintenance_running: bool
    _maintenance_timed_out: bool
    _maintenance_pending: dict[str, Any] | None
    _maintenance_pending_at: float

    if TYPE_CHECKING:
        engine: EngineBridge | None
        elevated: bool
        nav: Sidebar
        _busy: bool

        def section_visible(self, name: str) -> bool: ...
        def show_section(self, name: str, *, run_hook: bool = True) -> None: ...
        def set_status(self, text: str, color: str = ...) -> None: ...
        def set_job_status(self, lane: str, text: str) -> None: ...
        def _guard(self, *, needs_admin: bool = True) -> bool: ...
        def _set_busy(self, busy: bool, close: str = ..., *, action: str = ..., note: str = ...) -> None: ...
        def _show_error(self, title: str, exc: BaseException | None) -> None: ...
        def _refresh_journal(self) -> None: ...
        def load_history(self) -> None: ...

    def _init_maintenance_state(self) -> None:
        """Sets the section's state; runs before the window is built."""
        self._maintenance_supported = True
        self._maintenance_status = None
        # A status read is in flight; a reload asked for meanwhile runs once it returns.
        self._maintenance_loading = False
        self._maintenance_reload = False
        self._maintenance_polled = 0.0
        # Id of the run the monitor last reported as running, and of the newest run it reported.
        self._maintenance_seen = None
        self._maintenance_known = None
        # Runs already announced in the status bar.
        self._maintenance_noticed = set()
        # A run is in progress (an administrator holds the run lock and its row is running).
        self._maintenance_running = False
        self._maintenance_timed_out = False
        # A finished run whose announcement waits for another operation to end, and the
        # earliest frame time it may be announced at.
        self._maintenance_pending = None
        self._maintenance_pending_at = 0.0

    def _build_maintenance_tab(self, frame: ctk.CTkFrame) -> None:
        """Builds the section's panel into `frame`; handles a missing or outdated engine."""
        self.maintenance_panel = MaintenancePanel(
            frame,
            on_turn_on=self._on_turn_on,
            on_turn_off=self._on_turn_off,
            on_run_now=self._on_run_now,
            on_remove_task=self._on_remove_task,
            on_open_log=self._on_open_maintenance_log,
            on_open_tools=self._on_open_tools,
        )
        self.maintenance_panel.grid(row=0, column=0, sticky="nsew", padx=4, pady=4)
        if self.engine is None:
            self._maintenance_supported = False
            self.maintenance_panel.set_unsupported(NO_ENGINE_TEXT)
        elif not self.engine.supports("maintenance_status"):
            self._maintenance_supported = False
            self.maintenance_panel.set_unsupported(UNSUPPORTED_TEXT)

    def _maintenance_tab_shown(self) -> None:
        """Runs when the Maintenance section is shown while the engine is loaded: the status
        is read on every visit."""
        self.load_maintenance()

    def _refresh_maintenance_actions(self) -> None:
        """Runs whenever the window's busy state changes."""
        panel = getattr(self, "maintenance_panel", None)
        if panel is not None:
            panel.set_actions_enabled(not self._busy and self.engine is not None)

    def _maintenance_after_mutation(self) -> None:
        """Runs after any journaled change or undo: Revert All and History remove the task."""
        panel = getattr(self, "maintenance_panel", None)
        if panel is not None and panel.loaded:
            self.load_maintenance()

    def _maintenance_blocks_cleanup(self) -> str | None:
        """Why Cleanup must not start now (a maintenance run is cleaning), or None."""
        return RUNNING_ELSEWHERE_TEXT if self._maintenance_running else None

    # -- status --------------------------------------------------------------------------

    def load_maintenance(self) -> None:
        """Queues a read of the maintenance status; a read already in flight is reused."""
        if self.engine is None or not self._maintenance_supported:
            return
        if self._maintenance_loading:
            self._maintenance_reload = True
            return
        self._maintenance_loading = True
        self.maintenance_panel.set_loading()
        self.engine.maintenance_status(callback=self._maintenance_loaded)

    def _maintenance_loaded(self, future: Future[Any]) -> None:
        self._maintenance_loading = False
        if self._maintenance_reload:
            self._maintenance_reload = False
            self.load_maintenance()
            return
        exc = future.exception()
        if exc is not None:
            self.maintenance_panel.show_error(f"The maintenance status could not be read: {exc}")
            return
        status = future.result()
        self._maintenance_status = status
        self.maintenance_panel.show(status, engine_ready=True, elevated=self.elevated)
        self._set_maintenance_running(bool(status.get("running")))
        runs = status.get("runs") or []
        latest = runs[0] if runs else None
        if latest is not None and self.section_visible(SECTION):
            self._acknowledge_run(latest)

    def _acknowledge_run(self, run: dict[str, Any]) -> None:
        """Marks a finished, unseen run as seen (its notice was shown) and clears the badge."""
        if self.engine is None or run.get("acknowledged") or run_state(run) == "running":
            return
        run["acknowledged"] = True
        self._clear_maintenance_badge()
        run_id = int(run["id"])

        def done(future: Future[Any]) -> None:
            if future.exception() is not None:
                log.warning("cannot acknowledge maintenance run %s: %s", run_id, future.exception())

        self.engine.acknowledge_maintenance(run_id, callback=done)

    def _set_maintenance_badge(self) -> None:
        self.nav.set_badge(SECTION, "⚠", theme.WARNING)

    def _clear_maintenance_badge(self) -> None:
        self.nav.set_badge(SECTION, "", theme.WARNING)

    def _set_maintenance_running(self, running: bool) -> None:
        self._maintenance_running = running

    # -- actions -------------------------------------------------------------------------

    def _on_turn_on(self) -> None:
        if not self._guard(needs_admin=True):
            return
        assert self.engine is not None
        config = self.maintenance_panel.config()
        self._set_busy(True, "read")
        self.set_status("Planning the maintenance schedule…")

        def planned(future: Future[Any]) -> None:
            self._set_busy(False)
            exc = future.exception()
            if exc is not None:
                self._show_error("Scheduled maintenance could not be turned on", exc)
                return
            plan = future.result()
            if plan.get("blocked_reason"):
                MessageDialog(self, title=CANT_TURN_ON_TITLE, message=str(plan["blocked_reason"]))  # type: ignore[arg-type]
                self.set_status(f"⚠ {plan['blocked_reason']}", theme.WARNING)
                return
            if plan.get("unchanged"):
                self.set_status(UNCHANGED_TEXT)
                return
            creates = bool(plan.get("creates"))
            targets = (self._maintenance_status or {}).get("targets") or []
            plan_config = plan.get("config") or config
            MessageDialog(
                self,  # type: ignore[arg-type]
                title=TURN_ON_TITLE if creates else SAVE_TITLE,
                message=TURN_ON_MESSAGE.format(
                    day=day_label(plan_config.get("day", "")), time=time_text(plan_config.get("time", ""))
                ),
                details=plan_details(plan, targets),
                confirm_text="Turn on" if creates else "Save",
                cancel_text="Cancel",
                on_confirm=lambda: self._set_schedule(plan_config, creates),
            )

        self.engine.plan_maintenance_schedule(config, callback=planned)

    def _set_schedule(self, config: dict[str, Any], creates: bool) -> None:
        assert self.engine is not None
        action = "Turning on scheduled maintenance" if creates else "Saving the maintenance schedule"
        self._set_busy(True, "journaled", action=action)
        self.maintenance_panel.set_busy(f"{action}…")
        self.set_status(f"{action}…")

        def done(future: Future[Any]) -> None:
            self._set_busy(False)
            self.maintenance_panel.set_idle()
            exc = future.exception()
            if exc is not None:
                self._show_error("Scheduled maintenance could not be turned on", exc)
            else:
                self.set_status(f"✓ Scheduled maintenance is on: {schedule_text(config)}.", theme.GOOD)
            self._maintenance_changed()

        self.engine.set_maintenance_schedule(config, callback=done)

    def _maintenance_changed(self) -> None:
        """Refreshes what a schedule change affects: the journal, History and this section."""
        self._refresh_journal()
        if self.section_visible("History"):
            self.load_history()
        self.load_maintenance()

    def _on_turn_off(self) -> None:
        status = self._maintenance_status or {}
        task_path = status.get("task_path")
        if not task_path or not self._guard(needs_admin=True):
            return

        def run() -> None:
            assert self.engine is not None
            action = "Turning off scheduled maintenance"
            self._set_busy(True, "journaled", action=action)
            self.maintenance_panel.set_busy(f"{action}…")
            self.set_status(f"{action}…")
            self.engine.turn_off_maintenance(str(task_path), callback=self._turned_off)

        MessageDialog(
            self,  # type: ignore[arg-type]
            title=TURN_OFF_TITLE,
            message=TURN_OFF_MESSAGE,
            confirm_text="Turn off",
            cancel_text="Cancel",
            on_confirm=run,
        )

    def _turned_off(self, future: Future[Any]) -> None:
        self._set_busy(False)
        self.maintenance_panel.set_idle()
        exc = future.exception()
        if exc is not None:
            self._show_error("Scheduled maintenance could not be turned off", exc)
        else:
            report = future.result()
            failures = report.get("failures") or []
            if failures:
                MessageDialog(
                    self,  # type: ignore[arg-type]
                    title="Scheduled maintenance could not be turned off",
                    message="The task could not be removed; its journal record stays, so you can try again.",
                    details=[f"• {f.get('target', '')}: {f.get('error', '')}" for f in failures],
                )
                self.set_status(NOT_TURNED_OFF_TEXT, theme.WARNING)
            else:
                self.set_status(TURNED_OFF_TEXT, theme.GOOD)
        self._maintenance_changed()

    def _on_run_now(self) -> None:
        if not self._guard(needs_admin=True):
            return

        def run() -> None:
            assert self.engine is not None
            self._set_busy(True, "other", action="Starting maintenance")
            self.set_status("Starting maintenance…")
            self.engine.run_maintenance_now(callback=self._run_now_done)

        MessageDialog(
            self,  # type: ignore[arg-type]
            title=RUN_NOW_TITLE,
            message=RUN_NOW_MESSAGE,
            confirm_text="Run now",
            cancel_text="Cancel",
            danger=True,
            on_confirm=run,
        )

    def _run_now_done(self, future: Future[Any]) -> None:
        self._set_busy(False)
        exc = future.exception()
        if exc is not None:
            self._show_error("Maintenance could not be started", exc)
            return
        assert self.engine is not None
        self._maintenance_timed_out = False
        self.engine.maintenance_watch(expect_start=True)
        self.maintenance_panel.show_starting()
        self.set_status(STARTED_TEXT, theme.ACCENT)
        # The next frame reads the monitor at once.
        self._maintenance_polled = 0.0

    def _on_remove_task(self) -> None:
        if not self._guard(needs_admin=True):
            return

        def run() -> None:
            assert self.engine is not None
            self._set_busy(True, "irreversible", action="Removing the maintenance task")
            self.maintenance_panel.set_busy("Removing the maintenance task…")
            self.engine.remove_unrecorded_maintenance(callback=self._removed)

        MessageDialog(
            self,  # type: ignore[arg-type]
            title=REMOVE_TASK_TITLE,
            message=REMOVE_TASK_MESSAGE,
            confirm_text="Remove task",
            cancel_text="Cancel",
            danger=True,
            on_confirm=run,
        )

    def _removed(self, future: Future[Any]) -> None:
        self._set_busy(False)
        self.maintenance_panel.set_idle()
        exc = future.exception()
        if exc is not None:
            self._show_error("The maintenance task could not be removed", exc)
        else:
            self.set_status(REMOVED_TEXT, theme.GOOD)
        self.load_maintenance()

    def _on_open_maintenance_log(self, run_id: int) -> None:
        if self.engine is None:
            return

        def done(future: Future[Any]) -> None:
            if future.exception() is not None:
                self._show_error("The maintenance log could not be opened", future.exception())

        self.engine.open_maintenance_log(run_id, callback=done)

    def _on_open_tools(self) -> None:
        self.show_section("Tools")

    # -- job lane "maintenance" ----------------------------------------------------------

    def _maintenance_start_watch(self) -> None:
        """Runs once after the engine is loaded, when the window starts: starts the engine's
        run monitor, whose first observation announces an unseen finished run."""
        if self.engine is not None and self.engine.supports("maintenance_watch"):
            self.engine.maintenance_watch(expect_start=False)

    def _poll_maintenance(self, now: float) -> None:
        """Runs every frame while the engine is loaded; reads the run monitor once a second."""
        if self.engine is None or not self._maintenance_supported:
            return
        if now - self._maintenance_polled < 1.0 / MAINTENANCE_POLL_HZ:
            return
        self._maintenance_polled = now
        pending = self._maintenance_pending
        if pending is not None and not self._busy and now >= self._maintenance_pending_at:
            self._maintenance_pending = None
            self._maintenance_finished(pending)
        observation = self.engine.maintenance_progress()
        if observation is None:
            return
        self.set_job_status("maintenance", status_bar_text(observation))
        running = bool(observation.get("running"))
        run = observation.get("run")
        self._set_maintenance_running(running)
        panel = getattr(self, "maintenance_panel", None)
        if panel is not None:
            panel.show_progress(observation)
        if observation.get("start_timed_out") and not self._maintenance_timed_out:
            self._maintenance_timed_out = True
            self.set_status(START_TIMED_OUT_TEXT, theme.WARNING)
        if run is None:
            return
        run_id = int(run.get("id", 0))
        known = self._maintenance_known
        self._maintenance_known = run_id if known is None else max(known, run_id)
        if running:
            self._maintenance_seen = run_id
            return
        seen, self._maintenance_seen = self._maintenance_seen, None
        # A run seen running that ended, a run newer than any seen before (it ran between two
        # reads), or at start an unseen run that ended recently.
        if seen == run_id or (known is not None and run_id > known):
            self._maintenance_finished(run)
        elif known is None and not run.get("acknowledged") and _recent(run):
            # At start the window's first scan would replace the notice at once.
            self._maintenance_pending = run
            self._maintenance_pending_at = now + STARTUP_NOTICE_SECONDS

    def _maintenance_finished(self, run: dict[str, Any]) -> None:
        """Announces a finished run once: a status bar notice, the sidebar badge when it needs
        a look, and a reload of the section when it is loaded. While another operation runs, its
        own status would replace the notice, so the announcement waits until it ends."""
        if self._busy:
            self._maintenance_pending = run
            self._maintenance_pending_at = 0.0
            return
        run_id = int(run.get("id", 0))
        if run_id in self._maintenance_noticed:
            return
        self._maintenance_noticed.add(run_id)
        notice = notice_for(run)
        if notice is not None and not run.get("acknowledged"):
            self.set_status(*notice)
            if needs_attention(run) and not self.section_visible(SECTION):
                self._set_maintenance_badge()
        panel = getattr(self, "maintenance_panel", None)
        if panel is not None and panel.loaded:
            self.load_maintenance()

    def _maintenance_poll_failed(self) -> None:
        """Runs once after `_poll_maintenance` raised; the window stops polling this lane."""
        self.set_job_status("maintenance", "")
        self._maintenance_running = False

    def _maintenance_allow_close(self) -> bool:
        """Runs happen in the task's own process, so closing never waits for them."""
        return True

    def _maintenance_before_shutdown(self) -> None:
        """Nothing to stop: runs are out of process and the monitor is the engine's."""

    def _maintenance_running_title(self) -> str | None:
        """A run is out of process, so no window job runs in this lane."""
        return None
