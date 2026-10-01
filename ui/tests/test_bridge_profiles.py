"""EngineBridge profile calls: previews as dry runs by keyword, applies through the
restore-point policy of `_mutation`, the synchronous starter table, capability checks, and
parity between the FakeEngine and the real engine module.

Of the real module only pure functions are called: the starter table and `profile_check`,
which validate text and touch nothing on this PC.
"""

from __future__ import annotations

import json
import time
from typing import Any

import pytest

from optimizer.bridge.engine import EngineBridge, EngineUnavailable, load_engine_module

from .bridge_support import assert_signatures_match, wait
from .fake_engine import FakeEngine
from .fake_profiles import ELEVATION_ERROR, FAKE_STARTERS, starter_text

PROFILE_SIGNATURES = (
    "profile_starters",
    "profile_check",
    "profile_read",
    "profile_candidates",
    "profile_export",
    "profile_apply",
)
SUMMARY_KEYS = {"name", "description", "created", "created_with", "counts", "text"}
COUNT_KEYS = {"tweaks", "apps", "startup", "dns", "windows_update", "maintenance"}


def real_module() -> Any:
    try:
        module = load_engine_module()
    except EngineUnavailable as exc:
        pytest.skip(str(exc))
    if not callable(getattr(module, "profile_apply", None)):
        pytest.skip("the deployed engine module has no profile functions yet")
    return module


def test_plan_is_a_dry_run_on_the_worker_with_keywords() -> None:
    engine = FakeEngine(elevated=True)
    bridge = EngineBridge(module=engine)  # type: ignore[arg-type]
    try:
        text = starter_text("gaming")
        future = bridge.plan_profile(text)
        wait(bridge, [future])
        assert engine.calls_named("profile_apply") == [(text, None, "skip", True)]
        plan = future.result()
        assert plan["dry_run"] is True
        assert [r["key"] for r in plan["rows"]] == ["tweak:gaming.game_mode", "tweak:performance.sysmain"]
        assert engine.applied == set()
    finally:
        bridge.shutdown()


def test_apply_asks_for_a_restore_point_once_a_session_opened() -> None:
    engine = FakeEngine(elevated=True)
    bridge = EngineBridge(module=engine)  # type: ignore[arg-type]
    try:
        text = starter_text("gaming")
        first = bridge.apply_profile(text, ["tweak:gaming.game_mode"])
        wait(bridge, [first])
        assert engine.calls_named("profile_apply")[-1] == (text, ["tweak:gaming.game_mode"], "try", False)
        assert first.result()["session_id"] == 1
        assert not bridge.next_mutation_creates_restore_point
        second = bridge.apply_profile(text, ("tweak:performance.sysmain",))
        wait(bridge, [second])
        assert engine.calls_named("profile_apply")[-1] == (text, ["tweak:performance.sysmain"], "skip", False)
        assert engine.applied == {"gaming.game_mode", "performance.sysmain"}
    finally:
        bridge.shutdown()


def test_an_apply_that_changes_nothing_keeps_the_restore_point_request() -> None:
    engine = FakeEngine(elevated=True, preset=["gaming.game_mode"])
    bridge = EngineBridge(module=engine)  # type: ignore[arg-type]
    try:
        future = bridge.apply_profile(starter_text("gaming"), ["tweak:gaming.game_mode"])
        wait(bridge, [future])
        assert future.result()["session_id"] is None
        assert future.result()["already"] == 1
        assert bridge.next_mutation_creates_restore_point
    finally:
        bridge.shutdown()


def test_an_unelevated_apply_is_refused_before_planning() -> None:
    # The chosen row is already set, so only a refusal made before planning raises here.
    engine = FakeEngine(elevated=False, preset=["gaming.game_mode"])
    bridge = EngineBridge(module=engine)  # type: ignore[arg-type]
    try:
        future = bridge.apply_profile(starter_text("gaming"), ["tweak:gaming.game_mode"])
        wait(bridge, [future])
        assert isinstance(future.exception(), RuntimeError)
        assert str(future.exception()) == ELEVATION_ERROR
        assert bridge.next_mutation_creates_restore_point
        plan = bridge.plan_profile(starter_text("gaming"))
        wait(bridge, [plan])
        assert plan.result()["elevated"] is False
    finally:
        bridge.shutdown()


def test_failures_reach_the_future() -> None:
    engine = FakeEngine(elevated=True, profile_apply_error="journal is locked")
    bridge = EngineBridge(module=engine)  # type: ignore[arg-type]
    try:
        future = bridge.apply_profile(starter_text("gaming"), ["tweak:gaming.game_mode"])
        wait(bridge, [future])
        assert isinstance(future.exception(), RuntimeError)
        assert str(future.exception()) == "journal is locked"
        assert bridge.next_mutation_creates_restore_point
        invalid = bridge.plan_profile("[]")
        wait(bridge, [invalid])
        assert isinstance(invalid.exception(), ValueError)
    finally:
        bridge.shutdown()


def test_reads_and_exports_are_queued_with_their_arguments() -> None:
    path = r"C:\Users\Test\Documents\Gaming.json"
    engine = FakeEngine(elevated=False, profile_files={path: starter_text("gaming")})
    bridge = EngineBridge(module=engine)  # type: ignore[arg-type]
    try:
        read = bridge.read_profile(path)
        candidates = bridge.profile_candidates()
        wait(bridge, [read, candidates])
        assert engine.calls_named("profile_read") == [(path,)]
        assert set(read.result()) == SUMMARY_KEYS
        assert candidates.result() == {"rows": [], "other_account": None, "warnings": []}
        engine.applied.add("gaming.game_mode")
        out = r"C:\Users\Test\Documents\Mine.json"
        keys = ["tweak:gaming.game_mode"]
        export = bridge.export_profile(out, "Mine", "", keys)
        keys.append("tweak:changed.later")  # the bridge copied the list when the call was queued
        wait(bridge, [export])
        assert engine.calls_named("profile_export") == [(out, "Mine", "", ["tweak:gaming.game_mode"])]
        assert export.result()["missing"] == []
        assert json.loads(engine.exported[out])["tweaks"] == ["gaming.game_mode"]
    finally:
        bridge.shutdown()


def test_starters_are_synchronous_cached_and_do_not_sleep() -> None:
    engine = FakeEngine(delay=0.5)
    bridge = EngineBridge(module=engine)  # type: ignore[arg-type]
    try:
        started = time.perf_counter()
        starters = bridge.profile_starters()
        assert time.perf_counter() - started < 0.1, "the starter table was read with the call delay"
        assert not bridge.busy, "the starter table is not queued on the worker"
        assert [s["id"] for s in starters] == ["gaming", "privacy", "clean"]
        assert bridge.profile_starters() is starters
        assert engine.calls_named("profile_starters") == [()]
    finally:
        bridge.shutdown()


def test_supports_reports_an_engine_without_profiles() -> None:
    bridge = EngineBridge(module=FakeEngine(unsupported=["profile_apply"]))  # type: ignore[arg-type]
    try:
        assert not bridge.supports("profile_apply")
        assert bridge.supports("profile_starters")
    finally:
        bridge.shutdown()


def test_fake_profile_signatures_match_real_module() -> None:
    module = real_module()
    assert_signatures_match(FakeEngine(elevated=True), module, PROFILE_SIGNATURES)


def test_real_starters_have_the_fake_shape() -> None:
    module = real_module()
    starters = list(module.profile_starters())
    assert [s["id"] for s in starters] == [s["id"] for s in FAKE_STARTERS]
    for starter, fake in zip(starters, FAKE_STARTERS, strict=True):
        assert set(starter) == set(fake)
        assert set(starter["counts"]) == COUNT_KEYS
        assert starter["counts"]["tweaks"] > 0


def test_real_profile_check_round_trips_a_starter() -> None:
    module = real_module()
    starter = list(module.profile_starters())[0]
    summary = module.profile_check(starter["text"])
    assert set(summary) == SUMMARY_KEYS
    assert summary["text"] == starter["text"]
    assert summary["name"] == starter["name"]
    assert module.profile_check(summary["text"])["text"] == summary["text"]


@pytest.mark.parametrize(
    "text",
    [
        "[]",
        '{"format": "cairn.profile", "schema": 2, "name": "x", "tweaks": ["gaming.game_mode"]}',
        '{"format": "cairn.profile", "schema": 1, "name": "x", "tweaks": ["HKLM\\\\Software\\\\x"]}',
        '{"format": "cairn.profile", "schema": 1, "name": "x", "apps": ["C:\\\\x"]}',
        '{"format": "cairn.profile", "schema": 1, "name": "x", "tweaks": ["a.b"], "command": "x"}',
        '{"format": "cairn.profile", "schema": 1, "name": "x", "dns": {"wifi": {"ipv4": "1.1.1.1"}}}',
    ],
)
def test_real_profile_check_refuses_hostile_text(text: str) -> None:
    module = real_module()
    with pytest.raises(ValueError):
        module.profile_check(text)
