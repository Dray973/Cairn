"""EngineBridge app-permission calls: the guide is read on the worker, every change and plan is
refused with the engine's one message, and the FakeEngine mirrors the engine's guide, refusal and
signatures, and the permission records earlier builds left in the journal.

Of the real module only `permissions_list` (a read of Windows' usage records), `journal_summary`
(of the test data folder's journal) and dry runs of `permissions_set` are called, and the dry runs
only on an engine that lists the guide, which refuses them before anything is read.
"""

from __future__ import annotations

import inspect
import threading
from typing import Any

import pytest

from optimizer.bridge.engine import EngineBridge, EngineUnavailable, load_engine_module

from .bridge_support import assert_signatures_match, wait
from .fake_engine import FakeEngine
from .fake_permissions import (
    CAPABILITIES,
    CHANGE_REFUSED,
    DEVICE_STORE,
    LOCATION_SENSOR,
    SETTINGS_PAGES,
    USER_STORE,
    parse_permission_id,
    sensor_target,
)

PERMISSION_NAMES = ("permissions_list", "permissions_set")
MEET_FAMILY = "Contoso.Meet_aaaaaaaaaaaaa"
MEET_CAMERA = f"camera:app:{MEET_FAMILY}"
# Requests of every kind: ids earlier builds took, ids they rejected, and nonsense.
REQUESTS = (
    "camera:device",
    "microphone:apps",
    "location:desktop_apps",
    "location:device",
    MEET_CAMERA,
    "camera",
    "camera:app:",
    "camera:app:x\\y",
    "video:apps",
    "",
)


def recorder(engine: FakeEngine, name: str) -> list[tuple[tuple[Any, ...], dict[str, Any], bool]]:
    """Replaces the fake's `name` with a wrapper that records (args, kwargs, on a worker thread)."""
    calls: list[tuple[tuple[Any, ...], dict[str, Any], bool]] = []
    real = getattr(engine, name)

    def record(*args: Any, **kwargs: Any) -> Any:
        calls.append((args, kwargs, threading.current_thread() is not threading.main_thread()))
        return real(*args, **kwargs)

    setattr(engine, name, record)
    return calls


def test_the_guide_is_read_on_the_worker_thread() -> None:
    engine = FakeEngine(elevated=False)
    listed = recorder(engine, "permissions_list")
    bridge = EngineBridge(module=engine)  # type: ignore[arg-type]
    try:
        report = bridge.permissions_list()
        wait(bridge, [report])
        assert [on_worker for _, _, on_worker in listed] == [True]
        guide = report.result()
        assert [c["capability"] for c in guide["capabilities"]] == list(CAPABILITIES)
        assert [c["settings_uri"] for c in guide["capabilities"]] == list(SETTINGS_PAGES.values())
    finally:
        bridge.shutdown()


def test_changes_and_plans_are_refused_through_the_future() -> None:
    engine = FakeEngine(elevated=True)
    calls = recorder(engine, "permissions_set")
    bridge = EngineBridge(module=engine)  # type: ignore[arg-type]
    try:
        plan = bridge.plan_permission(MEET_CAMERA, False)
        change = bridge.set_permission("location:device", True)
        wait(bridge, [plan, change])
        # The policies are still passed by keyword, on the worker.
        assert [(args, kwargs, on_worker) for args, kwargs, on_worker in calls] == [
            ((MEET_CAMERA, False), {"restore_point": "skip", "dry_run": True}, True),
            (("location:device", True), {"restore_point": "skip", "dry_run": False}, True),
        ]
        for future in (plan, change):
            assert isinstance(future.exception(), RuntimeError)
            assert str(future.exception()) == CHANGE_REFUSED
        assert engine.journal_summary()["registry_active"] == 0, "a refusal records nothing"
    finally:
        bridge.shutdown()


def test_engine_errors_reach_the_future() -> None:
    bridge = EngineBridge(module=FakeEngine(permission_error="access denied"))  # type: ignore[arg-type]
    try:
        future = bridge.permissions_list()
        wait(bridge, [future])
        assert isinstance(future.exception(), RuntimeError)
        assert str(future.exception()) == "access denied"
    finally:
        bridge.shutdown()


def test_supports_reports_an_outdated_engine() -> None:
    bridge = EngineBridge(module=FakeEngine(unsupported=["permissions_list"]))  # type: ignore[arg-type]
    try:
        assert not bridge.supports("permissions_list")
        assert bridge.supports("permissions_set")
    finally:
        bridge.shutdown()


def parameters(fn: Any) -> str:
    """The parameter list as the engine module declares it, without annotations."""
    signature = inspect.signature(fn)
    plain = [p.replace(annotation=inspect.Parameter.empty) for p in signature.parameters.values()]
    return str(signature.replace(parameters=plain, return_annotation=inspect.Signature.empty))


def test_fake_signatures_are_the_engines() -> None:
    fake = FakeEngine()
    assert parameters(fake.permissions_list) == "()"
    assert parameters(fake.permissions_set) == "(id, allow, restore_point='skip', dry_run=False)"


def test_fake_refuses_every_request_with_the_engines_message() -> None:
    for elevated in (True, False):
        engine = FakeEngine(elevated=elevated, other_user=not elevated)
        for request in REQUESTS:
            for allow in (True, False):
                for dry_run in (True, False):
                    with pytest.raises(RuntimeError) as refused:
                        engine.permissions_set(request, allow, dry_run=dry_run)
                    assert str(refused.value) == CHANGE_REFUSED
        with pytest.raises(RuntimeError, match="does not change app permissions"):
            engine.permissions_set("camera:apps", True, restore_point="sometimes")
        assert len(engine.calls_named("permissions_set")) == len(REQUESTS) * 4 + 1
        assert engine._permission_active_count() == 0, "a refusal records nothing"


def test_fake_guide_lists_each_capabilitys_page_and_recent_desktop_use() -> None:
    engine = FakeEngine(
        permission_recent=(
            ("camera", "C:\\Tools\\old.exe", "2026-09-01T08:00:00Z", False),
            ("camera", "C:\\Tools\\new.exe", "2026-09-20T08:00:00Z", True),
        ),
        permission_warnings=["Location: the usage of 1 desktop app(s) could not be read"],
    )
    guide = engine.permissions_list()
    assert guide["warnings"] == ["Location: the usage of 1 desktop app(s) could not be read"]
    camera, microphone, location = guide["capabilities"]
    assert (camera["label"], camera["settings_uri"]) == ("Camera", "ms-settings:privacy-webcam")
    assert [r["path"] for r in camera["recent_desktop_apps"]] == ["C:\\Tools\\new.exe", "C:\\Tools\\old.exe"]
    assert camera["recent_desktop_apps"][0]["in_use"]
    assert microphone["recent_desktop_apps"] == [] == location["recent_desktop_apps"]
    assert engine.calls_named("permissions_list") == [()]


def test_fake_journal_holds_the_records_of_earlier_builds_until_they_are_undone() -> None:
    engine = FakeEngine(
        elevated=True,
        permission_recorded={MEET_CAMERA: "prompt", "location:device": "allow"},
    )
    assert engine.journal_summary()["registry_active"] == 5
    rows = engine._permission_export()
    meet = f"{USER_STORE}\\webcam\\{MEET_FAMILY}"
    location = f"{DEVICE_STORE}\\location"
    assert [(r["hive"], r["key_path"], r["value_name"]) for r in rows] == [
        ("HKCU", meet, "Value"),
        ("HKCU", meet, "LastSetTime"),
        ("HKLM", location, "Value"),
        ("HKLM", location, "LastSetTime"),
        ("HKLM", LOCATION_SENSOR, "SensorPermissionState"),
    ]
    assert rows[0]["original"] == {"type": "Sz", "value": "Prompt"}
    assert rows[-1]["original"] == {"type": "Dword", "value": 1}

    # Undo of one entry (History's group), then Revert All for the rest.
    targets = [{"hive": r["hive"], "key_path": r["key_path"], "value_name": r["value_name"]} for r in rows]
    other = {"hive": "HKLM", "key_path": "Test", "value_name": "privacy.cortana"}
    actions, restored, remaining = engine._permission_revert([*targets[2:], other], dry_run=True)
    assert (restored, remaining) == (0, [other])
    assert actions[-1] == f"restore 0x00000001 (1): {sensor_target()}"
    assert engine._permission_active_count() == 5, "a dry run restores nothing"
    report = engine.revert_targets({"registry": targets[2:]}, dry_run=False)
    assert report["registry_restored"] == 3
    assert engine._permission_active_count() == 2
    report = engine.revert_all(dry_run=False)
    assert f'restore "Prompt": HKCU\\{meet}\\Value' in report["actions"]
    assert engine.journal_summary()["registry_active"] == 0
    assert parse_permission_id("WEBCAM:Desktop_Apps") == ("camera", "desktop_apps", None)
    with pytest.raises(ValueError):
        FakeEngine(permission_recorded={"camera:everyone": "allow"})


def deployed_guide_engine() -> Any:
    """The deployed engine module when it lists the permissions guide; skips the test when the
    module is missing or was built before the guide (it lists switches and can change them)."""
    try:
        module = load_engine_module()
    except EngineUnavailable as exc:
        pytest.skip(str(exc))
    if not callable(getattr(module, "permissions_list", None)):
        pytest.skip("the deployed engine module has no permission functions")
    capability = module.permissions_list()["capabilities"][0]
    if "switches" in capability or "settings_uri" not in capability:
        pytest.skip("the deployed engine predates the permissions guide; rebuild and deploy it")
    return module


def test_fake_permissions_signatures_match_real_module() -> None:
    try:
        module = load_engine_module()
    except EngineUnavailable as exc:
        pytest.skip(str(exc))
    if not callable(getattr(module, "permissions_list", None)):
        pytest.skip("the deployed engine module has no permission functions yet")
    assert_signatures_match(FakeEngine(), module, PERMISSION_NAMES)


def test_real_guide_has_the_fakes_shape_and_pages() -> None:
    module = deployed_guide_engine()
    guide = module.permissions_list()
    fake = FakeEngine().permissions_list()
    assert set(guide) == set(fake)
    assert [c["capability"] for c in guide["capabilities"]] == list(CAPABILITIES)
    for capability, mirror in zip(guide["capabilities"], fake["capabilities"], strict=True):
        assert set(capability) == set(mirror)
        assert (capability["label"], capability["settings_uri"]) == (mirror["label"], mirror["settings_uri"])
        assert len(capability["recent_desktop_apps"]) <= 50
        for use in capability["recent_desktop_apps"]:
            assert set(use) == {"path", "last_used", "in_use"}


def test_real_module_refuses_every_request_before_a_session() -> None:
    module = deployed_guide_engine()
    sessions = module.journal_summary()["sessions"]
    for request in REQUESTS:
        for allow in (True, False):
            # Dry runs only: a guide engine refuses them before anything is read.
            with pytest.raises(RuntimeError) as refused:
                module.permissions_set(request, allow, restore_point="skip", dry_run=True)
            assert str(refused.value) == CHANGE_REFUSED
    with pytest.raises(RuntimeError, match="does not change app permissions"):
        module.permissions_set("camera:apps", True, restore_point="sometimes", dry_run=True)
    assert module.journal_summary()["sessions"] == sessions
