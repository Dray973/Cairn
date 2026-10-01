"""Scheduled-maintenance part of FakeEngine: the `maintenance_*` functions, plus the journal
hooks FakeEngine calls for task-definition records.

Mixed into `FakeEngine`, which calls `_init_maintenance` from its constructor. Task Scheduler
is never touched: the task is a dict in memory, and runs are driven by the test with
`maintenance_begin_run`, `maintenance_step` and `maintenance_end_run`, as the task's own
process would write them.

Options (popped by `_init_maintenance`):
- `maintenance_task`: the registered schedule (a config dict) or None (no task);
- `maintenance_recorded` (True), `maintenance_enabled` (True), `maintenance_drift` (()): the
  task's journal record, its enabled flag and how it differs from what Cairn registers;
- `maintenance_runs`: finished runs, oldest first (see `run_dict`);
- `maintenance_blocked`: why it can't be turned on; `maintenance_program_safe` (True);
- `maintenance_on_battery`, `maintenance_has_battery` (False);
- `maintenance_task_result`: the task's last result code (None: it hasn't run yet);
- `maintenance_errors`: function name -> message of the RuntimeError it raises.
"""

from __future__ import annotations

import json
from datetime import UTC, datetime
from typing import TYPE_CHECKING, Any

SID = "S-1-5-21-1111111111-2222222222-3333333333-1001"
TASK_PATH = f"\\Cairn\\Maintenance-{SID}"
PROGRAM = "C:\\Program Files\\Cairn\\cairn-maintenance.exe"
JOURNAL = "C:\\Users\\Test\\AppData\\Local\\PCOptimizer\\journal.db"
ACCOUNT = "TEST-PC\\Test"
NEXT_RUN = "2026-10-04T12:00:00"
PROGRAM_UNSAFE_TEXT = (
    "Scheduled maintenance needs Cairn installed where only administrators can change its files, "
    "such as Program Files."
)
NOT_ELEVATED_TEXT = "this operation requires an elevated (Administrator) process"
UNRECORDED_TEXT = (
    "A maintenance task for your account exists in Task Scheduler, but Cairn's journal has no "
    "record of creating it (the journal may have been reset). Cairn doesn't change a task it can't undo."
)
TASK_DISABLED_TEXT = "The task is disabled in Task Scheduler. Save the schedule again to turn it back on."

# (id, title, per_user, default_on, servicing_guard, requires_admin)
TARGETS = (
    ("user_temp", "Temporary files", True, True, False, False),
    ("windows_temp", "Windows temp folder", False, True, False, True),
    ("update_cache", "Windows Update downloads", False, True, True, True),
    ("delivery_optimization", "Delivery Optimization cache", False, True, True, True),
    ("crash_dumps", "Crash dumps", True, False, False, True),
    ("error_reports", "Error reports", True, True, False, True),
    ("thumbnail_cache", "Thumbnail cache", True, False, False, False),
    ("shader_cache", "GPU shader cache", True, False, False, False),
    ("browser_chrome", "Chrome cache", True, False, False, False),
    ("browser_edge", "Edge cache", True, False, False, False),
    ("browser_firefox", "Firefox cache", True, False, False, False),
)
DEFAULTS: dict[str, Any] = {
    "day": "sunday",
    "time": "12:00",
    "targets": [t[0] for t in TARGETS if t[3]],
    "system_file_check": True,
    "component_store_check": True,
}
DAYS = ("monday", "tuesday", "wednesday", "thursday", "friday", "saturday", "sunday")


def _schedule_time(value: Any) -> str:
    """ "HH:MM" of a time the engine accepts ("H:MM" or "HH:MM", 24-hour clock); ValueError
    for anything else, including a missing time."""
    text = value.strip() if isinstance(value, str) else ""
    hour, sep, minute = text.partition(":")
    if (
        not sep
        or not (hour.isascii() and hour.isdigit() and len(hour) <= 2)
        or not (minute.isascii() and minute.isdigit() and len(minute) == 2)
        or int(hour) > 23
        or int(minute) > 59
    ):
        raise ValueError(
            f"invalid maintenance schedule: not a time of day: {value!r}; expected HH:MM on the "
            "24-hour clock, such as 12:00"
        )
    return f"{int(hour):02d}:{minute}"


def _utc_now() -> str:
    """Now as an RFC 3339 time in UTC, like the times the journal writes."""
    return datetime.now(UTC).isoformat(timespec="seconds")


def target_dicts() -> list[dict[str, Any]]:
    return [
        {
            "id": tid,
            "title": title,
            "description": f"{title} description.",
            "per_user": per_user,
            "default_on": default_on,
            "servicing_guard": guard,
            "requires_admin": admin,
        }
        for tid, title, per_user, default_on, guard, admin in TARGETS
    ]


def run_dict(
    run_id: int,
    state: str = "completed",
    *,
    headline: str = "Freed 1.2 GB",
    attention: tuple[str, ...] | list[str] = (),
    stopped_reason: str | None = None,
    started_at: str = "2026-09-27T12:00:00Z",
    ended_at: str | None = "2026-09-27T12:14:00Z",
    acknowledged: bool = False,
    origin: str = "task",
    log_path: str
    | None = "C:\\Users\\Test\\AppData\\Local\\PCOptimizer\\maintenance\\20260927-120000-maintenance.log",
    progress: dict[str, Any] | None = None,
    checks: list[dict[str, Any]] | None = None,
) -> dict[str, Any]:
    """A run as `maintenance_status()["runs"]` lists it; a running one has no report."""
    request = {
        "targets": list(DEFAULTS["targets"]),
        "system_file_check": True,
        "component_store_check": True,
        "origin": origin,
    }
    report = None
    if state != "running":
        report = {
            "run_id": run_id,
            "origin": origin,
            "state": state,
            "started_at": started_at,
            "ended_at": ended_at,
            "duration_ms": 14 * 60 * 1000,
            "request": request,
            "cleanup": {
                "outcome": "ok",
                "freed_bytes": 1_288_490_189,
                "deleted_files": 120,
                "skipped_files": 3,
                "targets": [],
                "error": None,
            },
            "checks": checks
            if checks is not None
            else [
                {
                    "tool": "sfc_verify",
                    "title": "Check system files",
                    "command_line": "sfc.exe /verifyonly",
                    "outcome": "attention" if attention else "ok",
                    "text": "Windows found damaged system files."
                    if attention
                    else "No integrity violations found.",
                    "windows_message": None,
                    "hint": "Open Tools and run Repair system files." if attention else None,
                    "exit_code_hex": "0x00000000",
                    "restart_required": False,
                    "log_path": None,
                    "elapsed_ms": 600_000,
                }
            ],
            "stopped_reason": stopped_reason,
            "attention": list(attention),
            "headline": headline,
            "log_path": log_path,
        }
    return {
        "id": run_id,
        "origin": origin,
        "state": state,
        "started_at": started_at,
        "ended_at": None if state == "running" else ended_at,
        "request": request,
        "progress": progress,
        "report": report,
        "log_path": log_path,
        "acknowledged": acknowledged,
        "stale": False,
    }


class FakeMaintenance:
    """Scheduled maintenance with an in-memory task, journal record and runs."""

    if TYPE_CHECKING:
        elevated: bool

        def _record(self, name: str, *args: Any) -> None: ...
        def _record_quick(self, name: str, *args: Any) -> None: ...

    def _init_maintenance(self, options: dict[str, Any]) -> None:
        """Pops the maintenance options this fake understands from `options`."""
        task = options.pop("maintenance_task", None)
        self.maintenance_task: dict[str, Any] | None = dict(task) if task is not None else None
        self.maintenance_recorded: bool = options.pop("maintenance_recorded", True)
        self.maintenance_enabled: bool = options.pop("maintenance_enabled", True)
        self.maintenance_drift: list[str] = list(options.pop("maintenance_drift", ()))
        self.maintenance_runs: list[dict[str, Any]] = [dict(r) for r in options.pop("maintenance_runs", [])]
        self.maintenance_blocked: str | None = options.pop("maintenance_blocked", None)
        self.maintenance_program_safe: bool = options.pop("maintenance_program_safe", True)
        self.maintenance_on_battery: bool = options.pop("maintenance_on_battery", False)
        self.maintenance_has_battery: bool = options.pop("maintenance_has_battery", False)
        self.maintenance_task_result: int | None = options.pop("maintenance_task_result", None)
        self.maintenance_errors: dict[str, str] = dict(options.pop("maintenance_errors", {}))
        # The run the task's process is running now.
        self._maintenance_current: dict[str, Any] | None = None
        self._maintenance_watching = False
        self._maintenance_waiting = False
        self._maintenance_timed_out = False
        # Journal record of the task: (record id, active); the next id.
        self._maintenance_record_id = (
            1 if self.maintenance_task is not None and self.maintenance_recorded else 0
        )
        self._maintenance_record_ids = 1

    # -- helpers -------------------------------------------------------------------------

    def _maintenance_fail(self, name: str) -> None:
        message = self.maintenance_errors.get(name)
        if message is not None:
            raise RuntimeError(message)

    def _maintenance_has_record(self) -> bool:
        return self.maintenance_task is not None and self.maintenance_recorded

    def _maintenance_blocked_reason(self) -> str | None:
        if self.maintenance_blocked:
            return self.maintenance_blocked
        if not self.maintenance_program_safe:
            return PROGRAM_UNSAFE_TEXT
        if self.maintenance_task is not None and not self.maintenance_recorded:
            return UNRECORDED_TEXT
        return None

    def _maintenance_check_config(self, config: Any) -> dict[str, Any]:
        """The config as the engine accepts it (a dict or its JSON text); ValueError like the
        engine's argument check."""
        if isinstance(config, str):
            try:
                config = json.loads(config)
            except json.JSONDecodeError as exc:
                raise ValueError(f"invalid maintenance schedule: {exc}") from None
        if not isinstance(config, dict):
            raise ValueError("invalid maintenance schedule: expected a mapping")
        unknown = set(config) - {"day", "time", "targets", "system_file_check", "component_store_check"}
        if unknown:
            raise ValueError(f"invalid maintenance schedule: unknown field {sorted(unknown)[0]}")
        if config.get("day") not in DAYS:
            raise ValueError(f"invalid maintenance schedule: day {config.get('day')!r}")
        time = _schedule_time(config.get("time"))
        known = [t[0] for t in TARGETS]
        targets = list(config.get("targets") or [])
        if "recycle_bin" in targets:
            raise ValueError("the Recycle Bin is never emptied by scheduled maintenance")
        for target in targets:
            if target not in known:
                raise ValueError(f"{target!r} is not a cleanup location scheduled maintenance can clean")
        checked = {
            "day": config["day"],
            "time": time,
            "targets": [t for t in known if t in targets],
            "system_file_check": bool(config.get("system_file_check")),
            "component_store_check": bool(config.get("component_store_check")),
        }
        if (
            not checked["targets"]
            and not checked["system_file_check"]
            and not checked["component_store_check"]
        ):
            raise ValueError("scheduled maintenance needs at least one cleanup location or one check")
        return checked

    def _maintenance_running(self) -> bool:
        return self._maintenance_current is not None

    def _task_status(self) -> dict[str, Any] | None:
        task = self.maintenance_task
        if task is None:
            return None
        code = self.maintenance_task_result
        result = 0x41303 if code is None else code
        texts = {0: None, 0x41303: "It hasn't run yet", 11: "A step of the last run failed"}
        return {
            "enabled": self.maintenance_enabled,
            "state": ("running" if self._maintenance_running() else "ready")
            if self.maintenance_enabled
            else "disabled",
            "config": dict(task),
            "drift": list(self.maintenance_drift),
            "program": PROGRAM,
            "last_run_time": None if code is None else "2026-09-27T12:00:00",
            "last_result": result,
            "last_result_hex": f"0x{result & 0xFFFFFFFF:08X}",
            "last_result_text": texts.get(result, f"It ended with code 0x{result & 0xFFFFFFFF:08X}"),
            "next_run_time": NEXT_RUN if self.maintenance_enabled else None,
            "missed_runs": 0,
        }

    # -- module surface ------------------------------------------------------------------

    def maintenance_status(self) -> dict[str, Any]:
        self._record("maintenance_status")
        self._maintenance_fail("maintenance_status")
        task = self._task_status()
        recorded = self._maintenance_has_record()
        warnings: list[str] = []
        if task is not None and recorded:
            if not task["enabled"]:
                warnings.append(TASK_DISABLED_TEXT)
            if task["drift"]:
                warnings.append("The task was changed outside Cairn: " + "; ".join(task["drift"]) + ".")
        return {
            "elevated": self.elevated,
            "account": {"sid": SID, "name": ACCOUNT, "other_user": False, "service_account": False},
            "program": {
                "path": PROGRAM,
                "safe": self.maintenance_program_safe,
                "reason": None if self.maintenance_program_safe else PROGRAM_UNSAFE_TEXT,
            },
            "task_path": TASK_PATH,
            "task": task,
            "recorded": recorded,
            "defaults": dict(DEFAULTS, targets=list(DEFAULTS["targets"])),
            "targets": target_dicts(),
            "runs": [dict(r) for r in reversed(self.maintenance_runs)][:10],
            "running": self._maintenance_running(),
            "on_battery": self.maintenance_on_battery,
            "has_battery": self.maintenance_has_battery,
            "blocked_reason": self._maintenance_blocked_reason(),
            "warnings": warnings,
        }

    def maintenance_set_schedule(self, config: Any, dry_run: bool = False) -> dict[str, Any]:
        self._record("maintenance_set_schedule", config, dry_run)
        checked = self._maintenance_check_config(config)
        self._maintenance_fail("maintenance_set_schedule")
        unchanged = (
            self.maintenance_task == checked
            and self._maintenance_has_record()
            and self.maintenance_enabled
            and not self.maintenance_drift
        )
        if dry_run:
            journal_arg = f'--journal "{JOURNAL}"'
            targets = f" --targets {','.join(checked['targets'])}" if checked["targets"] else ""
            return {
                "config": checked,
                "task_path": TASK_PATH,
                "program": PROGRAM,
                "arguments": journal_arg
                + targets
                + (" --sfc" if checked["system_file_check"] else "")
                + (" --dism" if checked["component_store_check"] else ""),
                "account": ACCOUNT,
                "next_run": NEXT_RUN,
                "creates": self.maintenance_task is None,
                "unchanged": unchanged,
                "blocked_reason": self._maintenance_blocked_reason(),
                "notes": ["This PC has a battery: maintenance waits until it's plugged in."]
                if self.maintenance_has_battery
                else [],
            }
        if not self.elevated:
            raise RuntimeError(NOT_ELEVATED_TEXT)
        blocked = self._maintenance_blocked_reason()
        if blocked:
            raise RuntimeError(blocked)
        if unchanged:
            outcome = "unchanged"
        elif self.maintenance_task is None:
            outcome = "created"
        else:
            outcome = "updated"
        if outcome == "created":
            self._maintenance_record_ids += 1
            self._maintenance_record_id = self._maintenance_record_ids
        self.maintenance_task = checked
        self.maintenance_recorded = True
        self.maintenance_enabled = True
        self.maintenance_drift = []
        return {"session_id": 7, "task_path": TASK_PATH, "outcome": outcome, "next_run": NEXT_RUN}

    def maintenance_remove_unrecorded(self, dry_run: bool = False) -> dict[str, Any]:
        self._record("maintenance_remove_unrecorded", dry_run)
        self._maintenance_fail("maintenance_remove_unrecorded")
        if self.maintenance_task is None:
            raise RuntimeError("There is no maintenance task for your account in Task Scheduler.")
        if self.maintenance_recorded:
            raise RuntimeError("Cairn has a record of creating this task; turn maintenance off instead.")
        if dry_run:
            return {"task_path": TASK_PATH, "removed": None, "planned": True}
        if not self.elevated:
            raise RuntimeError(NOT_ELEVATED_TEXT)
        self.maintenance_task = None
        return {"task_path": TASK_PATH, "removed": True, "planned": False}

    def maintenance_run_now(self) -> dict[str, Any]:
        self._record("maintenance_run_now")
        self._maintenance_fail("maintenance_run_now")
        if not self.elevated:
            raise RuntimeError(NOT_ELEVATED_TEXT)
        if not self._maintenance_has_record():
            raise RuntimeError(
                "Turn on scheduled maintenance to run it now. For a one-time cleanup, use Cleanup."
            )
        if not self.maintenance_enabled:
            raise RuntimeError(TASK_DISABLED_TEXT)
        if self._maintenance_running():
            raise RuntimeError("Maintenance is already running.")
        if self.maintenance_on_battery:
            raise RuntimeError(
                "The PC is running on battery power. Maintenance runs only when it's plugged in."
            )
        return {"requested": True, "task_path": TASK_PATH}

    def maintenance_acknowledge(self, run_id: int) -> bool:
        self._record("maintenance_acknowledge", run_id)
        self._maintenance_fail("maintenance_acknowledge")
        for run in self.maintenance_runs:
            if run["id"] == run_id and run["state"] == "running" and run is not self._maintenance_current:
                # The run being marked is not the one holding the lock, so it ended without
                # finishing: close only this run as interrupted before marking it, as the engine
                # does (or a run that started since did). Every other row is left as it is.
                run.update(state="interrupted", stale=False, ended_at=_utc_now())
        for run in self.maintenance_runs:
            if run["id"] == run_id and not run["acknowledged"] and run["state"] != "running":
                run["acknowledged"] = True
                return True
        return False

    def maintenance_open_log(self, run_id: int) -> None:
        self._record("maintenance_open_log", run_id)
        self._maintenance_fail("maintenance_open_log")
        if not any(r["id"] == run_id and r.get("log_path") for r in self.maintenance_runs):
            raise RuntimeError(f"maintenance run {run_id} has no log")

    def maintenance_watch(self, expect_start: bool = False) -> None:
        self._record_quick("maintenance_watch", expect_start)
        self._maintenance_watching = True
        if expect_start:
            self._maintenance_waiting = True
            self._maintenance_timed_out = False

    def maintenance_progress(self) -> dict[str, Any] | None:
        """The window reads this once a second for as long as it is open, so it is not recorded
        in `calls`: tests that count the calls an action makes see only the action's own."""
        if not self._maintenance_watching:
            return None
        latest = self.maintenance_runs[-1] if self.maintenance_runs else None
        running = self._maintenance_running()
        return {
            "running": running,
            "waiting_for_start": self._maintenance_waiting and not running,
            "start_timed_out": self._maintenance_timed_out,
            "run": dict(latest) if latest is not None else None,
            "observed_at": "2026-09-27T12:00:00Z",
            "error": None,
        }

    # -- drivers -------------------------------------------------------------------------

    def maintenance_begin_run(self, origin: str = "task") -> int:
        """Starts a run as the task's process would: a running row appears and holds the lock."""
        run_id = (self.maintenance_runs[-1]["id"] + 1) if self.maintenance_runs else 1
        run = run_dict(run_id, "running", origin=origin, ended_at=None, log_path=None)
        self.maintenance_runs.append(run)
        self._maintenance_current = run
        self._maintenance_waiting = False
        return run_id

    def maintenance_step(
        self, step: str, title: str, index: int, count: int, percent: float | None = None
    ) -> None:
        """Updates the progress of the running run."""
        assert self._maintenance_current is not None, "no run is running"
        self._maintenance_current["progress"] = {
            "step": step,
            "title": title,
            "index": index,
            "count": count,
            "percent": percent,
            "updated_at": "2026-09-27T12:05:00Z",
        }

    def maintenance_end_run(
        self,
        state: str = "completed",
        headline: str = "Freed 1.2 GB",
        attention: tuple[str, ...] = (),
        stopped_reason: str | None = None,
    ) -> None:
        """Ends the running run with `state`; its row gets the report."""
        current = self._maintenance_current
        assert current is not None, "no run is running"
        finished = run_dict(
            current["id"],
            state,
            headline=headline,
            attention=attention,
            stopped_reason=stopped_reason,
            origin=current["origin"],
        )
        self.maintenance_runs[self.maintenance_runs.index(current)] = finished
        self._maintenance_current = None

    def maintenance_start_times_out(self) -> None:
        """No run appeared within a minute after Run now."""
        self._maintenance_waiting = False
        self._maintenance_timed_out = True

    # -- journal hooks -------------------------------------------------------------------

    def _task_definition_active_count(self) -> int:
        """Active journal records of scheduled tasks Cairn registered."""
        return 1 if self._maintenance_has_record() else 0

    def _task_definition_export(self) -> list[dict[str, Any]]:
        """`task_definitions` rows of the journal export."""
        if not self._maintenance_has_record():
            return []
        return [
            {
                "id": self._maintenance_record_id,
                "session_id": 7,
                "recorded_at": "2026-09-26T10:00:00+00:00",
                "target": f"task {TASK_PATH}",
                "path": TASK_PATH,
                "purpose": "maintenance",
                "folder_created": True,
                "active": True,
                "reverted_at": None,
            }
        ]

    def _task_definition_revert(self, paths: list[str] | None, dry_run: bool) -> tuple[list[str], int]:
        """Deletes the recorded tasks at `paths` (None: all of them). Returns the actions and
        the number of deleted tasks."""
        if not self._maintenance_has_record():
            return [], 0
        if paths is not None and TASK_PATH.lower() not in [p.lower() for p in paths]:
            return [], 0
        actions = [f"delete the scheduled task {TASK_PATH} that Cairn created"]
        if dry_run:
            return actions, 0
        self.maintenance_task = None
        self.maintenance_recorded = True
        return actions, 1
