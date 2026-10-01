"""The test-wide guards of conftest: the engine's refusal switches, the throwaway data folder
and the stubs that replace the process helpers."""

from __future__ import annotations

import os
from pathlib import Path

import pytest

from optimizer import system
from optimizer.bridge.engine import EngineUnavailable, load_engine_module

from .conftest import PROCESS_HELPERS, TEST_DATA_DIR

GUARDS = (
    "OPTIMIZER_FORBID_RESTORE_POINT",
    "OPTIMIZER_FORBID_DRIVE_TESTS",
    "OPTIMIZER_FORBID_UPDATE_SEARCH",
    "OPTIMIZER_FORBID_APP_INSTALLS",
)


def test_engine_guards_are_set() -> None:
    for name in GUARDS:
        assert os.environ.get(name) == "1", name


def test_data_folder_is_a_throwaway_folder() -> None:
    assert Path(os.environ["OPTIMIZER_DATA_DIR"]) == TEST_DATA_DIR
    assert TEST_DATA_DIR.name.startswith("cairn-pytest-")
    local = os.environ.get("LOCALAPPDATA")
    if local:
        assert not TEST_DATA_DIR.is_relative_to(Path(local) / "PCOptimizer")


def test_process_helpers_are_replaced() -> None:
    for name in PROCESS_HELPERS:
        helper = getattr(system, name)
        assert helper.__module__ != system.__name__, f"{name} is the real helper"
        assert "_failing_stub" in helper.__qualname__, name


def test_real_module_uses_the_test_data_dir() -> None:
    try:
        module = load_engine_module()
    except EngineUnavailable as exc:
        pytest.skip(str(exc))
    if not callable(getattr(module, "journal_path", None)):
        pytest.skip("the deployed engine module has no journal_path")
    assert Path(module.journal_path()).is_relative_to(TEST_DATA_DIR)
