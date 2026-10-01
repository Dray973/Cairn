"""The JSON shapes of the engine's background job host (`optimizer_core::jobs`), for fakes.

Every fake lane (storage, updates) builds its job snapshots and views with `host_snapshot`
and `host_view`, so the fakes cannot drift apart or from the Rust host: a snapshot has
exactly `HOST_SNAPSHOT_KEYS` and a view exactly `HOST_VIEW_KEYS`, and a view pages output
lines like the host does.
"""

from __future__ import annotations

from collections.abc import Sequence
from typing import Any

# The host's snapshot keys, in the order the engine serializes them.
HOST_SNAPSHOT_KEYS = (
    "id",
    "lane",
    "kind",
    "title",
    "command_line",
    "state",
    "started_at",
    "finished_at",
    "elapsed_ms",
    "idle_ms",
    "progress",
    "progress_line",
    "cancellable",
    "cancel_requested",
    "detached",
    "restart_required",
    "summary",
    "hint",
    "notes",
    "detail",
    "log_path",
    "line_count",
    "logged",
    "has_result",
    "result_revision",
)
# A view is a snapshot plus one page of output lines.
HOST_VIEW_KEYS = (*HOST_SNAPSHOT_KEYS, "lines", "first", "next", "skipped", "more")

# Output lines the host keeps per job; older lines are counted as skipped.
MAX_JOB_LINES = 2000
# Most lines one view returns.
MAX_LINES_PER_VIEW = 500

# What each key holds when a field is not given, as the Rust types serialize their defaults.
_DEFAULTS: dict[str, Any] = {
    "id": 0,
    "lane": "",
    "kind": "",
    "title": "",
    "command_line": "",
    "state": "running",
    "started_at": "",
    "finished_at": None,
    "elapsed_ms": 0,
    "idle_ms": 0,
    "progress": None,
    "progress_line": None,
    "cancellable": False,
    "cancel_requested": False,
    "detached": False,
    "restart_required": False,
    "summary": None,
    "hint": None,
    "notes": [],
    "detail": None,
    "log_path": None,
    "line_count": 0,
    "logged": False,
    "has_result": False,
    "result_revision": 0,
}


def host_snapshot(**fields: Any) -> dict[str, Any]:
    """A job snapshot with every key of `HOST_SNAPSHOT_KEYS`; keys not given get their
    defaults. An unknown field is a TypeError."""
    unknown = sorted(set(fields) - set(HOST_SNAPSHOT_KEYS))
    if unknown:
        raise TypeError(f"unknown job snapshot fields: {', '.join(unknown)}")
    snapshot: dict[str, Any] = {}
    for key in HOST_SNAPSHOT_KEYS:
        value = fields.get(key, _DEFAULTS[key])
        snapshot[key] = list(value) if isinstance(value, list) else value
    return snapshot


def page_lines(
    lines: Sequence[str], after: int, max_lines: int = MAX_LINES_PER_VIEW
) -> tuple[list[str], int | None, int, int, bool]:
    """(lines, first, next, skipped, more) of the output lines numbered after `after`.

    Lines are numbered from 1 over everything the job printed; only the newest
    `MAX_JOB_LINES` are kept, and lines evicted before `after` + 1 count as skipped. At most
    `max_lines` (clamped to 1..MAX_LINES_PER_VIEW) are returned; `more` says others wait.
    """
    total = len(lines)
    kept = list(lines[-MAX_JOB_LINES:]) if total else []
    after = max(0, min(after, total))
    oldest = total + 1 - len(kept)
    start = max(after + 1, oldest)
    skipped = start - (after + 1)
    available = total + 1 - start
    count = min(available, max(1, min(max_lines, MAX_LINES_PER_VIEW)))
    offset = start - oldest
    page = kept[offset : offset + count]
    first = start if count > 0 else None
    following = start + count - 1 if count > 0 else after
    return page, first, following, skipped, available > count


def host_view(
    snapshot: dict[str, Any], lines: Sequence[str], after: int, max_lines: int = MAX_LINES_PER_VIEW
) -> dict[str, Any]:
    """The view of the job `snapshot` describes, with the page of `lines` (everything the job
    printed, oldest first) after line `after`. `line_count` becomes the number of lines."""
    view = host_snapshot(**snapshot)
    view["line_count"] = len(lines)
    page, first, following, skipped, more = page_lines(lines, after, max_lines)
    view.update({"lines": page, "first": first, "next": following, "skipped": skipped, "more": more})
    return view
