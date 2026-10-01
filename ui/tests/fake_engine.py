"""In-memory stand-in for the optimizer_engine extension module.

Mirrors the module's function signatures and result shapes, records every call and never
touches the system, so UI flows that apply and revert changes can be exercised safely.

State model: `applied` holds the tweak ids Cairn changed (they have journal records
and can be reverted); `preset` holds ids whose values were already at their targets (they
scan as applied but have nothing to revert); `unreadable` ids scan as unavailable with a
read warning; `missing` ids (id -> reason) scan as unavailable with the reason as their note
and no warning, as a tweak whose requirement is not met on this PC. Disabled startup entries
are journaled as StartupApproved registry records, or as the `State` value of the task key
for packaged-app startup tasks.

Tweaks named in `scheduled_task_tweaks` (their ids must also be in `extra_tweaks`) turn off
scheduled tasks instead of writing a registry value: once applied, each of their task paths
is one scheduled-task journal record, reverted as `enable scheduled task <path>`.

The functions of the other sections come from the feature mixins `FakeNetwork`,
`FakeTools`, `FakeSysinfo`, `FakeHealth`, `FakePermissions`, `FakeStorage`, `FakeUpdates`,
`FakeMaintenance` and `FakeProfiles` (one file each). Each mixin takes its own constructor
options (`FakeEngine(**feature_options)`); an option no mixin understands is a `TypeError`.
`unsupported` names module functions to hide, as an outdated engine build would lack them.
Journaled kinds of the mixins join the journal through hooks: Windows Update settings
(`_wu_*`) and the app-permission records of earlier builds (`_permission_*`) are registry
records, scheduled maintenance tasks (`_task_definition_*`) are task definitions.

`other_user` is what `elevated_as_other_user` answers: False (the signed-in user), True
(another account) or None (it cannot be confirmed, so the call raises).

Timing: every call that runs on the bridge's worker sleeps `delay` seconds; `slow` maps a
function name to the seconds each call of it takes instead, so a test can close the window
while that one call runs.
"""

from __future__ import annotations

import json
import threading
import time
from collections.abc import Iterable
from typing import Any

from .fake_health import FakeHealth
from .fake_maintenance import FakeMaintenance
from .fake_network import FakeNetwork
from .fake_permissions import FakePermissions
from .fake_profiles import FakeProfiles
from .fake_storage import FakeStorage
from .fake_sysinfo import FakeSysinfo
from .fake_tools import FakeTools
from .fake_updates import FakeUpdates

CATEGORIES = ("privacy", "gaming", "performance", "interface", "bloatware")

# (id, category, recommended)
TWEAKS = (
    ("privacy.activity_history", "privacy", True),
    ("privacy.cortana", "privacy", True),
    ("gaming.game_mode", "gaming", True),
    ("performance.sysmain", "performance", True),
    ("interface.file_extensions", "interface", True),
)

# Task paths of a scheduled-task tweak, for `FakeEngine(scheduled_task_tweaks=...)`.
CEIP_TASKS = (
    "\\Microsoft\\Windows\\Customer Experience Improvement Program\\Consolidator",
    "\\Microsoft\\Windows\\Customer Experience Improvement Program\\UsbCeip",
)

# (id, title, requires_admin, default_on, bytes, paths, recent_files_kept)
CLEANUP_TARGETS = [
    (
        "user_temp",
        "Temporary files",
        False,
        True,
        50_000_000,
        ["C:\\Users\\Test\\AppData\\Local\\Temp"],
        True,
    ),
    ("windows_temp", "Windows temp folder", True, True, 20_000_000, ["C:\\Windows\\Temp"], True),
    ("recycle_bin", "Recycle Bin", False, False, 5_000_000, ["C:\\$Recycle.Bin"], False),
    ("browser_chrome", "Chrome cache", False, True, 0, ["C:\\Users\\Test\\Chrome\\Cache"], False),
]

# (source, name, can_toggle), optionally followed by the entry key when it differs from the
# name (a packaged task's key is `<package family>\<task id>`).
StartupSpec = tuple[str, str, bool] | tuple[str, str, bool, str]
STARTUP: tuple[StartupSpec, ...] = (
    ("user_run", "Discord", True),
    ("user_run", "Spotify", True),
    ("user_run", "Steam", True),
)

# source -> (location, requires_admin, hive, StartupApproved subkey)
STARTUP_SOURCES = {
    "user_run": ("HKCU Run", False, "HKCU", "Run"),
    "machine_run": ("HKLM Run", True, "HKLM", "Run"),
    "machine_run32": ("HKLM Run (32-bit)", True, "HKLM", "Run32"),
    "user_folder": ("Startup folder", False, "HKCU", "StartupFolder"),
    "common_folder": ("Startup folder (all users)", True, "HKLM", "StartupFolder"),
    "policy_run": ("Policy Run", True, "HKLM", "Run"),
    "packaged_task": ("Packaged app", False, "HKCU", ""),
}
APPROVED_ROOT = "Software\\Microsoft\\Windows\\CurrentVersion\\Explorer\\StartupApproved"
PACKAGED_ROOT = (
    "Software\\Classes\\Local Settings\\Software\\Microsoft\\Windows\\CurrentVersion\\AppModel\\SystemAppData"
)
# `note` of an entry that is not toggleable because Group Policy sets it.
POLICY_NOTE = "Set by Group Policy; it can only be changed through that policy."
# Where `journal_path` says the journal is.
JOURNAL_PATH = "C:\\Users\\Test\\AppData\\Local\\PCOptimizer\\journal.db"


def _item(
    item_id: str,
    category: str,
    state: str,
    *,
    kind: str = "tweak",
    recommended: bool = True,
    revertible: bool = False,
    note: str | None = None,
    description: str | None = None,
    actions: list[dict[str, Any]] | None = None,
) -> dict[str, Any]:
    if actions is None:
        actions = [{"state": state, "detail": f"HKLM\\Test\\{item_id} not set, target 1"}]
    return {
        "id": item_id,
        "kind": kind,
        "category": category,
        "title": item_id.split(".", 1)[1].replace("_", " ").title(),
        "description": description or f"Description of {item_id}.",
        "risk": "low",
        "recommended": recommended,
        "state": state,
        "restart": "none",
        "actions": actions,
        "revertible": revertible,
        "note": note,
    }


class FakeEngine(
    FakeNetwork,
    FakeTools,
    FakeSysinfo,
    FakeHealth,
    FakePermissions,
    FakeStorage,
    FakeUpdates,
    FakeMaintenance,
    FakeProfiles,
):
    def __init__(
        self,
        *,
        elevated: bool = True,
        other_user: bool | None = False,
        delay: float = 0.0,
        extra_tweaks: Iterable[tuple[str, str, bool]] = (),
        extra_startup: Iterable[StartupSpec] = (),
        preset: Iterable[str] = (),
        unreadable: Iterable[str] = (),
        missing: dict[str, str] | None = None,
        notes: dict[str, str] | None = None,
        descriptions: dict[str, str] | None = None,
        appx_inventory_error: str | None = None,
        revert_restart: str = "none",
        scheduled_task_tweaks: dict[str, tuple[str, ...]] | None = None,
        unsupported: Iterable[str] = (),
        slow: dict[str, float] | None = None,
        **feature_options: Any,
    ) -> None:
        self.calls: list[tuple[str, tuple[Any, ...]]] = []
        self.elevated = elevated
        self.other_user = other_user
        self.delay = delay
        self.slow = dict(slow or {})
        self.tweaks = list(TWEAKS) + list(extra_tweaks)
        # Tweak id -> the scheduled task paths it turns off.
        self.task_tweaks = {i: tuple(paths) for i, paths in (scheduled_task_tweaks or {}).items()}
        unknown = sorted(set(self.task_tweaks) - {i for i, _, _ in self.tweaks})
        if unknown:
            raise ValueError(
                f"scheduled_task_tweaks ids are not tweaks (pass them in extra_tweaks): {unknown}"
            )
        self.startup = list(STARTUP) + list(extra_startup)
        self.applied: set[str] = set()
        self.preset = set(preset)
        self.unreadable = set(unreadable)
        self.missing = dict(missing or {})
        self.notes = dict(notes or {})
        self.descriptions = dict(descriptions or {})
        self.appx_inventory_error = appx_inventory_error
        self.revert_restart = revert_restart
        self._startup_disabled: set[str] = set()
        self._lock = threading.Lock()

        options = dict(feature_options)
        self._init_network(options)
        self._init_tools(options)
        self._init_sysinfo(options)
        self._init_health(options)
        self._init_permissions(options)
        self._init_storage(options)
        self._init_updates(options)
        self._init_maintenance(options)
        self._init_profiles(options)
        if options:
            raise TypeError(f"unknown FakeEngine options: {', '.join(sorted(options))}")
        for name in unsupported:
            setattr(self, name, None)  # EngineBridge.supports(name) is then False

    def _record(self, name: str, *args: Any) -> None:
        with self._lock:
            self.calls.append((name, args))
        pause = self.slow.get(name, self.delay)
        if pause:
            time.sleep(pause)

    def _record_quick(self, name: str, *args: Any) -> None:
        """Records a call without the `delay` sleep, for functions the bridge calls
        synchronously on the UI thread."""
        with self._lock:
            self.calls.append((name, args))

    def calls_named(self, name: str) -> list[tuple[Any, ...]]:
        with self._lock:
            return [args for n, args in self.calls if n == name]

    # -- module surface ------------------------------------------------------------

    def version(self) -> str:
        return "0.0.0-test"

    def is_elevated(self) -> bool:
        return self.elevated

    def elevated_as_other_user(self) -> bool:
        self._record("elevated_as_other_user")
        if self.other_user is None:
            raise RuntimeError("cannot confirm the signed-in account")
        return self.other_user

    def journal_path(self) -> str:
        self._record_quick("journal_path")
        return JOURNAL_PATH

    def install_info(self) -> dict[str, Any] | None:
        self._record("install_info")
        return None

    def _state(self, item_id: str) -> str:
        if item_id in self.unreadable or item_id in self.missing:
            return "unavailable"
        if item_id in self.applied or item_id in self.preset:
            return "applied"
        return "not_applied"

    def _task_actions(self, item_id: str, state: str) -> list[dict[str, Any]] | None:
        """Scanned actions of a scheduled-task tweak, worded like the engine's; None for
        other tweaks."""
        paths = self.task_tweaks.get(item_id)
        if paths is None:
            return None
        if state == "unavailable":
            return [{"state": state, "detail": f"scheduled task {p} is not on this PC"} for p in paths]
        current = "disabled" if state == "applied" else "enabled"
        return [{"state": state, "detail": f"scheduled task {p}: {current}; target disabled"} for p in paths]

    def _task_paths(self, ids: Iterable[str]) -> list[str]:
        """Task paths of the scheduled-task tweaks among `ids`, in order."""
        return [p for i in ids for p in self.task_tweaks.get(i, ())]

    def _items(self) -> list[dict[str, Any]]:
        items = [
            _item(
                i,
                c,
                self._state(i),
                recommended=rec,
                revertible=i in self.applied,
                note=self.missing.get(i, self.notes.get(i)),
                description=self.descriptions.get(i),
                actions=self._task_actions(i, self._state(i)),
            )
            for i, c, rec in self.tweaks
        ]
        if self.appx_inventory_error is None:
            items.append(_item("appx.Microsoft.BingNews", "bloatware", "not_applied", kind="appx"))
        return items

    def scan(self) -> dict[str, Any]:
        self._record("scan")
        items = self._items()
        warnings = [f"{i}: cannot read registry value: access denied" for i in sorted(self.unreadable)]
        if self.appx_inventory_error is not None:
            warnings.append(f"Store package inventory unavailable: {self.appx_inventory_error}")
        categories = []
        for c in CATEGORIES:
            group = [i for i in items if i["category"] == c]
            rec = [i for i in group if i["recommended"] and i["state"] != "unavailable"]
            done = [i for i in rec if i["state"] == "applied"]
            categories.append(
                {
                    "category": c,
                    "items": len(group),
                    "applied": len([i for i in group if i["state"] == "applied"]),
                    "recommended": len(rec),
                    "recommended_applied": len(done),
                    "active": bool(rec) and len(done) == len(rec),
                }
            )
        return {
            "elevated": self.elevated,
            "has_battery": False,
            "items": items,
            "categories": categories,
            "warnings": warnings,
            "duration_ms": 12,
        }

    def journal_summary(self) -> dict[str, Any]:
        self._record("journal_summary")
        values = [i for i in self.applied if i not in self.task_tweaks]
        return {
            "registry_active": len(values)
            + len(self._startup_disabled)
            + self._wu_active_count()
            + self._permission_active_count(),
            "services_active": 0,
            "scheduled_tasks_active": len(self._task_paths(self.applied)),
            "appx_active": 0,
            "power_active": 0,
            "dns_active": self._dns_active_count(),
            "task_definitions_active": self._task_definition_active_count(),
        }

    def _apply_report(self, ids: list[str], dry_run: bool) -> dict[str, Any]:
        results = []
        for i in ids:
            if dry_run:
                outcome = "already_applied" if self._state(i) == "applied" else "planned"
            else:
                outcome = "already_applied" if i in self.preset else "applied"
            results.append({"id": i, "outcome": outcome, "details": [f"{i} detail"]})
        if not dry_run:
            self.applied.update(i for i in ids if i not in self.preset)
        return {
            "dry_run": dry_run,
            # The engine opens a journal session only for a real apply with work to do.
            "session_id": None if dry_run or not ids else 1,
            "restore_point": None,
            "results": results,
            "warnings": [],
            "restart": "none",
        }

    def apply(self, ids: list[str], restore_point: str = "try", dry_run: bool = False) -> dict[str, Any]:
        self._record("apply", list(ids), restore_point, dry_run)
        return self._apply_report(list(ids), dry_run)

    def apply_category(
        self, category: str, restore_point: str = "try", dry_run: bool = False
    ) -> dict[str, Any]:
        self._record("apply_category", category, restore_point, dry_run)
        ids = [
            i["id"]
            for i in self._items()
            if i["category"] == category and i["recommended"] and i["state"] in ("not_applied", "partial")
        ]
        return self._apply_report(ids, dry_run)

    def _revert_report(
        self,
        ids: list[str],
        dry_run: bool,
        startup_ids: Iterable[str] = (),
        dns: Iterable[str] | None = (),
        extra_actions: Iterable[str] = (),
        extra_registry: int = 0,
        task_definition_actions: Iterable[str] = (),
        task_definitions_deleted: int = 0,
    ) -> dict[str, Any]:
        """Report of reverting `ids`, `startup_ids` and the DNS records of the adapters in
        `dns` (None: every active DNS record), merged with what the mixins restored: registry
        values (`extra_actions`, `extra_registry`) and task definitions.

        A scheduled-task tweak restores one record per task path; the actions follow the
        engine's restore order (registry values, scheduled tasks, task definitions, DNS
        servers)."""
        targets = [i for i in ids if i in self.applied]
        values = [i for i in targets if i not in self.task_tweaks]
        task_paths = self._task_paths(targets)
        startup = [s for s in startup_ids if s in self._startup_disabled]
        if not dry_run:
            self.applied.difference_update(targets)
            self._startup_disabled.difference_update(startup)
        deleted = len(values) + len(startup)
        extra = list(extra_actions)
        definitions = list(task_definition_actions)
        # Records selected, dry run or not: the restart they need is reported either way.
        restored = deleted + len(task_paths) + len(extra) + len(definitions)
        dns_actions, dns_restored = self._dns_revert(dns, dry_run)
        return {
            "dry_run": dry_run,
            "registry_restored": 0 if dry_run else extra_registry,
            "registry_deleted": 0 if dry_run else deleted,
            "services_restored": 0,
            "services_started": 0,
            "scheduled_tasks_restored": 0 if dry_run else len(task_paths),
            "appx_restored": 0,
            "appx_store_required": [],
            "power_restored": 0,
            "dns_restored": dns_restored,
            "task_definitions_deleted": 0 if dry_run else task_definitions_deleted,
            "actions": [f"delete value: {t}" for t in values + startup]
            + extra
            + [f"enable scheduled task {p}" for p in task_paths]
            + definitions
            + dns_actions,
            "failures": [],
            # Like the engine, a dry run reports the restart the selected records need.
            "restart": self.revert_restart if restored else "none",
        }

    def revert(self, ids: list[str], dry_run: bool = False) -> dict[str, Any]:
        self._record("revert", list(ids), dry_run)
        return self._revert_report(list(ids), dry_run)

    def revert_category(self, category: str, dry_run: bool = False) -> dict[str, Any]:
        self._record("revert_category", category, dry_run)
        ids = [i for i in self.applied if i.startswith(category + ".")]
        return self._revert_report(ids, dry_run)

    def revert_all(self, dry_run: bool = False) -> dict[str, Any]:
        self._record("revert_all", dry_run)
        wu_actions, wu_restored, _ = self._wu_revert(None, dry_run)
        permission_actions, permission_restored, _ = self._permission_revert(None, dry_run)
        definition_actions, definitions_deleted = self._task_definition_revert(None, dry_run)
        return self._revert_report(
            sorted(self.applied),
            dry_run,
            sorted(self._startup_disabled),
            dns=None,
            extra_actions=wu_actions + permission_actions,
            extra_registry=wu_restored + permission_restored,
            task_definition_actions=definition_actions,
            task_definitions_deleted=definitions_deleted,
        )

    # -- catalog and history ---------------------------------------------------------

    def catalog(self) -> list[dict[str, Any]]:
        self._record("catalog")
        return [
            {"id": i["id"], "title": i["title"], "targets": self._targets(i["id"])} for i in self._items()
        ]

    def _targets(self, item_id: str) -> list[str]:
        """Journal target strings of a catalog item, as the engine words them."""
        if item_id in self.task_tweaks:
            return [f"scheduled task {p}" for p in self.task_tweaks[item_id]]
        return [f"HKLM\\Test\\{item_id}"]

    def _startup_record(self, entry_id: str) -> tuple[str, str, str]:
        """(hive, key path, value name) of the registry value a toggle writes."""
        source, key = entry_id.split(":", 1)
        _, _, hive, subkey = STARTUP_SOURCES[source]
        if source == "packaged_task":
            return hive, f"{PACKAGED_ROOT}\\{key}", "State"
        return hive, f"{APPROVED_ROOT}\\{subkey}", key

    def journal_export_json(self) -> str:
        self._record("journal_export_json")
        applied = sorted(self.applied)
        rows = [("HKLM", "Test", item_id) for item_id in applied if item_id not in self.task_tweaks]
        extra = self._wu_export() + self._permission_export()
        rows += [self._startup_record(e) for e in sorted(self._startup_disabled)]
        registry = [
            {
                "id": n,
                "session_id": 1,
                "recorded_at": "2026-09-25T10:00:00+00:00",
                "target": f"{hive}\\{key_path}\\{value_name}",
                "hive": hive,
                "key_path": key_path,
                "value_name": value_name,
                "key_existed": True,
                "value_existed": False,
                "original": None,
                "active": True,
                "reverted_at": None,
            }
            for n, (hive, key_path, value_name) in enumerate(rows, start=1)
        ]
        # The mixins' registry rows continue the numbering.
        registry += [{**row, "id": n} for n, row in enumerate(extra, start=len(registry) + 1)]
        scheduled_tasks = [
            {
                "id": n,
                "session_id": 1,
                "recorded_at": "2026-09-25T10:00:00+00:00",
                "target": f"scheduled task {path}",
                "path": path,
                "was_enabled": True,
                "active": True,
                "reverted_at": None,
            }
            for n, path in enumerate(self._task_paths(applied), start=1)
        ]
        ops = [
            {
                "id": 1,
                "session_id": 1,
                "ts": "2026-09-25T10:00:00+00:00",
                "op": "apply",
                "target": "x",
                "outcome": "applied",
                "detail": None,
            }
        ]
        return json.dumps(
            {
                "registry": registry,
                "services": [],
                "scheduled_tasks": scheduled_tasks,
                "appx": [],
                "power": [],
                "dns": self._dns_export(),
                "task_definitions": self._task_definition_export(),
                "ops": ops,
            }
        )

    def revert_targets(self, filter: dict[str, Any], dry_run: bool = False) -> dict[str, Any]:
        self._record("revert_targets", filter, dry_run)
        ids: list[str] = []
        startup: list[str] = []
        # Registry targets go to the mixins that own them first; the rest are tweaks and
        # startup entries.
        registry = list(filter.get("registry", []))
        wu_actions, wu_restored, registry = self._wu_revert(registry, dry_run)
        permission_actions, permission_restored, registry = self._permission_revert(registry, dry_run)
        definition_actions, definitions_deleted = self._task_definition_revert(
            list(filter.get("task_definitions", [])), dry_run
        )
        for r in registry:
            key = (r["hive"], r["key_path"].lower(), r["value_name"].lower())
            match = [e for e in self._startup_disabled if self._lower(self._startup_record(e)) == key]
            if match:
                startup += match
            else:
                ids.append(r["value_name"])
        # Task paths are compared ignoring case, like the engine's filter.
        for path in filter.get("scheduled_tasks", []):
            for tweak_id, paths in self.task_tweaks.items():
                if tweak_id not in ids and any(p.lower() == path.lower() for p in paths):
                    ids.append(tweak_id)
        return self._revert_report(
            ids,
            dry_run,
            startup,
            dns=filter.get("dns", []),
            extra_actions=wu_actions + permission_actions,
            extra_registry=wu_restored + permission_restored,
            task_definition_actions=definition_actions,
            task_definitions_deleted=definitions_deleted,
        )

    @staticmethod
    def _lower(record: tuple[str, str, str]) -> tuple[str, str, str]:
        return record[0], record[1].lower(), record[2].lower()

    # -- cleanup ---------------------------------------------------------------------

    def cleanup_scan(self) -> dict[str, Any]:
        self._record("cleanup_scan")
        targets = [
            {
                "id": tid,
                "title": title,
                "description": f"{title} description.",
                "requires_admin": admin,
                "default_on": default,
                "bytes": size,
                "files": size // 100_000,
                "blocked_reason": "Chrome is running" if tid == "browser_chrome" else None,
                "paths": list(paths),
                "recent_files_kept": recent,
            }
            for tid, title, admin, default, size, paths, recent in CLEANUP_TARGETS
        ]
        return {"targets": targets, "total_bytes": sum(t["bytes"] for t in targets), "duration_ms": 5}

    def cleanup_run(self, ids: list[str]) -> dict[str, Any]:
        self._record("cleanup_run", list(ids))
        sizes = {t[0]: t[4] for t in CLEANUP_TARGETS}
        results = [
            {
                "id": i,
                "freed_bytes": sizes.get(i, 0),
                "deleted_files": 3,
                "skipped_files": 1,
                "skipped_reason": None,
                "errors": [],
            }
            for i in ids
        ]
        return {"results": results, "freed_bytes": sum(r["freed_bytes"] for r in results), "duration_ms": 7}

    # -- startup -------------------------------------------------------------------

    def _startup_entries(self) -> list[dict[str, Any]]:
        entries = []
        for spec in self.startup:
            source, name, can_toggle = spec[0], spec[1], spec[2]
            key = spec[3] if len(spec) > 3 else name
            location, requires_admin, _, _ = STARTUP_SOURCES[source]
            entry_id = f"{source}:{key}"
            entries.append(
                {
                    "id": entry_id,
                    "name": name,
                    "source": source,
                    "location": location,
                    "command": f'"C:\\Apps\\{name}.exe" --minimized',
                    "path": f"C:\\Apps\\{name}.exe",
                    "publisher": f"{name} Inc.",
                    "exists": True,
                    "enabled": entry_id not in self._startup_disabled,
                    "requires_admin": requires_admin,
                    "can_toggle": can_toggle,
                    "note": "" if can_toggle else POLICY_NOTE,
                }
            )
        return entries

    def startup_list(self) -> list[dict[str, Any]]:
        self._record("startup_list")
        return self._startup_entries()

    def startup_set_enabled(self, id: str, enabled: bool, restore_point: str = "skip") -> dict[str, Any]:
        self._record("startup_set_enabled", id, enabled, restore_point)
        source = id.split(":", 1)[0]
        if STARTUP_SOURCES[source][1] and not self.elevated:
            raise RuntimeError("this operation requires an elevated (Administrator) process")
        if enabled:
            self._startup_disabled.discard(id)
        else:
            self._startup_disabled.add(id)
        return {"id": id, "outcome": "applied", "enabled": enabled}

    def restart_explorer(self) -> None:
        self._record("restart_explorer")
