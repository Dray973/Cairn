"""EngineBridge health calls: the checkup and the boot history run on the worker and carry
engine errors; the Windows Update search is started, read and stopped synchronously; an
outdated engine without them is tolerated; the FakeEngine mirrors the engine's signatures.

Of the real module only `health_update_scan` (the idle view; nothing starts) and
`health_boot_history(limit=5)` (an event-log read that needs administrator rights) are called.
`health_security_checkup` and `health_update_scan_start` never run on the real module: Windows
Update Agent objects can start the Windows Update service.
"""

from __future__ import annotations

import inspect
import threading
from typing import Any

import pytest

from optimizer.bridge.engine import EngineBridge, EngineUnavailable, load_engine_module

from .bridge_support import assert_signatures_match, wait
from .fake_engine import FakeEngine

HEALTH_SIGNATURES = {
    "health_security_checkup": "()",
    "health_update_scan_start": "(online=False)",
    "health_update_scan": "()",
    "health_update_scan_cancel": "()",
    "health_boot_history": "(limit=60)",
}
VIEW_KEYS = {"state", "online", "started_at", "finished_at", "elapsed_ms", "updates", "error"}


def recorder(engine: FakeEngine, name: str) -> list[bool]:
    """Replaces the fake's `name` with a wrapper that records whether it ran on a worker."""
    threads: list[bool] = []
    real = getattr(engine, name)

    def record(*args: Any, **kwargs: Any) -> Any:
        threads.append(threading.current_thread() is not threading.main_thread())
        return real(*args, **kwargs)

    setattr(engine, name, record)
    return threads


def test_checkup_and_boot_history_run_on_the_worker() -> None:
    engine = FakeEngine(elevated=True)
    checkup = recorder(engine, "health_security_checkup")
    boots = recorder(engine, "health_boot_history")
    bridge = EngineBridge(module=engine)  # type: ignore[arg-type]
    try:
        first = bridge.security_checkup()
        second = bridge.boot_history(limit=7)
        wait(bridge, [first, second])
        assert checkup == [True]
        assert boots == [True]
        assert len(first.result()["checks"]) == 24
        assert engine.calls_named("health_boot_history") == [(7,)]
        assert second.result()["access"] == "ok"
    finally:
        bridge.shutdown(wait=True)


def test_worker_failures_reach_the_future() -> None:
    engine = FakeEngine(elevated=True, health_failure="WMI is broken", health_boot_failure="log is corrupt")
    bridge = EngineBridge(module=engine)  # type: ignore[arg-type]
    try:
        first = bridge.security_checkup()
        second = bridge.boot_history()
        wait(bridge, [first, second])
        assert isinstance(first.exception(), RuntimeError)
        assert str(first.exception()) == "WMI is broken"
        assert str(second.exception()) == "log is corrupt"
    finally:
        bridge.shutdown(wait=True)


def test_update_scan_calls_are_synchronous() -> None:
    engine = FakeEngine(elevated=False)
    bridge = EngineBridge(module=engine)  # type: ignore[arg-type]
    try:
        view = bridge.start_update_scan(True)
        assert view["state"] == "running"
        assert view["online"] is True
        assert set(view) == VIEW_KEYS
        with pytest.raises(RuntimeError, match="already being checked"):
            bridge.start_update_scan(False)
        assert bridge.update_scan()["state"] == "running"
        assert bridge.cancel_update_scan() is True
        assert bridge.update_scan()["state"] == "cancelled"
        assert bridge.cancel_update_scan() is False
        names = [name for name, _ in engine.calls if name.startswith("health_update_scan")]
        assert names == [
            "health_update_scan_start",
            "health_update_scan_start",
            "health_update_scan",
            "health_update_scan_cancel",
            "health_update_scan",
            "health_update_scan_cancel",
        ]
        assert engine.calls_named("health_update_scan_start") == [(True,), (False,)]
        # Nothing was queued on the worker.
        assert not bridge.busy
    finally:
        bridge.shutdown(wait=True)


def test_a_finished_scan_is_reported_and_a_new_one_starts() -> None:
    engine = FakeEngine(elevated=True)
    bridge = EngineBridge(module=engine)  # type: ignore[arg-type]
    try:
        bridge.start_update_scan(False)
        engine.update_scan_finish([{"title": "2026-09 Security Update", "security": True}])
        view = bridge.update_scan()
        assert view["state"] == "done"
        assert view["updates"][0]["security"] is True
        assert set(view["updates"][0]) == {
            "title",
            "kb",
            "msrc_severity",
            "security",
            "downloaded",
            "released_at",
        }
        bridge.start_update_scan(True)
        engine.update_scan_finish(error="Windows Update could not be reached; check the internet connection.")
        assert bridge.update_scan()["state"] == "failed"
    finally:
        bridge.shutdown(wait=True)


def test_start_errors_are_raised() -> None:
    engine = FakeEngine(elevated=True, health_update_scan_error="The Windows Update service is disabled.")
    bridge = EngineBridge(module=engine)  # type: ignore[arg-type]
    try:
        with pytest.raises(RuntimeError, match="service is disabled"):
            bridge.start_update_scan(False)
    finally:
        bridge.shutdown(wait=True)


def test_an_outdated_engine_is_tolerated() -> None:
    engine = FakeEngine(
        elevated=True,
        unsupported=[
            "health_update_scan",
            "health_update_scan_cancel",
            "health_security_checkup",
            "health_boot_history",
        ],
    )
    bridge = EngineBridge(module=engine)  # type: ignore[arg-type]
    try:
        assert bridge.update_scan() is None
        assert bridge.cancel_update_scan() is False
        assert not bridge.supports("health_security_checkup")
        assert not bridge.supports("health_boot_history")
        assert bridge.supports("health_update_scan_start")
    finally:
        bridge.shutdown(wait=True)


def test_boot_history_needs_administrator_rights_in_the_fake() -> None:
    standard = FakeEngine(elevated=False).health_boot_history(limit=5)
    assert standard["access"] == "needs_admin"
    assert standard["boots"] == [] and standard["slow_items"] == []
    elevated = FakeEngine(elevated=True, health_boot_count=4).health_boot_history(limit=3)
    assert len(elevated["boots"]) == 3
    missing = FakeEngine(elevated=True, health_boot_access="log_missing").health_boot_history()
    assert missing["access"] == "log_missing" and missing["boots"] == []
    for limit in (0, -1, 501):
        with pytest.raises(ValueError, match="between 1 and 500"):
            FakeEngine(elevated=True).health_boot_history(limit=limit)


def test_fake_checkup_follows_elevation_and_the_scan() -> None:
    standard = FakeEngine(elevated=False).health_security_checkup()
    encryption = next(c for c in standard["checks"] if c["id"] == "encryption")
    assert encryption["state"] == "unknown" and encryption["needs_admin"]
    assert standard["update_scan_due"] is True
    engine = FakeEngine(elevated=True)
    engine.health_update_scan_start(online=False)
    running = engine.health_security_checkup()
    pending = next(c for c in running["checks"] if c["id"] == "pending_updates")
    assert pending["state"] == "checking"
    assert running["update_scan_due"] is False
    engine.update_scan_finish([])
    done = engine.health_security_checkup()
    assert next(c for c in done["checks"] if c["id"] == "pending_updates")["state"] == "good"
    assert done["update_scan_due"] is False
    # A stopped search counts as finished, as in the engine: no new one is due after it.
    engine.health_update_scan_start(online=True)
    assert engine.health_update_scan_cancel() is True
    stopped = engine.health_security_checkup()
    pending = next(c for c in stopped["checks"] if c["id"] == "pending_updates")
    assert pending["summary"] == "The check was stopped"
    assert stopped["update_scan_due"] is False
    engine.health_update_scan_start(online=False)
    engine.update_scan_finish(error="Windows Update could not be reached; check the internet connection.")
    assert engine.health_security_checkup()["update_scan_due"] is False


def test_boot_history_notes_and_errors_in_the_fake() -> None:
    plain = FakeEngine(elevated=True).health_boot_history()
    assert plain["notes"] == [] and plain["errors"] == []
    engine = FakeEngine(
        elevated=True,
        health_boot_notes=["The startup list could not be read."],
        health_boot_errors=["Could not read the start types: no access"],
    )
    history = engine.health_boot_history()
    assert history["notes"] == ["The startup list could not be read."]
    assert history["errors"] == ["Could not read the start types: no access"]
    # Nothing was read without administrator rights, so nothing can have gone wrong.
    engine.elevated = False
    standard = engine.health_boot_history()
    assert standard["notes"] == [] and standard["errors"] == []


def _parameters(fn: Any) -> str:
    """The parameters of `fn` as `(name, name=default)`, without annotations."""
    parts = [
        p.name if p.default is inspect.Parameter.empty else f"{p.name}={p.default!r}"
        for p in inspect.signature(fn).parameters.values()
    ]
    return f"({', '.join(parts)})"


def test_fake_signatures_are_the_contract() -> None:
    fake = FakeEngine()
    for name, signature in HEALTH_SIGNATURES.items():
        assert _parameters(getattr(fake, name)) == signature, name


def _real_module() -> Any:
    try:
        module = load_engine_module()
    except EngineUnavailable as exc:
        pytest.skip(str(exc))
    if not hasattr(module, "health_update_scan"):
        pytest.skip("the deployed engine module has no health functions yet")
    return module


def test_fake_health_signatures_match_real_module() -> None:
    module = _real_module()
    assert_signatures_match(FakeEngine(), module, HEALTH_SIGNATURES)


def test_real_module_update_scan_view_is_idle() -> None:
    module = _real_module()
    view = module.health_update_scan()
    assert set(view) == VIEW_KEYS
    assert view["state"] == "idle"


def test_real_module_boot_history_reads_or_needs_admin() -> None:
    module = _real_module()
    history = module.health_boot_history(limit=5)
    assert history["access"] in ("ok", "needs_admin", "log_disabled")
    assert len(history["boots"]) <= 5
    if not module.is_elevated():
        assert history["access"] == "needs_admin"
    for limit in (0, -1, 501):
        with pytest.raises(ValueError, match="between 1 and 500"):
            module.health_boot_history(limit=limit)
