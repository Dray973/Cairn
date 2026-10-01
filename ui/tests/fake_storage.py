"""Storage part of FakeEngine: the `storage_*` functions of the speed test, the space scan and
the duplicate search.

Mixed into `FakeEngine`, which calls `_init_storage` from its constructor. No file is written
and no folder is read: volumes, the folder tree, speed results and duplicate groups are
fixtures, and a job exists only in memory. Drive a job with `storage_progress(job_id, **fields)`
and `storage_finish(job_id, state='succeeded', result=None, error=None)`; `storage_forget(job_id)`
drops a finished job's result, as the engine does once a newer job of its kind finished.
Job snapshots are built with `fake_jobs.host_snapshot`, so they have the host's keys.

Options (`FakeEngine(**options)`): `storage_volumes` (replaces `VOLUMES`), `storage_history`
(a list of results, or an Exception instance `storage_speed_history` raises),
`storage_blocked` ({"speed" | "scan" | "duplicates": reason} blocks those plans),
`storage_start_error` (every real start raises it) and `storage_leftovers` ({letter: [leftover
dicts]} adds test folders to those volumes).

Like the engine, the fake runs one storage job at a time, refuses a real speed test and a
leftover removal without elevation, and raises ValueError for bad arguments. Storage journals
nothing, so it adds nothing to the journal hooks.
"""

from __future__ import annotations

import copy
import math
import re
import threading
from typing import TYPE_CHECKING, Any

from .fake_jobs import HOST_SNAPSHOT_KEYS, host_snapshot

MIB = 1 << 20
GIB = 1 << 30
MIN_TEST_SIZE = 16 * MIB
MAX_TEST_SIZE = 64 * GIB
NOT_ELEVATED = "this operation requires an elevated (Administrator) process"
NOT_ELEVATED_PLAN = "Needs administrator rights; restart Cairn as administrator."
CLOSING = "Cairn is closing; nothing was started."
RESULTS_GONE = "The scan results are no longer available; scan again."
SCAN_RUNNING = "Wait for the scan to finish."
NOT_ELEVATED_NOTE = (
    "Cairn runs without administrator rights, so some folders can't be read. Restart as administrator "
    "to include them."
)
STARTED_AT = "2026-09-28T12:00:00Z"
FINISHED_AT = "2026-09-28T12:02:41Z"
FOLDER_NAME = re.compile(r"^CairnSpeedTest-[0-9a-fA-F]{8}$")

VOLUME_KEYS = (
    "letter",
    "label",
    "file_system",
    "size_bytes",
    "free_bytes",
    "media",
    "bus",
    "model",
    "system",
    "read_only",
    "persistent_acls",
    "not_responding",
    "error",
    "speed_test_blocked",
    "scan_blocked",
    "leftovers",
)
SPEED_PLAN_KEYS = (
    "volume",
    "size_bytes",
    "runs",
    "tests",
    "max_write_bytes",
    "estimated_seconds",
    "folder_pattern",
    "requires_admin",
    "media",
    "leftovers",
    "blocked_reason",
    "notes",
)
SCAN_PLAN_KEYS = (
    "root",
    "volume",
    "whole_volume",
    "file_system",
    "media",
    "threads",
    "elevated",
    "blocked_reason",
    "notes",
)
DUPES_PLAN_KEYS = (
    "scan_job",
    "root",
    "min_size",
    "candidates",
    "size_groups",
    "bytes_to_read_max",
    "blocked_reason",
    "notes",
)
TREE_ROW_KEYS = (
    "kind",
    "node",
    "name",
    "path",
    "logical",
    "allocated",
    "online_only",
    "files",
    "folders",
    "count",
    "has_children",
    "denied",
    "error",
    "modified",
)
SPEED_BLOCK_KEYS = ("test", "direction", "run", "runs", "step", "steps", "live_mb_s", "bytes_written", "done")
SCAN_BLOCK_KEYS = ("files", "folders", "logical_bytes", "allocated_bytes", "denied_folders", "current")
DUPES_BLOCK_KEYS = ("files_total", "files_done", "bytes_total", "bytes_done", "groups_found", "current")

VOLUMES: tuple[dict[str, Any], ...] = (
    {
        "letter": "C:",
        "label": "Windows",
        "file_system": "NTFS",
        "size_bytes": 952 * GIB,
        "free_bytes": 611 * GIB,
        "media": "ssd",
        "bus": "NVMe",
        "model": "Test NVMe SSD",
        "system": True,
        "read_only": False,
        "persistent_acls": True,
        "not_responding": False,
        "error": None,
        "speed_test_blocked": None,
        "scan_blocked": None,
        "leftovers": [],
    },
    {
        "letter": "D:",
        "label": "Data",
        "file_system": "NTFS",
        "size_bytes": 1863 * GIB,
        "free_bytes": 1210 * GIB,
        "media": "hdd",
        "bus": "SATA",
        "model": "Test HDD",
        "system": False,
        "read_only": False,
        "persistent_acls": True,
        "not_responding": False,
        "error": None,
        "speed_test_blocked": None,
        "scan_blocked": None,
        "leftovers": [],
    },
)

TESTS = (
    ("seq1m_q8t1", "SEQ1M Q8T1", 1 << 20, 8, False),
    ("seq1m_q1t1", "SEQ1M Q1T1", 1 << 20, 1, False),
    ("rnd4k_q32t1", "RND4K Q32T1", 4096, 32, True),
    ("rnd4k_q1t1", "RND4K Q1T1", 4096, 1, True),
)
# test -> ((read MB/s, IOPS, latency µs), (write MB/s, IOPS, latency µs))
SPEEDS = {
    "seq1m_q8t1": ((7012.3, 6687.0, 1195.0), (6345.1, 6051.0, 1321.0)),
    "seq1m_q1t1": ((4102.5, 3912.0, 255.0), (3520.8, 3357.0, 297.0)),
    "rnd4k_q32t1": ((3511.4, 857_275.0, 37.0), (2890.2, 705_615.0, 45.0)),
    "rnd4k_q1t1": ((92.1, 22_485.0, 44.0), (310.4, 75_781.0, 13.0)),
}


def measurement(test: str, direction: str) -> dict[str, Any]:
    """A measurement of the fixture result."""
    _, label, block, queue, _ = next(t for t in TESTS if t[0] == test)
    mb_s, iops, latency = SPEEDS[test][0 if direction == "read" else 1]
    return {
        "test": test,
        "label": label,
        "direction": direction,
        "block_bytes": block,
        "queue_depth": queue,
        "threads": 1,
        "mb_s": mb_s,
        "iops": iops,
        "latency_us": latency,
        "runs": 3,
        "duration_ms": 5000,
        "bytes": int(mb_s * 5_000_000),
    }


SPEED_RESULT: dict[str, Any] = {
    "volume": "C:",
    "label": "Windows",
    "model": "Test NVMe SSD",
    "bus": "NVMe",
    "media": "ssd",
    "file_system": "NTFS",
    "size_bytes": GIB,
    "runs": 3,
    "started_at": STARTED_AT,
    "finished_at": FINISHED_AT,
    "elapsed_ms": 161_000,
    "completed": True,
    "measurements": [measurement(t[0], d) for d in ("read", "write") for t in TESTS],
    "skipped": [],
    "bytes_written": 13 * GIB,
    "notes": ["Other programs use C: while the test runs, which can lower the results."],
    "app_version": "0.2.0",
    "history_saved": True,
}


def tree_row(
    kind: str,
    name: str,
    *,
    node: int | None = None,
    path: str | None = None,
    logical: int = 0,
    allocated: int = 0,
    online_only: int = 0,
    files: int = 0,
    folders: int = 0,
    count: int = 0,
    has_children: bool = False,
    denied: bool = False,
    error: str | None = None,
    modified: str | None = None,
) -> dict[str, Any]:
    """A folder-tree row with exactly the engine's keys."""
    return {
        "kind": kind,
        "node": node,
        "name": name,
        "path": path,
        "logical": logical,
        "allocated": allocated,
        "online_only": online_only,
        "files": files,
        "folders": folders,
        "count": count,
        "has_children": has_children,
        "denied": denied,
        "error": error,
        "modified": modified,
    }


# Folder rows by node, and each folder's entries.
TREE_NODES: dict[int, dict[str, Any]] = {
    0: tree_row(
        "folder",
        "C:\\",
        node=0,
        path="C:\\",
        logical=341 * GIB,
        allocated=338 * GIB,
        files=1_204_332,
        folders=214_009,
        has_children=True,
    ),
    1: tree_row(
        "folder",
        "Users",
        node=1,
        path="C:\\Users",
        logical=180 * GIB,
        allocated=176 * GIB,
        files=402_118,
        folders=61_230,
        has_children=True,
    ),
    2: tree_row(
        "folder",
        "Windows",
        node=2,
        path="C:\\Windows",
        logical=38 * GIB,
        allocated=32 * GIB,
        files=512_004,
        folders=120_870,
        has_children=True,
    ),
    3: tree_row(
        "folder",
        "Program Files",
        node=3,
        path="C:\\Program Files",
        logical=21 * GIB,
        allocated=20 * GIB,
        files=90_120,
        folders=14_532,
        has_children=True,
    ),
    4: tree_row("link", "Documents and Settings", node=4, path="C:\\Documents and Settings"),
    5: tree_row(
        "folder", "System Volume Information", node=5, path="C:\\System Volume Information", denied=True
    ),
    6: tree_row(
        "folder",
        "Test",
        node=6,
        path="C:\\Users\\Test",
        logical=180 * GIB,
        allocated=176 * GIB,
        files=402_117,
        folders=61_229,
        has_children=True,
    ),
    7: tree_row(
        "folder",
        "Videos",
        node=7,
        path="C:\\Users\\Test\\Videos",
        logical=5 * GIB,
        allocated=5 * GIB,
        files=4,
        folders=0,
        has_children=True,
    ),
}
TREE_CHILDREN: dict[int, list[dict[str, Any]]] = {
    0: [
        TREE_NODES[1],
        TREE_NODES[2],
        TREE_NODES[3],
        TREE_NODES[4],
        TREE_NODES[5],
        tree_row(
            "small_files", "12 smaller files", logical=900 * 1024, allocated=1 * MIB, files=12, count=12
        ),
    ],
    1: [TREE_NODES[6]],
    2: [
        tree_row(
            "small_files",
            "512,004 smaller files",
            logical=38 * GIB,
            allocated=32 * GIB,
            files=512_004,
            count=512_004,
        )
    ],
    3: [
        tree_row(
            "small_files",
            "90,120 smaller files",
            logical=21 * GIB,
            allocated=20 * GIB,
            files=90_120,
            count=90_120,
        )
    ],
    5: [],
    6: [TREE_NODES[7]],
    7: [
        tree_row(
            "file",
            "holiday.mp4",
            path="C:\\Users\\Test\\Videos\\holiday.mp4",
            logical=5 * GIB - 3 * MIB,
            allocated=5 * GIB - 3 * MIB,
            files=1,
            modified="2026-08-14T09:30:00Z",
        ),
        tree_row("small_files", "3 smaller files", logical=2 * MIB, allocated=3 * MIB, files=3, count=3),
    ],
}

SCAN_SUMMARY: dict[str, Any] = {
    "root": "C:\\",
    "volume": "C:",
    "whole_volume": True,
    "completed": True,
    "files": 1_204_332,
    "folders": 214_009,
    "logical_bytes": 341 * GIB,
    "allocated_bytes": 338 * GIB,
    "online_only_bytes": 0,
    "online_only_files": 0,
    "hard_links_counted_once": 38_402,
    "links_skipped": 1,
    "denied_folders": 1,
    "unreadable_folders": 0,
    "volume_size": 952 * GIB,
    "volume_used": 341 * GIB,
    "not_reached_bytes": 3 * GIB,
    "elapsed_ms": 48_000,
    "threads": 8,
    "id_limit_reached": False,
    "node_limit_reached": False,
    "big_file_limit_reached": False,
    "duplicate_candidates": 7,
}
LARGEST_FILES = [TREE_CHILDREN[7][0]]


def _dupe_file(path: str, size: int) -> dict[str, Any]:
    return {"path": path, "modified": "2026-08-14T09:30:00Z", "allocated": size}


DUPLICATE_GROUPS: list[dict[str, Any]] = [
    {
        "size": GIB,
        "count": 3,
        "wasted": 2 * GIB,
        "hash": "0123456789abcdef",
        "files": [
            _dupe_file("C:\\Users\\Test\\Backup\\holiday.mp4", GIB),
            _dupe_file("C:\\Users\\Test\\Desktop\\holiday (1).mp4", GIB),
            _dupe_file("C:\\Users\\Test\\Videos\\holiday copy.mp4", GIB),
        ],
        "more_files": 0,
    },
    {
        "size": 300 * MIB,
        "count": 2,
        "wasted": 300 * MIB,
        "hash": "fedcba9876543210",
        "files": [
            _dupe_file("C:\\Users\\Test\\Documents\\setup.iso", 300 * MIB),
            _dupe_file("C:\\Users\\Test\\Downloads\\setup.iso", 300 * MIB),
        ],
        "more_files": 0,
    },
    {
        "size": 12 * MIB,
        "count": 2,
        "wasted": 12 * MIB,
        "hash": "00112233aabbccdd",
        "files": [
            _dupe_file("C:\\Users\\Test\\Pictures\\beach.jpg", 12 * MIB),
            _dupe_file("C:\\Users\\Test\\Pictures\\Contoso\\beach.jpg", 12 * MIB),
        ],
        "more_files": 0,
    },
]
DUPES_RESULT: dict[str, Any] = {
    "scan_job": 0,
    "root": "C:\\",
    "min_size": MIB,
    "completed": True,
    "groups": DUPLICATE_GROUPS,
    "group_count": 3,
    "wasted_bytes": 2 * GIB + 312 * MIB,
    "files_compared": 7,
    "bytes_read": 3 * GIB + 624 * MIB,
    "skipped_in_use": 0,
    "skipped_unreadable": 0,
    "skipped_changed": 0,
    "skipped_online_only": 0,
}


def _letter(volume: str) -> str | None:
    text = str(volume).strip().rstrip("\\").rstrip(":")
    return f"{text.upper()}:" if len(text) == 1 and text.isalpha() else None


def _absolute(path: str) -> bool:
    return bool(re.match(r"^[A-Za-z]:\\", path)) or path.startswith("\\\\")


class FakeStorage:
    """Storage section with fixture volumes, results and jobs held in memory."""

    if TYPE_CHECKING:
        elevated: bool

        def _record(self, name: str, *args: Any) -> None: ...
        def _record_quick(self, name: str, *args: Any) -> None: ...

    def _init_storage(self, options: dict[str, Any]) -> None:
        """Pops the storage options this fake understands from `options`."""
        volumes = options.pop("storage_volumes", None)
        self.storage_volume_list: list[dict[str, Any]] = copy.deepcopy(
            list(volumes) if volumes is not None else list(VOLUMES)
        )
        for letter, leftovers in dict(options.pop("storage_leftovers", None) or {}).items():
            for volume in self.storage_volume_list:
                if volume["letter"] == _letter(letter):
                    volume["leftovers"] = [dict(item) for item in leftovers]
        history = options.pop("storage_history", None)
        self.storage_history: list[dict[str, Any]] | BaseException = (
            history
            if isinstance(history, BaseException)
            else copy.deepcopy(list(history) if history is not None else [SPEED_RESULT])
        )
        self.storage_blocked: dict[str, str] = dict(options.pop("storage_blocked", None) or {})
        self.storage_start_error: str | None = options.pop("storage_start_error", None)
        self._storage_lock = threading.Lock()
        self._storage_jobs: dict[int, dict[str, Any]] = {}
        self._storage_results: dict[int, dict[str, Any]] = {}
        self._storage_next_id = 1
        self._storage_closed = False
        # Duplicate search id -> (scan job id, scanned folder).
        self._storage_scans_of: dict[int, tuple[int, str]] = {}

    # -- jobs ----------------------------------------------------------------------------

    def _storage_running(self) -> dict[str, Any] | None:
        return next((j for j in self._storage_jobs.values() if j["state"] == "running"), None)

    def _storage_busy_reason(self) -> str | None:
        running = self._storage_running()
        return f"{running['title']} is running; wait for it to finish or stop it." if running else None

    def _storage_start(
        self, kind: str, title: str, command_line: str, detail: dict[str, Any], *, logged: bool
    ) -> dict[str, Any]:
        if self._storage_closed:
            raise RuntimeError(CLOSING)
        if self.storage_start_error:
            raise RuntimeError(self.storage_start_error)
        job_id = self._storage_next_id
        self._storage_next_id += 1
        job = host_snapshot(
            id=job_id,
            lane="storage",
            kind=kind,
            title=title,
            command_line=command_line,
            state="running",
            started_at=STARTED_AT,
            cancellable=True,
            detail=detail,
            logged=logged,
            progress=0.0 if kind != "space_scan" else None,
        )
        self._storage_jobs[job_id] = job
        return copy.deepcopy(job)

    def storage_progress(self, job_id: int, **fields: Any) -> None:
        """Updates a running job: snapshot keys (`progress`, `progress_line`, …), `phase`, and
        the `speed`, `scan` or `duplicates` block (merged into the one there)."""
        with self._storage_lock:
            job = self._storage_jobs[job_id]
            detail = dict(job.get("detail") or {})
            if "phase" in fields:
                detail["phase"] = fields.pop("phase")
            for block in ("speed", "scan", "duplicates"):
                if block in fields:
                    merged = dict(detail.get(block) or {})
                    merged.update(fields.pop(block))
                    detail[block] = merged
            job["detail"] = detail
            unknown = sorted(set(fields) - set(HOST_SNAPSHOT_KEYS))
            if unknown:
                raise TypeError(f"unknown storage job fields: {', '.join(unknown)}")
            job.update(fields)

    def _default_result(self, job: dict[str, Any], state: str) -> dict[str, Any]:
        kind = job["kind"]
        if kind == "speed_test":
            result = {"kind": "speed", **copy.deepcopy(SPEED_RESULT)}
            result["volume"] = job["title"].removeprefix("Speed test of ")
            if state != "succeeded":
                result["completed"] = False
                result["measurements"] = result["measurements"][:2]
            return result
        if kind == "space_scan":
            summary = copy.deepcopy(SCAN_SUMMARY)
            summary["completed"] = state == "succeeded"
            return {
                "kind": "scan",
                "job_id": job["id"],
                "summary": summary,
                "root": copy.deepcopy(TREE_NODES[0]),
                "largest_files": copy.deepcopy(LARGEST_FILES),
                "largest_files_by_size": copy.deepcopy(LARGEST_FILES),
                "warnings": [],
            }
        result = {"kind": "duplicates", "job_id": job["id"], **copy.deepcopy(DUPES_RESULT)}
        scan_job, root = self._storage_scans_of.get(job["id"], (0, DUPES_RESULT["root"]))
        result["scan_job"] = scan_job
        result["root"] = root
        result["completed"] = state == "succeeded"
        return result

    def storage_finish(
        self,
        job_id: int,
        state: str = "succeeded",
        result: dict[str, Any] | None = None,
        error: str | None = None,
    ) -> None:
        """Ends a running job. Without `result`, a finished job publishes the fixture of its
        kind (a failed one publishes nothing). The newest finished job of each kind keeps its
        result; older ones of that kind lose theirs."""
        with self._storage_lock:
            self._storage_finish(job_id, state, result, error)

    def _storage_finish(
        self, job_id: int, state: str, result: dict[str, Any] | None, error: str | None
    ) -> None:
        job = self._storage_jobs[job_id]
        job["state"] = state
        job["finished_at"] = FINISHED_AT
        job["elapsed_ms"] = {"speed_test": 161_000, "space_scan": 48_000}.get(job["kind"], 12_000)
        detail = dict(job.get("detail") or {})
        detail["phase"] = "done"
        job["detail"] = detail
        if state == "failed":
            job["summary"] = error or "the job failed"
        elif state == "cancelled":
            job["summary"] = "Stopped."
        else:
            job["summary"] = "Finished."
        if result is None and state != "failed":
            result = self._default_result(job, state)
        if result is not None:
            self._storage_results[job_id] = copy.deepcopy(result)
            job["has_result"] = True
            job["result_revision"] = 1
            for other in self._storage_jobs.values():
                if other["id"] != job_id and other["kind"] == job["kind"] and other["state"] != "running":
                    self._storage_results.pop(other["id"], None)
                    other["has_result"] = False

    def storage_forget(self, job_id: int) -> None:
        """Drops a finished job's result, as the engine does once a newer job of its kind ends."""
        with self._storage_lock:
            self._storage_results.pop(job_id, None)
            if job_id in self._storage_jobs:
                self._storage_jobs[job_id]["has_result"] = False

    # -- module surface ------------------------------------------------------------------

    def storage_volumes(self) -> list[dict[str, Any]]:
        self._record("storage_volumes")
        return copy.deepcopy(self.storage_volume_list)

    def storage_speed_start(
        self, volume: str, size_bytes: int = 1073741824, runs: int = 3, dry_run: bool = False
    ) -> dict[str, Any]:
        self._record("storage_speed_start", volume, size_bytes, runs, dry_run)
        letter = _letter(volume)
        if letter is None:
            raise ValueError(f"not a drive letter: {volume!r}")
        if (
            not isinstance(size_bytes, int)
            or size_bytes % MIB
            or not MIN_TEST_SIZE <= size_bytes <= MAX_TEST_SIZE
        ):
            raise ValueError(
                f"size_bytes must be a whole number of MiB from 16 MiB to 64 GiB, got {size_bytes}"
            )
        if not isinstance(runs, int) or not 1 <= runs <= 9:
            raise ValueError(f"runs must be 1 to 9, got {runs}")
        with self._storage_lock:
            found = next((v for v in self.storage_volume_list if v["letter"] == letter), None)
            media = found["media"] if found else "unknown"
            rate = 100e6 if media == "hdd" else 400e6
            notes = []
            if found and found.get("system"):
                notes.append(f"Other programs use {letter} while the test runs, which can lower the results.")
            if media == "hdd":
                notes.append(
                    "On a hard disk a small test file shows better random speeds than the whole disk would."
                )
            if found is None:
                blocked: str | None = f"{letter} is not a fixed drive on this PC"
            elif found.get("speed_test_blocked"):
                blocked = str(found["speed_test_blocked"])
            elif not self.elevated:
                blocked = NOT_ELEVATED_PLAN
            else:
                blocked = self.storage_blocked.get("speed") or self._storage_busy_reason()
            plan = {
                "volume": letter,
                "size_bytes": size_bytes,
                "runs": runs,
                "tests": [
                    {"id": t, "label": label, "block_bytes": block, "queue_depth": queue, "random": random}
                    for t, label, block, queue, random in TESTS
                ],
                "max_write_bytes": size_bytes * (1 + 4 * runs),
                "estimated_seconds": math.ceil(8 * runs * 6 + size_bytes / rate),
                "folder_pattern": f"{letter}\\CairnSpeedTest-…",
                "requires_admin": True,
                "media": media,
                "leftovers": copy.deepcopy(found["leftovers"]) if found else [],
                "blocked_reason": blocked,
                "notes": notes,
            }
            if dry_run:
                return {"plan": plan, "job": None}
            if not self.elevated:
                raise RuntimeError(NOT_ELEVATED)
            if blocked:
                raise RuntimeError(blocked)
            detail = {
                "phase": "preparing",
                "speed": {
                    "test": None,
                    "direction": None,
                    "run": 0,
                    "runs": runs,
                    "step": 1,
                    "steps": 1 + 8 * runs,
                    "live_mb_s": None,
                    "bytes_written": 0,
                    "done": [],
                },
                "scan": None,
                "duplicates": None,
            }
            job = self._storage_start(
                "speed_test",
                f"Speed test of {letter}",
                f"optctl storage speed {letter} --size {size_bytes // MIB}M --runs {runs}",
                detail,
                logged=True,
            )
            return {"plan": plan, "job": job}

    def storage_speed_history(self) -> list[dict[str, Any]]:
        self._record("storage_speed_history")
        if isinstance(self.storage_history, BaseException):
            raise self.storage_history
        return copy.deepcopy(self.storage_history)

    def storage_remove_leftover(self, path: str) -> dict[str, Any]:
        self._record("storage_remove_leftover", path)
        name = str(path).strip().rstrip("\\").rsplit("\\", 1)[-1]
        if not _absolute(str(path)) or not FOLDER_NAME.match(name):
            raise ValueError(f"{path} is not a speed-test folder")
        if not self.elevated:
            raise RuntimeError(NOT_ELEVATED)
        with self._storage_lock:
            for volume in self.storage_volume_list:
                for leftover in list(volume["leftovers"]):
                    if leftover.get("path") == path:
                        volume["leftovers"].remove(leftover)
                        size = int(leftover.get("bytes") or 0)
                        return {
                            "path": path,
                            "removed": True,
                            "bytes": size,
                            "detail": f"removed the {size // MIB} MB test file and its folder",
                        }
        return {"path": path, "removed": False, "bytes": 0, "detail": "the folder is gone"}

    def storage_scan_start(self, path: str, dry_run: bool = False) -> dict[str, Any]:
        self._record("storage_scan_start", path, dry_run)
        root = str(path).strip()
        if not root or not _absolute(root):
            raise ValueError(f"the folder to scan must be an absolute path, got {path!r}")
        with self._storage_lock:
            whole = len(root.rstrip("\\")) == 2
            blocked = self.storage_blocked.get("scan") or self._storage_busy_reason()
            plan = {
                "root": root,
                "volume": root[:2].upper(),
                "whole_volume": whole,
                "file_system": "NTFS",
                "media": "ssd",
                "threads": 8,
                "elevated": self.elevated,
                "blocked_reason": blocked,
                "notes": [] if self.elevated else [NOT_ELEVATED_NOTE],
            }
            if dry_run:
                return {"plan": plan, "job": None}
            if blocked:
                raise RuntimeError(blocked)
            detail = {
                "phase": "scanning",
                "speed": None,
                "scan": {
                    "files": 0,
                    "folders": 0,
                    "logical_bytes": 0,
                    "allocated_bytes": 0,
                    "denied_folders": 0,
                    "current": root,
                },
                "duplicates": None,
            }
            title = f"Scan of {root[:2].upper()}" if whole else f"Scan of {root}"
            job = self._storage_start(
                "space_scan", title, f'optctl storage scan "{root}"', detail, logged=False
            )
            return {"plan": plan, "job": job}

    def storage_duplicates_start(
        self, scan_job: int, min_size: int = 1048576, dry_run: bool = False
    ) -> dict[str, Any]:
        self._record("storage_duplicates_start", scan_job, min_size, dry_run)
        if not isinstance(min_size, int) or min_size < MIB:
            raise ValueError(f"min_size must be at least 1 MiB, got {min_size}")
        with self._storage_lock:
            scan = self._storage_jobs.get(scan_job)
            result = self._storage_results.get(scan_job)
            if scan is not None and scan["kind"] == "space_scan" and scan["state"] == "running":
                blocked: str | None = SCAN_RUNNING
            elif scan is None or scan["kind"] != "space_scan" or result is None:
                blocked = RESULTS_GONE
            else:
                blocked = self.storage_blocked.get("duplicates") or self._storage_busy_reason()
            root = str(((result or {}).get("summary") or {}).get("root") or "")
            plan = {
                "scan_job": scan_job,
                "root": root,
                "min_size": min_size,
                "candidates": 7 if result else 0,
                "size_groups": 3 if result else 0,
                "bytes_to_read_max": (3 * GIB + 624 * MIB) if result else 0,
                "blocked_reason": blocked,
                "notes": [],
            }
            if dry_run:
                return {"plan": plan, "job": None}
            if blocked:
                raise RuntimeError(blocked)
            detail = {
                "phase": "sampling",
                "speed": None,
                "scan": None,
                "duplicates": {
                    "files_total": 7,
                    "files_done": 0,
                    "bytes_total": 7 * 128 * 1024,
                    "bytes_done": 0,
                    "groups_found": 0,
                    "current": "",
                },
            }
            job = self._storage_start(
                "duplicates",
                f"Duplicate search in {root}",
                f'optctl storage duplicates "{root}" --min-size {min_size // MIB}M',
                detail,
                logged=False,
            )
            self._storage_scans_of[job["id"]] = (scan_job, root)
            return {"plan": plan, "job": job}

    def storage_job(self, job_id: int) -> dict[str, Any] | None:
        self._record_quick("storage_job", job_id)
        with self._storage_lock:
            job = self._storage_jobs.get(job_id)
            return copy.deepcopy(job) if job is not None else None

    def storage_jobs(self) -> list[dict[str, Any]]:
        self._record_quick("storage_jobs")
        with self._storage_lock:
            return [copy.deepcopy(j) for j in sorted(self._storage_jobs.values(), key=lambda j: -j["id"])]

    def storage_cancel(self, job_id: int) -> bool:
        self._record_quick("storage_cancel", job_id)
        with self._storage_lock:
            job = self._storage_jobs.get(job_id)
            if job is None:
                raise RuntimeError(f"no storage job with id {job_id}")
            if job["state"] != "running":
                return False
            job["cancel_requested"] = True
            self._storage_finish(job_id, "cancelled", None, None)
            return True

    def storage_result(self, job_id: int) -> dict[str, Any] | None:
        self._record_quick("storage_result", job_id)
        with self._storage_lock:
            job = self._storage_jobs.get(job_id)
            result = self._storage_results.get(job_id)
            if job is None or job["state"] == "running" or result is None:
                return None
            return {**copy.deepcopy(result), "revision": job["result_revision"]}

    def storage_scan_children(
        self, job_id: int, node: int, order: str = "allocated", limit: int = 500
    ) -> dict[str, Any] | None:
        self._record_quick("storage_scan_children", job_id, node, order, limit)
        if order not in ("allocated", "logical"):
            raise ValueError(f"order must be 'allocated' or 'logical', got {order!r}")
        if not isinstance(limit, int) or not 1 <= limit <= 5000:
            raise ValueError(f"limit must be 1 to 5000, got {limit}")
        with self._storage_lock:
            job = self._storage_jobs.get(job_id)
            if job is None or job["kind"] != "space_scan" or job_id not in self._storage_results:
                return None
            if node not in TREE_NODES:
                return None
            rows = copy.deepcopy(TREE_CHILDREN.get(node, []))
        rows.sort(key=lambda r: (-int(r[order]), r["name"]))
        total = len(rows)
        if total > limit:
            rest = rows[limit - 1 :]
            rows = rows[: limit - 1]
            rows.append(
                tree_row(
                    "more",
                    f"{len(rest):,} more items",
                    logical=sum(r["logical"] for r in rest),
                    allocated=sum(r["allocated"] for r in rest),
                    files=sum(r["files"] for r in rest),
                    count=len(rest),
                )
            )
        return {"node": copy.deepcopy(TREE_NODES[node]), "order": order, "children": rows, "total": total}

    def storage_shutdown(self) -> list[dict[str, Any]]:
        self._record_quick("storage_shutdown")
        with self._storage_lock:
            self._storage_closed = True
            running = self._storage_running()
            if running is None:
                return []
            running["cancel_requested"] = True
            self._storage_finish(running["id"], "cancelled", None, None)
            return [{"id": running["id"], "kind": running["kind"], "action": "stopped"}]
