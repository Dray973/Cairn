"""EngineBridge and the maintenance tools: which calls go through the worker, which run
synchronously on the UI thread, how arguments are passed, and the FakeEngine's parity with
the real engine module.

The real-module tests only read: the catalog, the fixed drives, System Protection, the
Windows tools and a dry-run plan. No tool is started.
"""

from __future__ import annotations

import os
import time
import types
from typing import Any

import pytest

from optimizer.bridge.engine import EngineBridge, EngineUnavailable, load_engine_module

from .bridge_support import assert_signatures_match, wait
from .fake_engine import FakeEngine
from .fake_tools import JOB_SNAPSHOT_KEYS, JOB_VIEW_KEYS, MAX_JOB_LINES, TOOLS, VOLUMES

TOOLS_NAMES = (
    "tools_catalog",
    "tools_volumes",
    "tools_start",
    "tools_job",
    "tools_jobs",
    "tools_cancel",
    "tools_open_log",
    "tools_shutdown",
    "tools_restore_point",
    "tools_windows",
    "tools_open_windows",
    "system_restore_enabled",
)


def _start(engine: FakeEngine, tool: str = "sfc_verify", volume: str | None = None) -> int:
    return int(engine.tools_start(tool, volume, dry_run=False)["job"]["id"])


def test_start_and_plan_pass_every_parameter_and_dry_run_by_keyword() -> None:
    engine = FakeEngine(elevated=True)
    bridge = EngineBridge(module=engine)  # type: ignore[arg-type]
    try:
        plan = bridge.plan_tool("disk_check", "C:")
        start = bridge.start_tool("sfc_verify")
        wait(bridge, [plan, start])
        assert engine.calls_named("tools_start") == [("disk_check", "C:", True), ("sfc_verify", None, False)]
        assert plan.result()["job"] is None
        assert plan.result()["plan"]["command_line"] == "chkdsk.exe C:"
        job = start.result()["job"]
        assert set(job) == set(JOB_SNAPSHOT_KEYS)
        assert job["state"] == "running"
        assert job["detached"] and not job["cancellable"]
    finally:
        bridge.shutdown()


def test_tool_polling_is_synchronous_and_never_busy() -> None:
    engine = FakeEngine(elevated=True, delay=0.5)
    bridge = EngineBridge(module=engine)  # type: ignore[arg-type]
    try:
        disk_check = _start(engine, "disk_check", "C:")
        engine.tool_emit(disk_check, "Stage 1: Examining basic file system structure ...", progress=12.0)
        started = time.perf_counter()
        view = bridge.tool_job(disk_check)
        jobs = bridge.tool_jobs()
        stopped = bridge.cancel_tool(disk_check)
        elapsed = time.perf_counter() - started
        assert elapsed < 0.1, f"polling slept for {elapsed:.2f} s"
        assert not bridge.busy, "polling never occupies the worker"
        assert view is not None and view["lines"] == ["Stage 1: Examining basic file system structure ..."]
        assert view["progress"] == 12.0
        assert [j["id"] for j in jobs] == [disk_check]
        assert stopped is True
        assert bridge.tool_job(disk_check)["state"] == "cancelled"  # type: ignore[index]
        assert bridge.tool_job(999) is None
        assert engine.calls_named("tools_cancel") == [(disk_check,)]
    finally:
        bridge.shutdown()


def test_cancel_refuses_tools_that_cannot_be_stopped() -> None:
    engine = FakeEngine(elevated=True)
    bridge = EngineBridge(module=engine)  # type: ignore[arg-type]
    try:
        job = _start(engine)
        with pytest.raises(RuntimeError, match="can't be stopped"):
            bridge.cancel_tool(job)
        with pytest.raises(RuntimeError, match="no tool job"):
            bridge.cancel_tool(999)
    finally:
        bridge.shutdown()


def test_actions_that_start_processes_run_on_the_worker() -> None:
    engine = FakeEngine(elevated=True, delay=0.05)
    bridge = EngineBridge(module=engine)  # type: ignore[arg-type]
    try:
        job = _start(engine)
        futures = [
            bridge.tools_catalog(),
            bridge.tools_volumes(),
            bridge.system_restore_enabled(),
            bridge.windows_tools(),
            bridge.open_tool_log(job),
            bridge.create_restore_point(),
            bridge.open_windows_tool("task_manager"),
        ]
        assert bridge.busy
        wait(bridge, futures)
        assert [f.exception() for f in futures] == [None] * len(futures)
        assert len(futures[0].result()) == len(TOOLS)
        assert futures[2].result() is True
        assert futures[5].result()["sequence"] == 7
        assert engine.calls_named("tools_open_log") == [(job,)]
        assert engine.calls_named("tools_restore_point") == [("Cairn manual checkpoint",)]
        assert engine.calls_named("tools_open_windows") == [("task_manager",)]
    finally:
        bridge.shutdown()


def test_tools_shutdown_tolerates_an_outdated_module() -> None:
    old = FakeEngine(elevated=True, unsupported=["tools_shutdown", "tools_jobs"])
    bridge = EngineBridge(module=old)  # type: ignore[arg-type]
    try:
        assert bridge.tools_shutdown() == []
        assert bridge.tool_jobs() == []
        assert not bridge.supports("tools_shutdown")
    finally:
        bridge.shutdown()

    bare = EngineBridge(module=types.SimpleNamespace())  # type: ignore[arg-type]
    try:
        assert bare.tools_shutdown() == []
        assert bare.tool_jobs() == []
        assert not bare.supports("tools_catalog")
    finally:
        bare.shutdown()


def test_tools_shutdown_stops_the_stoppable_job_and_refuses_new_starts() -> None:
    engine = FakeEngine(elevated=True)
    bridge = EngineBridge(module=engine)  # type: ignore[arg-type]
    try:
        disk_check = _start(engine, "disk_check", "C:")
        assert bridge.tools_shutdown() == [{"id": disk_check, "tool": "disk_check", "action": "stopped"}]
        assert bridge.tool_job(disk_check)["state"] == "cancelled"  # type: ignore[index]
        late = bridge.start_tool("sfc_verify")
        wait(bridge, [late])
        assert "closing" in str(late.exception())
    finally:
        bridge.shutdown()

    for detached, action in ((True, "left_running"), (False, "may_end_with_app")):
        engine = FakeEngine(elevated=True, tool_detached=detached)
        job = _start(engine, "sfc_scan")
        assert engine.tools_shutdown() == [{"id": job, "tool": "sfc_scan", "action": action}]

    # Like the engine's, a job whose process has ended runs on while its result is judged,
    # finishes while closing waits for it, and is not listed.
    engine = FakeEngine(elevated=True)
    job = _start(engine, "dism_check")
    engine.tool_exit(job, 0)
    view = engine.tools_job(job)
    assert view is not None
    assert (view["state"], view["exit_code"], view["exit_code_hex"]) == ("running", 0, "0x00000000")
    assert engine.tools_shutdown() == []
    assert engine.tools_job(job)["state"] == "completed"  # type: ignore[index]

    # A stop that arrives after a Check Disk ended on its own does not change its result.
    engine = FakeEngine(elevated=True)
    job = _start(engine, "disk_check", "C:")
    engine.tool_exit(job, 0)
    assert engine.tools_cancel(job)
    engine.tool_finish(job)
    finished = engine.tools_job(job)
    assert finished is not None
    assert (finished["state"], finished["cancel_requested"]) == ("succeeded", True)


def test_supports_reports_missing_tools_functions() -> None:
    bridge = EngineBridge(module=FakeEngine(unsupported=["tools_catalog"]))  # type: ignore[arg-type]
    try:
        assert not bridge.supports("tools_catalog")
        assert bridge.supports("tools_volumes")
    finally:
        bridge.shutdown()


def test_fake_job_views_page_and_report_skipped_lines() -> None:
    engine = FakeEngine(elevated=True)
    job = _start(engine)
    engine.tool_emit(job, *[f"line {i}" for i in range(1, 1201)])
    first = engine.tools_job(job, 0)
    assert first is not None
    assert set(first) == set(JOB_SNAPSHOT_KEYS) | set(JOB_VIEW_KEYS)
    assert (first["first"], first["next"], first["skipped"], first["more"]) == (1, 500, 0, True)
    assert len(first["lines"]) == 500
    last = engine.tools_job(job, 1000)
    assert last is not None
    assert (last["first"], last["next"], last["more"]) == (1001, 1200, False)
    idle = engine.tools_job(job, 1200)
    assert idle is not None
    assert (idle["lines"], idle["first"], idle["next"], idle["skipped"]) == ([], None, 1200, 0)

    # The job keeps its newest lines; a reader that fell behind learns how many it missed.
    engine.tool_emit(job, *[f"line {i}" for i in range(1201, 1201 + MAX_JOB_LINES)])
    behind = engine.tools_job(job, 500)
    assert behind is not None
    assert (behind["skipped"], behind["first"], behind["lines"][0]) == (700, 1201, "line 1201")
    current = engine.tools_job(job, 1200)
    assert current is not None
    assert (current["skipped"], current["first"]) == (0, 1201)


def test_fake_start_refusals_follow_the_engine_order() -> None:
    standard = FakeEngine(elevated=False, tool_blocked={"sfc_scan": "blocked"})
    plan = standard.tools_start("sfc_scan", None, dry_run=True)["plan"]
    assert plan["blocked_reason"] == "needs administrator rights"
    with pytest.raises(RuntimeError, match="elevated"):
        standard.tools_start("sfc_scan", None, dry_run=False)

    engine = FakeEngine(elevated=True, tool_blocked={"sfc_scan": "System File Checker is already running"})
    with pytest.raises(RuntimeError, match="System File Checker is already running"):
        engine.tools_start("sfc_scan")
    with pytest.raises(RuntimeError, match="Retrim is for SSDs"):
        engine.tools_start("drive_retrim", "d")
    with pytest.raises(RuntimeError, match="Z: is not a fixed drive"):
        engine.tools_start("disk_check", "Z:\\")
    wrong = (("sfc_verify", "C:"), ("disk_check", None), ("disk_check", "C:\\Windows"), ("x", None))
    for tool, volume in wrong:
        with pytest.raises(ValueError):
            engine.tools_start(tool, volume, dry_run=True)
    _start(engine)
    with pytest.raises(RuntimeError, match="Check system files is already running in Cairn"):
        engine.tools_start("dism_check")
    busy_plan = engine.tools_start("dism_check", None, dry_run=True)["plan"]
    assert busy_plan["blocked_reason"] == "Check system files is already running in Cairn"

    failing = FakeEngine(elevated=True, tool_start_error="could not start: access denied")
    with pytest.raises(RuntimeError, match="access denied"):
        failing.tools_start("dism_check")


# -- the real engine module (read-only) ---------------------------------------------------


def _real_bridge() -> EngineBridge:
    try:
        bridge = EngineBridge()
    except EngineUnavailable as exc:
        pytest.skip(str(exc))
    if not bridge.supports("tools_catalog"):
        bridge.shutdown()
        pytest.skip("the deployed engine module has no maintenance tools")
    return bridge


def test_real_engine_tools_are_read_only() -> None:
    bridge = _real_bridge()
    try:
        catalog = bridge.tools_catalog()
        volumes = bridge.tools_volumes()
        windows = bridge.windows_tools()
        plan = bridge.plan_tool("sfc_verify")
        wait(bridge, [catalog, volumes, windows, plan], timeout=30)
        tools = catalog.result()
        assert [t["id"] for t in tools] == [t[0] for t in TOOLS]
        fake = FakeEngine(elevated=True)
        assert set(fake.tools_catalog()[0]) <= set(tools[0])
        letters = [str(v["letter"]).rstrip("\\").rstrip(":").upper() for v in volumes.result()]
        assert os.environ.get("SystemDrive", "C:").rstrip(":").upper() in letters
        assert set(VOLUMES[0]) <= set(volumes.result()[0])
        assert {"id", "title", "description", "requires_admin"} <= set(windows.result()[0])
        result: dict[str, Any] = plan.result()
        assert result["job"] is None
        assert set(fake.tools_start("sfc_verify", None, dry_run=True)["plan"]) <= set(result["plan"])
        assert str(result["plan"]["program"]).lower().endswith("\\system32\\sfc.exe")
    finally:
        bridge.shutdown()


def test_fake_tools_signatures_match_real_module() -> None:
    try:
        module = load_engine_module()
    except EngineUnavailable as exc:
        pytest.skip(str(exc))
    assert_signatures_match(FakeEngine(elevated=True), module, TOOLS_NAMES)
