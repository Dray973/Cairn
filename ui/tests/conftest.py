"""Test-wide guards and the `make_app` fixture.

Guards: no test may create a System Restore point, run a disk speed test at a volume root,
search Windows Update online or install or upgrade apps (the engine refuses each while its
`OPTIMIZER_FORBID_*` variable is "1"), and no test opens the user's real journal or data
folder: `OPTIMIZER_DATA_DIR` points at a throwaway folder before anything loads the engine,
and `tmp_path` folders are created inside it unless `--basetemp` is given.
No test may relaunch the process elevated, bring a window to the front, show a native message
box, open File Explorer, a folder, a URI or a file dialog, or write the clipboard: those
process helpers are replaced by stubs that fail the test that reaches them (a test that needs
one replaces it with a recorder), and every window's Tk clipboard calls go to a list on the
window (`app._test_clipboard`).
The windows of a run are created on a separate desktop that is never shown, so they neither
appear on screen nor take the keyboard focus; `OPTIMIZER_SHOW_TEST_WINDOWS=1` keeps them on
the user's desktop.
"""

from __future__ import annotations

import ctypes
import gc
import importlib
import os
import shutil
import tempfile
from collections.abc import Iterator
from ctypes import wintypes
from pathlib import Path
from typing import TYPE_CHECKING, Any

import pytest

os.environ["OPTIMIZER_FORBID_RESTORE_POINT"] = "1"
os.environ["OPTIMIZER_FORBID_DRIVE_TESTS"] = "1"
os.environ["OPTIMIZER_FORBID_UPDATE_SEARCH"] = "1"
os.environ["OPTIMIZER_FORBID_APP_INSTALLS"] = "1"
# Real-module calls that open the default journal use a throwaway data folder, set before any
# test imports the engine; tests that set their own folder with monkeypatch keep working.
TEST_DATA_DIR = Path(tempfile.mkdtemp(prefix="cairn-pytest-"))
os.environ["OPTIMIZER_DATA_DIR"] = str(TEST_DATA_DIR)

TEST_DESKTOP = "CairnTests"
GENERIC_ALL = 0x10000000


def _use_hidden_desktop() -> bool:
    """Moves the calling thread to the tests' desktop; False when Windows refuses, which
    leaves the thread on the user's desktop. A thread can move only while it owns no window,
    so this runs before anything creates one."""
    user32 = ctypes.WinDLL("user32", use_last_error=True)
    user32.CreateDesktopW.restype = wintypes.HANDLE
    user32.CreateDesktopW.argtypes = [
        wintypes.LPCWSTR,
        wintypes.LPCWSTR,
        ctypes.c_void_p,
        wintypes.DWORD,
        wintypes.DWORD,
        ctypes.c_void_p,
    ]
    user32.SetThreadDesktop.restype = wintypes.BOOL
    user32.SetThreadDesktop.argtypes = [wintypes.HANDLE]
    # The handle stays open for the life of the process: the desktop goes with its last user.
    desktop = user32.CreateDesktopW(TEST_DESKTOP, None, None, 0, GENERIC_ALL, None)
    return bool(desktop) and bool(user32.SetThreadDesktop(desktop))


if os.environ.get("OPTIMIZER_SHOW_TEST_WINDOWS") != "1":
    _use_hidden_desktop()

# `optimizer.system` helpers that start processes or open windows or dialogs.
PROCESS_HELPERS = (
    "relaunch_as_admin",
    "bring_window_to_front",
    "message_box",
    "open_folder",
    "show_in_explorer",
    "ask_folder",
    "ask_open_path",
    "ask_save_path",
    "open_uri",
)


@pytest.hookimpl(tryfirst=True)
def pytest_configure(config: pytest.Config) -> None:
    # tmp_path folders live in the throwaway folder too, unless --basetemp names another: the
    # shared default root (%TEMP%\pytest-of-<user>) may be unreadable after a run at a
    # different integrity level.
    if not config.option.basetemp:
        config.option.basetemp = str(TEST_DATA_DIR / "pytest")


def pytest_sessionfinish(session: pytest.Session, exitstatus: int) -> None:
    shutil.rmtree(TEST_DATA_DIR, ignore_errors=True)


if TYPE_CHECKING:
    from .app_support import App, AppFactory, FakeEngine


def _failing_stub(name: str, reached: list[str]) -> Any:
    def stub(*_args: Any, **_kwargs: Any) -> Any:
        reached.append(name)
        pytest.fail(f"a test reached the real {name}")

    return stub


@pytest.fixture(autouse=True)
def _no_real_side_effects(monkeypatch: pytest.MonkeyPatch) -> Iterator[None]:
    """Fails any test that reaches one of the process helpers or the native clipboard."""
    from optimizer import system

    reached: list[str] = []
    for name in PROCESS_HELPERS:
        monkeypatch.setattr(system, name, _failing_stub(name, reached))
    try:
        system_info = importlib.import_module("optimizer.features.system_info")
    except ImportError:
        system_info = None
    if system_info is not None:
        monkeypatch.setattr(
            system_info, "write_clipboard_text", _failing_stub("write_clipboard_text", reached)
        )
    yield
    # A stub reached from a Tk or engine callback may have had its failure swallowed.
    assert reached == [], f"a test reached the real {', '.join(reached)}"


@pytest.fixture
def make_app() -> Iterator[AppFactory]:
    """Creates windows backed by a FakeEngine: `make_app(elevated=..., other_user=...,
    **fake_engine_options)`; `other_user` is what the engine says about the account.

    Automatic garbage collection is off while the test runs, so no worker thread finalizes Tk
    objects; teardown closes every window still running, waits for its engine worker and
    collects on the Tk thread.
    """
    from .app_support import App, EngineBridge, FakeEngine, TimerResolution

    created: list[App] = []
    timer = TimerResolution()
    timer.__enter__()
    gc.disable()

    def factory(
        *, elevated: bool, other_user: bool | None = False, **engine_options: Any
    ) -> tuple[App, FakeEngine]:
        engine = FakeEngine(elevated=elevated, other_user=other_user, **engine_options)
        app = App(engine=EngineBridge(module=engine), elevated=elevated)  # type: ignore[arg-type]
        clipboard: list[str] = []
        app._test_clipboard = clipboard  # type: ignore[attr-defined]
        app.clipboard_clear = lambda *_a, **_k: clipboard.clear()  # type: ignore[method-assign]
        app.clipboard_append = lambda text, **_k: clipboard.append(text)  # type: ignore[method-assign]
        created.append(app)
        return app, engine

    try:
        yield factory
    finally:
        try:
            for app in created:
                if app._running:
                    app._request_close(force=True)
                if app.engine is not None:
                    app.engine.shutdown(wait=True)
            created.clear()
            # Finalize destroyed Tk objects here, on the Tk thread, rather than whenever a
            # later worker thread happens to trigger a collection.
            gc.collect()
        finally:
            gc.enable()
            timer.__exit__(None, None, None)
