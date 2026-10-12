"""In-memory stand-in for the optimizer_engine extension module.

Mirrors the module's function signatures and result shapes, records every call and never
touches the system, so UI flows that apply and revert changes can be exercised safely.

State model: `applied` holds the tweak ids Cairn changed (they have journal records
and can be reverted); `preset` holds ids whose values were already at their targets (they
scan as applied but have nothing to revert); `unreadable` ids scan as unavailable with a
read warning; `missing` ids (id -> reason) scan as not on this PC with the reason as their
note and no warning, as a tweak whose requirement is not met on this PC. Disabled startup
entries are journaled as StartupApproved registry records, or as the `State` value of the
task key for packaged-app startup tasks.

Tweaks named in `scheduled_task_tweaks` (their ids must also be in `extra_tweaks`) turn off
scheduled tasks instead of writing a registry value: once applied, each of their task paths
is one scheduled-task journal record, reverted as `enable scheduled task <path>`.

Availability, undo limits, earlier versions' values and modes (options, by tweak id):

- `unavailable`: id -> (kind, text), kind one of `not_on_this_pc`, `windows_version`,
  `edition`, `retired`. Without a record the tweak scans as unavailable with the text as its
  note; with one it scans as read, offers Undo and its note adds `RECORDED_SUFFIX`. Applying
  it is skipped with the text. A retired tweak is never recommended. `missing` is the same
  as the kind `not_on_this_pc`.
- `undo_limits`: id -> what Undo cannot bring back (`undo_limit`).
- `former`: id -> active records of values an earlier version set for the tweak, as the
  scan lists them (`{"target", "original", "reason"}`, target `HKLM\\…\\Name`, original a
  value text or "not set"). They make the tweak revertible and are journal records titled
  with it; applying the tweak restores them (its outcome is then "applied"), and so does an
  Undo of the tweak or a revert that selects them.
- `mode_undo`: ids whose mode's off switch restores them although they are not recommended.
- `topics`: id -> topic, beside `TOPICS` for the sample tweaks.
- `requires`: id -> the requirement the catalog names for the tweak (`edge`, `office`, …);
  only `catalog()` carries it, as the engine's catalog does.

Read-backs and drift (Mismatch dicts: `target`, `label`, `expected`, `found`, `original`,
`back_to_original`):

- `not_kept`: id -> the mismatches a real apply of the tweak reports; the tweak then stays
  changed again (its drift is those mismatches) until it is applied without them.
- `drift`: id -> mismatches of a recorded tweak: the scan marks it partly applied with the
  engine's drift note, and `drift()` and `verify(ids)` list them. As in the engine, a retired
  tweak's scanned row lists none (its note says why it is retired); its targets read as not
  held. Applying the tweak clears them; undoing it drops them.
- `drift_entries`: entries of `drift()` for records outside the catalog (startup entries,
  DNS servers), as the engine lists them.
- `not_restored`: id -> the mismatches an Undo of the tweak reports in `not_restored`.

`drift()` and `verify(ids)` count the records they read (`checked`) and those they do not
(`unchecked`), as the engine does. The targets of an `unreadable` tweak cannot be read: each
is counted as not read and adds a `warnings` line worded like the engine's. `drift()` also
leaves unread the records that carry no written value, which earlier builds made: values an
earlier version set (`former`), Windows Update settings and app permissions. Like the
engine's, it neither reads nor counts the scheduled tasks Cairn registered (task
definitions).

`forget_targets` (Keep as is) stops tracking the selected records whose targets changed and
writes nothing: tweak records with drift, former records (whatever they hold), and the
startup and DNS records that a selected `drift_entries` entry names (a DNS selection covers
every record of the adapter). Their export rows stay as forgotten, and the selected entries
leave `drift_entries`; an entry naming a record of another mixin (a Windows Update setting)
only leaves the list. A selected record that still holds Cairn's value is refused, and so is
any selection of Store apps or task definitions. `open_url` only records the URL.

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

import copy
import json
import threading
import time
from collections.abc import Iterable
from typing import Any

from .fake_health import FakeHealth
from .fake_maintenance import FakeMaintenance
from .fake_network import FakeNetwork, canonical_guid
from .fake_permissions import FakePermissions
from .fake_profiles import FakeProfiles
from .fake_storage import FakeStorage
from .fake_sysinfo import FakeSysinfo
from .fake_tools import FakeTools
from .fake_updates import FakeUpdates

CATEGORIES = ("privacy", "gaming", "performance", "interface", "security", "bloatware")

# (id, category, recommended)
TWEAKS = (
    ("privacy.activity_history", "privacy", True),
    ("privacy.advertising_id", "privacy", True),
    ("gaming.game_mode", "gaming", True),
    ("performance.sysmain", "performance", True),
    ("interface.file_extensions", "interface", True),
)

# Topic of each sample tweak, as the engine's catalog names it.
TOPICS = {
    "privacy.activity_history": "activity",
    "privacy.advertising_id": "ads",
    "gaming.game_mode": "games",
    "performance.sysmain": "services",
    "interface.file_extensions": "explorer",
}

# Kinds of `unavailable`, as the engine serializes them.
UNAVAILABLE_KINDS = ("not_on_this_pc", "windows_version", "edition", "retired")
# Sentence the engine adds to the note of a tweak that is unavailable but has records.
RECORDED_SUFFIX = "Undo removes the settings Cairn stored."
# What the scan reports about Windows.
WINDOWS = {
    "build": {"number": 26300, "revision": 9457},
    "family": 26100,
    "version": "26H2",
    "edition_id": "Core",
    "edition": "home",
    "name": "Windows 11 Home",
}
RECORDED_AT = "2026-09-25T10:00:00+00:00"
FORGOTTEN_AT = "2026-09-26T10:00:00+00:00"
# Refusals of `forget_targets`, worded like the engine's.
FORGET_SETTINGS_ONLY = "Keep as is applies to settings only; use Undo."
FORGET_STILL_HOLDS = "{target} still holds the value Cairn set; Undo restores the original."
# A drift report's warning about a recorded target that could not be read, worded like the
# engine's, and the error an `unreadable` tweak's targets give.
DRIFT_UNREADABLE = "{target} could not be read ({error})."
READ_ERROR = "access denied"

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
    topic: str | None = None,
    unavailable: str | None = None,
    undo_limit: str | None = None,
    former: list[dict[str, Any]] | None = None,
    mode_undo: bool = False,
    drift: list[dict[str, Any]] | None = None,
) -> dict[str, Any]:
    if actions is None:
        actions = [{"state": state, "detail": f"HKLM\\Test\\{item_id} not set, target 1", "held": None}]
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
        "topic": topic,
        "unavailable": unavailable,
        "undo_limit": undo_limit,
        "former": former or [],
        "mode_undo": mode_undo,
        "drift": drift or [],
    }


def _filter(**parts: Any) -> dict[str, Any]:
    """A complete rollback filter, as the engine serializes one, with `parts` set."""
    selection: dict[str, Any] = {
        "registry": [],
        "services": [],
        "appx_families": [],
        "power": False,
        "scheduled_tasks": [],
        "dns": [],
        "task_definitions": [],
    }
    selection.update(parts)
    return selection


def _split_target(target: str) -> tuple[str, str, str]:
    """(hive, key path, value name) of a registry target text `HKLM\\…\\Name`."""
    hive, _, rest = target.partition("\\")
    key_path, _, value_name = rest.rpartition("\\")
    return hive, key_path, value_name


def _same_value(entry: dict[str, Any], hive: str, key_path: str, value_name: str) -> bool:
    """Whether a filter's registry entry selects that value; paths and names ignore case."""
    return (
        entry.get("hive") == hive
        and str(entry.get("key_path", "")).lower() == key_path.lower()
        and str(entry.get("value_name", "")).lower() == value_name.lower()
    )


def _original(text: str) -> tuple[bool, dict[str, Any] | None]:
    """(value existed, exported original) of a scan's original text such as "1" or "not set"."""
    if text == "not set":
        return False, None
    if text.isdigit():
        return True, {"type": "Dword", "value": int(text)}
    return True, {"type": "Sz", "value": text}


def drift_note(mismatches: list[dict[str, Any]]) -> str | None:
    """The scan note of a tweak whose recorded targets changed, worded like the engine's."""
    if not mismatches:
        return None
    if len(mismatches) == 1:
        [m] = mismatches
        if m.get("back_to_original"):
            return (
                f"Changed back since Cairn applied it: {m['label']} is {m['found']} again, its value "
                "before Cairn's change. Apply sets it again."
            )
        return (
            f"Changed since Cairn applied it: {m['label']} is {m['found']}, not {m['expected']}. "
            "Apply sets it again."
        )
    labels = [m["label"] for m in mismatches]
    if len(labels) == 2:
        named = f"{labels[0]} and {labels[1]}"
    else:
        named = f"{labels[0]}, {labels[1]} and {len(labels) - 2} more"
    return (
        f"Changed since Cairn applied it: {len(labels)} settings no longer hold ({named}). "
        "Apply sets them again."
    )


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
        unavailable: dict[str, tuple[str, str]] | None = None,
        undo_limits: dict[str, str] | None = None,
        former: dict[str, list[dict[str, Any]]] | None = None,
        mode_undo: Iterable[str] = (),
        topics: dict[str, str] | None = None,
        requires: dict[str, str] | None = None,
        not_kept: dict[str, list[dict[str, Any]]] | None = None,
        drift: dict[str, list[dict[str, Any]]] | None = None,
        drift_entries: Iterable[dict[str, Any]] = (),
        not_restored: dict[str, list[dict[str, Any]]] | None = None,
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
        known = {i for i, _, _ in self.tweaks}
        unknown = sorted(set(self.task_tweaks) - known)
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
        self.unavailable = {i: (kind, text) for i, (kind, text) in (unavailable or {}).items()}
        bad = sorted(kind for kind, _ in self.unavailable.values() if kind not in UNAVAILABLE_KINDS)
        if bad:
            raise ValueError(f"unknown unavailable kinds: {bad}")
        self.undo_limits = dict(undo_limits or {})
        self.former_records = {i: [dict(r) for r in records] for i, records in (former or {}).items()}
        self.mode_undo_ids = set(mode_undo)
        self.topics = {**TOPICS, **(topics or {})}
        self.requires = dict(requires or {})
        self.not_kept = {i: [dict(m) for m in ms] for i, ms in (not_kept or {}).items()}
        self.drifted = {i: [dict(m) for m in ms] for i, ms in (drift or {}).items()}
        self.drift_entries = [copy.deepcopy(e) for e in drift_entries]
        self.not_restored = {i: [dict(m) for m in ms] for i, ms in (not_restored or {}).items()}
        for option, ids in (
            ("unavailable", self.unavailable),
            ("undo_limits", self.undo_limits),
            ("former", self.former_records),
            ("requires", self.requires),
            ("not_kept", self.not_kept),
            ("drift", self.drifted),
            ("not_restored", self.not_restored),
        ):
            stray = sorted(set(ids) - known)
            if stray:
                raise ValueError(f"{option} ids are not tweaks (pass them in extra_tweaks): {stray}")
        # Export rows of the records Keep as is stopped tracking.
        self._forgotten_registry: list[dict[str, Any]] = []
        self._forgotten_tasks: list[dict[str, Any]] = []
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

    def open_url(self, url: str) -> None:
        """Records the URL; nothing is opened. Only `https://` URLs are accepted, as by the engine."""
        self._record("open_url", url)
        if not url.startswith("https://"):
            raise ValueError(f"Cairn opens only https:// links, not {url}")

    # -- tweak state -----------------------------------------------------------------

    def _unmet(self, item_id: str) -> tuple[str, str] | None:
        """(kind, text) of a tweak this PC cannot use, else None."""
        if item_id in self.unavailable:
            return self.unavailable[item_id]
        if item_id in self.missing:
            return ("not_on_this_pc", self.missing[item_id])
        return None

    def _recorded(self, item_id: str) -> bool:
        """Whether the tweak has active journal records (its own or an earlier version's)."""
        return item_id in self.applied or bool(self.former_records.get(item_id))

    def _drift_of(self, item_id: str) -> list[dict[str, Any]]:
        """Mismatches of the tweak's recorded targets; only a recorded tweak has any."""
        return self.drifted.get(item_id, []) if item_id in self.applied else []

    def _state(self, item_id: str) -> str:
        if item_id in self.unreadable:
            return "unavailable"
        if self._unmet(item_id) is not None and not self._recorded(item_id):
            return "unavailable"
        if self._drift_of(item_id):
            return "partial"
        if item_id in self.applied or item_id in self.preset:
            return "applied"
        return "not_applied"

    def _note(self, item_id: str) -> str | None:
        """The scan note: why the tweak cannot be used here, then its drift, then `notes`."""
        unmet = self._unmet(item_id)
        if unmet is not None:
            return f"{unmet[1]} {RECORDED_SUFFIX}" if self._recorded(item_id) else unmet[1]
        return drift_note(self._drift_of(item_id)) or self.notes.get(item_id)

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

    def _actions(self, item_id: str, state: str) -> list[dict[str, Any]]:
        """Scanned actions; `held` is set for targets with an active record, false where they
        drifted."""
        held = None
        if item_id in self.applied and item_id not in self.unreadable:
            held = not self._drift_of(item_id)
        actions = self._task_actions(item_id, state) or [
            {"state": state, "detail": f"HKLM\\Test\\{item_id} not set, target 1"}
        ]
        return [{**a, "held": held} for a in actions]

    def _task_paths(self, ids: Iterable[str]) -> list[str]:
        """Task paths of the scheduled-task tweaks among `ids`, in order."""
        return [p for i in ids for p in self.task_tweaks.get(i, ())]

    def _tweak_item(self, item_id: str, category: str, rec: bool) -> dict[str, Any]:
        state = self._state(item_id)
        unmet = self._unmet(item_id)
        kind = unmet[0] if unmet is not None else None
        return _item(
            item_id,
            category,
            state,
            recommended=rec and kind != "retired",
            revertible=self._recorded(item_id),
            note=self._note(item_id),
            description=self.descriptions.get(item_id),
            actions=self._actions(item_id, state),
            topic=self.topics.get(item_id),
            unavailable=kind,
            undo_limit=self.undo_limits.get(item_id),
            former=[dict(r) for r in self.former_records.get(item_id, [])],
            mode_undo=rec or item_id in self.mode_undo_ids,
            drift=[] if kind == "retired" else [dict(m) for m in self._drift_of(item_id)],
        )

    def _items(self) -> list[dict[str, Any]]:
        items = [self._tweak_item(i, c, rec) for i, c, rec in self.tweaks]
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
            rec = [
                i
                for i in group
                if i["recommended"] and i["state"] != "unavailable" and i["unavailable"] is None
            ]
            # As the engine's category status: an applied item whose earlier version's values are
            # still recorded is not done, since turning its mode on still restores them.
            done = [i for i in rec if i["state"] == "applied" and not i["former"]]
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
            "windows": copy.deepcopy(WINDOWS),
        }

    def _former_count(self) -> int:
        return sum(len(records) for records in self.former_records.values())

    def journal_summary(self) -> dict[str, Any]:
        self._record("journal_summary")
        return self._summary()

    def _summary(self) -> dict[str, Any]:
        values = [i for i in self.applied if i not in self.task_tweaks]
        return {
            "registry_active": len(values)
            + self._former_count()
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

    def _apply_result(self, item_id: str, dry_run: bool) -> dict[str, Any]:
        unmet = self._unmet(item_id)
        if unmet is not None:
            # Refused before anything is recorded, even when the tweak has records.
            return {"id": item_id, "outcome": "skipped", "details": [unmet[1]], "not_kept": []}
        former = self.former_records.get(item_id, [])
        if dry_run:
            details = [
                f"restore {r['target']} to {r['original']} (no longer part of this tweak)" for r in former
            ]
            outcome = "planned" if former or self._state(item_id) != "applied" else "already_applied"
            details.append(f"{item_id} detail")
            return {"id": item_id, "outcome": outcome, "details": details, "not_kept": []}
        details = [
            f"{r['target']}: restored to {r['original']} (no longer part of this tweak)" for r in former
        ]
        details.append(f"{item_id} detail")
        self.former_records.pop(item_id, None)
        # Restoring an earlier version's value is a change even when the tweak's own values
        # were already set.
        outcome = "already_applied" if item_id in self.preset and not former else "applied"
        not_kept: list[dict[str, Any]] = []
        if item_id not in self.preset:
            self.applied.add(item_id)
            not_kept = [dict(m) for m in self.not_kept.get(item_id, [])]
            # Applying sets every target again; a value that was not kept stays changed.
            if not_kept:
                self.drifted[item_id] = [dict(m) for m in not_kept]
            else:
                self.drifted.pop(item_id, None)
        return {"id": item_id, "outcome": outcome, "details": details, "not_kept": not_kept}

    def _apply_report(self, ids: list[str], dry_run: bool) -> dict[str, Any]:
        results = [self._apply_result(i, dry_run) for i in ids]
        work = [r for r in results if r["outcome"] != "skipped"]
        return {
            "dry_run": dry_run,
            # The engine opens a journal session only for a real apply with work to do.
            "session_id": None if dry_run or not work else 1,
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
            if i["category"] == category
            and i["recommended"]
            and i["unavailable"] is None
            and (i["state"] in ("not_applied", "partial") or i["former"])
        ]
        return self._apply_report(ids, dry_run)

    def _restore_former(self, records: list[dict[str, Any]], dry_run: bool) -> tuple[list[str], int, int]:
        """(actions, values written back, values deleted) of restoring former records."""
        actions, restored, deleted = [], 0, 0
        for record in records:
            existed, original = _original(record["original"])
            verb = "delete value" if original is None else f"restore {record['original']}"
            actions.append(f"{verb}: {record['target']}")
            if existed:
                restored += 1
            else:
                deleted += 1
        if dry_run:
            return actions, 0, 0
        return actions, restored, deleted

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
        former: Iterable[tuple[str, dict[str, Any]]] | None = None,
    ) -> dict[str, Any]:
        """Report of reverting `ids`, the former records in `former` ((tweak id, record) pairs;
        None: every former record of `ids`, as a tweak's Undo restores them), `startup_ids` and
        the DNS records of the adapters in `dns` (None: every active DNS record), merged with
        what the mixins restored: registry values (`extra_actions`, `extra_registry`) and task
        definitions.

        A scheduled-task tweak restores one record per task path; the actions follow the
        engine's restore order (registry values, scheduled tasks, task definitions, DNS
        servers)."""
        targets = [i for i in ids if i in self.applied]
        values = [i for i in targets if i not in self.task_tweaks]
        task_paths = self._task_paths(targets)
        startup = [s for s in startup_ids if s in self._startup_disabled]
        if former is None:
            selected_former = [(i, r) for i in ids for r in self.former_records.get(i, [])]
        else:
            selected_former = list(former)
        former_actions, former_restored, former_deleted = self._restore_former(
            [r for _, r in selected_former], dry_run
        )
        not_restored = [] if dry_run else [dict(m) for i in targets for m in self.not_restored.get(i, [])]
        if not dry_run:
            self.applied.difference_update(targets)
            for i in targets:
                self.drifted.pop(i, None)
            for i, record in selected_former:
                remaining = [r for r in self.former_records.get(i, []) if r is not record]
                if remaining:
                    self.former_records[i] = remaining
                else:
                    self.former_records.pop(i, None)
            self._startup_disabled.difference_update(startup)
        deleted = len(values) + len(startup)
        extra = list(extra_actions)
        definitions = list(task_definition_actions)
        # Records selected, dry run or not: the restart they need is reported either way.
        restored = deleted + len(task_paths) + len(extra) + len(definitions) + len(former_actions)
        dns_actions, dns_restored = self._dns_revert(dns, dry_run)
        return {
            "dry_run": dry_run,
            "registry_restored": 0 if dry_run else extra_registry + former_restored,
            "registry_deleted": 0 if dry_run else deleted + former_deleted,
            "services_restored": 0,
            "services_started": 0,
            "scheduled_tasks_restored": 0 if dry_run else len(task_paths),
            "appx_restored": 0,
            "appx_store_required": [],
            "power_restored": 0,
            "dns_restored": dns_restored,
            "task_definitions_deleted": 0 if dry_run else task_definitions_deleted,
            "actions": [f"delete value: {t}" for t in values + startup]
            + former_actions
            + extra
            + [f"enable scheduled task {p}" for p in task_paths]
            + definitions
            + dns_actions,
            "failures": [],
            # Like the engine, a dry run reports the restart the selected records need.
            "restart": self.revert_restart if restored else "none",
            "not_restored": not_restored,
            "warnings": [],
        }

    def revert(self, ids: list[str], dry_run: bool = False) -> dict[str, Any]:
        self._record("revert", list(ids), dry_run)
        return self._revert_report(list(ids), dry_run)

    def revert_category(self, category: str, dry_run: bool = False) -> dict[str, Any]:
        self._record("revert_category", category, dry_run)
        ids = [i for i, _, _ in self.tweaks if i.startswith(category + ".") and self._recorded(i)]
        return self._revert_report(ids, dry_run)

    def revert_all(self, dry_run: bool = False) -> dict[str, Any]:
        self._record("revert_all", dry_run)
        wu_actions, wu_restored, _ = self._wu_revert(None, dry_run)
        permission_actions, permission_restored, _ = self._permission_revert(None, dry_run)
        definition_actions, definitions_deleted = self._task_definition_revert(None, dry_run)
        return self._revert_report(
            sorted(self.applied | set(self.former_records)),
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
        """The catalog entries of the tweaks and the Store app, with the engine's keys."""
        self._record("catalog")
        default_on = {i: rec for i, _, rec in self.tweaks}
        return [
            {
                "id": i["id"],
                "kind": i["kind"],
                "category": i["category"],
                "title": i["title"],
                "description": i["description"],
                "risk": i["risk"],
                "default_on": default_on.get(i["id"], i["recommended"]),
                "restart": i["restart"],
                "requires": self.requires.get(i["id"]),
                "targets": self._targets(i["id"]),
                "topic": i["topic"],
                "windows": None,
                "retired": self.unavailable[i["id"]][1] if i["unavailable"] == "retired" else None,
                "undo_limit": i["undo_limit"],
                "former_targets": [r["target"] for r in i["former"]],
            }
            for i in self._items()
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

    @staticmethod
    def _registry_row(
        hive: str,
        key_path: str,
        value_name: str,
        *,
        original: str = "not set",
        written: str | None = None,
        forgotten: bool = False,
    ) -> dict[str, Any]:
        """An export row of a registry record (without its id)."""
        existed, value = _original(original)
        return {
            "session_id": 1,
            "recorded_at": RECORDED_AT,
            "target": f"{hive}\\{key_path}\\{value_name}",
            "hive": hive,
            "key_path": key_path,
            "value_name": value_name,
            "key_existed": True,
            "value_existed": existed,
            "original": value,
            "active": not forgotten,
            "reverted_at": FORGOTTEN_AT if forgotten else None,
            "written": written,
            "forgotten": forgotten,
        }

    @staticmethod
    def _task_row(path: str, *, forgotten: bool = False) -> dict[str, Any]:
        """An export row of a scheduled-task record (without its id)."""
        return {
            "session_id": 1,
            "recorded_at": RECORDED_AT,
            "target": f"scheduled task {path}",
            "path": path,
            "was_enabled": True,
            "active": not forgotten,
            "reverted_at": FORGOTTEN_AT if forgotten else None,
            "forgotten": forgotten,
        }

    def journal_export_json(self) -> str:
        self._record("journal_export_json")
        applied = sorted(self.applied)
        rows = [
            self._registry_row("HKLM", "Test", item_id, written="1")
            for item_id in applied
            if item_id not in self.task_tweaks
        ]
        rows += [
            self._registry_row(*_split_target(r["target"]), original=r["original"])
            for i, _, _ in self.tweaks
            for r in self.former_records.get(i, [])
        ]
        rows += [self._registry_row(*self._startup_record(e)) for e in sorted(self._startup_disabled)]
        registry = [{"id": n, **row} for n, row in enumerate(rows, start=1)]
        # The mixins' registry rows continue the numbering, then the forgotten rows. Rows of
        # the mixins carry no written value, as records of earlier builds.
        extra = self._wu_export() + self._permission_export() + self._forgotten_registry
        registry += [
            {"written": None, "forgotten": False, **row, "id": n}
            for n, row in enumerate(extra, start=len(registry) + 1)
        ]
        task_rows = [self._task_row(path) for path in self._task_paths(applied)] + self._forgotten_tasks
        scheduled_tasks = [{"id": n, **row} for n, row in enumerate(task_rows, start=1)]
        ops = [
            {
                "id": 1,
                "session_id": 1,
                "ts": RECORDED_AT,
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
                "dns": [{"forgotten": False, **row} for row in self._dns_export()],
                "task_definitions": self._task_definition_export(),
                "ops": ops,
            }
        )

    def _former_selected(self, registry: list[dict[str, Any]]) -> list[tuple[str, dict[str, Any]]]:
        """(tweak id, record) of the former records the filter's registry entries select."""
        return [
            (i, record)
            for i, records in self.former_records.items()
            for record in records
            if any(_same_value(entry, *_split_target(record["target"])) for entry in registry)
        ]

    def revert_targets(self, filter: dict[str, Any], dry_run: bool = False) -> dict[str, Any]:
        self._record("revert_targets", filter, dry_run)
        ids: list[str] = []
        startup: list[str] = []
        # Registry targets go to the mixins that own them first; the rest are earlier versions'
        # values, tweaks and startup entries.
        registry = list(filter.get("registry", []))
        wu_actions, wu_restored, registry = self._wu_revert(registry, dry_run)
        permission_actions, permission_restored, registry = self._permission_revert(registry, dry_run)
        definition_actions, definitions_deleted = self._task_definition_revert(
            list(filter.get("task_definitions", [])), dry_run
        )
        former = self._former_selected(registry)
        for r in registry:
            if any(_same_value(r, *_split_target(record["target"])) for _, record in former):
                continue
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
        # The filter selects records, so a tweak's own records leave its former ones active.
        return self._revert_report(
            ids,
            dry_run,
            startup,
            dns=filter.get("dns", []),
            extra_actions=wu_actions + permission_actions,
            extra_registry=wu_restored + permission_restored,
            task_definition_actions=definition_actions,
            task_definitions_deleted=definitions_deleted,
            former=former,
        )

    @staticmethod
    def _lower(record: tuple[str, str, str]) -> tuple[str, str, str]:
        return record[0], record[1].lower(), record[2].lower()

    # -- drift and Keep as is --------------------------------------------------------

    def _drift_entry(self, item_id: str, mismatch: dict[str, Any]) -> dict[str, Any]:
        """The drift entry of one mismatch of a catalog tweak, with the filter that selects its record."""
        paths = self.task_tweaks.get(item_id)
        if paths is None:
            selection = _filter(registry=[{"hive": "HKLM", "key_path": "Test", "value_name": item_id}])
            kind = "registry"
        else:
            target = str(mismatch.get("target", ""))
            chosen = [p for p in paths if target.lower() == f"scheduled task {p}".lower()] or list(paths)
            selection = _filter(scheduled_tasks=chosen)
            kind = "scheduled_task"
        return {
            "item": item_id,
            "kind": kind,
            "mismatch": dict(mismatch),
            "filter": selection,
            "recorded_at": RECORDED_AT,
        }

    def _catalog_drift(self, ids: Iterable[str]) -> list[dict[str, Any]]:
        wanted = set(ids)
        return [
            self._drift_entry(i, m)
            for i, _, _ in self.tweaks
            if i in wanted and i not in self.unreadable
            for m in self._drift_of(i)
        ]

    def _read_back(self, ids: Iterable[str]) -> tuple[int, int, list[str]]:
        """(read, not read, warnings) of reading the recorded targets of the tweaks `ids`; the
        targets of an `unreadable` tweak cannot be read."""
        checked, unchecked, warnings = 0, 0, []
        for item_id in ids:
            targets = self._targets(item_id)
            if item_id in self.unreadable:
                unchecked += len(targets)
                warnings += [DRIFT_UNREADABLE.format(target=t, error=READ_ERROR) for t in targets]
            else:
                checked += len(targets)
        return checked, unchecked, warnings

    @staticmethod
    def _drift_report(
        entries: list[dict[str, Any]],
        checked: int,
        unchecked: int,
        warnings: list[str],
        unchecked_no_value: int = 0,
    ) -> dict[str, Any]:
        return {
            "entries": entries,
            "checked": checked,
            "unchecked": unchecked,
            "unchecked_no_value": unchecked_no_value,
            "warnings": warnings,
            "duration_ms": 4,
        }

    def drift(self) -> dict[str, Any]:
        self._record("drift")
        recorded = [i for i, _, _ in self.tweaks if i in self.applied]
        checked, unchecked, warnings = self._read_back(recorded)
        # Startup and DNS records store what Cairn wrote, so they are read; the records earlier
        # builds made carry no written value, so they are not.
        checked += len(self._startup_disabled) + self._dns_active_count()
        # App-permission records and the values an earlier version set (`former`) carry no
        # written value, so they count as unchecked for that reason, as in the engine.
        no_value = self._permission_active_count() + self._former_count()
        unchecked += self._wu_active_count() + no_value
        entries = self._catalog_drift(recorded) + [copy.deepcopy(e) for e in self.drift_entries]
        return self._drift_report(entries, checked, unchecked, warnings, no_value)

    def verify(self, ids: list[str]) -> dict[str, Any]:
        self._record("verify", list(ids))
        recorded = [i for i in ids if i in self.applied]
        checked, unchecked, warnings = self._read_back(recorded)
        return self._drift_report(self._catalog_drift(recorded), checked, unchecked, warnings)

    def forget_targets(self, filter: dict[str, Any]) -> dict[str, Any]:
        self._record("forget_targets", filter)
        forgotten: list[dict[str, Any]] = []
        refused = [
            {"target": str(target), "error": FORGET_SETTINGS_ONLY}
            for target in [*filter.get("appx_families", []), *filter.get("task_definitions", [])]
        ]
        registry = list(filter.get("registry", []))
        task_paths = [str(p).lower() for p in filter.get("scheduled_tasks", [])]
        guids = {canonical_guid(str(g)) for g in filter.get("dns", [])}

        # Entries outside the catalog that the filter selects are no longer listed; the
        # records they name changed, so those are the ones Keep as is may stop tracking.
        changed_values: list[tuple[str, str, str]] = []
        changed_guids: set[str] = set()
        for entry in list(self.drift_entries):
            selection = entry.get("filter") or {}
            values = [(v["hive"], v["key_path"], v["value_name"]) for v in selection.get("registry", [])]
            entry_guids = guids & {canonical_guid(str(g)) for g in selection.get("dns", [])}
            if any(_same_value(r, *v) for r in registry for v in values) or entry_guids:
                self.drift_entries.remove(entry)
                forgotten.append(dict(entry["mismatch"]))
                changed_values += values
                changed_guids |= entry_guids
        changed = {self._lower(v) for v in changed_values}
        for entry_id in sorted(self._startup_disabled):
            record = self._startup_record(entry_id)
            if not any(_same_value(r, *record) for r in registry):
                continue
            if self._lower(record) in changed:
                self._startup_disabled.discard(entry_id)
                self._forgotten_registry.append(self._registry_row(*record, forgotten=True))
            else:
                target = "\\".join(record)
                refused.append({"target": target, "error": FORGET_STILL_HOLDS.format(target=target)})
        # A DNS selection covers every record of the adapter.
        self._dns_forget(changed_guids)
        for target in self._dns_active_targets(guids - changed_guids):
            refused.append({"target": target, "error": FORGET_STILL_HOLDS.format(target=target)})

        # Earlier versions' values may be kept as they are whatever they hold.
        for i, record in self._former_selected(registry):
            self.former_records[i] = [r for r in self.former_records[i] if r is not record]
            if not self.former_records[i]:
                del self.former_records[i]
            hive, key_path, value_name = _split_target(record["target"])
            self._forgotten_registry.append(
                self._registry_row(hive, key_path, value_name, original=record["original"], forgotten=True)
            )
            forgotten.append(
                {
                    "target": record["target"],
                    "label": value_name,
                    "expected": record["original"],
                    "found": record["original"],
                    "original": record["original"],
                    "back_to_original": True,
                }
            )

        for i, _, _ in self.tweaks:
            if i not in self.applied:
                continue
            paths = self.task_tweaks.get(i)
            if paths is None:
                selected = any(_same_value(r, "HKLM", "Test", i) for r in registry)
            else:
                selected = any(p.lower() in task_paths for p in paths)
            if not selected:
                continue
            drift = self._drift_of(i)
            if not drift:
                for target in self._targets(i):
                    refused.append({"target": target, "error": FORGET_STILL_HOLDS.format(target=target)})
                continue
            forgotten += [dict(m) for m in drift]
            self.applied.discard(i)
            self.drifted.pop(i, None)
            if paths is None:
                self._forgotten_registry.append(
                    self._registry_row("HKLM", "Test", i, written="1", forgotten=True)
                )
            else:
                self._forgotten_tasks += [self._task_row(p, forgotten=True) for p in paths]
        return {"forgotten": forgotten, "refused": refused}

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
