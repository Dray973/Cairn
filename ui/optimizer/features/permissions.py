"""Permissions section: a guide to the camera, microphone and location permissions of apps.

Windows 11 manages these permissions itself, in Settings › Privacy & security, and on this version
of Windows an app like Cairn cannot change them, so the section changes nothing and journals
nothing. Each capability's card opens its Settings page (`system.open_uri`, only the three privacy
pages) and lists the desktop apps Windows recorded using the device, read on every visit. Journal
records of permission changes made by earlier builds are undone from History like any other
registry record.
"""

from __future__ import annotations

import logging
from concurrent.futures import Future
from typing import TYPE_CHECKING, Any

from .. import APP_NAME, system, theme
from ..widgets.permissions import PermissionsPanel, settings_page

if TYPE_CHECKING:
    import customtkinter as ctk

    from ..bridge.engine import EngineBridge

log = logging.getLogger(__name__)

PERMISSIONS_SECTION = "Permissions"
UNSUPPORTED_TEXT = (
    "The engine is out of date: rebuild and deploy it to see which desktop apps used each device."
)
NO_ENGINE_TEXT = "The engine is not available, so the desktop apps that used each device are not listed."


class PermissionsFeature:
    """State, widgets and flows of the Permissions section, mixed into `App`.

    Uses these members of the window: `engine`, `_busy` and `set_status`.
    """

    permissions_panel: PermissionsPanel
    _permissions_loading: bool
    _permissions_reload: bool
    _permissions_supported: bool

    if TYPE_CHECKING:
        engine: EngineBridge | None
        _busy: bool

        def set_status(self, text: str, color: str = ...) -> None: ...

    def _init_permissions_state(self) -> None:
        """Sets the section's state; runs before the window is built."""
        # A read is in flight; a reload asked for meanwhile runs once it returns.
        self._permissions_loading = False
        self._permissions_reload = False
        self._permissions_supported = True

    def _build_permissions_tab(self, frame: ctk.CTkFrame) -> None:
        """Builds the Permissions section into `frame`; handles a missing or outdated engine,
        whose only loss is the list of desktop apps."""
        self.permissions_panel = PermissionsPanel(
            frame,
            on_refresh=self.load_permissions,
            on_open_settings=self._open_permission_settings,
        )
        self.permissions_panel.grid(row=0, column=0, sticky="nsew", padx=4, pady=4)
        if self.engine is None:
            self._permissions_supported = False
            self.permissions_panel.set_unsupported(NO_ENGINE_TEXT)
        elif not self.engine.supports("permissions_list"):
            self._permissions_supported = False
            self.permissions_panel.set_unsupported(UNSUPPORTED_TEXT)
        self._refresh_permission_actions()

    def _permissions_tab_shown(self) -> None:
        """Runs when the Permissions section is shown while the engine is loaded. Windows'
        record is read on every visit: apps keep using the devices meanwhile."""
        self.load_permissions()

    def _refresh_permission_actions(self) -> None:
        """Runs whenever the window's busy state changes."""
        panel = getattr(self, "permissions_panel", None)
        if panel is not None:
            panel.set_actions_enabled(not self._busy and self.engine is not None)

    def _permissions_after_mutation(self) -> None:
        """Runs after any journaled change or undo. The guide shows nothing a change or an undo
        sets, so there is nothing to read again."""

    # -- reading -------------------------------------------------------------------

    def load_permissions(self) -> None:
        """Queues a read of the guide; while one is in flight, one more runs after it."""
        if self.engine is None or not self._permissions_supported:
            return
        if self._permissions_loading:
            self._permissions_reload = True
            return
        self._permissions_loading = True
        self.permissions_panel.set_loading()
        self.engine.permissions_list(callback=self._permissions_loaded)

    def _permissions_loaded(self, future: Future[Any]) -> None:
        self._permissions_loading = False
        try:
            exc = future.exception()
            if exc is not None:
                self.permissions_panel.show_error(str(exc))
            else:
                self.permissions_panel.show(future.result())
        except Exception as exc:  # noqa: BLE001 - a malformed guide is reported in the panel
            log.exception("the permissions guide could not be shown")
            self.permissions_panel.show_error(f"unexpected data ({exc})")
        if self._permissions_reload:
            self._permissions_reload = False
            self.load_permissions()

    # -- Windows Settings ------------------------------------------------------------

    def _open_permission_settings(self, capability: str) -> None:
        """Opens the Windows Settings page of `capability`; only the privacy pages of
        `widgets.permissions.SETTINGS_PAGES` are opened."""
        uri = settings_page(capability)
        if uri is None:
            log.warning("refused to open the settings of %r: not a permissions page", capability)
            self.set_status(f"{APP_NAME} has no Settings page for {capability}.", theme.WARNING)
            return
        try:
            system.open_uri(uri)
        except OSError as exc:
            self.set_status(f"Could not open Windows Settings: {exc}", theme.WARNING)
