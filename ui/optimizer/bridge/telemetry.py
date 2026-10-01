"""ctypes binding for optimizer_telemetry.dll.

The structures below mirror telemetry/include/telemetry/telemetry.h field for field. Every
struct in that header uses natural x64 alignment with explicit padding, so a plain
ctypes.Structure (no _pack_) reproduces the MSVC layout. The module-level assertions pin the
sizes and the offsets that the DLL itself asserts in sampler.h; a failure there means the
header and this mirror have diverged and the DLL must not be loaded.

Calling convention is __cdecl, so ctypes.CDLL is the correct loader and the GIL is released
for the duration of every foreign call.
"""

from __future__ import annotations

import ctypes
import os
import threading
from enum import IntEnum
from pathlib import Path
from typing import Any, NoReturn

from .. import NATIVE_DIR

__all__ = [
    "TEL_ABI_VERSION",
    "TEL_MAX_CORES",
    "TEL_MAX_TOP_PROCESSES",
    "TEL_PROCESS_NAME_CHARS",
    "TelConfig",
    "TelCpu",
    "TelCpuCore",
    "TelMemory",
    "TelProcess",
    "TelProcessSummary",
    "TelSnapshot",
    "TelStatus",
    "TelThreadStates",
    "Telemetry",
    "TelemetryError",
    "cores",
    "snapshot_to_dict",
    "top_processes",
]

TEL_ABI_VERSION = 2
TEL_MAX_CORES = 256
TEL_MAX_TOP_PROCESSES = 32
TEL_PROCESS_NAME_CHARS = 64

DEFAULT_DLL_NAME = "optimizer_telemetry.dll"


class TelStatus(IntEnum):
    """Return codes of every int32_t-returning export."""

    TEL_OK = 0
    TEL_E_NOT_INITIALIZED = -1
    TEL_E_ALREADY_INITIALIZED = -2
    TEL_E_INVALID_ARGUMENT = -3
    TEL_E_NTSTATUS = -4  # a sampler failed to initialise or sample; see tel_last_error()
    TEL_E_WIN32 = -5  # a lifecycle call failed; see tel_last_error()
    TEL_E_NO_SAMPLE = -6  # no sample has been published yet


# ---------------------------------------------------------------------------------------
# ABI structures
# ---------------------------------------------------------------------------------------


class TelConfig(ctypes.Structure):
    _fields_ = [
        ("sample_interval_ms", ctypes.c_uint32),  # CPU + memory cadence (default 16)
        ("process_interval_ms", ctypes.c_uint32),  # process/thread scan cadence (default 500)
        ("top_process_count", ctypes.c_uint32),  # <= TEL_MAX_TOP_PROCESSES (default 16)
        ("flags", ctypes.c_uint32),  # reserved, must be 0
    ]


class TelCpuCore(ctypes.Structure):
    _fields_ = [
        ("utilization", ctypes.c_float),  # % busy = 100 - idle
        ("kernel", ctypes.c_float),  # % kernel excluding idle (includes DPC + interrupt)
        ("user", ctypes.c_float),  # %
        ("dpc_interrupt", ctypes.c_float),  # % time in DPC + ISR
        ("frequency_mhz", ctypes.c_uint32),  # effective clock (Task Manager "speed"), 0 if unknown
        ("interrupts_per_sec", ctypes.c_uint32),
    ]


class TelCpu(ctypes.Structure):
    _fields_ = [
        ("core_count", ctypes.c_uint32),  # logical processors reported (<= TEL_MAX_CORES)
        ("group_count", ctypes.c_uint32),  # processor groups
        ("total_utilization", ctypes.c_float),  # mean over cores
        ("total_kernel", ctypes.c_float),
        ("total_user", ctypes.c_float),
        ("total_dpc_interrupt", ctypes.c_float),
        ("context_switches_per_sec", ctypes.c_uint64),
        ("syscalls_per_sec", ctypes.c_uint64),
        ("interrupts_per_sec", ctypes.c_uint64),
        ("cores", TelCpuCore * TEL_MAX_CORES),
    ]


class TelMemory(ctypes.Structure):
    _fields_ = [
        ("physical_total_bytes", ctypes.c_uint64),
        ("physical_available_bytes", ctypes.c_uint64),
        ("physical_used_bytes", ctypes.c_uint64),
        ("commit_total_bytes", ctypes.c_uint64),
        ("commit_limit_bytes", ctypes.c_uint64),
        ("commit_peak_bytes", ctypes.c_uint64),
        ("system_cache_bytes", ctypes.c_uint64),
        ("kernel_paged_pool_bytes", ctypes.c_uint64),
        ("kernel_nonpaged_pool_bytes", ctypes.c_uint64),
        ("page_size", ctypes.c_uint64),
        ("page_faults_per_sec", ctypes.c_uint64),  # all faults, soft + hard
        ("hard_faults_per_sec", ctypes.c_uint64),  # pages read from disk to satisfy faults
        ("page_reads_per_sec", ctypes.c_uint64),  # read I/O operations issued for faults
        ("pages_output_per_sec", ctypes.c_uint64),  # dirty + mapped pages written to disk
        ("page_writes_per_sec", ctypes.c_uint64),  # write I/O operations issued
        ("memory_load_percent", ctypes.c_float),
        ("commit_percent", ctypes.c_float),
    ]


class TelThreadStates(ctypes.Structure):
    _fields_ = [
        ("total", ctypes.c_uint32),
        ("running", ctypes.c_uint32),  # executing on a core
        ("ready", ctypes.c_uint32),  # runnable and waiting for a core (Ready, DeferredReady, Standby)
        ("waiting", ctypes.c_uint32),
        ("other", ctypes.c_uint32),  # initialized, transition, terminated, ...
        ("_pad0", ctypes.c_uint32),
    ]


class TelProcessSummary(ctypes.Structure):
    _fields_ = [
        ("process_count", ctypes.c_uint32),
        ("handle_count", ctypes.c_uint32),
        ("threads", TelThreadStates),
        ("scan_cost_us", ctypes.c_uint64),  # time spent inside the last process scan
        ("scan_age_ms", ctypes.c_uint64),  # age of the process data relative to the snapshot timestamp
    ]


class TelProcess(ctypes.Structure):
    _fields_ = [
        ("pid", ctypes.c_uint32),
        ("parent_pid", ctypes.c_uint32),
        ("thread_count", ctypes.c_uint32),
        ("handle_count", ctypes.c_uint32),
        ("working_set_bytes", ctypes.c_uint64),
        ("private_bytes", ctypes.c_uint64),
        ("cpu_percent", ctypes.c_float),  # share of total machine capacity, 0..100
        ("_pad0", ctypes.c_uint32),
        ("name", ctypes.c_wchar * TEL_PROCESS_NAME_CHARS),  # image name, NUL-terminated, truncated
    ]


class TelSnapshot(ctypes.Structure):
    _fields_ = [
        ("abi_version", ctypes.c_uint32),
        ("struct_size", ctypes.c_uint32),
        ("sequence", ctypes.c_uint64),  # increments for every published sample
        ("timestamp_qpc", ctypes.c_uint64),  # QueryPerformanceCounter at publish time
        ("qpc_frequency", ctypes.c_uint64),
        ("uptime_ms", ctypes.c_uint64),
        ("sample_interval_ms", ctypes.c_double),  # measured interval between the last two samples
        ("sample_cost_us", ctypes.c_double),  # time spent producing this sample
        ("cpu", TelCpu),
        ("memory", TelMemory),
        ("processes", TelProcessSummary),
        ("top_process_count", ctypes.c_uint32),
        ("_pad0", ctypes.c_uint32),
        ("top_processes", TelProcess * TEL_MAX_TOP_PROCESSES),
    ]


# Layout guards mirroring the static_asserts in telemetry/src/sampler.h. c_wchar is two
# bytes on Windows, which the TelProcess size depends on.
assert ctypes.sizeof(ctypes.c_wchar) == 2
assert ctypes.sizeof(TelConfig) == 16
assert ctypes.sizeof(TelCpuCore) == 24
assert ctypes.sizeof(TelCpu) == 6192
assert ctypes.sizeof(TelMemory) == 128
assert ctypes.sizeof(TelThreadStates) == 24
assert ctypes.sizeof(TelProcessSummary) == 48
assert ctypes.sizeof(TelProcess) == 168
assert ctypes.sizeof(TelSnapshot) == 11808
assert TelSnapshot.cpu.offset == 56
assert TelSnapshot.memory.offset == 6248
assert TelSnapshot.processes.offset == 6376
assert TelSnapshot.top_processes.offset == 6432

_SNAPSHOT_SIZE = ctypes.sizeof(TelSnapshot)

# Export name -> (argtypes, restype). None as restype means void.
_EXPORTS: dict[str, tuple[list[Any], Any]] = {
    "tel_abi_version": ([], ctypes.c_uint32),
    "tel_snapshot_size": ([], ctypes.c_uint32),
    "tel_version_string": ([], ctypes.c_char_p),
    "tel_default_config": ([ctypes.POINTER(TelConfig)], None),
    "tel_init": ([ctypes.POINTER(TelConfig)], ctypes.c_int32),
    "tel_shutdown": ([], None),
    "tel_is_running": ([], ctypes.c_int32),
    "tel_snapshot": ([ctypes.POINTER(TelSnapshot)], ctypes.c_int32),
    "tel_sample_now": ([], ctypes.c_int32),
    "tel_set_intervals": ([ctypes.c_uint32, ctypes.c_uint32], ctypes.c_int32),
    "tel_last_error": ([], ctypes.c_char_p),
    "tel_core_count": ([], ctypes.c_uint32),
}


# ---------------------------------------------------------------------------------------
# Errors
# ---------------------------------------------------------------------------------------


def _status_name(code: int) -> str:
    try:
        return TelStatus(code).name
    except ValueError:
        return "unknown status"


class TelemetryError(RuntimeError):
    """An export returned a non-zero TelStatus, or the DLL failed load-time validation.

    Attributes:
        status: the TelStatus (or raw int for codes this module does not know). None when
            the failure happened before any call reached the sampler, such as a missing
            export or an ABI mismatch.
        last_error: the thread-local text from tel_last_error(), empty when none was recorded.
    """

    status: TelStatus | int | None
    last_error: str

    def __init__(self, message: str, status: int | None = None, last_error: str = "") -> None:
        if status is not None:
            try:
                status = TelStatus(status)
            except ValueError:
                pass
        self.status = status
        self.last_error = last_error
        text = message
        if status is not None:
            text = f"{message}: {_status_name(status)} ({int(status)})"
        if last_error:
            text = f"{text}: {last_error}"
        super().__init__(text)


# ---------------------------------------------------------------------------------------
# Loader
# ---------------------------------------------------------------------------------------


# Directories already passed to os.add_dll_directory, keyed by normalised path. The
# registrations are kept for the life of the process so that dependencies which load after
# the DLL (and the PyO3 engine module sharing the directory) keep resolving; the cache only
# prevents registering the same directory again on every Telemetry() construction.
_dll_directories: dict[str, Any] = {}


def _add_dll_directory(directory: Path) -> None:
    key = os.path.normcase(str(directory))
    if key not in _dll_directories:
        _dll_directories[key] = os.add_dll_directory(str(directory))


# Sampler ownership per loaded DLL image, keyed by module handle. Every CDLL loaded from the
# same path shares one HMODULE and therefore one sampler; a separate copy of the DLL has its
# own handle and its own sampler. The value is the token of the instance whose start() last
# succeeded, and any instance's stop() removes the entry. An opaque per-instance token is
# stored rather than a weakref, which the cyclic collector clears before __del__ runs, or a
# strong reference, which would keep the instance alive so __del__ never runs. The lock is
# reentrant because __del__ can run on a thread that already holds it, when a collection is
# triggered inside start() or stop().
_sampler_owner: dict[int, object] = {}
_sampler_owner_lock = threading.RLock()


def _u32(name: str, value: int) -> int:
    # ctypes silently wraps out-of-range ints into c_uint32; a negative interval would
    # become a ~49-day wait instead of an error.
    if not 0 <= value <= 0xFFFFFFFF:
        raise ValueError(f"{name} must be in 0..4294967295, got {value}")
    return value


def _load_library(path: Path) -> ctypes.CDLL:
    """Load the DLL, declare every export's signature and validate the ABI."""
    if not path.is_file():
        raise FileNotFoundError(f"telemetry DLL not found: {path}")
    _add_dll_directory(path.parent)
    lib = ctypes.CDLL(str(path))
    for name, (argtypes, restype) in _EXPORTS.items():
        try:
            fn = getattr(lib, name)
        except AttributeError as exc:
            raise TelemetryError(f"{path.name} does not export {name}") from exc
        fn.argtypes = argtypes
        fn.restype = restype
    abi = lib.tel_abi_version()
    if abi != TEL_ABI_VERSION:
        raise TelemetryError(f"{path.name} has ABI version {abi}, expected {TEL_ABI_VERSION}")
    size = lib.tel_snapshot_size()
    if size != _SNAPSHOT_SIZE:
        raise TelemetryError(f"{path.name} has sizeof(TelSnapshot) == {size}, expected {_SNAPSHOT_SIZE}")
    return lib


class Telemetry:
    """Lifecycle and polling wrapper over the sampler exported by optimizer_telemetry.dll.

    The sampler is process-global: the DLL runs one background thread regardless of how many
    Telemetry instances exist, so a second start() while another instance is running fails
    with TEL_E_ALREADY_INITIALIZED.

    stop() always shuts the sampler down, whichever instance started it, and clears the
    ownership recorded by the last successful start(). Garbage collection shuts the sampler
    down only if this instance's start() is the most recent successful one for its DLL and no
    instance has called stop() since, so a stale instance being collected never stops a
    sampler that another instance has started in the meantime.

    snapshot() with copy=False writes into one TelSnapshot owned by the instance and returns
    that same object, so the returned structure is overwritten by the next copy=False call.
    That buffer belongs to a single polling thread (the UI thread); a copy=False call on the
    same instance from any other thread overwrites it while the owner may be reading it.
    snapshot(copy=True) fills a new TelSnapshot owned by the caller and never reads or writes
    the instance buffer, so any number of threads may use it at once, including alongside the
    copy=False poller. Callers that keep a sample also pass copy=True. Another thread that
    needs the allocation-free path uses its own Telemetry instance, which has its own buffer
    and can poll the running sampler without calling start().
    """

    _lib: ctypes.CDLL

    def __init__(self, dll_path: str | os.PathLike[str] | None = None) -> None:
        # Identity recorded in _sampler_owner by a successful start(). Bound before anything
        # that can raise.
        self._token = object()
        path = Path(dll_path) if dll_path is not None else NATIVE_DIR / DEFAULT_DLL_NAME
        self.path = path.resolve()
        lib = _load_library(self.path)
        self._hmodule: int = lib._handle
        self._snapshot = TelSnapshot()
        self._snapshot_ref = ctypes.byref(self._snapshot)
        # The bound export and the reusable byref() keep snapshot() free of attribute lookups
        # on the CDLL and of per-call argument allocation.
        self._tel_snapshot = lib.tel_snapshot
        self._lib = lib

    # -- lifecycle -----------------------------------------------------------------

    def start(
        self,
        sample_interval_ms: int = 16,
        process_interval_ms: int = 500,
        top_process_count: int = 16,
    ) -> None:
        """Start the sampler thread. Raises TelemetryError if it is already running."""
        config = TelConfig(
            _u32("sample_interval_ms", sample_interval_ms),
            _u32("process_interval_ms", process_interval_ms),
            _u32("top_process_count", top_process_count),
            0,
        )
        with _sampler_owner_lock:
            status = self._lib.tel_init(ctypes.byref(config))
            if status:
                self._raise("tel_init", status)
            _sampler_owner[self._hmodule] = self._token

    def stop(self) -> None:
        """Stop the sampler thread and release its buffers. Safe to call repeatedly.

        Stops the sampler whichever instance started it, and clears that ownership.
        """
        with _sampler_owner_lock:
            self._lib.tel_shutdown()
            _sampler_owner.pop(self._hmodule, None)

    def __enter__(self) -> Telemetry:
        return self

    def __exit__(self, *exc_info: object) -> None:
        self.stop()

    def __del__(self) -> None:
        try:
            with _sampler_owner_lock:
                if _sampler_owner.get(self._hmodule) is self._token:
                    self.stop()
        except (AttributeError, NameError, TypeError):
            # AttributeError: __init__ raised before the DLL was loaded, so nothing was
            # started. NameError / TypeError: the module globals were already torn down at
            # interpreter exit, where process teardown ends the sampler thread.
            pass

    def __repr__(self) -> str:
        running = self.running if hasattr(self, "_lib") else False
        return f"Telemetry(path={str(self.path)!r}, running={running})"

    # -- properties ----------------------------------------------------------------

    @property
    def running(self) -> bool:
        return bool(self._lib.tel_is_running())

    @property
    def core_count(self) -> int:
        """Logical processors reported by tel_core_count()."""
        return int(self._lib.tel_core_count())

    @property
    def version(self) -> str:
        raw = self._lib.tel_version_string()
        return raw.decode("ascii", "replace") if raw else ""

    @property
    def abi_version(self) -> int:
        return int(self._lib.tel_abi_version())

    @property
    def snapshot_size(self) -> int:
        """sizeof(TelSnapshot) as compiled into the DLL."""
        return int(self._lib.tel_snapshot_size())

    def default_config(self) -> TelConfig:
        """The configuration tel_init() applies when given a null pointer."""
        config = TelConfig()
        self._lib.tel_default_config(ctypes.byref(config))
        return config

    # -- sampling ------------------------------------------------------------------

    def snapshot(self, copy: bool = False) -> TelSnapshot:
        """Copy the most recent published sample out of the DLL.

        With copy=False (the 60 Hz path) the instance's preallocated buffer is filled and
        returned; nothing is allocated. The buffer belongs to one polling thread and is
        overwritten by the next copy=False call on this instance.

        With copy=True the DLL writes directly into a new TelSnapshot owned by the caller,
        which later calls never touch. The instance buffer is neither read nor refreshed, so
        any number of threads may call this at once. Each result holds exactly one published
        sample because the DLL copies it under its publish lock.
        """
        if copy:
            fresh = TelSnapshot()
            status = self._tel_snapshot(ctypes.byref(fresh))
            if status:
                self._raise("tel_snapshot", status)
            return fresh
        status = self._tel_snapshot(self._snapshot_ref)
        if status:
            self._raise("tel_snapshot", status)
        return self._snapshot

    def sample_now(self) -> None:
        """Run a full synchronous sample on the calling thread and publish it."""
        status = self._lib.tel_sample_now()
        if status:
            self._raise("tel_sample_now", status)

    def set_intervals(self, sample_interval_ms: int, process_interval_ms: int) -> None:
        """Adjust both cadences while running. The DLL clamps values below 1 ms to 1 ms."""
        status = self._lib.tel_set_intervals(
            _u32("sample_interval_ms", sample_interval_ms),
            _u32("process_interval_ms", process_interval_ms),
        )
        if status:
            self._raise("tel_set_intervals", status)

    # -- internals -----------------------------------------------------------------

    def _raise(self, export: str, status: int) -> NoReturn:
        # tel_last_error() is thread-local, so it is read on the thread that just failed.
        raw = self._lib.tel_last_error()
        text = raw.decode("utf-8", "replace") if raw else ""
        raise TelemetryError(f"{export} failed", status, text)


# ---------------------------------------------------------------------------------------
# Snapshot helpers
# ---------------------------------------------------------------------------------------


def cores(snapshot: TelSnapshot) -> list[TelCpuCore]:
    """The populated per-core entries. Each element is a view into `snapshot`, not a copy."""
    count = min(snapshot.cpu.core_count, TEL_MAX_CORES)
    return snapshot.cpu.cores[:count]


def top_processes(snapshot: TelSnapshot) -> list[TelProcess]:
    """The populated top-process entries, sorted by cpu_percent descending by the DLL."""
    count = min(snapshot.top_process_count, TEL_MAX_TOP_PROCESSES)
    return snapshot.top_processes[:count]


def _fields_to_dict(struct: ctypes.Structure) -> dict[str, Any]:
    """Plain-value dict of a structure's scalar and nested-structure fields, in header order.

    Padding fields are dropped. Array fields are skipped because their valid length is held
    by a sibling count field; the caller appends them explicitly.
    """
    result: dict[str, Any] = {}
    for field in struct._fields_:
        name = field[0]
        if name.startswith("_pad"):
            continue
        value = getattr(struct, name)
        if isinstance(value, ctypes.Array):
            continue
        if isinstance(value, ctypes.Structure):
            value = _fields_to_dict(value)
        result[name] = value
    return result


def snapshot_to_dict(snapshot: TelSnapshot) -> dict[str, Any]:
    """Nested dict of plain Python values (int, float, str, list, dict) for logging and JSON.

    Only the first core_count cores and top_process_count processes are included. Process
    names are decoded strings in which unpaired UTF-16 surrogates are replaced with U+FFFD,
    so the result is safe to encode as UTF-8. Such surrogates come only from image names that
    already contain one (legal in NTFS names); the DLL's truncation never splits a pair.
    """
    data = _fields_to_dict(snapshot)
    data["cpu"]["cores"] = [_fields_to_dict(core) for core in cores(snapshot)]
    procs = [_fields_to_dict(proc) for proc in top_processes(snapshot)]
    for proc in procs:
        proc["name"] = safe_text(proc["name"])
    data["top_processes"] = procs
    return data


def safe_text(text: str) -> str:
    """Replace unpaired UTF-16 surrogates with U+FFFD."""
    return text.encode("utf-16-le", "surrogatepass").decode("utf-16-le", "replace")
