"""History section: pending (revertible) changes grouped per tweak, and the audit log."""

from __future__ import annotations

import re
import tkinter as tk
from collections.abc import Callable, Hashable, Iterable, Mapping
from datetime import datetime
from typing import Any

import customtkinter as ctk

from .. import theme
from .controls import MIN_WRAP, ROW_WRAP, WRAP_MARGIN

# Horizontal padding of a change row's texts, and the room its detail lines keep free inside
# the row, in CustomTkinter units.
ROW_TEXT_PADX = 12
DETAIL_INSET = 2 * ROW_TEXT_PADX + WRAP_MARGIN


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


class ChangeGroup:
    """Journal records that belong to one tweak (or one app, service or startup entry)."""

    def __init__(self, title: str, kind: str) -> None:
        self.title = title
        self.kind = kind
        self.details: list[str] = []
        self.recorded_at = ""
        self.filter: dict[str, Any] = {
            "registry": [],
            "services": [],
            "appx_families": [],
            "power": False,
            "scheduled_tasks": [],
            "dns": [],
            "task_definitions": [],
        }

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
    Exports without some kinds of records (older engines) are grouped as far as they go.
    """
    groups: dict[Hashable, ChangeGroup] = {}

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
        g.add(f"{target}  ·  was {value_text(rec.get('original'))}", rec["recorded_at"])
        g.filter["registry"].append(
            {"hive": rec["hive"], "key_path": rec["key_path"], "value_name": rec["value_name"]}
        )
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


class ChangeRow(ctk.CTkFrame):
    """One active change: its title, when it was recorded, its records and the Undo button.

    The detail lines (`detail_label`) take the row's whole width under the button and wrap
    with it, so a long registry path breaks onto further lines instead of being cut off.
    """

    def __init__(
        self,
        master: tk.Misc,
        change: ChangeGroup,
        engine_ready: bool,
        elevated: bool,
        on_undo: Callable[[ChangeGroup], None],
    ) -> None:
        super().__init__(master, fg_color=theme.SURFACE_RAISED, corner_radius=8)
        self.change = change
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
        self.detail_label = ctk.CTkLabel(
            self,
            text="\n".join(change.details[:6]) + ("\n…" if count > 6 else ""),
            font=_font(10),
            text_color=theme.INK_MUTED,
            anchor="w",
            justify="left",
            wraplength=ROW_WRAP,
        )
        self.detail_label.grid(row=2, column=0, columnspan=2, sticky="w", padx=ROW_TEXT_PADX, pady=(0, 8))
        self.bind("<Configure>", self._fit_details, add="+")
        self.undo_button = ctk.CTkButton(
            self,
            text="Undo",
            width=74,
            height=26,
            font=_font(11),
            fg_color=theme.BUTTON_NEUTRAL,
            hover_color=theme.BUTTON_NEUTRAL_HOVER,
            state="normal" if can_undo else "disabled",
            command=lambda: on_undo(change),
        )
        self.undo_button.grid(row=0, column=1, rowspan=2, sticky="e", padx=10, pady=(8, 0))

    def _fit_details(self, event: tk.Event) -> None:
        """Wraps the detail lines at the row's width."""
        scale = ctk.ScalingTracker.get_widget_scaling(self) or 1.0
        wrap = max(MIN_WRAP, int(event.width / scale) - DETAIL_INSET)
        if self.detail_label.cget("wraplength") != wrap:
            self.detail_label.configure(wraplength=wrap)


class HistoryPanel(ctk.CTkFrame):
    def __init__(
        self,
        master: tk.Misc,
        on_refresh: Callable[[], None],
        on_undo: Callable[[ChangeGroup], None],
    ) -> None:
        super().__init__(master, fg_color="transparent")
        self._on_undo = on_undo
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
        self.refresh_button.grid(row=0, column=1, sticky="e")
        self.summary = ctk.CTkLabel(
            changes,
            text="Everything Cairn has changed and not yet undone.",
            font=_font(11),
            text_color=theme.INK_MUTED,
            anchor="w",
        )
        self.summary.grid(row=1, column=0, sticky="ew", padx=14, pady=(2, 6))
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

    @property
    def row_count(self) -> int:
        return len(self._rows)

    def set_loading(self) -> None:
        self.refresh_button.configure(state="disabled")

    def show_error(self, message: str) -> None:
        self.refresh_button.configure(state="normal")
        self.summary.configure(text=f"Could not read the journal: {message}", text_color=theme.CRITICAL)

    @property
    def rows(self) -> list[ChangeRow]:
        return list(self._rows)

    def show(
        self,
        export: dict[str, Any],
        titles: dict[str, str],
        engine_ready: bool,
        elevated: bool,
        startup_names: Mapping[str, str] | None = None,
        permission_names: Mapping[str, str] | None = None,
    ) -> None:
        """Lists the active changes; a standard user can undo the per-user ones.

        `startup_names` are the packaged-app task names for the startup records' titles and
        `permission_names` the Store apps' names for the app-permission records' titles (see
        `group_changes`).
        """
        self.refresh_button.configure(state="normal")
        for row in self._rows:
            row.destroy()
        self._rows.clear()
        self.groups = group_changes(export, titles, startup_names, permission_names)
        total = sum(len(g.details) for g in self.groups)
        if self.groups:
            items = len(self.groups)
            self.summary.configure(
                text=f"{items} item{'s' if items != 1 else ''} with {total} recorded "
                f"change{'s' if total != 1 else ''}, newest first. "
                "Undo restores the values from before Cairn changed them.",
                text_color=theme.INK_MUTED,
            )
        else:
            self.summary.configure(
                text="No active changes: the system is at its original state.", text_color=theme.GOOD
            )
        for i, change in enumerate(self.groups):
            row = ChangeRow(self.list, change, engine_ready, elevated, self._on_undo)
            row.grid(row=i, column=0, sticky="ew", padx=6, pady=4)
            self._rows.append(row)

        lines = []
        for op in export.get("ops", []):
            detail = f"  ({op['detail']})" if op.get("detail") else ""
            lines.append(f"{local_time(op['ts'])}  {op['op']}  {op['target']}  → {op['outcome']}{detail}")
        self.log.configure(state="normal")
        self.log.delete("1.0", "end")
        self.log.insert("1.0", "\n".join(lines) if lines else "No activity recorded yet.")
        self.log.configure(state="disabled")
