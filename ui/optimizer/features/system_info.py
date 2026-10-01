"""System section: a read-only snapshot of the hardware and Windows configuration.

The snapshot is read on the engine worker the first time the section is shown and again on
Refresh. Nothing is changed or journaled and no elevation is needed, so the section has no
busy state, no guard and no dialogs. "Copy as text" writes the snapshot's text to the
clipboard rendered immediately, so programs running without elevation can paste it too.

This module loads without Tk: the panel's widgets are imported when the section is built.
"""

from __future__ import annotations

import ctypes
import functools
import logging
import time
from concurrent.futures import Future
from ctypes import wintypes
from typing import TYPE_CHECKING, Any

from .. import theme

if TYPE_CHECKING:
    import customtkinter as ctk

    from ..bridge.engine import EngineBridge
    from ..widgets.sysinfo import SystemPanel

log = logging.getLogger(__name__)

UNSUPPORTED_TEXT = "The engine is out of date: rebuild and deploy it to use this section."
COPIED_TEXT = "System summary copied to the clipboard."

CF_UNICODETEXT = 13
GMEM_MOVEABLE = 0x0002
# Another program can hold the clipboard open for a moment; OpenClipboard is retried.
OPEN_RETRIES = 5
OPEN_RETRY_DELAY_S = 0.02


class _ClipboardApi:
    """The user32 clipboard and kernel32 global-memory functions with explicit prototypes.

    They are bound on private library objects, so the prototypes never leak into other
    ctypes users of `ctypes.windll`. HWND, HANDLE and HGLOBAL are `c_void_p` in ctypes.
    """

    def __init__(self) -> None:
        user32 = ctypes.WinDLL("user32", use_last_error=True)
        kernel32 = ctypes.WinDLL("kernel32", use_last_error=True)

        self.open_clipboard = user32.OpenClipboard
        self.open_clipboard.argtypes = [wintypes.HWND]
        self.open_clipboard.restype = wintypes.BOOL
        self.empty_clipboard = user32.EmptyClipboard
        self.empty_clipboard.argtypes = []
        self.empty_clipboard.restype = wintypes.BOOL
        self.set_clipboard_data = user32.SetClipboardData
        self.set_clipboard_data.argtypes = [wintypes.UINT, wintypes.HANDLE]
        self.set_clipboard_data.restype = wintypes.HANDLE
        self.close_clipboard = user32.CloseClipboard
        self.close_clipboard.argtypes = []
        self.close_clipboard.restype = wintypes.BOOL

        self.global_alloc = kernel32.GlobalAlloc
        self.global_alloc.argtypes = [wintypes.UINT, ctypes.c_size_t]
        self.global_alloc.restype = wintypes.HGLOBAL
        self.global_lock = kernel32.GlobalLock
        self.global_lock.argtypes = [wintypes.HGLOBAL]
        self.global_lock.restype = wintypes.LPVOID
        self.global_unlock = kernel32.GlobalUnlock
        self.global_unlock.argtypes = [wintypes.HGLOBAL]
        self.global_unlock.restype = wintypes.BOOL
        self.global_free = kernel32.GlobalFree
        self.global_free.argtypes = [wintypes.HGLOBAL]
        self.global_free.restype = wintypes.HGLOBAL


@functools.cache
def _clipboard_api() -> _ClipboardApi:
    return _ClipboardApi()


def _open_clipboard(api: _ClipboardApi, hwnd: int) -> bool:
    for attempt in range(OPEN_RETRIES + 1):
        if api.open_clipboard(hwnd):
            return True
        if attempt < OPEN_RETRIES:
            time.sleep(OPEN_RETRY_DELAY_S)
    return False


def _put_unicode_text(api: _ClipboardApi, payload: bytes) -> bool:
    """Replaces the open clipboard's contents with `payload` (NUL-terminated UTF-16LE) as
    CF_UNICODETEXT. The memory belongs to the system once SetClipboardData succeeds."""
    if not api.empty_clipboard():
        return False
    memory = api.global_alloc(GMEM_MOVEABLE, len(payload))
    if not memory:
        return False
    placed = False
    try:
        pointer = api.global_lock(memory)
        if not pointer:
            return False
        try:
            ctypes.memmove(pointer, payload, len(payload))
        finally:
            api.global_unlock(memory)
        placed = bool(api.set_clipboard_data(CF_UNICODETEXT, memory))
        return placed
    finally:
        if not placed:
            api.global_free(memory)


def write_clipboard_text(hwnd: int, text: str) -> bool:
    """Puts `text` on the clipboard, rendered immediately, with `hwnd` as the owner window.

    Returns False when the clipboard could not be written; the caller then falls back to Tk.
    """
    try:
        api = _clipboard_api()
        payload = text.encode("utf-16-le", errors="surrogatepass") + b"\x00\x00"
        if not _open_clipboard(api, hwnd):
            return False
        try:
            return _put_unicode_text(api, payload)
        finally:
            api.close_clipboard()
    except Exception:  # noqa: BLE001 - any failure falls back to the Tk clipboard
        log.warning("writing the clipboard failed", exc_info=True)
        return False


class SystemInfoFeature:
    """State, widgets and flows of the System section, mixed into `App`.

    The panel keeps the section's state (`loaded`, `loading`, the snapshot text).
    """

    engine: EngineBridge | None
    system_panel: SystemPanel

    def _init_system_info_state(self) -> None:
        """Sets the section's state; runs before the window is built. The System section keeps its
        state in its panel, so there is nothing to set up before the panel exists."""

    def _build_system_info_tab(self, frame: ctk.CTkFrame) -> None:
        """Builds the section's panel into `frame`; handles a missing engine itself."""
        from ..widgets.sysinfo import SystemPanel

        self.system_panel = SystemPanel(
            frame, on_refresh=self.load_system_info, on_copy=self._copy_system_info
        )
        self.system_panel.grid(row=0, column=0, sticky="nsew", padx=4, pady=4)
        if self.engine is None:
            self.system_panel.refresh_button.configure(state="disabled")
        elif not self.engine.supports("sysinfo_snapshot"):
            self.system_panel.set_unsupported(UNSUPPORTED_TEXT)

    def _system_info_tab_shown(self) -> None:
        """Runs when the System section is shown while the engine is loaded."""
        if not self.system_panel.loaded and not self.system_panel.loading:
            self.load_system_info()

    def load_system_info(self) -> None:
        """Reads a new snapshot on the engine worker; the current cards stay until it arrives."""
        if self.engine is None or not self.engine.supports("sysinfo_snapshot"):
            return
        self.system_panel.set_loading()
        self.engine.sysinfo_snapshot(callback=self._system_info_loaded)

    def _system_info_loaded(self, future: Future[Any]) -> None:
        exc = future.exception()
        if exc is not None:
            log.warning("reading system information failed: %s", exc)
            self.system_panel.show_error(str(exc))
            return
        self.system_panel.show(future.result())

    def _copy_system_info(self, text: str) -> None:
        self._set_clipboard(text)
        self.set_status(COPIED_TEXT, theme.GOOD)  # type: ignore[attr-defined]

    def _set_clipboard(self, text: str) -> None:
        """Writes `text` to the clipboard natively, or through Tk when that fails."""
        if not write_clipboard_text(self.winfo_id(), text):  # type: ignore[attr-defined]
            self.clipboard_clear()  # type: ignore[attr-defined]
            self.clipboard_append(text)  # type: ignore[attr-defined]
