"""EngineBridge and the Storage section: which calls go through the worker, which run
synchronously on the UI thread, how arguments are passed, and the FakeEngine's parity with the
real engine module.

The real-module tests only read: the fixed drives, a dry-run speed-test plan, and a scan of a
pytest temporary folder. No speed test runs and nothing is removed.
"""

from __future__ import annotations

import time
import types
from pathlib import Path
from typing import Any

import pytest

from optimizer.bridge.engine import EngineBridge, EngineUnavailable, load_engine_module

from .bridge_support import assert_signatures_match, wait
from .fake_engine import FakeEngine
from .fake_jobs import HOST_SNAPSHOT_KEYS
from .fake_storage import (
    DUPES_BLOCK_KEYS,
    SCAN_BLOCK_KEYS,
    SCAN_PLAN_KEYS,
    SPEED_BLOCK_KEYS,
    SPEED_PLAN_KEYS,
    TREE_ROW_KEYS,
    VOLUME_KEYS,
)

STORAGE_SIGNATURES = (
    "storage_volumes",
    "storage_speed_start",
    "storage_speed_history",
    "storage_remove_leftover",
    "storage_scan_start",
    "storage_duplicates_start",
    "storage_job",
    "storage_jobs",
    "storage_cancel",
    "storage_result",
    "storage_scan_children",
    "storage_shutdown",
)
GIB = 1 << 30


def real_module() -> Any:
    try:
        module = load_engine_module()
    except EngineUnavailable:
        pytest.skip("the engine module is not deployed")
    if not callable(getattr(module, "storage_volumes", None)):
        pytest.skip("the deployed engine has no storage functions")
    return module


def scan_job(engine: FakeEngine, path: str = "C:\\") -> int:
    return int(engine.storage_scan_start(path)["job"]["id"])


def test_starts_and_plans_run_on_the_worker_with_dry_run_by_keyword() -> None:
    engine = FakeEngine(elevated=True, delay=0.05)
    bridge = EngineBridge(module=engine)  # type: ignore[arg-type]
    try:
        futures = [
            bridge.storage_volumes(),
            bridge.speed_history(),
            bridge.plan_speed_test("C:", GIB, 3),
            bridge.plan_storage_scan("C:\\Users\\Test"),
        ]
        assert bridge.busy
        wait(bridge, futures)
        assert [f.exception() for f in futures] == [None] * len(futures)
        volumes, history, speed, scan = (f.result() for f in futures)
        assert [v["letter"] for v in volumes] == ["C:", "D:"]
        assert set(volumes[0]) == set(VOLUME_KEYS)
        assert history[0]["volume"] == "C:"
        assert speed["job"] is None and set(speed["plan"]) == set(SPEED_PLAN_KEYS)
        assert scan["job"] is None and set(scan["plan"]) == set(SCAN_PLAN_KEYS)
        assert engine.calls_named("storage_speed_start") == [("C:", GIB, 3, True)]
        assert engine.calls_named("storage_scan_start") == [("C:\\Users\\Test", True)]

        start = bridge.start_storage_scan("C:\\")
        wait(bridge, [start])
        job = start.result()["job"]
        assert set(job) == set(HOST_SNAPSHOT_KEYS)
        assert job["lane"] == "storage" and job["kind"] == "space_scan" and job["title"] == "Scan of C:"
        engine.storage_finish(job["id"])
        dupes = [bridge.plan_duplicates(job["id"], 10 << 20), bridge.start_duplicates(job["id"], 1 << 20)]
        wait(bridge, dupes)
        assert dupes[0].result()["plan"]["blocked_reason"] is None
        assert dupes[1].result()["job"]["kind"] == "duplicates"
        assert engine.calls_named("storage_duplicates_start") == [
            (job["id"], 10 << 20, True),
            (job["id"], 1 << 20, False),
        ]
    finally:
        bridge.shutdown()


def test_worker_calls_propagate_failures() -> None:
    engine = FakeEngine(elevated=False, storage_history=RuntimeError("the history file is damaged"))
    bridge = EngineBridge(module=engine)  # type: ignore[arg-type]
    try:
        futures = [
            bridge.speed_history(),
            bridge.start_speed_test("C:", GIB, 3),
            bridge.remove_speed_leftover("C:\\CairnSpeedTest-0123abcd"),
            bridge.start_duplicates(999, 1 << 20),
        ]
        wait(bridge, futures)
        errors = [str(f.exception()) for f in futures]
        assert errors[0] == "the history file is damaged"
        assert "elevated" in errors[1]
        assert "elevated" in errors[2]
        assert errors[3] == "The scan results are no longer available; scan again."
    finally:
        bridge.shutdown()


def test_polling_reads_are_synchronous_and_never_busy() -> None:
    engine = FakeEngine(elevated=True, delay=0.5)
    bridge = EngineBridge(module=engine)  # type: ignore[arg-type]
    try:
        job = scan_job(engine)
        engine.storage_progress(job, scan={"files": 10, "folders": 2})
        started = time.perf_counter()
        snapshot = bridge.storage_job(job)
        jobs = bridge.storage_jobs()
        result_while_running = bridge.storage_result(job)
        children_while_running = bridge.storage_children(job, 0)
        elapsed = time.perf_counter() - started
        assert elapsed < 0.1, f"polling slept for {elapsed:.2f} s"
        assert not bridge.busy, "polling never occupies the worker"
        assert snapshot is not None and snapshot["detail"]["scan"]["files"] == 10
        assert [j["id"] for j in jobs] == [job]
        assert result_while_running is None and children_while_running is None

        engine.storage_finish(job)
        result = bridge.storage_result(job)
        assert result is not None and result["kind"] == "scan" and result["revision"] == 1
        page = bridge.storage_children(job, 0, "logical", 3)
        assert page is not None and page["order"] == "logical" and page["total"] == 6
        assert [r["kind"] for r in page["children"]] == ["folder", "folder", "more"]
        assert all(set(r) == set(TREE_ROW_KEYS) for r in page["children"])
        assert engine.calls_named("storage_scan_children")[-1] == (job, 0, "logical", 3)
        assert bridge.storage_job(999) is None

        other = scan_job(engine)
        assert bridge.cancel_storage(other) is True
        assert bridge.storage_job(other)["state"] == "cancelled"  # type: ignore[index]
        assert bridge.cancel_storage(other) is False
        with pytest.raises(RuntimeError, match="no storage job"):
            bridge.cancel_storage(999)
        # The newer scan replaced the older one's result.
        assert bridge.storage_children(job, 0) is None
        assert not bridge.busy
    finally:
        bridge.shutdown()


def test_close_path_calls_tolerate_an_outdated_module() -> None:
    bridge = EngineBridge(module=types.SimpleNamespace())  # type: ignore[arg-type]
    try:
        assert bridge.storage_jobs() == []
        assert bridge.storage_shutdown() == []
        assert not bridge.supports("storage_volumes")
    finally:
        bridge.shutdown()


def test_shutdown_stops_the_running_job() -> None:
    engine = FakeEngine(elevated=True)
    bridge = EngineBridge(module=engine)  # type: ignore[arg-type]
    try:
        job = scan_job(engine)
        assert bridge.storage_shutdown() == [{"id": job, "kind": "space_scan", "action": "stopped"}]
        with pytest.raises(RuntimeError, match="closing"):
            engine.storage_scan_start("C:\\")
    finally:
        bridge.shutdown()


def test_supports_reflects_an_outdated_engine() -> None:
    engine = FakeEngine(unsupported=["storage_volumes"])
    bridge = EngineBridge(module=engine)  # type: ignore[arg-type]
    try:
        assert not bridge.supports("storage_volumes")
        assert bridge.supports("storage_job")
    finally:
        bridge.shutdown()


def test_fake_arguments_are_checked_like_the_engine() -> None:
    engine = FakeEngine(elevated=True)
    with pytest.raises(ValueError):
        engine.storage_speed_start("CC", GIB, 3)
    with pytest.raises(ValueError):
        engine.storage_speed_start("C:", GIB + 1, 3)
    with pytest.raises(ValueError):
        engine.storage_speed_start("C:", 8 << 20, 3)
    with pytest.raises(ValueError):
        engine.storage_speed_start("C:", GIB, 10)
    with pytest.raises(ValueError):
        engine.storage_scan_start("relative\\path")
    with pytest.raises(ValueError):
        engine.storage_duplicates_start(1, 1000)
    with pytest.raises(ValueError):
        engine.storage_scan_children(1, 0, "size")
    with pytest.raises(ValueError):
        engine.storage_scan_children(1, 0, "allocated", 0)
    with pytest.raises(ValueError):
        engine.storage_remove_leftover("C:\\Windows")


def test_fake_signatures_match_real_module() -> None:
    try:
        module = load_engine_module()
    except EngineUnavailable:
        pytest.skip("the engine module is not deployed")
    assert_signatures_match(FakeEngine(), module, STORAGE_SIGNATURES)


def test_real_volumes_and_a_dry_run_plan() -> None:
    module = real_module()
    volumes = module.storage_volumes()
    assert volumes, "this PC has a fixed drive"
    assert all(set(v) == set(VOLUME_KEYS) for v in volumes)
    assert volumes[0]["system"]
    letter = volumes[0]["letter"]
    started = module.storage_speed_start(letter, dry_run=True)
    assert started["job"] is None
    plan = started["plan"]
    assert set(plan) == set(SPEED_PLAN_KEYS)
    assert plan["max_write_bytes"] == 13 * GIB
    # Test runs forbid a speed test at a volume root (or refuse it without elevation).
    assert plan["blocked_reason"]
    with pytest.raises(ValueError):
        module.storage_speed_start(letter, 1000, 3, dry_run=True)


def test_real_scan_of_a_temporary_folder(tmp_path: Path) -> None:
    module = real_module()
    (tmp_path / "sub").mkdir()
    (tmp_path / "sub" / "big.bin").write_bytes(b"\x01" * (2 << 20))
    (tmp_path / "small.txt").write_text("hello")
    started = module.storage_scan_start(str(tmp_path))
    assert set(started["plan"]) == set(SCAN_PLAN_KEYS)
    job = started["job"]
    assert set(job) == set(HOST_SNAPSHOT_KEYS)
    deadline = time.monotonic() + 30
    snapshot = module.storage_job(job["id"])
    while snapshot["state"] == "running":
        assert time.monotonic() < deadline, "the scan did not finish"
        time.sleep(0.02)
        snapshot = module.storage_job(job["id"])
    assert snapshot["state"] == "succeeded"
    # The finished job keeps its own progress block, as the fake does.
    detail = snapshot["detail"]
    assert set(detail) == {"phase", "speed", "scan", "duplicates"}
    assert detail["phase"] == "done"
    assert detail["speed"] is None and detail["duplicates"] is None
    assert set(detail["scan"]) == set(SCAN_BLOCK_KEYS)
    assert detail["scan"]["files"] == 2 and detail["scan"]["folders"] == 1
    result = module.storage_result(job["id"])
    fake = FakeEngine(elevated=True)
    fake_job = scan_job(fake)
    fake.storage_finish(fake_job)
    fake_result = fake.storage_result(fake_job)
    assert set(result) == set(fake_result)
    assert set(result["summary"]) == set(fake_result["summary"])
    assert set(result["root"]) == set(TREE_ROW_KEYS)
    assert result["summary"]["files"] == 2
    page = module.storage_scan_children(job["id"], 0)
    assert set(page) == {"node", "order", "children", "total"}
    assert [r["name"] for r in page["children"]] == ["sub", "1 smaller file"]
    assert all(set(r) == set(TREE_ROW_KEYS) for r in page["children"])
    assert module.storage_scan_children(job["id"], 999_999) is None
    assert module.storage_cancel(job["id"]) is False


def test_detail_blocks_have_the_documented_keys() -> None:
    engine = FakeEngine(elevated=True)
    speed = engine.storage_speed_start("C:", GIB, 3)["job"]
    assert set(speed["detail"]["speed"]) == set(SPEED_BLOCK_KEYS)
    engine.storage_cancel(speed["id"])
    scan = engine.storage_scan_start("C:\\")["job"]
    assert set(scan["detail"]["scan"]) == set(SCAN_BLOCK_KEYS)
    engine.storage_finish(scan["id"])
    dupes = engine.storage_duplicates_start(scan["id"])["job"]
    assert set(dupes["detail"]["duplicates"]) == set(DUPES_BLOCK_KEYS)
