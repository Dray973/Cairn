"""Storage section: disk speed test, space analyzer and duplicate finder.

The speed test needs administrator rights and is audit-logged (a "started" row before the test
folder exists, one final row after the test file is gone). The space scan and the duplicate
search only read. Nothing is journaled and no user file is deleted: rows offer "Show in
Explorer", where the user deletes through Explorer's own Recycle Bin.

The work runs as engine-owned jobs of the "storage" job lane, one at a time. Starting one is
queued on the bridge worker like any other call; after that the frame loop reads the job 10
times a second while the section is shown and twice a second otherwise, and the status bar
shows it in every section. A storage job never holds the window's busy state, so every other
section stays usable while it runs.
"""

from __future__ import annotations

import logging
from collections.abc import Callable
from concurrent.futures import Future
from typing import TYPE_CHECKING, Any

from .. import APP_NAME, system, theme
from ..widgets.dialogs import MessageDialog
from ..widgets.storage import (
    KIND_DUPLICATES,
    KIND_SCAN,
    KIND_SPEED,
    NO_SCAN_TEXT,
    RESULTS_GONE_TEXT,
    StoragePanel,
    count_text,
    estimate_text,
    is_hdd,
    job_target,
    max_write_bytes,
    storage_status_text,
)
from ..widgets.tools import fmt_duration, sentence, size_text

if TYPE_CHECKING:
    import customtkinter as ctk

    from ..bridge.engine import EngineBridge

log = logging.getLogger(__name__)

STORAGE_SECTION = "Storage"
STORAGE_POLL_HZ = 10.0
STORAGE_POLL_HIDDEN_HZ = 2.0
# Entries of a folder read at a time.
CHILDREN_LIMIT = 500

UNSUPPORTED_TEXT = "The engine is out of date: rebuild and deploy it to use this section."
JOB_RUNNING_TEXT = "{title} is running; wait for it to finish or stop it."
SPEED_STOP_TITLE = "Stop the speed test?"
SPEED_STOP_MESSAGE = "The test file is deleted. Results measured so far are kept."
CLOSE_SPEED_MESSAGE = "Closing stops it and deletes its test file; nothing else on the drive changes."
CLOSE_SCAN_MESSAGE = "Closing stops it; its results are lost."
LEFTOVER_TITLE = "Remove the leftover test file?"
EXPLORER_FAILED = "Could not open File Explorer"
CHOOSE_FOLDER_TITLE = "Choose a folder to scan"
BLOCKED_TITLE = "The speed test can't run now"
RECORDED_NOTE = "• The run is recorded in History › Activity; the test leaves nothing behind."


def lost_text(title: str) -> str:
    return f"Lost track of {title}."


def speed_confirm_message(
    letter: str, volume: dict[str, Any], plan: dict[str, Any], size: int, runs: int
) -> str:
    """What the speed test of `letter` does, for its confirmation dialog."""
    label = str(volume.get("label") or "").strip() or "Local Disk"
    hdd = str(plan.get("media") or "").lower() == "hdd" or is_hdd(volume)
    endurance = "" if hdd else " and uses a little of the SSD's write endurance"
    writes = int(plan.get("max_write_bytes") or max_write_bytes(size, runs))
    return (
        f"{APP_NAME} writes a {size_text(size)} test file to {letter} ({label}), measures how fast the drive "
        f"reads and writes, then deletes the file. This writes up to {size_text(writes)} to the drive"
        f"{endurance}. It takes {estimate_text(float(plan.get('estimated_seconds') or 0))}. You can stop it "
        "at any time."
    )


class StorageFeature:
    """State, widgets and flows of the Storage section, mixed into `App`.

    `_storage_job` is the id of the storage job being followed (None when none runs),
    `_storage_kind` its kind and `_storage_title` its title; `_running_storage_title` is the
    only view other features have of them. `_storage_scan_job` is the finished scan whose tree
    is shown and whose files a duplicate search compares.
    """

    _storage_supported: bool
    _storage_job: int | None
    _storage_kind: str
    _storage_title: str
    storage_panel: StoragePanel

    if TYPE_CHECKING:
        engine: EngineBridge | None
        elevated: bool
        errors: list[str]
        _busy: bool

        def section_visible(self, name: str) -> bool: ...
        def set_job_status(self, lane: str, text: str) -> None: ...
        def set_status(self, text: str, color: str = ...) -> None: ...
        def withdraw(self) -> None: ...
        def update_idletasks(self) -> None: ...
        def _guard(self, *, needs_admin: bool = True) -> bool: ...
        def _set_busy(self, busy: bool, close: str = ..., *, action: str = ..., note: str = ...) -> None: ...
        def _show_error(self, title: str, exc: BaseException | None) -> None: ...
        def _set_clipboard(self, text: str) -> None: ...
        def load_history(self) -> None: ...
        def _continue_close(self, lane: str, *, busy_unchanged: bool) -> None: ...
        def _running_close(self) -> Any: ...

    def _init_storage_state(self) -> None:
        """Sets the section's state; runs before the window is built."""
        self._storage_supported = True
        self._storage_loading = False
        # The drives and history are read again the next time the section is shown.
        self._storage_reload = False
        self._storage_volumes: list[dict[str, Any]] = []
        self._storage_history: list[dict[str, Any]] = []
        self._storage_job = None
        self._storage_kind = ""
        self._storage_title = ""
        self._storage_polled = 0.0
        # What is being started while the start is queued on the worker ("the scan of C:\").
        self._storage_starting = ""
        # Test file size of the speed test being started or followed.
        self._storage_size: int | None = None
        self._storage_scan_job: int | None = None
        self._storage_scan_root: str | None = None
        self._storage_order = "allocated"

    # -- building and loading ----------------------------------------------------------

    def _build_storage_tab(self, frame: ctk.CTkFrame) -> None:
        """Builds the section's panel into `frame`; handles a missing engine itself."""
        self.storage_panel = StoragePanel(
            frame,
            on_refresh=self.load_storage,
            on_start_speed=self._on_start_speed,
            on_stop=self._on_stop_storage,
            on_remove_leftover=self._on_remove_leftover,
            on_copy_speed=self._on_copy_speed,
            on_scan=self._on_scan,
            on_choose_folder=self._on_choose_folder,
            on_find_duplicates=self._on_find_duplicates,
            on_children=self._storage_children,
            on_open=self._on_show_in_explorer,
            on_copy_path=self._on_copy_path,
            on_order=self._on_storage_order,
        )
        self.storage_panel.grid(row=0, column=0, sticky="nsew", padx=4, pady=4)
        if self.engine is None:
            self._storage_supported = False
            self.storage_panel.set_engine_ready(False)
        elif not self.engine.supports("storage_volumes"):
            self._storage_supported = False
            self.storage_panel.set_unsupported(UNSUPPORTED_TEXT)

    def _storage_tab_shown(self) -> None:
        """Runs when the Storage section is shown while the engine is loaded."""
        if not self._storage_supported:
            return
        if (not self.storage_panel.loaded or self._storage_reload) and not self._storage_loading:
            self.load_storage()
        # The next frame reads the job at once instead of waiting for the hidden-section period.
        self._storage_polled = 0.0

    def load_storage(self) -> None:
        """Reads the fixed drives and the earlier speed-test results; whichever callback
        completes the pair shows both. An unreadable history is shown as such."""
        engine = self.engine
        if engine is None or not self._storage_supported or self._storage_loading:
            return
        self._storage_loading = True
        self._storage_reload = False
        self.storage_panel.set_loading()
        results: dict[str, Future[Any]] = {}
        names = ("volumes", "history")

        def keep(name: str) -> Callable[[Future[Any]], None]:
            def done(future: Future[Any]) -> None:
                results[name] = future
                if len(results) == len(names):
                    self._storage_loaded(results)

            return done

        engine.storage_volumes(callback=keep("volumes"))
        engine.speed_history(callback=keep("history"))

    def _storage_loaded(self, results: dict[str, Future[Any]]) -> None:
        self._storage_loading = False
        volumes = results["volumes"]
        if volumes.exception() is not None:
            self.storage_panel.show_error(str(volumes.exception()))
            return
        history = results["history"]
        history_error = None if history.exception() is None else str(history.exception())
        self._storage_volumes = [dict(v) for v in volumes.result() or []]
        self._storage_history = [] if history_error else [dict(h) for h in history.result() or []]
        self.storage_panel.show(
            self._storage_volumes,
            self._storage_history,
            engine_ready=True,
            elevated=self.elevated,
            history_error=history_error,
        )

    def _refresh_storage_actions(self) -> None:
        """Runs whenever the window's busy state changes, to enable or disable the actions."""
        panel = getattr(self, "storage_panel", None)
        if panel is not None:
            panel.set_actions_enabled(not self._busy)

    # -- starting jobs -------------------------------------------------------------------

    def _storage_job_running(self) -> bool:
        """Says so and returns True while a storage job runs or is being started."""
        if self._storage_job is None and not self._storage_starting:
            return False
        title = self._storage_title if self._storage_job is not None else self._storage_starting
        title = title[:1].upper() + title[1:] if title else "A storage job"
        self.set_status(JOB_RUNNING_TEXT.format(title=title), theme.WARNING)
        return True

    def _on_start_speed(self, letter: str, size: int, runs: int) -> None:
        """Plans the speed test (a dry run), explains a block or asks for confirmation, then
        starts it."""
        if not self._guard(needs_admin=True):
            return
        if self._storage_job_running():
            return
        assert self.engine is not None
        self._set_busy(True, "read")
        self.set_status(f"Checking whether {letter} can be tested…")

        def planned(future: Future[Any]) -> None:
            self._set_busy(False)
            exc = future.exception()
            if exc is not None:
                self._show_error(f"Could not prepare the speed test of {letter}", exc)
                return
            plan = (future.result() or {}).get("plan") or {}
            blocked = plan.get("blocked_reason")
            notes = [f"• {note}" for note in plan.get("notes") or []]
            if blocked:
                self.set_status(f"The speed test of {letter} can't run now: {blocked}", theme.WARNING)
                MessageDialog(self, title=BLOCKED_TITLE, message=sentence(str(blocked)), details=notes)
                return
            volume = self.storage_panel.speed.selected_volume() or {}
            self.set_status(f"The speed test of {letter} is ready to start.")
            MessageDialog(
                self,
                title=f"Test the speed of {letter}?",
                message=speed_confirm_message(letter, volume, plan, size, runs),
                details=[*notes, RECORDED_NOTE],
                confirm_text="Start test",
                cancel_text="Cancel",
                on_confirm=lambda: self._start_speed(letter, size, runs),
                on_cancel=lambda: self.set_status(f"The speed test of {letter} was not started."),
            )

        self.engine.plan_speed_test(letter, size, runs, callback=planned)

    def _start_speed(self, letter: str, size: int, runs: int) -> None:
        if self.engine is None:
            return
        self._storage_starting = f"the speed test of {letter}"
        self._storage_size = size
        self._set_busy(True, "read")
        self.set_status(f"Testing the speed of {letter}…")
        self.engine.start_speed_test(letter, size, runs, callback=self._storage_started)

    def _on_scan(self, path: str) -> None:
        """Starts a scan of `path` at once: it only reads."""
        if not self._guard(needs_admin=False):
            return
        if self._storage_job_running():
            return
        assert self.engine is not None
        self._storage_starting = f"the scan of {path}"
        self._set_busy(True, "read")
        self.set_status(f"Scanning {path}…")
        self.engine.start_storage_scan(path, callback=self._storage_started)

    def _on_choose_folder(self) -> None:
        """Asks for a folder and adds it to the Space page's locations."""
        path = system.ask_folder(self, title=CHOOSE_FOLDER_TITLE)
        if path:
            self.storage_panel.add_scan_folder(path)

    def _on_find_duplicates(self, min_size: int) -> None:
        """Starts a duplicate search among the files of the scan that is shown."""
        scan_job = self._storage_scan_job
        if scan_job is None:
            self.set_status(NO_SCAN_TEXT, theme.WARNING)
            return
        if not self._guard(needs_admin=False):
            return
        if self._storage_job_running():
            return
        assert self.engine is not None
        self._storage_starting = "the duplicate search"
        self._set_busy(True, "read")
        self.set_status("Finding duplicate files…")
        self.engine.start_duplicates(scan_job, min_size, callback=self._storage_started)

    def _storage_started(self, future: Future[Any]) -> None:
        self._set_busy(False)
        what = self._storage_starting or "the storage job"
        self._storage_starting = ""
        exc = future.exception()
        started = None if exc is not None else (future.result() or {})
        job = (started or {}).get("job")
        job_id = job.get("id") if job else None
        if exc is not None or not job or not isinstance(job_id, int):
            self._show_error(f"Could not start {what}", exc or RuntimeError("the engine started no job"))
            if self.section_visible("History"):
                self.load_history()
            return
        plan = (started or {}).get("plan") or {}
        self._storage_job = job_id
        self._storage_kind = str(job.get("kind") or "")
        self._storage_title = str(job.get("title") or what)
        self._storage_polled = 0.0
        self.storage_panel.begin_job(job, notes=plan.get("notes") or [], size_bytes=self._storage_size)
        self.set_job_status("storage", storage_status_text(job))
        if self.section_visible("History"):
            self.load_history()

    # -- polling -------------------------------------------------------------------------

    def _poll_storage(self, now: float) -> None:
        """Runs every frame; reads the followed job at `STORAGE_POLL_HZ` while the section is
        shown and `STORAGE_POLL_HIDDEN_HZ` otherwise."""
        panel = getattr(self, "storage_panel", None)
        job = self._storage_job
        if panel is None or self.engine is None or job is None:
            return
        visible = self.section_visible(STORAGE_SECTION)
        period = 1.0 / (STORAGE_POLL_HZ if visible else STORAGE_POLL_HIDDEN_HZ)
        if now - self._storage_polled < period:
            return
        self._storage_polled = now
        try:
            view = self.engine.storage_job(job)
        except Exception:  # noqa: BLE001 - an unreadable job is reported as lost
            log.exception("reading storage job %s failed", job)
            view = None
        if view is None:
            self._lose_storage()
            return
        panel.update_job(view, visible=visible)
        if view.get("state") == "running":
            self.set_job_status("storage", storage_status_text(view))
        else:
            self._storage_finished(view)

    def _lose_storage(self) -> None:
        """Stops following the job and says so once."""
        message = lost_text(self._storage_title or "the storage job")
        self._storage_job = None
        self._storage_size = None
        self.set_job_status("storage", "")
        self.set_status(message, theme.CRITICAL)
        try:
            self.storage_panel.lose_job(message)
        except Exception:  # noqa: BLE001 - the status bar already reports the lost job
            log.exception("could not show the lost storage job")

    def _storage_poll_failed(self) -> None:
        """Runs once after `_poll_storage` raised; the window stops polling this lane."""
        if self._storage_job is not None:
            self._lose_storage()
            return
        self.set_job_status("storage", "")

    def _storage_finished(self, view: dict[str, Any]) -> None:
        job_id = view.get("id")
        kind = str(view.get("kind") or self._storage_kind)
        title = self._storage_title or str(view.get("title") or "The storage job")
        self._storage_job = None
        self._storage_size = None
        self.set_job_status("storage", "")
        result: dict[str, Any] | None = None
        if self.engine is not None and isinstance(job_id, int):
            try:
                result = self.engine.storage_result(job_id)
            except Exception:  # noqa: BLE001 - the job's state is still shown without its result
                log.exception("reading the result of storage job %s failed", job_id)
        self.storage_panel.finish_job(view, result)
        state = view.get("state")
        target = job_target(view)
        elapsed = fmt_duration(float(view.get("elapsed_ms") or 0))
        if kind == KIND_SPEED:
            if state == "succeeded":
                self.set_status(f"Done: speed test of {target} finished in {elapsed}.", theme.GOOD)
            elif state == "cancelled":
                self.set_status(
                    f"The speed test of {target} stopped; the test file was deleted.", theme.INK_SECONDARY
                )
            self._storage_reload = True
            if self.section_visible(STORAGE_SECTION):
                self.load_storage()
        elif kind == KIND_SCAN:
            self._scan_finished(view, result)
        elif kind == KIND_DUPLICATES and result is not None:
            self.storage_panel.show_duplicates(result)
            groups = int(result.get("group_count") or 0)
            if state == "cancelled":
                self.set_status(
                    "The duplicate search stopped; only the files compared so far are listed.",
                    theme.INK_SECONDARY,
                )
            elif groups:
                self.set_status(f"Found {count_text(groups, 'group')} of duplicate files.", theme.GOOD)
            else:
                self.set_status("No duplicate files found.", theme.GOOD)
        if state == "failed":
            summary = str(view.get("summary") or view.get("hint") or "")
            self.set_status(f"{title} failed: {summary or 'see the Storage section'}", theme.CRITICAL)
            MessageDialog(self, title=f"{title} failed", message=sentence(summary) or "It did not finish.")
        if self.section_visible("History"):
            self.load_history()

    def _scan_finished(self, view: dict[str, Any], result: dict[str, Any] | None) -> None:
        job_id = view.get("id")
        if result is None or not isinstance(job_id, int) or self.engine is None:
            return
        summary = result.get("summary") or {}
        self._storage_scan_job = job_id
        self._storage_scan_root = str(summary.get("root") or job_target(view))
        try:
            page = self.engine.storage_children(job_id, 0, self._storage_order, CHILDREN_LIMIT)
        except Exception:  # noqa: BLE001 - the summary is shown without the tree
            log.exception("reading the scanned folder failed")
            page = None
        self.storage_panel.show_scan(result, page)
        self.storage_panel.set_scan_available(self._storage_scan_root, view.get("finished_at"))
        target = str(summary.get("volume") if summary.get("whole_volume") else self._storage_scan_root)
        if summary.get("completed"):
            self.set_status(
                f"Scan of {target} finished: {size_text(int(summary.get('allocated_bytes') or 0))} in "
                f"{count_text(int(summary.get('files') or 0), 'file')}.",
                theme.GOOD,
            )
        else:
            self.set_status(
                f"The scan of {target} stopped; the numbers cover only what was scanned.", theme.INK_SECONDARY
            )

    def _running_storage_title(self) -> str | None:
        """Title of the storage job that is running, or None when none runs."""
        return self._storage_title if self._storage_job is not None else None

    # -- stopping ------------------------------------------------------------------------

    def _on_stop_storage(self) -> None:
        """Stops the followed job: a speed test after asking, a scan or search at once."""
        job = self._storage_job
        if job is None or self.engine is None:
            return
        if self._storage_kind == KIND_SPEED:
            MessageDialog(
                self,
                title=SPEED_STOP_TITLE,
                message=SPEED_STOP_MESSAGE,
                confirm_text="Stop",
                cancel_text="Keep running",
                on_confirm=lambda: self._cancel_storage(job),
            )
        else:
            self._cancel_storage(job)

    def _cancel_storage(self, job: int) -> None:
        if self._storage_job != job or self.engine is None:
            return
        try:
            self.engine.cancel_storage(job)
        except Exception as exc:  # noqa: BLE001 - shown to the user; the job keeps being polled
            self._show_error(f"Could not stop {self._storage_title}", exc)
            return
        self.storage_panel.set_stopping()
        self.set_status("Stopping…")

    # -- leftovers -----------------------------------------------------------------------

    def _on_remove_leftover(self, leftover: dict[str, Any]) -> None:
        """Asks, then removes a test folder a speed test left behind (needs administrator
        rights; recorded in the audit log)."""
        if not self._guard(needs_admin=True):
            return
        if self._storage_job_running():
            return
        path = str(leftover.get("path") or "")
        size = leftover.get("bytes")
        file = f"the {size_text(int(size))} test file" if size is not None else "the test file"
        MessageDialog(
            self,
            title=LEFTOVER_TITLE,
            message=(
                f"Deletes {path} and {file} in it, left by a speed test that didn't finish. "
                "Nothing else is touched."
            ),
            confirm_text="Remove",
            cancel_text="Cancel",
            on_confirm=lambda: self._remove_leftover(path),
        )

    def _remove_leftover(self, path: str) -> None:
        if self.engine is None:
            return
        self._set_busy(True, "irreversible", action="Removing the leftover test file")
        self.set_status("Removing the leftover test file…")

        def done(future: Future[Any]) -> None:
            self._set_busy(False)
            exc = future.exception()
            if exc is not None:
                self._show_error("Could not remove the leftover test file", exc)
            else:
                removal = future.result() or {}
                detail = str(removal.get("detail") or "")
                if removal.get("removed"):
                    self.set_status(f"Removed the leftover test file: {detail}.", theme.GOOD)
                else:
                    self.set_status(f"The leftover test file was not removed: {detail}", theme.WARNING)
            self._storage_reload = True
            self.load_storage()
            if self.section_visible("History"):
                self.load_history()

        self.engine.remove_speed_leftover(path, callback=done)

    # -- the scan tree -------------------------------------------------------------------

    def _storage_results_gone(self) -> None:
        self._storage_scan_job = None
        self._storage_scan_root = None
        self.set_status(RESULTS_GONE_TEXT, theme.WARNING)
        self.storage_panel.clear_scan()

    def _storage_children(self, node: int) -> dict[str, Any] | None:
        """One folder of the shown scan, read when the tree opens it; None once the scan's
        results are gone, which clears the tree."""
        job = self._storage_scan_job
        if job is None or self.engine is None:
            return None
        try:
            page = self.engine.storage_children(job, node, self._storage_order, CHILDREN_LIMIT)
        except Exception:  # noqa: BLE001 - treated like results that are gone
            log.exception("reading folder %s of scan %s failed", node, job)
            page = None
        if page is None:
            self._storage_results_gone()
            return None
        return page

    def _on_storage_order(self, order: str) -> None:
        """Sorts the tree by `order` ("allocated" or "logical") and reads the top folder again."""
        self._storage_order = order
        job = self._storage_scan_job
        if job is None or self.engine is None:
            return
        page = self._storage_children(0)
        if page is not None:
            self.storage_panel.space.show_page(page)

    def _on_show_in_explorer(self, path: str, is_file: bool) -> None:
        try:
            system.show_in_explorer(path, select=is_file)
        except Exception as exc:  # noqa: BLE001 - shown to the user
            self._show_error(EXPLORER_FAILED, exc)
            return
        self.set_status("Opened File Explorer.")

    def _on_copy_path(self, path: str) -> None:
        self._set_clipboard(path)
        self.set_status("Path copied.", theme.GOOD)

    def _on_copy_speed(self, text: str) -> None:
        self._set_clipboard(text)
        self.set_status("Results copied.", theme.GOOD)

    # -- closing -------------------------------------------------------------------------

    def _storage_allow_close(self) -> bool:
        """Whether the window may close now as far as storage jobs are concerned.

        With a running job, a dialog says what closing does to it and False is returned;
        confirming the dialog continues closing.
        """
        if self.engine is None:
            return True
        try:
            jobs = self.engine.storage_jobs()
        except Exception:  # noqa: BLE001 - an unreadable job list must not keep the window open
            log.exception("reading the storage jobs failed")
            jobs = []
        running = next((j for j in jobs if j.get("state") == "running"), None)
        if running is None:
            return True
        kind = running.get("kind")
        if kind == KIND_SPEED:
            heading = f"The speed test of {job_target(running)} is still running"
            message, confirm, cancel = CLOSE_SPEED_MESSAGE, "Stop and close", "Keep running"
        else:
            heading = (
                "The duplicate search is still running"
                if kind == KIND_DUPLICATES
                else "A storage scan is still running"
            )
            message, confirm, cancel = CLOSE_SCAN_MESSAGE, "Close", "Keep open"
        # The engine call running now, if any, was already confirmed in the busy dialog.
        accepted = self._running_close() if self._busy or self.engine.busy else None
        MessageDialog(
            self,
            title=heading,
            message=message,
            confirm_text=confirm,
            cancel_text=cancel,
            on_confirm=lambda: self._storage_close_confirmed(accepted),
        )
        return False

    def _storage_close_confirmed(self, accepted: Any) -> None:
        """Continues closing after the storage dialog was confirmed. An engine call other than
        a read that started while the dialog was open, and was not the one already accepted,
        is asked about in the busy dialog first."""
        if self.engine is None:
            self._continue_close("storage", busy_unchanged=True)
            return
        running = self._running_close() if self._busy or self.engine.busy else None
        unchanged = running is None or running.kind == "read" or running == accepted
        self._continue_close("storage", busy_unchanged=unchanged)

    def _storage_before_shutdown(self) -> None:
        """Runs on every close path just before the engine shuts down: stops the running
        storage job, which can take a few seconds, so the window is hidden first."""
        if self.engine is None:
            return
        panel = getattr(self, "storage_panel", None)
        if panel is not None:
            panel.stop_animations()
        self.withdraw()
        self.update_idletasks()
        try:
            outcomes = self.engine.storage_shutdown()
        except Exception:  # noqa: BLE001 - closing continues whatever the storage lane reports
            log.exception("stopping the storage job failed")
            return
        for outcome in outcomes:
            log.info("storage job %s (%s): %s", outcome.get("id"), outcome.get("kind"), outcome.get("action"))
        self._storage_job = None
