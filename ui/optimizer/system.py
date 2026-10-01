"""Windows process helpers: elevation and relaunch as administrator, the taskbar identity,
window activation, the parent console, message boxes, File Explorer, file dialogs and timer
resolution.

Mixins and widgets call these through the module (`from .. import system; system.x()`),
never `from ..system import x`, so tests can replace the ones that start processes, open
windows or dialogs, or change another window's state.
"""

from __future__ import annotations

import ctypes
import os
import re
import subprocess
import sys
from collections.abc import Mapping, MutableMapping, Sequence
from ctypes import wintypes
from pathlib import Path
from typing import Any

from . import APP_ID, DEV_APP_ID

_SW_SHOWNORMAL = 1
_MB_ICONS = {"error": 0x10, "warning": 0x30, "info": 0x40}
_MB_OK = 0x0
_MB_SETFOREGROUND = 0x10000
_ATTACH_PARENT_PROCESS = 0xFFFFFFFF
# Variables that make Tcl load its scripts from somewhere else than the runtime's own folder.
TCL_VARIABLES = ("TCL_LIBRARY", "TK_LIBRARY", "TCLLIBPATH", "TIX_LIBRARY")
# TCL8.6_TM_PATH, TCL8_6_TM_PATH and the same for every other version: Tcl puts their folders at
# the head of its module search path, where `package require` finds the modules Tk loads at
# every start. Windows and Tcl's `env` lookup ignore the case of variable names.
TCL_MODULE_PATH = re.compile(r"TCL\d+[._]\d+_TM_PATH", re.IGNORECASE | re.ASCII)
FOLDERID_DOCUMENTS = "{FDD39AD0-238F-46AF-ADB4-6C85480369C7}"


def is_admin() -> bool:
    try:
        return bool(ctypes.windll.shell32.IsUserAnAdmin())
    except (AttributeError, OSError):
        return False


def gui_interpreter() -> str:
    """pythonw.exe next to the running interpreter when present, so no console opens."""
    exe = Path(sys.executable)
    windowed = exe.with_name("pythonw.exe")
    return str(windowed if windowed.exists() else exe)


def installed_launcher() -> Path | None:
    """The installed Cairn.exe this process runs in, or None for a development run.

    The launcher embeds Python in isolated mode, so both its name and `sys.flags.isolated`
    are checked."""
    exe = Path(sys.executable)
    if exe.name.lower() == "cairn.exe" and sys.flags.isolated:
        return exe
    return None


def app_id() -> str:
    """AppUserModelID of this process: the installed app's, or the development one."""
    return APP_ID if installed_launcher() is not None else DEV_APP_ID


def set_app_id(app_id: str) -> bool:
    """Sets this process's AppUserModelID (taskbar grouping and icon); False on error."""
    try:
        shell32 = ctypes.WinDLL("shell32", use_last_error=True)
        fn = shell32.SetCurrentProcessExplicitAppUserModelID
        fn.argtypes = [wintypes.LPCWSTR]
        fn.restype = ctypes.c_long
        return fn(app_id) == 0
    except (AttributeError, OSError):
        return False


def relaunch_as_admin(after_pid: int | None = None) -> bool:
    """Starts Cairn elevated through a UAC prompt.

    The installed app starts its launcher; a development run starts `python -m optimizer`
    from the source tree. With `after_pid`, the new process first waits for that process to
    end (`--after PID`). Returns True when the elevated process was started (the caller
    then exits), False when the user declined the prompt or the launch failed.
    """
    shell32 = ctypes.WinDLL("shell32", use_last_error=True)
    shell32.ShellExecuteW.argtypes = [
        wintypes.HWND,
        wintypes.LPCWSTR,
        wintypes.LPCWSTR,
        wintypes.LPCWSTR,
        wintypes.LPCWSTR,
        ctypes.c_int,
    ]
    shell32.ShellExecuteW.restype = wintypes.HINSTANCE
    after = [] if after_pid is None else ["--after", str(int(after_pid))]
    launcher = installed_launcher()
    if launcher is not None:
        program, arguments, workdir = str(launcher), " ".join(after), str(launcher.parent)
    else:
        program = gui_interpreter()
        arguments = " ".join(["-m", "optimizer", *after])
        workdir = str(Path(__file__).resolve().parent.parent)
    result = shell32.ShellExecuteW(None, "runas", program, arguments or None, workdir, _SW_SHOWNORMAL)
    return int(result or 0) > 32


def bring_window_to_front(hwnd: int) -> bool:
    """Makes `hwnd` the foreground window; False when Windows refused."""
    try:
        user32 = ctypes.WinDLL("user32", use_last_error=True)
        user32.SetForegroundWindow.argtypes = [wintypes.HWND]
        user32.SetForegroundWindow.restype = wintypes.BOOL
        return bool(user32.SetForegroundWindow(hwnd))
    except (AttributeError, OSError):
        return False


def attach_parent_console() -> bool:
    """Attaches to the console of the process that started this one, such as a terminal
    running `Cairn.exe --check`, and prints there; False when that process has no console.
    For a windowed process only, which starts without stdout."""
    try:
        kernel32 = ctypes.WinDLL("kernel32", use_last_error=True)
        kernel32.AttachConsole.argtypes = [wintypes.DWORD]
        kernel32.AttachConsole.restype = wintypes.BOOL
        if not kernel32.AttachConsole(_ATTACH_PARENT_PROCESS):
            return False
        # Stays open as the new stdout.
        console = open("CONOUT$", "w", encoding="utf-8", errors="replace")
    except (AttributeError, OSError):
        return False
    sys.stdout = console
    if sys.stderr is None:
        sys.stderr = console
    return True


def message_box(title: str, text: str, icon: str = "error") -> None:
    """A native message box with an OK button, for use before Tk exists. `icon` is "error",
    "warning" or "info"."""
    user32 = ctypes.WinDLL("user32", use_last_error=True)
    user32.MessageBoxW.argtypes = [wintypes.HWND, wintypes.LPCWSTR, wintypes.LPCWSTR, wintypes.UINT]
    user32.MessageBoxW.restype = ctypes.c_int
    flags = _MB_OK | _MB_SETFOREGROUND | _MB_ICONS.get(icon, _MB_ICONS["error"])
    user32.MessageBoxW(None, text, title, flags)


class TimerResolution:
    """Raises the system timer resolution to 1 ms while held.

    Tk schedules `after` callbacks on the Windows timer, whose default 15.6 ms tick turns
    a 16 ms frame into a 15.6 / 31.2 ms alternation; 1 ms resolution keeps a steady 60 Hz.
    """

    def __init__(self, period_ms: int = 1) -> None:
        self._period = period_ms
        self._active = False

    def __enter__(self) -> TimerResolution:
        try:
            self._active = ctypes.windll.winmm.timeBeginPeriod(self._period) == 0
        except (AttributeError, OSError):
            self._active = False
        return self

    def __exit__(self, *exc: object) -> None:
        if self._active:
            ctypes.windll.winmm.timeEndPeriod(self._period)
            self._active = False


def open_uri(uri: str) -> None:
    """Opens a URI (for example an ms-windows-store: link) with its registered handler."""
    os.startfile(uri)  # noqa: S606 - URIs come from the engine's own Store-link format


def open_folder(path: str | Path) -> None:
    """Opens the folder `path` in File Explorer."""
    os.startfile(str(path))  # noqa: S606 - a folder Cairn names itself


def _windows_dir() -> str:
    """The Windows folder as the system reports it, never from the environment."""
    kernel32 = ctypes.WinDLL("kernel32", use_last_error=True)
    kernel32.GetWindowsDirectoryW.argtypes = [wintypes.LPWSTR, wintypes.UINT]
    kernel32.GetWindowsDirectoryW.restype = wintypes.UINT
    buffer = ctypes.create_unicode_buffer(260)
    length = kernel32.GetWindowsDirectoryW(buffer, len(buffer))
    if not 0 < length < len(buffer):
        raise OSError("the Windows folder could not be read")
    return buffer.value


def show_in_explorer(path: str, *, select: bool) -> None:
    """Opens File Explorer on the folder `path`, or on its folder with `path` selected (`select`).

    explorer.exe hands the request to the signed-in user's running shell, so the window is not
    elevated even when Cairn is (when Cairn runs as another administrator account, Explorer
    opens as that account)."""
    explorer = os.path.join(_windows_dir(), "explorer.exe")
    target = path[4:] if path.startswith("\\\\?\\") else path
    # Windows paths cannot contain '"', so quoting is enough.
    command = f'"{explorer}" /select,"{target}"' if select else f'"{explorer}" "{target}"'
    subprocess.Popen(command, close_fds=True)  # noqa: S603 - absolute explorer.exe, quoted path


def ask_folder(parent: Any, *, title: str) -> str:
    """Modal folder picker for an existing folder; "" when cancelled."""
    from tkinter import filedialog

    chosen = filedialog.askdirectory(parent=parent, mustexist=True, title=title)
    return os.path.normpath(chosen) if isinstance(chosen, str) and chosen else ""


def documents_dir() -> str:
    """The Documents known folder, else the profile folder; "" when neither can be read."""
    try:
        shell32 = ctypes.WinDLL("shell32", use_last_error=True)
        ole32 = ctypes.WinDLL("ole32", use_last_error=True)
        guid = (ctypes.c_byte * 16)()
        ole32.CLSIDFromString.argtypes = [wintypes.LPCWSTR, ctypes.c_void_p]
        ole32.CLSIDFromString.restype = ctypes.c_long
        shell32.SHGetKnownFolderPath.argtypes = [
            ctypes.c_void_p,
            wintypes.DWORD,
            wintypes.HANDLE,
            ctypes.POINTER(ctypes.c_wchar_p),
        ]
        shell32.SHGetKnownFolderPath.restype = ctypes.c_long
        ole32.CoTaskMemFree.argtypes = [ctypes.c_void_p]
        ole32.CoTaskMemFree.restype = None
        if ole32.CLSIDFromString(FOLDERID_DOCUMENTS, ctypes.byref(guid)) == 0:
            found = ctypes.c_wchar_p()
            result = shell32.SHGetKnownFolderPath(ctypes.byref(guid), 0, None, ctypes.byref(found))
            try:
                if result == 0 and found.value:
                    return found.value
            finally:
                ole32.CoTaskMemFree(found)
    except (AttributeError, OSError):
        pass
    profile = os.environ.get("USERPROFILE") or os.path.expanduser("~")
    return profile if profile and profile != "~" and os.path.isdir(profile) else ""


def _dialog_result(chosen: object) -> str:
    return chosen if isinstance(chosen, str) else ""


def ask_open_path(parent: Any, *, title: str, filetypes: Sequence[tuple[str, str]]) -> str:
    """Modal Open dialog starting in Documents; "" when cancelled."""
    from tkinter import filedialog

    return _dialog_result(
        filedialog.askopenfilename(
            parent=parent, title=title, filetypes=list(filetypes), initialdir=documents_dir() or None
        )
    )


def ask_save_path(
    parent: Any,
    *,
    title: str,
    filetypes: Sequence[tuple[str, str]],
    initialfile: str,
    defaultextension: str = ".json",
) -> str:
    """Modal Save dialog starting in Documents that asks before overwriting; "" when cancelled."""
    from tkinter import filedialog

    return _dialog_result(
        filedialog.asksaveasfilename(
            parent=parent,
            title=title,
            filetypes=list(filetypes),
            initialfile=initialfile,
            defaultextension=defaultextension,
            confirmoverwrite=True,
            initialdir=documents_dir() or None,
        )
    )


def scrub_tcl_environment(env: MutableMapping[str, str] = os.environ) -> list[str]:
    """Removes the variables that point Tcl and Tk at other script folders; returns the names
    that were set."""
    removed = [name for name in TCL_VARIABLES if name in env]
    for name in removed:
        del env[name]
    return removed


def tcl_module_path_variables(env: Mapping[str, str]) -> list[str]:
    """The names in `env` that add folders to Tcl's module search path (`TCL_MODULE_PATH`)."""
    return [name for name in env if TCL_MODULE_PATH.fullmatch(name)]


def scrub_tcl_module_paths(env: MutableMapping[str, str] = os.environ) -> list[str]:
    """Removes the variables that add folders to Tcl's module search path; returns their names."""
    removed = tcl_module_path_variables(env)
    for name in removed:
        del env[name]
    return removed


def installed_tcl_problem(env: Mapping[str, str], prefix: str) -> str | None:
    """For the installed launcher, which pins Tcl and Tk to the install's own script folders
    under `prefix` (`sys.prefix`) and removes the other Tcl variables, the module-path ones
    included: the variable that differs, or None."""
    expected = {
        "TCL_LIBRARY": str(Path(prefix) / "tcl" / "tcl8.6"),
        "TK_LIBRARY": str(Path(prefix) / "tcl" / "tk8.6"),
    }
    for name, value in expected.items():
        if env.get(name) != value:
            return name
    for name in ("TCLLIBPATH", "TIX_LIBRARY"):
        if name in env:
            return name
    module_paths = tcl_module_path_variables(env)
    return module_paths[0] if module_paths else None
