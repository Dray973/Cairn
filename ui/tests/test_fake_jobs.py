"""The fakes' model of the engine's job host JSON: key sets and output paging, mirrored from
the Rust host's tests, and the key set of a real snapshot when the deployed engine has one."""

from __future__ import annotations

import time
from pathlib import Path

import pytest

from optimizer.bridge.engine import EngineUnavailable, load_engine_module

from .fake_jobs import (
    HOST_SNAPSHOT_KEYS,
    HOST_VIEW_KEYS,
    MAX_JOB_LINES,
    host_snapshot,
    host_view,
    page_lines,
)

SNAPSHOT_KEYS = [
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
]


def test_snapshot_and_view_keys_match_the_contract() -> None:
    assert len(HOST_SNAPSHOT_KEYS) == 25
    assert sorted(HOST_SNAPSHOT_KEYS) == sorted(SNAPSHOT_KEYS)
    assert len(HOST_VIEW_KEYS) == 30
    assert sorted(HOST_VIEW_KEYS) == sorted([*SNAPSHOT_KEYS, "lines", "first", "next", "skipped", "more"])
    snapshot = host_snapshot(id=3, lane="storage", kind="space_scan", title="Scan C:\\")
    assert list(snapshot) == list(HOST_SNAPSHOT_KEYS)
    view = host_view(snapshot, ["a", "b"], 0)
    assert list(view) == list(HOST_VIEW_KEYS)
    assert view["id"] == 3 and view["lane"] == "storage" and view["line_count"] == 2


def test_snapshot_defaults_follow_the_rust_types() -> None:
    snapshot = host_snapshot()
    assert snapshot["state"] == "running"
    assert snapshot["finished_at"] is None and snapshot["progress"] is None and snapshot["detail"] is None
    assert snapshot["notes"] == [] and snapshot["line_count"] == 0 and snapshot["result_revision"] == 0
    assert snapshot["cancellable"] is False and snapshot["has_result"] is False
    notes = ["one"]
    copy = host_snapshot(notes=notes)
    copy["notes"].append("two")
    assert notes == ["one"], "a snapshot keeps its own list"


def test_unknown_fields_are_refused() -> None:
    with pytest.raises(TypeError, match="exit_code"):
        host_snapshot(exit_code=0)
    with pytest.raises(TypeError, match="lines"):
        host_view({**host_snapshot(), "lines": []}, [], 0)


def test_line_pages_number_from_one_and_skip_evicted_lines() -> None:
    assert page_lines([], 0, 10) == ([], None, 0, 0, False)
    lines = [str(i) for i in range(1, 6)]
    assert page_lines(lines, 0, 2) == (["1", "2"], 1, 2, 0, True)
    assert page_lines(lines, 2, 10) == (["3", "4", "5"], 3, 5, 0, False)
    assert page_lines(lines, 5, 10) == ([], None, 5, 0, False)
    assert page_lines(lines, 99, 10) == ([], None, 5, 0, False)
    lines += [str(i) for i in range(6, MAX_JOB_LINES + 11)]
    page, first, following, skipped, more = page_lines(lines, 3, 1)
    assert (first, following, skipped, more) == (11, 11, 7, True)
    assert page == ["11"]


def test_view_pages_at_most_500_lines() -> None:
    lines = [str(i) for i in range(1, 1201)]
    view = host_view(host_snapshot(id=1), lines, 0, max_lines=10_000)
    assert len(view["lines"]) == 500 and view["first"] == 1 and view["next"] == 500 and view["more"]
    view = host_view(host_snapshot(id=1), lines, view["next"], max_lines=0)
    assert view["lines"] == ["501"] and view["more"], "a page holds at least one line"
    view = host_view(host_snapshot(id=1), lines, 1200)
    assert view["lines"] == [] and view["first"] is None and view["next"] == 1200 and not view["more"]


def test_real_snapshot_keys_match_the_fakes(tmp_path: Path) -> None:
    try:
        module = load_engine_module()
    except EngineUnavailable as exc:
        pytest.skip(str(exc))
    if not callable(getattr(module, "storage_scan_start", None)) or not callable(
        getattr(module, "storage_jobs", None)
    ):
        pytest.skip("the deployed engine module has no storage jobs yet")
    # A scan of an empty pytest folder only reads that folder.
    started = module.storage_scan_start(str(tmp_path))
    job = started["job"]
    # The contract is the key set: the engine's dicts come from JSON maps, which sort their keys.
    assert sorted(job) == sorted(HOST_SNAPSHOT_KEYS)
    deadline = time.monotonic() + 10
    while module.storage_job(job["id"])["state"] == "running":
        assert time.monotonic() < deadline, "the scan of an empty folder did not finish"
        time.sleep(0.05)
    for snapshot in module.storage_jobs():
        assert sorted(snapshot) == sorted(HOST_SNAPSHOT_KEYS)
