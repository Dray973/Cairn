"""History grouping: journal records to tweak titles, Startup entries, the app-permission records
earlier builds left and the scheduled tasks Cairn registered; and the change rows' layout.

The grouping tests exercise pure functions, so they need neither a window nor a native module;
the layout test lays out the History section of a window with the FakeEngine.
"""

from __future__ import annotations

from typing import Any

from optimizer.widgets.history import (
    PERMISSION_KIND,
    STORE_KEYS,
    SWITCH_TITLES,
    group_changes,
    package_name,
    packaged_task_names,
    permission_title,
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
