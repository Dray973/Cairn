"""EngineBridge: serial execution on one worker thread, callbacks on the caller's thread,
and the restore-point policy."""

from __future__ import annotations

import itertools
import threading
import time
from concurrent.futures import Future
from typing import Any

import pytest

from optimizer import PROJECT_URL
from optimizer.bridge.engine import EngineBridge

from .bridge_support import wait as _wait
from .fake_engine import FakeEngine

# A recorded Game Mode value that changed after Cairn set it.
DRIFTED = {
    "target": "HKLM\\Test\\gaming.game_mode",
    "label": "gaming.game_mode",
    "expected": "1",
    "found": "0",
    "original": "not set",
    "back_to_original": False,
}


def test_calls_run_serially_on_one_worker_thread() -> None:
    # Every call records its start and its end, and the first call holds the worker until the
    # test has seen that the calls queued behind it did not start. The order of the records,
    # not a measured duration, shows whether calls overlapped.
    engine = FakeEngine(delay=0.05)
    records: list[tuple[str, int]] = []
    threads: set[str] = set()
    lock = threading.Lock()
    numbers = itertools.count()
    first_started = threading.Event()
    release_first = threading.Event()
    original = engine.scan

    def scan() -> int:
        with lock:
            number = next(numbers)
            records.append(("start", number))
        threads.add(threading.current_thread().name)
        if number == 0:
            first_started.set()
            release_first.wait(10.0)
        original()
        with lock:
            records.append(("end", number))
        return number

    engine.scan = scan  # type: ignore[method-assign]
    bridge = EngineBridge(module=engine)  # type: ignore[arg-type]
    try:
        futures = [bridge.scan() for _ in range(4)]
        assert first_started.wait(5.0), "the first call did not start"
        # Calls that could run beside the first one would start meanwhile.
        time.sleep(0.2)
        with lock:
            assert records == [("start", 0)], "a queued call started while the first one ran"
        assert bridge.busy
        release_first.set()
        _wait(bridge, futures)
        assert records == [(edge, number) for number in range(4) for edge in ("start", "end")], (
            "calls overlapped"
        )
        assert [f.result() for f in futures] == [0, 1, 2, 3], "calls ran in the order they were queued"
        assert len(threads) == 1
        assert threading.current_thread().name not in threads
        assert not bridge.busy
    finally:
        release_first.set()
        bridge.shutdown()


def test_callbacks_run_only_when_dispatched() -> None:
    bridge = EngineBridge(module=FakeEngine())  # type: ignore[arg-type]
    seen: list[str] = []
    try:
        future = bridge.scan(callback=lambda f: seen.append(threading.current_thread().name))
        while not future.done():
            time.sleep(0.005)
        assert seen == []
        assert bridge.dispatch_completed() == 1
        assert seen == [threading.current_thread().name]
        assert bridge.dispatch_completed() == 0
    finally:
        bridge.shutdown()


def test_errors_propagate_through_the_future() -> None:
    engine = FakeEngine()

    def boom() -> dict[str, Any]:
        raise RuntimeError("this operation requires an elevated (Administrator) process")

    engine.scan = boom  # type: ignore[method-assign]
    bridge = EngineBridge(module=engine)  # type: ignore[arg-type]
    try:
        future = bridge.scan()
        _wait(bridge, [future])
        with pytest.raises(RuntimeError, match="elevated"):
            future.result()
        assert not bridge.busy
    finally:
        bridge.shutdown()


def test_first_mutation_asks_for_a_restore_point_and_later_ones_skip() -> None:
    engine = FakeEngine()
    bridge = EngineBridge(module=engine)  # type: ignore[arg-type]
    try:
        assert bridge.next_mutation_creates_restore_point
        # Dry-run plans never consume the restore point.
        _wait(bridge, [bridge.plan_category("privacy"), bridge.plan_revert_all()])
        assert bridge.next_mutation_creates_restore_point
        _wait(bridge, [bridge.apply_category("privacy"), bridge.apply(["gaming.game_mode"])])
        assert engine.calls_named("apply_category")[-1] == ("privacy", "try", False)
        assert engine.calls_named("apply")[-1] == (["gaming.game_mode"], "skip", False)
        assert not bridge.next_mutation_creates_restore_point
    finally:
        bridge.shutdown()


def test_restore_point_request_survives_an_apply_that_fails_before_its_session() -> None:
    engine = FakeEngine()

    def locked(category: str, restore_point: str = "try", dry_run: bool = False) -> dict[str, Any]:
        engine._record("apply_category", category, restore_point, dry_run)
        raise RuntimeError("database is locked")

    engine.apply_category = locked  # type: ignore[method-assign]
    bridge = EngineBridge(module=engine)  # type: ignore[arg-type]
    try:
        failed = bridge.apply_category("privacy")
        _wait(bridge, [failed])
        assert isinstance(failed.exception(), RuntimeError)
        assert engine.calls_named("apply_category")[-1] == ("privacy", "try", False)
        assert bridge.next_mutation_creates_restore_point
        _wait(bridge, [bridge.apply(["gaming.game_mode"])])
        assert engine.calls_named("apply")[-1] == (["gaming.game_mode"], "try", False)
        assert not bridge.next_mutation_creates_restore_point
    finally:
        bridge.shutdown()


def test_restore_point_request_survives_an_apply_with_nothing_to_do() -> None:
    engine = FakeEngine()
    engine.applied.add("gaming.game_mode")
    bridge = EngineBridge(module=engine)  # type: ignore[arg-type]
    try:
        # Every recommended gaming change is applied, so no journal session opens.
        empty = bridge.apply_category("gaming")
        _wait(bridge, [empty])
        assert empty.result()["session_id"] is None
        assert bridge.next_mutation_creates_restore_point
        _wait(bridge, [bridge.apply(["privacy.advertising_id"])])
        assert engine.calls_named("apply")[-1] == (["privacy.advertising_id"], "try", False)
        assert not bridge.next_mutation_creates_restore_point
    finally:
        bridge.shutdown()


def test_plan_revert_is_a_dry_run() -> None:
    engine = FakeEngine()
    engine.applied.add("privacy.advertising_id")
    bridge = EngineBridge(module=engine)  # type: ignore[arg-type]
    try:
        plan = bridge.plan_revert(["privacy.advertising_id"])
        _wait(bridge, [plan])
        assert engine.calls_named("revert")[-1] == (["privacy.advertising_id"], True)
        assert plan.result()["actions"] == ["delete value: privacy.advertising_id"]
        assert "privacy.advertising_id" in engine.applied
    finally:
        bridge.shutdown()


def test_drift_checks_and_keep_as_is_are_worker_calls_with_copied_arguments() -> None:
    engine = FakeEngine(drift={"gaming.game_mode": [DRIFTED]})
    engine.applied.add("gaming.game_mode")
    bridge = EngineBridge(module=engine)  # type: ignore[arg-type]
    ids = ["gaming.game_mode"]
    record = {"hive": "HKLM", "key_path": "Test", "value_name": "gaming.game_mode"}
    selection: dict[str, Any] = {"registry": [record]}
    try:
        futures = [bridge.drift(), bridge.verify(ids), bridge.forget_targets(selection)]
        # Each call took a copy of its argument when it was queued.
        ids.append("privacy.advertising_id")
        selection["services"] = ["SysMain"]
        _wait(bridge, futures)
        drift, verify, forget = (f.result() for f in futures)
        assert [(e["item"], e["mismatch"]) for e in drift["entries"]] == [("gaming.game_mode", DRIFTED)]
        assert [e["item"] for e in verify["entries"]] == ["gaming.game_mode"]
        assert engine.calls_named("verify") == [(["gaming.game_mode"],)]
        assert engine.calls_named("forget_targets") == [({"registry": [record]},)]
        assert forget == {"forgotten": [DRIFTED], "refused": []}
        assert "gaming.game_mode" not in engine.applied
        assert not bridge.busy
    finally:
        bridge.shutdown()


def test_open_url_is_a_worker_call_and_supports_tells_what_an_older_engine_lacks() -> None:
    newer = ("drift", "verify", "forget_targets", "open_url")
    engine = FakeEngine()
    bridge = EngineBridge(module=engine)  # type: ignore[arg-type]
    try:
        assert all(bridge.supports(name) for name in newer)
        opened = bridge.open_url(PROJECT_URL)
        refused = bridge.open_url("http://example.com/")
        _wait(bridge, [opened, refused])
        assert opened.result() is None
        assert isinstance(refused.exception(), ValueError)
        assert engine.calls_named("open_url") == [(PROJECT_URL,), ("http://example.com/",)]
    finally:
        bridge.shutdown()

    older = EngineBridge(module=FakeEngine(unsupported=newer))  # type: ignore[arg-type]
    try:
        assert not any(older.supports(name) for name in newer)
        assert older.supports("revert_targets")
    finally:
        older.shutdown()


class _SettlingFuture:
    """A future that finishes between the first and second time its state is read."""

    def __init__(self) -> None:
        self.reads = 0

    def done(self) -> bool:
        self.reads += 1
        return self.reads > 1


def test_dispatch_never_drops_a_call_that_finishes_while_dispatching() -> None:
    bridge = EngineBridge(module=FakeEngine())  # type: ignore[arg-type]
    seen: list[Any] = []
    try:
        future = _SettlingFuture()
        bridge._pending.append((future, seen.append))  # type: ignore[arg-type]
        assert bridge.dispatch_completed() == 0
        assert bridge.dispatch_completed() == 1
        assert seen == [future]
        assert bridge.dispatch_completed() == 0
    finally:
        bridge.shutdown()


def test_failing_callback_does_not_drop_the_batch() -> None:
    bridge = EngineBridge(module=FakeEngine())  # type: ignore[arg-type]
    seen: list[str] = []

    def first_fails(_future: Future[Any]) -> None:
        seen.append("first")
        raise RuntimeError("first callback failed")

    def third_fails(_future: Future[Any]) -> None:
        seen.append("third")
        raise ValueError("third callback failed")

    try:
        futures = [
            bridge.scan(callback=first_fails),
            bridge.journal_summary(callback=lambda _f: seen.append("second")),
            bridge.catalog(callback=third_fails),
        ]
        while not all(f.done() for f in futures):
            time.sleep(0.005)
        with pytest.raises(RuntimeError, match="first callback failed"):
            bridge.dispatch_completed()
        assert seen == ["first", "second", "third"], "every callback of the batch ran"
        assert bridge.dispatch_completed() == 0, "the batch was dispatched once"
    finally:
        bridge.shutdown()


def test_shutdown_wait_drains_the_worker() -> None:
    engine = FakeEngine(delay=0.3)
    bridge = EngineBridge(module=engine)  # type: ignore[arg-type]
    running = bridge.scan()
    queued = bridge.journal_summary()
    deadline = time.monotonic() + 5.0
    while not engine.calls_named("scan"):
        assert time.monotonic() < deadline, "the first call did not start"
        time.sleep(0.005)
    bridge.shutdown(wait=True)
    assert running.done(), "shutdown(wait=True) returned while a call was still running"
    assert running.result()["items"]
    assert queued.cancelled(), "queued calls are cancelled"
    assert not engine.calls_named("journal_summary")


def test_restore_points_can_be_disabled() -> None:
    engine = FakeEngine()
    bridge = EngineBridge(module=engine, restore_points=False)  # type: ignore[arg-type]
    try:
        _wait(bridge, [bridge.apply_category("gaming")])
        assert engine.calls_named("apply_category")[-1] == ("gaming", "skip", False)
    finally:
        bridge.shutdown()


def test_real_engine_module_scan_is_read_only() -> None:
    try:
        bridge = EngineBridge()
    except RuntimeError as exc:
        pytest.skip(str(exc))
    try:
        future = bridge.scan()
        # A full scan of the real engine takes about 30 s on a 4-processor CI runner.
        _wait(bridge, [future], timeout=120)
        report = future.result()
        assert {c["category"] for c in report["categories"]} == {
            "privacy",
            "gaming",
            "performance",
            "interface",
            "security",
            "bloatware",
        }
        assert all(
            i["state"] in {"applied", "not_applied", "partial", "unavailable"} for i in report["items"]
        )
    finally:
        bridge.shutdown()
