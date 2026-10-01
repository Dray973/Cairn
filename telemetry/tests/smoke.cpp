// Smoke test for optimizer_telemetry.dll, driven purely through the public C ABI.
// Run: build\cpp\bin\Release\telemetry_smoke.exe
//
// Covers layout guards, error paths before init, lifecycle, snapshot plausibility,
// sequence progression, tel_snapshot latency, cadence changes, per-cycle memory growth
// and concurrent readers. Every check prints PASS/FAIL with the measured value so a
// human can read the log; the exit code is the number of failed checks.
//
// Tolerances are loose on purpose: they catch a broken implementation rather than
// scheduler jitter, so the test does not flake on a moderately loaded desktop.

#include <windows.h>
#include <psapi.h>

#include <algorithm>
#include <atomic>
#include <cstdarg>
#include <cstdint>
#include <cstdio>
#include <cstdlib>
#include <cstring>
#include <cwchar>
#include <exception>
#include <thread>
#include <vector>

#include "telemetry/telemetry.h"

namespace {

// Consumer-side layout guard. The ctypes mirror on the Python side hard-codes these.
static_assert(sizeof(TelSnapshot) == 11808, "TelSnapshot layout changed; update the ctypes mirror");
static_assert(sizeof(TelProcess) == 168, "TelProcess layout changed; update the ctypes mirror");

int g_failures = 0;

bool check(bool ok, const char* what) {
    std::printf("%s  %s\n", ok ? "PASS" : "FAIL", what);
    if (!ok) {
        ++g_failures;
    }
    return ok;
}

// check() with a printf-formatted measured value appended to the same line.
bool checkv(bool ok, const char* what, const char* fmt, ...) {
    char detail[512];
    va_list args;
    va_start(args, fmt);
    const int n = std::vsnprintf(detail, sizeof(detail), fmt, args);
    va_end(args);
    if (n < 0) {
        detail[0] = '\0';
    }
    char line[640];
    std::snprintf(line, sizeof(line), "%-58s %s", what, detail);
    return check(ok, line);
}

void section(const char* title) {
    std::printf("\n--- %s ---\n", title);
}

unsigned long long ull(uint64_t v) {
    return static_cast<unsigned long long>(v);
}

double mib(uint64_t bytes) {
    return static_cast<double>(bytes) / (1024.0 * 1024.0);
}

bool in_range(float v, float lo, float hi) {
    // A NaN fails both comparisons, which is the desired outcome.
    return v >= lo && v <= hi;
}

const char* status_name(int32_t rc) {
    switch (rc) {
        case TEL_OK:                    return "TEL_OK";
        case TEL_E_NOT_INITIALIZED:     return "TEL_E_NOT_INITIALIZED";
        case TEL_E_ALREADY_INITIALIZED: return "TEL_E_ALREADY_INITIALIZED";
        case TEL_E_INVALID_ARGUMENT:    return "TEL_E_INVALID_ARGUMENT";
        case TEL_E_NTSTATUS:            return "TEL_E_NTSTATUS";
        case TEL_E_WIN32:               return "TEL_E_WIN32";
        case TEL_E_NO_SAMPLE:           return "TEL_E_NO_SAMPLE";
        default:                        return "<unknown status>";
    }
}

const char* last_error_text() {
    const char* text = tel_last_error();
    return text != nullptr ? text : "(null)";
}

uint64_t qpc_now() {
    LARGE_INTEGER li;
    QueryPerformanceCounter(&li);
    return static_cast<uint64_t>(li.QuadPart);
}

uint64_t qpc_frequency() {
    LARGE_INTEGER li;
    QueryPerformanceFrequency(&li);
    return static_cast<uint64_t>(li.QuadPart);
}

uint64_t private_usage_bytes() {
    PROCESS_MEMORY_COUNTERS_EX pmc{};
    pmc.cb = sizeof(pmc);
    if (!GetProcessMemoryInfo(GetCurrentProcess(),
                              reinterpret_cast<PROCESS_MEMORY_COUNTERS*>(&pmc),
                              sizeof(pmc))) {
        return 0;
    }
    return static_cast<uint64_t>(pmc.PrivateUsage);
}

// UTF-8 copy of a process name so it prints correctly regardless of the C locale.
void narrow_name(const wchar_t* name, char* out, int out_chars) {
    wchar_t bounded[TEL_PROCESS_NAME_CHARS];
    for (uint32_t i = 0; i < TEL_PROCESS_NAME_CHARS; ++i) {
        bounded[i] = name[i];
    }
    bounded[TEL_PROCESS_NAME_CHARS - 1] = L'\0';
    const int written = WideCharToMultiByte(CP_UTF8, 0, bounded, -1, out, out_chars, nullptr, nullptr);
    if (written <= 0 && out_chars > 0) {
        out[0] = '\0';
    }
}

// ---------------------------------------------------------------------------------------

void test_abi() {
    section("1. ABI");
    // tel_last_error() is read first so the check covers the state before any other call.
    const char* error = tel_last_error();
    const char* version = tel_version_string();
    checkv(tel_abi_version() == TEL_ABI_VERSION, "tel_abi_version() == TEL_ABI_VERSION",
           "%u (expected %u)", tel_abi_version(), TEL_ABI_VERSION);
    checkv(tel_snapshot_size() == static_cast<uint32_t>(sizeof(TelSnapshot)),
           "tel_snapshot_size() == sizeof(TelSnapshot)",
           "%u (expected %zu)", tel_snapshot_size(), sizeof(TelSnapshot));
    checkv(version != nullptr, "tel_version_string() non-null", "\"%s\"",
           version != nullptr ? version : "(null)");
    checkv(error != nullptr, "tel_last_error() non-null before any call", "\"%s\"",
           error != nullptr ? error : "(null)");
}

void test_uninitialized() {
    section("2. Error paths before init");
    TelSnapshot s{};
    int32_t rc = tel_snapshot(&s);
    checkv(rc == TEL_E_NOT_INITIALIZED, "tel_snapshot(&s) before init", "rc=%s", status_name(rc));
    rc = tel_snapshot(nullptr);
    checkv(rc == TEL_E_INVALID_ARGUMENT, "tel_snapshot(nullptr)", "rc=%s", status_name(rc));
    rc = tel_sample_now();
    checkv(rc == TEL_E_NOT_INITIALIZED, "tel_sample_now() before init", "rc=%s", status_name(rc));
    checkv(tel_is_running() == 0, "tel_is_running() == 0 before init", "%d", tel_is_running());
    tel_shutdown();
    checkv(tel_is_running() == 0, "tel_shutdown() before init is harmless", "%d", tel_is_running());
}

// Returns false when the sampler could not be started; later sections need it running.
bool test_init() {
    section("3. Configuration and init");
    TelConfig cfg{};
    tel_default_config(&cfg);
    checkv(cfg.sample_interval_ms == 16u && cfg.process_interval_ms == 500u &&
               cfg.top_process_count == 16u && cfg.flags == 0u,
           "tel_default_config() == {16, 500, 16, 0}", "{%u, %u, %u, %u}",
           cfg.sample_interval_ms, cfg.process_interval_ms, cfg.top_process_count, cfg.flags);

    int32_t rc = tel_init(nullptr);
    const bool started = checkv(rc == TEL_OK, "tel_init(nullptr)", "rc=%s err=\"%s\"",
                                status_name(rc), last_error_text());
    rc = tel_init(&cfg);
    checkv(rc == TEL_E_ALREADY_INITIALIZED, "second tel_init()", "rc=%s", status_name(rc));
    checkv(tel_is_running() == 1, "tel_is_running() == 1", "%d", tel_is_running());
    checkv(tel_core_count() > 0u, "tel_core_count() > 0", "%u", tel_core_count());
    return started;
}

void test_first_snapshot() {
    section("4. Snapshot immediately after init");
    TelSnapshot s{};
    const int32_t rc = tel_snapshot(&s);
    checkv(rc == TEL_OK, "tel_snapshot() right after init", "rc=%s err=\"%s\"",
           status_name(rc), last_error_text());
    checkv(s.abi_version == TEL_ABI_VERSION, "snapshot.abi_version", "%u", s.abi_version);
    checkv(s.struct_size == static_cast<uint32_t>(sizeof(TelSnapshot)), "snapshot.struct_size",
           "%u", s.struct_size);
    std::printf("      sequence=%llu sample_cost_us=%.1f\n", ull(s.sequence), s.sample_cost_us);
}

void test_plausibility() {
    section("5. Snapshot plausibility after 700 ms");
    Sleep(700);
    TelSnapshot s{};
    const int32_t rc = tel_snapshot(&s);
    if (!checkv(rc == TEL_OK, "tel_snapshot() after warm-up", "rc=%s err=\"%s\"",
                status_name(rc), last_error_text())) {
        return;
    }

    // CPU
    const DWORD active = GetActiveProcessorCount(ALL_PROCESSOR_GROUPS);
    checkv(s.cpu.core_count >= 1u && s.cpu.core_count <= TEL_MAX_CORES, "cpu.core_count in 1..256",
           "%u (groups %u)", s.cpu.core_count, s.cpu.group_count);
    checkv(s.cpu.core_count == active, "cpu.core_count == GetActiveProcessorCount(ALL_PROCESSOR_GROUPS)",
           "%u vs %lu", s.cpu.core_count, active);

    const uint32_t cores = std::min<uint32_t>(s.cpu.core_count, TEL_MAX_CORES);
    uint32_t out_of_range = 0;
    uint32_t over_sum = 0;
    for (uint32_t i = 0; i < cores; ++i) {
        const TelCpuCore& c = s.cpu.cores[i];
        const bool ok = in_range(c.utilization, 0.0f, 100.0f) && in_range(c.kernel, 0.0f, 100.0f) &&
                        in_range(c.user, 0.0f, 100.0f) && in_range(c.dpc_interrupt, 0.0f, 100.0f);
        if (!ok) {
            ++out_of_range;
        }
        if (!(c.kernel + c.user <= 100.5f)) {
            ++over_sum;
        }
    }
    checkv(out_of_range == 0, "every core: four percentages in 0..100", "%u of %u cores out of range",
           out_of_range, cores);
    checkv(over_sum == 0, "every core: kernel + user <= 100.5", "%u of %u cores over", over_sum, cores);
    if (cores > 0) {
        const TelCpuCore& c0 = s.cpu.cores[0];
        std::printf("      core0: util=%.1f%% kernel=%.1f%% user=%.1f%% dpc=%.1f%% freq=%u MHz ints/s=%u\n",
                    c0.utilization, c0.kernel, c0.user, c0.dpc_interrupt, c0.frequency_mhz,
                    c0.interrupts_per_sec);
    }
    checkv(in_range(s.cpu.total_utilization, 0.0f, 100.0f), "cpu.total_utilization in 0..100",
           "%.2f%% (kernel %.2f%%, user %.2f%%, dpc %.2f%%)", s.cpu.total_utilization,
           s.cpu.total_kernel, s.cpu.total_user, s.cpu.total_dpc_interrupt);
    std::printf("      ctx/s=%llu syscalls/s=%llu ints/s=%llu\n", ull(s.cpu.context_switches_per_sec),
                ull(s.cpu.syscalls_per_sec), ull(s.cpu.interrupts_per_sec));

    // Memory
    const TelMemory& m = s.memory;
    checkv(m.physical_total_bytes > (1ull << 30), "memory.physical_total_bytes > 1 GiB", "%.1f MiB",
           mib(m.physical_total_bytes));
    checkv(m.physical_used_bytes <= m.physical_total_bytes, "memory.physical_used <= physical_total",
           "%.1f MiB used, %.1f MiB available", mib(m.physical_used_bytes),
           mib(m.physical_available_bytes));
    checkv(m.commit_limit_bytes >= m.commit_total_bytes, "memory.commit_limit >= commit_total",
           "%.1f / %.1f MiB (peak %.1f MiB)", mib(m.commit_total_bytes), mib(m.commit_limit_bytes),
           mib(m.commit_peak_bytes));
    checkv(m.page_size == 4096u, "memory.page_size == 4096", "%llu", ull(m.page_size));
    std::printf("      load=%.1f%% commit=%.1f%% cache=%.1f MiB paged=%.1f MiB nonpaged=%.1f MiB\n",
                m.memory_load_percent, m.commit_percent, mib(m.system_cache_bytes),
                mib(m.kernel_paged_pool_bytes), mib(m.kernel_nonpaged_pool_bytes));
    std::printf("      faults/s=%llu hard/s=%llu reads/s=%llu out/s=%llu writes/s=%llu\n",
                ull(m.page_faults_per_sec), ull(m.hard_faults_per_sec), ull(m.page_reads_per_sec),
                ull(m.pages_output_per_sec), ull(m.page_writes_per_sec));

    // Processes and threads
    const TelProcessSummary& p = s.processes;
    checkv(p.process_count > 10u, "processes.process_count > 10", "%u processes, %u handles",
           p.process_count, p.handle_count);
    checkv(p.threads.total >= p.process_count, "threads.total >= process_count",
           "%u threads (running %u, ready %u, waiting %u, other %u)", p.threads.total,
           p.threads.running, p.threads.ready, p.threads.waiting, p.threads.other);
    std::printf("      scan_cost_us=%llu scan_age_ms=%llu\n", ull(p.scan_cost_us), ull(p.scan_age_ms));

    checkv(s.top_process_count >= 1u && s.top_process_count <= TEL_MAX_TOP_PROCESSES,
           "top_process_count >= 1", "%u", s.top_process_count);
    const TelProcess& top = s.top_processes[0];
    char name_utf8[256];
    narrow_name(top.name, name_utf8, static_cast<int>(sizeof(name_utf8)));
    checkv(top.name[0] != L'\0', "top_processes[0].name non-empty", "\"%s\"", name_utf8);
    checkv(top.pid > 0u, "top_processes[0].pid > 0",
           "pid=%u parent=%u cpu=%.1f%% ws=%.1f MiB private=%.1f MiB threads=%u handles=%u", top.pid,
           top.parent_pid, top.cpu_percent, mib(top.working_set_bytes), mib(top.private_bytes),
           top.thread_count, top.handle_count);

    // Timing
    checkv(s.sample_interval_ms > 0.0 && s.sample_interval_ms <= 100.0, "sample_interval_ms in (0, 100]",
           "%.2f ms", s.sample_interval_ms);
    checkv(s.sample_cost_us < 5000.0, "sample_cost_us < 5000", "%.1f us", s.sample_cost_us);
    checkv(s.uptime_ms > 0u, "uptime_ms > 0", "%llu ms", ull(s.uptime_ms));
    std::printf("      sequence=%llu qpc_frequency=%llu\n", ull(s.sequence), ull(s.qpc_frequency));
}

void test_sequence() {
    section("6. Sequence advances");
    TelSnapshot first{};
    TelSnapshot second{};
    const int32_t rc1 = tel_snapshot(&first);
    Sleep(60);
    const int32_t rc2 = tel_snapshot(&second);
    checkv(rc1 == TEL_OK && rc2 == TEL_OK, "two snapshots 60 ms apart", "rc=%s / %s", status_name(rc1),
           status_name(rc2));
    checkv(second.sequence > first.sequence, "second.sequence > first.sequence", "%llu -> %llu",
           ull(first.sequence), ull(second.sequence));
    const double delta_ms = second.timestamp_qpc > first.timestamp_qpc
                                ? static_cast<double>(second.timestamp_qpc - first.timestamp_qpc) * 1000.0 /
                                      static_cast<double>(qpc_frequency())
                                : 0.0;
    checkv(second.timestamp_qpc > first.timestamp_qpc, "timestamp_qpc increases", "+%.2f ms", delta_ms);
}

void test_latency() {
    section("7. tel_snapshot latency");
    constexpr size_t kCalls = 10000;
    constexpr int kWarmup = 200;
    TelSnapshot s{};
    std::vector<double> samples_us;
    samples_us.reserve(kCalls);

    const double ticks_to_us = 1.0e6 / static_cast<double>(qpc_frequency());
    for (int i = 0; i < kWarmup; ++i) {
        tel_snapshot(&s);
    }

    // The loop measures the call, not the scheduler: a slightly raised priority makes a
    // preemption spike inside the 10000 calls less likely on a busy desktop.
    const HANDLE self = GetCurrentThread();
    const int old_priority = GetThreadPriority(self);
    SetThreadPriority(self, THREAD_PRIORITY_ABOVE_NORMAL);
    uint32_t bad = 0;
    for (size_t i = 0; i < kCalls; ++i) {
        const uint64_t t0 = qpc_now();
        const int32_t rc = tel_snapshot(&s);
        const uint64_t t1 = qpc_now();
        if (rc != TEL_OK) {
            ++bad;
        }
        samples_us.push_back(static_cast<double>(t1 - t0) * ticks_to_us);
    }
    if (old_priority != THREAD_PRIORITY_ERROR_RETURN) {
        SetThreadPriority(self, old_priority);
    }

    std::sort(samples_us.begin(), samples_us.end());
    double sum = 0.0;
    for (double v : samples_us) {
        sum += v;
    }
    const double mean = sum / static_cast<double>(samples_us.size());
    const double p99 = samples_us[std::min(samples_us.size() - 1, (samples_us.size() * 99) / 100)];
    const double max = samples_us.back();

    checkv(bad == 0, "10000 tel_snapshot calls all TEL_OK", "%u failures", bad);
    checkv(mean < 100.0, "tel_snapshot mean < 100 us", "%.2f us", mean);
    checkv(p99 < 1000.0, "tel_snapshot p99 < 1000 us", "%.2f us", p99);
    checkv(max < 20000.0, "tel_snapshot max < 20000 us", "%.2f us", max);
    std::printf("      min=%.2f us median=%.2f us\n", samples_us.front(),
                samples_us[samples_us.size() / 2]);
}

void test_sample_now_and_intervals() {
    section("8. tel_sample_now and tel_set_intervals");
    TelSnapshot before{};
    TelSnapshot after{};
    tel_snapshot(&before);
    int32_t rc = tel_sample_now();
    tel_snapshot(&after);
    checkv(rc == TEL_OK, "tel_sample_now()", "rc=%s err=\"%s\"", status_name(rc), last_error_text());
    checkv(after.sequence > before.sequence, "sequence increments after tel_sample_now()", "%llu -> %llu",
           ull(before.sequence), ull(after.sequence));
    std::printf("      synchronous sample_cost_us=%.1f scan_cost_us=%llu\n", after.sample_cost_us,
                ull(after.processes.scan_cost_us));

    rc = tel_set_intervals(8, 250);
    checkv(rc == TEL_OK, "tel_set_intervals(8, 250)", "rc=%s", status_name(rc));
    Sleep(200);
    TelSnapshot fast{};
    tel_snapshot(&fast);
    checkv(fast.sample_interval_ms < 40.0, "sample_interval_ms < 40 at 8 ms cadence", "%.2f ms",
           fast.sample_interval_ms);

    rc = tel_set_intervals(0, 0);
    checkv(rc == TEL_OK, "tel_set_intervals(0, 0) accepted (clamped to 1 ms)", "rc=%s", status_name(rc));
    rc = tel_set_intervals(16, 500);
    checkv(rc == TEL_OK, "tel_set_intervals(16, 500) restores defaults", "rc=%s", status_name(rc));
}

// One init + short run + snapshots + shutdown cycle. Failure counts accumulate in the
// out-parameters so the caller can report them as single checks.
void lifecycle_cycle(uint32_t& init_failures, uint32_t& snapshot_failures) {
    if (tel_init(nullptr) != TEL_OK) {
        ++init_failures;
        return;
    }
    Sleep(40);
    TelSnapshot s{};
    for (int i = 0; i < 5; ++i) {
        if (tel_snapshot(&s) != TEL_OK) {
            ++snapshot_failures;
        }
    }
    tel_shutdown();
}

void test_leak() {
    section("9. Leak check over 25 init/shutdown cycles");
    tel_shutdown();
    checkv(tel_is_running() == 0, "tel_shutdown() stops the running sampler", "%d", tel_is_running());

    // One unmeasured cycle first: heap segment growth and CRT thread-local storage are
    // paid once on the first restart and would otherwise be misread as per-cycle growth.
    uint32_t warm_init_failures = 0;
    uint32_t warm_snapshot_failures = 0;
    lifecycle_cycle(warm_init_failures, warm_snapshot_failures);

    const uint64_t before = private_usage_bytes();
    uint32_t init_failures = 0;
    uint32_t snapshot_failures = 0;
    constexpr int kCycles = 25;
    for (int cycle = 0; cycle < kCycles; ++cycle) {
        lifecycle_cycle(init_failures, snapshot_failures);
    }
    const uint64_t after = private_usage_bytes();
    const long long growth = static_cast<long long>(after) - static_cast<long long>(before);

    checkv(before != 0 && after != 0, "GetProcessMemoryInfo succeeded", "before=%llu after=%llu",
           ull(before), ull(after));
    checkv(init_failures == 0 && warm_init_failures == 0, "25 x tel_init() all TEL_OK", "%u failures",
           init_failures + warm_init_failures);
    checkv(snapshot_failures == 0 && warm_snapshot_failures == 0, "25 x 5 tel_snapshot() all TEL_OK",
           "%u failures", snapshot_failures + warm_snapshot_failures);
    checkv(growth < (4ll << 20), "PrivateUsage growth < 4 MiB over 25 cycles",
           "before=%llu after=%llu growth=%lld bytes (%.2f MiB)", ull(before), ull(after), growth,
           static_cast<double>(growth) / (1024.0 * 1024.0));

    checkv(tel_is_running() == 0, "tel_is_running() == 0 after final shutdown", "%d", tel_is_running());
    TelSnapshot s{};
    const int32_t rc = tel_snapshot(&s);
    checkv(rc == TEL_E_NOT_INITIALIZED, "tel_snapshot() after final shutdown", "rc=%s", status_name(rc));
}

void test_concurrency() {
    section("10. Concurrent readers with synchronous sampling");
    const int32_t rc = tel_init(nullptr);
    if (!checkv(rc == TEL_OK, "tel_init() for the concurrency test", "rc=%s err=\"%s\"", status_name(rc),
                last_error_text())) {
        return;
    }

    constexpr int kReaders = 4;
    constexpr int kCallsPerReader = 2000;
    constexpr int kSyncSamples = 20;

    std::atomic<uint32_t> reader_failures{0};
    std::vector<std::thread> readers;
    bool spawned = true;
    try {
        readers.reserve(kReaders);
        for (int t = 0; t < kReaders; ++t) {
            readers.emplace_back([&reader_failures] {
                TelSnapshot local{};
                uint32_t bad = 0;
                for (int i = 0; i < kCallsPerReader; ++i) {
                    if (tel_snapshot(&local) != TEL_OK) {
                        ++bad;
                    }
                }
                reader_failures.fetch_add(bad, std::memory_order_relaxed);
            });
        }
    } catch (const std::exception& e) {
        spawned = false;
        std::printf("      thread creation failed: %s\n", e.what());
    }

    uint32_t sampler_failures = 0;
    for (int i = 0; i < kSyncSamples; ++i) {
        if (tel_sample_now() != TEL_OK) {
            ++sampler_failures;
        }
        Sleep(5);
    }
    for (std::thread& th : readers) {
        th.join();
    }

    checkv(spawned && readers.size() == static_cast<size_t>(kReaders), "spawned 4 reader threads", "%zu",
           readers.size());
    checkv(reader_failures.load() == 0, "4 x 2000 concurrent tel_snapshot() all TEL_OK", "%u failures",
           reader_failures.load());
    checkv(sampler_failures == 0, "20 tel_sample_now() during concurrent reads all TEL_OK", "%u failures",
           sampler_failures);

    tel_shutdown();
    checkv(tel_is_running() == 0, "tel_shutdown() after the concurrency test", "%d", tel_is_running());
}

// Spins one thread for `ms` milliseconds. Runs in the child process started by
// test_process_attribution.
void burn(DWORD ms) {
    const ULONGLONG end = GetTickCount64() + ms;
    volatile uint64_t sink = 0;
    while (GetTickCount64() < end) {
        for (int i = 0; i < 100000; ++i) sink = sink + static_cast<uint64_t>(i);
    }
}

void test_process_attribution() {
    section("11. Per-process CPU attribution");
    TelConfig cfg{};
    tel_default_config(&cfg);
    cfg.process_interval_ms = 250;
    cfg.top_process_count = TEL_MAX_TOP_PROCESSES;
    int32_t rc = tel_init(&cfg);
    if (!checkv(rc == TEL_OK, "tel_init() for the attribution test", "rc=%s", status_name(rc))) return;

    wchar_t self[MAX_PATH] = {};
    GetModuleFileNameW(nullptr, self, MAX_PATH);
    wchar_t cmd[MAX_PATH + 32] = {};
    swprintf_s(cmd, L"\"%s\" --burn 2500", self);
    STARTUPINFOW si{};
    si.cb = sizeof si;
    PROCESS_INFORMATION pi{};
    const bool spawned = CreateProcessW(nullptr, cmd, nullptr, nullptr, FALSE, 0, nullptr, nullptr, &si, &pi) != 0;
    if (!checkv(spawned, "spawned a CPU-bound child", "error %lu", spawned ? 0ul : GetLastError())) {
        tel_shutdown();
        return;
    }

    // Two measuring scans must complete after the child starts spinning.
    Sleep(1200);
    TelSnapshot s{};
    rc = tel_snapshot(&s);
    checkv(rc == TEL_OK, "tel_snapshot() during the child's burn", "rc=%s", status_name(rc));

    const uint32_t cores = s.cpu.core_count != 0 ? s.cpu.core_count : 1u;
    const float one_core = 100.0f / static_cast<float>(cores);
    const TelProcess* child = nullptr;
    float sum = 0.0f;
    for (uint32_t i = 0; i < s.top_process_count; ++i) {
        sum += s.top_processes[i].cpu_percent;
        if (s.top_processes[i].pid == pi.dwProcessId) child = &s.top_processes[i];
    }
    checkv(child != nullptr, "child pid appears in the top list", "pid=%lu", pi.dwProcessId);
    if (child != nullptr) {
        checkv(child->cpu_percent > 0.25f * one_core && child->cpu_percent <= one_core + 1.0f,
               "child cpu_percent near one core's share", "%.2f%% (one core = %.2f%%)",
               child->cpu_percent, one_core);
    }
    checkv(sum <= 100.5f, "sum of top-process cpu_percent <= 100", "%.2f%%", sum);

    WaitForSingleObject(pi.hProcess, 10000);
    CloseHandle(pi.hThread);
    CloseHandle(pi.hProcess);
    tel_shutdown();
}

void run_all() {
    test_abi();
    test_uninitialized();
    if (!test_init()) {
        std::printf("\nsampler did not start; skipping the remaining sections\n");
        return;
    }
    test_first_snapshot();
    test_plausibility();
    test_sequence();
    test_latency();
    test_sample_now_and_intervals();
    test_leak();
    test_concurrency();
    test_process_attribution();
}

}  // namespace

int main(int argc, char** argv) {
    if (argc >= 3 && std::strcmp(argv[1], "--burn") == 0) {
        burn(static_cast<DWORD>(std::strtoul(argv[2], nullptr, 10)));
        return 0;
    }
    std::printf("optimizer_telemetry smoke test\n");
    try {
        run_all();
    } catch (const std::exception& e) {
        std::printf("FAIL  unhandled exception: %s\n", e.what());
        ++g_failures;
    }
    // Every path that returns early leaves the sampler in a defined state.
    tel_shutdown();
    std::printf("\n%d check(s) failed\n", g_failures);
    return g_failures;
}
