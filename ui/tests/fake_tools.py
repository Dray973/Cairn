"""Maintenance-tools part of FakeEngine: the `tools_*` functions and their jobs.

Mixed into `FakeEngine`, which calls `_init_tools` from its constructor. No process is ever
started: a job exists only in memory, and a test drives it with `tool_emit` (output lines and
progress), `tool_exit` (its process ended and its result is being judged; it still runs) and
`tool_finish` (its final state). Polling reads it through `tools_job` in pages, like the
engine's runner, which keeps the newest 2000 lines of each job.

Options (`FakeEngine(**options)`): `restore_enabled` (True; False for System Protection off,
None makes `system_restore_enabled` raise), `restore_point_error` (`tools_restore_point`
raises it), `tool_start_error` (every real start raises it), `tool_blocked` (tool id ->
blocking reason in the plan), `tool_detached` (whether jobs of tools that cannot be
stopped run detached from Cairn, True by default) and `tool_storage_busy` (drive letter ->
title of a disk speed test running on it, which blocks the drive tools of that volume like
the engine's runner).

`tools_windows` lists `WINDOWS_TOOLS`; `tools_open_windows` also opens the `FIX_TOOLS` that
security fixes use, like the engine.
"""

from __future__ import annotations

import threading
import time
from collections import deque
from typing import TYPE_CHECKING, Any

SYSTEM32 = "C:\\Windows\\System32"
NOT_ELEVATED = "this operation requires an elevated (Administrator) process"
CLOSING = "Cairn is closing; the tool was not started"
MANUAL_RESTORE_POINT_DESCRIPTION = "Cairn manual checkpoint"
# Stand-in for the drive letter in a tool's arguments.
VOLUME_ARG = "<volume>"
MAX_LINES_PER_VIEW = 500
MAX_JOB_LINES = 2000
KEPT_FINISHED_JOBS = 10
STARTED_AT = "2026-09-25T10:00:00+00:00"
GIB = 1024**3

# (id, group, title, verb, needs_volume, cancellable, requires_detach, changes_system, program, args)
TOOLS: tuple[tuple[str, str, str, str, bool, bool, bool, bool, str, tuple[str, ...]], ...] = (
    (
        "sfc_verify",
        "system_files",
        "Check system files",
        "Check",
        False,
        False,
        False,
        False,
        "sfc.exe",
        ("/verifyonly",),
    ),
    (
        "sfc_scan",
        "system_files",
        "Repair system files",
        "Repair",
        False,
        False,
        True,
        True,
        "sfc.exe",
        ("/scannow",),
    ),
    (
        "dism_check",
        "system_files",
        "Check the component store",
        "Check",
        False,
        False,
        False,
        False,
        "dism.exe",
        ("/Online", "/Cleanup-Image", "/CheckHealth"),
    ),
    (
        "dism_scan",
        "system_files",
        "Scan the component store",
        "Scan",
        False,
        False,
        False,
        False,
        "dism.exe",
        ("/Online", "/Cleanup-Image", "/ScanHealth"),
    ),
    (
        "dism_restore",
        "system_files",
        "Repair the component store",
        "Repair",
        False,
        False,
        True,
        True,
        "dism.exe",
        ("/Online", "/Cleanup-Image", "/RestoreHealth", "/NoRestart"),
    ),
    (
        "drive_optimize",
        "drives",
        "Optimize drive",
        "Optimize",
        True,
        False,
        False,
        True,
        "defrag.exe",
        (VOLUME_ARG, "/O", "/U", "/V"),
    ),
    (
        "drive_retrim",
        "drives",
        "Retrim SSD",
        "Retrim",
        True,
        False,
        False,
        True,
        "defrag.exe",
        (VOLUME_ARG, "/L", "/U", "/V"),
    ),
    (
        "disk_check",
        "drives",
        "Check disk (read-only)",
        "Check",
        True,
        True,
        False,
        False,
        "chkdsk.exe",
        (VOLUME_ARG,),
    ),
)

DESCRIPTIONS = {
    "sfc_verify": "System File Checker compares protected Windows files with known-good copies and "
    "reports problems without changing anything.",
    "sfc_scan": "System File Checker replaces damaged or missing protected Windows files with good copies "
    "from the component store.",
    "dism_check": "DISM reports whether Windows' component store (the source System File Checker repairs "
    "from) is already marked as damaged.",
    "dism_scan": "DISM scans the component store for damage. Nothing is repaired.",
    "dism_restore": "DISM downloads good copies of damaged components from Windows Update and repairs the "
    "component store. Run Repair system files again afterwards.",
    "drive_optimize": "Runs the optimization Windows picks for the drive: TRIM for SSDs, defragmentation "
    "for hard disks.",
    "drive_retrim": "Tells the SSD which blocks are free so it keeps its write speed. Only for SSDs and "
    "thin-provisioned drives.",
    "disk_check": "Check Disk scans the drive's file system for errors and reports them without fixing "
    "anything.",
}

DURATION_HINTS = {
    "sfc_verify": "usually 10–30 minutes",
    "sfc_scan": "usually 10–30 minutes",
    "dism_check": "under a minute",
    "dism_scan": "usually 5–20 minutes",
    "dism_restore": "usually 10–60 minutes; needs internet",
    "drive_optimize": "SSD: about a minute; hard disk: minutes to hours",
    "drive_retrim": "about a minute",
    "disk_check": "a few minutes; longer on large hard disks",
}

VOLUMES: tuple[dict[str, Any], ...] = (
    {
        "letter": "C:",
        "label": "Windows",
        "file_system": "NTFS",
        "size_bytes": 952 * GIB,
        "free_bytes": 611 * GIB,
        "media": "ssd",
        "trim": True,
        "system": True,
        "error": None,
        "optimize_blocked": None,
        "retrim_blocked": None,
        "check_blocked": None,
    },
    {
        "letter": "D:",
        "label": "Data",
        "file_system": "NTFS",
        "size_bytes": 1863 * GIB,
        "free_bytes": 1210 * GIB,
        "media": "hdd",
        "trim": None,
        "system": False,
        "error": None,
        "optimize_blocked": None,
        "retrim_blocked": "Retrim is for SSDs; this drive is a hard disk",
        "check_blocked": None,
    },
)

WINDOWS_TOOLS: tuple[dict[str, Any], ...] = (
    {
        "id": "task_manager",
        "title": "Task Manager",
        "description": "See and end running apps and processes.",
        "requires_admin": False,
    },
    {
        "id": "event_viewer",
        "title": "Event Viewer",
        "description": "Windows and application logs.",
        "requires_admin": True,
    },
    {
        "id": "system_protection",
        "title": "System Protection",
        "description": "Turn System Restore on and manage restore points.",
        "requires_admin": True,
    },
)

# Windows tools that fixes open but the Tools section does not list.
FIX_TOOLS: tuple[dict[str, Any], ...] = (
    {
        "id": "uac_settings",
        "title": "User Account Control settings",
        "description": "Choose when Windows asks before apps make changes.",
        "requires_admin": False,
    },
    {
        "id": "remote_settings",
        "title": "Remote settings",
        "description": "Remote Assistance and Remote Desktop settings of this PC.",
        "requires_admin": True,
    },
    {
        "id": "user_accounts",
        "title": "User Accounts",
        "description": "Accounts on this PC and whether they must enter a password to sign in.",
        "requires_admin": True,
    },
    {
        "id": "windows_features",
        "title": "Windows Features",
        "description": "Turn optional Windows components on or off.",
        "requires_admin": True,
    },
)
# Tools that work on a drive and wait for a disk speed test running on it.
DRIVE_TOOLS = ("drive_optimize", "drive_retrim", "disk_check")

# Every key of the engine's JobSnapshot.
JOB_SNAPSHOT_KEYS = (
    "id",
    "tool",
    "title",
    "command_line",
    "volume",
    "state",
    "started_at",
    "finished_at",
    "elapsed_ms",
    "idle_ms",
    "progress",
    "progress_line",
    "exit_code",
    "exit_code_hex",
    "cancellable",
    "cancel_requested",
    "detached",
    "restart_required",
    "hint",
    "summary",
    "log_path",
    "raw_log_path",
    "line_count",
    "logged",
)
# Keys a JobView adds to the snapshot keys.
JOB_VIEW_KEYS = ("lines", "first", "next", "skipped", "more")


def exit_code_hex(code: int) -> str:
    return f"0x{code & 0xFFFFFFFF:08X}"


def catalog_entry(tool_id: str) -> dict[str, Any]:
    """The `tools_catalog` row of `tool_id`."""
    spec = next(t for t in TOOLS if t[0] == tool_id)
    _, group, title, verb, needs_volume, cancellable, detach, changes, program, _ = spec
    return {
        "id": tool_id,
        "group": group,
        "title": title,
        "verb": verb,
        "description": DESCRIPTIONS[tool_id],
        "needs_volume": needs_volume,
        "requires_admin": True,
        "cancellable": cancellable,
        "requires_detach": detach,
        "changes_system": changes,
        "duration_hint": DURATION_HINTS[tool_id],
        "program": program,
    }


def normalize_volume(volume: str) -> str:
    """A drive letter given as "c", "C:" or "c:\\", as "C:"; anything else is a ValueError."""
    text = volume.strip()
    if text.endswith("\\"):
        text = text[:-1]
    if text.endswith(":"):
        text = text[:-1]
    if len(text) != 1 or not ("A" <= text.upper() <= "Z"):
        raise ValueError(f"not a drive letter: {volume!r}")
    return f"{text.upper()}:"


class _Job:
    """One fake job: its snapshot fields and its retained output lines."""

    def __init__(self, snapshot: dict[str, Any]) -> None:
        self.fields = snapshot
        self.lines: deque[tuple[int, str]] = deque(maxlen=MAX_JOB_LINES)
        self.seq = 0
        self.started = time.monotonic()
        self.last_output = self.started

    @property
    def running(self) -> bool:
        return self.fields["state"] == "running"

    def snapshot(self) -> dict[str, Any]:
        snap = dict(self.fields)
        if self.running:
            now = time.monotonic()
            snap["elapsed_ms"] = int((now - self.started) * 1000)
            snap["idle_ms"] = int((now - self.last_output) * 1000)
        return snap

    def finish(self, state: str, exit_code: int | None, **fields: Any) -> None:
        if self.running:
            self.fields["elapsed_ms"] = int((time.monotonic() - self.started) * 1000)
        self.fields.update(
            state=state,
            finished_at=STARTED_AT,
            idle_ms=0,
            exit_code=exit_code,
            exit_code_hex=None if exit_code is None else exit_code_hex(exit_code),
            **fields,
        )


class FakeTools:
    """In-memory tool catalog, volumes and jobs; no process is started."""

    if TYPE_CHECKING:
        elevated: bool

        def _record(self, name: str, *args: Any) -> None: ...
        def _record_quick(self, name: str, *args: Any) -> None: ...

    def _init_tools(self, options: dict[str, Any]) -> None:
        """Pops the tools options this fake understands from `options`."""
        self._restore_enabled: bool | None = options.pop("restore_enabled", True)
        self._restore_point_error: str | None = options.pop("restore_point_error", None)
        self._tool_start_error: str | None = options.pop("tool_start_error", None)
        self._tool_blocked: dict[str, str] = dict(options.pop("tool_blocked", None) or {})
        self._tool_detached = bool(options.pop("tool_detached", True))
        self._tool_storage_busy: dict[str, str] = dict(options.pop("tool_storage_busy", None) or {})
        self._tools_lock = threading.RLock()
        self._tool_jobs: dict[int, _Job] = {}
        self._tool_next_id = 1
        self._tools_closed = False

    # -- helpers -------------------------------------------------------------------------

    def _running_tool_job(self) -> _Job | None:
        return next((j for j in self._tool_jobs.values() if j.running), None)

    def _tool_job_or_error(self, job_id: int) -> _Job:
        job = self._tool_jobs.get(job_id)
        if job is None:
            raise RuntimeError(f"no tool job with id {job_id}")
        return job

    def _tool_plan(self, tool: str, volume: str | None) -> dict[str, Any]:
        """The ToolPlan of `tool`; raises ValueError for an unknown tool or a wrong volume."""
        if tool not in DESCRIPTIONS:
            raise ValueError(f"unknown tool {tool!r}; expected one of {', '.join(DESCRIPTIONS)}")
        info = catalog_entry(tool)
        if info["needs_volume"] and volume is None:
            raise ValueError(f"{tool} needs a volume")
        if not info["needs_volume"] and volume is not None:
            raise ValueError(f"{tool} does not take a volume")
        letter = normalize_volume(volume) if volume is not None else None
        spec = next(t for t in TOOLS if t[0] == tool)
        args = [letter if a == VOLUME_ARG else a for a in spec[9]]
        listed = next((v for v in VOLUMES if v["letter"] == letter), None)

        blocked: str | None = None
        running = self._running_tool_job()
        if not self.elevated:
            blocked = "needs administrator rights"
        elif running is not None:
            blocked = f"{running.fields['title']} is already running in Cairn"
        elif tool in self._tool_blocked:
            blocked = self._tool_blocked[tool]
        elif letter is not None:
            blocked = self._volume_block(tool, letter, listed)

        notes: list[str] = []
        if tool == "disk_check" and listed is not None and listed["system"]:
            notes.append(
                f"{letter} is in use, so the check runs read-only on a live file system and can report "
                "problems that are not real."
            )
        if tool == "drive_optimize" and listed is not None and listed["media"] == "hdd":
            notes.append("Defragmenting a hard disk can take a long time and slows the PC while it runs.")
        if tool == "dism_restore":
            notes.append("Downloads repair files from Windows Update; needs an internet connection.")
        if not info["cancellable"]:
            notes.append("Runs to completion; Cairn can't stop it once it starts.")
        return {
            "tool": tool,
            "title": info["title"],
            "volume": letter,
            "program": f"{SYSTEM32}\\{info['program']}",
            "args": args,
            "command_line": " ".join([info["program"], *args]),
            "requires_admin": True,
            "cancellable": info["cancellable"],
            "requires_detach": info["requires_detach"],
            "changes_system": info["changes_system"],
            "duration_hint": info["duration_hint"],
            "blocked_reason": blocked,
            "notes": notes,
        }

    def _volume_block(self, tool: str, letter: str, listed: dict[str, Any] | None) -> str | None:
        if listed is None:
            return f"{letter} is not a fixed drive on this PC"
        if listed["error"]:
            return str(listed["error"])
        key = {"drive_optimize": "optimize_blocked", "drive_retrim": "retrim_blocked"}.get(
            tool, "check_blocked"
        )
        if listed[key]:
            return str(listed[key])
        busy = self._tool_storage_busy.get(letter)
        if tool in DRIVE_TOOLS and busy:
            return f"{busy} is running on {letter}; wait for it to finish or stop it."
        return None

    # -- module surface ------------------------------------------------------------------

    def tools_catalog(self) -> list[dict[str, Any]]:
        self._record("tools_catalog")
        return [catalog_entry(t[0]) for t in TOOLS]

    def tools_volumes(self) -> list[dict[str, Any]]:
        self._record("tools_volumes")
        return [dict(v) for v in VOLUMES]

    def tools_windows(self) -> list[dict[str, Any]]:
        self._record("tools_windows")
        return [dict(t) for t in WINDOWS_TOOLS]

    def system_restore_enabled(self) -> bool:
        self._record("system_restore_enabled")
        if self._restore_enabled is None:
            raise RuntimeError("cannot read the System Restore configuration: access denied")
        return bool(self._restore_enabled)

    def tools_start(self, tool: str, volume: str | None = None, dry_run: bool = False) -> dict[str, Any]:
        self._record("tools_start", tool, volume, dry_run)
        with self._tools_lock:
            plan = self._tool_plan(tool, volume)
            if dry_run:
                return {"plan": plan, "job": None}
            # Refusals, first match wins: elevation, the tool's block, closing, a launch
            # failure, then a job that is already running.
            if not self.elevated:
                raise RuntimeError(NOT_ELEVATED)
            running = self._running_tool_job()
            reason = self._tool_blocked.get(tool)
            if reason is None and plan["volume"] is not None:
                listed = next((v for v in VOLUMES if v["letter"] == plan["volume"]), None)
                reason = self._volume_block(tool, plan["volume"], listed)
            if reason:
                raise RuntimeError(reason)
            if self._tools_closed:
                raise RuntimeError(CLOSING)
            if self._tool_start_error:
                raise RuntimeError(self._tool_start_error)
            if running is not None:
                raise RuntimeError(f"{running.fields['title']} is already running in Cairn")
            job_id = self._tool_next_id
            self._tool_next_id += 1
            cancellable = bool(plan["cancellable"])
            job = _Job(
                {
                    "id": job_id,
                    "tool": tool,
                    "title": plan["title"],
                    "command_line": plan["command_line"],
                    "volume": plan["volume"],
                    "state": "running",
                    "started_at": STARTED_AT,
                    "finished_at": None,
                    "elapsed_ms": 0,
                    "idle_ms": 0,
                    "progress": None,
                    "progress_line": None,
                    "exit_code": None,
                    "exit_code_hex": None,
                    "cancellable": cancellable,
                    "cancel_requested": False,
                    "detached": not cancellable and self._tool_detached,
                    "restart_required": False,
                    "hint": None,
                    "summary": None,
                    "log_path": f"C:\\Test\\tools\\{job_id}.log",
                    "raw_log_path": f"C:\\Test\\tools\\{job_id}.raw",
                    "line_count": 0,
                    "logged": True,
                }
            )
            self._tool_jobs[job_id] = job
            self._prune_tool_jobs()
            return {"plan": plan, "job": job.snapshot()}

    def _prune_tool_jobs(self) -> None:
        finished = sorted((i for i, j in self._tool_jobs.items() if not j.running), reverse=True)
        for job_id in finished[KEPT_FINISHED_JOBS:]:
            del self._tool_jobs[job_id]

    def tools_job(self, job_id: int, after: int = 0) -> dict[str, Any] | None:
        self._record_quick("tools_job", job_id, after)
        with self._tools_lock:
            job = self._tool_jobs.get(job_id)
            if job is None:
                return None
            view = job.snapshot()
            available = [(seq, text) for seq, text in job.lines if seq > after]
            batch = available[:MAX_LINES_PER_VIEW]
            oldest = job.lines[0][0] if job.lines else job.seq + 1
            view.update(
                lines=[text for _, text in batch],
                first=batch[0][0] if batch else None,
                next=batch[-1][0] if batch else max(after, job.seq),
                skipped=max(0, oldest - after - 1),
                more=len(available) > len(batch),
            )
            return view

    def tools_jobs(self) -> list[dict[str, Any]]:
        self._record_quick("tools_jobs")
        with self._tools_lock:
            return [self._tool_jobs[i].snapshot() for i in sorted(self._tool_jobs, reverse=True)]

    def tools_cancel(self, job_id: int) -> bool:
        """Stops a stoppable job at once (the engine's watcher does it within a tick). A job
        whose process has already ended is not stopped: it finishes as `tool_finish` says."""
        self._record_quick("tools_cancel", job_id)
        with self._tools_lock:
            job = self._tool_job_or_error(job_id)
            if not job.fields["cancellable"]:
                raise RuntimeError(f"{job.fields['title']} can't be stopped")
            if not job.running:
                return False
            job.fields["cancel_requested"] = True
            if job.fields["exit_code"] is None:
                job.finish("cancelled", 1)
            return True

    def tools_open_log(self, job_id: int) -> None:
        self._record("tools_open_log", job_id)
        with self._tools_lock:
            self._tool_job_or_error(job_id)

    def tools_shutdown(self) -> list[dict[str, Any]]:
        """Like the engine: a job whose process has ended finishes while this waits and is
        not listed; a stoppable job is stopped; the others are left running."""
        self._record_quick("tools_shutdown")
        outcomes = []
        with self._tools_lock:
            self._tools_closed = True
            for job in self._tool_jobs.values():
                if not job.running:
                    continue
                if job.fields["exit_code"] is not None:
                    job.finish("completed", job.fields["exit_code"])
                    continue
                if job.fields["cancellable"]:
                    job.fields["cancel_requested"] = True
                    job.finish("cancelled", 1)
                    action = "stopped"
                elif job.fields["detached"]:
                    action = "left_running"
                else:
                    action = "may_end_with_app"
                outcomes.append({"id": job.fields["id"], "tool": job.fields["tool"], "action": action})
        return outcomes

    def tools_restore_point(self, description: str = MANUAL_RESTORE_POINT_DESCRIPTION) -> dict[str, Any]:
        self._record("tools_restore_point", description)
        if not self.elevated:
            raise RuntimeError(NOT_ELEVATED)
        if self._restore_enabled is False:
            raise RuntimeError(
                "System Protection is off for the system drive, so no restore point can be created"
            )
        if self._restore_point_error:
            raise RuntimeError(self._restore_point_error)
        return {"sequence": 7, "description": description, "created_at": STARTED_AT}

    def tools_open_windows(self, tool_id: str) -> None:
        self._record("tools_open_windows", tool_id)
        info = next((t for t in (*WINDOWS_TOOLS, *FIX_TOOLS) if t["id"] == tool_id), None)
        if info is None:
            raise ValueError(f"unknown Windows tool {tool_id!r}")
        if info["requires_admin"] and not self.elevated:
            raise RuntimeError(NOT_ELEVATED)

    # -- test drivers --------------------------------------------------------------------

    def tool_emit(
        self, job_id: int, *lines: str, progress: float | None = None, progress_line: str | None = None
    ) -> None:
        """Adds output lines to a running job and, when given, its progress and progress line."""
        with self._tools_lock:
            job = self._tool_job_or_error(job_id)
            for line in lines:
                job.seq += 1
                job.lines.append((job.seq, line))
            job.fields["line_count"] = job.seq
            if progress is not None:
                job.fields["progress"] = progress
            if progress_line is not None:
                job.fields["progress_line"] = progress_line
            job.last_output = time.monotonic()

    def tool_exit(self, job_id: int, exit_code: int = 0) -> None:
        """Ends a running job's process with `exit_code`; the job keeps running while its result
        is judged, as the engine's does while it reads the component store state."""
        with self._tools_lock:
            job = self._tool_job_or_error(job_id)
            job.fields.update(exit_code=exit_code, exit_code_hex=exit_code_hex(exit_code))

    def tool_finish(
        self,
        job_id: int,
        state: str = "succeeded",
        exit_code: int | None = 0,
        hint: str | None = None,
        restart_required: bool = False,
        summary: str | None = None,
    ) -> None:
        """Ends a job with `state` as the engine's classification would."""
        with self._tools_lock:
            job = self._tool_job_or_error(job_id)
            job.finish(state, exit_code, hint=hint, restart_required=restart_required, summary=summary)
            self._prune_tool_jobs()
