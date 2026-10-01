// Internal contracts shared by the translation units of optimizer_telemetry.
//
// Ownership:
//   telemetry.cpp  g_nt, error text, clock helpers, lifecycle, sampler thread, exports
//   cpu.cpp        CpuSampler
//   memory.cpp     MemorySampler
//   process.cpp    ProcessSampler
//
// Rules for every sampler:
//   - No exceptions may escape a public method; report failure with a false return and
//     set_error()/set_nt_error().
//   - The hot path (sample) must not allocate except to grow a reusable buffer.
//   - A false return leaves the output struct untouched.
//   - CpuSampler::init() and MemorySampler::init() take a baseline reading, so the first
//     sample() reports values over the interval since init() rather than totals since
//     boot. ProcessSampler takes none; see its sample() contract.
#pragma once

#include <windows.h>

#include <cstddef>
#include <cstdint>
#include <memory>

#include "telemetry/ntinternal.h"
#include "telemetry/telemetry.h"

namespace tel {

// Resolved NTDLL entry points. Populated by telemetry.cpp before any sampler runs.
struct NtApi {
    nt::PFN_NtQuerySystemInformation   query    = nullptr;
    nt::PFN_NtQuerySystemInformationEx query_ex = nullptr;  // may be null on old kernels
};
extern NtApi g_nt;

// Thread-local error text (printf-style). Retrieved through tel_last_error().
void set_error(const char* fmt, ...);
void set_nt_error(const char* what, nt::NTSTATUS status);
void set_win32_error(const char* what, DWORD error);

// Monotonic clock helpers backed by QueryPerformanceCounter.
uint64_t qpc_now();
uint64_t qpc_frequency();
double   qpc_to_ms(uint64_t ticks);
double   qpc_to_us(uint64_t ticks);

// Shortest interval over which a per-second rate is computed. Shorter intervals (a
// tel_sample_now() microseconds after a timer tick) turn single events into huge rates,
// so the previous rates are republished instead. Kept well under one sample period
// because timer jitter makes normal intervals slightly shorter than the period.
constexpr double kMinRateWindowSec = 0.004;

// Per-second counters derived from SYSTEM_PERFORMANCE_INFORMATION that belong to TelCpu.
struct SystemRates {
    uint64_t context_switches_per_sec = 0;
    uint64_t syscalls_per_sec = 0;
};

class CpuSampler {
public:
    CpuSampler();
    ~CpuSampler();
    CpuSampler(const CpuSampler&) = delete;
    CpuSampler& operator=(const CpuSampler&) = delete;

    // Discovers processor groups and logical processor count, allocates per-core
    // buffers, and takes an initial reading so the next sample() has a baseline.
    bool init();
    void shutdown();

    // Fills `out` with utilisation and interrupt rates over a 250 ms trailing window
    // ending at this call (shorter while fewer readings exist). When `refresh_frequency`
    // is set the per-core effective clock is recomputed from a PDH
    // "% Processor Performance" collection (a few hundred microseconds, so it follows the
    // process-scan cadence); otherwise the last values are written again.
    bool sample(TelCpu& out, bool refresh_frequency);

    uint32_t core_count() const;

private:
    struct Impl;
    std::unique_ptr<Impl> impl_;
};

class MemorySampler {
public:
    MemorySampler();
    ~MemorySampler();
    MemorySampler(const MemorySampler&) = delete;
    MemorySampler& operator=(const MemorySampler&) = delete;

    bool init();
    void shutdown();

    // Fills `out` and the per-second `rates` computed against the previous call. Calls
    // less than kMinRateWindowSec after the previous one republish the previous rates.
    bool sample(TelMemory& out, SystemRates& rates);

private:
    struct Impl;
    std::unique_ptr<Impl> impl_;
};

class ProcessSampler {
public:
    ProcessSampler();
    ~ProcessSampler();
    ProcessSampler(const ProcessSampler&) = delete;
    ProcessSampler& operator=(const ProcessSampler&) = delete;

    // `top_count` is clamped to TEL_MAX_TOP_PROCESSES. Takes no baseline: the first
    // sample() reports cpu_percent 0 for every process, with the top list in working-set
    // order.
    bool init(uint32_t top_count);
    void shutdown();

    // Enumerates SystemProcessInformation once. Fills `summary`, writes up to
    // `top_count` entries sorted by cpu_percent descending into `top`, zeroes the rest of
    // `top`, and stores the number written in `written`. `top` must hold
    // TEL_MAX_TOP_PROCESSES elements. The Idle process (pid 0) is excluded from `top` and
    // from thread counts.
    //
    // cpu_percent is normalised by the larger of `total_cores` and the active logical
    // processor count across all groups recorded at init(). A process the previous
    // measuring scan did not list is charged all of its CPU time; when that scan's walk
    // ended early, only processes created at or after its start qualify. A scan less than
    // about 156 ms (ten clock ticks) after the last measuring scan republishes that
    // scan's percentages, and processes first seen since then report 0.
    //
    // summary.scan_cost_us and summary.scan_age_ms are left zero; the caller fills them.
    bool sample(TelProcessSummary& summary, TelProcess* top, uint32_t& written, uint32_t total_cores);

private:
    struct Impl;
    std::unique_ptr<Impl> impl_;
};

// ABI layout guards. A failure here means telemetry.h and the ctypes mirror diverged.
static_assert(sizeof(TelConfig) == 16);
static_assert(sizeof(TelCpuCore) == 24);
static_assert(sizeof(TelCpu) == 6192);
static_assert(sizeof(TelMemory) == 128);
static_assert(sizeof(TelThreadStates) == 24);
static_assert(sizeof(TelProcessSummary) == 48);
static_assert(sizeof(TelProcess) == 168);
static_assert(sizeof(TelSnapshot) == 11808);
static_assert(offsetof(TelSnapshot, cpu) == 56);
static_assert(offsetof(TelSnapshot, memory) == 6248);
static_assert(offsetof(TelSnapshot, processes) == 6376);
static_assert(offsetof(TelSnapshot, top_processes) == 6432);

}  // namespace tel
