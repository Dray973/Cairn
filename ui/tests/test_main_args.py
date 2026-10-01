"""`python -m optimizer` and Cairn.exe's Python side (`optimizer.__main__`): the command line,
the read-only `--check` and `--self-test` runs (no log files), the installed copy's Tcl check,
the single-instance paths and the last-resort error message. The window, the lock, the log
setup and every process helper are replaced with recorders."""

from __future__ import annotations

import json
import logging
import os
import sys
from collections.abc import Iterator
from pathlib import Path
from typing import Any

import pytest

import optimizer.app
from optimizer import APP_NAME, DEV_APP_ID, __main__, __version__, instance, logsetup, paths, selftest, system
from optimizer.__main__ import (
    ALREADY_OPEN_TEXT,
    EXIT_REPORTED,
    UNSAFE_TCL_TEXT,
    build_parser,
    main,
)

PREFIX = r"C:\Program Files\Cairn"


@pytest.fixture(autouse=True)
def restore_logging() -> Iterator[None]:
    root = logging.getLogger()
    handlers, level = list(root.handlers), root.level
    try:
        yield
    finally:
        for handler in list(root.handlers):
            if handler not in handlers:
                root.removeHandler(handler)
                handler.close()
        root.setLevel(level)


def _refuse(name: str) -> Any:
    def refuse(*_args: object, **_kwargs: object) -> Any:
        raise AssertionError(f"{name} must not be called")

    return refuse


class FakeLock:
    def __init__(self, events: list[str] | None = None) -> None:
        self.entered = 0
        self.exited = 0
        self._events = events

    def __enter__(self) -> FakeLock:
        self.entered += 1
        return self

    def __exit__(self, *exc: object) -> None:
        self.exited += 1
        if self._events is not None:
            self._events.append("lock released")


class EngineRecorder:
    """The window's engine bridge: records each shutdown in `events`; `error` makes it raise."""

    def __init__(self, events: list[str], error: Exception | None = None) -> None:
        self._events = events
        self._error = error

    def shutdown(self, wait: bool = False) -> None:
        self._events.append(f"engine shutdown wait={wait}")
        if self._error is not None:
            raise self._error


class NoTimer:
    def __enter__(self) -> NoTimer:
        return self

    def __exit__(self, *exc: object) -> None:
        return None


class Recorder:
    """Stands in for the interactive start's collaborators and records what they were asked."""

    def __init__(self, monkeypatch: pytest.MonkeyPatch, tmp_path: Path) -> None:
        self.log_path = tmp_path / "logs" / "cairn.log"
        self.configured: list[str] = []
        self.app_ids: list[str] = []
        self.scrubbed = 0
        self.module_paths_scrubbed = 0
        self.acquired: list[int | None] = []
        self.messages: list[tuple[str, str, str]] = []
        self.windows: list[dict[str, Any]] = []
        self.lock: FakeLock | None = FakeLock()
        self.signal = False
        self.waited: FakeLock | None = None
        self.window_error: Exception | None = None
        self.engine: EngineRecorder | None = None
        self.launcher: Path | None = None
        monkeypatch.setattr(logsetup, "configure", self._configure)
        monkeypatch.setattr(system, "set_app_id", lambda app_id: self.app_ids.append(app_id) or True)
        monkeypatch.setattr(system, "scrub_tcl_environment", self._scrub)
        monkeypatch.setattr(system, "scrub_tcl_module_paths", self._scrub_module_paths)
        monkeypatch.setattr(system, "installed_launcher", lambda: self.launcher)
        monkeypatch.setattr(system, "message_box", self._message_box)
        monkeypatch.setattr(system, "TimerResolution", NoTimer)
        monkeypatch.setattr(instance.InstanceLock, "acquire", self._acquire)
        monkeypatch.setattr(instance, "signal_running_instance", lambda *a, **k: self.signal)
        monkeypatch.setattr(instance, "acquire_when_free", lambda *a, **k: self.waited)
        monkeypatch.setattr(optimizer.app, "App", self._window)

    def _configure(self, level: str, **_kwargs: object) -> Path:
        self.configured.append(level)
        return self.log_path

    def _scrub(self, *_args: object) -> list[str]:
        self.scrubbed += 1
        return []

    def _scrub_module_paths(self, *_args: object) -> list[str]:
        self.module_paths_scrubbed += 1
        return []

    def _acquire(self, *_args: object, wait_for_pid: int | None = None, **_kwargs: object) -> FakeLock | None:
        self.acquired.append(wait_for_pid)
        return self.lock

    def _message_box(self, title: str, text: str, icon: str = "error") -> None:
        self.messages.append((title, text, icon))

    def _window(self, **kwargs: Any) -> Any:
        recorder = self

        class Window:
            def __init__(self) -> None:
                self.engine = recorder.engine

            def mainloop(self) -> None:
                recorder.windows.append(kwargs)
                if recorder.window_error is not None:
                    raise recorder.window_error

        return Window()


@pytest.fixture
def recorder(monkeypatch: pytest.MonkeyPatch, tmp_path: Path) -> Recorder:
    return Recorder(monkeypatch, tmp_path)


def test_parser_reads_the_launcher_arguments() -> None:
    args = build_parser().parse_args(["--after", "12", "--no-elevate", "--start-note", "elevation-declined"])
    assert args.after == 12
    assert args.no_elevate
    assert args.start_note == "elevation-declined"
    assert not args.check and args.self_test is None
    for note in ("elevation-failed:1223", "elevation-failed:0x80070005"):
        assert build_parser().parse_args(["--start-note", note]).start_note == note
    plain = build_parser().parse_args([])
    assert (plain.after, plain.no_elevate, plain.start_note) == (None, False, None)
    assert build_parser().description == APP_NAME


@pytest.mark.parametrize(
    "argv",
    [
        ["--after", "x"],
        ["--after"],
        ["--start-note", "bogus"],
        ["--start-note", "elevation-failed:"],
        ["--aft", "12"],
        ["--self-test"],
        ["--unknown"],
    ],
)
def test_parser_rejects_bad_arguments(argv: list[str], capsys: pytest.CaptureFixture[str]) -> None:
    with pytest.raises(SystemExit) as raised:
        build_parser().parse_args(argv)
    assert raised.value.code == 2
    capsys.readouterr()


def test_self_test_creates_no_log_folder(tmp_path: Path, monkeypatch: pytest.MonkeyPatch) -> None:
    sentinel = tmp_path / "data"
    monkeypatch.setattr(paths, "data_dir", lambda *_a, **_k: sentinel)
    monkeypatch.setattr(paths, "log_dir", lambda *_a, **_k: sentinel / "logs")
    monkeypatch.setattr(logsetup, "configure", _refuse("logsetup.configure"))
    monkeypatch.setattr(instance.InstanceLock, "acquire", _refuse("InstanceLock.acquire"))
    runs: list[Path] = []
    monkeypatch.setattr(selftest, "run", lambda path: runs.append(path) or 0)
    report = tmp_path / "s.json"
    assert main(["--self-test", str(report)]) == 0
    assert runs == [report]
    assert not sentinel.exists()
    assert [p.name for p in tmp_path.iterdir()] == []


def test_a_failed_self_test_is_exit_code_1(tmp_path: Path, monkeypatch: pytest.MonkeyPatch) -> None:
    monkeypatch.setattr(logsetup, "configure", _refuse("logsetup.configure"))
    monkeypatch.setattr(selftest, "run", lambda _path: 1)
    assert main(["--self-test", str(tmp_path / "s.json")]) == 1


SELF_TEST_KEYS = {
    "app",
    "engine",
    "engine_elevated",
    "telemetry",
    "python",
    "tcl",
    "tk",
    "customtkinter",
    "pillow",
    "isolated",
    "installed",
    "prefix",
    "tcl_library",
    "errors",
}


def _no_window(report: dict[str, Any], _errors: list[str]) -> None:
    report.update(tcl="8.6.15", tk="8.6.15")


def _no_monitor(report: dict[str, Any], _errors: list[str]) -> None:
    report["telemetry"] = "optimizer_telemetry 0.0.0"


def test_self_test_report_lists_every_part(tmp_path: Path, monkeypatch: pytest.MonkeyPatch) -> None:
    # The hidden Tk window and the telemetry sampler are left to the release run.
    monkeypatch.setattr(selftest, "_tk", _no_window)
    monkeypatch.setattr(selftest, "_telemetry", _no_monitor)
    path = tmp_path / "s.json"
    code = selftest.run(path)
    report = json.loads(path.read_text(encoding="utf-8"))
    assert set(report) == SELF_TEST_KEYS
    assert report["app"] == __version__
    assert report["python"].startswith("3.12")
    assert report["customtkinter"] and report["pillow"]
    assert report["isolated"] is False, "a test run is not the isolated launcher"
    assert report["installed"] is False
    assert (report["engine"] is None) == any(e.startswith("engine:") for e in report["errors"])
    assert code == (0 if not report["errors"] else 1)


def test_a_failing_part_fails_the_self_test(tmp_path: Path, monkeypatch: pytest.MonkeyPatch) -> None:
    def broken_window(_report: dict[str, Any], errors: list[str]) -> None:
        errors.append("tk: no display")

    monkeypatch.setattr(selftest, "_tk", broken_window)
    monkeypatch.setattr(selftest, "_telemetry", _no_monitor)
    path = tmp_path / "s.json"
    assert selftest.run(path) == 1
    assert "tk: no display" in json.loads(path.read_text(encoding="utf-8"))["errors"]


def test_an_unwritable_report_fails_the_self_test(tmp_path: Path, monkeypatch: pytest.MonkeyPatch) -> None:
    monkeypatch.setattr(selftest, "_tk", _no_window)
    monkeypatch.setattr(selftest, "_telemetry", _no_monitor)
    assert selftest.run(tmp_path) == 1, "a folder cannot be written as the report"


def _no_engine(report: dict[str, Any], _errors: list[str]) -> None:
    report.update(engine="0.0.0", engine_elevated=False)


def _no_packages(report: dict[str, Any], _errors: list[str]) -> None:
    report.update(customtkinter="0.0.0", pillow="0.0.0")


def _passing_self_test(monkeypatch: pytest.MonkeyPatch, launcher: Path | None, tcl: str, tk: str) -> None:
    """Every part of the self-test passes; the process has the given launcher and Tcl settings."""
    for name, part in (
        ("_engine", _no_engine),
        ("_telemetry", _no_monitor),
        ("_tk", _no_window),
        ("_packages", _no_packages),
    ):
        monkeypatch.setattr(selftest, name, part)
    monkeypatch.setattr(system, "installed_launcher", lambda: launcher)
    monkeypatch.setattr(sys, "prefix", PREFIX)
    _launcher_environment(monkeypatch, tcl, tk)


def _launcher_environment(monkeypatch: pytest.MonkeyPatch, tcl: str, tk: str) -> None:
    """The Tcl variables as a launcher leaves them, with the given Tcl and Tk folders."""
    monkeypatch.setenv("TCL_LIBRARY", tcl)
    monkeypatch.setenv("TK_LIBRARY", tk)
    for name in ("TCLLIBPATH", "TIX_LIBRARY", *system.tcl_module_path_variables(os.environ)):
        monkeypatch.delenv(name, raising=False)


@pytest.mark.parametrize(
    ("tcl", "tk", "named"),
    [
        (r"C:\Users\Test\planted", rf"{PREFIX}\tcl\tk8.6", "TCL_LIBRARY"),
        (rf"{PREFIX.lower()}\tcl\tcl8.6", rf"{PREFIX}\tcl\tk8.6", "TCL_LIBRARY"),
        (rf"{PREFIX}\tcl\tcl8.6" + "\\", rf"{PREFIX}\tcl\tk8.6", "TCL_LIBRARY"),
        (rf"{PREFIX}\tcl\tcl8.6", rf"{PREFIX}\tcl\tk8.5", "TK_LIBRARY"),
    ],
)
def test_the_self_test_fails_where_the_installed_copy_would_refuse_to_start(
    tcl: str, tk: str, named: str, tmp_path: Path, monkeypatch: pytest.MonkeyPatch
) -> None:
    _passing_self_test(monkeypatch, Path(PREFIX) / "Cairn.exe", tcl, tk)
    path = tmp_path / "s.json"
    assert selftest.run(path) == 1
    report = json.loads(path.read_text(encoding="utf-8"))
    assert report["installed"] is True
    assert len(report["errors"]) == 1
    assert report["errors"][0].startswith(f"tcl: {named} ")
    assert system.installed_tcl_problem({"TCL_LIBRARY": tcl, "TK_LIBRARY": tk}, PREFIX) == named, (
        "the interactive start refuses the same settings"
    )


def test_the_self_test_passes_with_the_launcher_s_tcl_settings(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch
) -> None:
    _passing_self_test(
        monkeypatch, Path(PREFIX) / "Cairn.exe", rf"{PREFIX}\tcl\tcl8.6", rf"{PREFIX}\tcl\tk8.6"
    )
    path = tmp_path / "s.json"
    assert selftest.run(path) == 0
    report = json.loads(path.read_text(encoding="utf-8"))
    assert report["installed"] is True
    assert report["errors"] == []


def test_a_variable_the_launcher_removes_fails_the_installed_self_test(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch
) -> None:
    _passing_self_test(
        monkeypatch, Path(PREFIX) / "Cairn.exe", rf"{PREFIX}\tcl\tcl8.6", rf"{PREFIX}\tcl\tk8.6"
    )
    monkeypatch.setenv("TCLLIBPATH", r"C:\Users\Test\planted")
    path = tmp_path / "s.json"
    assert selftest.run(path) == 1
    errors = json.loads(path.read_text(encoding="utf-8"))["errors"]
    assert len(errors) == 1 and errors[0].startswith("tcl: TCLLIBPATH ")


@pytest.mark.parametrize("name", ["TCL8_6_TM_PATH", "TCL8.5_TM_PATH"])
def test_a_tcl_module_path_fails_the_installed_self_test(
    name: str, tmp_path: Path, monkeypatch: pytest.MonkeyPatch
) -> None:
    _passing_self_test(
        monkeypatch, Path(PREFIX) / "Cairn.exe", rf"{PREFIX}\tcl\tcl8.6", rf"{PREFIX}\tcl\tk8.6"
    )
    monkeypatch.setenv(name, r"C:\Users\Test\planted")
    path = tmp_path / "s.json"
    assert selftest.run(path) == 1
    errors = json.loads(path.read_text(encoding="utf-8"))["errors"]
    assert len(errors) == 1 and errors[0].startswith(f"tcl: {name} "), errors


def test_a_development_self_test_has_no_launcher_settings_to_check(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch
) -> None:
    _passing_self_test(monkeypatch, None, r"C:\Users\Test\tcl", r"C:\Users\Test\tk")
    path = tmp_path / "s.json"
    assert selftest.run(path) == 0
    report = json.loads(path.read_text(encoding="utf-8"))
    assert report["installed"] is False
    assert report["errors"] == []


def test_check_prints_versions_without_log_files(
    monkeypatch: pytest.MonkeyPatch, capsys: pytest.CaptureFixture[str]
) -> None:
    monkeypatch.setattr(logsetup, "configure", _refuse("logsetup.configure"))
    monkeypatch.setattr(instance.InstanceLock, "acquire", _refuse("InstanceLock.acquire"))
    monkeypatch.setattr(__main__, "_check_natives", lambda: 0)
    assert main(["--check"]) == 0
    assert capsys.readouterr().out.startswith(f"Cairn {__version__}  admin=")
    monkeypatch.setattr(__main__, "_check_natives", lambda: 1)
    assert main(["--check"]) == 1
    capsys.readouterr()


def test_installed_tcl_check() -> None:
    good = {"TCL_LIBRARY": rf"{PREFIX}\tcl\tcl8.6", "TK_LIBRARY": rf"{PREFIX}\tcl\tk8.6", "PATH": "x"}
    problem = system.installed_tcl_problem
    assert problem(good, PREFIX) is None
    assert problem({**good, "TCL_LIBRARY": r"C:\Users\Test\tcl"}, PREFIX) == "TCL_LIBRARY"
    assert problem({"TCL_LIBRARY": good["TCL_LIBRARY"]}, PREFIX) == "TK_LIBRARY"
    assert problem({**good, "TCLLIBPATH": r"C:\x"}, PREFIX) == "TCLLIBPATH"
    assert problem({**good, "TIX_LIBRARY": ""}, PREFIX) == "TIX_LIBRARY"
    assert problem({}, PREFIX) == "TCL_LIBRARY"
    assert problem({**good, "TCL8_6_TM_PATH": r"C:\Users\Test\planted"}, PREFIX) == "TCL8_6_TM_PATH"
    assert problem({**good, "tcl8.0_tm_path": ""}, PREFIX) == "tcl8.0_tm_path"
    assert problem({**good, "TCL8_TM_PATH": "x", "TCL_TM_PATH": "x"}, PREFIX) is None


def test_a_development_start_opens_the_window(recorder: Recorder) -> None:
    assert main([]) == 0
    assert recorder.scrubbed == 1, "a development run drops inherited Tcl settings"
    assert recorder.module_paths_scrubbed == 1, "and the Tcl module paths"
    assert recorder.configured == ["WARNING"]
    assert recorder.app_ids == [DEV_APP_ID]
    assert recorder.acquired == [None]
    assert recorder.windows == [{"instance": recorder.lock, "can_elevate": True, "start_note": ""}]
    assert recorder.lock is not None and (recorder.lock.entered, recorder.lock.exited) == (1, 1)
    assert recorder.messages == []


def test_the_launcher_arguments_reach_the_window(recorder: Recorder, monkeypatch: pytest.MonkeyPatch) -> None:
    monkeypatch.setenv("OPTIMIZER_UI_LOG", "debug")
    assert main(["--after", "4321", "--no-elevate", "--start-note", "elevation-failed:5"]) == 0
    assert recorder.acquired == [4321]
    assert recorder.configured == ["DEBUG"]
    assert recorder.windows == [
        {"instance": recorder.lock, "can_elevate": False, "start_note": "elevation-failed:5"}
    ]


def test_a_second_start_activates_the_open_window(recorder: Recorder) -> None:
    recorder.lock = None
    recorder.signal = True
    assert main([]) == 0
    assert recorder.windows == []
    assert recorder.messages == []


def test_a_start_while_the_window_closes_waits_for_it(recorder: Recorder) -> None:
    recorder.lock = None
    recorder.waited = FakeLock()
    assert main([]) == 0
    assert recorder.windows == [{"instance": recorder.waited, "can_elevate": True, "start_note": ""}]
    assert recorder.messages == []


def test_a_window_that_never_lets_go_is_reported(recorder: Recorder) -> None:
    recorder.lock = None
    assert main([]) == 0
    assert recorder.windows == []
    assert recorder.messages == [(APP_NAME, ALREADY_OPEN_TEXT, "info")]
    assert ALREADY_OPEN_TEXT == "Cairn is already open."


def test_the_lock_is_held_until_the_closed_window_s_engine_call_ends(
    recorder: Recorder, monkeypatch: pytest.MonkeyPatch
) -> None:
    # Closing the window cancels the queued engine calls but not the one that runs; a second
    # start must wait until it has ended.
    events: list[str] = []
    recorder.lock = FakeLock(events)
    recorder.engine = EngineRecorder(events)

    class Timer:
        def __enter__(self) -> Timer:
            events.append("timer on")
            return self

        def __exit__(self, *exc: object) -> None:
            events.append("timer off")

    monkeypatch.setattr(system, "TimerResolution", Timer)
    assert main([]) == 0
    assert events == ["timer on", "timer off", "engine shutdown wait=True", "lock released"]
    assert recorder.messages == []


def test_a_failed_wait_for_the_engine_still_releases_the_lock(recorder: Recorder) -> None:
    events: list[str] = []
    recorder.lock = FakeLock(events)
    recorder.engine = EngineRecorder(events, error=RuntimeError("worker gone"))
    assert main([]) == 0
    assert events == ["engine shutdown wait=True", "lock released"]
    assert recorder.messages == []


def test_a_window_error_waits_for_the_engine_before_the_lock_is_released(recorder: Recorder) -> None:
    events: list[str] = []
    recorder.lock = FakeLock(events)
    recorder.engine = EngineRecorder(events)
    recorder.window_error = RuntimeError("boom")
    assert main([]) == EXIT_REPORTED
    assert events == ["engine shutdown wait=True", "lock released"]
    assert len(recorder.messages) == 1


def test_an_error_in_the_window_is_shown_and_logged(recorder: Recorder) -> None:
    recorder.window_error = RuntimeError("boom")
    assert main([]) == EXIT_REPORTED
    assert len(recorder.messages) == 1
    title, text, icon = recorder.messages[0]
    assert (title, icon) == (APP_NAME, "error")
    assert text.startswith("Cairn stopped because of an error:\nboom")
    assert str(recorder.log_path) in text
    assert recorder.lock is not None and recorder.lock.exited == 1, "the lock is released"


def test_the_installed_copy_refuses_a_changed_tcl_setting(
    recorder: Recorder, monkeypatch: pytest.MonkeyPatch
) -> None:
    recorder.launcher = Path(PREFIX) / "Cairn.exe"
    monkeypatch.setattr(sys, "prefix", PREFIX)
    monkeypatch.setenv("TCL_LIBRARY", r"C:\Users\Test\planted")
    monkeypatch.setenv("TK_LIBRARY", rf"{PREFIX}\tcl\tk8.6")
    assert main([]) == EXIT_REPORTED
    assert recorder.messages == [(APP_NAME, UNSAFE_TCL_TEXT, "error")]
    assert recorder.acquired == [], "nothing else starts"
    assert recorder.windows == []
    assert recorder.scrubbed == 0, "the installed copy's variables come from its launcher"
    assert recorder.module_paths_scrubbed == 0


def test_the_installed_copy_refuses_a_tcl_module_path(
    recorder: Recorder, monkeypatch: pytest.MonkeyPatch
) -> None:
    # A module-path variable the launcher left in place: Tk's first `package require` would
    # search that folder ahead of the install's own modules.
    recorder.launcher = Path(PREFIX) / "Cairn.exe"
    monkeypatch.setattr(sys, "prefix", PREFIX)
    _launcher_environment(monkeypatch, rf"{PREFIX}\tcl\tcl8.6", rf"{PREFIX}\tcl\tk8.6")
    monkeypatch.setenv("TCL8_6_TM_PATH", r"C:\Users\Test\planted")
    assert main([]) == EXIT_REPORTED
    assert recorder.messages == [(APP_NAME, UNSAFE_TCL_TEXT, "error")]
    assert recorder.acquired == [], "nothing else starts"
    assert recorder.windows == []
    assert (recorder.scrubbed, recorder.module_paths_scrubbed) == (0, 0)


def test_the_installed_copy_starts_with_its_own_tcl(
    recorder: Recorder, monkeypatch: pytest.MonkeyPatch
) -> None:
    recorder.launcher = Path(PREFIX) / "Cairn.exe"
    monkeypatch.setattr(sys, "prefix", PREFIX)
    _launcher_environment(monkeypatch, rf"{PREFIX}\tcl\tcl8.6", rf"{PREFIX}\tcl\tk8.6")
    monkeypatch.setattr(system, "app_id", lambda: "Cairn.App")
    assert main([]) == 0
    assert recorder.app_ids == ["Cairn.App"]
    assert len(recorder.windows) == 1
    assert (recorder.scrubbed, recorder.module_paths_scrubbed) == (0, 0)
