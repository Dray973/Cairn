"""`--self-test PATH`: checks that the runtime of this copy works and writes a JSON report.

Read-only: it loads the engine and the telemetry DLL, creates and destroys a hidden Tk
window and loads a Tcl package; it takes no instance lock, scans nothing, opens no journal
and writes only the report. Release builds run it on the staged Cairn.exe, where it also
checks the launcher's Tcl settings the way an interactive start does (`installed` in the
report says whether it ran as that launcher).
"""

from __future__ import annotations

import json
import logging
import os
import platform
import sys
import time
from pathlib import Path
from typing import Any

from . import __version__, system

log = logging.getLogger(__name__)


def _engine(report: dict[str, Any], errors: list[str]) -> None:
    try:
        import optimizer_engine  # type: ignore[import-not-found]

        report["engine"] = str(optimizer_engine.version())
        report["engine_elevated"] = bool(optimizer_engine.is_elevated())
    except Exception as exc:  # noqa: BLE001 - every failure goes into the report
        errors.append(f"engine: {exc}")


def _telemetry(report: dict[str, Any], errors: list[str]) -> None:
    try:
        from .bridge.telemetry import Telemetry

        with Telemetry() as tel:
            tel.start()
            time.sleep(0.3)
            tel.snapshot()
            report["telemetry"] = tel.version
    except Exception as exc:  # noqa: BLE001 - every failure goes into the report
        errors.append(f"telemetry: {exc}")


def _tk(report: dict[str, Any], errors: list[str]) -> None:
    try:
        import tkinter

        interpreter = tkinter.Tcl()
        report["tcl"] = str(interpreter.call("info", "patchlevel"))
        interpreter.call("package", "require", "msgcat")
        root = tkinter.Tk()
        try:
            root.withdraw()
            report["tk"] = str(root.call("info", "patchlevel"))
        finally:
            root.destroy()
    except Exception as exc:  # noqa: BLE001 - every failure goes into the report
        errors.append(f"tk: {exc}")


def _packages(report: dict[str, Any], errors: list[str]) -> None:
    try:
        import customtkinter

        report["customtkinter"] = str(customtkinter.__version__)
    except Exception as exc:  # noqa: BLE001 - every failure goes into the report
        errors.append(f"customtkinter: {exc}")
    try:
        import PIL

        report["pillow"] = str(PIL.__version__)
    except Exception as exc:  # noqa: BLE001 - every failure goes into the report
        errors.append(f"pillow: {exc}")


def _launcher_settings(report: dict[str, Any], errors: list[str]) -> None:
    """Records whether this is the installed copy. An interactive start of that copy refuses to
    run unless the Tcl settings its launcher made match `sys.prefix`; a self-test is
    dispatched before that refusal, so the same comparison is an error here."""
    report["installed"] = system.installed_launcher() is not None
    if not report["installed"]:
        return
    problem = system.installed_tcl_problem(os.environ, sys.prefix)
    if problem is not None:
        errors.append(
            f"tcl: {problem} is {os.environ.get(problem)!r}, which is not the launcher's setting"
            f" for {sys.prefix}"
        )


def collect() -> dict[str, Any]:
    """Runs every check; failures are listed in `errors`."""
    errors: list[str] = []
    report: dict[str, Any] = {
        "app": __version__,
        "engine": None,
        "engine_elevated": None,
        "telemetry": None,
        "python": platform.python_version(),
        "tcl": None,
        "tk": None,
        "customtkinter": None,
        "pillow": None,
        "isolated": bool(sys.flags.isolated),
        "installed": False,
        "prefix": sys.prefix,
        "tcl_library": os.environ.get("TCL_LIBRARY"),
        "errors": errors,
    }
    _launcher_settings(report, errors)
    _engine(report, errors)
    _telemetry(report, errors)
    _tk(report, errors)
    _packages(report, errors)
    return report


def run(path: Path) -> int:
    """Writes the report to `path`; 0 when every check passed, 1 otherwise or when the report
    cannot be written."""
    report = collect()
    try:
        path.write_text(json.dumps(report, indent=2), encoding="utf-8")
    except OSError:
        log.exception("the self-test report could not be written to %s", path)
        return 1
    return 0 if not report["errors"] else 1
