"""Updates section: app updates and installs through winget, and Windows Update settings.

What is journaled: the Windows Update settings are registry values whose originals the engine
records before each change, so the section's Undo, History and Revert All restore them. What
is only logged: updating or installing an app can't be undone; the engine writes an audit
row before each app starts and one when it ends (History › Activity).

winget runs as engine-owned jobs of the "updates" job lane. A start is queued on the worker
like any other call; after that the frame loop reads the job 10 times a second while the
section is shown and twice a second otherwise, and again on the next frame while the engine
has more output waiting. Output read while the output box is hidden is kept and inserted at
most 200 lines per frame once it is shown. The status bar shows the job in every section.

Nothing loads at start: the first visit reads winget's status and, once it is ready, checks
for app updates; the Windows Update view reads its settings on every visit.

Uses these members of the window: `engine`, `elevated`, `errors`, `_busy`,
`section_visible`, `set_status`, `set_job_status`, `_guard`, `_set_busy`, `_show_error`,
`_refresh_journal`, `load_history`, `_revert_done`, `_continue_close`, `_running_close`,
`state`, `withdraw` and `update_idletasks`.
"""

from __future__ import annotations

import logging
import tkinter as tk
from concurrent.futures import Future
from typing import TYPE_CHECKING, Any

from .. import APP_NAME, system, theme
from ..widgets.dialogs import MessageDialog
from ..widgets.updates import (
    AGREEMENTS_NOTE,
    HOME_DEFER_TEXT,
    IRREVERSIBLE_NOTE,
    OTHER_USER_TEXT,
    POLICY_CAVEAT,
    SETTING_TITLES,
    USER_UNKNOWN_TEXT,
    WINGET_MISSING_TEXT,
    WINGET_OUTDATED_TEXT,
    UpdatesPanel,
    apps_text,
    batch_line,
    version_change,
)

if TYPE_CHECKING:
    import customtkinter as ctk

    from ..bridge.engine import EngineBridge

__all__ = [
    "AGREEMENTS_NOTE",
    "ALL_INSTALLED_TEXT",
    "BATCH_RUNNING_TEXT",
    "HOME_DEFER_TEXT",
    "INSTALL_CONFIRM_MESSAGE",
    "IRREVERSIBLE_NOTE",
    "LOST_TEXT",
    "MAX_BATCH_ITEMS",
    "OTHER_USER_TEXT",
    "RESTORE_POINT_TIP",
    "STOPPING_TEXT",
    "TOO_MANY_TEXT",
    "UNSUPPORTED_TEXT",
    "UPDATES_SECTION",
    "UPDATE_CONFIRM_MESSAGE",
    "USER_UNKNOWN_TEXT",
    "UpdatesFeature",
    "WINGET_MISSING_TEXT",
    "WINGET_OUTDATED_TEXT",
]

log = logging.getLogger(__name__)

UNSUPPORTED_TEXT = "The engine is out of date: rebuild and deploy it to use this section."
UPDATES_SECTION = "Updates"
UPDATES_POLL_HZ = 10.0
UPDATES_POLL_HIDDEN_HZ = 2.0
# How often the frame loop moves the bar of a job without a known fraction.
UPDATES_ANIMATE_SECONDS = 0.05

RESTORE_POINT_TIP = "Want a way back? Create a restore point in Tools first."
UPDATE_CONFIRM_MESSAGE = (
    APP_NAME
    + " updates them one after another with winget. App updates can't be undone by "
    + APP_NAME
    + ". "
    "Open apps may need to be closed first, and some updates finish only after a restart."
)
INSTALL_CONFIRM_MESSAGE = (
    APP_NAME + " installs them one after another with winget, from each app's publisher. Installs can't be "
    "undone by " + APP_NAME + "; remove apps in Settings › Apps. Apps already on this PC are skipped."
)
ALL_INSTALLED_TEXT = "All selected apps are already installed."
BATCH_RUNNING_TEXT = "Apps are being updated or installed; wait for that to finish."
LOST_TEXT = (
    "Lost track of the app updates; they may still be running. Their log is in "
    + APP_NAME
    + "'s jobs folder."
)
STOPPING_TEXT = "Stopping after {name}…"
DEFER_CONFIRM_TEXT = (
    "Windows Update waits {days} days after each new Windows version is released before offering it. "
    + POLICY_CAVEAT
    + " You can undo it here or in History."
)
NOTHING_STARTED_TEXT = "Nothing was started."
SELECT_UPDATES_TEXT = "Select the apps to update."
SELECT_INSTALLS_TEXT = "Select the apps to install."
# Most apps the engine takes in one batch.
MAX_BATCH_ITEMS = 200
TOO_MANY_TEXT = "Pick at most {limit} apps at a time."
# States of a batch item after which the app is on this PC.
INSTALLED_STATES = ("succeeded", "already_installed", "already_current", "restart_required")


def _job_id(job: Any) -> int | None:
    value = job.get("id") if isinstance(job, dict) else None
    return value if isinstance(value, int) and not isinstance(value, bool) else None


class UpdatesFeature:
    """State, widgets and flows of the Updates section, mixed into `App`.

    `_updates_job` is the id of the winget job being followed (None when none runs) and
    `_updates_job_kind` its kind ("scan", "upgrade" or "install"); `_updates_running_title`
    is the only view other features have of it. `_last_updates_job` stays set after the job
    finishes, for its log, and `_last_batch_job` is the last update or install batch, whose
    log Open log opens while the job strip shows that batch's result line. `_updates_last_results`
    keeps each app's last batch result of this session (id lowercased), for the "Last attempt"
    notes, Update all and the installed apps.
    """

    _updates_supported: bool
    _updates_view: str
    _updates_status: dict[str, Any] | None
    _updates_status_loading: bool
    _updates_wu: dict[str, Any] | None
    _updates_wu_loading: bool
    _updates_wu_reload: bool
    _updates_apps: dict[str, Any] | None
    _updates_apps_loading: bool
    _updates_scan: dict[str, Any] | None
    _updates_scan_started: bool
    _updates_job: int | None
    _updates_job_kind: str | None
    _updates_job_title: str
    _updates_after: int
    _updates_more: bool
    _updates_polled: float
    _updates_animated: float
    _updates_result_rev: int
    _updates_result: dict[str, Any] | None
    _updates_last_results: dict[str, dict[str, Any]]
    _updates_starting: str | None
    _last_updates_job: int | None
    _last_batch_job: int | None
    updates_panel: UpdatesPanel

    if TYPE_CHECKING:
        engine: EngineBridge | None
        elevated: bool
        errors: list[str]
        _busy: bool

        def section_visible(self, name: str) -> bool: ...
        def set_status(self, text: str, color: str = ...) -> None: ...
        def set_job_status(self, lane: str, text: str) -> None: ...
        def _guard(self, *, needs_admin: bool = True) -> bool: ...
        def _set_busy(self, busy: bool, close: str = ..., *, action: str = ..., note: str = ...) -> None: ...
        def _show_error(self, title: str, exc: BaseException | None) -> None: ...
        def _refresh_journal(self) -> None: ...
        def load_history(self) -> None: ...
        def _revert_done(self, future: Future[Any]) -> None: ...
        def _continue_close(self, lane: str, *, busy_unchanged: bool) -> None: ...
        def _running_close(self) -> Any: ...
        def state(self) -> str: ...
        def withdraw(self) -> None: ...
        def update_idletasks(self) -> None: ...

    def _init_updates_state(self) -> None:
        """Sets the section's state; runs before the window is built."""
        self._updates_supported = True
        self._updates_view = "apps"
        self._updates_status = None
        self._updates_status_loading = False
        self._updates_wu = None
        self._updates_wu_loading = False
        self._updates_wu_reload = False
        self._updates_apps = None
        self._updates_apps_loading = False
        self._updates_scan = None
        self._updates_scan_started = False
        self._updates_job = None
        self._updates_job_kind = None
        self._updates_job_title = ""
        self._updates_after = 0
        self._updates_more = False
        self._updates_polled = 0.0
        self._updates_animated = 0.0
        self._updates_result_rev = 0
        self._updates_result = None
        self._updates_last_results = {}
        # Kind of the job whose start is queued on the worker.
        self._updates_starting = None
        self._last_updates_job = None
        self._last_batch_job = None

    # -- building and loading ----------------------------------------------------------

    def _build_updates_tab(self, frame: ctk.CTkFrame) -> None:
        """Builds the section's panel into `frame`; handles a missing or outdated engine itself."""
        self.updates_panel = UpdatesPanel(
            frame,
            on_view=self._on_updates_view,
            on_check=self._on_updates_check,
            on_update=self._on_updates_update,
            on_stop=self._on_updates_stop,
            on_output=self._on_updates_output,
            on_open_log=self._on_updates_open_log,
            on_install=self._on_updates_install,
            on_save_list=self._on_updates_save_list,
            on_wu_set=self._on_wu_set,
            on_wu_undo=self._on_wu_undo,
            on_open_uri=self._on_updates_open_uri,
        )
        self.updates_panel.grid(row=0, column=0, sticky="nsew", padx=4, pady=4)
        if self.engine is None or not self.engine.supports("updates_winget_status"):
            self._updates_supported = False
            self.updates_panel.set_unsupported(UNSUPPORTED_TEXT)
            return
        self.updates_panel.set_access(True, self.elevated)
        self._refresh_updates_actions()

    def _updates_tab_shown(self) -> None:
        """Runs when the Updates section is shown while the engine is loaded."""
        if not self._updates_supported or self.engine is None:
            return
        # The next frame reads the job at once instead of waiting for the hidden-section period.
        self._updates_polled = 0.0
        self._updates_view_needs()

    def _on_updates_view(self, view: str) -> None:
        self._updates_view = view
        if self._updates_supported and self.engine is not None:
            self._updates_view_needs()

    def _updates_ready(self) -> bool:
        return (self._updates_status or {}).get("availability") == "ready"

    def _updates_view_needs(self) -> None:
        """Loads what the current view shows: winget's status once, the Windows Update
        settings on every visit of that view, and a check for app updates once per session."""
        if self._updates_status is None:
            self._load_updates_status()
            if self._updates_view == "windows":
                self._load_wu_state()
            return
        if self._updates_view == "windows":
            self._load_wu_state()
            return
        idle = self._updates_job is None and self._updates_starting is None
        if self._updates_ready() and idle and not self._updates_scan_started:
            self._start_updates_scan()
        if self._updates_view == "install" and self._updates_apps is None:
            self._load_updates_apps()

    def _load_updates_status(self) -> None:
        engine = self.engine
        if engine is None or self._updates_status_loading:
            return
        self._updates_status_loading = True

        def done(future: Future[Any]) -> None:
            self._updates_status_loading = False
            exc = future.exception()
            if exc is not None:
                self.updates_panel.apps.show_error(f"Could not read winget's status: {exc}")
                return
            self._updates_status = dict(future.result() or {})
            self.updates_panel.set_winget_status(self._updates_status)
            self._refresh_updates_actions()
            if self.section_visible(UPDATES_SECTION) and self._updates_view != "windows":
                self._updates_view_needs()

        engine.updates_winget_status(callback=done)

    def _load_wu_state(self) -> None:
        """Reads the Windows Update settings; a request while a read runs reads again after it."""
        engine = self.engine
        if engine is None:
            return
        if self._updates_wu_loading:
            self._updates_wu_reload = True
            return
        self._updates_wu_loading = True
        self.updates_panel.windows.set_loading()

        def done(future: Future[Any]) -> None:
            self._updates_wu_loading = False
            exc = future.exception()
            if exc is not None:
                self.updates_panel.windows.show_error(str(exc))
            else:
                self._updates_wu = dict(future.result() or {})
                self.updates_panel.windows.show_state(self._updates_wu)
                self._refresh_updates_actions()
            if self._updates_wu_reload:
                self._updates_wu_reload = False
                self._load_wu_state()

        engine.updates_wu_state(callback=done)

    def _load_updates_apps(self) -> None:
        engine = self.engine
        if engine is None or self._updates_apps_loading:
            return
        self._updates_apps_loading = True

        def done(future: Future[Any]) -> None:
            self._updates_apps_loading = False
            exc = future.exception()
            if exc is not None:
                self.updates_panel.install.show_error(f"Could not read the install list: {exc}")
                return
            self._updates_apps = dict(future.result() or {})
            self.updates_panel.install.show_apps(self._updates_apps, self._updates_installed_ids())

        engine.updates_app_list(callback=done)

    def _updates_installed_ids(self) -> list[str] | None:
        """Ids the last check found installed, plus the apps a batch of this session left on
        this PC (winget's list of installed apps can leave out an app it finds by id); None
        before any check."""
        if self._updates_scan is None:
            return None
        ids = [str(i) for i in self._updates_scan.get("installed") or []]
        known = {i.lower() for i in ids}
        for key, item in self._updates_last_results.items():
            if item.get("state") in INSTALLED_STATES and key not in known:
                ids.append(str(item.get("id") or key))
                known.add(key)
        return ids

    def _refresh_updates_actions(self) -> None:
        """Runs whenever the window's busy state changes, to enable or disable the actions."""
        panel = getattr(self, "updates_panel", None)
        if panel is None or not self._updates_supported:
            return
        running = self._updates_job is not None or self._updates_starting is not None
        panel.set_actions_enabled(not self._busy and self.engine is not None, job_running=running)

    def _updates_after_mutation(self) -> None:
        """Runs after any journaled change or undo: the Windows Update settings may have changed.
        They are read again now when their view is shown, else on its next visit."""
        if not self._updates_supported or self.engine is None:
            return
        if self.section_visible(UPDATES_SECTION) and self._updates_view == "windows":
            self._load_wu_state()

    def _updates_history_titles(self) -> dict[str, str]:
        """History titles of the Windows Update journal targets (target -> title)."""
        if self.engine is None:
            return {}
        titles: dict[str, str] = {}
        for entry in self.engine.updates_wu_catalog():
            for target in entry.get("targets") or []:
                titles[str(target)] = str(entry.get("title") or "")
        return titles

    # -- winget jobs -----------------------------------------------------------------------

    def _on_updates_check(self) -> None:
        """Check again: reads winget's status again when it isn't ready, else checks for updates."""
        if self.engine is None or not self._updates_supported:
            return
        if self._updates_job is not None or self._updates_starting is not None:
            self.set_status(BATCH_RUNNING_TEXT, theme.WARNING)
            return
        if not self._updates_ready():
            self._updates_status = None
            self._updates_scan_started = False
            self._load_updates_status()
            return
        self._start_updates_scan()

    def _start_updates_scan(self) -> None:
        self._updates_scan_started = True
        self._start_updates_job("scan", None)

    def _start_updates_job(self, kind: str, items: list[dict[str, Any]] | None) -> None:
        """Queues the start of a job on the worker and follows it once it runs."""
        engine = self.engine
        if engine is None:
            return
        self._updates_starting = kind
        if kind == "scan":
            self.updates_panel.apps.set_checking()

        def started(future: Future[Any]) -> None:
            self._updates_starting = None
            if kind != "scan":
                self._set_busy(False)
            exc = future.exception()
            job = None if exc is not None else (future.result() or {}).get("job")
            if exc is not None or _job_id(job) is None:
                error = exc or RuntimeError("the engine started no job")
                if kind == "scan":
                    text = f"Could not check for app updates: {error}"
                    self.updates_panel.apps.show_error(text)
                    self.set_status(text, theme.WARNING)
                else:
                    noun = "updates" if kind == "upgrade" else "installs"
                    self._show_error(f"Could not start the {noun}", error)
                    if self.section_visible("History"):
                        self.load_history()
                self._refresh_updates_actions()
                return
            assert isinstance(job, dict)
            self._follow_updates_job(kind, job, items or [])

        engine.start_updates(kind, items, callback=started)
        self._refresh_updates_actions()

    def _follow_updates_job(self, kind: str, job: dict[str, Any], items: list[dict[str, Any]]) -> None:
        job_id = _job_id(job)
        assert job_id is not None
        panel = self.updates_panel
        self._updates_job = job_id
        self._last_updates_job = job_id
        if kind in ("upgrade", "install"):
            self._last_batch_job = job_id
        self._updates_job_kind = kind
        self._updates_job_title = str(job.get("title") or "")
        self._updates_after = 0
        self._updates_more = False
        self._updates_polled = 0.0
        self._updates_result_rev = 0
        self._updates_result = None
        panel.job.begin(kind, self._updates_job_title or "Checking for app updates")
        panel.show_job(True)
        if kind == "scan":
            panel.apps.set_checking()
        else:
            queued = [{"id": i.get("id"), "state": "queued"} for i in items]
            if kind == "upgrade":
                panel.apps.set_items(queued, "upgrade")
            else:
                panel.install.set_items(queued)
            self.set_status(f"{self._updates_job_title}…")
            if self.section_visible("History"):
                self.load_history()
        self.set_job_status("updates", self._updates_status_text())
        self._refresh_updates_actions()

    def _updates_status_text(self) -> str:
        """The status bar text of the followed job ("" when none runs)."""
        kind = self._updates_job_kind
        if self._updates_job is None or kind is None:
            return ""
        if kind == "scan":
            return "◐ Checking for app updates…"
        verb = "Updating apps" if kind == "upgrade" else "Installing apps"
        result = self._updates_result or {}
        current = result.get("current")
        items = result.get("items") or []
        if isinstance(current, int) and 0 <= current < len(items):
            name = items[current].get("name") or items[current].get("id")
            return f"◐ {verb}: {current + 1} of {len(items)}  ·  {name}"
        return f"◐ {verb}…"

    def _poll_updates(self, now: float) -> None:
        """Runs every frame; reads the followed job's progress, output and result.

        The job is read at `UPDATES_POLL_HZ` while the section is shown and
        `UPDATES_POLL_HIDDEN_HZ` otherwise, and again on the next frame while the engine has
        more lines waiting. Output is inserted only while the section and the output box are
        shown, one bounded batch per frame.
        """
        panel = getattr(self, "updates_panel", None)
        if panel is None or self.engine is None or not self._updates_supported:
            return
        visible = self.section_visible(UPDATES_SECTION)
        job = self._updates_job
        if job is not None:
            period = 1.0 / (UPDATES_POLL_HZ if visible else UPDATES_POLL_HIDDEN_HZ)
            if self._updates_more or now - self._updates_polled >= period:
                self._updates_polled = now
                self._read_updates_job(job)
            if (
                visible
                and self._updates_job is not None
                and now - self._updates_animated >= UPDATES_ANIMATE_SECONDS
            ):
                self._updates_animated = now
                panel.job.animate(now)
        if visible and panel.job.output_shown and panel.job.pending_output:
            panel.job.flush()

    def _read_updates_job(self, job: int) -> None:
        assert self.engine is not None
        panel = self.updates_panel
        try:
            view = self.engine.updates_job(job, self._updates_after)
        except Exception:  # noqa: BLE001 - an unreadable job is reported as lost
            log.exception("reading winget job %s failed", job)
            view = None
        if view is None:
            self._updates_lost()
            return
        after = view.get("next")
        if isinstance(after, int):
            self._updates_after = after
        self._updates_more = bool(view.get("more"))
        panel.job.append(view.get("lines") or [], int(view.get("skipped") or 0))
        try:
            result = self.engine.updates_result(job, self._updates_result_rev)
        except Exception:  # noqa: BLE001 - the job's view still shows its progress
            log.exception("reading the result of winget job %s failed", job)
            result = None
        if result is not None:
            revision = result.get("revision")
            if isinstance(revision, int):
                self._updates_result_rev = revision
            self._updates_result = dict(result)
            self._show_updates_result(self._updates_result)
        self._show_item_progress(view)
        panel.job.update_job(view)
        if view.get("state", "running") == "running":
            if self._updates_job_kind == "scan":
                panel.apps.set_checking(view.get("elapsed_ms"))
            self.set_job_status("updates", self._updates_status_text())
        elif not self._updates_more:
            self._updates_finished(view)

    def _show_updates_result(self, result: dict[str, Any]) -> None:
        """Shows a batch's published item states in its view."""
        kind = self._updates_job_kind
        if kind not in ("upgrade", "install") or result.get("kind") != kind:
            return
        items = [dict(i) for i in result.get("items") or []]
        if kind == "upgrade":
            self.updates_panel.apps.set_items(items, "upgrade")
        else:
            self.updates_panel.install.set_items(items)

    def _show_item_progress(self, view: dict[str, Any]) -> None:
        """Adds what winget's display shows for the app that runs (its download sizes or a
        percentage, from the job's detail) to that app's status."""
        kind = self._updates_job_kind
        detail = view.get("detail")
        result = self._updates_result or {}
        if kind not in ("upgrade", "install") or not isinstance(detail, dict):
            return
        index = detail.get("item")
        items = result.get("items") or []
        if not isinstance(index, int) or result.get("current") != index or not 0 <= index < len(items):
            return
        item = dict(items[index])
        if item.get("state") != "running":
            return
        item["progress"] = detail.get("progress")
        if kind == "upgrade":
            self.updates_panel.apps.set_items([item], "upgrade")
        else:
            self.updates_panel.install.set_items([item])

    def _updates_finished(self, view: dict[str, Any]) -> None:
        kind = self._updates_job_kind
        state = view.get("state")
        result = self._updates_result
        self._updates_job = None
        self._updates_more = False
        self.set_job_status("updates", "")
        if kind == "scan":
            self._updates_scan_finished(state, view, result)
        else:
            self._updates_batch_finished(state, result)
        self._refresh_updates_actions()

    def _updates_scan_finished(self, state: Any, view: dict[str, Any], result: dict[str, Any] | None) -> None:
        panel = self.updates_panel
        if state == "cancelled":
            panel.job.finish("○ The check for app updates was stopped.", theme.INK_MUTED)
            panel.apps.show_error("The check was stopped.")
            return
        if result is None or result.get("kind") != "scan":
            text = str(view.get("summary") or view.get("hint") or "The check failed.")
            panel.job.finish(f"⚠ {text}", theme.WARNING)
            panel.apps.show_error(text)
            self.set_status(f"Could not check for app updates: {text}", theme.WARNING)
            return
        self._updates_scan = result
        panel.apps.show_scan(result, self._updates_last_results)
        if self._updates_apps is not None:
            panel.install.show_apps(self._updates_apps, self._updates_installed_ids())
        error = result.get("error")
        if error:
            message = str(error.get("message") or "The check failed.")
            panel.job.finish(f"⚠ {message}", theme.WARNING)
            self.set_status(f"Could not check for app updates: {message}", theme.WARNING)
        elif panel.job.result_text:
            # The check that follows a batch keeps the batch's result line in view.
            panel.job.finish(None)
        else:
            panel.show_job(False)

    def _updates_batch_finished(self, state: Any, result: dict[str, Any] | None) -> None:
        panel = self.updates_panel
        items = [dict(i) for i in (result or {}).get("items") or []]
        for item in items:
            self._updates_last_results[str(item.get("id", "")).lower()] = item
        line = batch_line(result) if result else ""
        failed = any(i.get("state") in ("failed", "timed_out", "not_started") for i in items)
        color = theme.WARNING if failed or state in ("failed", "attention") else theme.GOOD
        if state == "cancelled":
            color = theme.INK_SECONDARY
        panel.job.finish(line or "The job ended without a result.", color)
        if result is not None:
            self._show_updates_result(result)
        self.set_status(f"Done: {line}" if line else "The apps job ended.", color)
        self._refresh_journal()
        if self.section_visible("History"):
            self.load_history()
        # A check follows every batch, so the list shows what is left.
        self._updates_scan_started = False
        if self._updates_ready():
            self._start_updates_scan()

    def _updates_lost(self) -> None:
        """Stops following the job and says so once."""
        self._updates_job = None
        self._updates_more = False
        self.set_job_status("updates", "")
        self.set_status(LOST_TEXT, theme.CRITICAL)
        try:
            self.updates_panel.job.lose(LOST_TEXT)
        except Exception:  # noqa: BLE001 - the status bar already reports the lost job
            log.exception("could not show the lost winget job")
        self._refresh_updates_actions()

    def _updates_poll_failed(self) -> None:
        """Runs once after `_poll_updates` raised; the window stops polling this lane."""
        if self._updates_job is not None:
            self._updates_lost()
            return
        self._updates_more = False
        self.set_job_status("updates", "")

    def _updates_running_title(self) -> str | None:
        """ "App updates" or "App installs" while a batch runs; None otherwise (a check too)."""
        if self._updates_job is None or self._updates_job_kind not in ("upgrade", "install"):
            return None
        return "App updates" if self._updates_job_kind == "upgrade" else "App installs"

    # -- updating and installing -------------------------------------------------------

    def _on_updates_update(self, everything: bool) -> None:
        """Update selected / Update all: plans the batch, asks, then starts it."""
        if self.engine is None:
            return
        view = self.updates_panel.apps
        items = view.all_items() if everything else view.selected_items()
        if not items:
            self.set_status(SELECT_UPDATES_TEXT, theme.WARNING)
            return
        if len(items) > MAX_BATCH_ITEMS:
            self.set_status(TOO_MANY_TEXT.format(limit=MAX_BATCH_ITEMS), theme.WARNING)
            return
        if not self._guard(needs_admin=True):
            return
        if self._updates_job is not None or self._updates_starting is not None:
            self.set_status(BATCH_RUNNING_TEXT, theme.WARNING)
            return
        self._plan_updates("upgrade", items)

    def _on_updates_install(self) -> None:
        """Install selected: apps known to be installed are never sent."""
        if self.engine is None:
            return
        view = self.updates_panel.install
        items = view.selected_items()
        if not items:
            ticked = view.selection_count > 0
            self.set_status(ALL_INSTALLED_TEXT if ticked else SELECT_INSTALLS_TEXT, theme.INK_SECONDARY)
            return
        if len(items) > MAX_BATCH_ITEMS:
            self.set_status(TOO_MANY_TEXT.format(limit=MAX_BATCH_ITEMS), theme.WARNING)
            return
        if not self._guard(needs_admin=True):
            return
        if self._updates_job is not None or self._updates_starting is not None:
            self.set_status(BATCH_RUNNING_TEXT, theme.WARNING)
            return
        self._plan_updates("install", items)

    def _plan_updates(self, kind: str, items: list[dict[str, Any]]) -> None:
        assert self.engine is not None
        noun = "updates" if kind == "upgrade" else "installs"
        self._set_busy(True, "read")
        self.set_status(f"Checking the {noun}…")

        def planned(future: Future[Any]) -> None:
            self._set_busy(False)
            exc = future.exception()
            if exc is not None:
                self._show_error(f"Could not prepare the {noun}", exc)
                return
            plan = (future.result() or {}).get("plan") or {}
            blocked = plan.get("blocked_reason")
            if blocked:
                self.set_status(str(blocked), theme.WARNING)
                return
            self._confirm_updates(kind, items, plan)

        self.engine.plan_updates(kind, items, callback=planned)

    def _confirm_updates(self, kind: str, items: list[dict[str, Any]], plan: dict[str, Any]) -> None:
        shown = [dict(i) for i in plan.get("items") or items]
        count = len(shown)
        if kind == "upgrade":
            details = [
                f"{i.get('name') or i.get('id')}  ·  {version_change(i.get('from'), i.get('to'))}"
                for i in shown
            ]
            title, verb, message = f"Update {apps_text(count)}?", "Update", UPDATE_CONFIRM_MESSAGE
            acknowledge = f"I understand these updates can't be undone by {APP_NAME}"
        else:
            details = [f"{i.get('name') or i.get('id')}  ·  {i.get('id')}" for i in shown]
            title, verb, message = f"Install {apps_text(count)}?", "Install", INSTALL_CONFIRM_MESSAGE
            acknowledge = f"I understand these installs can't be undone by {APP_NAME}"
        details += [f"• {note}" for note in plan.get("notes") or []]
        details += [AGREEMENTS_NOTE, RESTORE_POINT_TIP]
        self.set_status(f"{plan.get('title') or title} is ready to start.")
        MessageDialog(
            self,
            title=title,
            message=message,
            details=details,
            confirm_text=f"{verb} {apps_text(count)}",
            cancel_text="Cancel",
            danger=True,
            acknowledge=acknowledge,
            on_confirm=lambda: self._start_updates_batch(kind, items),
            on_cancel=lambda: self.set_status(NOTHING_STARTED_TEXT),
        )

    def _start_updates_batch(self, kind: str, items: list[dict[str, Any]]) -> None:
        if self.engine is None:
            return
        action = "Starting the app updates" if kind == "upgrade" else "Starting the app installs"
        self._set_busy(True, "irreversible", action=action)
        self.set_status(f"{action}…")
        self._start_updates_job(kind, items)

    def _on_updates_stop(self) -> None:
        """Stops a check now, or a batch after the app that runs."""
        job = self._updates_job
        if job is None or self.engine is None:
            return
        try:
            self.engine.cancel_updates(job)
        except Exception as exc:  # noqa: BLE001 - shown to the user; the job keeps being polled
            self._show_error("Could not stop the job", exc)
            return
        text = "Stopping…"
        result = self._updates_result or {}
        current = result.get("current")
        items = result.get("items") or []
        if self._updates_job_kind != "scan" and isinstance(current, int) and 0 <= current < len(items):
            text = STOPPING_TEXT.format(name=items[current].get("name") or items[current].get("id"))
        self.updates_panel.job.set_stopping(text)
        self.set_status(text)

    def _on_updates_output(self, shown: bool) -> None:
        if shown and self.updates_panel.job.pending_output:
            self.updates_panel.job.flush()

    def _on_updates_open_log(self) -> None:
        """Opens the log of the batch whose result line the job strip shows, also while the
        check that follows the batch runs; otherwise of the followed job, or of the last one."""
        if self.updates_panel.job.shows_batch_result and self._last_batch_job is not None:
            job: int | None = self._last_batch_job
        elif self._updates_job is not None:
            job = self._updates_job
        else:
            job = self._last_updates_job
        if self.engine is None or job is None:
            return

        def done(future: Future[Any]) -> None:
            if future.exception() is not None:
                self._show_error("Could not open the log", future.exception())

        self.engine.open_updates_log(job, callback=done)

    def _on_updates_save_list(self, apps: list[dict[str, Any]] | None) -> None:
        """Saves the edited install list (None: back to the default list)."""
        if self.engine is None:
            return

        def done(future: Future[Any]) -> None:
            exc = future.exception()
            if exc is not None:
                self.updates_panel.install.show_error(str(exc))
                return
            self._updates_apps = dict(future.result() or {})
            self.updates_panel.install.show_apps(self._updates_apps, self._updates_installed_ids())
            self.set_status(
                "The default install list is back." if apps is None else "The install list was saved."
            )

        self.engine.save_app_list(apps, callback=done)

    def _on_updates_open_uri(self, uri: str) -> None:
        try:
            system.open_uri(uri)
        except Exception as exc:  # noqa: BLE001 - shown to the user
            self._show_error("Could not open the link", exc)

    # -- Windows Update ----------------------------------------------------------------

    def _wu_setting(self, setting: str) -> dict[str, Any]:
        for entry in (self._updates_wu or {}).get("settings") or []:
            if entry.get("id") == setting:
                return dict(entry)
        return {}

    def _wu_title(self, setting: str) -> str:
        """The setting's History title ("Windows Update: pause")."""
        if self.engine is not None:
            for entry in self.engine.updates_wu_catalog():
                if entry.get("id") == f"wu.{setting}":
                    return str(entry.get("title"))
        return f"Windows Update: {SETTING_TITLES.get(setting, setting).lower()}"

    def _on_wu_set(self, setting: str, value: Any) -> None:
        """A control of the Windows Update view changed. Clearing a setting while a change Cairn
        made to it is in effect is its Undo. When the setting is back at the recorded original,
        Undo would restore what is already there, so clearing it is a change of its own. A
        delay of feature updates on Pro and higher is confirmed first."""
        if self.engine is None:
            return
        current = self._wu_setting(setting)
        clearing = value is None or value is False
        if clearing and current.get("by_cairn") and current.get("differs"):
            self._on_wu_undo(setting)
            return
        if not self._guard(needs_admin=True):
            self.updates_panel.windows.refresh()
            return
        home = bool(((self._updates_wu or {}).get("edition") or {}).get("home"))
        if setting == "defer_feature" and value and not home:
            MessageDialog(
                self,
                title=f"Delay feature updates by {value} days?",
                message=DEFER_CONFIRM_TEXT.format(days=value),
                confirm_text="Delay",
                cancel_text="Cancel",
                on_confirm=lambda: self._apply_wu(setting, value),
                on_cancel=self.updates_panel.windows.refresh,
            )
            return
        self._apply_wu(setting, value)

    def _apply_wu(self, setting: str, value: Any) -> None:
        engine = self.engine
        if engine is None:
            return
        title = self._wu_title(setting)
        self._set_busy(True, "journaled", action="The Windows Update change")
        self.set_status(f"Changing {title}…")

        def done(future: Future[Any]) -> None:
            self._set_busy(False)
            exc = future.exception()
            if exc is not None:
                self._show_error("Could not change Windows Update", exc)
                self.updates_panel.windows.refresh()
            else:
                self.set_status(f"✓ {title} changed", theme.GOOD)
            self._load_wu_state()
            self._refresh_journal()
            if self.section_visible("History"):
                self.load_history()

        engine.set_wu_setting(setting, value, callback=done)

    def _on_wu_undo(self, setting: str) -> None:
        """Undo: restores the values recorded before Cairn first changed the setting."""
        targets = self._wu_setting(setting).get("targets") or []
        if self.engine is None or not targets:
            return
        if not self._guard(needs_admin=True):
            self.updates_panel.windows.refresh()
            return
        title = self._wu_title(setting)
        self._set_busy(True, "journaled", action=f"Undoing {title}")
        self.set_status(f"Undoing {title}…")
        self.engine.revert_targets({"registry": [dict(t) for t in targets]}, callback=self._revert_done)

    # -- closing -----------------------------------------------------------------------

    def _updates_allow_close(self) -> bool:
        """Whether the window may close now as far as winget jobs are concerned.

        A check is stopped on close without asking. With a batch running, a dialog says what
        closing does to the app being updated or installed and False is returned; confirming
        it closes the window.
        """
        if self.engine is None:
            return True
        try:
            jobs = self.engine.updates_jobs()
        except Exception:  # noqa: BLE001 - an unreadable job list must not keep the window open
            log.exception("reading the winget jobs failed")
            jobs = []
        running = next((j for j in jobs if j.get("state") == "running"), None)
        if running is None or running.get("kind") == "winget_scan":
            return True
        installing = running.get("kind") == "winget_install"
        verb = "installing" if installing else "updating"
        title = "Apps are still being installed" if installing else "Apps are still being updated"
        result = self._updates_result if self._updates_job == running.get("id") else None
        items = (result or {}).get("items") or []
        current = (result or {}).get("current")
        danger = False
        if isinstance(current, int) and 0 <= current < len(items):
            item = items[current]
            name = str(item.get("name") or item.get("id"))
            index, total = current + 1, len(items)
            lead = f"{APP_NAME} is {verb} {name} ({index} of {total})."
            if item.get("detached", True):
                rest = total - index
                others = f", the other {rest} aren't started," if rest else ""
                message = (
                    f"{lead} If you close now, {name} finishes on its own{others} and {name}'s result won't "
                    "appear in the Activity log."
                )
            else:
                message = (
                    f"{lead} Closing {APP_NAME} now can stop {name}'s installer midway and leave the app "
                    "unusable until you reinstall it."
                )
                danger = True
        else:
            message = (
                f"{APP_NAME} is {verb} apps. If you close now, the apps that haven't started aren't started."
            )
        # The engine call running now, if any, was already confirmed in the busy dialog.
        accepted = self._running_close() if self._busy or self.engine.busy else None
        MessageDialog(
            self,
            title=title,
            message=message,
            confirm_text="Close anyway",
            cancel_text="Keep running",
            danger=danger,
            on_confirm=lambda: self._updates_close_confirmed(accepted),
        )
        return False

    def _updates_close_confirmed(self, accepted: Any) -> None:
        """Continues closing after the updates dialog was confirmed. An engine call other than a
        read that started while the dialog was open, and was not the one already accepted, is
        asked about in the busy dialog first."""
        if self.engine is None:
            self._continue_close("updates", busy_unchanged=True)
            return
        running = self._running_close() if self._busy or self.engine.busy else None
        unchanged = running is None or running.kind == "read" or running == accepted
        self._continue_close("updates", busy_unchanged=unchanged)

    def _updates_before_shutdown(self) -> None:
        """Runs on every close path just before the engine shuts down: stops a check and ends a
        batch after the app that runs (which finishes on its own). The window is hidden first,
        since the engine may wait here for a few seconds."""
        if self.engine is None:
            return
        try:
            if self.state() != "withdrawn":
                self.withdraw()
                self.update_idletasks()
        except tk.TclError:
            pass
        try:
            outcomes = self.engine.updates_shutdown()
        except Exception:  # noqa: BLE001 - closing continues whatever the lane reports
            log.exception("stopping the winget jobs failed")
            return
        for outcome in outcomes:
            log.info("winget job %s (%s): %s", outcome.get("id"), outcome.get("kind"), outcome.get("action"))
        self._updates_job = None
