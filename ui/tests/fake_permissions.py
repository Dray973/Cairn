"""App-permissions part of FakeEngine: `permissions_list` (the guide) and `permissions_set` (refused),
plus the journal hooks FakeEngine calls for the permission records earlier builds left.

Mixed into `FakeEngine`, which calls `_init_permissions` from its constructor. Options:

- `permission_recent`: desktop programs Windows recorded using a device, as (capability, path, last
  used, in use) tuples; default `PERMISSION_RECENT`.
- `permission_warnings`: the guide's read warnings; default none.
- `permission_error`: `permissions_list` raises RuntimeError with this text.
- `permission_recorded`: entry id -> state; journal records an earlier build left for that entry
  (its `Value` and `LastSetTime`, and for "location:device" also the location sensor's override),
  with the state as their baseline.

The guide's pages and order are the engine's. `permissions_set` raises RuntimeError with
`CHANGE_REFUSED`, the engine's text, for every request and records nothing. FakeEngine's journal
summary, export and reverts read the permission records through the hooks; an undo removes them.
"""

from __future__ import annotations

from typing import TYPE_CHECKING, Any, NoReturn

from optimizer import APP_NAME

USER_STORE = "Software\\Microsoft\\Windows\\CurrentVersion\\CapabilityAccessManager\\ConsentStore"
DEVICE_STORE = "SOFTWARE\\Microsoft\\Windows\\CurrentVersion\\CapabilityAccessManager\\ConsentStore"
LOCATION_SENSOR = (
    "SOFTWARE\\Microsoft\\Windows NT\\CurrentVersion\\Sensor\\Overrides"
    "\\{BFA794E4-F964-4FDB-90F6-51056BFE4B44}"
)
SENSOR_VALUE = "SensorPermissionState"
STORE_KEYS = {"camera": "webcam", "microphone": "microphone", "location": "location"}
CAPABILITIES = ("camera", "microphone", "location")
CAPABILITY_LABELS = {"camera": "Camera", "microphone": "Microphone", "location": "Location"}
SWITCH_SCOPES = ("device", "apps", "desktop_apps")
# capability -> the Windows Settings page the engine names for it.
SETTINGS_PAGES = {
    "camera": "ms-settings:privacy-webcam",
    "microphone": "ms-settings:privacy-microphone",
    "location": "ms-settings:privacy-location",
}
# (capability, path, last_used, in_use)
PERMISSION_RECENT = (
    ("camera", "C:\\Program Files\\Contoso\\Meet\\meet.exe", "2026-09-25T10:00:00Z", False),
    ("location", "C:\\Program Files\\Fabrikam\\Maps\\maps.exe", "2026-09-26T08:30:00Z", True),
)
# The engine's answer to every permission change and plan.
CHANGE_REFUSED = (
    f"{APP_NAME} does not change app permissions: Windows 11 manages camera, microphone and location "
    "permissions itself, in Settings › Privacy & security, and on this version of Windows an app like "
    f"{APP_NAME} cannot change them. Nothing was changed."
)
# Stored `Value` text of each state.
STORED_TEXT = {"allow": "Allow", "deny": "Deny", "prompt": "Prompt"}
RECORDED_AT = "2026-09-25T10:00:00+00:00"
_CONSENT_STORE = "\\capabilityaccessmanager\\consentstore\\"


def parse_permission_id(entry_id: str) -> tuple[str, str, str | None]:
    """(capability, scope, family) of an entry id as earlier builds named them
    (`<capability>:<device|apps|desktop_apps>` or `<capability>:app:<family>`); ValueError for a
    malformed one."""

    def malformed(why: str) -> ValueError:
        return ValueError(f"malformed permission id {entry_id!r}: {why}")

    parts = entry_id.split(":", 2)
    capability = parts[0].strip().lower()
    capability = {"webcam": "camera"}.get(capability, capability)
    if capability not in CAPABILITIES:
        raise malformed("unknown capability")
    scope = parts[1].strip().lower() if len(parts) > 1 else ""
    if scope not in (*SWITCH_SCOPES, "app"):
        raise malformed("unknown scope")
    family = parts[2] if len(parts) > 2 else None
    if scope != "app":
        if family is not None:
            raise malformed("only app ids name a package family")
        return capability, scope, None
    if not family or any(c in "\\/:" or ord(c) < 32 for c in family) or family.lower() == "nonpackaged":
        raise malformed("not a package family")
    return capability, scope, family


def entry_id_of(capability: str, scope: str, family: str | None = None) -> str:
    return f"{capability}:app:{family}" if scope == "app" else f"{capability}:{scope}"


def permission_key(capability: str, scope: str, family: str | None = None) -> tuple[str, str]:
    """(hive, key path) of the key that holds the entry's `Value`."""
    store = STORE_KEYS[capability]
    if scope == "device":
        return "HKLM", f"{DEVICE_STORE}\\{store}"
    if scope == "apps":
        return "HKCU", f"{USER_STORE}\\{store}"
    if scope == "desktop_apps":
        return "HKCU", f"{USER_STORE}\\{store}\\NonPackaged"
    return "HKCU", f"{USER_STORE}\\{store}\\{family}"


def sensor_target() -> str:
    """Registry target of the location sensor's override."""
    return f"HKLM\\{LOCATION_SENSOR}\\{SENSOR_VALUE}"


def _dword_text(value: int) -> str:
    """A DWORD as the engine's journal texts show it."""
    return f"0x{value:08x} ({value})"


class FakePermissions:
    """The permissions guide of an in-memory PC, and the permission records of earlier builds in
    its journal."""

    if TYPE_CHECKING:

        def _record(self, name: str, *args: Any) -> None: ...

    def _init_permissions(self, options: dict[str, Any]) -> None:
        """Pops the permission options (see the module docstring) from `options`."""
        self._permission_recent = [tuple(r) for r in options.pop("permission_recent", PERMISSION_RECENT)]
        self._permission_warnings = [str(w) for w in options.pop("permission_warnings", None) or []]
        self._permission_error: str | None = options.pop("permission_error", None)
        # entry id -> the state before the earlier build's change, which its records hold.
        self._permission_recorded: dict[str, str] = {}
        # The location sensor's override before that change ("on" or "off"), when it is recorded.
        self._sensor_recorded: str | None = None
        for entry_id, original in dict(options.pop("permission_recorded", None) or {}).items():
            entry_id = entry_id_of(*parse_permission_id(entry_id))
            self._permission_recorded[entry_id] = original
            if entry_id == "location:device":
                self._sensor_recorded = "off" if original == "deny" else "on"

    # -- module surface ------------------------------------------------------------------

    def permissions_list(self) -> dict[str, Any]:
        self._record("permissions_list")
        if self._permission_error is not None:
            raise RuntimeError(self._permission_error)
        capabilities = []
        for capability in CAPABILITIES:
            recent = sorted(
                (
                    {"path": path, "last_used": last_used, "in_use": in_use}
                    for c, path, last_used, in_use in self._permission_recent
                    if c == capability
                ),
                key=lambda r: r["last_used"] or "",
                reverse=True,
            )
            capabilities.append(
                {
                    "capability": capability,
                    "label": CAPABILITY_LABELS[capability],
                    "settings_uri": SETTINGS_PAGES[capability],
                    "recent_desktop_apps": recent,
                }
            )
        return {"capabilities": capabilities, "warnings": list(self._permission_warnings)}

    def permissions_set(
        self, id: str, allow: bool, restore_point: str = "skip", dry_run: bool = False
    ) -> NoReturn:
        self._record("permissions_set", id, allow, restore_point, dry_run)
        raise RuntimeError(CHANGE_REFUSED)

    # -- journal hooks -------------------------------------------------------------------

    def _permission_active_count(self) -> int:
        """Active journal records of earlier permission changes (Value and LastSetTime each
        count, and the location sensor's override)."""
        return 2 * len(self._permission_recorded) + (self._sensor_recorded is not None)

    def _permission_export(self) -> list[dict[str, Any]]:
        """Registry rows of the journal export for earlier permission changes: the `Value` (its
        state before the change) and the `LastSetTime` of each recorded entry, then the location
        sensor's override when it is recorded."""
        rows = []

        def row(hive: str, key: str, value_name: str, previous: dict[str, Any] | None) -> dict[str, Any]:
            return {
                "id": 0,
                "session_id": 1,
                "recorded_at": RECORDED_AT,
                "target": f"{hive}\\{key}\\{value_name}",
                "hive": hive,
                "key_path": key,
                "value_name": value_name,
                "key_existed": True,
                "value_existed": previous is not None,
                "original": previous,
                "active": True,
                "reverted_at": None,
            }

        for entry_id, original in self._permission_recorded.items():
            hive, key = permission_key(*parse_permission_id(entry_id))
            stored = STORED_TEXT.get(original)
            rows.append(row(hive, key, "Value", None if stored is None else {"type": "Sz", "value": stored}))
            rows.append(row(hive, key, "LastSetTime", None))
        if self._sensor_recorded is not None:
            value = 1 if self._sensor_recorded == "on" else 0
            rows.append(row("HKLM", LOCATION_SENSOR, SENSOR_VALUE, {"type": "Dword", "value": value}))
        return rows

    def _permission_revert(
        self, targets: list[dict[str, Any]] | None, dry_run: bool
    ) -> tuple[list[str], int, list[dict[str, Any]]]:
        """Restores the permission records among the registry `targets` (None: all of them).
        Every consent-store target and the location sensor's override belong to this fake,
        recorded or not. Returns the actions, the number of restored values and the targets
        this fake does not own."""
        recorded = {}
        for entry_id in self._permission_recorded:
            hive, key = permission_key(*parse_permission_id(entry_id))
            recorded[(hive, key.lower())] = entry_id
        selected: dict[str, list[str]] = {}
        remaining: list[dict[str, Any]] = []
        sensor = False
        if targets is None:
            selected = {entry_id: ["Value", "LastSetTime"] for entry_id in self._permission_recorded}
            sensor = self._sensor_recorded is not None
        else:
            for target in targets:
                key_path = str(target["key_path"]).lower()
                if target["hive"] == "HKLM" and key_path == LOCATION_SENSOR.lower():
                    sensor = sensor or self._sensor_recorded is not None
                    continue
                if _CONSENT_STORE not in key_path:
                    remaining.append(target)
                    continue
                entry_id = recorded.get((target["hive"], key_path))
                if entry_id is not None:
                    selected.setdefault(entry_id, []).append(str(target["value_name"]))
        actions: list[str] = []
        for entry_id, value_names in selected.items():
            original = self._permission_recorded[entry_id]
            hive, key = permission_key(*parse_permission_id(entry_id))
            for value_name in value_names:
                target = f"{hive}\\{key}\\{value_name}"
                stored = STORED_TEXT.get(original) if value_name.lower() == "value" else None
                actions.append(f'restore "{stored}": {target}' if stored else f"delete value: {target}")
            if not dry_run:
                del self._permission_recorded[entry_id]
        if sensor and self._sensor_recorded is not None:
            actions.append(f"restore {_dword_text(int(self._sensor_recorded == 'on'))}: {sensor_target()}")
            if not dry_run:
                self._sensor_recorded = None
        return actions, 0 if dry_run else len(actions), remaining
