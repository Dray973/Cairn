// optimizer_telemetry: lifecycle, sampler thread, snapshot publication and the exported
// C ABI. The sampling kernels live in cpu.cpp, memory.cpp and process.cpp.
//
// Locks, outermost first:
//   init_lock     serialises tel_init, tel_shutdown and tel_set_intervals.
//   sample_lock   serialises every use of the samplers and of the process cache, from the
//                 sampler thread and from tel_sample_now.
//   publish_lock  guards the published snapshot. Readers take it shared for one copy; the
//                 publish step takes it exclusive only after sample_lock is released, so a
//                 polling reader never waits behind a system call.
//
// Module lifetime: the sampler thread owns a reference on this DLL and leaves through
// FreeLibraryAndExitThread. The image therefore cannot be unmapped while the thread can
// still execute code in it, and FreeLibrary by the host never has to wait for the thread
// under the loader lock. The thread also waits on its own handles to the stop event and
// the timer, so shutdown can always close the module's handles, even when the thread
// outlives the stop timeout. Such a thread is kept in orphan_thread until it has exited.
// While it is kept, sample_lock is only tried, never waited for, with init_lock held,
// and tel_init refuses to start a new cycle.

#include "sampler.h"

#include <atomic>
#include <cassert>
#include <climits>
#include <cstdarg>
#include <cstddef>
#include <cstdio>
#include <cstring>
#include <memory>
#include <new>

// Windows 10 1803 and later. SDK targets below RS4 omit the flag and older kernels reject
// it with ERROR_INVALID_PARAMETER; tel_init falls back to a plain waitable timer then.
#ifndef CREATE_WAITABLE_TIMER_HIGH_RESOLUTION
#define CREATE_WAITABLE_TIMER_HIGH_RESOLUTION 0x00000002
#endif

namespace tel {

NtApi g_nt;

namespace {

constexpr uint32_t    kDefaultSampleIntervalMs  = 16;
constexpr uint32_t    kDefaultProcessIntervalMs = 500;
constexpr uint32_t    kDefaultTopProcessCount   = 16;
constexpr DWORD       kThreadStopTimeoutMs      = 5000;
constexpr size_t      kErrorChars               = 512;
constexpr size_t      kSystemMessageChars       = 256;  // UTF-16 units from FormatMessageW
constexpr const char* kVersionString            = "optimizer_telemetry 0.2.0";

thread_local char t_error[kErrorChars] = {};

// QueryPerformanceFrequency is fixed at boot. Concurrent first calls store the same value.
constinit std::atomic<uint64_t> g_qpc_frequency{0};

// Scoped SRWLOCK holders so an early return cannot leave a lock held.
class ExclusiveLock {
public:
    explicit ExclusiveLock(SRWLOCK& lock) noexcept : lock_(lock) { AcquireSRWLockExclusive(&lock_); }
    ~ExclusiveLock() { ReleaseSRWLockExclusive(&lock_); }
    ExclusiveLock(const ExclusiveLock&) = delete;
    ExclusiveLock& operator=(const ExclusiveLock&) = delete;

private:
    SRWLOCK& lock_;
};

class SharedLock {
public:
    explicit SharedLock(SRWLOCK& lock) noexcept : lock_(lock) { AcquireSRWLockShared(&lock_); }
    ~SharedLock() { ReleaseSRWLockShared(&lock_); }
    SharedLock(const SharedLock&) = delete;
    SharedLock& operator=(const SharedLock&) = delete;

private:
    SRWLOCK& lock_;
};

// The sampling kernels own heap buffers and have non-trivial destructors, so they live on
// the heap for one init/shutdown cycle instead of in a static whose destructor would run
// at process exit, after the sampler thread has been terminated mid-sample.
struct Samplers {
    CpuSampler     cpu;
    MemorySampler  memory;
    ProcessSampler process;
    bool cpu_ready     = false;
    bool memory_ready  = false;
    bool process_ready = false;

    bool all_ready() const { return cpu_ready && memory_ready && process_ready; }

    void shutdown_all() {
        if (process_ready) { process.shutdown(); process_ready = false; }
        if (memory_ready)  { memory.shutdown();  memory_ready  = false; }
        if (cpu_ready)     { cpu.shutdown();     cpu_ready     = false; }
    }
};

// Handed to the sampler thread, which takes ownership and frees it on entry. Everything
// in it belongs to the thread, which closes both handles and releases the module
// reference when it exits.
struct ThreadContext {
    HANDLE  stop_event;  // duplicate of State::stop_event
    HANDLE  timer;       // duplicate of State::timer
    HMODULE module;      // released by FreeLibraryAndExitThread
};

// All mutable module state. Zero is a valid initial value for every member and nothing
// here has a destructor, so the DLL registers no static destructors.
struct State {
    SRWLOCK init_lock;
    SRWLOCK sample_lock;
    SRWLOCK publish_lock;

    std::atomic<bool>     running;
    std::atomic<uint32_t> process_interval_ms;
    std::atomic<uint32_t> core_count;  // CpuSampler::core_count() for the current cycle
    // QPC of the sampler thread's latest timer tick, or of the arm time before the first
    // tick. tel_set_intervals places the first tick at a new period relative to it.
    std::atomic<uint64_t> last_tick_qpc;
    // Bumped under publish_lock whenever the published state is reset. A sample built in
    // an earlier cycle carries the old value and is discarded instead of published.
    std::atomic<uint64_t> epoch;

    // Guarded by init_lock.
    HANDLE   thread;
    HANDLE   stop_event;       // manual reset, so a late tick can never mask a stop request
    HANDLE   timer;            // auto-reset periodic timer, period = timer_period_ms
    uint32_t timer_period_ms;  // period the timer is currently armed with
    // Sampler thread of an earlier cycle that did not stop within kThreadStopTimeoutMs
    // and has not yet been seen to exit. It may still hold sample_lock.
    HANDLE   orphan_thread;

    // Guarded by sample_lock.
    Samplers*         samplers;
    TelProcessSummary proc_summary;
    TelProcess        top[TEL_MAX_TOP_PROCESSES];
    uint32_t          top_count;
    uint32_t          logical_processors;        // every active logical processor
    uint64_t          last_process_scan_qpc;     // last successful scan; scan_age_ms counts from it
    uint64_t          last_process_attempt_qpc;  // last scan attempt; the scan cadence counts from it
    uint64_t          last_publish_qpc;
    uint64_t          build_ordinal;  // monotonic across cycles, orders concurrent samples

    // Guarded by publish_lock.
    TelSnapshot published;
    bool        has_sample;
    uint64_t    sequence;
    uint64_t    published_ordinal;
};

constinit State g{};

// Cuts a UTF-8 sequence that truncation left incomplete off the end of `text`.
void drop_partial_utf8_tail(char* text) {
    const size_t length = std::strlen(text);
    size_t start = length;
    while (start > 0 && (static_cast<unsigned char>(text[start - 1]) & 0xC0u) == 0x80u) {
        --start;
    }
    if (start == 0) {
        return;
    }
    --start;  // the byte that begins the last sequence
    const unsigned char lead = static_cast<unsigned char>(text[start]);
    const size_t expected = lead >= 0xF0u ? 4 : lead >= 0xE0u ? 3 : lead >= 0xC0u ? 2 : 1;
    if (length - start < expected) {
        text[start] = '\0';
    }
}

}  // namespace

// ---------------------------------------------------------------------------------------
// Error text
// ---------------------------------------------------------------------------------------

void set_error(const char* fmt, ...) {
    if (fmt == nullptr) {
        t_error[0] = '\0';
        return;
    }
    va_list args;
    va_start(args, fmt);
    const int written = std::vsnprintf(t_error, kErrorChars, fmt, args);
    va_end(args);
    // vsnprintf NUL-terminates on truncation; only an encoding error leaves no text.
    if (written < 0) {
        t_error[0] = '\0';
    }
}

void set_nt_error(const char* what, nt::NTSTATUS status) {
    set_error("%s: NTSTATUS 0x%08X", what != nullptr ? what : "NT call",
              static_cast<unsigned>(status));
}

// The system message is reported in UTF-8. FormatMessageA would return it in the ANSI
// code page, which differs between locales.
void set_win32_error(const char* what, DWORD error) {
    wchar_t wide[kSystemMessageChars] = {};
    DWORD wide_length = FormatMessageW(FORMAT_MESSAGE_FROM_SYSTEM | FORMAT_MESSAGE_IGNORE_INSERTS,
                                       nullptr, error, 0, wide,
                                       static_cast<DWORD>(kSystemMessageChars), nullptr);
    if (wide_length >= kSystemMessageChars) {
        wide_length = static_cast<DWORD>(kSystemMessageChars) - 1;
    }
    // System messages end in "\r\n", sometimes preceded by a blank.
    while (wide_length > 0 && (wide[wide_length - 1] == L'\r' || wide[wide_length - 1] == L'\n' ||
                               wide[wide_length - 1] == L' ')) {
        --wide_length;
    }
    // Three bytes per UTF-16 unit covers every conversion (a surrogate pair needs four
    // bytes for two units), so the conversion cannot fail for lack of room.
    char message[3 * kSystemMessageChars + 1] = {};
    int length = 0;
    if (wide_length > 0) {
        length = WideCharToMultiByte(CP_UTF8, 0, wide, static_cast<int>(wide_length), message,
                                     static_cast<int>(sizeof message) - 1, nullptr, nullptr);
        if (length < 0) {
            length = 0;
        }
    }
    message[length] = '\0';

    const char* label = what != nullptr ? what : "Win32 call";
    if (length > 0) {
        set_error("%s: Win32 error %lu: %s", label, static_cast<unsigned long>(error), message);
    } else {
        set_error("%s: Win32 error %lu", label, static_cast<unsigned long>(error));
    }
    // A long message is truncated to the error buffer; keep what remains valid UTF-8.
    drop_partial_utf8_tail(t_error);
}

// ---------------------------------------------------------------------------------------
// Clock helpers
// ---------------------------------------------------------------------------------------

uint64_t qpc_frequency() {
    uint64_t frequency = g_qpc_frequency.load(std::memory_order_relaxed);
    if (frequency == 0) {
        LARGE_INTEGER value{};
        if (QueryPerformanceFrequency(&value) && value.QuadPart > 0) {
            frequency = static_cast<uint64_t>(value.QuadPart);
            g_qpc_frequency.store(frequency, std::memory_order_relaxed);
        }
    }
    return frequency;
}

uint64_t qpc_now() {
    LARGE_INTEGER value{};
    QueryPerformanceCounter(&value);
    return static_cast<uint64_t>(value.QuadPart);
}

double qpc_to_ms(uint64_t ticks) {
    const uint64_t frequency = qpc_frequency();
    if (frequency == 0) {
        return 0.0;
    }
    return static_cast<double>(ticks) * 1000.0 / static_cast<double>(frequency);
}

double qpc_to_us(uint64_t ticks) {
    const uint64_t frequency = qpc_frequency();
    if (frequency == 0) {
        return 0.0;
    }
    return static_cast<double>(ticks) * 1000000.0 / static_cast<double>(frequency);
}

namespace {

// Milliseconds to QPC ticks, split so the product cannot overflow for any 32-bit input.
uint64_t ms_to_qpc(uint32_t ms) {
    const uint64_t frequency = qpc_frequency();
    const uint64_t whole     = ms / 1000u;
    const uint64_t remainder = ms % 1000u;
    return whole * frequency + (remainder * frequency) / 1000u;
}

uint32_t clamp_interval(uint32_t ms) { return ms < 1 ? 1u : ms; }

// ---------------------------------------------------------------------------------------
// NT resolution
// ---------------------------------------------------------------------------------------

bool resolve_nt() {
    const HMODULE ntdll = GetModuleHandleW(L"ntdll.dll");
    if (ntdll == nullptr) {
        set_win32_error("GetModuleHandleW(ntdll.dll)", GetLastError());
        return false;
    }
    const FARPROC query = GetProcAddress(ntdll, "NtQuerySystemInformation");
    if (query == nullptr) {
        set_win32_error("GetProcAddress(NtQuerySystemInformation)", GetLastError());
        return false;
    }
    g_nt.query = reinterpret_cast<nt::PFN_NtQuerySystemInformation>(query);
    // Optional (exported since Windows 7). Only CpuSampler uses it: without it, on a
    // machine with several processor groups, CpuSampler reads just the calling thread's
    // group through query and publishes at most group 0's core count.
    g_nt.query_ex = reinterpret_cast<nt::PFN_NtQuerySystemInformationEx>(
        GetProcAddress(ntdll, "NtQuerySystemInformationEx"));
    return true;
}

// ---------------------------------------------------------------------------------------
// Sampling and publication
// ---------------------------------------------------------------------------------------

// The first sample of a cycle differences against baselines that init() took moments
// earlier, a window too short for utilisation percentages or per-second rates to mean
// anything, so that snapshot (the one with sample_interval_ms == 0) reports them as 0.
// Process cpu_percent is already 0 on a sampler's first scan.
void clear_interval_rates(TelSnapshot& snap) {
    TelCpu& cpu = snap.cpu;
    cpu.total_utilization        = 0.0f;
    cpu.total_kernel             = 0.0f;
    cpu.total_user               = 0.0f;
    cpu.total_dpc_interrupt      = 0.0f;
    cpu.context_switches_per_sec = 0;
    cpu.syscalls_per_sec         = 0;
    cpu.interrupts_per_sec       = 0;
    for (TelCpuCore& core : cpu.cores) {
        core.utilization        = 0.0f;
        core.kernel             = 0.0f;
        core.user               = 0.0f;
        core.dpc_interrupt      = 0.0f;
        core.interrupts_per_sec = 0;
    }
    TelMemory& memory = snap.memory;
    memory.page_faults_per_sec  = 0;
    memory.hard_faults_per_sec  = 0;
    memory.page_reads_per_sec   = 0;
    memory.pages_output_per_sec = 0;
    memory.page_writes_per_sec  = 0;
}

// Takes one sample on the calling thread and publishes it.
//
// A CPU or memory failure publishes nothing and leaves the published snapshot and the
// process cache as they were. A failed process scan still publishes the new CPU and
// memory data together with the cached process data, whose scan_age_ms keeps growing,
// and then returns TEL_E_NTSTATUS; failed scans count as attempts, so the scan is retried
// at the process cadence rather than on every tick. Every failure leaves the error text
// set by the step that failed.
int32_t run_sample(bool force_process_scan) {
    const uint64_t t0 = qpc_now();

    // Built on the stack so the publish copy can run after sample_lock is released.
    TelSnapshot snap;
    std::memset(&snap, 0, sizeof snap);
    uint64_t epoch          = 0;
    uint64_t ordinal        = 0;
    bool     process_failed = false;

    {
        ExclusiveLock lock(g.sample_lock);
        // sample_cost_us counts from here, so the time spent waiting for a concurrent
        // sample to release the lock is not charged to this one.
        const uint64_t t_locked = qpc_now();

        Samplers* const samplers = g.samplers;
        if (samplers == nullptr || !samplers->all_ready()) {
            set_error("telemetry is not initialised");
            return TEL_E_NOT_INITIALIZED;
        }
        epoch = g.epoch.load(std::memory_order_acquire);

        // t0 is read before the lock, so a concurrent sample may have recorded a later
        // attempt; treat that as "just attempted" instead of letting the difference wrap.
        const uint64_t last_attempt  = g.last_process_attempt_qpc;
        const uint64_t since_attempt = t0 > last_attempt ? t0 - last_attempt : 0;
        const bool process_due =
            force_process_scan || last_attempt == 0 ||
            since_attempt >= ms_to_qpc(g.process_interval_ms.load(std::memory_order_relaxed));

        // The per-core frequency query is the slow part of the CPU sample, so it follows
        // the process-scan cadence.
        if (!samplers->cpu.sample(snap.cpu, process_due)) {
            return TEL_E_NTSTATUS;
        }

        SystemRates rates{};
        if (!samplers->memory.sample(snap.memory, rates)) {
            return TEL_E_NTSTATUS;
        }
        snap.cpu.context_switches_per_sec = rates.context_switches_per_sec;
        snap.cpu.syscalls_per_sec         = rates.syscalls_per_sec;

        bool scanned = false;
        if (process_due) {
            // Scan straight into the snapshot; the cache is updated only on success.
            const uint64_t scan_start = qpc_now();
            g.last_process_attempt_qpc = scan_start;
            uint32_t written = 0;
            if (samplers->process.sample(snap.processes, snap.top_processes, written,
                                         g.logical_processors)) {
                assert(written <= TEL_MAX_TOP_PROCESSES);
                const uint64_t scan_end = qpc_now();
                snap.processes.scan_cost_us = static_cast<uint64_t>(qpc_to_us(scan_end - scan_start));

                g.proc_summary = snap.processes;
                std::memcpy(g.top, snap.top_processes, sizeof(TelProcess) * written);
                g.top_count = written;
                g.last_process_scan_qpc = scan_end;
                scanned = true;
            } else {
                // A failed scan leaves its outputs untouched; the cache stands in below.
                process_failed = true;
            }
        }
        if (!scanned) {
            snap.processes = g.proc_summary;
            std::memcpy(snap.top_processes, g.top, sizeof(TelProcess) * g.top_count);
        }
        snap.top_process_count = g.top_count;

        const uint64_t now            = qpc_now();
        const bool     first_of_cycle = g.last_publish_qpc == 0;
        snap.abi_version   = TEL_ABI_VERSION;
        snap.struct_size   = static_cast<uint32_t>(sizeof(TelSnapshot));
        snap.timestamp_qpc = now;
        snap.qpc_frequency = qpc_frequency();
        snap.uptime_ms     = GetTickCount64();
        snap.sample_interval_ms = (!first_of_cycle && now > g.last_publish_qpc)
                                      ? qpc_to_ms(now - g.last_publish_qpc)
                                      : 0.0;
        snap.sample_cost_us = qpc_to_us(now - t_locked);
        snap.processes.scan_age_ms =
            (g.last_process_scan_qpc != 0 && now > g.last_process_scan_qpc)
                ? static_cast<uint64_t>(qpc_to_ms(now - g.last_process_scan_qpc))
                : 0;
        if (first_of_cycle) {
            clear_interval_rates(snap);
        }

        g.last_publish_qpc = now;
        ordinal = ++g.build_ordinal;
    }

    // A process-scan failure is reported after the publish, which still carries the new
    // CPU and memory data.
    const int32_t status = process_failed ? TEL_E_NTSTATUS : TEL_OK;

    ExclusiveLock lock(g.publish_lock);
    if (g.epoch.load(std::memory_order_relaxed) != epoch) {
        set_error("telemetry was shut down while the sample was being taken");
        return TEL_E_NOT_INITIALIZED;
    }
    // Two samplers can finish in either order once sample_lock is released. Publishing an
    // older build over a newer one would move timestamp_qpc backwards, so drop it; the
    // newer snapshot already contains everything this one refreshed.
    if (ordinal <= g.published_ordinal) {
        return status;
    }
    g.published_ordinal = ordinal;
    g.sequence += 1;
    snap.sequence = g.sequence;
    std::memcpy(&g.published, &snap, sizeof(TelSnapshot));
    g.has_sample = true;
    return status;
}

DWORD WINAPI sampler_thread(LPVOID param) {
    ThreadContext* const context = static_cast<ThreadContext*>(param);
    // Index 0 wins when both are signalled, so a pending stop beats a pending tick.
    const HANDLE  handles[2] = {context->stop_event, context->timer};
    const HMODULE module     = context->module;
    delete context;

    for (;;) {
        const DWORD wait = WaitForMultipleObjects(2, handles, FALSE, INFINITE);
        if (wait != WAIT_OBJECT_0 + 1) {
            // Stop requested, or WAIT_FAILED.
            break;
        }
        // tel_set_intervals keeps the tick phase relative to this time.
        g.last_tick_qpc.store(qpc_now(), std::memory_order_relaxed);
        // A failed sample keeps the previous snapshot until the next tick. The timer is
        // auto-reset, so ticks missed during a long sample coalesce into one signal and
        // the loop cannot spin.
        try {
            static_cast<void>(run_sample(false));
        } catch (...) {
            set_error("sampler thread: unexpected C++ exception");
        }
    }

    // The thread's own duplicates; shutdown closes the module's copies.
    CloseHandle(handles[1]);
    CloseHandle(handles[0]);
    FreeLibraryAndExitThread(module, 0);
}

// Arms `timer` to expire first at `due_100ns` (negative: relative, in 100 ns units) and
// then every `period_ms`.
bool arm_timer(HANDLE timer, LONGLONG due_100ns, uint32_t period_ms) {
    LARGE_INTEGER due{};
    due.QuadPart = due_100ns;
    const LONG period =
        period_ms > static_cast<uint32_t>(LONG_MAX) ? LONG_MAX : static_cast<LONG>(period_ms);
    return SetWaitableTimer(timer, &due, period, nullptr, nullptr, FALSE) != FALSE;
}

// Relative due time, in 100 ns units, of the first tick at a new period: one new period
// after the previous tick, or at once (-1, the shortest relative time) when that moment
// has already passed.
LONGLONG next_tick_due_100ns(uint64_t last_tick_qpc, uint32_t period_ms) {
    const uint64_t frequency = qpc_frequency();
    if (frequency == 0) {
        return -static_cast<LONGLONG>(period_ms) * 10000LL;
    }
    const uint64_t now     = qpc_now();
    const uint64_t elapsed = now > last_tick_qpc ? now - last_tick_qpc : 0;
    const uint64_t period  = ms_to_qpc(period_ms);
    if (elapsed >= period) {
        return -1;
    }
    // Split like ms_to_qpc so the product cannot overflow.
    const uint64_t remaining = period - elapsed;
    const uint64_t units     = (remaining / frequency) * 10000000u +
                               (remaining % frequency) * 10000000u / frequency;
    return units == 0 ? -1 : -static_cast<LONGLONG>(units);
}

// ---------------------------------------------------------------------------------------
// Lifecycle. Every function below runs with init_lock held.
// ---------------------------------------------------------------------------------------

// Closes orphan_thread once that thread can no longer hold sample_lock: when it has
// exited, when it is the calling thread (DLL_PROCESS_DETACH run by its own
// FreeLibraryAndExitThread, after it has left its loop), or when `unloading` is set.
// `unloading` means DLL_PROCESS_DETACH from FreeLibrary with the module reference count at
// zero: the orphan holds a reference until FreeLibraryAndExitThread, which it calls only
// after leaving its loop, so it can no longer hold sample_lock or run code in this image,
// even while it is still waiting on the loader lock to finish exiting.
void reap_orphan_locked(bool unloading) {
    if (g.orphan_thread == nullptr) {
        return;
    }
    if (unloading || WaitForSingleObject(g.orphan_thread, 0) == WAIT_OBJECT_0 ||
        GetThreadId(g.orphan_thread) == GetCurrentThreadId()) {
        CloseHandle(g.orphan_thread);
        g.orphan_thread = nullptr;
    }
}

// Stops the sampler thread if one exists, releases every object of the current cycle and
// clears the published state. When the thread ignores the stop request for
// kThreadStopTimeoutMs it is presumed blocked inside a system call, possibly holding
// sample_lock. The timer is still cancelled and the module's handles are still closed:
// the thread waits on its own handles to both objects, which keep them alive until it
// exits, and the stop event stays signalled, so it exits as soon as it returns to its
// wait. Its handle moves to orphan_thread, and start_locked waits for it to exit before
// it starts a new cycle.
//
// Idempotent: with no cycle running it only reaps a finished orphan and frees a sampler
// set that an earlier timed-out teardown could not lock. `unloading` is passed through to
// reap_orphan_locked.
void teardown_locked(bool unloading = false) {
    g.running.store(false, std::memory_order_release);
    reap_orphan_locked(unloading);

    bool thread_stopped = true;
    if (g.thread != nullptr) {
        if (g.stop_event != nullptr) {
            SetEvent(g.stop_event);
        }
        // DLL_PROCESS_DETACH can run on the sampler thread itself, when its
        // FreeLibraryAndExitThread drops the last reference; it has left its loop by then.
        if (GetThreadId(g.thread) != GetCurrentThreadId()) {
            const DWORD wait = WaitForSingleObject(g.thread, kThreadStopTimeoutMs);
            if (wait == WAIT_TIMEOUT) {
                thread_stopped = false;
                set_error("tel_shutdown: sampler thread did not stop within %lu ms and is "
                          "presumed blocked in a system call; tel_init cannot start a new "
                          "cycle until it exits",
                          static_cast<unsigned long>(kThreadStopTimeoutMs));
                // start_locked creates no thread while an orphan is kept, so at most one
                // exists.
                assert(g.orphan_thread == nullptr);
                g.orphan_thread = g.thread;
                g.thread = nullptr;
            } else if (wait == WAIT_FAILED) {
                // The handle cannot be waited on, so it is of no use for tracking the
                // thread and is closed below.
                thread_stopped = false;
                set_win32_error("tel_shutdown: WaitForSingleObject(tel-sampler)", GetLastError());
            }
        }
        if (g.thread != nullptr) {
            CloseHandle(g.thread);
            g.thread = nullptr;
        }
    }

    // Cancelled whether or not the thread stopped, so a thread that outlives the timeout
    // leaves no periodic timer expiring behind it.
    if (g.timer != nullptr) {
        CancelWaitableTimer(g.timer);
        CloseHandle(g.timer);
        g.timer = nullptr;
    }
    if (g.stop_event != nullptr) {
        CloseHandle(g.stop_event);
        g.stop_event = nullptr;
    }

    // A thread that did not stop, or one kept in orphan_thread from an earlier shutdown,
    // may own sample_lock indefinitely, so the lock is only tried then. Otherwise a
    // blocking acquire waits at most for an in-flight tel_sample_now. When the try fails
    // the samplers stay in g.samplers, which is reached only under the lock, and the next
    // start_locked or teardown that takes the lock frees them.
    const bool lock_may_be_stuck = !thread_stopped || g.orphan_thread != nullptr;
    bool locked = false;
    if (lock_may_be_stuck) {
        locked = TryAcquireSRWLockExclusive(&g.sample_lock) != FALSE;
    } else {
        AcquireSRWLockExclusive(&g.sample_lock);
        locked = true;
    }
    Samplers* doomed = nullptr;
    if (locked) {
        doomed = g.samplers;
        g.samplers = nullptr;
        ReleaseSRWLockExclusive(&g.sample_lock);
    }
    if (doomed != nullptr) {
        doomed->shutdown_all();
        delete doomed;
    }
    g.core_count.store(0, std::memory_order_relaxed);

    {
        ExclusiveLock lock(g.publish_lock);
        g.epoch.fetch_add(1, std::memory_order_relaxed);
        std::memset(&g.published, 0, sizeof g.published);
        g.has_sample = false;
        g.sequence = 0;
    }
}

// Releases a context whose thread was never started.
void free_thread_context(ThreadContext* context) {
    if (context->timer != nullptr) {
        CloseHandle(context->timer);
    }
    if (context->stop_event != nullptr) {
        CloseHandle(context->stop_event);
    }
    if (context->module != nullptr) {
        FreeLibrary(context->module);
    }
    delete context;
}

// Gives a new sampler thread its own handles to the stop event and the timer and its own
// reference on this image. Returns nullptr with the error text set on failure.
ThreadContext* make_thread_context() {
    ThreadContext* const context = new (std::nothrow) ThreadContext{};
    if (context == nullptr) {
        set_error("tel_init: out of memory allocating the thread context");
        return nullptr;
    }
    const HANDLE process = GetCurrentProcess();
    HANDLE stop_event = nullptr;
    if (!DuplicateHandle(process, g.stop_event, process, &stop_event, 0, FALSE,
                         DUPLICATE_SAME_ACCESS)) {
        set_win32_error("tel_init: DuplicateHandle(stop event)", GetLastError());
        free_thread_context(context);
        return nullptr;
    }
    context->stop_event = stop_event;
    HANDLE timer = nullptr;
    if (!DuplicateHandle(process, g.timer, process, &timer, 0, FALSE, DUPLICATE_SAME_ACCESS)) {
        set_win32_error("tel_init: DuplicateHandle(timer)", GetLastError());
        free_thread_context(context);
        return nullptr;
    }
    context->timer = timer;
    HMODULE self = nullptr;
    if (!GetModuleHandleExW(GET_MODULE_HANDLE_EX_FLAG_FROM_ADDRESS,
                            reinterpret_cast<LPCWSTR>(&g), &self)) {
        set_win32_error("tel_init: GetModuleHandleExW", GetLastError());
        free_thread_context(context);
        return nullptr;
    }
    context->module = self;
    return context;
}

// Waits for a sampler thread kept in orphan_thread to exit, then brings up the samplers,
// the first snapshot, the timer and the sampler thread. On failure it returns the status
// with the error text set; the caller tears down whatever was stored in `g`. The thread
// is created last, so a failure never leaves one running.
//
// Status codes: failures to bring up or sample the samplers, allocations included, are
// TEL_E_NTSTATUS; failures of ntdll resolution, of the event, the timer or the thread,
// and a previous sampler thread that is still blocked or cannot be waited on are
// TEL_E_WIN32.
int32_t start_locked(const TelConfig& cfg) {
    // A thread left over from a shutdown that timed out may still hold sample_lock, which
    // is acquired below with init_lock held, so it gets one more stop timeout to exit.
    reap_orphan_locked(false);
    if (g.orphan_thread != nullptr) {
        const DWORD wait = WaitForSingleObject(g.orphan_thread, kThreadStopTimeoutMs);
        if (wait == WAIT_TIMEOUT) {
            set_error("tel_init: the previous sampler thread is still blocked in a system call");
            return TEL_E_WIN32;
        }
        const bool failed = wait == WAIT_FAILED;
        if (failed) {
            set_win32_error("tel_init: WaitForSingleObject(previous tel-sampler)", GetLastError());
        }
        // Exited, or a handle that cannot be waited on and so is of no use for tracking
        // the thread.
        CloseHandle(g.orphan_thread);
        g.orphan_thread = nullptr;
        if (failed) {
            return TEL_E_WIN32;
        }
    }

    if (!resolve_nt()) {
        return TEL_E_WIN32;
    }

    // The sampler set is initialised completely before it is published under sample_lock,
    // so a tel_sample_now that takes the lock later never sees one half built. Until then
    // it is owned here and freed on failure.
    std::unique_ptr<Samplers> samplers(new (std::nothrow) Samplers);
    if (!samplers) {
        set_error("tel_init: out of memory allocating the samplers");
        return TEL_E_NTSTATUS;
    }
    // Process first: its buffers are the largest allocations of a cycle, and the CPU and
    // memory baselines taken afterwards keep those page faults out of their first interval.
    if (!samplers->process.init(cfg.top_process_count)) {
        return TEL_E_NTSTATUS;
    }
    samplers->process_ready = true;
    if (!samplers->cpu.init()) {
        return TEL_E_NTSTATUS;
    }
    samplers->cpu_ready = true;
    if (!samplers->memory.init()) {
        return TEL_E_NTSTATUS;
    }
    samplers->memory_ready = true;

    // Processes are charged time on every logical processor, including any beyond the
    // TEL_MAX_CORES published in TelCpu, so their CPU share is scaled by the full count.
    uint32_t logical = static_cast<uint32_t>(GetActiveProcessorCount(ALL_PROCESSOR_GROUPS));
    if (logical == 0) {
        logical = samplers->cpu.core_count();
    }
    // Stored before the publish, whose lock release orders them for every later sampler.
    g.core_count.store(samplers->cpu.core_count(), std::memory_order_relaxed);
    g.process_interval_ms.store(cfg.process_interval_ms, std::memory_order_relaxed);

    Samplers* stale = nullptr;
    {
        ExclusiveLock lock(g.sample_lock);
        // A set left in g.samplers by a teardown that could not take sample_lock is out
        // of use once this lock is held, because samplers are only reached through
        // g.samplers under the lock.
        stale = g.samplers;
        g.samplers = samplers.release();
        g.logical_processors = logical;
        std::memset(&g.proc_summary, 0, sizeof g.proc_summary);
        std::memset(g.top, 0, sizeof g.top);
        g.top_count = 0;
        g.last_process_scan_qpc = 0;
        g.last_process_attempt_qpc = 0;
        g.last_publish_qpc = 0;
    }
    if (stale != nullptr) {
        stale->shutdown_all();
        delete stale;
    }

    // One synchronous sample so tel_snapshot has data as soon as tel_init returns.
    const int32_t first = run_sample(true);
    if (first != TEL_OK) {
        return first;
    }

    g.stop_event = CreateEventW(nullptr, TRUE, FALSE, nullptr);
    if (g.stop_event == nullptr) {
        set_win32_error("tel_init: CreateEventW", GetLastError());
        return TEL_E_WIN32;
    }

    // A high-resolution timer holds a 16 ms period without raising the system-wide
    // timer resolution.
    g.timer = CreateWaitableTimerExW(nullptr, nullptr, CREATE_WAITABLE_TIMER_HIGH_RESOLUTION,
                                     TIMER_ALL_ACCESS);
    if (g.timer == nullptr) {
        g.timer = CreateWaitableTimerExW(nullptr, nullptr, 0, TIMER_ALL_ACCESS);
    }
    if (g.timer == nullptr) {
        set_win32_error("tel_init: CreateWaitableTimerExW", GetLastError());
        return TEL_E_WIN32;
    }
    const uint64_t armed_at = qpc_now();
    if (!arm_timer(g.timer, -static_cast<LONGLONG>(cfg.sample_interval_ms) * 10000LL,
                   cfg.sample_interval_ms)) {
        set_win32_error("tel_init: SetWaitableTimer", GetLastError());
        return TEL_E_WIN32;
    }
    g.timer_period_ms = cfg.sample_interval_ms;
    g.last_tick_qpc.store(armed_at, std::memory_order_relaxed);

    ThreadContext* const context = make_thread_context();
    if (context == nullptr) {
        return TEL_E_WIN32;
    }
    g.thread = CreateThread(nullptr, 0, sampler_thread, context, 0, nullptr);
    if (g.thread == nullptr) {
        const DWORD error = GetLastError();
        free_thread_context(context);
        set_win32_error("tel_init: CreateThread", error);
        return TEL_E_WIN32;
    }
    // Cosmetic and best-effort respectively; neither failure affects sampling.
    static_cast<void>(SetThreadDescription(g.thread, L"tel-sampler"));
    static_cast<void>(SetThreadPriority(g.thread, THREAD_PRIORITY_ABOVE_NORMAL));
    return TEL_OK;
}

int32_t init_locked(const TelConfig* config) {
    if (g.running.load(std::memory_order_acquire)) {
        set_error("tel_init: already initialised");
        return TEL_E_ALREADY_INITIALIZED;
    }

    TelConfig cfg{};
    tel_default_config(&cfg);
    if (config != nullptr) {
        if (config->flags != 0) {
            set_error("tel_init: TelConfig.flags must be 0 (got 0x%08X)", config->flags);
            return TEL_E_INVALID_ARGUMENT;
        }
        cfg.sample_interval_ms  = clamp_interval(config->sample_interval_ms);
        cfg.process_interval_ms = clamp_interval(config->process_interval_ms);
        cfg.top_process_count   = config->top_process_count;
        if (cfg.top_process_count < 1) {
            cfg.top_process_count = 1;
        } else if (cfg.top_process_count > TEL_MAX_TOP_PROCESSES) {
            cfg.top_process_count = TEL_MAX_TOP_PROCESSES;
        }
        cfg.flags = 0;
    }

    const int32_t rc = start_locked(cfg);
    if (rc != TEL_OK) {
        // A failed start never leaves g.thread set, so teardown has no thread to stop and
        // keeps the error text of the start.
        teardown_locked();
        return rc;
    }
    g.running.store(true, std::memory_order_release);
    return TEL_OK;
}

int32_t set_intervals_locked(uint32_t sample_interval_ms, uint32_t process_interval_ms) {
    sample_interval_ms  = clamp_interval(sample_interval_ms);
    process_interval_ms = clamp_interval(process_interval_ms);
    if (!g.running.load(std::memory_order_acquire)) {
        set_error("tel_set_intervals: not initialised");
        return TEL_E_NOT_INITIALIZED;
    }
    // run_sample reads the process cadence on every tick, so it needs no timer change.
    g.process_interval_ms.store(process_interval_ms, std::memory_order_relaxed);

    // Re-arming restarts the countdown and drops a pending tick, so an unchanged period
    // leaves the timer alone; repeated calls then cannot hold off sampling.
    if (sample_interval_ms == g.timer_period_ms) {
        return TEL_OK;
    }
    // A new period keeps the tick phase: the next tick comes one new period after the
    // previous one, or at once when that moment has passed. SetWaitableTimer on a timer
    // another thread is waiting on is supported; the pending wait adopts the new due
    // time and period.
    const LONGLONG due =
        next_tick_due_100ns(g.last_tick_qpc.load(std::memory_order_relaxed), sample_interval_ms);
    if (!arm_timer(g.timer, due, sample_interval_ms)) {
        set_win32_error("tel_set_intervals: SetWaitableTimer", GetLastError());
        return TEL_E_WIN32;
    }
    // Recorded only once the timer holds the new period, so a retry after a failure
    // re-arms instead of taking the early return above.
    g.timer_period_ms = sample_interval_ms;
    return TEL_OK;
}

}  // namespace
}  // namespace tel

// ---------------------------------------------------------------------------------------
// Exported C ABI. No C++ exception may leave any function below.
// ---------------------------------------------------------------------------------------

extern "C" {

TEL_API uint32_t TEL_CALL tel_abi_version(void) { return TEL_ABI_VERSION; }

TEL_API uint32_t TEL_CALL tel_snapshot_size(void) {
    return static_cast<uint32_t>(sizeof(TelSnapshot));
}

TEL_API const char* TEL_CALL tel_version_string(void) { return tel::kVersionString; }

TEL_API void TEL_CALL tel_default_config(TelConfig* out) {
    if (out == nullptr) {
        return;
    }
    out->sample_interval_ms  = tel::kDefaultSampleIntervalMs;
    out->process_interval_ms = tel::kDefaultProcessIntervalMs;
    out->top_process_count   = tel::kDefaultTopProcessCount;
    out->flags               = 0;
}

TEL_API int32_t TEL_CALL tel_init(const TelConfig* config) {
    tel::ExclusiveLock lock(tel::g.init_lock);
    try {
        return tel::init_locked(config);
    } catch (...) {
        // Every allocation in this file is nothrow and Win32 calls do not throw, so the
        // exception came from sampler code.
        try {
            tel::teardown_locked();
        } catch (...) {
        }
        tel::set_error("tel_init: unexpected C++ exception from a sampler");
        return TEL_E_NTSTATUS;
    }
}

namespace tel {
namespace {

// Shared body of tel_shutdown and the FreeLibrary detach path. teardown_locked is
// idempotent, so it also runs when no cycle is active: that releases an orphan that has
// exited and any sampler set a timed-out teardown left behind.
void shutdown_all_state(bool unloading) {
    ExclusiveLock lock(g.init_lock);
    try {
        teardown_locked(unloading);
    } catch (...) {
        set_error("tel_shutdown: unexpected C++ exception");
    }
}

}  // namespace
}  // namespace tel

TEL_API void TEL_CALL tel_shutdown(void) { tel::shutdown_all_state(false); }

TEL_API int32_t TEL_CALL tel_is_running(void) {
    return tel::g.running.load(std::memory_order_acquire) ? 1 : 0;
}

TEL_API int32_t TEL_CALL tel_snapshot(TelSnapshot* out) {
    try {
        if (out == nullptr) {
            tel::set_error("tel_snapshot: out is null");
            return TEL_E_INVALID_ARGUMENT;
        }
        if (!tel::g.running.load(std::memory_order_acquire)) {
            tel::set_error("tel_snapshot: not initialised");
            return TEL_E_NOT_INITIALIZED;
        }
        tel::SharedLock lock(tel::g.publish_lock);
        if (!tel::g.has_sample) {
            tel::set_error("tel_snapshot: no sample has been published yet");
            return TEL_E_NO_SAMPLE;
        }
        std::memcpy(out, &tel::g.published, sizeof(TelSnapshot));
        return TEL_OK;
    } catch (...) {
        tel::set_error("tel_snapshot: unexpected C++ exception");
        return TEL_E_WIN32;
    }
}

TEL_API int32_t TEL_CALL tel_sample_now(void) {
    try {
        if (!tel::g.running.load(std::memory_order_acquire)) {
            tel::set_error("tel_sample_now: not initialised");
            return TEL_E_NOT_INITIALIZED;
        }
        return tel::run_sample(true);
    } catch (...) {
        // Only the sampler calls inside run_sample can throw.
        tel::set_error("tel_sample_now: unexpected C++ exception from a sampler");
        return TEL_E_NTSTATUS;
    }
}

TEL_API int32_t TEL_CALL tel_set_intervals(uint32_t sample_interval_ms,
                                           uint32_t process_interval_ms) {
    tel::ExclusiveLock lock(tel::g.init_lock);
    try {
        return tel::set_intervals_locked(sample_interval_ms, process_interval_ms);
    } catch (...) {
        tel::set_error("tel_set_intervals: unexpected C++ exception");
        return TEL_E_WIN32;
    }
}

TEL_API const char* TEL_CALL tel_last_error(void) { return tel::t_error; }

TEL_API uint32_t TEL_CALL tel_core_count(void) {
    if (tel::g.running.load(std::memory_order_acquire)) {
        const uint32_t count = tel::g.core_count.load(std::memory_order_relaxed);
        if (count != 0) {
            return count;
        }
    }
    return GetActiveProcessorCount(static_cast<WORD>(ALL_PROCESSOR_GROUPS));
}

// C linkage is required: the CRT entry point calls DllMain by its unmangled name and
// substitutes an empty default when none is found.
BOOL APIENTRY DllMain(HMODULE /*module*/, DWORD reason, LPVOID reserved) {
    switch (reason) {
    case DLL_PROCESS_ATTACH:
        // Thread notifications stay enabled. DisableThreadLibraryCalls does not take
        // effect on an image with static TLS (t_error), and Microsoft advises against it
        // for a DLL linked with the static CRT, as this one is. t_error is
        // constant-initialised implicit TLS, which the loader sets up for every thread
        // without a notification.
        break;
    case DLL_PROCESS_DETACH:
        // reserved == nullptr: FreeLibrary in a live process with the reference count at
        // zero. A sampler thread holds a reference until FreeLibraryAndExitThread, which
        // it calls only after leaving its loop, so any sampler thread that still exists
        // is either the caller itself or one that is waiting on the loader lock to finish
        // exiting; neither can run code in this image again. Teardown therefore releases
        // every object, including a kept orphan handle.
        // reserved != nullptr: process exit. Every other thread is already gone, possibly
        // mid-sample with locks held, so touching shared state could deadlock.
        if (reserved == nullptr) {
            tel::shutdown_all_state(true);
        }
        break;
    default:
        break;
    }
    return TRUE;
}

}  // extern "C"
