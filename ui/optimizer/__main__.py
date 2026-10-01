"""`python -m optimizer` (and the installed Cairn.exe): opens the Cairn window.

`--check` loads both native modules and prints their versions; `--self-test PATH` writes the
release self-test report. Both are read-only, log to stderr only and never take the
single-instance lock. An interactive start configures the log files, takes the lock (a
second start brings the open window to the front instead) and runs the window; the lock is
held until an engine call the closed window left running has ended.

Exit codes: 0; 1 when `--check` or the self-test failed; 2 for a bad command line;
`EXIT_REPORTED` after an error this module has already shown to the user.
"""

from __future__ import annotations

import argparse
import logging
import os
import re
import sys
import time
from pathlib import Path

from . import APP_NAME, NATIVE_DIR, __version__, instance, logsetup, selftest, system

log = logging.getLogger("optimizer")

# The launcher shows its own "closed unexpectedly" message for every other non-zero code.
EXIT_REPORTED = 3
START_NOTE = re.compile(r"elevation-declined|elevation-failed:(?:\d+|0x[0-9A-Fa-f]+)")
ALREADY_OPEN_TEXT = "Cairn is already open."
UNSAFE_TCL_TEXT = "Cairn can't start safely because its Tcl library setting was changed. Reinstall Cairn."
# How long a start waits for a window that is closing to release the single-instance lock.
CLOSING_WAIT_SECONDS = 30.0


def _start_note(value: str) -> str:
    if not START_NOTE.fullmatch(value):
        raise argparse.ArgumentTypeError("expected elevation-declined or elevation-failed:<code>")
    return value


def build_parser() -> argparse.ArgumentParser:
    parser = argparse.ArgumentParser(prog="optimizer", description=APP_NAME, allow_abbrev=False)
    parser.add_argument(
        "--check", action="store_true", help="load the native modules, print versions and exit"
    )
    parser.add_argument(
        "--self-test", metavar="PATH", help="check the runtime, write a JSON report to PATH and exit"
    )
    parser.add_argument(
        "--after", type=int, metavar="PID", help="wait for process PID to end first (a relaunch)"
    )
    parser.add_argument(
        "--no-elevate",
        action="store_true",
        help="this copy may not ask for administrator rights (it is not installed)",
    )
    parser.add_argument(
        "--start-note",
        type=_start_note,
        metavar="NOTE",
        help="why the window runs without administrator rights (set by the launcher)",
    )
    return parser


def _check_natives() -> int:
    """Loads both native bridges, prints their versions and returns a process exit code."""
    from .bridge.telemetry import Telemetry, TelemetryError

    ok = True
    try:
        with Telemetry() as tel:
            tel.start()
            time.sleep(0.3)
            s = tel.snapshot()
            gib = 1024**3
            print(
                f"[telemetry] {tel.version} cores={s.cpu.core_count}"
                f" cpu={s.cpu.total_utilization:.1f}%"
                f" mem={s.memory.physical_used_bytes / gib:.2f}/{s.memory.physical_total_bytes / gib:.2f} GiB"
                f" processes={s.processes.process_count}"
            )
    except (TelemetryError, OSError) as exc:
        ok = False
        print(f"[telemetry] failed: {exc}")

    try:
        import optimizer_engine  # type: ignore[import-not-found]

        print(f"[engine]    v{optimizer_engine.version()} elevated={optimizer_engine.is_elevated()}")
    except ImportError as exc:
        ok = False
        print(f"[engine]    missing optimizer_engine.pyd in {NATIVE_DIR}: {exc}")
    return 0 if ok else 1


def _log_level() -> str:
    return os.environ.get("OPTIMIZER_UI_LOG", "WARNING").upper()


def _diagnostic(args: argparse.Namespace) -> int:
    logsetup.configure_stderr(_log_level())
    if args.check:
        if sys.stdout is None:
            system.attach_parent_console()
        print(f"{APP_NAME} {__version__}  admin={system.is_admin()}  python={sys.version.split()[0]}")
        return _check_natives()
    return selftest.run(Path(args.self_test))


def main(argv: list[str] | None = None) -> int:
    args = build_parser().parse_args(argv)
    # Dispatched before any log file is configured: a release gate must not create or rotate
    # the user's log folder.
    if args.check or args.self_test:
        return _diagnostic(args)

    if system.installed_launcher() is None:
        system.scrub_tcl_environment()
        system.scrub_tcl_module_paths()
    log_path = logsetup.configure(_log_level())
    if system.installed_launcher() is not None:
        problem = system.installed_tcl_problem(os.environ, sys.prefix)
        if problem is not None:
            log.critical("refusing to start: %s was changed", problem)
            system.message_box(APP_NAME, UNSAFE_TCL_TEXT)
            return EXIT_REPORTED
    system.set_app_id(system.app_id())

    lock = instance.InstanceLock.acquire(wait_for_pid=args.after)
    if lock is None:
        if instance.signal_running_instance():
            return 0
        # The open window stopped accepting activation because it is closing.
        lock = instance.acquire_when_free(timeout=CLOSING_WAIT_SECONDS)
        if lock is None:
            system.message_box(APP_NAME, ALREADY_OPEN_TEXT, icon="info")
            return 0
    try:
        from .app import App

        with lock:
            window = None
            try:
                with system.TimerResolution():
                    window = App(
                        instance=lock, can_elevate=not args.no_elevate, start_note=args.start_note or ""
                    )
                    window.mainloop()
            finally:
                _finish_engine_call(window)
    except Exception as exc:  # noqa: BLE001 - last resort: report and exit
        log.critical("Cairn stopped because of an error", exc_info=True)
        where = f"\n\nDetails are in {log_path}." if log_path is not None else ""
        system.message_box(APP_NAME, f"Cairn stopped because of an error:\n{exc}{where}")
        return EXIT_REPORTED
    return 0


def _finish_engine_call(window: object | None) -> None:
    """Waits for the engine call the window left running when it closed (closing cancels the
    queued ones). The single-instance lock is released only afterwards, so a second start never
    works on the journal and the settings while this process still changes them."""
    engine = getattr(window, "engine", None)
    if engine is None:
        return
    try:
        engine.shutdown(wait=True)
    except Exception:  # noqa: BLE001 - the lock is released either way
        log.warning("waiting for the engine's last call failed", exc_info=True)


if __name__ == "__main__":
    raise SystemExit(main())
