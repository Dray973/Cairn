"""The test-wide guards of conftest: the engine's refusal switches, the throwaway data folder,
the stubs that replace the process helpers and the engine module's desktop functions, and the
`CAIRN_REQUIRE_NATIVES` check."""

from __future__ import annotations

import inspect
import os
import sys
import types
from pathlib import Path

import pytest

from optimizer import system
from optimizer.bridge.engine import EngineUnavailable, load_engine_module

from .conftest import (
    ENGINE_DESKTOP_FUNCTIONS,
    PROCESS_HELPERS,
    REQUIRE_NATIVES,
    TEST_DATA_DIR,
    missing_natives,
    natives_problem,
    stub_engine_desktop_functions,
)

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


def test_engine_desktop_functions_get_failing_stubs_with_their_signatures() -> None:
    def open_url(url: str) -> None:
        raise AssertionError("the real function ran")

    engine = types.ModuleType("optimizer_engine_stand_in")
    engine.open_url = open_url  # type: ignore[attr-defined]
    reached: list[str] = []
    assert set(ENGINE_DESKTOP_FUNCTIONS) == {"open_url"}
    with pytest.MonkeyPatch.context() as patch:
        stub_engine_desktop_functions(patch, None, reached)
        stub_engine_desktop_functions(patch, engine, reached)
        stub = engine.open_url  # type: ignore[attr-defined]
        assert stub is not open_url
        assert inspect.signature(stub) == inspect.signature(open_url)
        with pytest.raises(pytest.fail.Exception, match="optimizer_engine.open_url"):
            stub("https://example.com/")
    assert reached == ["optimizer_engine.open_url"]
    assert engine.open_url is open_url  # type: ignore[attr-defined]

    # A module loaded before this test has its stub in place now.
    loaded = sys.modules.get("optimizer_engine")
    for name in ENGINE_DESKTOP_FUNCTIONS:
        if getattr(loaded, name, None) is not None:
            assert hasattr(getattr(loaded, name), "__wrapped__"), f"{name} is the real function"


def test_natives_are_checked_only_when_required() -> None:
    def unexpected() -> list[str]:
        raise AssertionError("checked without the variable")

    # Unset, or anything but "1", changes nothing: the natives are not even loaded.
    assert natives_problem({}, unexpected) is None
    assert natives_problem({REQUIRE_NATIVES: "0"}, unexpected) is None
    assert natives_problem({REQUIRE_NATIVES: ""}, unexpected) is None
    assert natives_problem({REQUIRE_NATIVES: "1"}, list) is None
    problem = natives_problem({REQUIRE_NATIVES: "1"}, lambda: ["engine missing", "DLL missing"])
    assert problem == "CAIRN_REQUIRE_NATIVES=1, but the natives cannot be used: engine missing; DLL missing"


def test_missing_natives_names_what_cannot_be_loaded() -> None:
    problems = missing_natives()
    try:
        load_engine_module()
    except EngineUnavailable:
        assert any("optimizer_engine" in p for p in problems)
    else:
        assert not any("optimizer_engine" in p for p in problems)
    assert all(isinstance(p, str) and p for p in problems)


def test_real_module_uses_the_test_data_dir() -> None:
    try:
        module = load_engine_module()
    except EngineUnavailable as exc:
        pytest.skip(str(exc))
    if not callable(getattr(module, "journal_path", None)):
        pytest.skip("the deployed engine module has no journal_path")
    assert Path(module.journal_path()).is_relative_to(TEST_DATA_DIR)
