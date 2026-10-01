"""EngineBridge and scheduled maintenance: which calls go through the worker, which run
synchronously on the UI thread, how arguments are passed, the fallbacks for an outdated
engine, and the FakeEngine's parity with the real engine module.

The real-module tests only read: the status, a dry-run plan and the run monitor. Nothing is
registered, started or removed.
"""

from __future__ import annotations

import json
import time

import pytest

from optimizer.bridge.engine import EngineBridge, EngineUnavailable, load_engine_module

from .bridge_support import assert_signatures_match, wait
from .fake_engine import FakeEngine
from .fake_maintenance import DEFAULTS, TASK_PATH, run_dict

MAINTENANCE_NAMES = (
    "maintenance_status",
    "maintenance_set_schedule",
    "maintenance_remove_unrecorded",
    "maintenance_run_now",
    "maintenance_acknowledge",
    "maintenance_open_log",
    "maintenance_watch",
    "maintenance_progress",
)


def _bridge(**options: object) -> tuple[EngineBridge, FakeEngine]:
    engine = FakeEngine(elevated=True, **options)  # type: ignore[arg-type]
    return EngineBridge(module=engine), engine  # type: ignore[arg-type]


def test_plans_and_saves_pass_dry_run_by_keyword() -> None:
    bridge, engine = _bridge()
    try:
        config = dict(DEFAULTS, day="monday")
        plan = bridge.plan_maintenance_schedule(config)
        saved = bridge.set_maintenance_schedule(config)
        wait(bridge, [plan, saved])
        assert engine.calls_named("maintenance_set_schedule") == [(config, True), (config, False)]
        assert plan.result()["creates"] is True
        assert saved.result()["outcome"] == "created"
        assert engine.maintenance_task == config
    finally:
        bridge.shutdown()


def test_turning_off_undoes_the_task_record() -> None:
    bridge, engine = _bridge(maintenance_task=DEFAULTS)
    try:
        assert engine.journal_summary()["task_definitions_active"] == 1
        future = bridge.turn_off_maintenance(TASK_PATH)
        wait(bridge, [future])
        assert engine.calls_named("revert_targets") == [({"task_definitions": [TASK_PATH]}, False)]
        assert future.result()["task_definitions_deleted"] == 1
        assert engine.maintenance_task is None
        assert engine.journal_summary()["task_definitions_active"] == 0
    finally:
        bridge.shutdown()


def test_worker_calls_are_queued() -> None:
    runs = [run_dict(1, "attention", attention=["x"])]
    bridge, engine = _bridge(maintenance_task=DEFAULTS, maintenance_runs=runs, delay=0.2)
    try:
        futures = [
            bridge.maintenance_status(),
            bridge.run_maintenance_now(),
            bridge.acknowledge_maintenance(1),
            bridge.open_maintenance_log(1),
        ]
        assert bridge.busy
        wait(bridge, futures, timeout=10)
        assert [name for name, _ in engine.calls if name.startswith("maintenance_")] == [
            "maintenance_status",
            "maintenance_run_now",
            "maintenance_acknowledge",
            "maintenance_open_log",
        ]
        assert futures[1].result() == {"requested": True, "task_path": TASK_PATH}
        assert futures[2].result() is True
        assert engine.calls_named("maintenance_open_log") == [(1,)]
    finally:
        bridge.shutdown()


def test_removing_an_unrecorded_task_passes_dry_run_false() -> None:
    bridge, engine = _bridge(maintenance_task=DEFAULTS, maintenance_recorded=False)
    try:
        future = bridge.remove_unrecorded_maintenance()
        wait(bridge, [future])
        assert engine.calls_named("maintenance_remove_unrecorded") == [(False,)]
        assert future.result() == {"task_path": TASK_PATH, "removed": True, "planned": False}
    finally:
        bridge.shutdown()


def test_watching_and_progress_are_synchronous_and_never_busy() -> None:
    bridge, engine = _bridge(maintenance_task=DEFAULTS, delay=0.5)
    try:
        started = time.perf_counter()
        assert bridge.maintenance_progress() is None
        bridge.maintenance_watch(expect_start=True)
        observation = bridge.maintenance_progress()
        elapsed = time.perf_counter() - started
        assert elapsed < 0.1, f"watching slept for {elapsed:.2f} s"
        assert not bridge.busy
        assert observation is not None and observation["waiting_for_start"] is True
        assert engine.calls_named("maintenance_watch") == [(True,)]
        engine.maintenance_begin_run()
        engine.maintenance_step("system_files", "Checking system files… 45%", 2, 3, 45.0)
        observation = bridge.maintenance_progress()
        assert observation is not None and observation["running"] is True
        assert observation["run"]["progress"]["percent"] == 45.0
    finally:
        bridge.shutdown()


def test_an_outdated_engine_falls_back() -> None:
    bridge, _ = _bridge(unsupported=("maintenance_watch", "maintenance_progress", "maintenance_status"))
    try:
        assert not bridge.supports("maintenance_status")
        assert bridge.supports("maintenance_run_now")
        bridge.maintenance_watch(expect_start=True)
        assert bridge.maintenance_progress() is None
    finally:
        bridge.shutdown()


def test_the_fake_refuses_what_the_engine_refuses() -> None:
    engine = FakeEngine(elevated=False)
    with pytest.raises(ValueError, match="Recycle Bin"):
        engine.maintenance_set_schedule(dict(DEFAULTS, targets=["recycle_bin"]), dry_run=True)
    with pytest.raises(ValueError, match="at least one"):
        engine.maintenance_set_schedule(
            dict(DEFAULTS, targets=[], system_file_check=False, component_store_check=False), dry_run=True
        )
    for time_of_day in ("24:00", "7:5", ""):
        with pytest.raises(ValueError, match="time of day"):
            engine.maintenance_set_schedule(dict(DEFAULTS, time=time_of_day), dry_run=True)
    untimed = {k: v for k, v in DEFAULTS.items() if k != "time"}
    with pytest.raises(ValueError, match="time of day"):
        engine.maintenance_set_schedule(untimed, dry_run=True)
    with pytest.raises(ValueError, match="unknown field"):
        engine.maintenance_set_schedule(dict(DEFAULTS, sfc_scan=True), dry_run=True)
    as_text = engine.maintenance_set_schedule(json.dumps(dict(DEFAULTS, time="7:05")), dry_run=True)
    assert as_text["config"]["time"] == "07:05"
    assert engine.maintenance_set_schedule(DEFAULTS, dry_run=True)["blocked_reason"] is None
    with pytest.raises(RuntimeError, match="elevated"):
        engine.maintenance_set_schedule(DEFAULTS)
    with pytest.raises(RuntimeError, match="Turn on scheduled maintenance"):
        FakeEngine(elevated=True).maintenance_run_now()
    status = FakeEngine(elevated=True, maintenance_on_battery=True, maintenance_task=DEFAULTS)
    with pytest.raises(RuntimeError, match="battery"):
        status.maintenance_run_now()


def test_fake_maintenance_signatures_match_real_module() -> None:
    try:
        module = load_engine_module()
    except EngineUnavailable as exc:
        pytest.skip(str(exc))
    assert_signatures_match(FakeEngine(elevated=True), module, MAINTENANCE_NAMES)


def test_real_engine_checks_schedules_like_the_fake() -> None:
    try:
        module = load_engine_module()
    except EngineUnavailable as exc:
        pytest.skip(str(exc))
    if not hasattr(module, "maintenance_set_schedule"):
        pytest.skip("the deployed engine module has no scheduled maintenance")
    fake = FakeEngine(elevated=True)
    refused = [
        dict(DEFAULTS, time="24:00"),
        dict(DEFAULTS, targets=["recycle_bin"]),
        dict(DEFAULTS, sfc_scan=True),
        dict(DEFAULTS, targets=[], system_file_check=False, component_store_check=False),
    ]
    for config in refused:
        with pytest.raises(ValueError):
            fake.maintenance_set_schedule(config, dry_run=True)
        with pytest.raises(ValueError):
            module.maintenance_set_schedule(config, dry_run=True)
    text = json.dumps(dict(DEFAULTS, time="7:05"))
    assert (
        module.maintenance_set_schedule(text, dry_run=True)["config"]
        == (fake.maintenance_set_schedule(text, dry_run=True)["config"])
    )


def _real_bridge() -> EngineBridge:
    try:
        bridge = EngineBridge()
    except EngineUnavailable as exc:
        pytest.skip(str(exc))
    if not bridge.supports("maintenance_status"):
        bridge.shutdown()
        pytest.skip("the deployed engine module has no scheduled maintenance")
    return bridge


def test_real_engine_maintenance_reads_only() -> None:
    bridge = _real_bridge()
    try:
        status = bridge.maintenance_status()
        plan = bridge.plan_maintenance_schedule(DEFAULTS)
        wait(bridge, [status, plan], timeout=30)
        fake = FakeEngine(elevated=True)
        real_status = status.result()
        assert set(real_status) == set(fake.maintenance_status())
        assert [t["id"] for t in real_status["targets"]] == [
            t["id"] for t in fake.maintenance_status()["targets"]
        ]
        assert real_status["defaults"] == DEFAULTS
        result = plan.result()
        assert set(result) == set(fake.maintenance_set_schedule(DEFAULTS, dry_run=True))
        assert result["blocked_reason"], "a development copy is never a safe program location"
        assert str(result["task_path"]).startswith("\\Cairn\\Maintenance-S-1-5-")
        observation = bridge.maintenance_progress()
        assert observation is None or set(observation) == {
            "running",
            "waiting_for_start",
            "start_timed_out",
            "run",
            "observed_at",
            "error",
        }
    finally:
        bridge.shutdown()
