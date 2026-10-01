// Physical, commit and kernel pool usage from GlobalMemoryStatusEx and
// SystemPerformanceInformation, plus paging, context-switch and system-call rates
// derived from the ULONG event counters of the latter.
#include "sampler.h"

#include <cstring>
#include <new>

namespace tel {

namespace {

// The event counters in SYSTEM_PERFORMANCE_INFORMATION are 32-bit and wrap. Unsigned
// subtraction yields the true distance as long as fewer than 2^32 events occur between
// two readings, which no counter approaches at the sampling cadence.
uint32_t delta32(ULONG now, ULONG prev) noexcept {
    return static_cast<uint32_t>(now) - static_cast<uint32_t>(prev);
}

// Converts an event count over dt_sec seconds (> 0) into events per second, rounded to
// the nearest integer.
uint64_t per_second(uint32_t events, double dt_sec) noexcept {
    const double rate = static_cast<double>(events) / dt_sec;
    if (!(rate > 0.0)) return 0;
    // Saturate below 2^64 so the double-to-integer cast stays defined for any dt_sec.
    // Unreachable in practice: 2^32 events over a 100 ns interval is only 4.3e16 per second.
    if (rate >= 1.8e19) return UINT64_MAX;
    return static_cast<uint64_t>(rate + 0.5);
}

float clamp_percent(double value) noexcept {
    if (!(value > 0.0)) return 0.0f;  // negative and NaN both map to 0
    if (value > 100.0) return 100.0f;
    return static_cast<float>(value);
}

// Raw counters carried from one reading to the next.
struct Counters {
    ULONG page_faults = 0;       // PageFaultCount: soft + hard
    ULONG pages_read = 0;        // PageReadCount: pages read from disk to satisfy faults
    ULONG page_read_ios = 0;     // PageReadIoCount
    ULONG pages_written = 0;     // DirtyPagesWriteCount + MappedPagesWriteCount
    ULONG page_write_ios = 0;    // DirtyWriteIoCount + MappedWriteIoCount
    ULONG context_switches = 0;  // ContextSwitches
    ULONG system_calls = 0;      // SystemCalls
};

// The summed pairs wrap modulo 2^32 exactly like the individual counters, so delta32 on
// the sum remains correct across a wrap of either term.
Counters capture(const nt::SYSTEM_PERFORMANCE_INFORMATION& p) noexcept {
    Counters c;
    c.page_faults      = p.PageFaultCount;
    c.pages_read       = p.PageReadCount;
    c.page_read_ios    = p.PageReadIoCount;
    c.pages_written    = p.DirtyPagesWriteCount + p.MappedPagesWriteCount;
    c.page_write_ios   = p.DirtyWriteIoCount + p.MappedWriteIoCount;
    c.context_switches = p.ContextSwitches;
    c.system_calls     = p.SystemCalls;
    return c;
}

// Rates computed over the last interval with positive elapsed time. They are reported
// again when a sample lands on the same QPC tick as the previous one.
struct Rates {
    uint64_t page_faults = 0;
    uint64_t hard_faults = 0;
    uint64_t page_reads = 0;
    uint64_t pages_output = 0;
    uint64_t page_writes = 0;
    uint64_t context_switches = 0;
    uint64_t syscalls = 0;
};

// NtQuerySystemInformation rejects SystemBasicInformation with STATUS_INFO_LENGTH_MISMATCH
// unless the length equals the x64 kernel structure (0x40 bytes) exactly.
static_assert(sizeof(nt::SYSTEM_BASIC_INFORMATION) == 0x40);

}  // namespace

struct MemorySampler::Impl {
    // Receives SystemPerformanceInformation. Sized generously because newer kernels
    // append fields after SystemCalls; only the declared prefix is ever read.
    std::unique_ptr<unsigned char[]> perf_buffer;
    ULONG    perf_buffer_size = 0;

    uint64_t page_size = 0;
    uint64_t qpc_freq  = 0;
    uint64_t prev_qpc  = 0;
    Counters prev;
    Rates    last;
    // Set only after init() has stored a valid baseline, so a failed init() can never
    // produce a first sample whose deltas span every event since boot.
    bool     ready = false;

    bool ensure_buffer(ULONG bytes);
    bool query_performance(nt::SYSTEM_PERFORMANCE_INFORMATION& out);
};

bool MemorySampler::Impl::ensure_buffer(ULONG bytes) {
    if (perf_buffer && perf_buffer_size >= bytes) return true;
    unsigned char* grown = new (std::nothrow) unsigned char[bytes];
    if (grown == nullptr) {
        set_error("MemorySampler: cannot allocate %lu bytes for SystemPerformanceInformation",
                  static_cast<unsigned long>(bytes));
        return false;
    }
    perf_buffer.reset(grown);
    perf_buffer_size = bytes;
    return true;
}

bool MemorySampler::Impl::query_performance(nt::SYSTEM_PERFORMANCE_INFORMATION& out) {
    constexpr ULONG kPrefix = static_cast<ULONG>(sizeof(nt::SYSTEM_PERFORMANCE_INFORMATION));

    ULONG returned = 0;
    nt::NTSTATUS st = g_nt.query(nt::SystemPerformanceInformation, perf_buffer.get(),
                                 perf_buffer_size, &returned);
    if (st == nt::STATUS_INFO_LENGTH_MISMATCH && returned > perf_buffer_size) {
        // The kernel structure has outgrown the buffer: grow once to the size it reports
        // and retry. This is the only allocation the hot path can make.
        if (!ensure_buffer(returned)) return false;
        returned = 0;
        st = g_nt.query(nt::SystemPerformanceInformation, perf_buffer.get(),
                        perf_buffer_size, &returned);
    }
    if (!nt::success(st)) {
        set_nt_error("NtQuerySystemInformation(SystemPerformanceInformation)", st);
        return false;
    }
    if (returned < kPrefix) {
        set_error("SystemPerformanceInformation returned %lu bytes, expected at least %lu",
                  static_cast<unsigned long>(returned), static_cast<unsigned long>(kPrefix));
        return false;
    }
    // memcpy rather than a pointer cast: the buffer is raw byte storage and the
    // structure contains 8-byte members.
    std::memcpy(&out, perf_buffer.get(), sizeof(out));
    return true;
}

MemorySampler::MemorySampler() = default;

MemorySampler::~MemorySampler() { shutdown(); }

bool MemorySampler::init() {
    try {
        if (!impl_) {
            impl_.reset(new (std::nothrow) Impl);
            if (!impl_) {
                set_error("MemorySampler: cannot allocate sampler state");
                return false;
            }
        }
        Impl& im = *impl_;
        im.ready = false;

        if (g_nt.query == nullptr) {
            set_error("MemorySampler: NtQuerySystemInformation is not resolved");
            return false;
        }

        // Page size from the kernel via SystemBasicInformation; GetSystemInfo is the
        // fallback and cannot fail.
        im.page_size = 0;
        {
            nt::SYSTEM_BASIC_INFORMATION basic{};
            ULONG returned = 0;
            const nt::NTSTATUS st = g_nt.query(nt::SystemBasicInformation, &basic,
                                               static_cast<ULONG>(sizeof(basic)), &returned);
            if (nt::success(st) && basic.PageSize != 0) im.page_size = basic.PageSize;
        }
        if (im.page_size == 0) {
            SYSTEM_INFO si{};
            GetSystemInfo(&si);
            im.page_size = si.dwPageSize;
        }
        if (im.page_size == 0) {
            set_error("MemorySampler: page size is unavailable");
            return false;
        }

        if (!im.ensure_buffer(nt::SYSTEM_PERFORMANCE_INFORMATION_QUERY_SIZE)) return false;

        // Baseline reading: the first sample() computes its deltas against these counters
        // and this timestamp, so its rates cover only the short interval since init().
        nt::SYSTEM_PERFORMANCE_INFORMATION perf{};
        if (!im.query_performance(perf)) return false;
        im.prev     = capture(perf);
        im.prev_qpc = qpc_now();
        im.qpc_freq = qpc_frequency();
        im.last     = Rates{};
        im.ready    = true;
        return true;
    } catch (...) {
        set_error("MemorySampler: unexpected exception in init()");
        return false;
    }
}

void MemorySampler::shutdown() {
    // Releases the query buffer and every carried counter; init() may run again.
    impl_.reset();
}

bool MemorySampler::sample(TelMemory& out, SystemRates& rates) {
    try {
        if (!impl_ || !impl_->ready || g_nt.query == nullptr) {
            set_error("MemorySampler: sample() called before a successful init()");
            return false;
        }
        Impl& im = *impl_;

        MEMORYSTATUSEX mem{};
        mem.dwLength = static_cast<DWORD>(sizeof(mem));
        if (!GlobalMemoryStatusEx(&mem)) {
            set_win32_error("GlobalMemoryStatusEx", GetLastError());
            return false;
        }

        nt::SYSTEM_PERFORMANCE_INFORMATION perf{};
        if (!im.query_performance(perf)) return false;
        const uint64_t now = qpc_now();

        // Both system calls succeeded and nothing below can fail, so the outputs are
        // written from here on.
        const Counters cur = capture(perf);
        double dt_sec = 0.0;
        if (now > im.prev_qpc && im.qpc_freq != 0) {
            dt_sec = static_cast<double>(now - im.prev_qpc) / static_cast<double>(im.qpc_freq);
        }
        if (dt_sec >= kMinRateWindowSec) {
            Rates r;
            r.page_faults      = per_second(delta32(cur.page_faults, im.prev.page_faults), dt_sec);
            r.hard_faults      = per_second(delta32(cur.pages_read, im.prev.pages_read), dt_sec);
            r.page_reads       = per_second(delta32(cur.page_read_ios, im.prev.page_read_ios), dt_sec);
            r.pages_output     = per_second(delta32(cur.pages_written, im.prev.pages_written), dt_sec);
            r.page_writes      = per_second(delta32(cur.page_write_ios, im.prev.page_write_ios), dt_sec);
            r.context_switches = per_second(delta32(cur.context_switches, im.prev.context_switches), dt_sec);
            r.syscalls         = per_second(delta32(cur.system_calls, im.prev.system_calls), dt_sec);
            im.last     = r;
            im.prev     = cur;
            im.prev_qpc = now;
        }
        // dt_sec < kMinRateWindowSec (a forced sample right after a tick, or a clock that
        // did not advance): the previous rates stand and the baseline is kept so the next
        // sample spans both intervals.

        const uint64_t page = im.page_size;
        out.physical_total_bytes     = mem.ullTotalPhys;
        out.physical_available_bytes = mem.ullAvailPhys;
        out.physical_used_bytes      = mem.ullTotalPhys >= mem.ullAvailPhys
                                           ? mem.ullTotalPhys - mem.ullAvailPhys
                                           : 0;
        out.commit_total_bytes         = static_cast<uint64_t>(perf.CommittedPages) * page;
        out.commit_limit_bytes         = static_cast<uint64_t>(perf.CommitLimit) * page;
        out.commit_peak_bytes          = static_cast<uint64_t>(perf.PeakCommitment) * page;
        out.system_cache_bytes         = static_cast<uint64_t>(perf.ResidentSystemCachePage) * page;
        out.kernel_paged_pool_bytes    = static_cast<uint64_t>(perf.PagedPoolPages) * page;
        out.kernel_nonpaged_pool_bytes = static_cast<uint64_t>(perf.NonPagedPoolPages) * page;
        out.page_size                  = page;
        out.page_faults_per_sec  = im.last.page_faults;
        out.hard_faults_per_sec  = im.last.hard_faults;
        out.page_reads_per_sec   = im.last.page_reads;
        out.pages_output_per_sec = im.last.pages_output;
        out.page_writes_per_sec  = im.last.page_writes;
        out.memory_load_percent  = clamp_percent(static_cast<double>(mem.dwMemoryLoad));
        out.commit_percent       = out.commit_limit_bytes != 0
            ? clamp_percent(static_cast<double>(out.commit_total_bytes) * 100.0 /
                            static_cast<double>(out.commit_limit_bytes))
            : 0.0f;

        rates.context_switches_per_sec = im.last.context_switches;
        rates.syscalls_per_sec         = im.last.syscalls;
        return true;
    } catch (...) {
        set_error("MemorySampler: unexpected exception in sample()");
        return false;
    }
}

}  // namespace tel
