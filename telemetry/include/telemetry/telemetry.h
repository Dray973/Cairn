// optimizer_telemetry: public C ABI consumed by Python via ctypes.
//
// Layout contract: every struct uses natural alignment with explicit padding so a
// ctypes.Structure can mirror it field for field. Consumers must check that
// tel_abi_version() == TEL_ABI_VERSION and tel_snapshot_size() == sizeof(TelSnapshot)
// before reading any snapshot.
//
// Threading contract: tel_init() starts one background sampler thread. tel_snapshot()
// copies the most recent published sample under a shared lock and never issues a
// system call, so it is safe to poll from a UI thread at 60 Hz.
#pragma once

#include <stdint.h>
#include <wchar.h>

#if defined(TEL_EXPORTS)
#  define TEL_API __declspec(dllexport)
#else
#  define TEL_API __declspec(dllimport)
#endif
#define TEL_CALL __cdecl

#ifdef __cplusplus
extern "C" {
#endif

#define TEL_ABI_VERSION        2u
#define TEL_MAX_CORES          256u
#define TEL_MAX_TOP_PROCESSES  32u
#define TEL_PROCESS_NAME_CHARS 64u

enum TelStatus {
    TEL_OK                    = 0,
    TEL_E_NOT_INITIALIZED     = -1,
    TEL_E_ALREADY_INITIALIZED = -2,
    TEL_E_INVALID_ARGUMENT    = -3,
    TEL_E_NTSTATUS            = -4,  // a sampler failed to initialise or sample (NT query,
                                     // Win32 call or allocation); see tel_last_error()
    TEL_E_WIN32               = -5,  // a lifecycle call failed (ntdll resolution, timer,
                                     // thread or event creation, allocation, or a previous
                                     // sampler thread still blocked); see tel_last_error()
    TEL_E_NO_SAMPLE           = -6   // no sample has been published yet
};

typedef struct TelConfig {
    uint32_t sample_interval_ms;   // CPU + memory cadence (default 16)
    uint32_t process_interval_ms;  // process/thread scan cadence (default 500)
    uint32_t top_process_count;    // <= TEL_MAX_TOP_PROCESSES (default 16)
    uint32_t flags;                // reserved, must be 0
} TelConfig;

typedef struct TelCpuCore {
    float    utilization;         // % busy = 100 - idle
    float    kernel;              // % kernel excluding idle (includes DPC + interrupt)
    float    user;                // %
    float    dpc_interrupt;       // % time in DPC + ISR
    uint32_t frequency_mhz;       // effective clock: nominal MHz x "% Processor Performance"
                                  // (as Task Manager shows), refreshed at the process-scan
                                  // cadence; 0 if unknown
    uint32_t interrupts_per_sec;
} TelCpuCore;                     // 24 bytes

typedef struct TelCpu {
    uint32_t core_count;          // logical processors reported (<= TEL_MAX_CORES)
    uint32_t group_count;         // processor groups
    float    total_utilization;   // mean over cores
    float    total_kernel;
    float    total_user;
    float    total_dpc_interrupt;
    uint64_t context_switches_per_sec;
    uint64_t syscalls_per_sec;
    uint64_t interrupts_per_sec;
    TelCpuCore cores[TEL_MAX_CORES];
} TelCpu;                         // 48 + 256 * 24 = 6192 bytes

typedef struct TelMemory {
    uint64_t physical_total_bytes;
    uint64_t physical_available_bytes;
    uint64_t physical_used_bytes;
    uint64_t commit_total_bytes;
    uint64_t commit_limit_bytes;
    uint64_t commit_peak_bytes;
    uint64_t system_cache_bytes;
    uint64_t kernel_paged_pool_bytes;
    uint64_t kernel_nonpaged_pool_bytes;
    uint64_t page_size;
    uint64_t page_faults_per_sec;   // all faults, soft + hard
    uint64_t hard_faults_per_sec;   // pages read from disk to satisfy faults (Pages Input/sec);
                                    // clustered reads make this exceed the fault event count
    uint64_t page_reads_per_sec;    // read I/O operations issued for faults
    uint64_t pages_output_per_sec;  // dirty + mapped pages written to disk
    uint64_t page_writes_per_sec;   // write I/O operations issued
    float    memory_load_percent;
    float    commit_percent;
} TelMemory;                        // 128 bytes

typedef struct TelThreadStates {
    uint32_t total;
    uint32_t running;   // executing on a core
    uint32_t ready;     // runnable and waiting for a core (Ready, DeferredReady, Standby)
    uint32_t waiting;
    uint32_t other;     // initialized, transition, terminated, ...
    uint32_t _pad0;
} TelThreadStates;      // 24 bytes

typedef struct TelProcessSummary {
    uint32_t process_count;
    uint32_t handle_count;
    TelThreadStates threads;
    uint64_t scan_cost_us;  // time spent inside the last process scan
    uint64_t scan_age_ms;   // age of the process data relative to the snapshot timestamp
} TelProcessSummary;        // 48 bytes

typedef struct TelProcess {
    uint32_t pid;
    uint32_t parent_pid;
    uint32_t thread_count;
    uint32_t handle_count;
    uint64_t working_set_bytes;
    uint64_t private_bytes;
    float    cpu_percent;   // share of total machine capacity, 0..100; 0 in the first
                            // scan after tel_init()
    uint32_t _pad0;
    wchar_t  name[TEL_PROCESS_NAME_CHARS];  // image name, NUL-terminated, truncated
} TelProcess;               // 168 bytes

typedef struct TelSnapshot {
    uint32_t abi_version;
    uint32_t struct_size;
    uint64_t sequence;            // increments for every published sample
    uint64_t timestamp_qpc;       // QueryPerformanceCounter at publish time
    uint64_t qpc_frequency;
    uint64_t uptime_ms;
    double   sample_interval_ms;  // measured interval between the last two samples; 0.0
                                  // marks the snapshot published by tel_init(), whose
                                  // interval-based values (CPU percentages, per-second
                                  // rates, process cpu_percent) are 0; instantaneous
                                  // values such as byte counts, memory_load_percent and
                                  // commit_percent are valid
    double   sample_cost_us;      // time spent producing this sample, excluding any wait
                                  // for a concurrent sample
    TelCpu   cpu;
    TelMemory memory;
    TelProcessSummary processes;
    uint32_t top_process_count;
    uint32_t _pad0;
    TelProcess top_processes[TEL_MAX_TOP_PROCESSES];
} TelSnapshot;                    // 11808 bytes

TEL_API uint32_t    TEL_CALL tel_abi_version(void);
TEL_API uint32_t    TEL_CALL tel_snapshot_size(void);
TEL_API const char* TEL_CALL tel_version_string(void);
TEL_API void        TEL_CALL tel_default_config(TelConfig* out);

// Takes one synchronous sample and publishes it, then starts the sampler thread, so
// tel_snapshot() succeeds as soon as this returns TEL_OK. NULL config selects defaults.
// Returns TEL_E_ALREADY_INITIALIZED if already running.
TEL_API int32_t TEL_CALL tel_init(const TelConfig* config);
// Stops the sampler thread and releases every buffer. Safe to call repeatedly.
TEL_API void    TEL_CALL tel_shutdown(void);
TEL_API int32_t TEL_CALL tel_is_running(void);

// Copies the most recent sample into `out`. Never blocks on a system call. Argument
// checks come first: NULL `out` is TEL_E_INVALID_ARGUMENT even before tel_init().
TEL_API int32_t TEL_CALL tel_snapshot(TelSnapshot* out);
// Performs a full synchronous sample (CPU, memory, processes) on the calling thread and
// publishes it. Requires tel_init(); returns TEL_E_NOT_INITIALIZED otherwise. Process
// counts, threads and working sets are always refreshed; per-process cpu_percent is
// re-measured only when about 156 ms have passed since the last measured scan, otherwise
// the previous percentages are republished. If the process scan fails, the new CPU and
// memory data is still published with the cached process data (scan_age_ms keeps
// growing) and TEL_E_NTSTATUS is returned.
TEL_API int32_t TEL_CALL tel_sample_now(void);
// Adjusts cadences while running. Values below 1 ms are clamped to 1 ms. Returns
// TEL_E_NOT_INITIALIZED when not running. An unchanged sample interval leaves the tick
// phase untouched; a changed one schedules the next tick one new period after the
// previous tick, or immediately if that time has passed.
TEL_API int32_t TEL_CALL tel_set_intervals(uint32_t sample_interval_ms, uint32_t process_interval_ms);

// Thread-local, NUL-terminated UTF-8 description of the last failure on the calling
// thread. Never NULL; empty when nothing has failed.
TEL_API const char* TEL_CALL tel_last_error(void);
// Logical processors covered by snapshots while running; the machine's active logical
// processor count before tel_init().
TEL_API uint32_t    TEL_CALL tel_core_count(void);

#ifdef __cplusplus
}
#endif
