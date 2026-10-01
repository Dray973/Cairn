"""Thread-safe wrapper over the Rust `optimizer_engine` extension module.

Every engine call runs on one worker thread, so scans and mutations never overlap and
the Tk thread never blocks on PowerShell, the Service Control Manager or System Restore.
Results come back as `concurrent.futures.Future` objects; the UI polls them from its own
thread with `EngineBridge.dispatch_completed`.
"""

from __future__ import annotations

import json
import threading
from collections.abc import Callable, Sequence
from concurrent.futures import Future, ThreadPoolExecutor
from types import ModuleType
from typing import Any

CATEGORY_MODES = {
    "privacy": "Privacy Mode",
    "gaming": "Gaming Mode",
    "performance": "Max Performance",
    "interface": "Clean Interface",
}

Callback = Callable[[Future[Any]], None]


class EngineUnavailable(RuntimeError):
    """The native engine module could not be imported."""


def load_engine_module() -> ModuleType:
    try:
        import optimizer_engine  # type: ignore[import-not-found]
    except ImportError as exc:
        raise EngineUnavailable(f"optimizer_engine is not available: {exc}") from exc
    return optimizer_engine


def _copy(values: Sequence[str] | None) -> list[str] | None:
    return None if values is None else list(values)


def _copy_items(items: Sequence[dict[str, Any]] | None) -> list[dict[str, Any]] | None:
    return None if items is None else [dict(item) for item in items]


class EngineBridge:
    """Queues engine calls on a single worker thread and hands results back to the UI.

    Restore points: the first `apply` or `apply_category` of a bridge's lifetime that opens
    a journal session asks for one ("try"); later applies skip it, because each checkpoint
    takes several seconds and Windows keeps a limited number. The policy is decided on the
    worker when the call runs, so an apply that fails before its session starts leaves the
    request to the next one. Startup toggles always pass "skip"; reverts and cleanup never
    create a restore point. Applied tweaks and startup toggles are journaled and
    individually revertible; cleanup deletions are only logged and cannot be undone.

    System information is read-only and never journaled. DNS changes are journaled like
    other changes. Maintenance tools run as engine-owned background jobs and never occupy
    the worker; `tool_job`, `tool_jobs` and `cancel_tool` are synchronous, non-blocking calls
    for the UI thread, and `tools_shutdown` waits at most a few seconds.

    The jobs of the winget and storage lanes work the same way: their starts are queued on
    the worker, and they are then polled synchronously (`updates_job`, `storage_job` and the
    like read in-memory state and never wait). The Windows Update search of the security
    checkup runs on its own engine thread (`start_update_scan`, `update_scan`). Windows Update
    settings are journaled registry values like startup toggles. App permissions are a read-only
    guide: the engine refuses every permission change and plan (`set_permission`,
    `plan_permission`), and the records of permission changes made by earlier builds undo like
    other registry records. Turning scheduled maintenance off is an undo of its journal record.
    Profiles: previews, file reads and exports are worker reads that change nothing; applying
    goes through `_mutation`; undoing uses `revert_targets` with the report's `undo` filter.

    Dry runs and restore-point policies are passed to the engine by keyword (`dry_run=`,
    `restore_point=`).
    """

    def __init__(self, module: ModuleType | None = None, *, restore_points: bool = True) -> None:
        self._engine = module if module is not None else load_engine_module()
        self._executor = ThreadPoolExecutor(max_workers=1, thread_name_prefix="engine")
        self._pending: list[tuple[Future[Any], Callback]] = []
        self._lock = threading.Lock()
        self._busy = 0
        self.restore_points = restore_points
        self._restore_point_taken = False
        self._dns_presets: list[dict[str, Any]] | None = None
        self._wu_catalog: list[dict[str, Any]] | None = None
        self._profile_starters: list[dict[str, Any]] | None = None

    # -- lifecycle -----------------------------------------------------------------

    def shutdown(self, wait: bool = False) -> None:
        """Stops the worker; queued calls are cancelled. With `wait`, returns once the call
        that is running has finished."""
        self._executor.shutdown(wait=wait, cancel_futures=True)

    @property
    def busy(self) -> bool:
        with self._lock:
            return self._busy > 0

    @property
    def next_mutation_creates_restore_point(self) -> bool:
        """Whether the next `apply` or `apply_category` asks for a restore point."""
        with self._lock:
            return self.restore_points and not self._restore_point_taken

    @property
    def version(self) -> str:
        return str(self._engine.version())

    def is_elevated(self) -> bool:
        return bool(self._engine.is_elevated())

    # -- capabilities ------------------------------------------------------------------

    def supports(self, name: str) -> bool:
        """Whether the loaded engine module provides the callable `name` (False for an outdated build)."""
        return callable(getattr(self._engine, name, None))

    # -- queueing ------------------------------------------------------------------

    def _submit(
        self, fn: Callable[..., Any], *args: Any, callback: Callback | None = None, **kw: Any
    ) -> Future[Any]:
        def run() -> Any:
            try:
                return fn(*args, **kw)
            finally:
                with self._lock:
                    self._busy -= 1

        with self._lock:
            self._busy += 1
        future = self._executor.submit(run)
        if callback is not None:
            with self._lock:
                self._pending.append((future, callback))
        return future

    def dispatch_completed(self) -> int:
        """Runs the callbacks of finished calls on the calling (UI) thread.

        Every finished call's callback runs, even when an earlier one raises; the first
        exception is raised again once all of them have run. Returns the number of
        callbacks run.
        """
        # Each future's state is read once: a second read could see a call that finished in
        # between and drop it from both lists.
        done: list[tuple[Future[Any], Callback]] = []
        pending: list[tuple[Future[Any], Callback]] = []
        with self._lock:
            for entry in self._pending:
                (done if entry[0].done() else pending).append(entry)
            self._pending = pending
        first: Exception | None = None
        for future, callback in done:
            try:
                callback(future)
            except Exception as exc:  # noqa: BLE001 - one failing callback must not drop the rest of the batch
                if first is None:
                    first = exc
        if first is not None:
            raise first
        return len(done)

    def _mutation(self, fn: Callable[..., Any], *args: Any, callback: Callback | None = None) -> Future[Any]:
        """Queues an apply whose restore-point policy is chosen when it runs on the worker.

        The restore point counts as used only once the engine reports a journal session
        (`session_id`), which is when it attempts the checkpoint. A call that raises or
        changes nothing leaves the request to the next apply.
        """

        def run() -> Any:
            with self._lock:
                policy = "try" if self.restore_points and not self._restore_point_taken else "skip"
            report = fn(*args, restore_point=policy, dry_run=False)
            if policy == "try" and isinstance(report, dict) and report.get("session_id") is not None:
                with self._lock:
                    self._restore_point_taken = True
            return report

        return self._submit(run, callback=callback)

    # -- engine calls --------------------------------------------------------------

    def scan(self, callback: Callback | None = None) -> Future[Any]:
        return self._submit(self._engine.scan, callback=callback)

    def journal_summary(self, callback: Callback | None = None) -> Future[Any]:
        return self._submit(self._engine.journal_summary, callback=callback)

    def plan_category(self, category: str, callback: Callback | None = None) -> Future[Any]:
        return self._submit(
            self._engine.apply_category, category, restore_point="skip", dry_run=True, callback=callback
        )

    def plan_revert_category(self, category: str, callback: Callback | None = None) -> Future[Any]:
        return self._submit(self._engine.revert_category, category, dry_run=True, callback=callback)

    def plan_revert_all(self, callback: Callback | None = None) -> Future[Any]:
        return self._submit(self._engine.revert_all, dry_run=True, callback=callback)

    def plan_revert(self, ids: list[str], callback: Callback | None = None) -> Future[Any]:
        """Dry run of `revert`: the report's `actions` list the journal records it would restore."""
        return self._submit(self._engine.revert, list(ids), dry_run=True, callback=callback)

    def apply_category(self, category: str, callback: Callback | None = None) -> Future[Any]:
        return self._mutation(self._engine.apply_category, category, callback=callback)

    def apply(self, ids: list[str], callback: Callback | None = None) -> Future[Any]:
        return self._mutation(self._engine.apply, list(ids), callback=callback)

    def revert_category(self, category: str, callback: Callback | None = None) -> Future[Any]:
        return self._submit(self._engine.revert_category, category, dry_run=False, callback=callback)

    def revert(self, ids: list[str], callback: Callback | None = None) -> Future[Any]:
        return self._submit(self._engine.revert, list(ids), dry_run=False, callback=callback)

    def revert_all(self, callback: Callback | None = None) -> Future[Any]:
        return self._submit(self._engine.revert_all, dry_run=False, callback=callback)

    # -- catalog, history --------------------------------------------------------------

    def catalog(self, callback: Callback | None = None) -> Future[Any]:
        return self._submit(self._engine.catalog, callback=callback)

    def journal_export(self, callback: Callback | None = None) -> Future[Any]:
        return self._submit(lambda: json.loads(self._engine.journal_export_json()), callback=callback)

    def revert_targets(
        self, target_filter: dict[str, Any], dry_run: bool = False, callback: Callback | None = None
    ) -> Future[Any]:
        return self._submit(
            self._engine.revert_targets, dict(target_filter), dry_run=dry_run, callback=callback
        )

    # -- cleanup -------------------------------------------------------------------

    def cleanup_scan(self, callback: Callback | None = None) -> Future[Any]:
        return self._submit(self._engine.cleanup_scan, callback=callback)

    def cleanup_run(self, ids: list[str], callback: Callback | None = None) -> Future[Any]:
        return self._submit(self._engine.cleanup_run, list(ids), callback=callback)

    # -- startup and shell ---------------------------------------------------------

    def startup_list(self, callback: Callback | None = None) -> Future[Any]:
        return self._submit(self._engine.startup_list, callback=callback)

    def startup_set_enabled(
        self, entry_id: str, enabled: bool, callback: Callback | None = None
    ) -> Future[Any]:
        # Startup toggles are single journaled registry writes; a restore point per toggle
        # would cost seconds each for no extra safety.
        return self._submit(
            self._engine.startup_set_enabled, entry_id, enabled, restore_point="skip", callback=callback
        )

    def restart_explorer(self, callback: Callback | None = None) -> Future[Any]:
        return self._submit(self._engine.restart_explorer, callback=callback)

    # -- system information ------------------------------------------------------------

    def sysinfo_snapshot(self, callback: Callback | None = None) -> Future[Any]:
        return self._submit(self._engine.sysinfo_snapshot, callback=callback)

    # -- network -----------------------------------------------------------------------

    def network_list(self, callback: Callback | None = None) -> Future[Any]:
        return self._submit(self._engine.network_list, callback=callback)

    def dns_presets(self) -> list[dict[str, Any]]:
        """DNS preset table; pure data read on the calling thread and cached."""
        if self._dns_presets is None:
            self._dns_presets = list(self._engine.network_dns_presets())
        return self._dns_presets

    def plan_set_dns(
        self,
        adapter_id: str,
        preset: str,
        ipv4: Sequence[str] | None = None,
        ipv6: Sequence[str] | None = None,
        callback: Callback | None = None,
    ) -> Future[Any]:
        return self._submit(
            self._engine.network_set_dns,
            adapter_id,
            preset,
            _copy(ipv4),
            _copy(ipv6),
            restore_point="skip",
            dry_run=True,
            callback=callback,
        )

    def set_dns(
        self,
        adapter_id: str,
        preset: str,
        ipv4: Sequence[str] | None = None,
        ipv6: Sequence[str] | None = None,
        callback: Callback | None = None,
    ) -> Future[Any]:
        # DNS records revert on their own; a restore point would only delay the change.
        return self._submit(
            self._engine.network_set_dns,
            adapter_id,
            preset,
            _copy(ipv4),
            _copy(ipv6),
            restore_point="skip",
            dry_run=False,
            callback=callback,
        )

    def flush_dns(self, callback: Callback | None = None) -> Future[Any]:
        return self._submit(self._engine.network_flush_dns, callback=callback)

    def renew_lease(self, adapter_id: str, callback: Callback | None = None) -> Future[Any]:
        return self._submit(
            self._engine.network_renew_dhcp, adapter_id, release_first=False, callback=callback
        )

    def plan_network_reset(self, callback: Callback | None = None) -> Future[Any]:
        return self._submit(self._engine.network_reset, restore_point="skip", dry_run=True, callback=callback)

    def network_reset(self, callback: Callback | None = None) -> Future[Any]:
        policy = "try" if self.restore_points else "skip"
        return self._submit(
            self._engine.network_reset, restore_point=policy, dry_run=False, callback=callback
        )

    # -- maintenance tools ---------------------------------------------------------------

    def tools_catalog(self, callback: Callback | None = None) -> Future[Any]:
        return self._submit(self._engine.tools_catalog, callback=callback)

    def tools_volumes(self, callback: Callback | None = None) -> Future[Any]:
        return self._submit(self._engine.tools_volumes, callback=callback)

    def system_restore_enabled(self, callback: Callback | None = None) -> Future[Any]:
        return self._submit(self._engine.system_restore_enabled, callback=callback)

    def windows_tools(self, callback: Callback | None = None) -> Future[Any]:
        return self._submit(self._engine.tools_windows, callback=callback)

    def plan_tool(
        self, tool: str, volume: str | None = None, callback: Callback | None = None
    ) -> Future[Any]:
        return self._submit(self._engine.tools_start, tool, volume, dry_run=True, callback=callback)

    def start_tool(
        self, tool: str, volume: str | None = None, callback: Callback | None = None
    ) -> Future[Any]:
        # Queued on the worker so a start never interleaves with a scan or a change; returns once the
        # process runs.
        return self._submit(self._engine.tools_start, tool, volume, dry_run=False, callback=callback)

    def open_tool_log(self, job_id: int, callback: Callback | None = None) -> Future[Any]:
        return self._submit(self._engine.tools_open_log, job_id, callback=callback)

    def create_restore_point(self, callback: Callback | None = None) -> Future[Any]:
        return self._submit(self._engine.tools_restore_point, callback=callback)

    def open_windows_tool(self, tool_id: str, callback: Callback | None = None) -> Future[Any]:
        return self._submit(self._engine.tools_open_windows, tool_id, callback=callback)

    def tool_job(self, job_id: int, after: int = 0) -> dict[str, Any] | None:
        """The job's state and its output lines after `after`; synchronous and non-blocking."""
        return self._engine.tools_job(job_id, after)

    def tool_jobs(self) -> list[dict[str, Any]]:
        """Running and recently finished jobs, newest first; synchronous and non-blocking."""
        fn = getattr(self._engine, "tools_jobs", None)
        return list(fn()) if fn is not None else []

    def cancel_tool(self, job_id: int) -> bool:
        """Asks a stoppable job to stop; synchronous and non-blocking."""
        return bool(self._engine.tools_cancel(job_id))

    def tools_shutdown(self) -> list[dict[str, Any]]:
        """Stops or detaches the running jobs before the window closes. Blocks the calling thread
        for up to 10 s while the result of a job whose process has ended is read, and for up to
        3 s while a start settles or a stopped job ends."""
        fn = getattr(self._engine, "tools_shutdown", None)  # tolerates an outdated module on the close path
        return list(fn()) if fn is not None else []

    # -- shell -------------------------------------------------------------------------

    def elevated_as_other_user(self, *, callback: Callback | None = None) -> Future[Any]:
        """Queued: whether this process runs as another account than the signed-in user (bool);
        the future raises when that can't be decided."""
        return self._submit(self._engine.elevated_as_other_user, callback=callback)

    def journal_path(self) -> str | None:
        """Path of the journal the engine opens; None for a module without the function."""
        fn = getattr(self._engine, "journal_path", None)
        return str(fn()) if callable(fn) else None

    def install_info(self, *, callback: Callback | None = None) -> Future[Any]:
        """Queued: the installed copy's location and version, or None when Cairn is not installed."""
        return self._submit(self._engine.install_info, callback=callback)

    # -- permissions -------------------------------------------------------------------

    def permissions_list(self, callback: Callback | None = None) -> Future[Any]:
        """Queued: the permissions guide (each capability's Settings page and the desktop apps
        Windows recorded using the device). Read-only."""
        return self._submit(self._engine.permissions_list, callback=callback)

    def plan_permission(self, entry_id: str, allow: bool, callback: Callback | None = None) -> Future[Any]:
        """Queued dry run of a permission change; the engine refuses it (RuntimeError)."""
        return self._submit(
            self._engine.permissions_set,
            entry_id,
            allow,
            restore_point="skip",
            dry_run=True,
            callback=callback,
        )

    def set_permission(self, entry_id: str, allow: bool, callback: Callback | None = None) -> Future[Any]:
        """Queued permission change; the engine refuses it (RuntimeError) and changes nothing."""
        return self._submit(
            self._engine.permissions_set,
            entry_id,
            allow,
            restore_point="skip",
            dry_run=False,
            callback=callback,
        )

    # -- health ------------------------------------------------------------------------

    def security_checkup(self, callback: Callback | None = None) -> Future[Any]:
        return self._submit(self._engine.health_security_checkup, callback=callback)

    def boot_history(self, limit: int = 60, callback: Callback | None = None) -> Future[Any]:
        return self._submit(self._engine.health_boot_history, limit=limit, callback=callback)

    def start_update_scan(self, online: bool) -> dict[str, Any]:
        """Starts a Windows Update search on an engine thread; synchronous and non-blocking.
        RuntimeError while one runs."""
        return dict(self._engine.health_update_scan_start(online=online))

    def update_scan(self) -> dict[str, Any] | None:
        """The Windows Update search's state; synchronous and non-blocking."""
        fn = getattr(self._engine, "health_update_scan", None)
        return dict(fn()) if fn is not None else None

    def cancel_update_scan(self) -> bool:
        """Asks a running Windows Update search to stop; synchronous and non-blocking."""
        fn = getattr(self._engine, "health_update_scan_cancel", None)  # tolerated on the close path
        return bool(fn()) if fn is not None else False

    # -- storage -----------------------------------------------------------------------

    def storage_volumes(self, callback: Callback | None = None) -> Future[Any]:
        return self._submit(self._engine.storage_volumes, callback=callback)

    def speed_history(self, callback: Callback | None = None) -> Future[Any]:
        return self._submit(self._engine.storage_speed_history, callback=callback)

    def plan_speed_test(
        self, volume: str, size_bytes: int, runs: int, callback: Callback | None = None
    ) -> Future[Any]:
        return self._submit(
            self._engine.storage_speed_start, volume, size_bytes, runs, dry_run=True, callback=callback
        )

    def start_speed_test(
        self, volume: str, size_bytes: int, runs: int, callback: Callback | None = None
    ) -> Future[Any]:
        # Queued on the worker so a start never interleaves with a change; returns once the job runs.
        return self._submit(
            self._engine.storage_speed_start, volume, size_bytes, runs, dry_run=False, callback=callback
        )

    def remove_speed_leftover(self, path: str, callback: Callback | None = None) -> Future[Any]:
        return self._submit(self._engine.storage_remove_leftover, path, callback=callback)

    def plan_storage_scan(self, path: str, callback: Callback | None = None) -> Future[Any]:
        return self._submit(self._engine.storage_scan_start, path, dry_run=True, callback=callback)

    def start_storage_scan(self, path: str, callback: Callback | None = None) -> Future[Any]:
        return self._submit(self._engine.storage_scan_start, path, dry_run=False, callback=callback)

    def plan_duplicates(self, scan_job: int, min_size: int, callback: Callback | None = None) -> Future[Any]:
        return self._submit(
            self._engine.storage_duplicates_start, scan_job, min_size, dry_run=True, callback=callback
        )

    def start_duplicates(self, scan_job: int, min_size: int, callback: Callback | None = None) -> Future[Any]:
        return self._submit(
            self._engine.storage_duplicates_start, scan_job, min_size, dry_run=False, callback=callback
        )

    def storage_job(self, job_id: int) -> dict[str, Any] | None:
        """The storage job's snapshot; synchronous and non-blocking."""
        return self._engine.storage_job(job_id)

    def storage_jobs(self) -> list[dict[str, Any]]:
        """Running and retained storage jobs, newest first; synchronous and non-blocking."""
        fn = getattr(self._engine, "storage_jobs", None)  # tolerated on the close path
        return list(fn()) if fn is not None else []

    def cancel_storage(self, job_id: int) -> bool:
        """Asks a storage job to stop; synchronous and non-blocking."""
        return bool(self._engine.storage_cancel(job_id))

    def storage_result(self, job_id: int) -> dict[str, Any] | None:
        """The finished storage job's result; synchronous and non-blocking."""
        return self._engine.storage_result(job_id)

    def storage_children(
        self, job_id: int, node: int, order: str = "allocated", limit: int = 500
    ) -> dict[str, Any] | None:
        """One folder's entries of a scan result; synchronous and non-blocking."""
        return self._engine.storage_scan_children(job_id, node, order, limit)

    def storage_shutdown(self) -> list[dict[str, Any]]:
        """Stops the running storage job before the window closes; blocks for a few seconds at most."""
        fn = getattr(self._engine, "storage_shutdown", None)  # tolerated on the close path
        return list(fn()) if fn is not None else []

    # -- updates -----------------------------------------------------------------------

    def updates_winget_status(self, callback: Callback | None = None) -> Future[Any]:
        return self._submit(self._engine.updates_winget_status, callback=callback)

    def updates_wu_state(self, callback: Callback | None = None) -> Future[Any]:
        return self._submit(self._engine.updates_wu_state, callback=callback)

    def plan_wu_setting(self, setting: str, value: Any, callback: Callback | None = None) -> Future[Any]:
        return self._submit(
            self._engine.updates_wu_set, setting, value, restore_point="skip", dry_run=True, callback=callback
        )

    def set_wu_setting(self, setting: str, value: Any, callback: Callback | None = None) -> Future[Any]:
        # Journaled registry values; a restore point would only delay the change.
        return self._submit(
            self._engine.updates_wu_set,
            setting,
            value,
            restore_point="skip",
            dry_run=False,
            callback=callback,
        )

    def updates_app_list(self, callback: Callback | None = None) -> Future[Any]:
        return self._submit(self._engine.updates_app_list, callback=callback)

    def save_app_list(
        self, apps: Sequence[dict[str, Any]] | None, callback: Callback | None = None
    ) -> Future[Any]:
        return self._submit(self._engine.updates_save_app_list, _copy_items(apps), callback=callback)

    def plan_updates(
        self, kind: str, items: Sequence[dict[str, Any]] | None, callback: Callback | None = None
    ) -> Future[Any]:
        return self._submit(
            self._engine.updates_start, kind, _copy_items(items), dry_run=True, callback=callback
        )

    def start_updates(
        self, kind: str, items: Sequence[dict[str, Any]] | None, callback: Callback | None = None
    ) -> Future[Any]:
        # Queued on the worker so a start never interleaves with a change; returns once the job runs.
        return self._submit(
            self._engine.updates_start, kind, _copy_items(items), dry_run=False, callback=callback
        )

    def open_updates_log(self, job_id: int, callback: Callback | None = None) -> Future[Any]:
        return self._submit(self._engine.updates_open_log, job_id, callback=callback)

    def updates_job(self, job_id: int, after: int = 0) -> dict[str, Any] | None:
        """The winget job's state and its output lines after `after`; synchronous and non-blocking."""
        return self._engine.updates_job(job_id, after)

    def updates_jobs(self) -> list[dict[str, Any]]:
        """Running and retained winget jobs, newest first; synchronous and non-blocking."""
        fn = getattr(self._engine, "updates_jobs", None)  # tolerated on the close path
        return list(fn()) if fn is not None else []

    def updates_result(self, job_id: int, since: int = 0) -> dict[str, Any] | None:
        """The winget job's result when its revision is newer than `since`; synchronous and non-blocking."""
        return self._engine.updates_result(job_id, since)

    def cancel_updates(self, job_id: int) -> bool:
        """Stops a check now, or a batch after the current app; synchronous and non-blocking."""
        return bool(self._engine.updates_cancel(job_id))

    def updates_shutdown(self) -> list[dict[str, Any]]:
        """Stops the running winget job before the window closes; blocks for a few seconds at most."""
        fn = getattr(self._engine, "updates_shutdown", None)  # tolerated on the close path
        return list(fn()) if fn is not None else []

    def updates_wu_catalog(self) -> list[dict[str, Any]]:
        """Windows Update settings with their journal targets; pure data read on the calling
        thread and cached."""
        if self._wu_catalog is None:
            fn = getattr(self._engine, "updates_wu_catalog", None)
            if fn is None:
                return []
            self._wu_catalog = list(fn())
        return self._wu_catalog

    # -- maintenance -------------------------------------------------------------------

    def maintenance_status(self, callback: Callback | None = None) -> Future[Any]:
        return self._submit(self._engine.maintenance_status, callback=callback)

    def plan_maintenance_schedule(
        self, config: dict[str, Any], callback: Callback | None = None
    ) -> Future[Any]:
        return self._submit(
            self._engine.maintenance_set_schedule, dict(config), dry_run=True, callback=callback
        )

    def set_maintenance_schedule(
        self, config: dict[str, Any], callback: Callback | None = None
    ) -> Future[Any]:
        return self._submit(
            self._engine.maintenance_set_schedule, dict(config), dry_run=False, callback=callback
        )

    def turn_off_maintenance(self, task_path: str, callback: Callback | None = None) -> Future[Any]:
        """Undoes the journal record of the scheduled task at `task_path`, which deletes the task."""
        return self._submit(
            self._engine.revert_targets, {"task_definitions": [task_path]}, dry_run=False, callback=callback
        )

    def remove_unrecorded_maintenance(self, callback: Callback | None = None) -> Future[Any]:
        return self._submit(self._engine.maintenance_remove_unrecorded, dry_run=False, callback=callback)

    def run_maintenance_now(self, callback: Callback | None = None) -> Future[Any]:
        return self._submit(self._engine.maintenance_run_now, callback=callback)

    def acknowledge_maintenance(self, run_id: int, callback: Callback | None = None) -> Future[Any]:
        return self._submit(self._engine.maintenance_acknowledge, run_id, callback=callback)

    def open_maintenance_log(self, run_id: int, callback: Callback | None = None) -> Future[Any]:
        return self._submit(self._engine.maintenance_open_log, run_id, callback=callback)

    def maintenance_watch(self, expect_start: bool = False) -> None:
        """Starts the engine's run monitor once; synchronous and non-blocking."""
        fn = getattr(self._engine, "maintenance_watch", None)
        if fn is not None:
            fn(expect_start=expect_start)

    def maintenance_progress(self) -> dict[str, Any] | None:
        """The run monitor's latest observation; synchronous and non-blocking."""
        fn = getattr(self._engine, "maintenance_progress", None)
        return fn() if fn is not None else None

    # -- profiles ----------------------------------------------------------------------

    def profile_starters(self) -> list[dict[str, Any]]:
        """Built-in starter profiles; pure data read on the calling thread and cached."""
        if self._profile_starters is None:
            self._profile_starters = list(self._engine.profile_starters())
        return self._profile_starters

    def read_profile(self, path: str, callback: Callback | None = None) -> Future[Any]:
        return self._submit(self._engine.profile_read, path, callback=callback)

    def plan_profile(self, text: str, callback: Callback | None = None) -> Future[Any]:
        return self._submit(
            self._engine.profile_apply, text, None, restore_point="skip", dry_run=True, callback=callback
        )

    def apply_profile(self, text: str, keys: Sequence[str], callback: Callback | None = None) -> Future[Any]:
        # The first apply of the bridge's lifetime asks for a restore point, like apply().
        return self._mutation(self._engine.profile_apply, text, list(keys), callback=callback)

    def profile_candidates(self, callback: Callback | None = None) -> Future[Any]:
        return self._submit(self._engine.profile_candidates, callback=callback)

    def export_profile(
        self, path: str, name: str, description: str, keys: Sequence[str], callback: Callback | None = None
    ) -> Future[Any]:
        return self._submit(
            self._engine.profile_export, path, name, description, list(keys), callback=callback
        )
