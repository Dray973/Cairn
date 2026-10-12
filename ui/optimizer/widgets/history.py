"""History section: pending (revertible) changes grouped per tweak, the changes that no longer
hold, and the audit log."""

from __future__ import annotations

import re
import tkinter as tk
from collections.abc import Callable, Collection, Hashable, Iterable, Mapping, Sequence
from datetime import datetime
from typing import Any

import customtkinter as ctk

from .. import theme
from .controls import MIN_WRAP, PANEL_WRAP, ROW_WRAP, WRAP_MARGIN, fitted_wrap

# Horizontal padding of a change row's texts, and the room its detail lines keep free inside
# the row, in CustomTkinter units.
ROW_TEXT_PADX = 12
DETAIL_INSET = 2 * ROW_TEXT_PADX + WRAP_MARGIN
# Width of the changes card that its summary does not get (padding and margin).
SUMMARY_INSET = 2 * 14 + WRAP_MARGIN

# The two views of the active changes: every change, or the changes that no longer hold.
VIEW_ALL = "all"
VIEW_CHANGED = "changed"
ALL_VIEW_LABEL = "All"
CHANGED_VIEW_LABEL = "Changed ({n})"
# What Keep as is stops tracking: the records that no longer hold, or the values an earlier
# version of Cairn set that this version no longer sets.
KEEP_DRIFT = "drift"
KEEP_FORMER = "former"

ALL_HOLD_TEXT = "✓ Every recorded change still holds."
# The Changed view's summary when the drift check could not read some records: values Cairn
# does not know it wrote (recorded by earlier builds), values Windows changes itself, targets
# that could not be read, and the records it never reads (`NEVER_CHECKED_KINDS`).
SOME_UNCHECKED_ONE = "✓ Every change Cairn could check still holds. Cairn could not check 1 recorded change."
SOME_UNCHECKED_MANY = (
    "✓ Every change Cairn could check still holds. Cairn could not check {n} recorded changes."
)
NONE_CHECKED_ONE = "Cairn could not check whether 1 recorded change still holds."
NONE_CHECKED_MANY = "Cairn could not check whether {n} recorded changes still hold."
# The same summaries when every unchecked record was made by an earlier version of Cairn that
# did not store what it set, so the reason can be named.
SOME_UNCHECKED_OLD_ONE = (
    "✓ Every change Cairn could check still holds. Cairn could not check 1 change made by an earlier "
    "version of Cairn, which did not record what it set."
)
SOME_UNCHECKED_OLD_MANY = (
    "✓ Every change Cairn could check still holds. Cairn could not check {n} changes made by an earlier "
    "version of Cairn, which did not record what it set."
)
NONE_CHECKED_OLD_ONE = (
    "Cairn could not check 1 change made by an earlier version of Cairn, which did not record what it set."
)
NONE_CHECKED_OLD_MANY = (
    "Cairn could not check {n} changes made by an earlier version of Cairn, which did not record what it set."
)
CHANGED_SUMMARY_ONE = (
    "⚠ 1 change no longer holds: Windows or another program changed it after Cairn did. Cairn never "
    "sets it again on its own."
)
CHANGED_SUMMARY_MANY = (
    "⚠ {n} changes no longer hold: Windows or another program changed them after Cairn did. Cairn "
    "never sets them again on its own."
)
DRIFT_FAILED_TEXT = "⚠ Cairn could not check which changes still hold: {error}"
# The All view's line on a change that no longer holds.
SEE_CHANGED_TEXT = "⚠ Changed since Cairn applied it; see Changed."
# A startup entry's line in the Changed view, by the state its value has now.
STARTUP_ON_AGAIN_TEXT = "⚠ Turned on again outside Cairn, in Settings or Task Manager."
STARTUP_REMOVED_TEXT = "⚠ Removed outside Cairn."
# Detail part of a value an earlier version of Cairn set for a tweak.
FORMER_DETAIL = "set by an earlier version of Cairn"
# Startup values as the engine reads them back: switched on, and absent.
STARTUP_ON = "on"
NOT_SET = "not set"


def _font(size: int, weight: str = "normal") -> ctk.CTkFont:
    return ctk.CTkFont(family=theme.FONT_FAMILY, size=size, weight=weight)


def local_time(stamp: str) -> str:
    try:
        return datetime.fromisoformat(stamp).astimezone().strftime("%Y-%m-%d %H:%M")
    except (TypeError, ValueError):
        return stamp or ""


def value_text(original: dict[str, Any] | None) -> str:
    if original is None:
        return "not set"
    kind, value = original.get("type"), original.get("value")
    if kind in ("Sz", "ExpandSz"):
        return f'"{value}"'
    if kind == "Binary":
        return f"<{len(value or [])} bytes>"
    if kind == "MultiSz":
        return ", ".join(value or [])
    return str(value)


def package_name(family: str) -> str:
    return family.rsplit("_", 1)[0] if "_" in family else family


# (hive, StartupApproved subkey in lower case) -> the Startup section's location text of the
# Run-key and Startup-folder entries that subkey switches.
_STARTUP_LOCATIONS = {
    ("HKCU", "run"): "HKCU Run",
    ("HKLM", "run"): "HKLM Run",
    ("HKLM", "run32"): "HKLM Run (32-bit)",
    ("HKCU", "startupfolder"): "Startup folder",
    ("HKLM", "startupfolder"): "Startup folder (all users)",
}
_STARTUP_APPROVED = "\\startupapproved\\"
_PACKAGED_TASKS = "\\appmodel\\systemappdata\\"
# Source key and location text of packaged-app startup tasks in the startup list.
PACKAGED_TASK_SOURCE = "packaged_task"
PACKAGED_TASK_LOCATION = "Packaged app"


# App permissions, for the records of permission changes earlier builds made (the Permissions
# section is a read-only guide and makes none): the consent store key of each capability, its
# label, and the titles of the three switches as Windows Settings names them.
_CONSENT_STORE = "\\capabilityaccessmanager\\consentstore\\"
STORE_KEYS = {"webcam": "camera", "microphone": "microphone", "location": "location"}
CAPABILITY_LABELS = {"camera": "Camera", "microphone": "Microphone", "location": "Location"}
SWITCH_TITLES = {
    ("camera", "device"): "Camera access on this PC",
    ("camera", "apps"): "Let apps access your camera",
    ("camera", "desktop_apps"): "Let desktop apps access your camera",
    ("microphone", "device"): "Microphone access on this PC",
    ("microphone", "apps"): "Let apps access your microphone",
    ("microphone", "desktop_apps"): "Let desktop apps access your microphone",
    ("location", "device"): "Location services on this PC",
    ("location", "apps"): "Let apps access your location",
    ("location", "desktop_apps"): "Let desktop apps access your location",
}
PERMISSION_KIND = "permission"
_NON_PACKAGED = "nonpackaged"
# The location sensor's override (HKLM), written with the location switch for the whole PC as
# Windows Settings writes it; its record undoes together with that switch's records.
_LOCATION_SENSOR = (
    "software\\microsoft\\windows nt\\currentversion\\sensor\\overrides"
    "\\{bfa794e4-f964-4fdb-90f6-51056bfe4b44}"
)

# Scheduled tasks Cairn registers; the maintenance task is \Cairn\Maintenance-<user SID>.
TASK_DEFINITION_KIND = "task_definition"
_MAINTENANCE_FOLDER = "\\cairn"
_MAINTENANCE_PREFIX = "maintenance-"
_SID_SUFFIX = re.compile(r"-(S-1-5-[0-9-]+)$", re.IGNORECASE)

# Journal export lists whose records the drift check never reads and does not count among the
# records it could not read: Store apps Cairn removed and scheduled tasks Cairn registered.
NEVER_CHECKED_KINDS = ("appx", "task_definitions")


def permission_title(hive: str, key_path: str, names: Mapping[str, str] | None = None) -> str | None:
    """History title of a record written by an app-permission change of an earlier build, else
    None.

    `...\\ConsentStore\\<store key>` is the switch for the whole PC (HKLM) or for Store apps
    (HKCU), `...\\<store key>\\NonPackaged` the desktop-apps switch and
    `...\\<store key>\\<package family>` one Store app, titled with its display name from
    `names` (package family in lower case -> name), else its package name. The location
    sensor's override under HKLM is the location switch for the whole PC. Matching ignores
    case; an unknown store key is used as the label itself.
    """
    record = permission_record(hive, key_path, names)
    return None if record is None else record[1]


def permission_record(
    hive: str, key_path: str, names: Mapping[str, str] | None = None
) -> tuple[Hashable, str] | None:
    """(group key, title) of a record written by an app-permission change of an earlier build,
    else None.

    The records of one entry share the key, so one Undo restores them together: the `Value`
    and `LastSetTime` of a consent-store key, and for the location switch for the whole PC
    also the location sensor's override. Titles as in `permission_title`.
    """
    lowered = key_path.lower().strip("\\")
    if hive.upper() == "HKLM" and lowered == _LOCATION_SENSOR:
        return (PERMISSION_KIND, "HKLM", "location", ""), SWITCH_TITLES[("location", "device")]
    at = lowered.find(_CONSENT_STORE)
    if at < 0:
        return None
    rest = key_path.strip("\\")[at + len(_CONSENT_STORE) :]
    parts = [p for p in rest.split("\\") if p]
    if not parts or len(parts) > 2:
        return None
    store = parts[0]
    capability = STORE_KEYS.get(store.lower())
    label = CAPABILITY_LABELS[capability] if capability else store
    key = (PERMISSION_KIND, hive.upper(), store.lower(), parts[1].lower() if len(parts) == 2 else "")

    def switch(scope: str) -> str:
        if capability:
            return SWITCH_TITLES[(capability, scope)]
        return {
            "device": f"{store} access on this PC",
            "apps": f"Let apps access your {store}",
            "desktop_apps": f"Let desktop apps access your {store}",
        }[scope]

    if len(parts) == 1:
        return key, switch("device" if hive.upper() == "HKLM" else "apps")
    if parts[1].lower() == _NON_PACKAGED:
        return key, switch("desktop_apps")
    family = parts[1]
    name = (names or {}).get(family.lower()) or package_name(family)
    return key, f"{label} permission: {name}"


def task_display_name(path: str) -> str:
    """A task path as History shows it: a user SID at the end of the task's name becomes
    "(your account)", so `\\Cairn\\Maintenance-S-1-5-21-…` reads `\\Cairn\\Maintenance (your account)`."""
    return _SID_SUFFIX.sub(" (your account)", path)


def task_definition_title(path: str) -> str:
    """Title of a scheduled task Cairn registered: the maintenance task has its own."""
    folder, _, name = path.rpartition("\\")
    if folder.lower() == _MAINTENANCE_FOLDER and name.lower().startswith(_MAINTENANCE_PREFIX):
        return "Scheduled maintenance"
    return f"Task created by Cairn: {task_display_name(name)}"


def packaged_task_names(entries: Iterable[dict[str, Any]]) -> dict[str, str]:
    """Display names of the packaged-app startup tasks in a startup list.

    Keyed by `<package family>\\<task id>` in lower case, the part of the entry id after
    `packaged_task:` and the last two components of the task's registry key.
    """
    names: dict[str, str] = {}
    for entry in entries:
        source, _, key = str(entry.get("id", "")).partition(":")
        if source == PACKAGED_TASK_SOURCE and key:
            names[key.lower()] = entry["name"]
    return names


def startup_record(
    hive: str, key_path: str, value_name: str, names: Mapping[str, str] | None = None
) -> tuple[str, str] | None:
    """(entry name, location) of a journal record written by a startup toggle, else None.

    Names and locations are those the Startup section shows. A packaged-app task takes its
    display name from `names` (see `packaged_task_names`); without one it gets the name
    the Startup section uses when the package cannot be read: `<package name> (<task id>)`.
    """
    lowered = key_path.lower()
    if _STARTUP_APPROVED in lowered:
        subkey = key_path.rsplit("\\", 1)[-1]
        return value_name, _STARTUP_LOCATIONS.get((hive, subkey.lower()), f"{hive} {subkey}")
    if _PACKAGED_TASKS in lowered:
        # ...\SystemAppData\<package family>\<task id>, value "State".
        parts = key_path.split("\\")
        if len(parts) >= 2:
            family, task_id = parts[-2], parts[-1]
            name = (names or {}).get(f"{family}\\{task_id}".lower())
            return name or f"{package_name(family)} ({task_id})", PACKAGED_TASK_LOCATION
    return None


def empty_filter() -> dict[str, Any]:
    """A `revert_targets` / `forget_targets` filter that selects nothing."""
    return {
        "registry": [],
        "services": [],
        "appx_families": [],
        "power": False,
        "scheduled_tasks": [],
        "dns": [],
        "task_definitions": [],
    }


class ChangeGroup:
    """Journal records that belong to one tweak (or one app, service or startup entry).

    `filter` selects every record of the change. `drift` lists the mismatches of the records
    that no longer hold what Cairn set (see `attach_drift`), `item_ids` the catalog tweaks
    they belong to and `drift_filter` exactly those records. `former` lists (target, original)
    of the values an earlier version of Cairn set for the tweak that this version no longer
    sets, and `former_filter` selects them.
    """

    def __init__(self, title: str, kind: str) -> None:
        self.title = title
        self.kind = kind
        self.details: list[str] = []
        self.recorded_at = ""
        self.filter: dict[str, Any] = empty_filter()
        self.drift: list[dict[str, Any]] = []
        self.item_ids: list[str] = []
        self.drift_filter: dict[str, Any] = empty_filter()
        self.former: list[tuple[str, str]] = []
        self.former_filter: dict[str, Any] = empty_filter()

    def add(self, detail: str, recorded_at: str) -> None:
        self.details.append(detail)
        self.recorded_at = max(self.recorded_at, recorded_at)

    @property
    def needs_admin(self) -> bool:
        """Undoing the group needs an elevated process.

        Only per-user registry values outside `Software\\Policies` (which is read-only
        for the user) can be restored by a standard user; everything else is treated as
        needing administrator rights.
        """
        f = self.filter
        if f["services"] or f["appx_families"] or f["power"] or f.get("scheduled_tasks") or f.get("dns"):
            return True
        if f.get("task_definitions"):
            return True
        return any(
            r["hive"] != "HKCU" or r["key_path"].lower().startswith("software\\policies")
            for r in f["registry"]
        )


def _guid_key(guid: str) -> str:
    """Interface GUID compared the way the engine compares it: braces and case ignored."""
    return guid.strip().strip("{}").lower()


def _catalog_title(titles: Mapping[str, str], target: str) -> str | None:
    """Title of the catalog item that owns `target`: an exact match first, then ignoring case."""
    if target in titles:
        return titles[target]
    lowered = target.lower()
    return next((title for key, title in titles.items() if key.lower() == lowered), None)


def group_changes(
    export: dict[str, Any],
    titles: dict[str, str],
    startup_names: Mapping[str, str] | None = None,
    permission_names: Mapping[str, str] | None = None,
    former_targets: Collection[str] = (),
) -> list[ChangeGroup]:
    """Groups the export's active records by the catalog item they belong to.

    `titles` maps a catalog target string (for example `HKLM\\...\\AllowTelemetry`,
    `service DiagTrack` or `scheduled task \\Microsoft\\...\\Proxy`) to the title of the
    tweak that owns it. Records of different kinds that belong to one tweak share its
    group. Startup records are grouped per registry target (hive, key path and value name),
    so each group matches one entry of the Startup section and its Undo restores only that
    entry; the group is titled with that entry's name and location. `startup_names`
    supplies the display names of packaged-app tasks, which the journal does not store.
    App-permission records are grouped per entry, so the `Value` and `LastSetTime` of one
    consent-store key (and the location sensor's override with the location switch for the
    whole PC) undo together; `permission_names` supplies the Store apps' display names
    (package family in lower case -> name). Windows Update settings are titled
    through `titles`. DNS records are grouped per adapter, so one Undo restores both address
    families. Scheduled tasks Cairn registered (task definitions) are one group each.
    `former_targets` are the registry targets (any case) of values earlier versions of Cairn
    set for a tweak and this version no longer sets; their records join the tweak's group
    (through `titles`), say so in their detail line and are listed in the group's `former`.
    Exports without some kinds of records (older engines) are grouped as far as they go.
    """
    groups: dict[Hashable, ChangeGroup] = {}
    former = {t.lower() for t in former_targets}

    def group(key: Hashable, title: str, kind: str) -> ChangeGroup:
        if key not in groups:
            groups[key] = ChangeGroup(title, kind)
        return groups[key]

    for rec in export.get("registry", []):
        if not rec.get("active"):
            continue
        target = rec["target"]
        startup = startup_record(rec["hive"], rec["key_path"], rec["value_name"], startup_names)
        permission = (
            None if startup is not None else permission_record(rec["hive"], rec["key_path"], permission_names)
        )
        if startup is not None:
            name, location = startup
            key = ("startup", rec["hive"], rec["key_path"].lower(), rec["value_name"].lower())
            g = group(key, f"Startup app: {name} ({location})", "startup")
        elif permission is not None:
            permission_key, permission_title_text = permission
            g = group(permission_key, permission_title_text, PERMISSION_KIND)
        else:
            title = titles.get(target, target)
            g = group(title, title, "setting")
        selection = {"hive": rec["hive"], "key_path": rec["key_path"], "value_name": rec["value_name"]}
        original = value_text(rec.get("original"))
        if target.lower() in former:
            g.add(f"{target}  ·  was {original}  ·  {FORMER_DETAIL}", rec["recorded_at"])
            g.former.append((target, original))
            g.former_filter["registry"].append(dict(selection))
        else:
            g.add(f"{target}  ·  was {original}", rec["recorded_at"])
        g.filter["registry"].append(selection)
    for rec in export.get("services", []):
        if not rec.get("active"):
            continue
        title = titles.get(f"service {rec['name']}", f"Service {rec['name']}")
        g = group(title, title, "service")
        state = "running" if rec["was_running"] else "stopped"
        g.add(f"service {rec['name']}  ·  was {rec['start_type']}, {state}", rec["recorded_at"])
        g.filter["services"].append(rec["name"])
    for rec in export.get("scheduled_tasks", []):
        if not rec.get("active"):
            continue
        target = rec.get("target") or f"scheduled task {rec['path']}"
        task_name = rec["path"].rsplit("\\", 1)[-1]
        title = _catalog_title(titles, target) or f"Scheduled task: {task_name}"
        # Keyed by title, so the tweak's other kinds of records join the same group.
        g = group(title, title, "scheduled_task")
        g.add(f"{target}  ·  was {'enabled' if rec['was_enabled'] else 'disabled'}", rec["recorded_at"])
        g.filter["scheduled_tasks"].append(rec["path"])
    for rec in export.get("appx", []):
        if not rec.get("active"):
            continue
        title = f"Removed app: {package_name(rec['package_family'])}"
        g = group(title, title, "app")
        g.add(rec["package_full_name"], rec["recorded_at"])
        g.filter["appx_families"].append(rec["package_family"])
    for rec in export.get("power", []):
        if not rec.get("active"):
            continue
        title = titles.get("power plan Ultimate Performance", "Power plan")
        g = group(title, title, "power")
        g.add(f"power plan  ·  was {rec['previous_scheme']}", rec["recorded_at"])
        g.filter["power"] = True
    for rec in export.get("dns", []):
        if not rec.get("active"):
            continue
        guid = rec["interface_guid"]
        g = group(("dns", _guid_key(guid)), f"DNS servers: {rec['adapter_name']}", "dns")
        family = "IPv4" if rec["family"] == "ipv4" else "IPv6"
        previous = ", ".join(rec.get("previous_servers") or []) or "automatic"
        g.add(f"{family} DNS  ·  was {previous}", rec["recorded_at"])
        # One GUID selects both address families.
        if all(_guid_key(known) != _guid_key(guid) for known in g.filter["dns"]):
            g.filter["dns"].append(guid)
    for rec in export.get("task_definitions", []):
        if not rec.get("active"):
            continue
        path = rec["path"]
        g = group((TASK_DEFINITION_KIND, path.lower()), task_definition_title(path), TASK_DEFINITION_KIND)
        g.add(f"Task Scheduler: {task_display_name(path)}  ·  created by Cairn", rec["recorded_at"])
        g.filter["task_definitions"].append(path)
    return sorted(groups.values(), key=lambda g: g.recorded_at, reverse=True)


def _registry_key(entry: Mapping[str, Any]) -> tuple[str, str, str]:
    """A filter's registry entry as the engine compares it: hive, key path and value name, case
    ignored."""
    return (
        str(entry.get("hive", "")).upper(),
        str(entry.get("key_path", "")).lower(),
        str(entry.get("value_name", "")).lower(),
    )


def _same_record(group_filter: Mapping[str, Any], selection: Mapping[str, Any]) -> bool:
    """Whether `group_filter` selects a record that `selection` (a drift entry's filter) selects:
    a registry value or a service name or task path ignoring case, the power plan, or an
    adapter's DNS servers by its GUID."""
    registry = {_registry_key(r) for r in group_filter.get("registry") or []}
    if any(_registry_key(r) in registry for r in selection.get("registry") or []):
        return True
    services = {str(s).lower() for s in group_filter.get("services") or []}
    if any(str(s).lower() in services for s in selection.get("services") or []):
        return True
    tasks = {str(p).lower() for p in group_filter.get("scheduled_tasks") or []}
    if any(str(p).lower() in tasks for p in selection.get("scheduled_tasks") or []):
        return True
    if selection.get("power") and group_filter.get("power"):
        return True
    guids = {_guid_key(str(g)) for g in group_filter.get("dns") or []}
    return any(_guid_key(str(g)) in guids for g in selection.get("dns") or [])


def selects_every_record(group_filter: Mapping[str, Any], selection: Mapping[str, Any]) -> bool:
    """Whether `selection` selects every record that `group_filter` selects, compared as
    `_same_record` compares them: a change whose filter has more than the selection keeps the
    rest tracked after Keep as is."""
    registry = {_registry_key(r) for r in selection.get("registry") or []}
    if any(_registry_key(r) not in registry for r in group_filter.get("registry") or []):
        return False
    for key in ("services", "scheduled_tasks", "appx_families", "task_definitions"):
        chosen = {str(v).lower() for v in selection.get(key) or []}
        if any(str(v).lower() not in chosen for v in group_filter.get(key) or []):
            return False
    if group_filter.get("power") and not selection.get("power"):
        return False
    guids = {_guid_key(str(g)) for g in selection.get("dns") or []}
    return all(_guid_key(str(g)) in guids for g in group_filter.get("dns") or [])


def _merge_filter(into: dict[str, Any], selection: Mapping[str, Any]) -> None:
    """Adds what `selection` selects to the filter `into`, without repeating an entry."""
    for key, value in selection.items():
        if key == "power":
            into["power"] = bool(into.get("power")) or bool(value)
        elif isinstance(value, list):
            known = into.setdefault(key, [])
            known.extend(v for v in value if v not in known)


def attach_drift(groups: Sequence[ChangeGroup], report: Mapping[str, Any] | None) -> int:
    """Adds each entry of a drift report (`engine.drift()`) to the change whose filter selects
    the same record: its mismatch to `drift`, its catalog tweak to `item_ids` and its filter to
    `drift_filter`. An entry no change selects is dropped (its record was undone between the
    two reads). Returns the number of entries added."""
    attached = 0
    for entry in (report or {}).get("entries") or []:
        selection = entry.get("filter") or {}
        change = next((g for g in groups if _same_record(g.filter, selection)), None)
        if change is None:
            continue
        change.drift.append(dict(entry.get("mismatch") or {}))
        item = entry.get("item")
        if item and item not in change.item_ids:
            change.item_ids.append(str(item))
        _merge_filter(change.drift_filter, selection)
        attached += 1
    return attached


def holds(count: int) -> str:
    """ "change no longer holds" or "changes no longer hold", for `count` changes."""
    return "change no longer holds" if count == 1 else "changes no longer hold"


def changed_summary(count: int) -> str:
    """The Changed view's summary for `count` changes that no longer hold."""
    if count == 1:
        return CHANGED_SUMMARY_ONE
    return CHANGED_SUMMARY_MANY.format(n=count)


def hold_summary(checked: int, unchecked: int, unchecked_no_value: int = 0) -> tuple[str, str]:
    """Text and colour of the Changed view's summary when it lists no change: the drift check
    read `checked` records and could not read `unchecked` ones, and only what it read is said
    to hold. When every unchecked record is one an earlier version of Cairn left without a
    written value (`unchecked_no_value == unchecked`), the summary names that reason."""
    if not unchecked:
        return ALL_HOLD_TEXT, theme.GOOD
    old = unchecked_no_value >= unchecked
    if not checked:
        if old:
            text = NONE_CHECKED_OLD_ONE if unchecked == 1 else NONE_CHECKED_OLD_MANY.format(n=unchecked)
        else:
            text = NONE_CHECKED_ONE if unchecked == 1 else NONE_CHECKED_MANY.format(n=unchecked)
        return text, theme.INK_MUTED
    if old:
        text = SOME_UNCHECKED_OLD_ONE if unchecked == 1 else SOME_UNCHECKED_OLD_MANY.format(n=unchecked)
    else:
        text = SOME_UNCHECKED_ONE if unchecked == 1 else SOME_UNCHECKED_MANY.format(n=unchecked)
    return text, theme.GOOD


def records_never_checked(export: Mapping[str, Any]) -> int:
    """Active records of a journal export that the drift check never reads and leaves out of its
    `unchecked` count: those of `NEVER_CHECKED_KINDS`."""
    return sum(1 for kind in NEVER_CHECKED_KINDS for rec in export.get(kind) or [] if rec.get("active"))


def mismatch_line(mismatch: Mapping[str, Any]) -> str:
    """One value that no longer holds: what it is now, what Cairn set and its original."""
    label, found, expected = (
        mismatch.get("label", ""),
        mismatch.get("found", ""),
        mismatch.get("expected", ""),
    )
    line = f"⚠ {label} is {found}  ·  Cairn set {expected}"
    original = mismatch.get("original")
    if original is not None:
        line += f"  ·  original {original}"
    if mismatch.get("back_to_original"):
        line += "  ·  back to the original"
    return line


def drift_lines(change: ChangeGroup) -> list[str]:
    """The Changed view's lines of a change: one per value that no longer holds; a startup
    entry says whether it was turned on again or removed outside Cairn."""
    lines = []
    for mismatch in change.drift:
        found = str(mismatch.get("found", ""))
        if change.kind == "startup" and found == STARTUP_ON:
            lines.append(STARTUP_ON_AGAIN_TEXT)
        elif change.kind == "startup" and found == NOT_SET:
            lines.append(STARTUP_REMOVED_TEXT)
        else:
            lines.append(mismatch_line(mismatch))
    return lines


def _button(
    master: tk.Misc, text: str, width: int, enabled: bool, command: Callable[[], None]
) -> ctk.CTkButton:
    return ctk.CTkButton(
        master,
        text=text,
        width=width,
        height=26,
        font=_font(11),
        fg_color=theme.BUTTON_NEUTRAL,
        hover_color=theme.BUTTON_NEUTRAL_HOVER,
        state="normal" if enabled else "disabled",
        command=command,
    )


class ChangeRow(ctk.CTkFrame):
    """One active change: its title, when it was recorded, its records and its actions.

    In the All view the records are detail lines and Undo sits beside the title; a change that
    no longer holds adds a line that points to the Changed view, and a change with values an
    earlier version of Cairn set offers Keep as is for those values. In the Changed view the
    lines say what each value that no longer holds is now, and a row of buttons offers Apply
    again (when a catalog tweak can be applied), Undo and Keep as is.

    The detail lines (`detail_label`) take the row's whole width and wrap with it, so a long
    registry path breaks onto further lines instead of being cut off.
    """

    # Widths of the buttons, in CustomTkinter units.
    APPLY_AGAIN_WIDTH = 96
    UNDO_WIDTH = 74
    KEEP_WIDTH = 90

    def __init__(
        self,
        master: tk.Misc,
        change: ChangeGroup,
        engine_ready: bool,
        elevated: bool,
        on_undo: Callable[[ChangeGroup], None],
        *,
        view: str = VIEW_ALL,
        on_apply_again: Callable[[ChangeGroup], None] | None = None,
        on_keep: Callable[[ChangeGroup, str], None] | None = None,
        can_apply_again: bool = False,
    ) -> None:
        super().__init__(master, fg_color=theme.SURFACE_RAISED, corner_radius=8)
        self.change = change
        self.view = view
        self.drift_label: ctk.CTkLabel | None = None
        self.apply_again_button: ctk.CTkButton | None = None
        self.keep_button: ctk.CTkButton | None = None
        self.grid_columnconfigure(0, weight=1)
        ctk.CTkLabel(self, text=change.title, font=_font(12, "bold"), text_color=theme.INK, anchor="w").grid(
            row=0, column=0, sticky="w", padx=ROW_TEXT_PADX, pady=(8, 0)
        )
        count = len(change.details)
        meta = f"{local_time(change.recorded_at)}  ·  {count} change{'s' if count != 1 else ''}"
        if engine_ready and change.needs_admin and not elevated:
            meta += "  ·  undoing it needs administrator rights"
        can_undo = engine_ready and (elevated or not change.needs_admin)
        ctk.CTkLabel(self, text=meta, font=_font(10), text_color=theme.INK_SECONDARY, anchor="w").grid(
            row=1, column=0, sticky="w", padx=ROW_TEXT_PADX
        )
        changed = view == VIEW_CHANGED
        if changed:
            text = "\n".join(drift_lines(change))
        else:
            text = "\n".join(change.details[:6]) + ("\n…" if count > 6 else "")
        self.detail_label = self._text(text, theme.WARNING if changed else theme.INK_MUTED)
        # Rows under the title and meta lines, each with its own bottom padding; the last one
        # keeps the row's bottom margin.
        body: list[tuple[tk.Widget, int]] = [(self.detail_label, 2)]
        if changed:
            buttons = ctk.CTkFrame(self, fg_color="transparent")
            if on_apply_again is not None and can_apply_again:
                self.apply_again_button = _button(
                    buttons,
                    "Apply again",
                    self.APPLY_AGAIN_WIDTH,
                    engine_ready and elevated,
                    lambda: on_apply_again(change),
                )
                self.apply_again_button.pack(side="left", padx=(0, 6))
            self.undo_button = _button(buttons, "Undo", self.UNDO_WIDTH, can_undo, lambda: on_undo(change))
            self.undo_button.pack(side="left", padx=(0, 6))
            if on_keep is not None:
                self.keep_button = _button(
                    buttons, "Keep as is", self.KEEP_WIDTH, engine_ready, lambda: on_keep(change, KEEP_DRIFT)
                )
                self.keep_button.pack(side="left")
            body.append((buttons, 10))
        else:
            self.undo_button = _button(self, "Undo", self.UNDO_WIDTH, can_undo, lambda: on_undo(change))
            self.undo_button.grid(row=0, column=1, rowspan=2, sticky="e", padx=10, pady=(8, 0))
            if change.drift:
                self.drift_label = self._text(SEE_CHANGED_TEXT, theme.WARNING)
                body.append((self.drift_label, 2))
            if change.former and on_keep is not None:
                buttons = ctk.CTkFrame(self, fg_color="transparent")
                self.keep_button = _button(
                    buttons, "Keep as is", self.KEEP_WIDTH, engine_ready, lambda: on_keep(change, KEEP_FORMER)
                )
                self.keep_button.pack(side="left")
                body.append((buttons, 10))
        for index, (widget, bottom) in enumerate(body):
            last = index == len(body) - 1
            widget.grid(
                row=2 + index,
                column=0,
                columnspan=2,
                sticky="w",
                padx=ROW_TEXT_PADX,
                pady=(0 if index == 0 else 2, max(bottom, 8) if last else bottom),
            )
        self.bind("<Configure>", self._fit_details, add="+")

    def _text(self, text: str, color: str) -> ctk.CTkLabel:
        return ctk.CTkLabel(
            self,
            text=text,
            font=_font(10),
            text_color=color,
            anchor="w",
            justify="left",
            wraplength=ROW_WRAP,
        )

    @property
    def wrapped_labels(self) -> list[ctk.CTkLabel]:
        """The labels that wrap to the row's width: the detail lines and the drift line."""
        return [self.detail_label] + ([self.drift_label] if self.drift_label is not None else [])

    def _fit_details(self, event: tk.Event) -> None:
        """Wraps the detail lines at the row's width."""
        scale = ctk.ScalingTracker.get_widget_scaling(self) or 1.0
        wrap = max(MIN_WRAP, int(event.width / scale) - DETAIL_INSET)
        for label in self.wrapped_labels:
            if label.cget("wraplength") != wrap:
                label.configure(wraplength=wrap)


class HistoryPanel(ctk.CTkFrame):
    """The active changes, newest first, beside the audit log.

    With a drift report (an engine that reads which records still hold what Cairn set), a
    selector switches between every active change (All) and the changes that no longer hold
    (Changed). `on_apply_again(change)` and `on_keep(change, KEEP_DRIFT | KEEP_FORMER)` run the
    rows' Apply again and Keep as is.
    """

    def __init__(
        self,
        master: tk.Misc,
        on_refresh: Callable[[], None],
        on_undo: Callable[[ChangeGroup], None],
        on_apply_again: Callable[[ChangeGroup], None] | None = None,
        on_keep: Callable[[ChangeGroup, str], None] | None = None,
    ) -> None:
        super().__init__(master, fg_color="transparent")
        self._on_undo = on_undo
        self._on_apply_again = on_apply_again
        self._on_keep = on_keep
        self.grid_columnconfigure(0, weight=3)
        self.grid_columnconfigure(1, weight=2)
        self.grid_rowconfigure(0, weight=1)

        changes = ctk.CTkFrame(
            self, fg_color=theme.SURFACE, corner_radius=10, border_width=1, border_color=theme.BORDER
        )
        changes.grid(row=0, column=0, sticky="nsew", padx=(0, 5))
        changes.grid_columnconfigure(0, weight=1)
        changes.grid_rowconfigure(2, weight=1)
        header = ctk.CTkFrame(changes, fg_color="transparent")
        header.grid(row=0, column=0, sticky="ew", padx=14, pady=(10, 0))
        header.grid_columnconfigure(0, weight=1)
        ctk.CTkLabel(
            header, text="Active changes", font=_font(13, "bold"), text_color=theme.INK_SECONDARY
        ).grid(row=0, column=0, sticky="w")
        self.view_bar = ctk.CTkSegmentedButton(
            header,
            values=[ALL_VIEW_LABEL, CHANGED_VIEW_LABEL.format(n=0)],
            font=_font(11),
            selected_color=theme.ACCENT,
            selected_hover_color=theme.ACCENT_HOVER,
            unselected_color=theme.SURFACE_RAISED,
            unselected_hover_color=theme.BUTTON_NEUTRAL_HOVER,
            command=self._view_selected,
        )
        self.view_bar.set(ALL_VIEW_LABEL)
        self.refresh_button = ctk.CTkButton(
            header,
            text="Refresh",
            width=100,
            height=30,
            font=_font(12),
            fg_color=theme.BUTTON_NEUTRAL,
            hover_color=theme.BUTTON_NEUTRAL_HOVER,
            command=on_refresh,
        )
        self.refresh_button.grid(row=0, column=2, sticky="e")
        self.summary = ctk.CTkLabel(
            changes,
            text="Everything Cairn has changed and not yet undone.",
            font=_font(11),
            text_color=theme.INK_MUTED,
            anchor="w",
            justify="left",
            wraplength=PANEL_WRAP,
        )
        self.summary.grid(row=1, column=0, sticky="ew", padx=14, pady=(2, 6))
        self._changes_width = 0
        changes.bind("<Configure>", self._changes_resized, add="+")
        self.list = ctk.CTkScrollableFrame(
            changes,
            fg_color=theme.SURFACE,
            scrollbar_button_color=theme.BASELINE,
            scrollbar_button_hover_color=theme.INK_MUTED,
        )
        self.list.grid(row=2, column=0, sticky="nsew", padx=6, pady=(0, 8))
        self.list.grid_columnconfigure(0, weight=1)

        log = ctk.CTkFrame(
            self, fg_color=theme.SURFACE, corner_radius=10, border_width=1, border_color=theme.BORDER
        )
        log.grid(row=0, column=1, sticky="nsew", padx=(5, 0))
        log.grid_columnconfigure(0, weight=1)
        log.grid_rowconfigure(1, weight=1)
        ctk.CTkLabel(log, text="Activity log", font=_font(13, "bold"), text_color=theme.INK_SECONDARY).grid(
            row=0, column=0, sticky="w", padx=14, pady=(10, 6)
        )
        self.log = ctk.CTkTextbox(
            log,
            font=_font(10),
            fg_color=theme.PAGE,
            text_color=theme.INK_SECONDARY,
            border_width=1,
            border_color=theme.BORDER,
            wrap="none",
        )
        self.log.grid(row=1, column=0, sticky="nsew", padx=10, pady=(0, 10))
        self._rows: list[ChangeRow] = []
        self.groups: list[ChangeGroup] = []
        self._view = VIEW_ALL
        self._drift_shown = False
        self._drift_count = 0
        # Records the last drift report read and could not read.
        self._drift_checked = 0
        self._drift_unchecked = 0
        self._drift_unchecked_no_value = 0
        self._drift_error: str | None = None
        self._engine_ready = False
        self._elevated = False
        self._unavailable_ids: frozenset[str] = frozenset()

    @property
    def row_count(self) -> int:
        return len(self._rows)

    @property
    def view(self) -> str:
        """`VIEW_ALL` or `VIEW_CHANGED`."""
        return self._view

    @property
    def drift_shown(self) -> bool:
        """Whether the last load read which changes still hold, so the view selector is shown."""
        return self._drift_shown

    @property
    def drift_count(self) -> int:
        """Records of the last load that no longer hold what Cairn set."""
        return self._drift_count

    @property
    def changed_groups(self) -> list[ChangeGroup]:
        """The changes with at least one record that no longer holds."""
        return [g for g in self.groups if g.drift]

    def set_loading(self) -> None:
        self.refresh_button.configure(state="disabled")

    def show_error(self, message: str) -> None:
        self.refresh_button.configure(state="normal")
        self.summary.configure(text=f"Could not read the journal: {message}", text_color=theme.CRITICAL)

    @property
    def rows(self) -> list[ChangeRow]:
        return list(self._rows)

    def _changes_resized(self, event: tk.Event) -> None:
        if event.width == self._changes_width:
            return
        self._changes_width = event.width
        scale = ctk.ScalingTracker.get_widget_scaling(self) or 1.0
        wrap = fitted_wrap(event.width / scale - SUMMARY_INSET, PANEL_WRAP)
        if self.summary.cget("wraplength") != wrap:
            self.summary.configure(wraplength=wrap)

    def show(
        self,
        export: dict[str, Any],
        titles: dict[str, str],
        engine_ready: bool,
        elevated: bool,
        startup_names: Mapping[str, str] | None = None,
        permission_names: Mapping[str, str] | None = None,
        *,
        drift: Mapping[str, Any] | None = None,
        drift_error: str | None = None,
        former_targets: Collection[str] = (),
        unavailable_ids: Collection[str] = (),
    ) -> None:
        """Lists the active changes; a standard user can undo the per-user ones.

        `startup_names` are the packaged-app task names for the startup records' titles and
        `permission_names` the Store apps' names for the app-permission records' titles;
        `former_targets` are the targets of values earlier versions of Cairn set (see
        `group_changes`). `drift` is the engine's drift report: with it the Changed view is
        offered, and its summary claims only the records the report read (the records the
        check never reads, `records_never_checked`, count as not checked); `drift_error` says
        why it could not be read. Apply again is not offered for `unavailable_ids`, the tweaks
        an apply skips on this PC: retired ones and those this PC cannot use.
        """
        self.refresh_button.configure(state="normal")
        self.groups = group_changes(export, titles, startup_names, permission_names, former_targets)
        self._drift_shown = drift is not None
        self._drift_count = attach_drift(self.groups, drift)
        self._drift_checked = int((drift or {}).get("checked") or 0)
        self._drift_unchecked = int((drift or {}).get("unchecked") or 0)
        self._drift_unchecked_no_value = int((drift or {}).get("unchecked_no_value") or 0)
        if drift is not None:
            self._drift_unchecked += records_never_checked(export)
        self._drift_error = drift_error
        self._engine_ready = engine_ready
        self._elevated = elevated
        self._unavailable_ids = frozenset(unavailable_ids)
        if not self._drift_shown:
            self._view = VIEW_ALL
        self._place_view_bar()
        self._render()

        lines = []
        for op in export.get("ops", []):
            detail = f"  ({op['detail']})" if op.get("detail") else ""
            lines.append(f"{local_time(op['ts'])}  {op['op']}  {op['target']}  → {op['outcome']}{detail}")
        self.log.configure(state="normal")
        self.log.delete("1.0", "end")
        self.log.insert("1.0", "\n".join(lines) if lines else "No activity recorded yet.")
        self.log.configure(state="disabled")

    def set_view(self, view: str) -> None:
        """Shows `VIEW_ALL` or, when the last load read which changes hold, `VIEW_CHANGED`."""
        if view not in (VIEW_ALL, VIEW_CHANGED):
            raise ValueError(f"unknown History view: {view}")
        if view == VIEW_CHANGED and not self._drift_shown:
            return
        self._view = view
        self._place_view_bar()
        self._render()

    def _view_selected(self, label: str) -> None:
        self._view = VIEW_ALL if label == ALL_VIEW_LABEL else VIEW_CHANGED
        self._render()

    def _place_view_bar(self) -> None:
        if not self._drift_shown:
            # grid_forget: CustomTkinter replays the last grid call on a scaling change.
            self.view_bar.grid_forget()
            return
        labels = [ALL_VIEW_LABEL, CHANGED_VIEW_LABEL.format(n=self._drift_count)]
        self.view_bar.configure(values=labels)
        self.view_bar.set(labels[0] if self._view == VIEW_ALL else labels[1])
        self.view_bar.grid(row=0, column=1, sticky="e", padx=(0, 8))

    def _render(self) -> None:
        for row in self._rows:
            row.destroy()
        self._rows.clear()
        if self._view == VIEW_CHANGED:
            shown = self.changed_groups
            if shown:
                self.summary.configure(text=changed_summary(self._drift_count), text_color=theme.WARNING)
            else:
                text, color = hold_summary(
                    self._drift_checked, self._drift_unchecked, self._drift_unchecked_no_value
                )
                self.summary.configure(text=text, text_color=color)
        else:
            shown = self.groups
            self._show_all_summary()
        for i, change in enumerate(shown):
            row = ChangeRow(
                self.list,
                change,
                self._engine_ready,
                self._elevated,
                self._on_undo,
                view=self._view,
                on_apply_again=self._on_apply_again,
                on_keep=self._on_keep,
                can_apply_again=any(item not in self._unavailable_ids for item in change.item_ids),
            )
            row.grid(row=i, column=0, sticky="ew", padx=6, pady=4)
            self._rows.append(row)

    def _show_all_summary(self) -> None:
        total = sum(len(g.details) for g in self.groups)
        if not self.groups:
            self.summary.configure(
                text="No active changes: the system is at its original state.", text_color=theme.GOOD
            )
            return
        items = len(self.groups)
        text = (
            f"{items} item{'s' if items != 1 else ''} with {total} recorded "
            f"change{'s' if total != 1 else ''}, newest first. "
            "Undo restores the values from before Cairn changed them."
        )
        if self._drift_error:
            text += "\n" + DRIFT_FAILED_TEXT.format(error=self._drift_error)
            self.summary.configure(text=text, text_color=theme.WARNING)
        else:
            self.summary.configure(text=text, text_color=theme.INK_MUTED)
