"""Updates part of FakeEngine: the `updates_*` functions of winget and Windows Update, plus the
journal hooks FakeEngine calls for Windows Update settings.

Mixed into `FakeEngine`, which calls `_init_updates` from its constructor. No process is
started and nothing on the system is read or written. Options (`FakeEngine(**options)`):

- `winget` ("ready"): "ready", "missing", "outdated" (ready until a check, whose result then
  says winget is too old), "other_user" or "user_unknown".
- `winget_version` ("1.29.380"): the version a check reports.
- `upgrades`: the rows a check finds; by default Contoso Editor 1.2.0 → 1.3.0 and Fabrikam
  Player 2.0.0 → 2.1.0, both from winget.
- `installed` (["Contoso.Editor", "Fabrikam.Player"]): the ids a check reports as installed.
- `edition` ("Professional"): Windows' EditionID; "Core" is Home.
- `wu_values` ({}): the current Windows Update values by value name, such as
  {"ActiveHoursStart": 8}; `wu_policy` ({}): the policy values, such as
  {"SetDisablePauseUXAccess": 1}.
- `wu_service` ("manual") and `wu_restart_pending` (False): the Windows Update service and a
  pending restart.
- `app_list` (None): the install list; None is the six generic defaults.
- `now` ("2026-09-25T10:00:00Z"): the time the fake engine sees.
- `wu_fail` (None): a setting id whose real change raises RuntimeError.

Jobs start "running" with no result (revision 0) and change only when a test drives them:
`updates_emit`, `updates_progress`, `updates_item`, `updates_finish`, `updates_finish_scan`
and `updates_lose`. Snapshots and views are built with `fake_jobs.host_snapshot` and
`fake_jobs.host_view`, so they have exactly the engine's keys; while an app of a batch runs,
the snapshot's `detail` is `{"item": index, "progress": text or None}`, as the engine's. A
batch item's `retry` is True unless `updates_item` says otherwise, as for a failure the engine
knows a retry can't change.
Windows Update changes are journaled like the engine's registry records: the first change of
a value keeps its original, and the journal hooks export, count and revert them. Pausing while
a pause runs extends it as the engine does: only the end values move, to at most 35 days after
the pause began.
"""

from __future__ import annotations

import copy
import threading
from datetime import datetime, timedelta
from typing import TYPE_CHECKING, Any

from .fake_jobs import host_snapshot, host_view

APP_LIST_PATH = "C:\\Users\\Test\\AppData\\Local\\PCOptimizer\\app_list.json"
LOG_DIR = "C:\\Users\\Test\\AppData\\Local\\PCOptimizer\\jobs\\winget"
PACKAGE_FULL_NAME = "Microsoft.DesktopAppInstaller_1.29.380.0_x64__8wekyb3d8bbwe"
WINGET_PATH = f"C:\\Program Files\\WindowsApps\\{PACKAGE_FULL_NAME}\\winget.exe"
STORE_URI = "ms-windows-store://pdp/?ProductId=9NBLGGH4NNS1"
MIN_VERSION = "1.6.0"
NOW = "2026-09-25T10:00:00Z"
UX_SETTINGS = "SOFTWARE\\Microsoft\\WindowsUpdate\\UX\\Settings"
WU_POLICY = "SOFTWARE\\Policies\\Microsoft\\Windows\\WindowsUpdate"
NOT_ELEVATED = "this operation requires an elevated (Administrator) process"

WINGET_MISSING_TEXT = (
    "winget (App Installer) isn't set up for this account. Windows installs it shortly after your first "
    "sign-in; you can also get App Installer from the Microsoft Store. Then choose Check again."
)
OTHER_USER_TEXT = (
    "Cairn is running as a different account than the one signed in, so app updates and installs would go "
    "to that account. They are turned off; start Cairn from your own account to use them."
)
USER_UNKNOWN_TEXT = (
    "Cairn can't tell which account is signed in, so app updates and installs are turned off to be safe."
)
ELEVATED_NOTE = (
    "Apps installed only for your account are installed from an administrator app; a few refuse that and "
    "are reported as failed."
)
HOME_DEFER_TEXT = (
    "Windows 11 Home ignores this setting. On Home, a new Windows version installs only when you choose it "
    "in Settings, until your current version nears the end of its support."
)
HOME_DRIVERS_CAVEAT = "Microsoft documents this setting for Pro and higher; Windows 11 Home may ignore it."
POLICY_CAVEAT = (
    "This is a policy, so Windows Settings will say some settings are managed by your organization."
)
PAUSE_POLICY_TEXT = "Your organization turned off pausing updates on this PC."
PAUSE_LIMIT_TEXT = "Updates are already paused for the 5 weeks Windows allows from the start of this pause."
MAX_PAUSE_DAYS = 35
ACTIVE_HOURS_POLICY_TEXT = "Active hours are set by a policy on this PC."
WSUS_NOTE = (
    "Windows Update uses an update server set by your organization, so Cairn's settings may have no effect."
)
NO_AUTO_UPDATE_NOTE = "Automatic updates are turned off by a policy on this PC."
NO_ACCESS_NOTE = "A policy blocks access to Windows Update on this PC."

DEFAULT_UPGRADES: tuple[dict[str, Any], ...] = (
    {
        "id": "Contoso.Editor",
        "name": "Contoso Editor",
        "installed": "1.2.0",
        "available": "1.3.0",
        "source": "winget",
        "explicit_only": False,
        "selectable": True,
        "note": None,
    },
    {
        "id": "Fabrikam.Player",
        "name": "Fabrikam Player",
        "installed": "2.0.0",
        "available": "2.1.0",
        "source": "winget",
        "explicit_only": False,
        "selectable": True,
        "note": None,
    },
)
DEFAULT_INSTALLED = ("Contoso.Editor", "Fabrikam.Player")
DEFAULT_APPS: tuple[dict[str, str], ...] = (
    {"id": "Contoso.Browser", "name": "Contoso Browser", "category": "browsers", "source": "winget"},
    {"id": "Fabrikam.Chat", "name": "Fabrikam Chat", "category": "chat", "source": "winget"},
    {"id": "Northwind.Game", "name": "Northwind Game", "category": "gaming", "source": "winget"},
    {"id": "Tailspin.Media", "name": "Tailspin Media", "category": "media", "source": "winget"},
    {"id": "Contoso.Editor", "name": "Contoso Editor", "category": "productivity", "source": "winget"},
    {"id": "Litware.Dev", "name": "Litware Dev", "category": "developer", "source": "winget"},
)
CATEGORIES = ("browsers", "chat", "gaming", "media", "productivity", "utilities", "developer")
KINDS = ("scan", "upgrade", "install")
JOB_KINDS = {"scan": "winget_scan", "upgrade": "winget_upgrade", "install": "winget_install"}
ALREADY_RUNNING = {
    "winget_scan": "Cairn is already checking for app updates.",
    "winget_upgrade": "Cairn is already updating apps.",
    "winget_install": "Cairn is already installing apps.",
}
MAX_BATCH_ITEMS = 200
MAX_APPS = 500
# Longest job command line the engine's job host keeps, and the room a batch's command line
# keeps after an id for " and 200 more".
MAX_COMMAND_LINE = 200
MORE_IDS_ROOM = 14

# Setting id -> (History title, [(key path, value name)] in the engine's target order).
WU_SETTINGS: dict[str, tuple[str, list[tuple[str, str]]]] = {
    "pause": (
        "Windows Update: pause",
        [
            (UX_SETTINGS, "PauseFeatureUpdatesStartTime"),
            (UX_SETTINGS, "PauseQualityUpdatesStartTime"),
            (UX_SETTINGS, "PauseUpdatesStartTime"),
            (UX_SETTINGS, "PauseFeatureUpdatesEndTime"),
            (UX_SETTINGS, "PauseQualityUpdatesEndTime"),
            (UX_SETTINGS, "PauseUpdatesExpiryTime"),
        ],
    ),
    "active_hours": (
        "Windows Update: active hours",
        [
            (UX_SETTINGS, "ActiveHoursStart"),
            (UX_SETTINGS, "ActiveHoursEnd"),
            (UX_SETTINGS, "SmartActiveHoursState"),
        ],
    ),
    "exclude_drivers": ("Windows Update: skip drivers", [(WU_POLICY, "ExcludeWUDriversInQualityUpdate")]),
    "defer_feature": (
        "Windows Update: delay feature updates",
        [(WU_POLICY, "DeferFeatureUpdatesPeriodInDays"), (WU_POLICY, "DeferFeatureUpdates")],
    ),
    "restart_notify": (
        "Windows Update: restart notification",
        [(UX_SETTINGS, "RestartNotificationsAllowed2")],
    ),
}
WU_SETTING_TITLES = {
    "pause": "Pause updates",
    "active_hours": "Active hours",
    "exclude_drivers": "Skip drivers in Windows Update",
    "defer_feature": "Delay feature updates",
    "restart_notify": "Notify me before restarting",
}
# Value name -> key path, for the values the settings write.
WU_KEYS = {name: key for _, targets in WU_SETTINGS.values() for key, name in targets}

RecordKey = tuple[str, str, str]


def _parse_time(text: Any) -> datetime | None:
    try:
        return datetime.fromisoformat(str(text).replace("Z", "+00:00"))
    except (TypeError, ValueError):
        return None


def _format_time(time: datetime) -> str:
    return time.strftime("%Y-%m-%dT%H:%M:%SZ")


def _display(value: Any) -> str | None:
    """A value as the engine words it."""
    if value is None:
        return None
    if isinstance(value, int):
        return f"0x{value:08x} ({value})"
    return f'"{value}"'


def valid_package_id(text: str, source: str = "winget") -> bool:
    """The engine's package id rule."""
    if not text or len(text) > 128 or text.startswith("-"):
        return False
    if any(c.isspace() or ord(c) < 32 or ord(c) == 127 or c in '\\/:*?"<>|' for c in text):
        return False
    if source.lower() == "msstore":
        return 12 <= len(text) <= 14 and text.isascii() and text.isalnum()
    parts = text.split(".")
    return 2 <= len(parts) <= 8 and all(parts)


def _apps_text(count: int) -> str:
    return f"{count} app{'' if count == 1 else 's'}"


def batch_command_line(kind: str, ids: list[str]) -> str:
    """The engine's command line of an update or install batch: the run each app gets and as
    many of the apps' ids as fit in `MAX_COMMAND_LINE` characters."""
    line = f"winget {kind} --id <id> --exact … for {_apps_text(len(ids))}:"
    for index, package in enumerate(ids):
        separator = " " if index == 0 else ", "
        room = MAX_COMMAND_LINE if index + 1 == len(ids) else MAX_COMMAND_LINE - MORE_IDS_ROOM
        if len(line) + len(separator) + len(package) > room:
            line += f" and {len(ids) - index} more"
            break
        line += separator + package
    return line


class FakeUpdates:
    """Updates section with scripted winget jobs and journaled Windows Update values."""

    if TYPE_CHECKING:
        elevated: bool

        def _record(self, name: str, *args: Any) -> None: ...
        def _record_quick(self, name: str, *args: Any) -> None: ...

    def _init_updates(self, options: dict[str, Any]) -> None:
        """Pops the updates options (see the module docstring) from `options`."""
        self._winget = str(options.pop("winget", "ready"))
        self._winget_version = str(options.pop("winget_version", "1.29.380"))
        self._upgrades = [dict(r) for r in options.pop("upgrades", DEFAULT_UPGRADES)]
        self._installed_ids = list(options.pop("installed", DEFAULT_INSTALLED))
        self._edition = str(options.pop("edition", "Professional"))
        self._wu_values: dict[str, Any] = dict(options.pop("wu_values", {}))
        self._wu_policy: dict[str, Any] = dict(options.pop("wu_policy", {}))
        self._wu_service = str(options.pop("wu_service", "manual"))
        self._wu_restart_pending = bool(options.pop("wu_restart_pending", False))
        apps = options.pop("app_list", None)
        self._apps = [dict(a) for a in (apps if apps is not None else DEFAULT_APPS)]
        self._apps_custom = apps is not None
        self._updates_now = str(options.pop("now", NOW))
        self._wu_fail = options.pop("wu_fail", None)
        # (hive, key path, value name) lowercased -> (key path, value name, original value).
        self._wu_records: dict[RecordKey, tuple[str, str, Any]] = {}
        self._wu_sessions = 0
        self._updates_jobs: dict[int, dict[str, Any]] = {}
        self._updates_next_id = 1
        self._updates_closed = False
        self._updates_lock = threading.Lock()

    # -- winget ------------------------------------------------------------------------

    def updates_winget_status(self) -> dict[str, Any]:
        self._record("updates_winget_status")
        availability = "ready" if self._winget == "outdated" else self._winget
        message = {
            "missing": WINGET_MISSING_TEXT,
            "other_user": OTHER_USER_TEXT,
            "user_unknown": USER_UNKNOWN_TEXT,
        }.get(availability)
        location = None
        if availability == "ready":
            location = {
                "path": WINGET_PATH,
                "package_full_name": PACKAGE_FULL_NAME,
                "package_version": "1.29.380.0",
            }
        return {
            "availability": availability,
            "message": message,
            "location": location,
            "elevated": self.elevated,
            "min_version": MIN_VERSION,
            "store_uri": STORE_URI,
        }

    def _ready(self) -> bool:
        return self._winget in ("ready", "outdated")

    def _request_items(self, kind: str, items: list[dict[str, Any]] | None) -> list[dict[str, Any]]:
        """The request's items as the engine keeps them; ValueError as the engine raises it."""
        if kind not in KINDS:
            raise ValueError(f"unknown kind {kind!r}; expected one of: scan, upgrade, install")
        if kind == "scan":
            if items is not None:
                raise ValueError("a scan takes no items")
            return []
        if not items:
            raise ValueError("choose at least one app")
        if len(items) > MAX_BATCH_ITEMS:
            raise ValueError(f"at most {MAX_BATCH_ITEMS} apps can be updated or installed at once")
        kept: list[dict[str, Any]] = []
        for item in items:
            package = str(item.get("id", "")).strip()
            source = str(item.get("source") or "winget").strip()
            if not valid_package_id(package, source):
                raise ValueError(f"{package!r} isn't a winget package id")
            if any(k["id"].lower() == package.lower() for k in kept):
                continue
            kept.append(
                {
                    "id": package,
                    "source": source,
                    "name": str(item.get("name") or package),
                    "from": item.get("from"),
                    "to": item.get("to"),
                }
            )
        return kept

    def _running_job(self) -> dict[str, Any] | None:
        return next(
            (j for j in self._updates_jobs.values() if j["snapshot"]["state"] == "running"),
            None,
        )

    def _plan(self, kind: str, items: list[dict[str, Any]]) -> dict[str, Any]:
        batch = kind != "scan"
        blocked = None
        if not self._ready():
            blocked = {
                "missing": WINGET_MISSING_TEXT,
                "other_user": OTHER_USER_TEXT,
                "user_unknown": USER_UNKNOWN_TEXT,
            }.get(self._winget)
        running = self._running_job()
        if blocked is None and running is not None:
            blocked = ALREADY_RUNNING[running["snapshot"]["kind"]]
        if blocked is None and self._updates_closed:
            blocked = "Cairn is closing."
        if kind == "scan":
            title = "Check for app updates"
            lines = [
                "winget --version",
                "winget export --output <private folder>\\inventory.json --include-versions "
                "--accept-source-agreements --disable-interactivity",
                "winget upgrade --accept-source-agreements --disable-interactivity",
            ]
        else:
            title = f"{'Update' if kind == 'upgrade' else 'Install'} {_apps_text(len(items))}"
            extra = "--no-upgrade " if kind == "install" else ""
            lines = [
                f"winget {kind} --id {i['id']} --exact --source {i['source']} --silent {extra}"
                "--accept-package-agreements --accept-source-agreements --disable-interactivity"
                for i in items
            ]
        notes = [ELEVATED_NOTE] if batch and self.elevated else []
        return {
            "kind": kind,
            "title": title,
            "items": copy.deepcopy(items),
            "program": WINGET_PATH if self._ready() else None,
            "command_lines": lines,
            "requires_admin": batch,
            "irreversible": batch,
            "cancellable": True,
            "blocked_reason": blocked,
            "notes": notes,
        }

    def updates_start(
        self, kind: str, items: list[dict[str, Any]] | None = None, dry_run: bool = False
    ) -> dict[str, Any]:
        self._record("updates_start", kind, items, dry_run)
        with self._updates_lock:
            request = self._request_items(kind, items)
            plan = self._plan(kind, request)
            if dry_run:
                return {"plan": plan, "job": None}
            if plan["requires_admin"] and not self.elevated:
                raise RuntimeError(NOT_ELEVATED)
            if plan["blocked_reason"]:
                raise RuntimeError(plan["blocked_reason"])
            job_id = self._updates_next_id
            self._updates_next_id += 1
            job_kind = JOB_KINDS[kind]
            if kind == "scan":
                command_line = "winget upgrade --accept-source-agreements --disable-interactivity"
            else:
                command_line = batch_command_line(kind, [i["id"] for i in request])
            snapshot = host_snapshot(
                id=job_id,
                lane="winget",
                kind=job_kind,
                title=plan["title"],
                command_line=command_line,
                state="running",
                started_at=self._updates_now,
                cancellable=True,
                log_path=f"{LOG_DIR}\\20260925-100000-{job_kind}.log",
                logged=True,
            )
            self._updates_jobs[job_id] = {
                "snapshot": snapshot,
                "lines": [],
                "result": None,
                "revision": 0,
                "lost": False,
                "kind": kind,
                "items": request,
            }
            return {"plan": plan, "job": dict(snapshot)}

    def _job(self, job_id: int) -> dict[str, Any] | None:
        job = self._updates_jobs.get(job_id)
        return None if job is None or job["lost"] else job

    def updates_job(self, job_id: int, after: int = 0) -> dict[str, Any] | None:
        self._record_quick("updates_job", job_id, after)
        with self._updates_lock:
            job = self._job(job_id)
            return None if job is None else host_view(job["snapshot"], job["lines"], after)

    def updates_jobs(self) -> list[dict[str, Any]]:
        self._record_quick("updates_jobs")
        with self._updates_lock:
            jobs = [j for j in self._updates_jobs.values() if not j["lost"]]
            return [dict(j["snapshot"]) for j in sorted(jobs, key=lambda j: -j["snapshot"]["id"])]

    def updates_result(self, job_id: int, since: int = 0) -> dict[str, Any] | None:
        self._record_quick("updates_result", job_id, since)
        with self._updates_lock:
            job = self._job(job_id)
            if job is None or job["result"] is None or job["revision"] <= since:
                return None
            return {**copy.deepcopy(job["result"]), "revision": job["revision"]}

    def updates_cancel(self, job_id: int) -> bool:
        self._record_quick("updates_cancel", job_id)
        with self._updates_lock:
            job = self._job(job_id)
            if job is None:
                raise RuntimeError(f"no winget job with id {job_id}")
            if job["snapshot"]["state"] != "running":
                return False
            job["snapshot"]["cancel_requested"] = True
            if job["result"] is not None and job["kind"] != "scan":
                job["result"]["stopping"] = True
                self._publish(job, job["result"])
            return True

    def updates_shutdown(self) -> list[dict[str, Any]]:
        self._record_quick("updates_shutdown")
        with self._updates_lock:
            self._updates_closed = True
            outcomes = []
            for job in self._updates_jobs.values():
                snapshot = job["snapshot"]
                if snapshot["state"] != "running":
                    continue
                snapshot["state"] = "cancelled"
                snapshot["finished_at"] = self._updates_now
                outcomes.append({"id": snapshot["id"], "kind": snapshot["kind"], "action": "stopped"})
            return outcomes

    def updates_open_log(self, job_id: int) -> None:
        self._record("updates_open_log", job_id)
        if self._job(job_id) is None:
            raise RuntimeError(f"no winget job with id {job_id}")

    # -- job drivers (tests) -----------------------------------------------------------

    def _publish(self, job: dict[str, Any], result: dict[str, Any]) -> None:
        job["result"] = result
        job["revision"] += 1
        job["snapshot"]["has_result"] = True
        job["snapshot"]["result_revision"] = job["revision"]

    def _batch_result(self, job: dict[str, Any]) -> dict[str, Any]:
        if job["result"] is None:
            job["result"] = {
                "kind": job["kind"],
                "items": [
                    {
                        "id": i["id"],
                        "name": i["name"],
                        "source": i["source"],
                        "from": i.get("from"),
                        "to": i.get("to"),
                        "state": "queued",
                        "exit_code": None,
                        "exit_code_hex": None,
                        "message": None,
                        "elapsed_ms": 0,
                        "detached": False,
                        "retry": True,
                    }
                    for i in job["items"]
                ],
                "current": None,
                "done": 0,
                "total": len(job["items"]),
                "stopping": False,
                "restart_required": False,
            }
        return job["result"]

    def updates_emit(self, job_id: int, *lines: str) -> None:
        with self._updates_lock:
            self._updates_jobs[job_id]["lines"].extend(lines)

    def updates_progress(
        self, job_id: int, progress: float | None, line: str | None, item_progress: str | None = None
    ) -> None:
        """Sets the job's progress and line; during a batch, `item_progress` is what winget's
        display shows for the app that runs ("12.0 MB / 32.5 MB" or "45%"), in the job's detail
        as the engine puts it."""
        with self._updates_lock:
            job = self._updates_jobs[job_id]
            snapshot = job["snapshot"]
            snapshot["progress"] = progress
            snapshot["progress_line"] = line
            current = (job["result"] or {}).get("current")
            if job["kind"] != "scan" and current is not None:
                snapshot["detail"] = {"item": current, "progress": item_progress}

    def updates_item(
        self,
        job_id: int,
        index: int,
        state: str,
        exit_code: int | None = None,
        message: str | None = None,
        detached: bool = True,
        retry: bool = True,
    ) -> None:
        """Sets app `index` of a batch to `state` and publishes the batch; `retry` False is a
        failure trying again can't change."""
        with self._updates_lock:
            job = self._updates_jobs[job_id]
            result = self._batch_result(job)
            item = result["items"][index]
            item.update(
                state=state,
                exit_code=exit_code,
                exit_code_hex=None if exit_code is None else f"0x{exit_code & 0xFFFFFFFF:08X}",
                message=message,
                detached=detached,
                retry=retry,
            )
            if state == "running":
                result["current"] = index
                verb = "Installing" if job["kind"] == "install" else "Updating"
                job["snapshot"]["progress_line"] = f"{verb} {item['name']} ({index + 1} of {result['total']})"
                job["snapshot"]["detail"] = {"item": index, "progress": None}
            elif result["current"] == index:
                result["current"] = None
                job["snapshot"]["detail"] = None
            result["done"] = sum(1 for i in result["items"] if i["state"] not in ("queued", "running"))
            result["restart_required"] = any(i["state"] == "restart_required" for i in result["items"])
            job["snapshot"]["restart_required"] = result["restart_required"]
            self._publish(job, result)

    def updates_finish(self, job_id: int, state: str = "succeeded", summary: str | None = None) -> None:
        """Ends a job in `state`; a batch without a result publishes its queued items first."""
        with self._updates_lock:
            job = self._updates_jobs[job_id]
            if job["kind"] != "scan" and job["result"] is None:
                self._publish(job, self._batch_result(job))
            snapshot = job["snapshot"]
            snapshot["state"] = state
            snapshot["finished_at"] = self._updates_now
            snapshot["summary"] = summary
            snapshot["progress"] = None if state != "succeeded" else 100.0

    def updates_finish_scan(
        self,
        job_id: int,
        upgrades: list[dict[str, Any]] | None = None,
        installed: list[str] | None = None,
        error: dict[str, Any] | str | None = None,
        warnings: tuple[str, ...] | list[str] = (),
        unparsed_rows: int = 0,
    ) -> None:
        """Publishes a check's result (the options' rows and ids unless given) and ends it."""
        if isinstance(error, str):
            error = {"code": None, "message": error, "availability": None, "outdated": False}
        if error is None and self._winget == "outdated":
            error = {
                "code": None,
                "message": f"winget {self._winget_version} is too old; Cairn needs {MIN_VERSION} or newer. "
                "Update App Installer from the Microsoft Store, then choose Check again.",
                "availability": None,
                "outdated": True,
            }
        rows = self._upgrades if upgrades is None else upgrades
        result = {
            "kind": "scan",
            "winget_version": self._winget_version,
            "checked_at": self._updates_now,
            "upgrades": [] if error else copy.deepcopy(rows),
            "installed": list(self._installed_ids if installed is None else installed),
            "inventory_complete": True,
            "unparsed_rows": unparsed_rows,
            "warnings": list(warnings),
            "error": error,
        }
        with self._updates_lock:
            self._publish(self._updates_jobs[job_id], result)
        count = len(result["upgrades"])
        summary = (
            error["message"]
            if error
            else f"{count} updates available"
            if count
            else "All apps are up to date"
        )
        self.updates_finish(job_id, "failed" if error else "succeeded", summary)

    def updates_lose(self, job_id: int) -> None:
        """The engine forgets the job: its view and result become None."""
        with self._updates_lock:
            self._updates_jobs[job_id]["lost"] = True

    def updates_job_ids(self) -> list[int]:
        """Ids of every job started, oldest first."""
        with self._updates_lock:
            return sorted(self._updates_jobs)

    # -- Windows Update ----------------------------------------------------------------

    def _home(self) -> bool:
        return self._edition.startswith("Core")

    def _now(self) -> datetime:
        now = _parse_time(self._updates_now)
        assert now is not None, "the fake's `now` must be an RFC 3339 time"
        return now

    @staticmethod
    def _record_key(key_path: str, name: str) -> RecordKey:
        return ("hklm", key_path.lower(), name.lower())

    def _value(self, name: str) -> Any:
        return self._wu_values.get(name)

    def _marks(self, setting_id: str) -> tuple[bool, bool]:
        by_cairn = differs = False
        for key_path, name in WU_SETTINGS[setting_id][1]:
            record = self._wu_records.get(self._record_key(key_path, name))
            if record is None:
                continue
            by_cairn = True
            if self._value(name) != record[2]:
                differs = True
        return by_cairn, differs

    def _wu_setting(self, setting_id: str) -> dict[str, Any]:
        targets = WU_SETTINGS[setting_id][1]
        by_cairn, differs = self._marks(setting_id)
        setting: dict[str, Any] = {
            "id": setting_id,
            "title": WU_SETTING_TITLES[setting_id],
            "available": True,
            "unavailable_reason": None,
            "caveat": None,
            "value": {},
            "by_cairn": by_cairn,
            "differs": differs,
            "targets": [{"hive": "HKLM", "key_path": k, "value_name": n} for k, n in targets],
        }
        now = self._now()
        if setting_id == "pause":
            ends = [
                t
                for t in (
                    _parse_time(self._value("PauseUpdatesExpiryTime")),
                    _parse_time(self._value("PauseQualityUpdatesEndTime")),
                )
                if t is not None
            ]
            until = max(ends) if ends else None
            started = _parse_time(self._value("PauseUpdatesStartTime")) or _parse_time(
                self._value("PauseQualityUpdatesStartTime")
            )
            setting["value"] = {
                "kind": "pause",
                "paused": until is not None and until > now,
                "until": None if until is None else _format_time(until),
                "started": None if started is None else _format_time(started),
                "expired": until is not None and until <= now,
            }
            if self._wu_policy.get("SetDisablePauseUXAccess") == 1:
                setting["available"] = False
                setting["unavailable_reason"] = PAUSE_POLICY_TEXT
        elif setting_id == "active_hours":
            if self._wu_policy.get("SetActiveHours") == 1:
                start, end = self._wu_policy.get("ActiveHoursStart"), self._wu_policy.get("ActiveHoursEnd")
                setting["value"] = {"kind": "active_hours", "automatic": False, "start": start, "end": end}
                setting["value"]["policy"] = True
                setting["available"] = False
                setting["unavailable_reason"] = (
                    f"Active hours are set by a policy on this PC ({start:02}:00–{end:02}:00)."
                    if start is not None and end is not None
                    else ACTIVE_HOURS_POLICY_TEXT
                )
            else:
                start, end = self._value("ActiveHoursStart"), self._value("ActiveHoursEnd")
                automatic = self._value("SmartActiveHoursState") == 1 or start is None or end is None
                setting["value"] = {
                    "kind": "active_hours",
                    "automatic": automatic,
                    "start": start,
                    "end": end,
                    "policy": False,
                }
        elif setting_id == "exclude_drivers":
            setting["value"] = {"kind": "switch", "on": self._value("ExcludeWUDriversInQualityUpdate") == 1}
            setting["caveat"] = HOME_DRIVERS_CAVEAT if self._home() else POLICY_CAVEAT
        elif setting_id == "defer_feature":
            on = self._value("DeferFeatureUpdates") == 1
            setting["value"] = {
                "kind": "defer",
                "days": self._value("DeferFeatureUpdatesPeriodInDays") if on else None,
            }
            if self._home():
                setting["available"] = False
                setting["unavailable_reason"] = HOME_DEFER_TEXT
            else:
                setting["caveat"] = POLICY_CAVEAT
        else:
            setting["value"] = {"kind": "switch", "on": self._value("RestartNotificationsAllowed2") == 1}
        return setting

    def updates_wu_state(self) -> dict[str, Any]:
        self._record("updates_wu_state")
        home = self._home()
        managed = []
        if str(self._wu_policy.get("WUServer") or "").strip() or self._wu_policy.get("UseWUServer") == 1:
            managed.append(WSUS_NOTE)
        if self._wu_policy.get("NoAutoUpdate") == 1:
            managed.append(NO_AUTO_UPDATE_NOTE)
        if self._wu_policy.get("DisableWindowsUpdateAccess") == 1:
            managed.append(NO_ACCESS_NOTE)
        return {
            "edition": {
                "id": self._edition,
                "name": "Windows 11 Home" if home else "Windows 11 Pro",
                "home": home,
                "version": "25H2",
                "build": "26200.9457",
            },
            "service": self._wu_service,
            "restart_pending": self._wu_restart_pending,
            "managed": managed,
            "settings": [self._wu_setting(s) for s in WU_SETTINGS],
            "warnings": [],
        }

    def updates_wu_catalog(self) -> list[dict[str, Any]]:
        self._record_quick("updates_wu_catalog")
        return [
            {
                "id": f"wu.{setting_id}",
                "title": title,
                "targets": [f"HKLM\\{key}\\{name}" for key, name in targets],
            }
            for setting_id, (title, targets) in WU_SETTINGS.items()
        ]

    @staticmethod
    def _whole(value: Any, what: str) -> int:
        if isinstance(value, bool) or not isinstance(value, int):
            raise ValueError(f"{what} must be a whole number")
        return value

    def _wu_ops(self, setting_id: str, value: Any) -> list[tuple[str, Any]]:
        """(value name, new value or None to delete) of a change, in the engine's order;
        ValueError for a value the engine rejects."""
        if setting_id == "pause":
            if value is None:
                return [
                    (name, None)
                    for name in (
                        "PauseUpdatesExpiryTime",
                        "PauseQualityUpdatesEndTime",
                        "PauseFeatureUpdatesEndTime",
                        "PauseUpdatesStartTime",
                        "PauseQualityUpdatesStartTime",
                        "PauseFeatureUpdatesStartTime",
                    )
                ]
            days = self._whole(value, "pause days")
            if not 1 <= days <= 35:
                raise ValueError("Updates can be paused for 1 to 35 days.")
            start = self._now()
            end = start + timedelta(days=days)
            names = [n for _, n in WU_SETTINGS["pause"][1]]
            return [(n, _format_time(start)) for n in names[:3]] + [(n, _format_time(end)) for n in names[3:]]
        if setting_id == "active_hours":
            if value is None:
                return [("SmartActiveHoursState", 1)]
            if not isinstance(value, list | tuple) or len(value) != 2:
                raise ValueError("active_hours takes [start, end] or None")
            start, end = (self._whole(v, "an hour") for v in value)
            if start > 23 or end > 23:
                raise ValueError("Active hours start and end must be hours from 0 to 23.")
            if start == end:
                raise ValueError("Start and end must differ.")
            if (end + 24 - start) % 24 > 18:
                raise ValueError("Active hours can span at most 18 hours.")
            return [("ActiveHoursStart", start), ("ActiveHoursEnd", end), ("SmartActiveHoursState", 0)]
        if setting_id == "defer_feature":
            days = 0 if value is None else self._whole(value, "defer_feature days")
            if days == 0:
                return [("DeferFeatureUpdates", None), ("DeferFeatureUpdatesPeriodInDays", None)]
            if not 1 <= days <= 365:
                raise ValueError("Feature updates can be delayed by 1 to 365 days.")
            return [("DeferFeatureUpdatesPeriodInDays", days), ("DeferFeatureUpdates", 1)]
        if not isinstance(value, bool):
            raise ValueError(f"{setting_id} takes True or False")
        name = (
            "ExcludeWUDriversInQualityUpdate"
            if setting_id == "exclude_drivers"
            else "RestartNotificationsAllowed2"
        )
        return [(name, 1 if value else None)]

    def _extend_pause(self, days: int, ops: list[tuple[str, Any]]) -> list[tuple[str, Any]]:
        """The writes of a pause of `days` (`ops` start one now): a running pause keeps its start
        and its end moves `days` later, to at most `MAX_PAUSE_DAYS` after it began (after now when
        its start can't be read); RuntimeError when that end is not later than its current one."""
        now = self._now()
        ends = [
            t
            for t in (
                _parse_time(self._value("PauseUpdatesExpiryTime")),
                _parse_time(self._value("PauseQualityUpdatesEndTime")),
            )
            if t is not None
        ]
        until = max(ends) if ends else None
        if until is None or until <= now:
            return ops
        started = _parse_time(self._value("PauseUpdatesStartTime")) or _parse_time(
            self._value("PauseQualityUpdatesStartTime")
        )
        began = now if started is None else min(started, now)
        end = min(until + timedelta(days=days), began + timedelta(days=MAX_PAUSE_DAYS))
        if end <= until:
            raise RuntimeError(PAUSE_LIMIT_TEXT)
        return [(name, _format_time(end)) for _, name in WU_SETTINGS["pause"][1][3:]]

    def updates_wu_set(
        self, setting: str, value: Any = None, restore_point: str = "skip", dry_run: bool = False
    ) -> dict[str, Any]:
        self._record("updates_wu_set", setting, value, restore_point, dry_run)
        setting_id = str(setting).strip().lower().replace("-", "_")
        if setting_id not in WU_SETTINGS:
            raise ValueError(f"unknown Windows Update setting {setting!r}")
        if restore_point not in ("skip", "try", "require"):
            raise ValueError(f"restore_point must be 'skip', 'try' or 'require', got {restore_point!r}")
        ops = self._wu_ops(setting_id, value)
        delays = setting_id == "defer_feature" and any(v is not None for _, v in ops)
        if delays and self._home():
            raise RuntimeError(HOME_DEFER_TEXT)
        pausing = setting_id == "pause" and value is not None
        if pausing and self._wu_policy.get("SetDisablePauseUXAccess") == 1:
            raise RuntimeError(PAUSE_POLICY_TEXT)
        if setting_id == "active_hours" and self._wu_policy.get("SetActiveHours") == 1:
            raise RuntimeError(ACTIVE_HOURS_POLICY_TEXT)
        if pausing:
            ops = self._extend_pause(value, ops)
        warnings = [HOME_DRIVERS_CAVEAT] if setting_id == "exclude_drivers" and value and self._home() else []
        writes = [
            {
                "target": f"HKLM\\{WU_KEYS[name]}\\{name}",
                "before": _display(self._value(name)),
                "after": _display(new),
                "outcome": None,
            }
            for name, new in ops
        ]
        if dry_run:
            return {
                "dry_run": True,
                "setting": setting_id,
                "session_id": None,
                "writes": writes,
                "warnings": warnings,
            }
        if not self.elevated:
            raise RuntimeError(NOT_ELEVATED)
        if self._wu_fail == setting_id:
            raise RuntimeError(f"cannot write {setting_id}: access denied")
        self._wu_sessions += 1
        for write, (name, new) in zip(writes, ops, strict=True):
            key = self._record_key(WU_KEYS[name], name)
            current = self._value(name)
            # The first change of a value keeps its original, as the engine's journal does.
            self._wu_records.setdefault(key, (WU_KEYS[name], name, current))
            if current == new:
                write["outcome"] = "already_in_desired_state"
                continue
            if new is None:
                self._wu_values.pop(name, None)
            else:
                self._wu_values[name] = new
            write["outcome"] = "applied"
        return {
            "dry_run": False,
            "setting": setting_id,
            "session_id": 100 + self._wu_sessions,
            "writes": writes,
            "warnings": warnings,
        }

    # -- install list ------------------------------------------------------------------

    def updates_app_list(self) -> dict[str, Any]:
        self._record("updates_app_list")
        return {
            "apps": copy.deepcopy(self._apps),
            "custom": self._apps_custom,
            "path": APP_LIST_PATH,
            "warnings": [],
        }

    def updates_save_app_list(self, apps: list[dict[str, Any]] | None = None) -> dict[str, Any]:
        self._record("updates_save_app_list", apps)
        if apps is None:
            self._apps = [dict(a) for a in DEFAULT_APPS]
            self._apps_custom = False
            return self.updates_app_list()
        if len(apps) > MAX_APPS:
            raise ValueError(f"The list can hold at most {MAX_APPS} apps.")
        kept: list[dict[str, Any]] = []
        for app in apps:
            package = str(app.get("id", "")).strip()
            name = str(app.get("name", "")).strip()
            category = str(app.get("category", ""))
            source = str(app.get("source") or "winget")
            if not valid_package_id(package, source):
                raise ValueError(f"{package!r} isn't a winget package id.")
            if not 1 <= len(name) <= 80:
                raise ValueError("An app's name must have 1 to 80 characters.")
            if category not in CATEGORIES:
                raise ValueError(f"invalid app list: unknown category {category!r}")
            if any(k["id"].lower() == package.lower() for k in kept):
                raise ValueError(f"{package} is on the list twice.")
            kept.append({"id": package, "name": name, "category": category, "source": source})
        self._apps = kept
        self._apps_custom = True
        return self.updates_app_list()

    # -- journal hooks -------------------------------------------------------------------

    def _wu_active_count(self) -> int:
        """Active journal records of Windows Update settings."""
        return len(self._wu_records)

    def _wu_export(self) -> list[dict[str, Any]]:
        """Registry rows of the journal export for Windows Update settings."""
        rows = []
        for key_path, name, original in self._wu_records.values():
            if original is None:
                recorded = None
            elif isinstance(original, int):
                recorded = {"type": "Dword", "value": original}
            else:
                recorded = {"type": "Sz", "value": original}
            rows.append(
                {
                    "id": 0,
                    "session_id": 100,
                    "recorded_at": "2026-09-25T10:00:00+00:00",
                    "target": f"HKLM\\{key_path}\\{name}",
                    "hive": "HKLM",
                    "key_path": key_path,
                    "value_name": name,
                    "key_existed": True,
                    "value_existed": original is not None,
                    "original": recorded,
                    "active": True,
                    "reverted_at": None,
                }
            )
        return rows

    def _wu_revert(
        self, targets: list[dict[str, Any]] | None, dry_run: bool
    ) -> tuple[list[str], int, list[dict[str, Any]]]:
        """Restores the Windows Update records among the registry `targets` (None: all of them).
        Returns the actions, the number of restored values and the targets this fake does
        not own."""
        if targets is None:
            owned = list(self._wu_records)
            remaining: list[dict[str, Any]] = []
        else:
            owned, remaining = [], []
            for target in targets:
                key = (
                    str(target.get("hive", "")).lower(),
                    str(target.get("key_path", "")).lower(),
                    str(target.get("value_name", "")).lower(),
                )
                if key in self._wu_records:
                    owned.append(key)
                else:
                    remaining.append(target)
        actions = []
        for key in owned:
            key_path, name, original = self._wu_records[key]
            target = f"HKLM\\{key_path}\\{name}"
            actions.append(
                f"delete value: {target}" if original is None else f"restore {_display(original)}: {target}"
            )
            if dry_run:
                continue
            if original is None:
                self._wu_values.pop(name, None)
            else:
                self._wu_values[name] = original
            del self._wu_records[key]
        return actions, len(owned), remaining
