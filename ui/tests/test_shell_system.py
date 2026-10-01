"""Process helpers of `optimizer.system`: which copy runs (installed launcher or development),
its taskbar identity and the Tcl environment scrub; and the engine's reading of the installed
copy. Nothing is started, shown or changed."""

from __future__ import annotations

import os
import sys
from pathlib import Path
from types import SimpleNamespace

import pytest

from optimizer import APP_ID, DEV_APP_ID, system
from optimizer.bridge.engine import EngineUnavailable, load_engine_module


def pretend(monkeypatch: pytest.MonkeyPatch, executable: str, *, isolated: int) -> None:
    monkeypatch.setattr(sys, "executable", executable)
    monkeypatch.setattr(sys, "flags", SimpleNamespace(isolated=isolated))


def test_the_installed_launcher_is_recognised(monkeypatch: pytest.MonkeyPatch) -> None:
    pretend(monkeypatch, r"C:\Program Files\Cairn\Cairn.exe", isolated=1)
    assert system.installed_launcher() == Path(r"C:\Program Files\Cairn\Cairn.exe")
    assert system.app_id() == APP_ID == "Cairn.App"
    pretend(monkeypatch, r"C:\Program Files\Cairn\CAIRN.EXE", isolated=1)
    assert system.installed_launcher() == Path(r"C:\Program Files\Cairn\CAIRN.EXE"), "case is ignored"


def test_a_development_run_is_not_the_launcher(monkeypatch: pytest.MonkeyPatch) -> None:
    for executable, isolated in (
        (r"X:\src\Cairn\.venv\Scripts\python.exe", 0),
        (r"X:\src\Cairn\.venv\Scripts\pythonw.exe", 1),
        # The launcher always runs Python isolated; a file merely named Cairn.exe does not count.
        (r"C:\Users\Test\Downloads\Cairn.exe", 0),
        (r"C:\Program Files\Cairn\Cairn.exe.bak", 1),
    ):
        pretend(monkeypatch, executable, isolated=isolated)
        assert system.installed_launcher() is None, executable
        assert system.app_id() == DEV_APP_ID == "Cairn.App.Dev"


def test_this_test_run_is_a_development_run() -> None:
    assert system.installed_launcher() is None
    assert system.app_id() == DEV_APP_ID


def test_scrub_removes_only_the_tcl_variables() -> None:
    env = {
        "TCL_LIBRARY": r"C:\Users\Test\tcl",
        "TK_LIBRARY": r"C:\Users\Test\tk",
        "TCLLIBPATH": r"C:\x",
        "TIX_LIBRARY": "",
        "PATH": r"C:\Windows",
        "OPTIMIZER_DATA_DIR": r"C:\data",
    }
    assert system.scrub_tcl_environment(env) == ["TCL_LIBRARY", "TK_LIBRARY", "TCLLIBPATH", "TIX_LIBRARY"]
    assert env == {"PATH": r"C:\Windows", "OPTIMIZER_DATA_DIR": r"C:\data"}
    assert system.scrub_tcl_environment(env) == []
    assert system.TCL_VARIABLES == ("TCL_LIBRARY", "TK_LIBRARY", "TCLLIBPATH", "TIX_LIBRARY")


def test_scrub_defaults_to_the_process_environment(monkeypatch: pytest.MonkeyPatch) -> None:
    monkeypatch.setenv("TCLLIBPATH", r"C:\Users\Test\planted")
    monkeypatch.delenv("TIX_LIBRARY", raising=False)
    removed = system.scrub_tcl_environment()
    assert "TCLLIBPATH" in removed and "TIX_LIBRARY" not in removed
    assert "TCLLIBPATH" not in os.environ


def test_module_path_scrub_removes_every_spelling_of_the_variables() -> None:
    planted = r"C:\Users\Test\planted"
    env = {
        "TCL8.6_TM_PATH": planted,
        "TCL8_6_TM_PATH": planted,
        "tcl8_0_tm_path": planted,
        "TCL9.0_TM_PATH": planted,
        "TCL8_TM_PATH": "x",
        "TCL_TM_PATH": "x",
        "TCL_LIBRARY": r"C:\Users\Test\tcl",
        "PATH": r"C:\Windows",
    }
    assert system.scrub_tcl_module_paths(env) == [
        "TCL8.6_TM_PATH",
        "TCL8_6_TM_PATH",
        "tcl8_0_tm_path",
        "TCL9.0_TM_PATH",
    ]
    assert env == {
        "TCL8_TM_PATH": "x",
        "TCL_TM_PATH": "x",
        "TCL_LIBRARY": r"C:\Users\Test\tcl",
        "PATH": r"C:\Windows",
    }
    assert system.scrub_tcl_module_paths(env) == []
    assert system.TCL_VARIABLES == ("TCL_LIBRARY", "TK_LIBRARY", "TCLLIBPATH", "TIX_LIBRARY"), "unchanged"


def _tcl_module_path() -> list[str]:
    """The module search path of a new Tcl interpreter (no window), normalized for comparison."""
    import tkinter

    interpreter = tkinter.Tcl()
    found = interpreter.splitlist(interpreter.eval("::tcl::tm::path list"))
    return [os.path.normcase(os.path.normpath(str(path))) for path in found]


def test_module_path_scrub_takes_the_folder_off_tcl_s_module_path(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch
) -> None:
    planted = os.path.normcase(os.path.normpath(tmp_path))
    monkeypatch.setenv("TCL8_6_TM_PATH", str(tmp_path))
    assert planted in _tcl_module_path(), "Tcl searches the folder the variable names"
    assert "TCL8_6_TM_PATH" in system.scrub_tcl_module_paths()
    assert "TCL8_6_TM_PATH" not in os.environ
    assert planted not in _tcl_module_path()


def test_the_windowed_interpreter_is_next_to_python(tmp_path: Path, monkeypatch: pytest.MonkeyPatch) -> None:
    python = tmp_path / "python.exe"
    python.write_bytes(b"")
    monkeypatch.setattr(sys, "executable", str(python))
    assert system.gui_interpreter() == str(python), "no pythonw.exe: the interpreter itself"
    (tmp_path / "pythonw.exe").write_bytes(b"")
    assert system.gui_interpreter() == str(tmp_path / "pythonw.exe")


def test_documents_folder_is_read_only() -> None:
    found = system.documents_dir()
    assert found == "" or Path(found).is_dir()


def test_process_helpers_are_reached_through_the_module() -> None:
    # Tests replace these on the module, so the window and the mixins must look them up there.
    import optimizer.__main__ as main_module
    import optimizer.app as app_module

    for module in (app_module, main_module):
        for name in ("relaunch_as_admin", "bring_window_to_front", "message_box", "open_folder", "open_uri"):
            assert name not in vars(module), f"{module.__name__} imports {name} directly"


def test_the_real_installed_copy_is_none_or_complete() -> None:
    # Reads the installer's uninstall entry only.
    try:
        module = load_engine_module()
    except EngineUnavailable as exc:
        pytest.skip(str(exc))
    if not callable(getattr(module, "install_info", None)):
        pytest.skip("the deployed engine module has no install_info")
    info = module.install_info()
    if info is None:
        return
    assert set(info) == {"dir", "version", "launcher", "cli"}
    assert all(isinstance(value, str) for value in info.values()), info
    assert Path(info["launcher"]) == Path(info["dir"]) / "Cairn.exe"
    assert Path(info["cli"]) == Path(info["dir"]) / "optctl.exe"
    assert Path(info["launcher"]).is_file() and Path(info["cli"]).is_file()
