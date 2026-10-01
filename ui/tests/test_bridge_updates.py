"""EngineBridge winget and Windows Update calls: worker calls run on the worker and pass their
policies by keyword, job reads stay on the calling thread, and the FakeEngine mirrors the
engine's rules, job shapes and signatures.

Of the real module only read-only calls are made: winget's status (no process), the Windows
Update state and catalog, the install list, a check planned as a dry run and a Windows Update
change planned as a dry run (the journal is the test data folder's).
"""

from __future__ import annotations

import inspect
import threading
from typing import Any

import pytest

from optimizer.bridge.engine import EngineBridge, EngineUnavailable, load_engine_module

from .bridge_support import assert_signatures_match, wait
from .fake_engine import FakeEngine
from .fake_jobs import HOST_SNAPSHOT_KEYS, HOST_VIEW_KEYS
from .fake_updates import DEFAULT_APPS, UX_SETTINGS, WU_POLICY, WU_SETTINGS
from .test_view_helpers import ENGINE_SIGNATURES

UPDATES_NAMES = tuple(name for name in ENGINE_SIGNATURES if name.startswith("updates_"))
EDITOR = {
    "id": "Contoso.Editor",
    "source": "winget",
    "name": "Contoso Editor",
    "from": "1.2.0",
    "to": "1.3.0",
}


def recorder(engine: FakeEngine, name: str) -> list[tuple[tuple[Any, ...], dict[str, Any], bool]]:
    """Replaces the fake's `name` with a wrapper that records (args, kwargs, on a worker thread)."""
    calls: list[tuple[tuple[Any, ...], dict[str, Any], bool]] = []
    real = getattr(engine, name)

    def record(*args: Any, **kwargs: Any) -> Any:
        calls.append((args, kwargs, threading.current_thread() is not threading.main_thread()))
        return real(*args, **kwargs)

    setattr(engine, name, record)
    return calls


def parameters(fn: Any) -> str:
    """The parameter list as the engine module declares it, without annotations."""
    signature = inspect.signature(fn)
    plain = [p.replace(annotation=inspect.Parameter.empty) for p in signature.parameters.values()]
    return str(signature.replace(parameters=plain, return_annotation=inspect.Signature.empty))


def real_module() -> Any:
    try:
        module = load_engine_module()
    except EngineUnavailable as exc:
        pytest.skip(str(exc))
    if not callable(getattr(module, "updates_winget_status", None)):
        pytest.skip("the deployed engine module has no updates functions yet")
    return module


# -- the bridge ------------------------------------------------------------------------


def test_worker_calls_run_on_the_worker_and_job_reads_do_not() -> None:
    engine = FakeEngine(elevated=True)
    names = ("updates_winget_status", "updates_wu_state", "updates_app_list", "updates_start", "updates_job")
    calls = {name: recorder(engine, name) for name in names}
    bridge = EngineBridge(module=engine)  # type: ignore[arg-type]
    try:
        futures = [
            bridge.updates_winget_status(),
            bridge.updates_wu_state(),
            bridge.updates_app_list(),
            bridge.start_updates("scan", None),
        ]
        wait(bridge, futures)
        for name in names[:4]:
            assert [on_worker for _, _, on_worker in calls[name]] == [True], name
        job = futures[3].result()["job"]
        view = bridge.updates_job(job["id"])
        assert calls["updates_job"][-1][2] is False, "job reads stay on the calling thread"
        assert view is not None and view["state"] == "running"
        assert bridge.updates_jobs()[0]["id"] == job["id"]
        assert bridge.updates_result(job["id"]) is None, "no result is published yet"
        assert bridge.cancel_updates(job["id"]) is True
        assert bridge.updates_job(job["id"])["cancel_requested"] is True
    finally:
        bridge.shutdown()


def test_plans_and_changes_pass_their_policies_by_keyword() -> None:
    engine = FakeEngine(elevated=True)
    wu = recorder(engine, "updates_wu_set")
    starts = recorder(engine, "updates_start")
    bridge = EngineBridge(module=engine)  # type: ignore[arg-type]
    try:
        futures = [
            bridge.plan_wu_setting("pause", 14),
            bridge.set_wu_setting("restart_notify", True),
            bridge.plan_updates("upgrade", [EDITOR]),
            bridge.start_updates("upgrade", [EDITOR]),
        ]
        wait(bridge, futures)
        assert [c[:2] for c in wu] == [
            (("pause", 14), {"restore_point": "skip", "dry_run": True}),
            (("restart_notify", True), {"restore_point": "skip", "dry_run": False}),
        ]
        assert [c[:2] for c in starts] == [
            (("upgrade", [EDITOR]), {"dry_run": True}),
            (("upgrade", [EDITOR]), {"dry_run": False}),
        ]
        assert futures[0].result()["session_id"] is None
        assert futures[1].result()["writes"][0]["outcome"] == "applied"
        assert futures[2].result()["job"] is None
        assert futures[3].result()["job"]["kind"] == "winget_upgrade"
    finally:
        bridge.shutdown()


def test_items_are_copied_before_they_reach_the_worker() -> None:
    engine = FakeEngine(elevated=True)
    starts = recorder(engine, "updates_start")
    bridge = EngineBridge(module=engine)  # type: ignore[arg-type]
    try:
        items = [dict(EDITOR)]
        future = bridge.plan_updates("upgrade", items)
        items[0]["id"] = "Changed.Later"
        wait(bridge, [future])
        assert starts[0][0][1][0]["id"] == "Contoso.Editor"
    finally:
        bridge.shutdown()


def test_engine_errors_reach_the_future() -> None:
    bridge = EngineBridge(module=FakeEngine(elevated=False))  # type: ignore[arg-type]
    try:
        future = bridge.start_updates("upgrade", [EDITOR])
        wait(bridge, [future])
        assert isinstance(future.exception(), RuntimeError), "updates need an elevated process"
        plan = bridge.plan_updates("upgrade", [EDITOR])
        wait(bridge, [plan])
        assert plan.result()["plan"]["requires_admin"] is True, "a plan needs no elevation"
    finally:
        bridge.shutdown()

    bridge = EngineBridge(module=FakeEngine(elevated=True, wu_fail="pause"))  # type: ignore[arg-type]
    try:
        future = bridge.set_wu_setting("pause", 7)
        wait(bridge, [future])
        assert isinstance(future.exception(), RuntimeError)
        plan = bridge.plan_wu_setting("pause", 7)
        wait(bridge, [plan])
        assert plan.exception() is None, "a dry run never writes"
    finally:
        bridge.shutdown()


def test_supports_reports_an_outdated_engine() -> None:
    bridge = EngineBridge(module=FakeEngine(unsupported=["updates_winget_status"]))  # type: ignore[arg-type]
    try:
        assert not bridge.supports("updates_winget_status")
        assert bridge.supports("updates_wu_state")
    finally:
        bridge.shutdown()


def test_wu_catalog_is_read_once() -> None:
    engine = FakeEngine()
    bridge = EngineBridge(module=engine)  # type: ignore[arg-type]
    try:
        first = bridge.updates_wu_catalog()
        second = bridge.updates_wu_catalog()
        assert first is second
        assert len(engine.calls_named("updates_wu_catalog")) == 1
        assert [e["id"] for e in first] == [f"wu.{s}" for s in WU_SETTINGS]
    finally:
        bridge.shutdown()


def test_close_path_reads_tolerate_an_engine_without_them() -> None:
    engine = FakeEngine(unsupported=["updates_jobs", "updates_shutdown", "updates_wu_catalog"])
    bridge = EngineBridge(module=engine)  # type: ignore[arg-type]
    try:
        assert bridge.updates_jobs() == []
        assert bridge.updates_shutdown() == []
        assert bridge.updates_wu_catalog() == []
    finally:
        bridge.shutdown()


# -- the fake mirrors the engine -------------------------------------------------------


def test_fake_signatures_are_the_engines() -> None:
    fake = FakeEngine()
    for name in UPDATES_NAMES:
        assert parameters(getattr(fake, name)) == ENGINE_SIGNATURES[name], name
    assert len(UPDATES_NAMES) == 13


def test_fake_updates_signatures_match_real_module() -> None:
    assert_signatures_match(FakeEngine(), real_module(), UPDATES_NAMES)


def test_fake_jobs_have_the_hosts_keys() -> None:
    engine = FakeEngine(elevated=True)
    job = engine.updates_start("scan")["job"]
    assert tuple(job) == HOST_SNAPSHOT_KEYS
    assert job["lane"] == "winget" and job["kind"] == "winget_scan" and job["cancellable"] is True
    engine.updates_emit(job["id"], *[f"line {n}" for n in range(1, 8)])
    view = engine.updates_job(job["id"], 2)
    assert tuple(view) == HOST_VIEW_KEYS
    assert (view["lines"][0], view["first"], view["next"], view["more"]) == ("line 3", 3, 7, False)
    assert tuple(engine.updates_jobs()[0]) == HOST_SNAPSHOT_KEYS


def test_fake_scan_publishes_its_result_with_a_revision() -> None:
    engine = FakeEngine(elevated=False)
    job = engine.updates_start("scan")["job"]
    engine.updates_finish_scan(job["id"])
    result = engine.updates_result(job["id"])
    assert result is not None and result["revision"] == 1
    assert [r["id"] for r in result["upgrades"]] == ["Contoso.Editor", "Fabrikam.Player"]
    assert engine.updates_result(job["id"], since=1) is None
    snapshot = engine.updates_job(job["id"])
    assert snapshot["state"] == "succeeded" and snapshot["has_result"] and snapshot["result_revision"] == 1
    assert engine.updates_cancel(job["id"]) is False


def test_fake_refuses_like_the_engine() -> None:
    engine = FakeEngine(elevated=True)
    with pytest.raises(ValueError):
        engine.updates_start("scan", [EDITOR])
    with pytest.raises(ValueError):
        engine.updates_start("upgrade", [])
    with pytest.raises(ValueError):
        engine.updates_start("upgrade", [{"id": "-h"}])
    with pytest.raises(ValueError):
        engine.updates_start("refresh")
    engine.updates_start("scan")
    plan = engine.updates_start("upgrade", [EDITOR], dry_run=True)["plan"]
    assert plan["blocked_reason"] == "Cairn is already checking for app updates."
    with pytest.raises(RuntimeError, match="Cairn is already checking for app updates."):
        engine.updates_start("upgrade", [EDITOR])
    with pytest.raises(RuntimeError, match="no winget job"):
        engine.updates_cancel(999)

    missing = FakeEngine(elevated=True, winget="missing")
    assert missing.updates_winget_status()["availability"] == "missing"
    blocked = missing.updates_start("scan", dry_run=True)["plan"]["blocked_reason"]
    assert blocked is not None and blocked.startswith("winget (App Installer) isn't set up")


def test_fake_shutdown_stops_the_running_job_and_refuses_later_starts() -> None:
    engine = FakeEngine(elevated=True)
    job = engine.updates_start("upgrade", [EDITOR])["job"]
    assert engine.updates_shutdown() == [{"id": job["id"], "kind": "winget_upgrade", "action": "stopped"}]
    assert engine.updates_job(job["id"])["state"] == "cancelled"
    with pytest.raises(RuntimeError, match="Cairn is closing."):
        engine.updates_start("scan")


def test_fake_windows_update_changes_are_journaled_and_revert() -> None:
    engine = FakeEngine(elevated=True, wu_values={"ActiveHoursStart": 9})
    report = engine.updates_wu_set("active_hours", [8, 17])
    assert [w["target"] for w in report["writes"]] == [
        f"HKLM\\{UX_SETTINGS}\\ActiveHoursStart",
        f"HKLM\\{UX_SETTINGS}\\ActiveHoursEnd",
        f"HKLM\\{UX_SETTINGS}\\SmartActiveHoursState",
    ]
    assert report["writes"][0]["before"] == "0x00000009 (9)"
    state = engine.updates_wu_state()
    hours = next(s for s in state["settings"] if s["id"] == "active_hours")
    assert hours["value"] == {
        "kind": "active_hours",
        "automatic": False,
        "start": 8,
        "end": 17,
        "policy": False,
    }
    assert hours["by_cairn"] and hours["differs"]
    assert engine._wu_active_count() == 3
    exported = engine._wu_export()
    assert exported[0]["original"] == {"type": "Dword", "value": 9}
    assert engine.journal_summary()["registry_active"] == 3

    actions, restored, remaining = engine._wu_revert(
        [dict(t) for t in hours["targets"]] + [{"hive": "HKLM", "key_path": "Test", "value_name": "x"}], False
    )
    assert restored == 3 and len(actions) == 3
    assert remaining == [{"hive": "HKLM", "key_path": "Test", "value_name": "x"}]
    assert engine._wu_values == {"ActiveHoursStart": 9}
    assert engine._wu_active_count() == 0


def test_fake_windows_update_rules_follow_the_engine() -> None:
    home = FakeEngine(elevated=True, edition="Core")
    with pytest.raises(RuntimeError, match="Windows 11 Home ignores this setting"):
        home.updates_wu_set("defer_feature", 30)
    assert home.updates_wu_set("defer_feature", None, dry_run=True)["session_id"] is None
    policy = FakeEngine(elevated=True, wu_policy={"SetDisablePauseUXAccess": 1})
    with pytest.raises(RuntimeError, match="Your organization turned off pausing"):
        policy.updates_wu_set("pause", 7)
    engine = FakeEngine(elevated=False)
    for setting, value in (
        ("pause", 36),
        ("pause", True),
        ("active_hours", [8, 3]),
        ("active_hours", [5, 5]),
        ("exclude_drivers", None),
        ("defer_feature", 366),
        ("bogus", 1),
    ):
        with pytest.raises(ValueError):
            engine.updates_wu_set(setting, value, dry_run=True)
    with pytest.raises(RuntimeError, match="elevated"):
        engine.updates_wu_set("restart_notify", True)
    pause = engine.updates_wu_set("pause", 14, dry_run=True)
    assert [w["after"] for w in pause["writes"]][-1] == '"2026-10-09T10:00:00Z"'
    catalog = engine.updates_wu_catalog()
    assert catalog[3]["targets"] == [
        f"HKLM\\{WU_POLICY}\\DeferFeatureUpdatesPeriodInDays",
        f"HKLM\\{WU_POLICY}\\DeferFeatureUpdates",
    ]


def test_fake_install_list_validates_and_restores_the_defaults() -> None:
    engine = FakeEngine()
    assert [a["id"] for a in engine.updates_app_list()["apps"]] == [a["id"] for a in DEFAULT_APPS]
    saved = engine.updates_save_app_list(
        [{"id": "Contoso.Browser", "name": "Browser", "category": "browsers"}]
    )
    assert saved["custom"] is True and len(saved["apps"]) == 1
    for bad in (
        [{"id": "nope", "name": "x", "category": "browsers"}],
        [{"id": "A.B", "name": "", "category": "browsers"}],
        [{"id": "A.B", "name": "x", "category": "games"}],
        [{"id": "A.B", "name": "x", "category": "chat"}, {"id": "a.b", "name": "y", "category": "chat"}],
    ):
        with pytest.raises(ValueError):
            engine.updates_save_app_list(bad)
    assert engine.updates_save_app_list(None)["custom"] is False


# -- the real module, read-only ------------------------------------------------------


def test_real_winget_status_starts_nothing() -> None:
    status = real_module().updates_winget_status()
    assert set(status) == {"availability", "message", "location", "elevated", "min_version", "store_uri"}
    assert status["availability"] in ("ready", "missing", "other_user", "user_unknown")
    assert status["min_version"] == "1.6.0"


def test_real_windows_update_state_and_catalog() -> None:
    module = real_module()
    state = module.updates_wu_state()
    assert [s["id"] for s in state["settings"]] == list(WU_SETTINGS)
    assert set(state) == {"edition", "service", "restart_pending", "managed", "settings", "warnings"}
    catalog = module.updates_wu_catalog()
    assert catalog == FakeEngine().updates_wu_catalog(), "the fake's targets are the engine's"


def test_real_install_list_reads_the_test_data_folder() -> None:
    listing = real_module().updates_app_list()
    assert set(listing) == {"apps", "custom", "path", "warnings"}
    assert "cairn-pytest-" in listing["path"]


def test_real_dry_runs_change_nothing() -> None:
    module = real_module()
    plan = module.updates_start("scan", dry_run=True)
    assert plan["job"] is None
    assert plan["plan"]["kind"] == "scan" and plan["plan"]["requires_admin"] is False
    report = module.updates_wu_set("restart_notify", True, dry_run=True)
    assert report["dry_run"] is True and report["session_id"] is None
    with pytest.raises(ValueError):
        module.updates_wu_set("restart_notify", None, dry_run=True)
    with pytest.raises(ValueError):
        module.updates_start("scan", [EDITOR], dry_run=True)
