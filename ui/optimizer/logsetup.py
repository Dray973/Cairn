"""Cairn's logs in `%LOCALAPPDATA%\\PCOptimizer\\logs`: `cairn.log` holds the dashboard's Python
logging (rotated by size), and `native.log` the engine's tracing output, Python's fault
handler and anything written to stdout or stderr when the window has no console (pythonw.exe
and Cairn.exe).

Only the interactive start configures the files; `--check` and `--self-test` log to stderr
alone (`configure_stderr`), so a release gate never creates or rotates the log folder. The
logs contain no user name of their own, but paths inside messages may.
"""

from __future__ import annotations

import faulthandler
import io
import logging
import sys
import threading
from logging.handlers import RotatingFileHandler
from pathlib import Path
from types import TracebackType

from . import __version__, paths, system

LOG_NAME = "cairn.log"
NATIVE_LOG_NAME = "native.log"
MAX_BYTES = 1_000_000
BACKUP_COUNT = 2
NATIVE_MAX_BYTES = 2_000_000
FORMAT = "%(asctime)s %(levelname)s %(name)s: %(message)s"
# Logger of the start line, which is written whatever the configured level.
START_LOGGER = "optimizer.start"
_STD_ERROR_HANDLE = -12

log = logging.getLogger("optimizer")


def _level(level: str) -> int:
    """The numeric value of a level name such as "WARNING"; WARNING for an unknown name."""
    value = logging.getLevelName(level.upper())
    return value if isinstance(value, int) else logging.WARNING


def configure_stderr(level: str) -> None:
    """Logs to stderr only, or nowhere when the process has no stderr (a windowed start)."""
    root = logging.getLogger()
    root.setLevel(_level(level))
    if sys.stderr is not None:
        handler: logging.Handler = logging.StreamHandler(sys.stderr)
        handler.setFormatter(logging.Formatter(FORMAT))
    else:
        handler = logging.NullHandler()
    root.addHandler(handler)


def rotate_if_large(path: Path, limit: int) -> bool:
    """Renames `path` to `<name>.1`, replacing an older one, when it is larger than `limit`
    bytes; True when it did."""
    try:
        if path.stat().st_size <= limit:
            return False
    except FileNotFoundError:
        return False
    path.replace(path.with_name(path.name + ".1"))
    return True


def _set_native_stderr(fileno: int) -> None:
    """Points this process's standard error handle at file descriptor `fileno`, so the
    engine's tracing output, which Rust writes to that handle, lands in the same file."""
    import ctypes
    import msvcrt
    from ctypes import wintypes

    kernel32 = ctypes.WinDLL("kernel32", use_last_error=True)
    kernel32.SetStdHandle.argtypes = [wintypes.DWORD, wintypes.HANDLE]
    kernel32.SetStdHandle.restype = wintypes.BOOL
    kernel32.SetStdHandle(wintypes.DWORD(_STD_ERROR_HANDLE & 0xFFFFFFFF), msvcrt.get_osfhandle(fileno))


def _redirect_native_output(directory: Path) -> None:
    """Sends stdout, stderr, the fault handler and the engine's standard error to
    `native.log`, rotated at start when it grew past `NATIVE_MAX_BYTES`."""
    native = directory / NATIVE_LOG_NAME
    try:
        rotate_if_large(native, NATIVE_MAX_BYTES)
    except OSError:
        log.warning("%s could not be rotated", native, exc_info=True)
    # Stays open for the life of the process: the fault handler and the engine write to it.
    stream = open(native, "ab")
    faulthandler.enable(stream)
    text = io.TextIOWrapper(stream, "utf-8", errors="replace", line_buffering=True)
    sys.stdout = sys.stderr = text
    _set_native_stderr(stream.fileno())


def _log_uncaught(kind: type[BaseException], value: BaseException, tb: TracebackType | None) -> None:
    log.critical("uncaught exception", exc_info=(kind, value, tb))


def _log_uncaught_in_thread(args: threading.ExceptHookArgs) -> None:
    if args.exc_type is SystemExit:
        return
    name = args.thread.name if args.thread is not None else "?"
    log.error(
        "uncaught exception in thread %s", name, exc_info=(args.exc_type, args.exc_value, args.exc_traceback)
    )


def configure(level: str, *, directory: Path | None = None, windowed: bool | None = None) -> Path | None:
    """Sets up logging for the interactive start and returns the path of `cairn.log`.

    `directory` defaults to `paths.log_dir()`; `windowed` (default: the process has no
    stderr) also redirects the native output to `native.log`, which must happen before the
    engine module loads. When the folder cannot be created, logging goes to stderr only and
    None is returned. Uncaught exceptions of the main thread and of other threads are logged.
    """
    directory = paths.log_dir() if directory is None else directory
    windowed = sys.stderr is None if windowed is None else windowed
    try:
        directory.mkdir(parents=True, exist_ok=True)
    except OSError:
        configure_stderr(level)
        log.warning("the log folder %s could not be created", directory, exc_info=True)
        return None

    root = logging.getLogger()
    root.setLevel(_level(level))
    formatter = logging.Formatter(FORMAT)
    path = directory / LOG_NAME
    file_handler = RotatingFileHandler(
        path, maxBytes=MAX_BYTES, backupCount=BACKUP_COUNT, encoding="utf-8", delay=True
    )
    file_handler.setFormatter(formatter)
    root.addHandler(file_handler)
    if sys.stderr is not None:
        console = logging.StreamHandler(sys.stderr)
        console.setFormatter(formatter)
        root.addHandler(console)
    if windowed:
        try:
            _redirect_native_output(directory)
        except OSError:
            log.warning("native output stays unredirected", exc_info=True)

    sys.excepthook = _log_uncaught
    threading.excepthook = _log_uncaught_in_thread

    start = logging.getLogger(START_LOGGER)
    start.setLevel(logging.INFO)
    start.info(
        "Cairn %s starting: python %s, %s, elevated=%s",
        __version__,
        sys.version.split()[0],
        "installed" if system.installed_launcher() is not None else "development",
        system.is_admin(),
    )
    return path
