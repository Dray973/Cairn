"""Pure helpers of the dashboard views: status-bar text, close-while-busy texts, wrap lengths,
mode counts, warning routing, the actions offered per scanned item and startup entry, and the
fake engine's journal model.

No window is created. Only the signature comparison loads the native module; it calls
nothing and is skipped where the module is not deployed, so these run in any checkout.
"""

from __future__ import annotations

import inspect
import json
from typing import Any

import pytest

from optimizer.app import (
    CLOSE_IN_BACKGROUND,
    CLOSE_IRREVERSIBLE,
    CLOSE_JOURNALED,
    CLOSE_READ,
    CLOSE_TEXTS,
    STATUS_LIMIT,
    BusyClose,
    busy_close_text,
    status_line,
)
from optimizer.bridge.engine import EngineUnavailable, load_engine_module
from optimizer.widgets.controls import (
    APPX_INVENTORY_WARNING,
    MIN_WRAP,
    MODE_DESCRIPTIONS,
    SCANNING_TEXT,
    ScanRow,
    fitted_wrap,
    recorded_recommended,
    route_warnings,
)
from optimizer.widgets.history import group_changes
from optimizer.widgets.startup import can_change

from .bridge_support import assert_signatures_match
from .fake_engine import CEIP_TASKS, FakeEngine

# Engine functions the FakeEngine class itself mirrors (the feature mixins have their own
# lists), with the parameters the engine module declares for them.
ENGINE_SIGNATURES = {
    "version": "()",
    "is_elevated": "()",
    "scan": "()",
    "catalog": "()",
    "journal_summary": "()",
    "journal_export_json": "()",
    "apply": "(ids, restore_point='try', dry_run=False)",
    "apply_category": "(category, restore_point='try', dry_run=False)",
    "revert": "(ids, dry_run=False)",
    "revert_category": "(category, dry_run=False)",
    "revert_all": "(dry_run=False)",
    "revert_targets": "(filter, dry_run=False)",
    "cleanup_scan": "()",
    "cleanup_run": "(ids)",
    "startup_list": "()",
    "startup_set_enabled": "(id, enabled, restore_point='skip')",
    "restart_explorer": "()",
    # shell
    "install_info": "()",
    "elevated_as_other_user": "()",
    "journal_path": "()",
    # permissions
    "permissions_list": "()",
    "permissions_set": "(id, allow, restore_point='skip', dry_run=False)",
    # health
    "health_security_checkup": "()",
    "health_update_scan_start": "(online=False)",
    "health_update_scan": "()",
    "health_update_scan_cancel": "()",
    "health_boot_history": "(limit=60)",
    # storage
    "storage_volumes": "()",
    "storage_speed_start": "(volume, size_bytes=1073741824, runs=3, dry_run=False)",
    "storage_speed_history": "()",
    "storage_remove_leftover": "(path)",
    "storage_scan_start": "(path, dry_run=False)",
    "storage_duplicates_start": "(scan_job, min_size=1048576, dry_run=False)",
    "storage_job": "(job_id)",
    "storage_jobs": "()",
    "storage_cancel": "(job_id)",
    "storage_result": "(job_id)",
    "storage_scan_children": "(job_id, node, order='allocated', limit=500)",
    "storage_shutdown": "()",
    # updates
    "updates_winget_status": "()",
    "updates_start": "(kind, items=None, dry_run=False)",
    "updates_job": "(job_id, after=0)",
    "updates_jobs": "()",
    "updates_result": "(job_id, since=0)",
    "updates_cancel": "(job_id)",
    "updates_shutdown": "()",
    "updates_open_log": "(job_id)",
    "updates_wu_state": "()",
    "updates_wu_catalog": "()",
    "updates_wu_set": "(setting, value=None, restore_point='skip', dry_run=False)",
    "updates_app_list": "()",
    "updates_save_app_list": "(apps=None)",
    # maintenance
    "maintenance_status": "()",
    "maintenance_set_schedule": "(config, dry_run=False)",
    "maintenance_remove_unrecorded": "(dry_run=False)",
    "maintenance_run_now": "()",
    "maintenance_acknowledge": "(run_id)",
    "maintenance_open_log": "(run_id)",
    "maintenance_watch": "(expect_start=False)",
    "maintenance_progress": "()",
    # profiles
    "profile_starters": "()",
    "profile_check": "(text)",
    "profile_read": "(path)",
    "profile_candidates": "()",
    "profile_export": "(path, name, description='', keys=None)",
    "profile_apply": "(text, keys=None, restore_point='try', dry_run=False)",
}

PS_ERROR = (
    "Get-AppxPackage : Access is denied.\n"
    "At line:1 char:1\n"
    "+ CategoryInfo          : NotSpecified: (:) [Get-AppxPackage], UnauthorizedAccessException\n"
    "+ FullyQualifiedErrorId : System.UnauthorizedAccessException"
)


def test_status_line_keeps_the_first_line() -> None:
    assert status_line("Scan complete.") == "Scan complete."
    assert status_line(f"Scan complete with 1 warning: {PS_ERROR}") == (
        "Scan complete with 1 warning: Get-AppxPackage : Access is denied. …"
    )
    assert status_line("\n  \n  first\nsecond") == "first …"
    assert status_line("") == ""
    assert status_line(" \n ") == ""


def test_status_line_cuts_long_text() -> None:
    text = "x" * (STATUS_LIMIT + 50)
    line = status_line(text)
    assert len(line) == STATUS_LIMIT
    assert line.endswith("…")
    assert status_line("abcdef", limit=4) == "abc…"
    assert status_line("abcd", limit=4) == "abcd"


def test_busy_close_texts_say_what_closing_does() -> None:
    read = busy_close_text(BusyClose(CLOSE_READ))
    assert read == CLOSE_TEXTS[CLOSE_READ]
    assert "closing now is safe" in read
    assert "background" not in read

    # Every change finishes in the background; only journaled ones promise an undo.
    journaled = busy_close_text(BusyClose(CLOSE_JOURNALED))
    assert journaled.startswith(CLOSE_IN_BACKGROUND)
    assert "History lists" in journaled and "undo" in journaled
    reset = busy_close_text(
        BusyClose(CLOSE_IRREVERSIBLE, "The network stack reset", "Restart Windows once it has finished.")
    )
    assert reset == (
        f"The network stack reset is still running. {CLOSE_IN_BACKGROUND} It can't be undone. "
        "Restart Windows once it has finished."
    )
    assert "recorded" not in reset
    assert busy_close_text(BusyClose()) == CLOSE_IN_BACKGROUND, "no promise either way"
    assert busy_close_text(BusyClose("unknown")) == CLOSE_IN_BACKGROUND


def test_wrap_length_follows_the_available_width() -> None:
    assert fitted_wrap(800, 560) == 560, "a wide column keeps the usual line length"
    assert fitted_wrap(560, 560) == 560
    assert fitted_wrap(469.6, 560) == 460, "rounded down so the text never reaches the edge"
    assert fitted_wrap(40, 560) == MIN_WRAP
    # The text column is the list minus the icon and action columns and the row padding.
    assert ScanRow.TEXT_INSET > ScanRow.ICON_WIDTH + ScanRow.ACTION_WIDTH


def _scan(**options: Any) -> dict[str, Any]:
    return FakeEngine(**options).scan()


def test_mode_counts_only_recorded_recommended_changes() -> None:
    engine = FakeEngine(
        preset={"privacy.cortana"},
        extra_tweaks=[("privacy.location", "privacy", False)],
    )
    engine.applied.update({"privacy.activity_history", "privacy.location"})
    items = engine.scan()["items"]
    # activity_history: recorded; cortana: already in place; location: not recommended.
    assert recorded_recommended(items, "privacy") == 1
    assert recorded_recommended(items, "gaming") == 0

    engine.applied.clear()
    assert recorded_recommended(engine.scan()["items"], "privacy") == 0, "a preset value is not recorded"


def test_mode_count_without_the_revertible_field_counts_applied_items() -> None:
    items = [
        {"category": "gaming", "recommended": True, "state": "applied"},
        {"category": "gaming", "recommended": True, "state": "not_applied"},
        {"category": "gaming", "recommended": True, "state": "applied", "revertible": False},
    ]
    assert recorded_recommended(items, "gaming") == 1


def test_scan_warnings_are_routed_to_items_and_panels() -> None:
    report = _scan(unreadable={"privacy.cortana"}, appx_inventory_error=PS_ERROR)
    report["warnings"].append("power plan: cannot read the active scheme")
    appx_warning = f"{APPX_INVENTORY_WARNING}: {PS_ERROR}"
    item_warning = "privacy.cortana: cannot read registry value: access denied"

    per_item, panel = route_warnings(report, ("privacy", "performance", "gaming", "interface"))
    assert per_item == {"privacy.cortana": ["cannot read registry value: access denied"]}
    assert panel == [item_warning, "power plan: cannot read the active scheme"]

    per_item, panel = route_warnings(report, ("bloatware",))
    assert per_item == {}
    assert panel == [appx_warning, "power plan: cannot read the active scheme"]


def _item(state: str, *, kind: str = "tweak", **extra: Any) -> dict[str, Any]:
    return {"kind": kind, "state": state, **extra}


def test_item_actions_follow_state_and_journal() -> None:
    assert ScanRow.action_for(_item("not_applied")) == ("Apply", "apply")
    assert ScanRow.action_for(_item("partial")) == ("Apply", "apply")
    assert ScanRow.action_for(_item("not_applied", kind="appx")) == ("Remove", "apply")
    assert ScanRow.action_for(_item("applied", revertible=True)) == ("Undo", "revert")
    assert ScanRow.action_for(_item("applied", kind="appx", revertible=True)) == ("Restore", "revert")
    assert ScanRow.action_for(_item("applied")) == ("Undo", "revert"), "engines without the field"
    assert ScanRow.action_for(_item("applied", revertible=False)) is None
    assert ScanRow.action_for(_item("unavailable")) is None


def test_startup_switch_needs_rights_only_for_machine_wide_entries() -> None:
    per_user = {"can_toggle": True, "requires_admin": False}
    machine = {"can_toggle": True, "requires_admin": True}
    fixed = {"can_toggle": False, "requires_admin": False, "note": "Set by Group Policy."}
    assert can_change(per_user, engine_ready=True, elevated=False)
    assert not can_change(machine, engine_ready=True, elevated=False)
    assert can_change(machine, engine_ready=True, elevated=True)
    assert not can_change(fixed, engine_ready=True, elevated=True)
    assert not can_change(per_user, engine_ready=False, elevated=True)


def test_privacy_mode_and_scan_texts_name_scheduled_tasks() -> None:
    assert MODE_DESCRIPTIONS["privacy"] == (
        "Limits Windows, Office and Edge diagnostic data and turns off the telemetry service and "
        "telemetry scheduled tasks, Activity History, Recall, the advertising ID, suggested apps and "
        "lock screen ads."
    )
    assert SCANNING_TEXT == "Reading policies, services, scheduled tasks, Store apps and the power plan…"


CEIP_ID = "privacy.ceip_tasks"


def _task_engine() -> FakeEngine:
    return FakeEngine(extra_tweaks=[(CEIP_ID, "privacy", True)], scheduled_task_tweaks={CEIP_ID: CEIP_TASKS})


def _ceip_item(engine: FakeEngine) -> dict[str, Any]:
    return next(i for i in engine.scan()["items"] if i["id"] == CEIP_ID)


def test_fake_scheduled_task_tweak_is_journaled_per_task_path() -> None:
    engine = _task_engine()
    item = _ceip_item(engine)
    assert [a["detail"] for a in item["actions"]] == [
        f"scheduled task {p}: enabled; target disabled" for p in CEIP_TASKS
    ]
    catalog = {e["id"]: e for e in engine.catalog()}
    assert catalog[CEIP_ID]["targets"] == [f"scheduled task {p}" for p in CEIP_TASKS]

    engine.apply([CEIP_ID, "privacy.cortana"], restore_point="skip", dry_run=False)
    item = _ceip_item(engine)
    assert item["state"] == "applied" and item["revertible"]
    assert [a["detail"] for a in item["actions"]] == [
        f"scheduled task {p}: disabled; target disabled" for p in CEIP_TASKS
    ]
    summary = engine.journal_summary()
    assert summary["registry_active"] == 1, "the task tweak writes no registry value"
    assert summary["scheduled_tasks_active"] == 2, "one record per task path"
    export = json.loads(engine.journal_export_json())
    assert [r["value_name"] for r in export["registry"]] == ["privacy.cortana"]
    assert [(r["path"], r["target"], r["was_enabled"]) for r in export["scheduled_tasks"]] == [
        (p, f"scheduled task {p}", True) for p in CEIP_TASKS
    ]

    # The export and the catalog group the records under the tweak's title.
    titles = {t: e["title"] for e in engine.catalog() for t in e["targets"]}
    [ceip] = [g for g in group_changes(export, titles) if g.kind == "scheduled_task"]
    assert ceip.title == "Ceip Tasks"
    assert ceip.filter["scheduled_tasks"] == list(CEIP_TASKS)
    assert ceip.needs_admin


def test_fake_revert_maps_task_paths_back_to_their_tweak_ignoring_case() -> None:
    engine = _task_engine()
    engine.applied.update({CEIP_ID, "privacy.cortana"})

    plan = engine.revert_all(dry_run=True)
    assert plan["actions"] == ["delete value: privacy.cortana"] + [
        f"enable scheduled task {p}" for p in CEIP_TASKS
    ], "registry values are restored before scheduled tasks"
    assert plan["scheduled_tasks_restored"] == 0 and plan["registry_deleted"] == 0

    plan = engine.revert_targets({"scheduled_tasks": [CEIP_TASKS[1].upper()]}, dry_run=True)
    assert plan["actions"] == [f"enable scheduled task {p}" for p in CEIP_TASKS]
    assert engine.applied == {CEIP_ID, "privacy.cortana"}, "a dry run changes nothing"

    report = engine.revert_targets({"scheduled_tasks": list(CEIP_TASKS)}, dry_run=False)
    assert report["scheduled_tasks_restored"] == 2
    assert report["registry_deleted"] == 0
    assert engine.applied == {"privacy.cortana"}
    assert engine.journal_summary()["scheduled_tasks_active"] == 0


def test_fake_scheduled_task_tweaks_must_be_listed_tweaks() -> None:
    with pytest.raises(ValueError, match="extra_tweaks"):
        FakeEngine(scheduled_task_tweaks={CEIP_ID: CEIP_TASKS})


def _parameters_text(fn: Any) -> str:
    """The parameters of `fn` as `(name, name=default)`, without annotations."""
    parts = [
        p.name if p.default is inspect.Parameter.empty else f"{p.name}={p.default!r}"
        for p in inspect.signature(fn).parameters.values()
    ]
    return f"({', '.join(parts)})"


def test_fake_engine_takes_the_engine_modules_parameters() -> None:
    engine = FakeEngine()
    for name, expected in ENGINE_SIGNATURES.items():
        assert _parameters_text(getattr(engine, name)) == expected, name


def test_fake_engine_signatures_match_the_real_module() -> None:
    try:
        module = load_engine_module()
    except EngineUnavailable as exc:
        pytest.skip(str(exc))
    # Functions an older build lacks are skipped; the next deploy brings them.
    for name, expected in ENGINE_SIGNATURES.items():
        real = getattr(module, name, None)
        if real is not None:
            assert _parameters_text(real) == expected, name
    assert_signatures_match(FakeEngine(), module, ENGINE_SIGNATURES)
