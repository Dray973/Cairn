"""History grouping: journal records to tweak titles, Startup entries, the app-permission records
earlier builds left and the scheduled tasks Cairn registered; the change rows' layout; and the
Changed view of the changes that no longer hold, with Apply again, Undo and Keep as is.

The grouping tests exercise pure functions, so they need neither a window nor a native module;
the window tests lay out and use the History section of a window with the FakeEngine, which
writes nothing on this PC.
"""

from __future__ import annotations

import logging
from collections.abc import Iterator
from typing import Any

import pytest

from optimizer import theme
from optimizer.widgets.history import (
    ALL_HOLD_TEXT,
    FORMER_DETAIL,
    PERMISSION_KIND,
    STARTUP_ON_AGAIN_TEXT,
    STORE_KEYS,
    SWITCH_TITLES,
    VIEW_ALL,
    VIEW_CHANGED,
    changed_summary,
    group_changes,
    hold_summary,
    package_name,
    packaged_task_names,
    permission_title,
    records_never_checked,
    selects_every_record,
    startup_record,
    task_display_name,
    value_text,
)

APPROVED = "Software\\Microsoft\\Windows\\CurrentVersion\\Explorer\\StartupApproved"
PACKAGED = (
    "Software\\Classes\\Local Settings\\Software\\Microsoft\\Windows\\CurrentVersion\\AppModel\\SystemAppData"
)
TERMINAL_FAMILY = "Microsoft.WindowsTerminal_8wekyb3d8bbwe"
TERMINAL_TASK = "StartTerminalOnLoginTask"
T0 = "2026-09-25T10:00:00+00:00"


def _record(
    hive: str,
    key_path: str,
    value_name: str,
    recorded_at: str = T0,
    *,
    active: bool = True,
    original: dict[str, Any] | None = None,
) -> dict[str, Any]:
    return {
        "target": f"{hive}\\{key_path}\\{value_name}",
        "hive": hive,
        "key_path": key_path,
        "value_name": value_name,
        "original": original,
        "active": active,
        "recorded_at": recorded_at,
    }


def _startup_record(hive: str, subkey: str, name: str, recorded_at: str = T0) -> dict[str, Any]:
    """Journal record of a StartupApproved value written by a startup toggle."""
    return _record(hive, f"{APPROVED}\\{subkey}", name, recorded_at)


def _export(*registry: dict[str, Any], **other: list[dict[str, Any]]) -> dict[str, Any]:
    return {
        "registry": list(registry),
        "services": [],
        "scheduled_tasks": [],
        "appx": [],
        "power": [],
        "dns": [],
        **other,
    }


CEIP = "\\Microsoft\\Windows\\Customer Experience Improvement Program"
CONSOLIDATOR = f"{CEIP}\\Consolidator"
USB_CEIP = f"{CEIP}\\UsbCeip"
PROXY = "\\Microsoft\\Windows\\Autochk\\Proxy"
WIFI_GUID = "{aaaaaaaa-0000-0000-0000-000000000001}"


def _task(
    path: str, recorded_at: str = T0, *, was_enabled: bool = True, active: bool = True
) -> dict[str, Any]:
    """Scheduled-task journal record, shaped like the engine's export entry."""
    return {
        "id": 1,
        "session_id": 1,
        "recorded_at": recorded_at,
        "target": f"scheduled task {path}",
        "path": path,
        "was_enabled": was_enabled,
        "active": active,
        "reverted_at": None if active else T0,
    }


def _dns(
    family: str,
    previous: list[str],
    recorded_at: str = T0,
    *,
    guid: str = WIFI_GUID,
    adapter: str = "Wi-Fi",
    active: bool = True,
) -> dict[str, Any]:
    """DNS journal record, shaped like the engine's export entry."""
    label = "IPv4" if family == "ipv4" else "IPv6"
    return {
        "id": 1,
        "session_id": 1,
        "recorded_at": recorded_at,
        "target": f"{label} DNS servers of {adapter}",
        "interface_guid": guid,
        "family": family,
        "adapter_name": adapter,
        "previous_servers": previous,
        "target_servers": ["1.1.1.1", "1.0.0.1"] if family == "ipv4" else ["2606:4700:4700::1111"],
        "active": active,
        "reverted_at": None if active else T0,
    }


def test_history_keeps_same_named_startup_entries_apart() -> None:
    export = _export(
        _startup_record("HKCU", "Run", "Discord", "2026-09-25T10:00:00+00:00"),
        _startup_record("HKCU", "StartupFolder", "Discord", "2026-09-25T10:01:00+00:00"),
        _startup_record("HKLM", "Run32", "Discord", "2026-09-25T10:02:00+00:00"),
    )
    groups = group_changes(export, {})

    assert [g.title for g in groups] == [
        "Startup app: Discord (HKLM Run (32-bit))",
        "Startup app: Discord (Startup folder)",
        "Startup app: Discord (HKCU Run)",
    ]
    assert all(g.kind == "startup" for g in groups)
    # Each Undo restores only its own entry.
    assert [g.filter["registry"] for g in groups] == [
        [{"hive": "HKLM", "key_path": f"{APPROVED}\\Run32", "value_name": "Discord"}],
        [{"hive": "HKCU", "key_path": f"{APPROVED}\\StartupFolder", "value_name": "Discord"}],
        [{"hive": "HKCU", "key_path": f"{APPROVED}\\Run", "value_name": "Discord"}],
    ]
    assert [g.needs_admin for g in groups] == [True, False, False]


def test_records_of_one_startup_entry_share_a_group() -> None:
    # Registry names are case-insensitive, so both records belong to the same value.
    export = _export(
        _startup_record("HKCU", "Run", "Discord", "2026-09-25T10:00:00+00:00"),
        _record("HKCU", f"{APPROVED.upper()}\\RUN", "discord", "2026-09-25T11:00:00+00:00"),
    )
    [group] = group_changes(export, {})
    assert group.title == "Startup app: Discord (HKCU Run)"
    assert len(group.details) == 2
    assert group.recorded_at == "2026-09-25T11:00:00+00:00"


def test_packaged_startup_task_is_titled_like_the_startup_tab() -> None:
    task_key = f"{TERMINAL_FAMILY}\\{TERMINAL_TASK}"
    entries = [
        {"id": f"packaged_task:{task_key}", "name": "Terminal"},
        {"id": "user_run:Discord", "name": "Discord"},
    ]
    names = packaged_task_names(entries)
    assert names == {task_key.lower(): "Terminal"}

    export = _export(_record("HKCU", f"{PACKAGED}\\{task_key}", "State"))
    [group] = group_changes(export, {}, names)
    assert group.title == "Startup app: Terminal (Packaged app)"
    assert group.kind == "startup"
    assert not group.needs_admin, "the task key is per-user"

    # Without the startup list the Startup section's fallback name is used.
    [group] = group_changes(export, {})
    assert group.title == "Startup app: Microsoft.WindowsTerminal (StartTerminalOnLoginTask) (Packaged app)"


def test_startup_record_recognises_only_startup_toggles() -> None:
    assert startup_record("HKCU", f"{APPROVED}\\Run", "Spotify") == ("Spotify", "HKCU Run")
    assert startup_record("HKLM", f"{APPROVED}\\StartupFolder", "Agent") == (
        "Agent",
        "Startup folder (all users)",
    )
    assert startup_record("HKCU", f"{APPROVED}\\Unknown", "App") == ("App", "HKCU Unknown")
    policy = "Software\\Policies\\Microsoft\\Windows\\DataCollection"
    assert startup_record("HKLM", policy, "AllowTelemetry") is None


def test_tweak_records_are_grouped_by_catalog_title() -> None:
    telemetry = "SOFTWARE\\Policies\\Microsoft\\Windows\\DataCollection"
    export = _export(
        _record(
            "HKLM",
            telemetry,
            "AllowTelemetry",
            "2026-09-25T10:00:00+00:00",
            original={"type": "Dword", "value": 3},
        ),
        _record("HKLM", telemetry, "MaxTelemetryAllowed", "2026-09-25T10:00:01+00:00"),
        _record("HKCU", "Software\\Test", "Unlisted", "2026-09-25T09:00:00+00:00"),
        _record("HKCU", "Software\\Test", "Reverted", active=False),
        services=[
            {
                "name": "DiagTrack",
                "start_type": "Automatic",
                "was_running": True,
                "active": True,
                "recorded_at": "2026-09-25T10:00:02+00:00",
            }
        ],
        appx=[
            {
                "package_family": "Microsoft.BingNews_8wekyb3d8bbwe",
                "package_full_name": "Microsoft.BingNews_4.1.0.0_x64__8wekyb3d8bbwe",
                "active": True,
                "recorded_at": "2026-09-25T08:00:00+00:00",
            }
        ],
    )
    titles = {
        f"HKLM\\{telemetry}\\AllowTelemetry": "Limit diagnostic data",
        f"HKLM\\{telemetry}\\MaxTelemetryAllowed": "Limit diagnostic data",
        "service DiagTrack": "Limit diagnostic data",
    }
    groups = group_changes(export, titles)

    assert [(g.title, g.kind) for g in groups] == [
        ("Limit diagnostic data", "setting"),
        ("HKCU\\Software\\Test\\Unlisted", "setting"),
        ("Removed app: Microsoft.BingNews", "app"),
    ]
    limit = groups[0]
    assert len(limit.details) == 3
    assert limit.details[0].endswith("was 3")
    assert limit.filter["services"] == ["DiagTrack"]
    assert limit.needs_admin
    unlisted = groups[1]
    assert unlisted.details == ["HKCU\\Software\\Test\\Unlisted  ·  was not set"]
    assert not unlisted.needs_admin, "a per-user value outside the policy keys"
    assert groups[2].filter["appx_families"] == ["Microsoft.BingNews_8wekyb3d8bbwe"]


def test_value_and_package_names() -> None:
    assert value_text(None) == "not set"
    assert value_text({"type": "Sz", "value": "x"}) == '"x"'
    assert value_text({"type": "Binary", "value": [1, 2, 3]}) == "<3 bytes>"
    assert value_text({"type": "MultiSz", "value": ["a", "b"]}) == "a, b"
    assert value_text({"type": "Dword", "value": 0}) == "0"
    assert package_name(TERMINAL_FAMILY) == "Microsoft.WindowsTerminal"
    assert package_name("NoPublisher") == "NoPublisher"


def test_scheduled_task_records_are_grouped_by_catalog_title() -> None:
    dm_client_path = "\\Microsoft\\Windows\\Feedback\\Siuf\\DmClient"
    unlisted = _task(dm_client_path, "2026-09-25T09:00:00+00:00")
    del unlisted["target"]  # the target is derived from the path when an export lacks it
    export = _export(
        scheduled_tasks=[
            _task(CONSOLIDATOR, "2026-09-25T10:00:00+00:00"),
            _task(USB_CEIP, "2026-09-25T10:00:01+00:00", was_enabled=False),
            _task(PROXY, "2026-09-25T11:00:00+00:00", active=False),
            unlisted,
        ]
    )
    # The catalog spells one path in a different case; task paths are case-insensitive.
    titles = {
        f"scheduled task {CONSOLIDATOR.upper()}": "Turn off the CEIP tasks",
        f"scheduled task {USB_CEIP}": "Turn off the CEIP tasks",
        f"scheduled task {PROXY}": "Turn off the CEIP tasks",
    }
    groups = group_changes(export, titles)

    assert [(g.title, g.kind) for g in groups] == [
        ("Turn off the CEIP tasks", "scheduled_task"),
        ("Scheduled task: DmClient", "scheduled_task"),
    ]
    ceip, dm_client = groups
    assert ceip.details == [
        f"scheduled task {CONSOLIDATOR}  ·  was enabled",
        f"scheduled task {USB_CEIP}  ·  was disabled",
    ]
    assert ceip.recorded_at == "2026-09-25T10:00:01+00:00", "the reverted Proxy record is ignored"
    assert ceip.filter["scheduled_tasks"] == [CONSOLIDATOR, USB_CEIP]
    assert ceip.filter["registry"] == [] and ceip.filter["dns"] == []
    assert dm_client.details == [f"scheduled task {dm_client_path}  ·  was enabled"]
    assert dm_client.filter["scheduled_tasks"] == [dm_client_path]
    assert ceip.needs_admin and dm_client.needs_admin, "task changes are machine-wide"


def test_scheduled_task_group_merges_with_other_records_of_the_same_tweak() -> None:
    policy = "SOFTWARE\\Policies\\Microsoft\\SQMClient\\Windows"
    export = _export(
        _record(
            "HKLM",
            policy,
            "CEIPEnable",
            "2026-09-25T10:00:00+00:00",
            original={"type": "Dword", "value": 1},
        ),
        scheduled_tasks=[_task(CONSOLIDATOR, "2026-09-25T10:00:02+00:00")],
    )
    titles = {
        f"HKLM\\{policy}\\CEIPEnable": "Customer Experience Improvement Program",
        f"scheduled task {CONSOLIDATOR}": "Customer Experience Improvement Program",
    }
    [group] = group_changes(export, titles)

    assert group.title == "Customer Experience Improvement Program"
    assert group.kind == "setting", "the group keeps the kind of its first record"
    assert group.details == [
        f"HKLM\\{policy}\\CEIPEnable  ·  was 1",
        f"scheduled task {CONSOLIDATOR}  ·  was enabled",
    ]
    assert group.recorded_at == "2026-09-25T10:00:02+00:00"
    # One Undo restores both records.
    assert group.filter["registry"] == [{"hive": "HKLM", "key_path": policy, "value_name": "CEIPEnable"}]
    assert group.filter["scheduled_tasks"] == [CONSOLIDATOR]
    assert group.needs_admin


def test_dns_records_group_per_adapter() -> None:
    ethernet = "{aaaaaaaa-0000-0000-0000-000000000002}"
    export = _export(
        dns=[
            _dns("ipv4", [], "2026-09-25T10:00:00+00:00"),
            _dns("ipv6", ["2001:db8::1"], "2026-09-25T10:00:01+00:00"),
            _dns(
                "ipv4",
                ["192.168.0.1", "8.8.8.8"],
                "2026-09-25T09:00:00+00:00",
                guid=ethernet,
                adapter="Ethernet",
            ),
        ]
    )
    groups = group_changes(export, {})

    assert [(g.title, g.kind) for g in groups] == [
        ("DNS servers: Wi-Fi", "dns"),
        ("DNS servers: Ethernet", "dns"),
    ]
    wifi, wired = groups
    assert wifi.details == ["IPv4 DNS  ·  was automatic", "IPv6 DNS  ·  was 2001:db8::1"]
    assert wifi.recorded_at == "2026-09-25T10:00:01+00:00"
    # A GUID selects both address families, so it is sent once.
    assert wifi.filter["dns"] == [WIFI_GUID]
    assert wifi.filter["registry"] == [] and wifi.filter["scheduled_tasks"] == []
    assert wifi.needs_admin, "DNS servers are machine-wide"
    assert wired.details == ["IPv4 DNS  ·  was 192.168.0.1, 8.8.8.8"]
    assert wired.filter["dns"] == [ethernet]


def test_dns_records_of_one_adapter_share_a_group_whatever_the_guid_spelling() -> None:
    export = _export(
        dns=[
            _dns("ipv4", [], "2026-09-25T10:00:00+00:00"),
            _dns("ipv6", [], "2026-09-25T10:00:01+00:00", guid=WIFI_GUID.upper().strip("{}")),
        ]
    )
    [group] = group_changes(export, {})
    assert len(group.details) == 2
    assert group.filter["dns"] == [WIFI_GUID]


def test_inactive_dns_records_are_ignored() -> None:
    assert group_changes(_export(dns=[_dns("ipv4", [], active=False)]), {}) == []

    export = _export(dns=[_dns("ipv4", ["9.9.9.9"]), _dns("ipv6", [], active=False)])
    [group] = group_changes(export, {})
    assert group.details == ["IPv4 DNS  ·  was 9.9.9.9"]
    assert group.filter["dns"] == [WIFI_GUID]


def test_exports_without_new_keys_still_group() -> None:
    # An engine that predates scheduled-task and DNS records exports neither key.
    export = {
        "registry": [_record("HKCU", "Software\\Test", "Value", original={"type": "Dword", "value": 0})],
        "services": [
            {
                "name": "DiagTrack",
                "start_type": "Automatic",
                "was_running": False,
                "active": True,
                "recorded_at": "2026-09-25T09:00:00+00:00",
            }
        ],
    }
    groups = group_changes(export, {"service DiagTrack": "Turn off the telemetry service"})

    assert [(g.title, g.kind) for g in groups] == [
        ("HKCU\\Software\\Test\\Value", "setting"),
        ("Turn off the telemetry service", "service"),
    ]
    assert groups[0].details == ["HKCU\\Software\\Test\\Value  ·  was 0"]
    assert groups[1].details == ["service DiagTrack  ·  was Automatic, stopped"]
    assert all(g.filter["scheduled_tasks"] == [] and g.filter["dns"] == [] for g in groups)
    assert group_changes({}, {}) == []


MAINTENANCE_TASK = "\\Cairn\\Maintenance-S-1-5-21-1111111111-2222222222-3333333333-1001"
USER_STORE = "Software\\Microsoft\\Windows\\CurrentVersion\\CapabilityAccessManager\\ConsentStore"
DEVICE_STORE = "SOFTWARE\\Microsoft\\Windows\\CurrentVersion\\CapabilityAccessManager\\ConsentStore"
MEET_FAMILY = "Contoso.Meet_aaaaaaaaaaaaa"


def _task_definition(path: str, recorded_at: str = T0, *, active: bool = True) -> dict[str, Any]:
    """Task-definition journal record, shaped like the engine's export entry."""
    return {
        "id": 1,
        "session_id": 1,
        "recorded_at": recorded_at,
        "target": f"task {path}",
        "path": path,
        "purpose": "maintenance",
        "folder_created": True,
        "active": active,
        "reverted_at": None,
    }


def test_task_definitions_are_one_group_each_and_need_admin() -> None:
    other = "\\PCOptimizerSelfTest\\NoSuchMaintenance"
    export = _export(
        task_definitions=[
            _task_definition(MAINTENANCE_TASK, "2026-09-25T11:00:00+00:00"),
            _task_definition(other),
            _task_definition("\\Cairn\\Maintenance-S-1-5-21-9", active=False),
        ]
    )
    groups = group_changes(export, {})
    assert [(g.title, g.kind) for g in groups] == [
        ("Scheduled maintenance", "task_definition"),
        ("Task created by Cairn: NoSuchMaintenance", "task_definition"),
    ]
    maintenance, created = groups
    assert maintenance.details == ["Task Scheduler: \\Cairn\\Maintenance (your account)  ·  created by Cairn"]
    assert maintenance.filter["task_definitions"] == [MAINTENANCE_TASK]
    assert created.filter["task_definitions"] == [other]
    assert maintenance.needs_admin and created.needs_admin
    assert all(g.filter["registry"] == [] for g in groups)


def test_maintenance_title_ignores_case() -> None:
    export = _export(task_definitions=[_task_definition("\\cairn\\MAINTENANCE-S-1-5-21-7")])
    [group] = group_changes(export, {})
    assert group.title == "Scheduled maintenance"


def test_task_display_name_hides_the_account_sid() -> None:
    assert task_display_name(MAINTENANCE_TASK) == "\\Cairn\\Maintenance (your account)"
    assert task_display_name("\\Cairn\\Other") == "\\Cairn\\Other"


def test_filters_select_no_task_definitions_by_default() -> None:
    [group] = group_changes(_export(_record("HKCU", "Software\\Test", "Value")), {})
    assert group.filter["task_definitions"] == []
    assert not group.needs_admin


def test_permission_values_of_one_entry_share_a_group() -> None:
    app_key = f"{USER_STORE}\\webcam\\{MEET_FAMILY}"
    export = _export(
        _record("HKCU", app_key, "Value", original={"type": "Sz", "value": "Allow"}),
        _record("HKCU", app_key.upper(), "LastSetTime", "2026-09-25T10:00:01+00:00"),
        _record("HKCU", f"{USER_STORE}\\microphone\\NonPackaged", "Value"),
        _record("HKLM", f"{DEVICE_STORE}\\location", "Value"),
        _record("HKCU", f"{USER_STORE}\\location", "Value", "2026-09-25T09:00:00+00:00"),
    )
    groups = group_changes(export, {}, permission_names={MEET_FAMILY.lower(): "Contoso Meet"})
    by_title = {g.title: g for g in groups}
    assert set(by_title) == {
        "Camera permission: Contoso Meet",
        "Let desktop apps access your microphone",
        "Location services on this PC",
        "Let apps access your location",
    }
    assert all(g.kind == PERMISSION_KIND for g in groups)
    camera = by_title["Camera permission: Contoso Meet"]
    assert [r["value_name"] for r in camera.filter["registry"]] == ["Value", "LastSetTime"]
    assert camera.details[0] == f'HKCU\\{app_key}\\Value  ·  was "Allow"'
    assert not camera.needs_admin, "a per-user permission undoes without elevation"
    assert not by_title["Let apps access your location"].needs_admin
    assert by_title["Location services on this PC"].needs_admin, "the whole-PC switch is in HKLM"


def test_permission_app_without_a_name_uses_its_package_name() -> None:
    [group] = group_changes(_export(_record("HKCU", f"{USER_STORE}\\webcam\\{MEET_FAMILY}", "Value")), {})
    assert group.title == "Camera permission: Contoso.Meet"


def test_permission_titles() -> None:
    assert permission_title("HKLM", f"{DEVICE_STORE}\\webcam") == SWITCH_TITLES[("camera", "device")]
    assert permission_title("HKCU", f"{USER_STORE}\\WEBCAM") == "Let apps access your camera"
    desktop = permission_title("HKCU", f"{USER_STORE}\\location\\nonpackaged")
    assert desktop == SWITCH_TITLES[("location", "desktop_apps")]
    assert permission_title("HKCU", f"{USER_STORE}\\bluetooth") == "Let apps access your bluetooth"
    assert permission_title("HKCU", "Software\\Test") is None
    assert permission_title("HKCU", USER_STORE) is None
    assert STORE_KEYS == {"webcam": "camera", "microphone": "microphone", "location": "location"}
    assert len(SWITCH_TITLES) == 9


def test_startup_records_are_not_taken_for_permissions() -> None:
    [group] = group_changes(_export(_startup_record("HKCU", "Run", "Discord")), {})
    assert group.kind == "startup"


def test_hold_summary_claims_only_what_was_read() -> None:
    assert hold_summary(4, 0) == (ALL_HOLD_TEXT, theme.GOOD)
    assert ALL_HOLD_TEXT == "✓ Every recorded change still holds."
    assert hold_summary(0, 0) == (ALL_HOLD_TEXT, theme.GOOD), "nothing left unread"
    assert hold_summary(4, 1) == (
        "✓ Every change Cairn could check still holds. Cairn could not check 1 recorded change.",
        theme.GOOD,
    )
    assert hold_summary(4, 3) == (
        "✓ Every change Cairn could check still holds. Cairn could not check 3 recorded changes.",
        theme.GOOD,
    )
    # With nothing read, nothing is said to hold.
    assert hold_summary(0, 1) == (
        "Cairn could not check whether 1 recorded change still holds.",
        theme.INK_MUTED,
    )
    assert hold_summary(0, 2) == (
        "Cairn could not check whether 2 recorded changes still hold.",
        theme.INK_MUTED,
    )


def test_hold_summary_names_an_earlier_version_as_the_reason() -> None:
    # Every unchecked record was left by an earlier version of Cairn with no written value:
    # the summary gives that reason instead of the general "could not check".
    assert hold_summary(0, 5, 5) == (
        "Cairn could not check 5 changes made by an earlier version of Cairn, which did not record "
        "what it set.",
        theme.INK_MUTED,
    )
    assert hold_summary(0, 1, 1) == (
        "Cairn could not check 1 change made by an earlier version of Cairn, which did not record "
        "what it set.",
        theme.INK_MUTED,
    )
    assert hold_summary(3, 2, 2) == (
        "✓ Every change Cairn could check still holds. Cairn could not check 2 changes made by an "
        "earlier version of Cairn, which did not record what it set.",
        theme.GOOD,
    )
    # A mix of an earlier-version record and another unreadable one keeps the general wording.
    assert hold_summary(0, 2, 1) == (
        "Cairn could not check whether 2 recorded changes still hold.",
        theme.INK_MUTED,
    )


def test_records_the_drift_check_never_reads() -> None:
    removed = {
        "package_family": "Contoso.Notes_aaaaaaaaaaaaa",
        "package_full_name": "Contoso.Notes_1.0.0.0_x64__aaaaaaaaaaaaa",
        "active": True,
        "recorded_at": T0,
    }
    export = _export(
        _record("HKCU", "Software\\Test", "Value"),
        scheduled_tasks=[_task(PROXY)],
        dns=[_dns("ipv4", [])],
        appx=[removed, {**removed, "active": False}],
        task_definitions=[
            _task_definition(MAINTENANCE_TASK),
            _task_definition("\\Cairn\\Other", active=False),
        ],
    )
    # The active Store app removal and the active registered task; the check reads or counts
    # every other kind of record itself.
    assert records_never_checked(export) == 2
    assert records_never_checked(_export(_record("HKCU", "Software\\Test", "Value"))) == 0
    assert records_never_checked({}) == 0, "an export without these lists"


def test_a_selection_of_every_record_is_told_from_one_that_leaves_some() -> None:
    value = {"hive": "HKLM", "key_path": "SOFTWARE\\Policies\\Test", "value_name": "Value"}
    other = {"hive": "HKLM", "key_path": "SOFTWARE\\Policies\\Test", "value_name": "Other"}
    change = {
        "registry": [value, other],
        "scheduled_tasks": ["\\Microsoft\\Test\\Proxy"],
        "power": True,
        "dns": ["{00000000-0000-0000-0000-00000000C0DE}"],
    }
    everything = {
        "registry": [{**value, "key_path": value["key_path"].upper()}, other],
        "scheduled_tasks": ["\\MICROSOFT\\TEST\\PROXY"],
        "power": True,
        "dns": ["00000000-0000-0000-0000-00000000c0de"],
    }
    assert selects_every_record(change, everything), "compared as the engine compares records"
    for left_out in ("registry", "scheduled_tasks", "power", "dns"):
        selection = {**everything, left_out: False if left_out == "power" else everything[left_out][1:]}
        assert not selects_every_record(change, selection), left_out
    assert selects_every_record({"registry": [value]}, {"registry": [value], "power": True})


LOCATION_SENSOR = (
    "SOFTWARE\\Microsoft\\Windows NT\\CurrentVersion\\Sensor\\Overrides"
    "\\{BFA794E4-F964-4FDB-90F6-51056BFE4B44}"
)


def test_the_location_override_undoes_with_the_location_switch() -> None:
    location = f"{DEVICE_STORE}\\location"
    export = _export(
        _record("HKLM", location, "Value"),
        _record("HKLM", location, "LastSetTime"),
        _record("HKLM", LOCATION_SENSOR.upper(), "SensorPermissionState"),
        _record("HKCU", f"{USER_STORE}\\location", "Value"),
    )
    groups = {g.title: g for g in group_changes(export, {})}
    assert set(groups) == {"Location services on this PC", "Let apps access your location"}
    device = groups["Location services on this PC"]
    assert device.kind == PERMISSION_KIND
    assert [r["value_name"] for r in device.filter["registry"]] == [
        "Value",
        "LastSetTime",
        "SensorPermissionState",
    ]
    assert device.needs_admin
    assert permission_title("HKLM", LOCATION_SENSOR) == "Location services on this PC"
    # Only the location sensor's override under HKLM is a permission record.
    assert permission_title("HKCU", LOCATION_SENSOR) is None
    assert permission_title("HKLM", LOCATION_SENSOR.replace("BFA794E4", "AAAAAAAA")) is None


def test_long_detail_lines_wrap_inside_their_row(make_app: Any) -> None:
    # Imported here: the window helpers skip this test when CustomTkinter or the telemetry DLL
    # is missing, and the grouping tests above run without them.
    from .app_support import FIT_SIZES, ctk, idle, pump, resize_to, show_section

    # The detail line of a Store app's permission record, its consent-store path, is longer than
    # a row is wide at every size the sections must fit.
    app, _ = make_app(
        elevated=True,
        permission_recorded={
            "camera:app:Microsoft.WindowsCamera_8wekyb3d8bbwe": "prompt",
            f"microphone:app:{MEET_FAMILY}": "deny",
        },
    )
    show_section(app, "History")
    pump(app, 5.0, until=lambda: app.history_panel.row_count == 2 and idle(app))
    app.minsize(900, 560)
    for width, height in FIT_SIZES:
        resize_to(app, width, height)
        for row in app.history_panel.rows:
            [detail] = [
                w
                for w in row.winfo_children()
                if isinstance(w, ctk.CTkLabel) and int(w.grid_info()["row"]) == 2
            ]
            where = f"{row.change.title} at {width}x{height}"
            assert detail.cget("text") == "\n".join(row.change.details), where
            need, got = detail._label.winfo_reqwidth(), detail.winfo_width()
            assert need <= got + 1, f"{where}: the details need {need} px and get {got}"
            left, top = detail.winfo_rootx() - row.winfo_rootx(), detail.winfo_rooty() - row.winfo_rooty()
            assert left >= 0 and top >= 0, where
            assert left + got <= row.winfo_width() + 1, f"{where}: the details stick out of the row"
            assert top + detail.winfo_height() <= row.winfo_height() + 1, where
    assert app.errors == []


# -- the Changed view (window tests) ------------------------------------------------------

GAME_MODE = "gaming.game_mode"
GAME_MODE_RECORD = {"hive": "HKLM", "key_path": "Test", "value_name": GAME_MODE}
CHANGED = {
    "target": f"HKLM\\Test\\{GAME_MODE}",
    "label": "AllowAutoGameMode",
    "expected": "1",
    "found": "0",
    "original": "not set",
    "back_to_original": False,
}
CHANGED_LINE = "⚠ AllowAutoGameMode is 0  ·  Cairn set 1  ·  original not set"
BACK = {**CHANGED, "found": "not set", "back_to_original": True}
FORMER = {
    "target": "HKLM\\SOFTWARE\\Policies\\Test\\Former",
    "original": "not set",
    "reason": "Cairn 0.2 also set this value, which this tweak no longer uses, so Cairn no longer sets it.",
}
TOO_NEW = (
    "Windows 11 24H2 and later no longer use this setting; this PC runs Windows 11 Home 26H2 "
    "(build 26300.9457)."
)


class Window:
    """The window helpers and the App's History texts, imported only by the window tests."""

    def __init__(self) -> None:
        from optimizer import app as app_module

        from . import app_support

        self.app_module = app_module
        self.support = app_support


@pytest.fixture
def ui() -> Iterator[Window]:
    yield Window()


def open_history(ui: Window, app: Any, rows: int) -> Any:
    s = ui.support
    s.pump(app, 5.0, until=lambda: s.scanned(app) and bool(app._titles))
    s.show_section(app, "History")
    s.pump(app, 5.0, until=lambda: app.history_panel.row_count == rows and s.idle(app))
    return app.history_panel


def test_changed_view_lists_only_drifted_groups(make_app: Any, ui: Window) -> None:
    s = ui.support
    app, engine = make_app(elevated=True, drift={GAME_MODE: [CHANGED]})
    engine.applied.update({GAME_MODE, "privacy.advertising_id"})
    panel = open_history(ui, app, rows=2)
    assert panel.drift_shown and panel.view == VIEW_ALL
    assert panel.view_bar.winfo_ismapped()
    assert panel.view_bar.cget("values") == ["All", "Changed (1)"]
    assert engine.calls_named("drift"), "History reads which changes still hold"
    by_title = {row.change.title: row for row in panel.rows}
    assert by_title["Game Mode"].drift_label is not None
    assert by_title["Game Mode"].drift_label.cget("text") == "⚠ Changed since Cairn applied it; see Changed."
    assert by_title["Advertising Id"].drift_label is None
    assert all(row.keep_button is None and row.apply_again_button is None for row in panel.rows)

    panel._view_selected("Changed (1)")
    assert panel.view == VIEW_CHANGED and panel.row_count == 1
    [row] = panel.rows
    assert row.change.title == "Game Mode"
    assert row.change.item_ids == [GAME_MODE]
    assert row.detail_label.cget("text") == CHANGED_LINE
    assert row.detail_label.cget("text_color") == s.theme.WARNING
    assert panel.summary.cget("text") == changed_summary(1)
    assert panel.summary.cget("text_color") == s.theme.WARNING
    assert [b.cget("text") for b in (row.apply_again_button, row.undo_button, row.keep_button)] == [
        "Apply again",
        "Undo",
        "Keep as is",
    ]
    assert all(
        b.cget("state") == "normal" for b in (row.apply_again_button, row.undo_button, row.keep_button)
    )

    # A refresh keeps the view.
    app.load_history()
    s.pump(app, 5.0, until=lambda: s.idle(app) and len(engine.calls_named("drift")) >= 2)
    s.pump(app, 0.2)
    assert panel.view == VIEW_CHANGED and panel.row_count == 1
    panel._view_selected("All")
    assert panel.view == VIEW_ALL and panel.row_count == 2
    assert app.errors == []


def test_keep_as_is_back_to_original_is_not_danger(make_app: Any, ui: Window) -> None:
    s, texts = ui.support, ui.app_module
    app, engine = make_app(elevated=True, drift={GAME_MODE: [BACK]})
    engine.applied.add(GAME_MODE)
    panel = open_history(ui, app, rows=1)
    panel.set_view(VIEW_CHANGED)
    [row] = panel.rows
    assert "back to the original" in row.detail_label.cget("text")
    row.keep_button.invoke()
    dialog = s.next_dialog(app)
    assert dialog.title_text == "Keep Game Mode as it is?"
    text = s.dialog_text(dialog)
    assert texts.KEEP_BACK_TEXT in text
    assert dialog.confirm_button.cget("fg_color") == s.theme.ACCENT
    assert dialog.confirm_button.cget("text") == "Keep as is"
    s.pump(app, 0.3)
    dialog.confirm_button.invoke()
    s.pump(app, 5.0, until=lambda: bool(engine.calls_named("forget_targets")))
    [(sent,)] = engine.calls_named("forget_targets")
    assert sent["registry"] == [GAME_MODE_RECORD], "only the record that changed"
    s.pump(app, 5.0, until=lambda: panel.row_count == 0 and s.idle(app))
    assert app.status_message.cget("text") == texts.KEEP_DONE_TEXT.format(title="Game Mode")
    assert app.status_message.cget("text_color") == s.theme.GOOD
    assert engine.applied == set(), "no longer tracked"
    assert not engine.calls_named("revert_targets") and not engine.calls_named("apply")
    assert panel.summary.cget("text") == ALL_HOLD_TEXT
    assert app.errors == []


def test_keep_as_is_otherwise_uses_the_danger_dialog(make_app: Any, ui: Window) -> None:
    s, texts = ui.support, ui.app_module
    app, engine = make_app(elevated=False, drift={GAME_MODE: [CHANGED]})
    engine.applied.add(GAME_MODE)
    panel = open_history(ui, app, rows=1)
    panel.set_view(VIEW_CHANGED)
    [row] = panel.rows
    assert row.keep_button.cget("state") == "normal", "Keep as is needs no administrator rights"
    assert row.apply_again_button.cget("state") == "disabled", "Apply again does"
    row.keep_button.invoke()
    dialog = s.next_dialog(app)
    text = s.dialog_text(dialog)
    assert texts.KEEP_LOSES_TEXT in text
    assert "• AllowAutoGameMode: original not set, now 0" in text
    assert dialog.confirm_button.cget("fg_color") == s.theme.CRITICAL
    s.pump(app, 0.3)
    dialog._cancel()
    s.pump(app, 0.2)
    assert not engine.calls_named("forget_targets")

    # A record the engine still tracks is listed in a dialog of its own.
    target = f"HKLM\\Test\\{GAME_MODE}"

    def refuse(selection: dict[str, Any]) -> dict[str, Any]:
        engine._record("forget_targets", selection)
        error = f"{target} still holds the value Cairn set; Undo restores the original."
        return {"forgotten": [], "refused": [{"target": target, "error": error}]}

    engine.forget_targets = refuse  # type: ignore[method-assign]
    row.keep_button.invoke()
    s.confirm_dialog(app)
    s.pump(app, 5.0, until=lambda: any(d.title_text == texts.KEEP_REFUSED_TITLE for d in s.dialogs(app)))
    refused = next(d for d in s.dialogs(app) if d.title_text == texts.KEEP_REFUSED_TITLE)
    assert f"• {target} still holds the value Cairn set" in s.dialog_text(refused)
    assert app.status_message.cget("text") == texts.KEEP_PARTLY_TEXT.format(title="Game Mode")
    s.pump(app, 0.3)
    refused._cancel()
    assert engine.applied == {GAME_MODE}
    assert app.errors == []


def test_apply_again_applies_the_item_ids(make_app: Any, ui: Window) -> None:
    s, texts = ui.support, ui.app_module
    app, engine = make_app(elevated=True, drift={GAME_MODE: [CHANGED]})
    engine.applied.add(GAME_MODE)
    panel = open_history(ui, app, rows=1)
    panel.set_view(VIEW_CHANGED)
    panel.rows[0].apply_again_button.invoke()
    dialog = s.next_dialog(app)
    assert dialog.title_text == "Apply Game Mode again?"
    text = s.dialog_text(dialog)
    assert texts.APPLY_AGAIN_TEXT in text and CHANGED_LINE in text
    assert dialog.confirm_button.cget("text") == "Apply again"
    s.pump(app, 0.3)
    dialog.confirm_button.invoke()
    s.pump(app, 5.0, until=lambda: bool(engine.calls_named("apply")))
    assert engine.calls_named("apply")[-1] == ([GAME_MODE], "try", False)
    # The value holds again: History, which is shown, reloads without it.
    s.pump(app, 5.0, until=lambda: panel.drift_count == 0 and s.idle(app))
    assert panel.view == VIEW_CHANGED and panel.row_count == 0
    assert panel.summary.cget("text") == ALL_HOLD_TEXT
    assert panel.summary.cget("text_color") == s.theme.GOOD
    assert panel.view_bar.cget("values") == ["All", "Changed (0)"]
    assert app.errors == []


def test_apply_again_is_not_offered_for_a_retired_tweak(make_app: Any, ui: Window) -> None:
    app, engine = make_app(
        elevated=True,
        extra_tweaks=[("privacy.cortana", "privacy", True)],
        unavailable={"privacy.cortana": ("retired", "Cairn found no effect of this policy on Windows 11.")},
        drift={
            "privacy.cortana": [{**CHANGED, "target": "HKLM\\Test\\privacy.cortana", "label": "AllowCortana"}]
        },
    )
    engine.applied.add("privacy.cortana")
    panel = open_history(ui, app, rows=1)
    panel.set_view(VIEW_CHANGED)
    [row] = panel.rows
    assert row.change.item_ids == ["privacy.cortana"]
    assert row.apply_again_button is None, "a retired tweak is never applied"
    assert row.undo_button is not None and row.keep_button is not None
    assert app.errors == []


def test_apply_again_is_not_offered_for_a_tweak_this_pc_cannot_use(make_app: Any, ui: Window) -> None:
    # The drift check reads every recorded tweak, but an apply skips a tweak this PC cannot use
    # before anything is recorded; its Optimize row offers only Undo.
    s = ui.support
    copilot = "privacy.copilot"
    app, engine = make_app(
        elevated=True,
        extra_tweaks=[(copilot, "privacy", False)],
        unavailable={copilot: ("windows_version", TOO_NEW)},
        drift={copilot: [{**CHANGED, "target": f"HKLM\\Test\\{copilot}", "label": "TurnOffWindowsCopilot"}]},
    )
    engine.applied.add(copilot)
    panel = open_history(ui, app, rows=1)
    panel.set_view(VIEW_CHANGED)
    [row] = panel.rows
    assert row.change.item_ids == [copilot]
    assert row.apply_again_button is None, "the apply would skip it"
    assert row.undo_button is not None and row.keep_button is not None
    app._on_history_apply_again(row.change)
    s.pump(app, 0.3)
    assert s.dialogs(app) == [] and engine.calls_named("apply") == []
    assert app.errors == []


UNDO_LIMIT = "Undo allows linking again, but a phone that was linked has to be linked again in Phone Link."


def test_apply_again_warns_about_what_undo_cannot_bring_back(make_app: Any, ui: Window) -> None:
    s, texts = ui.support, ui.app_module
    app, engine = make_app(elevated=True, undo_limits={GAME_MODE: UNDO_LIMIT}, drift={GAME_MODE: [CHANGED]})
    engine.applied.add(GAME_MODE)
    panel = open_history(ui, app, rows=1)
    panel.set_view(VIEW_CHANGED)
    panel.rows[0].apply_again_button.invoke()
    dialog = s.next_dialog(app)
    text = s.dialog_text(dialog)
    assert texts.APPLY_AGAIN_TEXT in text and CHANGED_LINE in text
    assert f"⚠ Game Mode: {UNDO_LIMIT}" in text
    assert dialog.confirm_button.cget("fg_color") == s.theme.CRITICAL, "the danger style"
    s.pump(app, 0.3)
    dialog._cancel()
    s.pump(app, 0.2)
    assert not engine.calls_named("apply")
    assert app.errors == []


def test_apply_again_warns_about_the_undo_limit_without_a_scan(make_app: Any, ui: Window) -> None:
    s = ui.support
    app, engine = make_app(elevated=True, undo_limits={GAME_MODE: UNDO_LIMIT}, drift={GAME_MODE: [CHANGED]})
    engine.applied.add(GAME_MODE)

    def locked() -> dict[str, Any]:
        engine._record("scan")
        raise RuntimeError("database is locked")

    # The scan at start fails, so only the catalog says what Undo cannot bring back.
    engine.scan = locked  # type: ignore[method-assign]
    s.pump(app, 5.0, until=lambda: app._scan_failed and bool(app._titles) and s.idle(app))
    assert app._last_scan is None
    s.show_section(app, "History")
    panel = app.history_panel
    s.pump(app, 5.0, until=lambda: panel.row_count == 1 and s.idle(app))
    panel.set_view(VIEW_CHANGED)
    panel.rows[0].apply_again_button.invoke()
    dialog = s.next_dialog(app)
    assert f"⚠ Game Mode: {UNDO_LIMIT}" in s.dialog_text(dialog)
    assert dialog.confirm_button.cget("fg_color") == s.theme.CRITICAL
    s.pump(app, 0.3)
    dialog._cancel()
    s.pump(app, 0.2)
    assert not engine.calls_named("apply")
    assert app.errors == []


def test_changed_view_claims_only_what_it_could_check(
    make_app: Any, ui: Window, caplog: pytest.LogCaptureFixture
) -> None:
    s = ui.support
    # An earlier build's permission change: its two records store no written value, so the
    # check reads only Game Mode's value, and the summary says why the other two were not read.
    app, engine = make_app(elevated=True, permission_recorded={f"camera:app:{MEET_FAMILY}": "deny"})
    engine.applied.add(GAME_MODE)
    panel = open_history(ui, app, rows=2)
    panel.set_view(VIEW_CHANGED)
    assert panel.row_count == 0
    assert panel.summary.cget("text") == (
        "✓ Every change Cairn could check still holds. Cairn could not check 2 changes made by an "
        "earlier version of Cairn, which did not record what it set."
    )
    assert panel.summary.cget("text_color") == s.theme.GOOD

    # With nothing read, nothing is claimed; a record that could not be read for another reason
    # keeps the general wording, and why it could not be read goes to the log.
    caplog.set_level(logging.WARNING, logger="optimizer.app")
    engine.unreadable.add(GAME_MODE)
    app.load_history()
    s.pump(app, 5.0, until=lambda: len(engine.calls_named("drift")) >= 2 and s.idle(app))
    s.pump(app, 0.3)
    assert panel.view == VIEW_CHANGED and panel.row_count == 0
    assert panel.summary.cget("text") == "Cairn could not check whether 3 recorded changes still hold."
    assert panel.summary.cget("text_color") == s.theme.INK_MUTED
    assert f"HKLM\\Test\\{GAME_MODE} could not be read (access denied)." in caplog.text
    assert app.errors == []

    # With only the earlier build's records left, nothing was read and the reason is named.
    engine.applied.discard(GAME_MODE)
    engine.unreadable.discard(GAME_MODE)
    app.load_history()
    s.pump(app, 5.0, until=lambda: len(engine.calls_named("drift")) >= 3 and s.idle(app))
    s.pump(app, 0.3)
    assert panel.view == VIEW_CHANGED and panel.row_count == 0
    assert panel.summary.cget("text") == (
        "Cairn could not check 2 changes made by an earlier version of Cairn, which did not record "
        "what it set."
    )
    assert panel.summary.cget("text_color") == s.theme.INK_MUTED
    assert app.errors == []


def test_changed_view_counts_records_the_check_never_reads(make_app: Any, ui: Window) -> None:
    from .fake_maintenance import DEFAULTS

    s = ui.support
    # The scheduled maintenance task is a recorded change that the drift check neither reads nor
    # counts as unread, like the Store apps Cairn removed.
    app, engine = make_app(elevated=True, maintenance_task=DEFAULTS)
    engine.applied.add(GAME_MODE)
    panel = open_history(ui, app, rows=2)
    panel.set_view(VIEW_CHANGED)
    assert panel.row_count == 0
    assert panel.summary.cget("text") == (
        "✓ Every change Cairn could check still holds. Cairn could not check 1 recorded change."
    )
    assert panel.summary.cget("text_color") == s.theme.GOOD

    # With only the task recorded, nothing was read, so nothing is said to hold.
    engine.applied.discard(GAME_MODE)
    app.load_history()
    s.pump(app, 5.0, until=lambda: len(engine.calls_named("drift")) >= 2 and s.idle(app))
    s.pump(app, 0.3)
    assert panel.view == VIEW_CHANGED and panel.row_count == 0
    assert panel.summary.cget("text") == "Cairn could not check whether 1 recorded change still holds."
    assert panel.summary.cget("text_color") == s.theme.INK_MUTED
    assert app.errors == []


def test_older_engine_without_drift_has_no_changed_view(make_app: Any, ui: Window) -> None:
    app, engine = make_app(elevated=True, unsupported=["drift"], drift={GAME_MODE: [CHANGED]})
    engine.applied.add(GAME_MODE)
    panel = open_history(ui, app, rows=1)
    assert not panel.drift_shown
    assert not panel.view_bar.winfo_ismapped()
    panel.set_view(VIEW_CHANGED)
    assert panel.view == VIEW_ALL, "nothing to show"
    [row] = panel.rows
    assert row.drift_label is None and row.keep_button is None
    assert not engine.calls_named("drift")
    assert app.errors == []


def test_a_failed_drift_read_keeps_the_all_view(make_app: Any, ui: Window) -> None:
    s = ui.support
    app, engine = make_app(elevated=True)
    engine.applied.add(GAME_MODE)

    def broken() -> dict[str, Any]:
        engine._record("drift")
        raise RuntimeError("journal is locked")

    engine.drift = broken  # type: ignore[method-assign]
    panel = open_history(ui, app, rows=1)
    assert not panel.drift_shown and not panel.view_bar.winfo_ismapped()
    failed = "⚠ Cairn could not check which changes still hold: journal is locked"
    assert panel.summary.cget("text").endswith(f"\n{failed}")
    assert panel.summary.cget("text_color") == s.theme.WARNING
    assert app.errors == []


def test_startup_entry_turned_on_again_outside_cairn(make_app: Any, ui: Window) -> None:
    s, texts = ui.support, ui.app_module
    from .fake_engine import APPROVED_ROOT

    run = f"{APPROVED_ROOT}\\Run"
    discord = {"hive": "HKCU", "key_path": run, "value_name": "Discord"}
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
    app, engine = make_app(elevated=False, drift_entries=[turned_on])
    engine.startup_set_enabled("user_run:Discord", False)
    panel = open_history(ui, app, rows=1)
    panel.set_view(VIEW_CHANGED)
    [row] = panel.rows
    assert row.change.title == "Startup app: Discord (HKCU Run)"
    assert row.detail_label.cget("text") == STARTUP_ON_AGAIN_TEXT
    assert row.apply_again_button is None, "not a catalog tweak"
    assert row.undo_button.cget("state") == "normal", "a per-user entry"

    row.keep_button.invoke()
    dialog = s.next_dialog(app)
    assert texts.KEEP_BACK_TEXT in s.dialog_text(dialog), "the entry is back to its original"
    s.pump(app, 0.3)
    dialog.confirm_button.invoke()
    s.pump(app, 5.0, until=lambda: panel.row_count == 0 and s.idle(app))
    assert engine.calls_named("forget_targets") == [(full_filter(discord),)]
    assert engine._startup_disabled == set()
    assert app.errors == []


def full_filter(record: dict[str, str]) -> dict[str, Any]:
    """The filter Keep as is sends for one registry record."""
    return {
        "registry": [record],
        "services": [],
        "appx_families": [],
        "power": False,
        "scheduled_tasks": [],
        "dns": [],
        "task_definitions": [],
    }


def test_keep_as_is_of_values_an_earlier_version_set(make_app: Any, ui: Window) -> None:
    s, texts = ui.support, ui.app_module
    app, engine = make_app(elevated=True, former={"privacy.advertising_id": [FORMER]})
    panel = open_history(ui, app, rows=1)
    [row] = panel.rows
    assert row.change.title == "Advertising Id", "titled with the tweak through the catalog"
    assert row.change.details == [f"{FORMER['target']}  ·  was not set  ·  {FORMER_DETAIL}"]
    assert row.keep_button is not None and row.keep_button.cget("text") == "Keep as is"
    assert row.drift_label is None
    row.keep_button.invoke()
    dialog = s.next_dialog(app)
    text = s.dialog_text(dialog)
    assert texts.KEEP_LOSES_TEXT in text
    assert f"• {FORMER['target']}: original not set" in text
    assert dialog.confirm_button.cget("fg_color") == s.theme.CRITICAL, (
        "the original can no longer be restored"
    )
    s.pump(app, 0.3)
    dialog.confirm_button.invoke()
    s.pump(app, 5.0, until=lambda: panel.row_count == 0 and s.idle(app))
    assert engine.calls_named("forget_targets") == [
        (full_filter({"hive": "HKLM", "key_path": "SOFTWARE\\Policies\\Test", "value_name": "Former"}),)
    ]
    assert not engine.former_records
    assert app.status_message.cget("text") == texts.KEEP_DONE_TEXT.format(title="Advertising Id")
    assert app.errors == []


def test_keep_as_is_of_some_values_says_the_rest_is_still_tracked(make_app: Any, ui: Window) -> None:
    s, texts = ui.support, ui.app_module
    # Game Mode's own value and one an earlier version set: keeping the earlier version's value
    # as it is leaves Game Mode's own value tracked.
    app, engine = make_app(elevated=True, former={GAME_MODE: [FORMER]})
    engine.applied.add(GAME_MODE)
    panel = open_history(ui, app, rows=1)
    [row] = panel.rows
    assert len(row.change.details) == 2 and len(row.change.former) == 1
    row.keep_button.invoke()
    dialog = s.next_dialog(app)
    text = s.dialog_text(dialog)
    assert texts.KEEP_LOSES_SOME_TEXT in text and texts.KEEP_LOSES_TEXT not in text
    s.pump(app, 0.3)
    dialog.confirm_button.invoke()
    s.pump(app, 5.0, until=lambda: not engine.former_records and s.idle(app))
    status = app.status_message.cget("text")
    assert status == texts.KEEP_DONE_SOME_TEXT.format(title="Game Mode")
    assert status == (
        "✓ Game Mode: Cairn no longer tracks the values you kept and still tracks the rest of this "
        "change. Nothing on this PC was changed."
    )
    assert app.status_message.cget("text_color") == s.theme.GOOD
    s.pump(app, 5.0, until=lambda: panel.row_count == 1 and not panel.rows[0].change.former and s.idle(app))
    [row] = panel.rows
    assert row.change.title == "Game Mode" and len(row.change.details) == 1
    assert engine.applied == {GAME_MODE}, "Game Mode's own value is still tracked"
    assert app.errors == []


def test_keep_as_is_of_a_partly_changed_change_names_the_rest(make_app: Any, ui: Window) -> None:
    s, texts = ui.support, ui.app_module
    from .fake_engine import CEIP_TASKS

    ceip = "privacy.ceip_tasks"
    # One of the tweak's two tasks was turned on again; Keep as is lets go of that one only.
    turned_on = {
        "target": f"scheduled task {CEIP_TASKS[0]}",
        "label": "task Consolidator",
        "expected": "disabled",
        "found": "enabled",
        "original": "enabled",
        "back_to_original": True,
    }
    app, engine = make_app(
        elevated=True,
        extra_tweaks=[(ceip, "privacy", True)],
        scheduled_task_tweaks={ceip: CEIP_TASKS},
        drift={ceip: [turned_on]},
    )
    engine.applied.add(ceip)
    panel = open_history(ui, app, rows=1)
    panel.set_view(VIEW_CHANGED)
    [row] = panel.rows
    assert len(row.change.details) == 2 and len(row.change.drift) == 1
    row.keep_button.invoke()
    dialog = s.next_dialog(app)
    text = s.dialog_text(dialog)
    assert texts.KEEP_BACK_SOME_TEXT in text and texts.KEEP_BACK_TEXT not in text
    assert dialog.confirm_button.cget("fg_color") == s.theme.ACCENT
    s.pump(app, 0.3)
    dialog._cancel()
    s.pump(app, 0.2)
    assert not engine.calls_named("forget_targets")
    assert app.errors == []


@pytest.mark.parametrize("view", [VIEW_ALL, VIEW_CHANGED])
def test_history_fits_with_the_changed_view(make_app: Any, ui: Window, view: str) -> None:
    s = ui.support
    from .fake_engine import APPROVED_ROOT

    run = f"{APPROVED_ROOT}\\Run"
    long_label = {
        **CHANGED,
        "target": "HKLM\\Test\\performance.sysmain",
        "label": "service SysMain",
        "found": "Automatic",
    }
    turned_on = {
        "item": None,
        "kind": "registry",
        "mismatch": {**BACK, "target": f"HKCU\\{run}\\Discord", "label": "Discord", "found": "on"},
        "filter": {"registry": [{"hive": "HKCU", "key_path": run, "value_name": "Discord"}]},
        "recorded_at": "2026-09-25T10:00:00+00:00",
    }
    app, engine = make_app(
        elevated=True,
        drift={GAME_MODE: [CHANGED], "performance.sysmain": [long_label]},
        drift_entries=[turned_on],
        former={"privacy.advertising_id": [FORMER]},
    )
    engine.applied.update({GAME_MODE, "performance.sysmain"})
    engine.startup_set_enabled("user_run:Discord", False)
    panel = open_history(ui, app, rows=4)
    panel.set_view(view)
    if view == VIEW_CHANGED:
        assert panel.row_count == 3
        assert panel.summary.cget("text") == changed_summary(3)
    s.assert_section_fits(app, "History")
    for width, height in s.FIT_SIZES:
        s.resize_to(app, width, height)
        for row in panel.rows:
            buttons = [b for b in (row.apply_again_button, row.undo_button, row.keep_button) if b is not None]
            for button in buttons:
                right = button.winfo_rootx() + button.winfo_width()
                assert right <= row.winfo_rootx() + row.winfo_width() + 1, (
                    f"{row.change.title} at {width}x{height}"
                )
    assert app.errors == []
