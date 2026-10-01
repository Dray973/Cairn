"""Security and Boot history sections: a read-only security checkup and the history of
Windows starts and shutdowns.

Both sections only read; nothing is journaled or logged. Fixes reuse the window's journaled
flows ("Show file extensions" is the catalog tweak applied through `_on_item_action`, "Turn off
at startup" is `_on_startup_toggle`) or open Windows' own settings pages and tools. The Windows
Update search runs on an engine thread and is polled from the frame loop through the
"security" job lane; closing the window asks it to stop and never waits for it.
"""

from __future__ import annotations

import logging
import time
from collections.abc import Mapping
from concurrent.futures import Future
from typing import TYPE_CHECKING, Any

from .. import system, theme
from ..widgets.dialogs import MessageDialog
from ..widgets.security import allowed_uri

if TYPE_CHECKING:
    import customtkinter as ctk

    from ..bridge.engine import EngineBridge
    from ..widgets.boot import BootPanel
    from ..widgets.security import SecurityPanel

log = logging.getLogger(__name__)

UNSUPPORTED_TEXT = "The engine is out of date: rebuild and deploy it to use this section."
NO_ENGINE_TEXT = "The engine is not available."
SECURITY_SECTION = "Security"
BOOT_SECTION = "Boot history"
OPTIMIZE_SECTION = "Optimize"
# A checkup older than this is read again when the section is shown.
CHECKUP_STALE_S = 300.0
UPDATE_POLL_VISIBLE_S = 0.5
UPDATE_POLL_HIDDEN_S = 2.0
BOOT_LIMIT = 60
# Same title as the window's relaunch dialog, so tests' confirm helpers refuse it.
ADMIN_DIALOG_TITLE = "Administrator rights needed"
ELEVATE_SECURITY_TEXT = (
    "Checking drive encryption needs administrator rights. Restart Cairn as administrator?"
)
ELEVATE_BOOT_TEXT = (
    "Reading Windows' startup records needs administrator rights. Restart Cairn as administrator?"
)
SCAN_STATUS = "◐ Checking Windows Update…"
SCAN_STATUS_ONLINE = "◐ Checking Windows Update online…"
SCAN_RUNNING_TEXT = "Windows Update is already being checked."
PER_USER_TEXT = "Your own settings can only be changed when Cairn runs as your account."
NO_SCAN_TEXT = "Run a scan first: open Optimize to show file extensions."
SECURITY_LANE = "security"


class HealthFeature:
    """State, widgets and flows of the Security and Boot history sections, mixed into `App`.

    Uses these members of the window: `engine`, `elevated`, `errors`, `_busy`, `_last_scan`,
    `can_elevate`, `section_visible`, `show_section`, `set_status`, `set_job_status`,
    `_show_error`, `_relaunch_elevated`, `_on_item_action` and `_on_startup_toggle`.
    """

    security_panel: SecurityPanel
    boot_panel: BootPanel
    _health_supported: bool
    _boot_supported: bool
    _checkup_at: float | None
    _checkup_loading: bool
    _checkup_stale: bool
    _checkup_auto_scan: bool
    _update_scan_active: bool
    _update_scan_online: bool
    _update_next_poll: float
    _update_auto_started: bool
    _boot_loading: bool
    _boot_stale: bool

    if TYPE_CHECKING:
        engine: EngineBridge | None
        elevated: bool
        errors: list[str]
        _busy: bool
        _last_scan: dict[str, Any] | None

        def section_visible(self, name: str) -> bool: ...
        def show_section(self, name: str, *, run_hook: bool = True) -> None: ...
        def set_status(self, text: str, color: str = ...) -> None: ...
        def set_job_status(self, lane: str, text: str) -> None: ...
        def _show_error(self, title: str, exc: BaseException | None) -> None: ...
        def _relaunch_elevated(self) -> None: ...
        def _on_item_action(self, item: dict[str, Any], kind: str) -> None: ...
        def _on_startup_toggle(
            self, entry: dict[str, Any], enabled: bool, *, on_done: Any = None
        ) -> None: ...

    def _init_health_state(self) -> None:
        """Sets the sections' state; runs before the window is built."""
        self._health_supported = True
        self._boot_supported = True
        # Monotonic time the shown checkup was read at; None before the first one.
        self._checkup_at = None
        self._checkup_loading = False
        # A change or a finished Windows Update search made the shown checkup outdated.
        self._checkup_stale = False
        # Whether the checkup being read may start the automatic offline search.
        self._checkup_auto_scan = False
        self._update_scan_active = False
        self._update_scan_online = False
        self._update_next_poll = 0.0
        # The load in progress already started its automatic search.
        self._update_auto_started = False
        self._boot_loading = False
        self._boot_stale = False

    # -- sections ------------------------------------------------------------------

    def _build_security_tab(self, frame: ctk.CTkFrame) -> None:
        """Builds the Security section into `frame`; handles a missing or outdated engine."""
        from ..widgets.security import SecurityPanel

        self.security_panel = SecurityPanel(
            frame,
            on_refresh=self.load_security,
            on_fix=self._on_security_fix,
            on_elevate=lambda: self._offer_elevation(ELEVATE_SECURITY_TEXT),
            on_stop=self._stop_update_scan,
        )
        self.security_panel.grid(row=0, column=0, sticky="nsew", padx=4, pady=4)
        if self.engine is None:
            self._health_supported = False
            self.security_panel.set_unsupported(NO_ENGINE_TEXT)
        elif not self.engine.supports("health_security_checkup"):
            self._health_supported = False
            self.security_panel.set_unsupported(UNSUPPORTED_TEXT)

    def _security_tab_shown(self) -> None:
        """Runs when the Security section is shown while the engine is loaded: reads the checkup
        when none was read, it is older than `CHECKUP_STALE_S` or a change outdated it."""
        if not self._health_supported or self._checkup_loading:
            return
        if self._checkup_due():
            self.load_security()
        elif self._update_scan_active:
            self._update_next_poll = 0.0

    def _checkup_due(self) -> bool:
        if self._checkup_at is None or self._checkup_stale:
            return True
        return _now() - self._checkup_at > CHECKUP_STALE_S

    def _build_boot_tab(self, frame: ctk.CTkFrame) -> None:
        """Builds the Boot history section into `frame`; handles a missing or outdated engine."""
        from ..widgets.boot import BootPanel

        self.boot_panel = BootPanel(
            frame,
            on_refresh=self.load_boot_history,
            on_elevate=lambda: self._offer_elevation(ELEVATE_BOOT_TEXT),
            on_turn_off=self._on_boot_turn_off,
            on_open_tool=self._on_boot_open_tool,
        )
        self.boot_panel.grid(row=0, column=0, sticky="nsew", padx=4, pady=4)
        if self.engine is None:
            self._boot_supported = False
            self.boot_panel.set_unsupported(NO_ENGINE_TEXT)
        elif not self.engine.supports("health_boot_history"):
            self._boot_supported = False
            self.boot_panel.set_unsupported(UNSUPPORTED_TEXT)

    def _boot_tab_shown(self) -> None:
        """Runs when the Boot history section is shown while the engine is loaded. Without
        administrator rights nothing is read: Windows lets only administrators read the log."""
        if not self._boot_supported or self._boot_loading:
            return
        if not self.elevated:
            self.boot_panel.set_needs_admin()
            return
        if not self.boot_panel.loaded or self._boot_stale:
            self.load_boot_history()

    # -- window hooks --------------------------------------------------------------

    def _refresh_health_actions(self) -> None:
        """Runs whenever the window's busy state changes: fixes that change Windows wait."""
        enabled = not self._busy and self.engine is not None
        for name in ("security_panel", "boot_panel"):
            panel = getattr(self, name, None)
            if panel is not None:
                panel.set_actions_enabled(enabled)

    def _health_after_mutation(self) -> None:
        """Runs after any journaled change or undo: both views may be outdated; the visible
        one is read again (after the read in flight, if any)."""
        self._checkup_stale = True
        self._boot_stale = True
        if self._health_supported and self.section_visible(SECURITY_SECTION) and not self._checkup_loading:
            self.load_security()
        if (
            self._boot_supported
            and self.elevated
            and self.section_visible(BOOT_SECTION)
            and not self._boot_loading
        ):
            self.load_boot_history()

    # -- job lane "security" -------------------------------------------------------

    def _poll_health(self, now: float) -> None:
        """Runs every frame while the engine is loaded; reads the Windows Update search's state
        twice a second while Security is shown and every two seconds otherwise."""
        if not self._update_scan_active or self.engine is None:
            return
        if now < self._update_next_poll:
            return
        visible = self.section_visible(SECURITY_SECTION)
        self._update_next_poll = now + (UPDATE_POLL_VISIBLE_S if visible else UPDATE_POLL_HIDDEN_S)
        view = self.engine.update_scan()
        if view is None:
            self._end_update_scan()
            return
        panel = getattr(self, "security_panel", None)
        if panel is not None:
            panel.update_scan(view)
        if view.get("state") == "running":
            return
        self._update_scan_finished(view)

    def _health_poll_failed(self) -> None:
        """Runs once after `_poll_health` raised; the window stops polling this lane."""
        self._end_update_scan()

    def _health_allow_close(self) -> bool:
        """Closing never waits for the security lane: the search only reads."""
        return True

    def _health_before_shutdown(self) -> None:
        """Asks a running Windows Update search to stop; never waits for it."""
        if self._update_scan_active and self.engine is not None:
            try:
                self.engine.cancel_update_scan()
            except Exception:  # noqa: BLE001 - closing goes on whatever the engine says
                log.warning("stopping the Windows Update search failed", exc_info=True)
        self._update_scan_active = False

    def _health_running_title(self) -> str | None:
        """The security lane holds no job that blocks anything."""
        return None

    # -- security checkup ----------------------------------------------------------

    def load_security(self, *, auto_scan: bool = True) -> None:
        """Reads a new checkup on the engine worker; the current rows stay until it arrives.
        With `auto_scan`, a checkup that finds no recent Windows Update search starts an
        offline one (once per load)."""
        if self.engine is None or not self._health_supported:
            return
        if self._checkup_loading:
            return
        self._checkup_loading = True
        # A change made while this read runs marks the view stale again.
        self._checkup_stale = False
        self._checkup_auto_scan = auto_scan
        self._update_auto_started = False
        self.security_panel.set_loading()
        self.engine.security_checkup(callback=self._security_loaded)

    def _security_loaded(self, future: Future[Any]) -> None:
        self._checkup_loading = False
        exc = future.exception()
        if exc is not None:
            log.warning("the security checkup failed: %s", exc)
            self.security_panel.show_error(str(exc))
            return
        checkup = future.result()
        try:
            self.security_panel.show(checkup, elevated=self.elevated)
        except Exception as error:  # noqa: BLE001 - malformed data is reported in the panel
            log.exception("the security checkup could not be shown")
            self.security_panel.show_error(f"unexpected checkup data ({error})")
            return
        self._checkup_at = _now()
        if self._checkup_stale and self.section_visible(SECURITY_SECTION):
            # Something changed while this checkup was read. The read that replaces it may
            # start the automatic search only if this one was allowed to.
            self.load_security(auto_scan=self._checkup_auto_scan)
            return
        scan = checkup.get("update_scan") if isinstance(checkup, Mapping) else None
        if isinstance(scan, Mapping) and scan.get("state") == "running":
            self._watch_update_scan(bool(scan.get("online")))
            self.security_panel.update_scan(scan)
        if (
            self._checkup_auto_scan
            and not self._update_auto_started
            and not self._update_scan_active
            and isinstance(checkup, Mapping)
            and checkup.get("update_scan_due")
        ):
            self._update_auto_started = True
            self._start_update_scan(False, automatic=True)

    def _start_update_scan(self, online: bool, *, automatic: bool = False) -> None:
        """Starts a Windows Update search (online, or Windows Update's cached data). A second
        start while one runs is refused with a status line."""
        if self.engine is None:
            return
        if not self.engine.supports("health_update_scan_start"):
            if not automatic:
                self.set_status(UNSUPPORTED_TEXT, theme.WARNING)
            return
        if self._update_scan_active:
            self.set_status(SCAN_RUNNING_TEXT, theme.WARNING)
            return
        try:
            view = self.engine.start_update_scan(online)
        except RuntimeError as exc:
            if not automatic:
                self.set_status(f"Could not check Windows Update: {exc}", theme.WARNING)
            else:
                log.warning("the automatic Windows Update search did not start: %s", exc)
            return
        self._watch_update_scan(online)
        self.security_panel.update_scan(view)

    def _watch_update_scan(self, online: bool) -> None:
        self._update_scan_active = True
        self._update_scan_online = online
        self._update_next_poll = 0.0
        self.set_job_status(SECURITY_LANE, SCAN_STATUS_ONLINE if online else SCAN_STATUS)

    def _stop_update_scan(self) -> None:
        """Asks the running Windows Update search to stop; the poll sees it end."""
        if self.engine is None or not self._update_scan_active:
            return
        self.engine.cancel_update_scan()
        self._update_next_poll = 0.0

    def _end_update_scan(self) -> None:
        self._update_scan_active = False
        self.set_job_status(SECURITY_LANE, "")

    def _update_scan_finished(self, view: Mapping[str, Any]) -> None:
        online = self._update_scan_online
        self._end_update_scan()
        state = view.get("state")
        if online and state == "done":
            count = len(view.get("updates") or [])
            if count:
                noun = "update" if count == 1 else "updates"
                self.set_status(f"Windows Update check finished: {count} {noun} waiting.", theme.GOOD)
            else:
                self.set_status("Windows Update check finished: nothing waiting.", theme.GOOD)
        elif online and state == "failed":
            self.set_status(
                f"Windows Update check failed: {view.get('error') or 'unknown error'}", theme.WARNING
            )
        elif online and state == "cancelled":
            self.set_status("The Windows Update check was stopped.", theme.WARNING)
        # The checkup is read again without starting another search.
        if self.section_visible(SECURITY_SECTION) and not self._checkup_loading:
            self.load_security(auto_scan=False)
        else:
            self._checkup_stale = True

    # -- fixes ---------------------------------------------------------------------

    def _on_security_fix(self, check: Mapping[str, Any], fix: Mapping[str, Any]) -> None:
        """Runs a fix button of a check."""
        action = fix.get("action")
        if not isinstance(action, Mapping):
            return
        kind = action.get("kind")
        label = str(fix.get("label", ""))
        if kind == "uri":
            self._open_uri(str(action.get("uri", "")))
        elif kind == "windows_tool":
            self._open_fix_tool(str(action.get("tool", "")), label, bool(action.get("requires_admin")))
        elif kind == "tweak":
            self._apply_fix_tweak(check, str(action.get("id", "")))
        elif kind == "update_scan":
            self._start_update_scan(bool(action.get("online")))
        elif kind == "elevate":
            self._offer_elevation(ELEVATE_SECURITY_TEXT)

    def _open_uri(self, uri: str) -> None:
        if not allowed_uri(uri):
            log.warning("refused to open %r: not an allowed settings page", uri)
            self.set_status(f"Cairn does not open {uri}.", theme.WARNING)
            return
        try:
            system.open_uri(uri)
        except OSError as exc:
            self.set_status(f"Could not open {uri}: {exc}", theme.WARNING)

    def _open_fix_tool(self, tool: str, label: str, requires_admin: bool) -> None:
        if self.engine is None:
            return
        title = label[len("Open ") :] if label.startswith("Open ") else label
        if requires_admin and not self.elevated:
            self.set_status(
                f"{label} needs administrator rights; restart as administrator to open it.", theme.WARNING
            )
            return

        def done(future: Future[Any]) -> None:
            exc = future.exception()
            if exc is not None:
                self._show_error(f"Could not open {title}", exc)
            else:
                self.set_status(f"Opened {title}.")

        self.engine.open_windows_tool(tool, callback=done)

    def _apply_fix_tweak(self, check: Mapping[str, Any], tweak_id: str) -> None:
        other_user = (self.security_panel.checkup or {}).get("other_user", False)
        if check.get("per_user") and other_user is not False:
            self.set_status(PER_USER_TEXT, theme.WARNING)
            return
        items = (self._last_scan or {}).get("items") or []
        item = next((i for i in items if i.get("id") == tweak_id), None)
        if item is None:
            self.set_status(NO_SCAN_TEXT, theme.WARNING)
            show = getattr(self, "show_section", None)
            if show is not None:
                show(OPTIMIZE_SECTION)
            return
        self._on_item_action(item, "apply")

    def _offer_elevation(self, text: str) -> None:
        """Offers to restart Cairn as administrator (the relaunch shows a UAC prompt)."""
        if not getattr(self, "can_elevate", True):
            from ..app import ADMIN_UNAVAILABLE_TEXT, ADMIN_UNAVAILABLE_TITLE

            MessageDialog(
                self, title=ADMIN_UNAVAILABLE_TITLE, message=ADMIN_UNAVAILABLE_TEXT, confirm_text="OK"
            )
            return
        MessageDialog(
            self,
            title=ADMIN_DIALOG_TITLE,
            message=text,
            confirm_text="Restart as administrator",
            cancel_text="Not now",
            on_confirm=self._relaunch_elevated,
        )

    # -- boot history --------------------------------------------------------------

    def load_boot_history(self) -> None:
        """Reads the boot history on the engine worker (administrators only)."""
        if self.engine is None or not self._boot_supported or self._boot_loading:
            return
        if not self.elevated:
            self.boot_panel.set_needs_admin()
            return
        self._boot_loading = True
        # A change made while this read runs marks the view stale again.
        self._boot_stale = False
        self.boot_panel.set_loading()
        self.engine.boot_history(BOOT_LIMIT, callback=self._boot_loaded)

    def _boot_loaded(self, future: Future[Any]) -> None:
        self._boot_loading = False
        exc = future.exception()
        if exc is not None:
            log.warning("reading the boot history failed: %s", exc)
            self.boot_panel.show_error(str(exc))
            return
        try:
            self.boot_panel.show(future.result())
        except Exception as error:  # noqa: BLE001 - malformed data is reported in the panel
            log.exception("the boot history could not be shown")
            self.boot_panel.show_error(f"unexpected boot history data ({error})")
            return
        if self._boot_stale and self.section_visible(BOOT_SECTION):
            # Something changed while the history was read.
            self.load_boot_history()

    def _on_boot_turn_off(self, entry: Mapping[str, Any]) -> None:
        """Turns a slow app's startup entry off (journaled), then reads the history again."""
        self._on_startup_toggle(dict(entry), False, on_done=lambda _ok: self._boot_toggled())

    def _boot_toggled(self) -> None:
        self._boot_stale = True
        self.load_boot_history()

    def _on_boot_open_tool(self, tool_id: str) -> None:
        """Opens Device Manager, Services or Event Viewer (each needs administrator rights)."""
        titles = {
            "device_manager": "Open Device Manager",
            "services": "Open Services",
            "event_viewer": "Open Event Viewer",
        }
        self._open_fix_tool(tool_id, titles.get(tool_id, tool_id), True)


def _now() -> float:
    return time.monotonic()
