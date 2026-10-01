"""EngineBridge and system information: the snapshot is read on the worker thread, and the
FakeEngine's snapshot matches the real engine module's shape and signature.

The real-module tests only call `sysinfo_snapshot`, which reads the system and changes
nothing; they skip when the engine is missing or older than the System section.
"""

from __future__ import annotations

import threading
import time
from typing import Any

import pytest

from optimizer.bridge.engine import EngineBridge, EngineUnavailable, load_engine_module

from .bridge_support import assert_signatures_match, wait
from .fake_engine import FakeEngine
from .fake_sysinfo import SYSINFO_ROW_KEYS, SYSINFO_SECTION_IDS

SYSINFO_NAMES = ("sysinfo_snapshot",)


def test_sysinfo_snapshot_runs_on_the_worker() -> None:
    engine = FakeEngine(delay=0.2)
    threads: list[str] = []
    original = engine.sysinfo_snapshot

    def snapshot() -> dict[str, Any]:
        threads.append(threading.current_thread().name)
        return original()

    engine.sysinfo_snapshot = snapshot  # type: ignore[method-assign]
    bridge = EngineBridge(module=engine)  # type: ignore[arg-type]
    seen: list[dict[str, Any]] = []
    try:
        started = time.monotonic()
        future = bridge.sysinfo_snapshot(callback=lambda f: seen.append(f.result()))
        assert time.monotonic() - started < 0.1, "the call blocked the calling thread"
        assert bridge.busy
        wait(bridge, [future])
        assert threads and threads[0] != threading.current_thread().name
        assert engine.calls_named("sysinfo_snapshot") == [()]
        assert [s["id"] for s in seen[0]["sections"]] == list(SYSINFO_SECTION_IDS)
        assert not bridge.busy
    finally:
        bridge.shutdown()


def test_sysinfo_failure_propagates_through_the_future() -> None:
    bridge = EngineBridge(module=FakeEngine(sysinfo_failure="cannot read the firmware table"))  # type: ignore[arg-type]
    try:
        future = bridge.sysinfo_snapshot()
        wait(bridge, [future])
        with pytest.raises(RuntimeError, match="firmware table"):
            future.result()
    finally:
        bridge.shutdown()


def test_supports_reports_a_missing_sysinfo_function() -> None:
    current = EngineBridge(module=FakeEngine())  # type: ignore[arg-type]
    outdated = EngineBridge(module=FakeEngine(unsupported=["sysinfo_snapshot"]))  # type: ignore[arg-type]
    try:
        assert current.supports("sysinfo_snapshot")
        assert not outdated.supports("sysinfo_snapshot")
    finally:
        current.shutdown()
        outdated.shutdown()


def test_fake_sysinfo_signature_matches_the_real_module() -> None:
    try:
        module = load_engine_module()
    except EngineUnavailable as exc:
        pytest.skip(str(exc))
    if getattr(module, "sysinfo_snapshot", None) is None:
        pytest.skip("the deployed engine module has no sysinfo_snapshot")
    assert_signatures_match(FakeEngine(), module, SYSINFO_NAMES)


def _all_rows(section: dict[str, Any]) -> list[dict[str, Any]]:
    return list(section["rows"]) + [r for g in section["groups"] for r in g["rows"]]


def test_real_module_snapshot_is_read_only_and_matches_the_fake() -> None:
    try:
        bridge = EngineBridge()
    except EngineUnavailable as exc:
        pytest.skip(str(exc))
    try:
        if not bridge.supports("sysinfo_snapshot"):
            pytest.skip("the deployed engine module has no sysinfo_snapshot")
        future = bridge.sysinfo_snapshot()
        wait(bridge, [future], timeout=30)
        real = future.result()
        fake = FakeEngine().sysinfo_snapshot()
        assert set(fake) <= set(real)
        assert set(fake["info"]) <= set(real["info"])
        assert [s["id"] for s in real["sections"]] == [s["id"] for s in fake["sections"]]
        assert [s["title"] for s in real["sections"]] == [s["title"] for s in fake["sections"]]
        fake_section_keys = set(fake["sections"][0])
        for section in real["sections"]:
            assert fake_section_keys <= set(section), f"section {section['id']} lacks keys"
            for group in section["groups"]:
                assert {"title", "rows"} <= set(group)
            for row in _all_rows(section):
                assert set(SYSINFO_ROW_KEYS) <= set(row), f"row {row.get('label')} lacks keys"
                assert row["level"] in {"normal", "good", "warning"}
        assert real["text"].strip()
        assert real["text"].startswith("Cairn ")
        private = [row["label"] for s in real["sections"] for row in _all_rows(s) if row["private"]]
        for label in private:
            assert f"  {label} " not in real["text"], f"the text holds the private row {label}"
        assert real["info"]["taken_at"]
    finally:
        bridge.shutdown()
