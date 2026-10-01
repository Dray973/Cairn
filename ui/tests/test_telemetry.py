"""Integration tests for the optimizer_telemetry.dll ctypes bridge.

Every test loads the real DLL, so the module is skipped when the DLL has not been deployed to
optimizer/native. Most tests also start the sampler thread inside the DLL. The sampler is
process-global; each test that starts it stops it again through the fixture teardown so the
next test begins from a clean state.
"""

from __future__ import annotations

import contextlib
import ctypes
import gc
import itertools
import json
import os
import statistics
import threading
import time
from collections.abc import Iterator
from ctypes import wintypes
from typing import NamedTuple

import pytest

from optimizer import NATIVE_DIR
from optimizer.bridge.telemetry import (
    TEL_ABI_VERSION,
    TEL_MAX_CORES,
    TEL_MAX_TOP_PROCESSES,
    TelCpu,
    TelCpuCore,
    Telemetry,
    TelemetryError,
    TelMemory,
    TelProcess,
    TelProcessSummary,
    TelSnapshot,
    TelStatus,
    TelThreadStates,
    cores,
    snapshot_to_dict,
    top_processes,
)

DLL_PATH = NATIVE_DIR / "optimizer_telemetry.dll"
GIB = 1024**3
MIB = 1024**2

pytestmark = pytest.mark.skipif(not DLL_PATH.is_file(), reason=f"telemetry DLL not built: {DLL_PATH}")


@pytest.fixture
def tel() -> Iterator[Telemetry]:
    """A loaded but not started bridge; the sampler is always stopped on teardown."""
    instance = Telemetry(DLL_PATH)
    try:
        yield instance
    finally:
        instance.stop()


@pytest.fixture
def running(tel: Telemetry) -> Telemetry:
    """A started bridge; tel_init() has already published the first sample."""
    tel.start()
    tel.snapshot()  # tel_init publishes one sample synchronously before returning TEL_OK
    return tel


# ---------------------------------------------------------------------------------------
# CPU reference measurement
# ---------------------------------------------------------------------------------------

_ALL_PROCESSOR_GROUPS = 0xFFFF
_THREAD_PRIORITY_ABOVE_NORMAL = 1
_THREAD_PRIORITY_ERROR_RETURN = 0x7FFFFFFF
_CPU_WARMUP_S = 0.6  # longer than the trailing utilisation window, so it is full
_CPU_WINDOW_S = 1.5
_CPU_POLL_S = 1.0 / 120.0  # about twice the default 16 ms publish rate
# Every published total_utilization is the mean over a trailing window W of this length
# (CpuSampler::sample). A time average of such values over [a, b] weights the busy time in
# [a - W, b] with a trapezoid, which the box [a - W/2, b - W/2] matches best.
_CPU_TRAILING_WINDOW_S = 0.25
# Longest stretch of the collection without an observed sample. Shorter than the trailing
# window, so the sample that ends a gap still averages over the whole gap.
_CPU_MAX_GAP_S = 0.2
# Largest offset between the first or last sample and the matching end of the shifted
# reference interval. Nothing compensates for such an offset, so the limit is tight:
# nominally under about 35 ms (one poll plus one publish period plus the g1 read).
_CPU_MAX_EDGE_S = 0.075


class _CpuSample(NamedTuple):
    sequence: int
    timestamp_qpc: int
    utilization: float  # cpu.total_utilization


class _CpuWindow(NamedTuple):
    samples: list[_CpuSample]  # one per distinct sequence, in publish order
    mean_utilization: float  # samples weighted by the QPC time since the previous observed one
    system_utilization: float  # GetSystemTimes busy share over the matching interval, percent


class _SystemTimes(NamedTuple):
    qpc: int  # QueryPerformanceCounter right after the reading
    idle: int  # 100 ns units summed over every logical processor
    kernel: int  # includes idle
    user: int


def _kernel32() -> ctypes.WinDLL:
    # A private instance, so these prototypes do not leak into ctypes.windll.kernel32.
    k32 = ctypes.WinDLL("kernel32", use_last_error=True)
    k32.GetSystemTimes.argtypes = [ctypes.POINTER(wintypes.FILETIME)] * 3
    k32.GetSystemTimes.restype = wintypes.BOOL
    k32.GetActiveProcessorCount.argtypes = [wintypes.WORD]
    k32.GetActiveProcessorCount.restype = wintypes.DWORD
    k32.QueryPerformanceCounter.argtypes = [ctypes.POINTER(wintypes.LARGE_INTEGER)]
    k32.QueryPerformanceCounter.restype = wintypes.BOOL
    k32.GetCurrentThread.argtypes = []
    k32.GetCurrentThread.restype = wintypes.HANDLE
    k32.GetThreadPriority.argtypes = [wintypes.HANDLE]
    k32.GetThreadPriority.restype = ctypes.c_int
    k32.SetThreadPriority.argtypes = [wintypes.HANDLE, ctypes.c_int]
    k32.SetThreadPriority.restype = wintypes.BOOL
    return k32


def _filetime(value: wintypes.FILETIME) -> int:
    return (value.dwHighDateTime << 32) | value.dwLowDateTime


def _system_times(k32: ctypes.WinDLL) -> _SystemTimes:
    idle, kernel, user = wintypes.FILETIME(), wintypes.FILETIME(), wintypes.FILETIME()
    if not k32.GetSystemTimes(ctypes.byref(idle), ctypes.byref(kernel), ctypes.byref(user)):
        raise ctypes.WinError(ctypes.get_last_error())
    qpc = wintypes.LARGE_INTEGER()
    k32.QueryPerformanceCounter(ctypes.byref(qpc))  # cannot fail on Windows XP and later
    return _SystemTimes(qpc.value, _filetime(idle), _filetime(kernel), _filetime(user))


@contextlib.contextmanager
def _thread_priority_at_least(k32: ctypes.WinDLL, priority: int) -> Iterator[None]:
    """Runs the block with the calling thread at `priority` or above, where permitted."""
    thread = k32.GetCurrentThread()
    previous = k32.GetThreadPriority(thread)
    raised = (
        previous != _THREAD_PRIORITY_ERROR_RETURN
        and previous < priority
        and bool(k32.SetThreadPriority(thread, priority))
    )
    try:
        yield
    finally:
        if raised:
            k32.SetThreadPriority(thread, previous)


def _poll_cpu(tel: Telemetry, samples: list[_CpuSample], until: float) -> None:
    """Appends each distinct published sequence seen before perf_counter() reaches `until`."""
    while time.perf_counter() < until:
        s = tel.snapshot()
        if not samples or s.sequence != samples[-1].sequence:
            samples.append(_CpuSample(s.sequence, s.timestamp_qpc, s.cpu.total_utilization))
        time.sleep(_CPU_POLL_S)


def _collect_cpu_window(tel: Telemetry) -> _CpuWindow:
    """Poll total_utilization for _CPU_WINDOW_S and read GetSystemTimes over the matching interval.

    Every published value averages the trailing _CPU_TRAILING_WINDOW_S, so the reference
    interval g0..g1 leads the collection by half that window. The mean weights each sample
    by the QPC time since the previous observed one: while other processes saturate the
    cores the poll loop misses publishes, and a per-sample mean would under-weight exactly
    those intervals. The loop runs above normal priority, where permitted, so that it and
    the GetSystemTimes readings keep their timing under such load.

    `tel` must be running. The test is skipped when the snapshot does not cover every
    active logical processor, because GetSystemTimes always does.
    """
    k32 = _kernel32()
    first = tel.snapshot()
    frequency = first.qpc_frequency
    cpu = first.cpu
    active = int(k32.GetActiveProcessorCount(_ALL_PROCESSOR_GROUPS))
    if cpu.group_count != 1 or cpu.core_count != active:
        pytest.skip(
            f"snapshot covers {cpu.core_count} of {active} logical processors in "
            f"{cpu.group_count} groups; GetSystemTimes is not comparable"
        )
    time.sleep(_CPU_WARMUP_S)

    half_window_s = _CPU_TRAILING_WINDOW_S / 2
    samples: list[_CpuSample] = []
    with _thread_priority_at_least(k32, _THREAD_PRIORITY_ABOVE_NORMAL):
        g0 = _system_times(k32)
        time.sleep(half_window_s)
        deadline = time.perf_counter() + _CPU_WINDOW_S
        _poll_cpu(tel, samples, deadline - half_window_s)
        g1 = _system_times(k32)
        # Measured from g1 itself, so a late g1 read does not shorten the tail.
        _poll_cpu(tel, samples, time.perf_counter() + half_window_s)

    # KernelTime includes IdleTime.
    total = (g1.kernel - g0.kernel) + (g1.user - g0.user)
    busy = max(0, total - (g1.idle - g0.idle))
    assert total > 0, "GetSystemTimes did not advance"

    assert len(samples) >= 2, f"only {len(samples)} distinct samples observed in {_CPU_WINDOW_S} s"
    weighted = 0.0
    largest_gap = 0
    for previous, current in itertools.pairwise(samples):
        gap = current.timestamp_qpc - previous.timestamp_qpc
        weighted += gap * current.utilization
        largest_gap = max(largest_gap, gap)
    span = samples[-1].timestamp_qpc - samples[0].timestamp_qpc
    missed = samples[-1].sequence - samples[0].sequence + 1 - len(samples)
    assert span > 0 and largest_gap <= _CPU_MAX_GAP_S * frequency, (
        f"{len(samples)} samples ({missed} publishes missed) leave a "
        f"{largest_gap * 1000.0 / frequency:.0f} ms gap; every gap must stay within "
        f"{_CPU_MAX_GAP_S * 1000.0:.0f} ms"
    )
    # The samples also have to reach both ends of g0..g1 shifted by half the trailing window.
    shift = round(half_window_s * frequency)
    edge = max(
        abs(samples[0].timestamp_qpc - (g0.qpc + shift)),
        abs((g1.qpc + shift) - samples[-1].timestamp_qpc),
    )
    assert edge <= _CPU_MAX_EDGE_S * frequency, (
        f"samples miss an end of the reference interval by {edge * 1000.0 / frequency:.0f} ms; "
        f"the limit is {_CPU_MAX_EDGE_S * 1000.0:.0f} ms"
    )
    return _CpuWindow(samples, weighted / span, 100.0 * busy / total)


def test_abi_sizes_match_dll(tel: Telemetry) -> None:
    assert tel.abi_version == TEL_ABI_VERSION
    assert tel.snapshot_size == ctypes.sizeof(TelSnapshot) == 11808
    assert ctypes.sizeof(TelCpuCore) == 24
    assert ctypes.sizeof(TelCpu) == 6192
    assert ctypes.sizeof(TelMemory) == 128
    assert ctypes.sizeof(TelThreadStates) == 24
    assert ctypes.sizeof(TelProcessSummary) == 48
    assert ctypes.sizeof(TelProcess) == 168
    assert TelSnapshot.cpu.offset == 56
    assert TelSnapshot.memory.offset == 6248
    assert TelSnapshot.processes.offset == 6376
    assert TelSnapshot.top_processes.offset == 6432
    assert tel.version


def test_snapshot_contents_are_plausible(tel: Telemetry) -> None:
    tel.start()
    time.sleep(0.7)
    s = tel.snapshot()

    assert s.abi_version == TEL_ABI_VERSION
    assert s.struct_size == ctypes.sizeof(TelSnapshot)
    assert s.sequence >= 1
    assert s.qpc_frequency > 0
    assert s.uptime_ms > 0

    cpu = s.cpu
    assert 1 <= cpu.core_count <= TEL_MAX_CORES
    assert cpu.core_count == min(os.cpu_count() or 1, TEL_MAX_CORES)
    assert cpu.core_count == tel.core_count
    assert cpu.group_count >= 1
    for core in cores(s):
        assert 0.0 <= core.utilization <= 100.0
        assert 0.0 <= core.kernel <= 100.0
        assert 0.0 <= core.user <= 100.0
        assert 0.0 <= core.dpc_interrupt <= 100.0
    assert 0.0 <= cpu.total_utilization <= 100.0
    assert 0.0 <= cpu.total_kernel <= 100.0
    assert 0.0 <= cpu.total_user <= 100.0
    assert 0.0 <= cpu.total_dpc_interrupt <= 100.0

    mem = s.memory
    assert mem.physical_total_bytes > GIB
    assert mem.physical_used_bytes <= mem.physical_total_bytes
    assert mem.physical_available_bytes <= mem.physical_total_bytes
    assert mem.commit_limit_bytes >= mem.commit_total_bytes
    assert mem.page_size in (4096, 2 * MIB)
    assert 0.0 <= mem.memory_load_percent <= 100.0
    assert 0.0 <= mem.commit_percent <= 100.0

    procs = s.processes
    assert procs.process_count > 10
    assert procs.threads.total >= procs.process_count
    assert procs.threads.total >= (
        procs.threads.running + procs.threads.ready + procs.threads.waiting + procs.threads.other
    )

    assert 1 <= s.top_process_count <= TEL_MAX_TOP_PROCESSES
    top = top_processes(s)
    assert len(top) == s.top_process_count
    for proc in top:
        assert proc.name
        assert proc.pid > 0
        assert 0.0 <= proc.cpu_percent <= 100.0


def test_cpu_matches_get_system_times(running: Telemetry) -> None:
    window = _collect_cpu_window(running)
    assert abs(window.mean_utilization - window.system_utilization) < 10.0, (
        f"time-weighted mean total_utilization {window.mean_utilization:.1f}% vs GetSystemTimes "
        f"{window.system_utilization:.1f}% over {len(window.samples)} samples"
    )


def test_cpu_samples_are_smoothed(running: Telemetry) -> None:
    window = _collect_cpu_window(running)
    samples = window.samples

    # Consecutive publishes share most of the trailing window, so large steps between them
    # are rare. Pairs with missed publishes in between share less of it and are not compared.
    steps = [
        abs(b.utilization - a.utilization)
        for a, b in itertools.pairwise(samples)
        if b.sequence == a.sequence + 1
    ]
    assert steps, f"no two consecutive publishes among {len(samples)} observed samples"
    jumps = sum(step > 15.0 for step in steps)
    assert jumps < 0.2 * len(steps), (
        f"{jumps} of {len(steps)} consecutive samples moved by more than 15 points (largest {max(steps):.1f})"
    )

    # A window spanning several clock ticks per core is rarely all idle unless the machine
    # itself is.
    if window.system_utilization >= 0.5:
        zeros = sum(sample.utilization == 0.0 for sample in samples)
        assert zeros < 0.5 * len(samples), (
            f"{zeros} of {len(samples)} samples are exactly 0.0 while GetSystemTimes reports "
            f"{window.system_utilization:.2f}% busy"
        )


def test_sequence_increases_between_snapshots(running: Telemetry) -> None:
    # copy=True: a plain snapshot() returns the shared buffer, which s3 would overwrite.
    s1 = running.snapshot(copy=True)
    time.sleep(0.05)
    seq2 = running.snapshot().sequence
    time.sleep(0.05)
    s3 = running.snapshot()
    assert s1.sequence < seq2 < s3.sequence
    # One interval can be arbitrarily short when the sampler wakes late and the periodic
    # timer's next tick is already due, so a single interval is bounded only loosely.
    assert 0.0 < s3.sample_interval_ms <= 200.0
    # The mean over several publishes cancels per-tick wake jitter. The divisor is at least 2.
    elapsed_ms = (s3.timestamp_qpc - s1.timestamp_qpc) * 1000.0 / s3.qpc_frequency
    mean_ms = elapsed_ms / (s3.sequence - s1.sequence)
    assert 8.0 <= mean_ms <= 64.0, f"mean sample cadence {mean_ms:.2f} ms"


def test_snapshot_latency(running: Telemetry) -> None:
    for _ in range(20):
        running.snapshot()
    durations_ns = []
    for _ in range(500):
        t0 = time.perf_counter_ns()
        running.snapshot()
        durations_ns.append(time.perf_counter_ns() - t0)
    # A preemption of the calling thread lands on single calls and can last several
    # scheduler quanta, so the mean and max are unreliable. Order statistics are not moved by
    # a few preempted calls; the max bound only detects a reader that blocks outright.
    median_us = statistics.median(durations_ns) / 1_000
    p99_us = statistics.quantiles(durations_ns, n=100)[98] / 1_000
    max_ms = max(durations_ns) / 1_000_000
    assert median_us < 200.0, f"median snapshot() latency {median_us:.1f} us"
    assert p99_us < 5_000.0, f"p99 snapshot() latency {p99_us:.1f} us"
    assert max_ms < 1_000.0, f"max snapshot() latency {max_ms:.2f} ms"


def test_sample_now_publishes_a_new_sample(running: Telemetry) -> None:
    before = running.snapshot(copy=True)
    running.sample_now()
    after = running.snapshot()
    assert after.sequence > before.sequence
    assert after.timestamp_qpc >= before.timestamp_qpc


def test_snapshot_copy_is_independent(running: Telemetry) -> None:
    shared = running.snapshot()
    shared_sequence = shared.sequence
    copied = running.snapshot(copy=True)
    assert copied is not shared
    assert ctypes.addressof(copied) != ctypes.addressof(shared)
    # copy=True never refreshes the instance buffer; the sampler may publish in between.
    assert shared.sequence == shared_sequence
    assert copied.sequence >= shared_sequence
    frozen = copied.sequence
    running.sample_now()
    assert running.snapshot() is shared
    assert shared.sequence > frozen
    assert copied.sequence == frozen


def test_collecting_stale_instance_keeps_newer_sampler_running(tel: Telemetry) -> None:
    stale = Telemetry(DLL_PATH)
    stale.start()
    Telemetry(DLL_PATH).stop()  # another instance stops the sampler stale started
    tel.start()
    del stale
    gc.collect()
    assert tel.running
    assert tel.snapshot().sequence >= 1


def test_collecting_owner_without_stop_shuts_sampler_down(tel: Telemetry) -> None:
    owner = Telemetry(DLL_PATH)
    owner.start()
    owner.cycle = owner  # type: ignore[attr-defined]  # only the cyclic collector frees it
    token = owner._token
    del owner
    gc.collect()
    import optimizer.bridge.telemetry as bridge_module

    assert not tel.running, (
        f"owner registry={bridge_module._sampler_owner!r} owner_token={token!r} "
        f"garbage={len(gc.garbage)} threads={[t.name for t in threading.enumerate()]}"
    )


def test_concurrent_copies_are_never_torn(running: Telemetry) -> None:
    running.set_intervals(1, 1)
    size = ctypes.sizeof(TelSnapshot)
    seen: dict[int, bytes] = {}
    mismatches: list[int] = []
    lock = threading.Lock()
    deadline = time.perf_counter() + 1.5

    def reader() -> None:
        while time.perf_counter() < deadline:
            s = running.snapshot(copy=True)
            raw = ctypes.string_at(ctypes.addressof(s), size)
            with lock:
                previous = seen.setdefault(s.sequence, raw)
                if previous != raw:
                    mismatches.append(s.sequence)

    threads = [threading.Thread(target=reader) for _ in range(4)]
    for t in threads:
        t.start()
    for t in threads:
        t.join()
    assert len(seen) > 10
    assert mismatches == []


def test_start_stop_cycles(tel: Telemetry) -> None:
    for _ in range(5):
        assert not tel.running
        tel.start()
        assert tel.running
        s = tel.snapshot()
        assert s.cpu.core_count >= 1
        tel.stop()
        assert not tel.running
    tel.stop()
    assert not tel.running


def test_double_start_is_rejected(running: Telemetry) -> None:
    with pytest.raises(TelemetryError) as info:
        running.start()
    assert info.value.status == TelStatus.TEL_E_ALREADY_INITIALIZED
    assert running.running


def test_set_intervals_while_running(running: Telemetry) -> None:
    running.set_intervals(8, 250)
    seq = running.snapshot().sequence
    time.sleep(0.1)
    s = running.snapshot()
    assert s.sequence > seq
    assert 0.0 < s.sample_interval_ms <= 200.0
    with pytest.raises(ValueError):
        running.set_intervals(-1, 250)


def test_snapshot_before_start_raises(tel: Telemetry) -> None:
    assert not tel.running
    with pytest.raises(TelemetryError) as info:
        tel.snapshot()
    assert info.value.status == TelStatus.TEL_E_NOT_INITIALIZED
    assert "TEL_E_NOT_INITIALIZED" in str(info.value)


def test_snapshot_to_dict_is_json_serialisable(running: Telemetry) -> None:
    time.sleep(0.6)
    s = running.snapshot()
    data = snapshot_to_dict(s)
    encoded = json.dumps(data)
    decoded = json.loads(encoded)

    assert decoded["sequence"] == s.sequence
    assert decoded["cpu"]["core_count"] == s.cpu.core_count
    assert len(decoded["cpu"]["cores"]) == s.cpu.core_count
    assert decoded["memory"]["physical_total_bytes"] == s.memory.physical_total_bytes
    assert decoded["processes"]["threads"]["total"] == s.processes.threads.total
    assert len(decoded["top_processes"]) == s.top_process_count
    assert "_pad0" not in decoded
    assert "_pad0" not in decoded["processes"]["threads"]
    if decoded["top_processes"]:
        first = decoded["top_processes"][0]
        assert isinstance(first["name"], str)
        assert first["name"] == s.top_processes[0].name
        assert "_pad0" not in first
