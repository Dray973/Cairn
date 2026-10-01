"""Cairn's data folder (`optimizer.paths`) and log files (`optimizer.logsetup`), in temporary
folders only. The windowed branch, which takes over this process's standard error, is left to
the release self-test."""

from __future__ import annotations

import logging
import os
import sys
import threading
from collections.abc import Iterator
from logging.handlers import RotatingFileHandler
from pathlib import Path

import pytest

from optimizer import logsetup, paths
from optimizer.bridge.engine import EngineUnavailable, load_engine_module


@pytest.fixture
def restore_logging() -> Iterator[None]:
    """Puts back the root logger, the start logger and the exception hooks that `configure`
    changes, and closes the handlers it added."""
    root = logging.getLogger()
    start = logging.getLogger(logsetup.START_LOGGER)
    handlers, level, start_level = list(root.handlers), root.level, start.level
    hooks = sys.excepthook, threading.excepthook
    try:
        yield
    finally:
        for handler in list(root.handlers):
            if handler not in handlers:
                root.removeHandler(handler)
                handler.close()
        root.setLevel(level)
        start.setLevel(start_level)
        sys.excepthook, threading.excepthook = hooks


def added_handlers(before: list[logging.Handler]) -> list[logging.Handler]:
    return [h for h in logging.getLogger().handlers if h not in before]


def test_data_dir_follows_the_engine() -> None:
    assert paths.data_dir({"OPTIMIZER_DATA_DIR": r"D:\Data\Cairn", "LOCALAPPDATA": r"C:\X"}) == Path(
        r"D:\Data\Cairn"
    )
    assert paths.data_dir({"LOCALAPPDATA": r"C:\Users\Test\AppData\Local"}) == Path(
        r"C:\Users\Test\AppData\Local\PCOptimizer"
    )
    assert paths.data_dir({"LOCALAPPDATA": ""}) == Path(".") / "PCOptimizer"
    assert paths.data_dir({}) == Path(".") / "PCOptimizer"
    assert paths.log_dir({"LOCALAPPDATA": r"C:\Users\Test\AppData\Local"}) == Path(
        r"C:\Users\Test\AppData\Local\PCOptimizer\logs"
    )


def test_data_dir_reads_the_process_environment() -> None:
    assert paths.data_dir() == Path(os.environ["OPTIMIZER_DATA_DIR"])
    assert paths.log_dir() == paths.data_dir() / "logs"


def test_data_dir_matches_the_real_engine() -> None:
    try:
        module = load_engine_module()
    except EngineUnavailable as exc:
        pytest.skip(str(exc))
    if not callable(getattr(module, "journal_path", None)):
        pytest.skip("the deployed engine module has no journal_path")
    assert Path(module.journal_path()).parent == paths.data_dir()


def test_rotate_if_large(tmp_path: Path) -> None:
    log = tmp_path / "native.log"
    assert not logsetup.rotate_if_large(log, 10), "a missing file stays missing"
    log.write_bytes(b"x" * 10)
    assert not logsetup.rotate_if_large(log, 10)
    assert log.read_bytes() == b"x" * 10
    log.write_bytes(b"y" * 11)
    (tmp_path / "native.log.1").write_bytes(b"old")
    assert logsetup.rotate_if_large(log, 10)
    assert not log.exists()
    assert (tmp_path / "native.log.1").read_bytes() == b"y" * 11, "the older copy is replaced"


def test_configure_writes_cairn_log(tmp_path: Path, restore_logging: None) -> None:
    before = list(logging.getLogger().handlers)
    path = logsetup.configure("info", directory=tmp_path / "logs", windowed=False)
    assert path == tmp_path / "logs" / "cairn.log"
    files = [h for h in added_handlers(before) if isinstance(h, RotatingFileHandler)]
    assert len(files) == 1
    assert files[0].maxBytes == logsetup.MAX_BYTES == 1_000_000
    assert files[0].backupCount == logsetup.BACKUP_COUNT == 2
    assert Path(files[0].baseFilename) == path
    assert logging.getLogger().level == logging.INFO
    logging.getLogger("optimizer.test").warning("a test record")
    for handler in added_handlers(before):
        handler.flush()
    text = path.read_text(encoding="utf-8")
    assert "WARNING optimizer.test: a test record" in text
    assert "Cairn " in text and " starting: python " in text and "development" in text
    user = os.environ.get("USERNAME", "")
    start = next(line for line in text.splitlines() if " starting: " in line)
    assert not user or user not in start, "the start line names no user"
    assert sys.excepthook is logsetup._log_uncaught
    assert threading.excepthook is logsetup._log_uncaught_in_thread
    assert not (tmp_path / "logs" / "native.log").exists(), "only a windowed start redirects"


def test_the_start_line_is_written_at_any_level(tmp_path: Path, restore_logging: None) -> None:
    before = list(logging.getLogger().handlers)
    path = logsetup.configure("ERROR", directory=tmp_path, windowed=False)
    assert path is not None
    logging.getLogger("optimizer.test").warning("below the level")
    for handler in added_handlers(before):
        handler.flush()
    text = path.read_text(encoding="utf-8")
    assert " starting: " in text
    assert "below the level" not in text


def test_uncaught_exceptions_are_logged(tmp_path: Path, restore_logging: None) -> None:
    before = list(logging.getLogger().handlers)
    path = logsetup.configure("WARNING", directory=tmp_path, windowed=False)
    assert path is not None
    try:
        raise ValueError("boom in main")
    except ValueError as exc:
        sys.excepthook(type(exc), exc, exc.__traceback__)
    worker = threading.Thread(target=lambda: (_ for _ in ()).throw(RuntimeError("boom in thread")))
    worker.start()
    worker.join()
    for handler in added_handlers(before):
        handler.flush()
    text = path.read_text(encoding="utf-8")
    assert "CRITICAL optimizer: uncaught exception" in text and "ValueError: boom in main" in text
    assert "ERROR optimizer: uncaught exception in thread" in text and "RuntimeError: boom in thread" in text


def test_a_folder_that_cannot_be_created_falls_back_to_stderr(tmp_path: Path, restore_logging: None) -> None:
    blocker = tmp_path / "file"
    blocker.write_text("not a folder", encoding="utf-8")
    before = list(logging.getLogger().handlers)
    assert logsetup.configure("INFO", directory=blocker / "logs", windowed=False) is None
    added = added_handlers(before)
    assert added and not any(isinstance(h, RotatingFileHandler) for h in added)


def test_configure_stderr_adds_no_file(restore_logging: None, monkeypatch: pytest.MonkeyPatch) -> None:
    before = list(logging.getLogger().handlers)
    logsetup.configure_stderr("debug")
    added = added_handlers(before)
    assert len(added) == 1 and isinstance(added[0], logging.StreamHandler)
    assert not isinstance(added[0], logging.FileHandler)
    assert logging.getLogger().level == logging.DEBUG

    monkeypatch.setattr(sys, "stderr", None)
    logsetup.configure_stderr("nonsense")
    added = added_handlers(before)
    assert len(added) == 2 and isinstance(added[1], logging.NullHandler)
    assert logging.getLogger().level == logging.WARNING, "an unknown level name means WARNING"
