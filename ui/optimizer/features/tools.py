"""Tools section: Windows maintenance tools run as engine-owned background jobs, polled from
the frame loop (job lane "tools"), plus quick actions and a launcher for the built-in Windows
tools.

A job runs inside the engine, not on the bridge worker, so the rest of the window stays
usable while it runs. Starting one is queued on the worker like any other call; after that
the frame loop reads the job 10 times a second while the section is shown and twice a second
otherwise. Output read while the section is hidden is buffered and inserted a bounded number
of lines per frame once the section is shown. The status bar shows the job in every section.
Tool runs are only audit-logged; nothing they change is journaled.
"""

from __future__ import annotations

import logging
from collections.abc import Callable
from concurrent.futures import Future
from typing import TYPE_CHECKING, Any

from .. import theme
from ..widgets.dialogs import MessageDialog
from ..widgets.tools import (
    RESTART_SENTENCE,
    ToolsPanel,
    duration_text,
    fmt_duration,
    sentence,
    tool_status_text,
)

if TYPE_CHECKING:
    import customtkinter as ctk

    from ..bridge.engine import EngineBridge

log = logging.getLogger(__name__)

TOOL_POLL_HZ = 10.0
TOOL_POLL_HIDDEN_HZ = 2.0

UNSUPPORTED_TEXT = "The engine is out of date: rebuild and deploy it to use this section."
ALREADY_RUNNING_TEXT = "A maintenance tool is already running; wait for it to finish or stop it."
FINISHING_TEXT = (
    "It has finished, and Cairn is reading its result, which takes a few seconds. Closing now waits for it."
)
SYSTEM_PROTECTION_TOOL = "system_protection"


def confirm_message(tool: dict[str, Any]) -> str:
    """What running `tool` does, for its confirmation dialog."""
    if tool.get("requires_detach"):
        return (
            "Windows repairs its own files. The repairs can't be undone from History, and the tool can't "
            "be stopped safely once it starts; it keeps running if you close Cairn."
        )
    if tool.get("changes_system"):
        return (
            "Windows optimizes the drive. It runs to completion and Cairn can't stop it; on a hard "
            "disk this can take hours. You can keep using the PC."
        )
    if tool.get("cancellable"):
        return "This only checks; nothing is changed. You can stop it at any time."
    return "This only checks; nothing is changed. It runs inside Windows and can't be stopped once it starts."


def lost_text(title: str) -> str:
    return f"Lost track of {title}; it may still be running. Its log is in the tools folder."


class ToolsFeature:
    """State, widgets and flows of the Tools section, mixed into `App`.

    `_tool_job` is the id of the job being followed (None when no tool runs) and
    `_tool_title` its tool's title. `_running_tool_title` (the lane's running title) is the
    only view other features have of them. `_last_tool_job` stays set after the job
    finishes, for its log.
    """

    _tool_job: int | None
    _tool_title: str
    _tool_after: int
    _tool_polled: float
    _last_tool_job: int | None
    tools_panel: ToolsPanel

    if TYPE_CHECKING:
        engine: EngineBridge | None
        elevated: bool
        errors: list[str]
        _busy: bool
        _tool_poll_failed: bool

        def section_visible(self, name: str) -> bool: ...
        def set_job_status(self, lane: str, text: str) -> None: ...
        def set_status(self, text: str, color: str = ...) -> None: ...
        def withdraw(self) -> None: ...
        def update_idletasks(self) -> None: ...
        def _guard(self, *, needs_admin: bool = True) -> bool: ...
        def _set_busy(self, busy: bool, close: str = ..., *, action: str = ..., note: str = ...) -> None: ...
        def _show_error(self, title: str, exc: BaseException | None) -> None: ...
        def _restart_explorer(self) -> None: ...
        def load_history(self) -> None: ...
        def _continue_close(self, lane: str, *, busy_unchanged: bool) -> None: ...
        def _running_close(self) -> Any: ...

    def _init_tools_state(self) -> None:
        """Sets the section's state; runs before the window is built."""
        self._tool_job = None
        self._tool_title = ""
        self._tool_after = 0
        self._tool_polled = 0.0
        self._last_tool_job = None
        # The last view said more output lines are waiting, so the next frame polls again.
        self._tool_more = False
        self._tool_cancellable = False
        # Title of the tool whose start is queued on the worker.
        self._tool_starting = ""
        self._tools_loading = False
        self._tools_supported = True
        self._tool_catalog: list[dict[str, Any]] = []
        self._windows_tool_list: list[dict[str, Any]] = []

    # -- building and loading ----------------------------------------------------------

    def _build_tools_tab(self, frame: ctk.CTkFrame) -> None:
        """Builds the section's panel into `frame`; handles a missing engine itself."""
        self.tools_panel = ToolsPanel(
            frame,
            on_refresh=self.load_tools,
            on_run=self._on_run_tool,
            on_stop=self._on_stop_tool,
            on_open_log=self._on_open_tool_log,
            on_restore_point=self._on_restore_point,
            on_restart_explorer=self._on_tools_restart_explorer,
            on_open_windows_tool=self._on_open_windows_tool,
        )
        self.tools_panel.grid(row=0, column=0, sticky="nsew", padx=4, pady=4)
        if self.engine is None:
            self._tools_supported = False
            self.tools_panel.set_engine_ready(False)
        elif not self.engine.supports("tools_catalog"):
            self._tools_supported = False
            self.tools_panel.set_unsupported(UNSUPPORTED_TEXT)

    def _tools_tab_shown(self) -> None:
        """Runs when the Tools section is shown while the engine is loaded."""
        if not self._tools_supported:
            return
        if not self.tools_panel.loaded and not self._tools_loading:
            self.load_tools()
        # The next frame reads the job at once instead of waiting for the hidden-section period.
        self._tool_polled = 0.0

    def load_tools(self) -> None:
        """Reads the tool catalog, the fixed drives, System Protection and the Windows tools.

        The four calls run in order on the worker; whichever callback completes the set shows
        them all. A failed System Protection read shows its state as unknown.
        """
        engine = self.engine
        if engine is None or not self._tools_supported or self._tools_loading:
            return
        self._tools_loading = True
        self.tools_panel.set_loading()
        results: dict[str, Future[Any]] = {}
        names = ("catalog", "volumes", "restore", "windows")

        def keep(name: str) -> Callable[[Future[Any]], None]:
            def done(future: Future[Any]) -> None:
                results[name] = future
                if len(results) == len(names):
                    self._tools_loaded(results)

            return done

        engine.tools_catalog(callback=keep("catalog"))
        engine.tools_volumes(callback=keep("volumes"))
        engine.system_restore_enabled(callback=keep("restore"))
        engine.windows_tools(callback=keep("windows"))

    def _tools_loaded(self, results: dict[str, Future[Any]]) -> None:
        """Shows the four reads of `load_tools` once all of them have finished."""
        self._tools_loading = False
        for name in ("catalog", "volumes", "windows"):
            exc = results[name].exception()
            if exc is not None:
                self.tools_panel.show_error(str(exc))
                return
        restore = results["restore"]
        restore_enabled = None if restore.exception() is not None else bool(restore.result())
        self._tool_catalog = [dict(t) for t in results["catalog"].result()]
        self._windows_tool_list = [dict(t) for t in results["windows"].result()]
        self.tools_panel.show(
            self._tool_catalog,
            [dict(v) for v in results["volumes"].result()],
            restore_enabled,
            self._windows_tool_list,
            engine_ready=True,
            elevated=self.elevated,
        )

    def _refresh_tool_actions(self) -> None:
        """Runs whenever the window's busy state changes, to enable or disable the tools."""
        panel = getattr(self, "tools_panel", None)
        if panel is not None:
            panel.set_actions_enabled(not self._busy)

    def _tool_info(self, tool: dict[str, Any] | str) -> dict[str, Any]:
        if isinstance(tool, dict):
            return tool
        found = next((t for t in self._tool_catalog if t.get("id") == tool), None)
        return found if found is not None else {"id": tool, "title": tool}

    # -- running a tool ------------------------------------------------------------------

    def _on_run_tool(self, tool: dict[str, Any] | str, volume: str | None = None) -> None:
        """Plans the tool (a dry run), explains a block or asks for confirmation, then starts it."""
        info = self._tool_info(tool)
        title = str(info.get("title") or info["id"])
        if not self._guard(needs_admin=True):
            return
        if self._tool_job is not None:
            self.set_status(ALREADY_RUNNING_TEXT, theme.WARNING)
            return
        assert self.engine is not None
        self._set_busy(True, "read")
        self.set_status(f"Checking whether {title} can run…")

        def planned(future: Future[Any]) -> None:
            self._set_busy(False)
            exc = future.exception()
            if exc is not None:
                self._show_error(f"Could not prepare {title}", exc)
                return
            plan = (future.result() or {}).get("plan") or {}
            blocked = plan.get("blocked_reason")
            if blocked:
                self.set_status(f"{title} can't run now: {blocked}", theme.WARNING)
                MessageDialog(
                    self,
                    title=f"{title} can't run now",
                    message=sentence(str(blocked)),
                    details=[f"• {note}" for note in plan.get("notes") or []],
                )
                return
            self._confirm_tool(info, plan, volume)

        self.engine.plan_tool(str(info["id"]), volume, callback=planned)

    def _confirm_tool(self, info: dict[str, Any], plan: dict[str, Any], volume: str | None) -> None:
        title = str(info.get("title") or info["id"])
        facts = {**info, **{k: v for k, v in plan.items() if v is not None}}
        self.set_status(f"{title} is ready to run.")
        details = [f"Command: {plan.get('command_line') or info.get('program') or info['id']}"]
        duration = duration_text(str(facts.get("duration_hint") or ""), "Usually takes")
        if duration:
            details.append(duration)
        details += [f"• {note}" for note in plan.get("notes") or []]
        MessageDialog(
            self,
            title=f"Run {title}?",
            message=confirm_message(facts),
            details=details,
            confirm_text=str(info.get("verb") or "Run"),
            cancel_text="Cancel",
            danger=bool(facts.get("requires_detach")),
            on_confirm=lambda: self._start_tool(info, volume),
            on_cancel=lambda: self.set_status(f"{title} was not started."),
        )

    def _start_tool(self, info: dict[str, Any], volume: str | None) -> None:
        if self.engine is None:
            return
        title = str(info.get("title") or info["id"])
        self._tool_starting = title
        self._set_busy(True)
        self.set_status(f"Starting {title}…")
        self.engine.start_tool(str(info["id"]), volume, callback=self._tool_started)

    def _tool_started(self, future: Future[Any]) -> None:
        self._set_busy(False)
        title = self._tool_starting or "the tool"
        self._tool_starting = ""
        exc = future.exception()
        job = None if exc is not None else (future.result() or {}).get("job")
        job_id = job.get("id") if job else None
        if exc is not None or not job or not isinstance(job_id, int):
            self._show_error(f"Could not start {title}", exc or RuntimeError("the engine started no job"))
            if self.section_visible("History"):
                self.load_history()
            return
        self._tool_job = job_id
        self._last_tool_job = self._tool_job
        self._tool_title = str(job.get("title") or title)
        self._tool_after = 0
        self._tool_polled = 0.0
        self._tool_more = False
        self._tool_cancellable = bool(job.get("cancellable"))
        self._tool_poll_failed = False
        self.tools_panel.begin_job(job)
        self.set_job_status("tools", tool_status_text(self._tool_title, job.get("progress")))
        self.set_status(f"Running {self._tool_title}…")
        if self.section_visible("History"):
            self.load_history()

    # -- polling -------------------------------------------------------------------------

    def _poll_tools(self, now: float) -> None:
        """Runs every frame; reads the followed job's progress and output.

        The job is read at `TOOL_POLL_HZ` while the section is shown and `TOOL_POLL_HIDDEN_HZ`
        otherwise, and again on the next frame while the engine has more lines waiting.
        Buffered output is inserted only while the section is shown, one bounded batch per frame.
        """
        panel = getattr(self, "tools_panel", None)
        if panel is None or self.engine is None:
            return
        visible = self.section_visible("Tools")
        polled = False
        job = self._tool_job
        if job is not None:
            period = 1.0 / (TOOL_POLL_HZ if visible else TOOL_POLL_HIDDEN_HZ)
            if self._tool_more or now - self._tool_polled >= period:
                self._tool_polled = now
                polled = True
                try:
                    view = self.engine.tool_job(job, self._tool_after)
                except Exception:  # noqa: BLE001 - an unreadable job is reported as lost
                    log.exception("reading tool job %s failed", job)
                    view = None
                if view is None:
                    self._lose_tool()
                    return
                after = view.get("next")
                if isinstance(after, int):
                    self._tool_after = after
                self._tool_more = bool(view.get("more"))
                panel.update_job(view, visible=visible)
                state = view.get("state", "running")
                if state == "running":
                    self.set_job_status("tools", tool_status_text(self._tool_title, view.get("progress")))
                elif not self._tool_more:
                    self._tool_finished(view)
        if visible and not polled and panel.pending_output:
            panel.flush()

    def _lose_tool(self) -> None:
        """Stops following the job and says so once."""
        title = self._tool_title or "the tool"
        self._tool_job = None
        self._tool_more = False
        self.set_job_status("tools", "")
        message = lost_text(title)
        self.set_status(message, theme.CRITICAL)
        try:
            self.tools_panel.lose_job(message)
        except Exception:  # noqa: BLE001 - the status bar already reports the lost job
            log.exception("could not show the lost tool job")

    def _tools_poll_failed(self) -> None:
        """Runs once after `_poll_tools` raised; polling stops until a new job starts."""
        if self._tool_job is not None:
            self._lose_tool()
            return
        self._tool_more = False
        self.set_job_status("tools", "")

    def _tool_finished(self, view: dict[str, Any]) -> None:
        title = self._tool_title or str(view.get("title") or "The tool")
        self._tool_job = None
        self._tool_more = False
        job_id = view.get("id")
        if isinstance(job_id, int):
            self._last_tool_job = job_id
        self.set_job_status("tools", "")
        self.tools_panel.finish_job(view)
        state = view.get("state")
        hint = str(view.get("hint") or "")
        summary = str(view.get("summary") or "")
        restart = bool(view.get("restart_required"))
        if state == "succeeded":
            text = f"Done: {title} finished in {fmt_duration(float(view.get('elapsed_ms') or 0))}."
            color = theme.GOOD
        elif state == "completed":
            text, color = f"Done: {title} finished; read its result in the output.", theme.INK_SECONDARY
        elif state == "attention":
            text, color = f"{title} reported problems; see its output.", theme.WARNING
        elif state == "cancelled":
            text, color = f"{title} stopped.", theme.INK_SECONDARY
        else:
            text, color = f"{title} failed: {hint or summary or 'see its output'}", theme.CRITICAL
        if restart:
            text += f" {RESTART_SENTENCE}"
        self.set_status(text, color)
        if state in ("attention", "failed") or restart:
            heading = {"attention": f"{title} needs attention", "failed": f"{title} failed"}.get(
                str(state), f"{title} finished"
            )
            parts = [p for p in (hint, summary) if p]
            if restart:
                parts.append(RESTART_SENTENCE)
            MessageDialog(
                self,
                title=heading,
                message=" ".join(parts) or "The tool's output has the details.",
                links=[("Open log", self._on_open_tool_log)],
            )
        if self.section_visible("History"):
            self.load_history()

    def _running_tool_title(self) -> str | None:
        """Title of the tool that is running, or None when no tool runs."""
        return self._tool_title if self._tool_job is not None else None

    # -- stop and log --------------------------------------------------------------------

    def _on_stop_tool(self) -> None:
        """Asks before stopping the followed job; only stoppable jobs offer this."""
        job = self._tool_job
        if job is None or not self._tool_cancellable or self.engine is None:
            return
        title = self._tool_title

        def stop() -> None:
            if self._tool_job != job or self.engine is None:
                return
            try:
                self.engine.cancel_tool(job)
            except Exception as exc:  # noqa: BLE001 - shown to the user; the job keeps being polled
                self._show_error(f"Could not stop {title}", exc)
                return
            self.tools_panel.set_stopping()
            self.set_status(f"Stopping {title}…")

        MessageDialog(
            self,
            title=f"Stop {title}?",
            message="Nothing has been changed by the check.",
            confirm_text="Stop",
            cancel_text="Keep running",
            danger=True,
            on_confirm=stop,
        )

    def _on_open_tool_log(self) -> None:
        """Opens the readable log of the followed job, or of the last one."""
        job = self._tool_job if self._tool_job is not None else self._last_tool_job
        if self.engine is None or job is None:
            return

        def done(future: Future[Any]) -> None:
            if future.exception() is not None:
                self._show_error("Could not open the log", future.exception())

        self.engine.open_tool_log(job, callback=done)

    # -- closing -------------------------------------------------------------------------

    def _tools_allow_close(self) -> bool:
        """Whether the window may close now as far as tool jobs are concerned.

        With a running job, a dialog says what closing does to it and False is returned;
        confirming the dialog closes the window.
        """
        if self.engine is None:
            return True
        try:
            jobs = self.engine.tool_jobs()
        except Exception:  # noqa: BLE001 - an unreadable job list must not keep the window open
            log.exception("reading the tool jobs failed")
            jobs = []
        running = next((j for j in jobs if j.get("state") == "running"), None)
        if running is None:
            return True
        title = str(running.get("title") or "A maintenance tool")
        heading = f"{title} is still running"
        cancel = "Keep running"
        if running.get("exit_code") is not None:
            # The process has ended; the engine is reading its result.
            heading = f"{title} is finishing"
            message = FINISHING_TEXT
            confirm, cancel, danger = "Close", "Cancel", False
        elif running.get("cancellable"):
            message = "Closing stops it. Nothing has been changed by the check."
            confirm, danger = "Stop and close", True
        elif running.get("detached"):
            raw_log = running.get("raw_log_path") or "the tools folder"
            message = (
                "It can't be stopped safely, so it keeps running after Cairn closes and finishes "
                f"on its own. Its raw output is saved to {raw_log}. The result won't appear in the "
                "Activity log."
            )
            confirm, danger = "Close anyway", False
        else:
            message = (
                "Closing Cairn now can end it before it finishes. Nothing is damaged, but you'd have "
                "to run it again. Keep Cairn open until it finishes."
            )
            confirm, danger = "Close anyway", True
        # The engine call running now, if any, was already confirmed in the busy dialog.
        accepted = self._running_close() if self._busy or self.engine.busy else None
        MessageDialog(
            self,
            title=heading,
            message=message,
            confirm_text=confirm,
            cancel_text=cancel,
            danger=danger,
            on_confirm=lambda: self._tools_close_confirmed(accepted),
        )
        return False

    def _tools_close_confirmed(self, accepted: Any) -> None:
        """Continues closing after the tools dialog was confirmed. An engine call other than a
        read that started while the dialog was open, and was not the one already accepted, is
        asked about in the busy dialog first."""
        if self.engine is None:
            self._continue_close("tools", busy_unchanged=True)
            return
        running = self._running_close() if self._busy or self.engine.busy else None
        unchanged = running is None or running.kind == "read" or running == accepted
        self._continue_close("tools", busy_unchanged=unchanged)

    def _tools_before_shutdown(self) -> None:
        """Runs on every close path just before the engine shuts down: waits for the result of a
        job whose process has ended, stops a stoppable job and records the others as left running.

        The engine can wait on this thread for up to 10 s for a finishing job, and a few seconds
        for a start or a stop to settle, so the window is hidden first: a window that stops
        responding invites ending the process while a tool is being finished.
        """
        if self.engine is None:
            return
        self.withdraw()
        self.update_idletasks()
        try:
            outcomes = self.engine.tools_shutdown()
        except Exception:  # noqa: BLE001 - closing continues whatever the tools report
            log.exception("stopping the maintenance tools failed")
            return
        for outcome in outcomes:
            log.info("tool job %s (%s): %s", outcome.get("id"), outcome.get("tool"), outcome.get("action"))
        self._tool_job = None

    # -- quick actions and Windows tools ---------------------------------------------------

    def _on_restore_point(self) -> None:
        """Creates a System Restore point now; no dialog, since it only adds a checkpoint."""
        if not self._guard():
            return
        assert self.engine is not None
        self._set_busy(True)
        self.set_status("Creating a restore point…")

        def done(future: Future[Any]) -> None:
            self._set_busy(False)
            exc = future.exception()
            if exc is not None:
                title = "The restore point was not created"
                self.set_status(f"{title}: {exc}", theme.CRITICAL)
                MessageDialog(
                    self,
                    title=title,
                    message=str(exc),
                    links=[
                        (
                            "Open System Protection",
                            lambda: self._on_open_windows_tool(SYSTEM_PROTECTION_TOOL),
                        )
                    ],
                )
                return
            point = future.result() or {}
            self.set_status(f"Restore point #{point.get('sequence')} created.", theme.GOOD)

        self.engine.create_restore_point(callback=done)

    def _on_tools_restart_explorer(self) -> None:
        if not self._guard(needs_admin=False):
            return
        MessageDialog(
            self,
            title="Restart File Explorer?",
            message="Open File Explorer windows close and the taskbar disappears for a few seconds.",
            confirm_text="Restart File Explorer",
            cancel_text="Cancel",
            on_confirm=self._restart_explorer,
        )

    def _on_open_windows_tool(self, tool_id: str) -> None:
        """Opens a built-in Windows tool through the engine, which starts it by its full path."""
        if self.engine is None:
            return
        info = next((t for t in self._windows_tool_list if t.get("id") == tool_id), {})
        title = str(info.get("title") or tool_id.replace("_", " ").capitalize())
        if info.get("requires_admin") and not self.elevated:
            self.set_status(
                f"{title} needs administrator rights; restart as administrator to open it.", theme.WARNING
            )
            return

        def done(future: Future[Any]) -> None:
            if future.exception() is not None:
                self._show_error(f"Could not open {title}", future.exception())
            else:
                self.set_status(f"Opened {title}.")

        self.engine.open_windows_tool(tool_id, callback=done)
