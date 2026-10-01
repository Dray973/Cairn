"""Helpers for tests that drive the real window with a FakeEngine.

Importing this module skips the importing test module when CustomTkinter or the telemetry
DLL is missing, so app-level test modules import the window and its helpers from here.
The `make_app` fixture (see conftest.py) creates the windows.
"""

from __future__ import annotations

import time
import tkinter as tk
from collections.abc import Callable, Iterator
from typing import Any

import pytest

from optimizer import NATIVE_DIR, theme
from optimizer.bridge.engine import EngineBridge
from optimizer.sections import SECTIONS

from .fake_engine import FakeEngine

ctk = pytest.importorskip("customtkinter")
if not (NATIVE_DIR / "optimizer_telemetry.dll").is_file():
    pytest.skip("optimizer_telemetry.dll is not deployed", allow_module_level=True)

from optimizer.app import REVERT_RESTART_TEXT, App  # noqa: E402
from optimizer.system import TimerResolution  # noqa: E402
from optimizer.widgets.controls import STATUS_UNKNOWN, ScanRow  # noqa: E402
from optimizer.widgets.dialogs import MessageDialog  # noqa: E402

__all__ = [
    "ADMIN_DIALOG_TITLE",
    "FIT_SIZES",
    "REVERT_RESTART_TEXT",
    "SECTIONS",
    "STATUS_UNKNOWN",
    "App",
    "AppFactory",
    "EngineBridge",
    "FakeEngine",
    "MessageDialog",
    "ScanRow",
    "TimerResolution",
    "assert_section_fits",
    "confirm_dialog",
    "confirm_dialog_text",
    "ctk",
    "dialog_text",
    "dialogs",
    "idle",
    "next_dialog",
    "pump",
    "resize_to",
    "rows_by_id",
    "scanned",
    "section_layout_problems",
    "show_section",
    "theme",
]

# The `make_app` fixture: `make_app(elevated=..., **fake_engine_options)`.
AppFactory = Callable[..., tuple[App, FakeEngine]]

# Title of the dialog that offers to relaunch elevated; confirming it would start a UAC prompt.
ADMIN_DIALOG_TITLE = "Administrator rights needed"

# Window sizes every section must fit (logical px): the minimum size, a 1920x1080 screen at
# 150 % (the work area limits the window) and the smallest window before content scrolls.
FIT_SIZES = ((1120, 700), (1240, 630), (1000, 600))
# Widgets whose text must be fully visible unless they sit in a scrollable frame.
FIT_TYPES = (ctk.CTkLabel, ctk.CTkButton, ctk.CTkSwitch, ctk.CTkCheckBox, ctk.CTkOptionMenu)


def pump(app: App, seconds: float, until: Callable[[], bool] | None = None) -> None:
    deadline = time.perf_counter() + seconds
    while time.perf_counter() < deadline:
        app.update()
        if until is not None and until():
            return
        time.sleep(0.001)
    if until is not None:
        raise AssertionError("condition not reached while pumping the event loop")


def show_section(app: App, name: str, *, run_hook: bool = True) -> None:
    """Shows section `name` as a click on its sidebar row does (with `run_hook`), or only lays
    it out."""
    app.show_section(name, run_hook=run_hook)


def resize_to(app: App, width: int, height: int) -> None:
    """Sets the window to `width` x `height` logical pixels and waits until it is drawn."""
    scale = ctk.ScalingTracker.get_window_scaling(app) or 1.0
    app.geometry(f"{width}x{height}")
    sizes = {(width, height), (round(width * scale), round(height * scale))}
    pump(app, 5.0, until=lambda: (app.winfo_width(), app.winfo_height()) in sizes)
    pump(app, 0.3)


def _fit_widgets(widget: Any) -> Iterator[Any]:
    """The widgets under `widget` whose text must fit, skipping scrollable frames' content."""
    for child in widget.winfo_children():
        if isinstance(child, ctk.CTkScrollableFrame):
            continue
        if isinstance(child, FIT_TYPES):
            yield child
        else:
            yield from _fit_widgets(child)


def _text(widget: Any) -> str:
    try:
        return str(widget.cget("text"))[:50]
    except (ValueError, tk.TclError):
        return ""


def section_layout_problems(app: App, name: str) -> list[str]:
    """Shown labels, buttons, switches, check boxes and option menus of section `name`, outside
    scrollable frames, that stick out of the section's frame or get less width than their text
    needs (1 px tolerance)."""
    frame = app.section_frame(name)
    left, top = frame.winfo_rootx(), frame.winfo_rooty()
    width, height = frame.winfo_width(), frame.winfo_height()
    problems = []
    for widget in _fit_widgets(frame):
        if not widget.winfo_ismapped():
            continue
        x0, y0 = widget.winfo_rootx() - left, widget.winfo_rooty() - top
        x1, y1 = x0 + widget.winfo_width(), y0 + widget.winfo_height()
        kind = type(widget).__name__
        if x0 < -1 or y0 < -1 or x1 > width + 1 or y1 > height + 1:
            problems.append(f"{kind} {_text(widget)!r} spans {x0},{y0}..{x1},{y1} of {width}x{height}")
        inner = widget._label if isinstance(widget, ctk.CTkLabel) else widget
        need = inner.winfo_reqwidth()
        if need > widget.winfo_width() + 1:
            problems.append(f"{kind} {_text(widget)!r} needs {need} px and gets {widget.winfo_width()}")
    return problems


def assert_section_fits(app: App, name: str, sizes: tuple[tuple[int, int], ...] = FIT_SIZES) -> None:
    """Section `name` fits each of `sizes`: nothing outside a scrollable frame sticks out of
    the section or is cut off. Shows the section without its hook, then puts back the window
    size and the section that was shown."""
    shown = app.current_section
    geometry = app.geometry()
    app.minsize(900, 560)
    for width, height in sizes:
        resize_to(app, width, height)
        show_section(app, name, run_hook=False)
        pump(app, 5.0, until=lambda: idle(app))
        pump(app, 0.3)
        problems = section_layout_problems(app, name)
        assert problems == [], f"{name} at {width}x{height}: {problems}"
    app.geometry(geometry)
    show_section(app, shown, run_hook=False)
    pump(app, 0.3)
    assert app.errors == []


def dialogs(app: App) -> list[MessageDialog]:
    return [w for w in app.winfo_children() if isinstance(w, MessageDialog) and w.winfo_exists()]


def next_dialog(app: App) -> MessageDialog:
    pump(app, 5.0, until=lambda: bool(dialogs(app)))
    return dialogs(app)[-1]


def _refuse_admin_dialog(dialog: MessageDialog) -> None:
    if dialog.title_text == ADMIN_DIALOG_TITLE:
        raise AssertionError("tests must never confirm the administrator relaunch dialog")


def confirm_dialog(app: App) -> MessageDialog:
    """Confirms the next dialog. Refuses the administrator relaunch dialog."""
    dialog = next_dialog(app)
    _refuse_admin_dialog(dialog)
    dialog.confirm_button.invoke()
    return dialog


def confirm_dialog_text(app: App) -> str:
    """Confirms the next dialog and returns its text, read while it still exists. Refuses the
    administrator relaunch dialog."""
    dialog = next_dialog(app)
    _refuse_admin_dialog(dialog)
    text = dialog_text(dialog)
    dialog.confirm_button.invoke()
    return text


def dialog_text(dialog: MessageDialog) -> str:
    """Heading, message and detail lines of a dialog."""
    parts = []
    for widget in dialog.winfo_children():
        if isinstance(widget, ctk.CTkLabel):
            parts.append(str(widget.cget("text")))
        elif isinstance(widget, ctk.CTkTextbox):
            parts.append(widget.get("1.0", "end"))
    return "\n".join(parts)


def idle(app: App) -> bool:
    """No engine call is queued or running, so a new change passes the busy guard."""
    return app.engine is not None and not app.engine.busy and not app._busy


def scanned(app: App) -> bool:
    return app._last_scan is not None and idle(app)


def rows_by_id(app: App) -> dict[str, ScanRow]:
    return {r.item["id"]: r for r in app.optimize_list.rows + app.apps_list.rows}
