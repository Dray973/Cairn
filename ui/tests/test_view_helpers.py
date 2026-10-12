"""Pure helpers of the dashboard views: status-bar text, close-while-busy texts, wrap lengths,
mode counts, warning routing, the actions offered per scanned item and startup entry, the
category, availability and topic constants, tweak search and the Show choices, apply and undo
result counts, History's drift texts and matching, the wording rule of the result texts, and
the fake engine's journal, availability, drift and Keep as is model.

No window is created. Only the signature comparison and the catalog comparisons load the
native module; they call nothing that reads or changes the system (the catalog is static
data) and are skipped where the module is not deployed, so these run in any checkout. The
topic labels are also compared with the engine's catalog source when the checkout has it.
"""

from __future__ import annotations

import ast
import inspect
import json
import re
from pathlib import Path
from typing import Any

import pytest

from optimizer import theme
from optimizer.app import (
    APPLY_AGAIN_TEXT,
    CLOSE_IN_BACKGROUND,
    CLOSE_IRREVERSIBLE,
    CLOSE_JOURNALED,
    CLOSE_READ,
    CLOSE_TEXTS,
    KEEP_BACK_SOME_TEXT,
    KEEP_BACK_TEXT,
    KEEP_LOSES_SOME_TEXT,
    KEEP_LOSES_TEXT,
    NOT_KEPT_TEXT,
    NOT_RESTORED_ONE,
    SETTLED_CHANGED_TEXT,
    SETTLED_KEPT_TEXT,
    STATUS_LIMIT,
    TWEAK_CATEGORIES,
    BusyClose,
    apply_outcomes,
    apply_summary,
    busy_close_text,
    not_restored_text,
    status_line,
)
from optimizer.bridge.engine import CATEGORY_MODES, EngineUnavailable, load_engine_module
from optimizer.widgets.controls import (
    APPX_INVENTORY_WARNING,
    CATEGORY_HEADINGS,
    MIN_WRAP,
    MODE_DESCRIPTIONS,
    NO_MATCH_UNUSABLE_MANY,
    NO_MATCH_UNUSABLE_ONE,
    SCANNING_TEXT,
    SEARCH_PLACEHOLDER,
    SHOW_CHOICES,
    SHOW_DEFAULT,
    SHOW_MENU_WIDTH,
    TOPIC_LABELS,
    UNAVAILABLE_LABELS,
    ScanRow,
    fitted_wrap,
    former_caveat,
    item_changed,
    item_matches,
    item_shown,
    item_unread,
    mode_status,
    never_shown,
    pending_text,
    recorded_recommended,
    route_warnings,
    topic_label,
)
from optimizer.widgets.history import (
    FORMER_DETAIL,
    STARTUP_ON_AGAIN_TEXT,
    STARTUP_REMOVED_TEXT,
    ChangeGroup,
    attach_drift,
    changed_summary,
    drift_lines,
    group_changes,
    holds,
    mismatch_line,
)
from optimizer.widgets.search import search_terms
from optimizer.widgets.startup import can_change

from .bridge_support import assert_signatures_match
from .fake_engine import (
    APPROVED_ROOT,
    CEIP_TASKS,
    RECORDED_SUFFIX,
    TOPICS,
    UNAVAILABLE_KINDS,
    FakeEngine,
    drift_note,
)
from .fake_network import ETHERNET, WIFI

# The engine's catalog source, read for its topic labels (absent outside a checkout).
CATALOG_SOURCE = Path(__file__).resolve().parents[2] / "crates" / "core" / "src" / "debloat" / "catalog.rs"

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
    "drift": "()",
    "verify": "(ids)",
    "forget_targets": "(filter)",
    "cleanup_scan": "()",
    "cleanup_run": "(ids)",
    "startup_list": "()",
    "startup_set_enabled": "(id, enabled, restore_point='skip')",
    "restart_explorer": "()",
    # shell
    "install_info": "()",
    "elevated_as_other_user": "()",
    "journal_path": "()",
    "open_url": "(url)",
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
        preset={"privacy.advertising_id"},
        extra_tweaks=[("privacy.location", "privacy", False)],
    )
    engine.applied.update({"privacy.activity_history", "privacy.location"})
    items = engine.scan()["items"]
    # activity_history: recorded; advertising_id: already in place; location: not recommended.
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
    report = _scan(unreadable={"privacy.advertising_id"}, appx_inventory_error=PS_ERROR)
    report["warnings"].append("power plan: cannot read the active scheme")
    appx_warning = f"{APPX_INVENTORY_WARNING}: {PS_ERROR}"
    item_warning = "privacy.advertising_id: cannot read registry value: access denied"

    per_item, panel = route_warnings(report, ("privacy", "performance", "gaming", "interface"))
    assert per_item == {"privacy.advertising_id": ["cannot read registry value: access denied"]}
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


def test_unavailable_items_offer_only_undo_and_former_values_offer_apply() -> None:
    for kind in UNAVAILABLE_LABELS:
        # The availability rule comes before the state: a recorded item offers Undo whatever it reads.
        assert ScanRow.action_for(_item("applied", unavailable=kind, revertible=True)) == ("Undo", "revert")
        assert ScanRow.action_for(_item("partial", unavailable=kind, revertible=True)) == ("Undo", "revert")
        assert ScanRow.action_for(_item("not_applied", unavailable=kind, revertible=True)) == (
            "Undo",
            "revert",
        )
        assert ScanRow.action_for(_item("unavailable", unavailable=kind, revertible=False)) is None
        assert ScanRow.action_for(_item("applied", unavailable=kind, revertible=False)) is None
    former = [
        {"target": "HKLM\\SOFTWARE\\Test\\Former", "original": "not set", "reason": "Cairn 0.2 set it."}
    ]
    # Applying restores an earlier version's value, even where the tweak's own values are set.
    assert ScanRow.action_for(_item("applied", revertible=True, former=former)) == ("Apply", "apply")
    assert ScanRow.action_for(_item("not_applied", revertible=True, former=former)) == ("Apply", "apply")
    assert ScanRow.action_for(_item("applied", revertible=True, former=[])) == ("Undo", "revert")
    assert UNAVAILABLE_LABELS == {
        "not_on_this_pc": "Not on this PC",
        "windows_version": "Not on this version of Windows",
        "edition": "Not on this edition of Windows",
        "retired": "Retired",
    }


def test_former_caveat_names_the_value_its_reason_and_both_ways_back() -> None:
    record = {
        "target": "HKLM\\SOFTWARE\\Policies\\Test\\Former",
        "original": "not set",
        "reason": "Cairn 0.2 also set a value that this tweak no longer uses, so Cairn no longer sets it.",
    }
    assert former_caveat(record) == (
        "⚠ Set by an earlier version of Cairn: HKLM\\SOFTWARE\\Policies\\Test\\Former. Cairn 0.2 also set a "
        "value that this tweak no longer uses, so Cairn no longer sets it. Apply this tweak to restore it "
        "to not set; Undo in History restores everything."
    )


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
        "Limits Windows and Edge diagnostic data and feedback, turns off the telemetry service "
        "and telemetry scheduled tasks, the advertising ID, suggested apps, lock screen ads and "
        "notifications, and the tips and setup reminders in Start and Settings."
    )
    # Retired, removed or no longer recommended: Activity History, account notices, search
    # highlights, the CEIP policy and the Office policies (business Office only).
    for gone in ("Activity History", "account notices", "search highlights", "Office", "CEIP"):
        assert gone not in MODE_DESCRIPTIONS["privacy"], gone
    # Background recording, the Sticky Keys shortcuts and Edge's default-browser prompts are not
    # recommended: no PC available for checks could show their effect.
    assert MODE_DESCRIPTIONS["gaming"] == "Turns on Game Mode."
    for gone in ("multimedia scheduling", "background", "Sticky Keys"):
        assert gone not in MODE_DESCRIPTIONS["gaming"], gone
    assert MODE_DESCRIPTIONS["interface"] == (
        "Shows file extensions, adds End task to the taskbar menu, hides sync app notices in "
        "File Explorer and stops Edge's new tab page news."
    )
    assert "default-browser" not in MODE_DESCRIPTIONS["interface"]
    assert "Widgets" not in MODE_DESCRIPTIONS["interface"], "Widgets is retired"
    assert "Recall" not in MODE_DESCRIPTIONS["privacy"], "Recall is no longer recommended"
    assert SCANNING_TEXT == "Reading policies, services, scheduled tasks, Store apps and the power plan…"


def test_mode_status_counts_one_recommended_change_in_the_singular() -> None:
    # Gaming Mode has a single recommended change.
    assert mode_status(True, 1, 1) == ("✓ On  ·  1 recommended change applied", theme.GOOD)
    assert mode_status(False, 0, 1) == ("○ Off  ·  1 recommended change available", theme.INK_SECONDARY)
    assert mode_status(True, 16, 16) == ("✓ On  ·  all 16 recommended changes applied", theme.GOOD)
    assert mode_status(False, 2, 7) == ("◐ Partly on  ·  2 of 7 recommended changes applied", theme.WARNING)
    assert mode_status(False, 0, 4) == ("○ Off  ·  4 recommended changes available", theme.INK_SECONDARY)
    assert pending_text(1) == "1 recommended change pending"
    assert pending_text(0) == "0 recommended changes pending"
    assert pending_text(12) == "12 recommended changes pending"


def test_security_is_a_tweak_category_without_a_mode() -> None:
    assert TWEAK_CATEGORIES == ("privacy", "performance", "gaming", "interface", "security")
    assert CATEGORY_HEADINGS["security"] == "Security"
    assert set(TWEAK_CATEGORIES) | {"bloatware"} == set(CATEGORY_HEADINGS)
    assert "security" not in CATEGORY_MODES and "security" not in MODE_DESCRIPTIONS
    assert set(CATEGORY_MODES) == set(MODE_DESCRIPTIONS)
    # Filter segments are the first word of each heading, so every one must differ.
    segments = [CATEGORY_HEADINGS[c].split(" ")[0] for c in TWEAK_CATEGORIES]
    assert len(set(segments)) == len(segments)


def test_unavailable_and_topic_labels_cover_the_engines_kinds() -> None:
    assert set(UNAVAILABLE_LABELS) == set(UNAVAILABLE_KINDS)
    assert UNAVAILABLE_LABELS["windows_version"] == "Not on this version of Windows"
    assert UNAVAILABLE_LABELS["edition"] == "Not on this edition of Windows"
    assert len(TOPIC_LABELS) == 20
    assert len(set(TOPIC_LABELS.values())) == 20, "topic labels are unique"
    assert TOPIC_LABELS["files"] == "Files and storage"
    assert set(TOPICS.values()) <= set(TOPIC_LABELS), "the fake's topics are engine topics"


def test_topic_labels_match_the_engine_catalog() -> None:
    if not CATALOG_SOURCE.is_file():
        pytest.skip(f"no engine source at {CATALOG_SOURCE}")
    source = CATALOG_SOURCE.read_text(encoding="utf-8")
    engine = {
        re.sub(r"(?<!^)(?=[A-Z])", "_", name).lower(): label
        for name, label in re.findall(r'Topic::(\w+) => "([^"]+)"', source)
    }
    assert engine == TOPIC_LABELS


def _real_catalog() -> list[dict[str, Any]]:
    """The deployed engine's catalog (static data; nothing on this PC is read), or a skip."""
    try:
        module = load_engine_module()
    except EngineUnavailable as exc:
        pytest.skip(str(exc))
    return list(module.catalog())


def test_topic_labels_match_the_engine() -> None:
    catalog = _real_catalog()
    topics = {e.get("topic") for e in catalog if e.get("kind") == "tweak"}
    assert None not in topics, "every tweak has a topic"
    assert topics <= set(TOPIC_LABELS), f"topics without a label: {sorted(topics - set(TOPIC_LABELS))}"


def test_fake_catalog_has_the_engines_keys() -> None:
    catalog = _real_catalog()
    real = next(e for e in catalog if e.get("kind") == "tweak")
    fake = next(e for e in FakeEngine().catalog() if e["kind"] == "tweak")
    assert set(fake) == set(real), "the fake's catalog entries carry the engine's keys"


def test_fake_catalog_names_requirements() -> None:
    engine = FakeEngine(
        extra_tweaks=[("privacy.edge_feedback", "privacy", True)], requires={"privacy.edge_feedback": "edge"}
    )
    catalog = {e["id"]: e for e in engine.catalog()}
    assert catalog["privacy.edge_feedback"]["requires"] == "edge"
    assert catalog["gaming.game_mode"]["requires"] is None
    assert catalog["privacy.edge_feedback"]["default_on"] is True
    assert catalog["appx.Microsoft.BingNews"]["kind"] == "appx"
    assert "requires" not in next(i for i in engine.scan()["items"] if i["id"] == "privacy.edge_feedback")
    with pytest.raises(ValueError, match="extra_tweaks"):
        FakeEngine(requires={"privacy.unknown": "edge"})


def _tweak(title: str = "Turn off something", **extra: Any) -> dict[str, Any]:
    return {
        "id": "privacy.edge_feedback",
        "kind": "tweak",
        "title": title,
        "description": "Edge no longer offers Send feedback.",
        "topic": "edge",
        **extra,
    }


def test_fold_drops_case_and_accents() -> None:
    item = _tweak("Télémétrie réduite")
    assert search_terms("TÉLÉMÉTRIE") == ["telemetrie"]
    assert item_matches(item, search_terms("telemetrie"))
    assert item_matches(item, search_terms("RÉDUITE"))
    assert item_matches(item, search_terms("Tele redu"))
    assert not item_matches(item, search_terms("telemetries"))


def test_every_search_word_must_match() -> None:
    item = _tweak("Stop Edge feedback prompts")
    assert item_matches(item, search_terms("edge feedback"))
    assert item_matches(item, search_terms("feed  ed"))
    assert not item_matches(item, search_terms("edge office"))
    assert item_matches(item, []), "an empty search matches everything"
    assert item_matches(item, search_terms("   ")), "a blank search is no search"


def test_search_matches_title_description_id_and_topic() -> None:
    item = _tweak("Hide prompts")
    assert topic_label(item) == "Microsoft Edge"
    assert item_matches(item, search_terms("hide")), "title"
    assert item_matches(item, search_terms("send")), "description"
    assert item_matches(item, search_terms("privacy")), "id part"
    assert item_matches(item, search_terms("edge_feedback")), "id parts are words"
    assert item_matches(item, search_terms("microsoft")), "topic label"
    # Words match at their start only.
    search = {"id": "privacy.web_search", "topic": "search"}
    assert not item_matches(_tweak(description="Not available on Home.", **search), search_terms("ai"))
    assert not item_matches(_tweak(description="Common knowledge.", **search), search_terms("edge"))
    assert item_matches(_tweak(description="Common knowledge.", **search), search_terms("know"))
    assert item_matches(_tweak(topic="ai", id="privacy.web_search"), search_terms("ai")), "AI features"
    assert topic_label({"topic": None}) == "" and topic_label({}) == ""


def _listed(**fields: Any) -> dict[str, Any]:
    return {"state": "not_applied", "revertible": False, "unavailable": None, "drift": [], **fields}


def test_show_choices() -> None:
    assert SHOW_CHOICES == (
        ("Available here", "available"),
        ("Changed since applied", "changed"),
        ("Not available here", "unavailable"),
        ("All tweaks", "all"),
    )
    assert SHOW_DEFAULT == "available"
    assert SEARCH_PLACEHOLDER == "Search tweaks  (Ctrl+F)" and SHOW_MENU_WIDTH == 160
    usable = _listed()
    drifted = _listed(state="partial", revertible=True, drift=[{"label": "X"}])
    too_new = _listed(state="unavailable", unavailable="windows_version")
    recorded_too_new = _listed(state="applied", unavailable="windows_version", revertible=True)
    retired = _listed(state="unavailable", unavailable="retired")
    recorded_retired = _listed(
        state="applied", unavailable="retired", revertible=True, actions=[{"held": True}]
    )
    # A retired tweak's row lists no drift; the scan marks its recorded target as not held.
    retired_changed = _listed(
        state="partial", unavailable="retired", revertible=True, actions=[{"held": False}, {"held": None}]
    )
    shown = {
        value: [
            name
            for name, item in (
                ("usable", usable),
                ("drifted", drifted),
                ("too_new", too_new),
                ("recorded_too_new", recorded_too_new),
                ("retired", retired),
                ("recorded_retired", recorded_retired),
                ("retired_changed", retired_changed),
            )
            if item_shown(item, value)
        ]
        for _, value in SHOW_CHOICES
    }
    assert shown == {
        # Recorded items stay listed, because they offer Undo.
        "available": ["usable", "drifted", "recorded_too_new", "recorded_retired", "retired_changed"],
        "changed": ["drifted", "retired_changed"],
        "unavailable": ["too_new", "recorded_too_new", "recorded_retired", "retired_changed"],
        "all": ["usable", "drifted", "too_new", "recorded_too_new", "recorded_retired", "retired_changed"],
    }
    assert item_changed(drifted) and item_changed(retired_changed)
    assert not item_changed(recorded_retired) and not item_changed(usable)
    assert not item_changed({"state": "applied", "actions": [{"state": "applied"}]}), "engines without `held`"
    # A retired tweak without records is never listed, whatever the choice.
    assert never_shown(retired) and not never_shown(recorded_retired) and not never_shown(too_new)
    assert item_shown({"state": "applied"}, "available"), "engines without the new fields"


def test_unread_items_are_recorded_items_with_a_read_error() -> None:
    error = ["cannot read registry value: access denied"]
    unread = _listed(state="unavailable", revertible=True, actions=[{"state": "unavailable"}])
    assert item_unread(unread, error)
    assert not item_unread(unread, []), "no read warning"
    assert not item_unread({**unread, "revertible": False}, error), "nothing recorded"
    assert not item_unread({**unread, "actions": [{"state": "applied"}]}, error), "every value was read"


def test_the_empty_search_hint_names_the_show_choices() -> None:
    # A search that matches only tweaks this PC cannot use names the choice that lists them and
    # the default choice that hides them, by their menu labels.
    labels = dict(SHOW_CHOICES)
    assert (labels["All tweaks"], labels["Available here"]) == ("all", SHOW_DEFAULT)
    for template in (NO_MATCH_UNUSABLE_ONE, NO_MATCH_UNUSABLE_MANY):
        assert "Choose All tweaks instead of Available here to see" in template


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

    engine.apply([CEIP_ID, "privacy.advertising_id"], restore_point="skip", dry_run=False)
    item = _ceip_item(engine)
    assert item["state"] == "applied" and item["revertible"]
    assert [a["detail"] for a in item["actions"]] == [
        f"scheduled task {p}: disabled; target disabled" for p in CEIP_TASKS
    ]
    summary = engine.journal_summary()
    assert summary["registry_active"] == 1, "the task tweak writes no registry value"
    assert summary["scheduled_tasks_active"] == 2, "one record per task path"
    export = json.loads(engine.journal_export_json())
    assert [r["value_name"] for r in export["registry"]] == ["privacy.advertising_id"]
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
    engine.applied.update({CEIP_ID, "privacy.advertising_id"})

    plan = engine.revert_all(dry_run=True)
    assert plan["actions"] == ["delete value: privacy.advertising_id"] + [
        f"enable scheduled task {p}" for p in CEIP_TASKS
    ], "registry values are restored before scheduled tasks"
    assert plan["scheduled_tasks_restored"] == 0 and plan["registry_deleted"] == 0

    plan = engine.revert_targets({"scheduled_tasks": [CEIP_TASKS[1].upper()]}, dry_run=True)
    assert plan["actions"] == [f"enable scheduled task {p}" for p in CEIP_TASKS]
    assert engine.applied == {CEIP_ID, "privacy.advertising_id"}, "a dry run changes nothing"

    report = engine.revert_targets({"scheduled_tasks": list(CEIP_TASKS)}, dry_run=False)
    assert report["scheduled_tasks_restored"] == 2
    assert report["registry_deleted"] == 0
    assert engine.applied == {"privacy.advertising_id"}
    assert engine.journal_summary()["scheduled_tasks_active"] == 0


def test_fake_scheduled_task_tweaks_must_be_listed_tweaks() -> None:
    with pytest.raises(ValueError, match="extra_tweaks"):
        FakeEngine(scheduled_task_tweaks={CEIP_ID: CEIP_TASKS})


TOO_NEW = (
    "Windows 11 24H2 and later no longer use this setting; this PC runs Windows 11 Home 26H2 "
    "(build 26300.9457)."
)
RETIRED = "Cairn found no effect of this policy on Windows 11."
FORMER = {
    "target": "HKLM\\SOFTWARE\\Policies\\Test\\Former",
    "original": "not set",
    "reason": "Cairn 0.2 set this value; this version no longer does.",
}
FORMER_SELECTION = {"hive": "HKLM", "key_path": "SOFTWARE\\Policies\\Test", "value_name": "Former"}
GAME_MODE_RESET = {
    "target": "HKLM\\Test\\gaming.game_mode",
    "label": "gaming.game_mode",
    "expected": "1",
    "found": "not set",
    "original": "not set",
    "back_to_original": True,
}


def _record(item_id: str) -> dict[str, str]:
    """Filter entry of a sample tweak's registry record."""
    return {"hive": "HKLM", "key_path": "Test", "value_name": item_id}


def _items_by_id(engine: FakeEngine) -> dict[str, dict[str, Any]]:
    return {i["id"]: i for i in engine.scan()["items"]}


def test_fake_unavailable_tweaks_carry_their_kind_and_are_never_applied() -> None:
    engine = FakeEngine(
        extra_tweaks=[("privacy.copilot", "privacy", False), ("privacy.cortana", "privacy", True)],
        unavailable={
            "privacy.copilot": ("windows_version", TOO_NEW),
            "privacy.cortana": ("retired", RETIRED),
        },
        mode_undo={"privacy.copilot"},
    )
    items = _items_by_id(engine)
    copilot, cortana = items["privacy.copilot"], items["privacy.cortana"]
    assert (copilot["state"], copilot["unavailable"], copilot["note"]) == (
        "unavailable",
        "windows_version",
        TOO_NEW,
    )
    assert not copilot["revertible"] and copilot["mode_undo"]
    assert cortana["unavailable"] == "retired" and not cortana["recommended"] and cortana["mode_undo"]
    assert items["privacy.advertising_id"]["topic"] == "ads"
    privacy = next(c for c in engine.scan()["categories"] if c["category"] == "privacy")
    assert privacy["recommended"] == 2, "a retired tweak is never recommended"

    # A record of an earlier version is read and offers Undo; its note says so.
    engine.applied.add("privacy.cortana")
    cortana = _items_by_id(engine)["privacy.cortana"]
    assert (cortana["state"], cortana["revertible"]) == ("applied", True)
    assert cortana["note"] == f"{RETIRED} {RECORDED_SUFFIX}"

    report = engine.apply(["privacy.cortana", "privacy.copilot"], restore_point="skip", dry_run=False)
    assert [(r["outcome"], r["details"]) for r in report["results"]] == [
        ("skipped", [RETIRED]),
        ("skipped", [TOO_NEW]),
    ]
    assert report["session_id"] is None, "refused before anything is recorded"
    plan = engine.apply_category("privacy", dry_run=True)
    assert [r["id"] for r in plan["results"]] == ["privacy.activity_history", "privacy.advertising_id"]
    catalog = {e["id"]: e for e in engine.catalog()}
    assert catalog["privacy.cortana"]["retired"] == RETIRED
    assert catalog["privacy.copilot"]["retired"] is None
    assert engine.scan()["windows"]["edition"] == "home"
    with pytest.raises(ValueError, match="unavailable kinds"):
        FakeEngine(unavailable={"gaming.game_mode": ("too_old", TOO_NEW)})
    with pytest.raises(ValueError, match="extra_tweaks"):
        FakeEngine(undo_limits={"privacy.unknown": "Deleted snapshots are gone."})


def test_fake_former_records_are_restored_by_apply_and_by_undo() -> None:
    limit = "Recordings deleted while this is applied cannot be brought back."
    engine = FakeEngine(former={"privacy.advertising_id": [FORMER]}, undo_limits={"gaming.game_mode": limit})
    items = _items_by_id(engine)
    item = items["privacy.advertising_id"]
    assert (item["state"], item["revertible"], item["former"]) == ("not_applied", True, [FORMER])
    assert items["gaming.game_mode"]["undo_limit"] == limit
    assert engine.journal_summary()["registry_active"] == 1
    assert {e["id"]: e for e in engine.catalog()}["privacy.advertising_id"]["former_targets"] == [
        FORMER["target"]
    ]
    [row] = json.loads(engine.journal_export_json())["registry"]
    assert (row["target"], row["active"], row["forgotten"], row["written"]) == (
        FORMER["target"],
        True,
        False,
        None,
    )

    plan = engine.apply(["privacy.advertising_id"], restore_point="skip", dry_run=True)
    assert plan["results"][0]["details"][0] == (
        f"restore {FORMER['target']} to not set (no longer part of this tweak)"
    )
    assert engine.former_records, "a dry run changes nothing"
    report = engine.apply(["privacy.advertising_id"], restore_point="skip", dry_run=False)
    [result] = report["results"]
    assert result["outcome"] == "applied"
    assert result["details"][0] == f"{FORMER['target']}: restored to not set (no longer part of this tweak)"
    assert not engine.former_records and engine.applied == {"privacy.advertising_id"}

    # Undo of the tweak restores an earlier version's value as well.
    engine = FakeEngine(former={"privacy.advertising_id": [FORMER]})
    undo = engine.revert(["privacy.advertising_id"], dry_run=False)
    assert undo["actions"] == [f"delete value: {FORMER['target']}"]
    assert (undo["registry_deleted"], undo["not_restored"], undo["warnings"]) == (1, [], [])
    assert not engine.former_records


def test_fake_category_counts_an_applied_item_with_former_records_as_not_done() -> None:
    # As the engine's category status: turning the mode on would still restore that value.
    engine = FakeEngine(former={"privacy.advertising_id": [FORMER]})
    engine.applied.update({"privacy.activity_history", "privacy.advertising_id"})

    def privacy() -> dict[str, Any]:
        return next(c for c in engine.scan()["categories"] if c["category"] == "privacy")

    assert (privacy()["recommended"], privacy()["recommended_applied"], privacy()["active"]) == (
        2,
        1,
        False,
    )
    engine.former_records.clear()
    assert (privacy()["recommended_applied"], privacy()["active"]) == (2, True)


def test_fake_not_kept_values_stay_changed_until_applied_again() -> None:
    engine = FakeEngine(not_kept={"gaming.game_mode": [GAME_MODE_RESET]})
    report = engine.apply(["gaming.game_mode"], restore_point="skip", dry_run=False)
    assert report["results"][0]["not_kept"] == [GAME_MODE_RESET]
    item = _items_by_id(engine)["gaming.game_mode"]
    assert (item["state"], item["drift"]) == ("partial", [GAME_MODE_RESET])
    assert item["note"] == (
        "Changed back since Cairn applied it: gaming.game_mode is not set again, its value before "
        "Cairn's change. Apply sets it again."
    )
    assert [a["held"] for a in item["actions"]] == [False]
    assert _items_by_id(engine)["privacy.advertising_id"]["actions"][0]["held"] is None, "no record"

    checked = engine.verify(["gaming.game_mode", "privacy.advertising_id"])
    assert [(e["item"], e["mismatch"]) for e in checked["entries"]] == [("gaming.game_mode", GAME_MODE_RESET)]
    [entry] = engine.drift()["entries"]
    assert entry["kind"] == "registry" and entry["filter"]["registry"] == [_record("gaming.game_mode")]

    engine.not_kept.clear()
    engine.apply(["gaming.game_mode"], restore_point="skip", dry_run=False)
    item = _items_by_id(engine)["gaming.game_mode"]
    assert (item["state"], item["drift"], item["note"]) == ("applied", [], None)
    assert [a["held"] for a in item["actions"]] == [True]
    assert engine.drift()["entries"] == []


def test_fake_undo_reports_values_not_restored_and_drops_drift() -> None:
    changed = {**GAME_MODE_RESET, "expected": "not set", "found": "1", "back_to_original": False}
    engine = FakeEngine(
        drift={"gaming.game_mode": [GAME_MODE_RESET]}, not_restored={"gaming.game_mode": [changed]}
    )
    assert _items_by_id(engine)["gaming.game_mode"]["drift"] == [], "drift needs a record"
    engine.applied.add("gaming.game_mode")
    assert _items_by_id(engine)["gaming.game_mode"]["state"] == "partial"
    assert engine.revert(["gaming.game_mode"], dry_run=True)["not_restored"] == []
    assert engine.revert(["gaming.game_mode"], dry_run=False)["not_restored"] == [changed]
    assert engine.drift()["entries"] == [] and not engine.drifted


def test_fake_drift_counts_the_records_it_cannot_read() -> None:
    engine = FakeEngine(
        unreadable={"performance.sysmain"},
        former={"privacy.advertising_id": [FORMER]},
        permission_recorded={"camera:app:Contoso.Meet_aaaaaaaaaaaaa": "deny"},
    )
    engine.applied.update({"gaming.game_mode", "performance.sysmain"})
    engine._startup_disabled.add("user_run:Discord")
    unread = "HKLM\\Test\\performance.sysmain could not be read (access denied)."
    # Read: Game Mode's value and the startup entry. Not read: SysMain's value, the value an
    # earlier version set and the two values of an earlier build's permission change; the last
    # three carry no written value, as the engine counts them.
    report = engine.drift()
    assert (report["checked"], report["unchecked"], report["warnings"]) == (2, 4, [unread])
    assert report["unchecked_no_value"] == 3
    assert report["entries"] == []
    checked = engine.verify(["gaming.game_mode", "performance.sysmain", "privacy.advertising_id"])
    assert (checked["checked"], checked["unchecked"], checked["warnings"]) == (1, 1, [unread])
    assert checked["entries"] == []


def test_fake_retired_row_lists_no_drift_but_history_does() -> None:
    retired = "Cairn found no effect of this policy on Windows 11."
    engine = FakeEngine(
        unavailable={"gaming.game_mode": ("retired", retired)},
        drift={"gaming.game_mode": [GAME_MODE_RESET]},
    )
    engine.applied.add("gaming.game_mode")
    item = _items_by_id(engine)["gaming.game_mode"]
    assert (item["unavailable"], item["drift"]) == ("retired", [])
    assert item["note"] == f"{retired} {RECORDED_SUFFIX}"
    assert [a["held"] for a in item["actions"]] == [False]
    assert [e["item"] for e in engine.drift()["entries"]] == ["gaming.game_mode"]


def test_fake_drift_note_follows_the_engines_wording() -> None:
    one = {**GAME_MODE_RESET, "found": "0", "back_to_original": False}
    assert drift_note([]) is None
    assert drift_note([one]) == (
        "Changed since Cairn applied it: gaming.game_mode is 0, not 1. Apply sets it again."
    )
    labels = [{**one, "label": name} for name in ("A", "B", "C", "D")]
    assert drift_note(labels[:2]) == (
        "Changed since Cairn applied it: 2 settings no longer hold (A and B). Apply sets them again."
    )
    assert drift_note(labels) == (
        "Changed since Cairn applied it: 4 settings no longer hold (A, B and 2 more). Apply sets them again."
    )


def test_fake_keep_as_is_forgets_only_changed_records() -> None:
    run = f"{APPROVED_ROOT}\\Run"
    discord = {"hive": "HKCU", "key_path": run, "value_name": "Discord"}
    spotify = {"hive": "HKCU", "key_path": run, "value_name": "Spotify"}
    turned_on = {
        "item": None,
        "kind": "registry",
        "mismatch": {
            "target": f"HKCU\\{run}\\Discord",
            "label": "Discord",
            "expected": "off",
            "found": "on",
            "original": "on",
            "back_to_original": True,
        },
        "filter": {"registry": [discord]},
        "recorded_at": "2026-09-25T10:00:00+00:00",
    }
    engine = FakeEngine(
        drift={"gaming.game_mode": [GAME_MODE_RESET]},
        drift_entries=[turned_on],
        former={"privacy.advertising_id": [FORMER]},
    )
    engine.applied.update({"gaming.game_mode", "performance.sysmain"})
    engine._startup_disabled.update({"user_run:Discord", "user_run:Spotify"})
    selection = {
        "registry": [
            _record("gaming.game_mode"),
            _record("performance.sysmain"),
            discord,
            spotify,
            FORMER_SELECTION,
        ],
        "appx_families": ["Microsoft.BingNews_8wekyb3d8bbwe"],
    }

    report = engine.forget_targets(selection)
    assert [m["target"] for m in report["forgotten"]] == [
        f"HKCU\\{run}\\Discord",
        FORMER["target"],
        GAME_MODE_RESET["target"],
    ]
    assert [r["target"] for r in report["refused"]] == [
        "Microsoft.BingNews_8wekyb3d8bbwe",
        f"HKCU\\{run}\\Spotify",
        "HKLM\\Test\\performance.sysmain",
    ]
    assert report["refused"][0]["error"] == "Keep as is applies to settings only; use Undo."
    assert report["refused"][2]["error"] == (
        "HKLM\\Test\\performance.sysmain still holds the value Cairn set; Undo restores the original."
    )
    assert engine.applied == {"performance.sysmain"}
    assert engine._startup_disabled == {"user_run:Spotify"}
    assert not engine.former_records and not engine.drift_entries and not engine.drifted
    assert engine.journal_summary()["registry_active"] == 2

    export = json.loads(engine.journal_export_json())
    forgotten = [r for r in export["registry"] if r["forgotten"]]
    assert [r["target"] for r in forgotten] == [
        f"HKCU\\{run}\\Discord",
        FORMER["target"],
        GAME_MODE_RESET["target"],
    ]
    assert all(not r["active"] and r["reverted_at"] for r in forgotten)
    assert engine.drift()["entries"] == []


def test_fake_keep_as_is_of_dns_needs_a_changed_entry() -> None:
    engine = FakeEngine()
    for adapter in (WIFI, ETHERNET):
        engine.network_set_dns(adapter, "cloudflare", restore_point="skip", dry_run=False)
    active = engine.journal_summary()["dns_active"]
    wifi_records = [r for r in json.loads(engine.journal_export_json())["dns"] if r["interface_guid"] == WIFI]
    assert active > len(wifi_records) > 0
    engine.drift_entries.append(
        {
            "item": None,
            "kind": "dns",
            "mismatch": {
                "target": wifi_records[0]["target"],
                "label": "DNS servers of Wi-Fi (IPv4)",
                "expected": "1.1.1.1, 1.0.0.1",
                "found": "automatic",
                "original": "automatic",
                "back_to_original": True,
            },
            "filter": {"dns": [WIFI]},
            "recorded_at": "2026-09-25T10:00:00+00:00",
        }
    )
    report = engine.forget_targets({"dns": [WIFI.upper().strip("{}"), ETHERNET]})
    assert [m["label"] for m in report["forgotten"]] == ["DNS servers of Wi-Fi (IPv4)"]
    assert report["refused"] and all("still holds" in r["error"] for r in report["refused"])
    assert engine.journal_summary()["dns_active"] == active - len(wifi_records)
    rows = json.loads(engine.journal_export_json())["dns"]
    assert all(r["forgotten"] == (r["interface_guid"] == WIFI) for r in rows)


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


def _result(item_id: str, outcome: str, not_kept: list[dict[str, Any]] | None = None) -> dict[str, Any]:
    return {"id": item_id, "outcome": outcome, "details": [], "not_kept": not_kept or []}


def test_apply_results_count_values_that_were_not_kept_apart() -> None:
    results = [
        _result("a", "applied"),
        _result("b", "applied", [GAME_MODE_RESET]),
        _result("c", "already_applied", [GAME_MODE_RESET]),
        _result("d", "already_applied"),
        _result("e", "skipped"),
        _result("f", "failed"),
        {"id": "g", "outcome": "applied", "details": []},
    ]
    kept, not_kept, skipped, failed = apply_outcomes(results)
    assert [r["id"] for r in kept] == ["a", "g"], "an engine without read-backs counts as before"
    assert [r["id"] for r in not_kept] == ["b", "c"], "a restored earlier value can be not kept too"
    assert [r["id"] for r in skipped] == ["e"] and [r["id"] for r in failed] == ["f"]
    assert apply_summary(2, 2, 1, 1) == "2 applied, 2 not kept, 1 skipped, 1 failed"
    assert apply_summary(0, 1, 0, 0) == "0 applied, 1 not kept"
    assert apply_summary(3, 0, 0, 0) == "3 applied"
    assert NOT_KEPT_TEXT == (
        "Cairn set these values and read them back half a second later, but Windows or another program "
        "had already changed them. Their originals stay recorded, so Undo restores them. Applying them "
        "again may be undone the same way."
    )


def test_undo_results_name_no_cause() -> None:
    assert (
        not_restored_text(1)
        == NOT_RESTORED_ONE
        == (
            "Windows or another program changed 1 restored setting again right after Cairn put back the "
            "original. Cairn does not write it again."
        )
    )
    assert not_restored_text(3) == (
        "Windows or another program changed 3 restored settings again right after Cairn put back the "
        "original. Cairn does not write them again."
    )
    assert SETTLED_CHANGED_TEXT.format(settings="2 settings", titles="Game Mode, Sysmain") == (
        "⚠ Checked again 5 seconds later: 2 settings changed back (Game Mode, Sysmain). See Optimize."
    )
    assert SETTLED_KEPT_TEXT == "✓ Checked again 5 seconds later: every value Cairn just set is still set."


TELEMETRY = "SOFTWARE\\Policies\\Microsoft\\Windows\\DataCollection"


def _registry_record(hive: str, key_path: str, value_name: str, **extra: Any) -> dict[str, Any]:
    return {
        "target": f"{hive}\\{key_path}\\{value_name}",
        "hive": hive,
        "key_path": key_path,
        "value_name": value_name,
        "original": None,
        "active": True,
        "recorded_at": "2026-09-25T10:00:00+00:00",
        **extra,
    }


def _mismatch(label: str, found: str = "3", **extra: Any) -> dict[str, Any]:
    return {
        "target": f"HKLM\\{TELEMETRY}\\{label}",
        "label": label,
        "expected": "0",
        "found": found,
        "original": "not set",
        "back_to_original": False,
        **extra,
    }


def _entry(mismatch: dict[str, Any], item: str | None = None, **selection: Any) -> dict[str, Any]:
    return {
        "item": item,
        "kind": "registry",
        "mismatch": mismatch,
        "filter": selection,
        "recorded_at": "2026-09-25T10:00:00+00:00",
    }


def test_attach_drift_matches_the_records_of_each_change() -> None:
    run = f"{APPROVED_ROOT}\\Run"
    wifi = "{AAAAAAAA-0000-0000-0000-000000000001}"
    export = {
        "registry": [
            _registry_record("HKLM", TELEMETRY, "AllowTelemetry"),
            _registry_record("HKLM", TELEMETRY, "MaxTelemetryAllowed"),
            _registry_record("HKCU", run, "Discord"),
        ],
        "services": [
            {
                "name": "SysMain",
                "start_type": "Automatic",
                "was_running": True,
                "active": True,
                "recorded_at": "2026-09-25T09:00:00+00:00",
            }
        ],
        "scheduled_tasks": [],
        "appx": [],
        "power": [
            {"previous_scheme": "Balanced", "active": True, "recorded_at": "2026-09-25T08:00:00+00:00"}
        ],
        "dns": [
            {
                "interface_guid": wifi.lower(),
                "family": "ipv4",
                "adapter_name": "Wi-Fi",
                "previous_servers": [],
                "active": True,
                "recorded_at": "2026-09-25T07:00:00+00:00",
            }
        ],
    }
    titles = {f"HKLM\\{TELEMETRY}\\AllowTelemetry": "Limit diagnostic data", "service SysMain": "SysMain"}
    groups = group_changes(export, titles)
    telemetry = {"hive": "hklm", "key_path": TELEMETRY.upper(), "value_name": "allowtelemetry"}
    report = {
        "entries": [
            # Registry values compare ignoring case.
            _entry(_mismatch("AllowTelemetry"), "privacy.diagnostic_data", registry=[telemetry]),
            _entry(
                _mismatch("Discord", "on", expected="off", original="on", back_to_original=True),
                registry=[{"hive": "HKCU", "key_path": run, "value_name": "Discord"}],
            ),
            _entry(_mismatch("service SysMain", "Automatic"), "performance.sysmain", services=["sysmain"]),
            _entry(_mismatch("the power plan", "Balanced"), "performance.power_plan", power=True),
            # A GUID matches without braces and in any case.
            _entry(_mismatch("DNS servers of Wi-Fi (IPv4)", "automatic"), dns=[wifi.strip("{}")]),
            # A record undone between the two reads matches no change.
            _entry(
                _mismatch("Gone"),
                "privacy.gone",
                registry=[{"hive": "HKLM", "key_path": "X", "value_name": "Y"}],
            ),
        ]
    }
    assert attach_drift(groups, report) == 5
    by_title = {g.title: g for g in groups}
    limit = by_title["Limit diagnostic data"]
    assert [m["label"] for m in limit.drift] == ["AllowTelemetry"]
    assert limit.item_ids == ["privacy.diagnostic_data"]
    assert limit.drift_filter["registry"] == [telemetry], "Keep as is selects only the record that changed"
    assert (
        len(limit.filter["registry"]) == 1
        and by_title["HKLM\\" + TELEMETRY + "\\MaxTelemetryAllowed"].drift == []
    )
    startup = by_title["Startup app: Discord (HKCU Run)"]
    assert startup.item_ids == [] and len(startup.drift) == 1
    assert by_title["SysMain"].item_ids == ["performance.sysmain"]
    assert by_title["Power plan"].drift_filter["power"] is True
    assert by_title["DNS servers: Wi-Fi"].drift_filter["dns"] == [wifi.strip("{}")]
    assert attach_drift(groups, None) == 0 and attach_drift(groups, {}) == 0


def test_drift_lines_and_summaries() -> None:
    assert holds(1) == "change no longer holds" and holds(2) == "changes no longer hold"
    assert changed_summary(1) == (
        "⚠ 1 change no longer holds: Windows or another program changed it after Cairn did. Cairn never "
        "sets it again on its own."
    )
    assert changed_summary(3) == (
        "⚠ 3 changes no longer hold: Windows or another program changed them after Cairn did. Cairn never "
        "sets them again on its own."
    )
    assert mismatch_line(_mismatch("AllowTelemetry")) == (
        "⚠ AllowTelemetry is 3  ·  Cairn set 0  ·  original not set"
    )
    back = _mismatch("AllowTelemetry", "not set", back_to_original=True)
    assert mismatch_line(back) == (
        "⚠ AllowTelemetry is not set  ·  Cairn set 0  ·  original not set  ·  back to the original"
    )
    assert mismatch_line(_mismatch("X", original=None)) == "⚠ X is 3  ·  Cairn set 0"

    startup = ChangeGroup("Startup app: Discord (HKCU Run)", "startup")
    startup.drift = [
        _mismatch("Discord", "on", expected="off"),
        _mismatch("Discord", "not set", expected="off"),
    ]
    # The line follows the state the entry has now, not its bytes.
    assert drift_lines(startup) == [STARTUP_ON_AGAIN_TEXT, STARTUP_REMOVED_TEXT]
    assert STARTUP_ON_AGAIN_TEXT == "⚠ Turned on again outside Cairn, in Settings or Task Manager."
    assert STARTUP_REMOVED_TEXT == "⚠ Removed outside Cairn."
    setting = ChangeGroup("Limit diagnostic data", "setting")
    setting.drift = [_mismatch("AllowTelemetry", "on")]
    assert drift_lines(setting) == ["⚠ AllowTelemetry is on  ·  Cairn set 0  ·  original not set"]


def test_values_of_earlier_versions_join_their_tweak_and_can_be_kept() -> None:
    former = "HKLM\\SOFTWARE\\Policies\\Test\\Former"
    export = {
        "registry": [
            _registry_record("HKLM", TELEMETRY, "AllowTelemetry"),
            _registry_record(
                "HKLM", "SOFTWARE\\Policies\\Test", "Former", original={"type": "Dword", "value": 1}
            ),
        ]
    }
    titles = {f"HKLM\\{TELEMETRY}\\AllowTelemetry": "Limit diagnostic data", former: "Limit diagnostic data"}
    [group] = group_changes(export, titles, former_targets={former.upper()})
    assert group.details == [
        f"HKLM\\{TELEMETRY}\\AllowTelemetry  ·  was not set",
        f"{former}  ·  was 1  ·  {FORMER_DETAIL}",
    ]
    assert group.former == [(former, "1")]
    assert group.former_filter["registry"] == [
        {"hive": "HKLM", "key_path": "SOFTWARE\\Policies\\Test", "value_name": "Former"}
    ]
    assert len(group.filter["registry"]) == 2, "Undo restores both"
    [plain] = group_changes(export | {"registry": export["registry"][:1]}, titles)
    assert plain.former == [] and plain.former_filter["registry"] == []


# The UI modules whose texts follow the wording rule of read-backs.
WORDING_MODULES = ("app.py", "widgets/controls.py", "widgets/history.py", "widgets/profiles.py")
UI_PACKAGE = Path(__file__).resolve().parents[1] / "optimizer"
# A read-back shows that a value is still set, never that it takes effect or who changed it.
FORBIDDEN_WORDING = (
    r"\benforced\b",
    r"\bin effect\b",
    r"\bworks\b",
    r"\bblocked by windows\b",
    r"\bwindows did not keep\b",
    r"\bby windows\b",
    # "Windows may be managing this" names Windows alone as the cause of a change; the texts say
    # "Windows or another program" instead.
    r"\bwindows may be managing\b",
)


def _string_constants(path: Path) -> list[str]:
    """The string literals of a module, docstrings left out."""
    tree = ast.parse(path.read_text(encoding="utf-8"))
    docstrings: set[int] = set()
    for node in ast.walk(tree):
        if isinstance(node, (ast.Module, ast.ClassDef, ast.FunctionDef, ast.AsyncFunctionDef)) and node.body:
            first = node.body[0]
            if isinstance(first, ast.Expr) and isinstance(first.value, ast.Constant):
                docstrings.add(id(first.value))
    return [
        node.value
        for node in ast.walk(tree)
        if isinstance(node, ast.Constant) and isinstance(node.value, str) and id(node) not in docstrings
    ]


def test_texts_never_claim_an_effect() -> None:
    found = []
    allowed = 0
    for name in WORDING_MODULES:
        for text in _string_constants(UI_PACKAGE / name):
            lowered = text.lower()
            found += [f"{name}: {text!r}" for pattern in FORBIDDEN_WORDING if re.search(pattern, lowered)]
            allowed += "windows or another program" in lowered
    assert found == []
    assert allowed >= 3, "the results name no cause but 'Windows or another program'"
    for text in (
        NOT_KEPT_TEXT,
        APPLY_AGAIN_TEXT,
        KEEP_BACK_TEXT,
        KEEP_BACK_SOME_TEXT,
        KEEP_LOSES_TEXT,
        KEEP_LOSES_SOME_TEXT,
    ):
        assert not any(re.search(pattern, text.lower()) for pattern in FORBIDDEN_WORDING), text
