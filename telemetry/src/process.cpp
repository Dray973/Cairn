// Process and thread enumeration via SystemProcessInformation.
//
// A single NtQuerySystemInformation(SystemProcessInformation) call returns every process
// together with its inline thread list and image name in one variable-length buffer. The
// buffer is owned by the sampler, reused between scans and only ever grows. Per-process
// CPU time is differenced against the previous scan under a (pid, create time) key so a
// recycled pid is never charged with its predecessor's time. A key missing from the
// previous scan's table has no baseline. When that scan's walk reached the last entry, the
// process was not in the list at the time, so all of its CPU time accrued inside the
// interval and it is charged in full; no clock comparison is involved, so a step of the
// wall clock between the scans cannot hide it. When a malformed entry or the end of the
// returned bytes cut that walk short,
// a missing key may instead belong to an older process the walk never reached, so only a
// process whose create time is at or after that scan's start is charged in full and any
// other reports 0.
#include "sampler.h"

#include <algorithm>
#include <cstddef>
#include <cstdint>
#include <cstring>
#include <new>
#include <utility>
#include <vector>

namespace tel {

namespace {

constexpr size_t kInitialBufferBytes  = 1024u * 1024u;
constexpr size_t kGrowSlackBytes      = 256u * 1024u;
constexpr size_t kMaxBufferBytes      = 512u * 1024u * 1024u;
constexpr int    kMaxQueryAttempts    = 8;
constexpr size_t kInitialCandidates   = 1024;
constexpr size_t kInitialHistorySlots = 2048;  // power of two; holds 1024 processes at load 0.5

// Entry offsets are stored as 32-bit values in the candidate list.
static_assert(kMaxBufferBytes <= UINT32_MAX);
static_assert((kInitialHistorySlots & (kInitialHistorySlots - 1)) == 0);

constexpr uint64_t kTicks100nsPerSecond = 10000000ull;

// Per-process KernelTime and UserTime advance in whole clock ticks (15.625 ms by default),
// so over a window of only a few ticks every share is a multiple of one tick's worth of
// capacity. Shares are measured over at least ten ticks; a scan inside a shorter window
// republishes the shares of the last measured window.
constexpr uint64_t kClockTick100ns    = 156250;
constexpr uint64_t kMinCpuWindow100ns = 10 * kClockTick100ns;

// Fixed part of a process entry; NumberOfThreads thread records follow it inline.
constexpr size_t kEntryHeaderBytes  = offsetof(nt::SYSTEM_PROCESS_INFORMATION, Threads);
constexpr size_t kThreadRecordBytes = sizeof(nt::SYSTEM_THREAD_INFORMATION);
constexpr size_t kEntryAlignment    = alignof(nt::SYSTEM_PROCESS_INFORMATION);

constexpr uint32_t kIdlePid   = 0;
constexpr uint32_t kSystemPid = 4;

// TelProcess::name is copied from UTF-16 image names with memcpy.
static_assert(sizeof(wchar_t) == sizeof(WCHAR));
constexpr wchar_t kSystemName[]  = L"System";
constexpr wchar_t kUnknownName[] = L"<unknown>";
static_assert(sizeof(kSystemName) <= sizeof(TelProcess::name));
static_assert(sizeof(kUnknownName) <= sizeof(TelProcess::name));
// Truncation keeps at least TEL_PROCESS_NAME_CHARS - 2 units after dropping a split pair.
static_assert(TEL_PROCESS_NAME_CHARS >= 3);

constexpr bool is_high_surrogate(wchar_t c) noexcept {
    return c >= 0xD800 && c <= 0xDBFF;
}

// Current system time in 100 ns units since 1601, the clock CreateTime is recorded in.
int64_t system_time_now() noexcept {
    FILETIME ft;
    GetSystemTimePreciseAsFileTime(&ft);
    return static_cast<int64_t>((static_cast<uint64_t>(ft.dwHighDateTime) << 32) | ft.dwLowDateTime);
}

// Identity of a process across scans. CreateTime disambiguates pid reuse.
struct ProcessKey {
    uint32_t pid;
    int64_t  create_time;  // CreateTime.QuadPart, 100 ns units since 1601
};

size_t hash_key(const ProcessKey& key) noexcept {
    // splitmix64 finaliser over both fields: pids are small multiples of four and would
    // otherwise collide in the low index bits.
    uint64_t x = static_cast<uint64_t>(key.pid) ^
                 (static_cast<uint64_t>(key.create_time) * 0x9E3779B97F4A7C15ull);
    x ^= x >> 30;
    x *= 0xBF58476D1CE4E5B9ull;
    x ^= x >> 27;
    x *= 0x94D049BB133111EBull;
    x ^= x >> 31;
    return static_cast<size_t>(x);
}

struct HistorySlot {
    int64_t  create_time    = 0;
    uint64_t cpu_time_100ns = 0;     // KernelTime + UserTime when the process was observed
    uint64_t generation     = 0;     // live only while equal to the owning table's generation
    uint32_t pid            = 0;
    float    cpu_percent    = 0.0f;  // share measured by the scan that recorded the slot
};

// Linear-probing table of CPU-time observations made during one scan. Two instances
// alternate: a scan reads the table built by the previous scan and writes a fresh one,
// so a process that exited is dropped simply by not being re-inserted. Starting a table
// only bumps its generation, which invalidates every slot at once without touching
// memory; slots start at generation 0 and scans use generations from 1, and 64-bit
// generations cannot wrap in practice. Nodes are never allocated per entry: the slot
// array grows only when a scan sees more processes than it can hold at load 0.5.
class HistoryTable {
public:
    void allocate(size_t slot_count) {
        slots_.assign(slot_count, HistorySlot{});
        mask_       = slot_count - 1;
        live_       = 0;
        generation_ = 0;
    }

    void begin(uint64_t generation) noexcept {
        generation_ = generation;
        live_       = 0;
    }

    // Returns the recorded CPU time and share for `key`, or false when the table does not
    // hold it. The outputs are written only when the key is found.
    bool find(const ProcessKey& key, uint64_t& cpu_time_100ns, float& cpu_percent) const noexcept {
        if (generation_ == 0 || slots_.empty()) return false;
        size_t index = hash_key(key) & mask_;
        for (size_t probes = 0; probes < slots_.size(); ++probes) {
            const HistorySlot& slot = slots_[index];
            if (slot.generation != generation_) return false;
            if (slot.pid == key.pid && slot.create_time == key.create_time) {
                cpu_time_100ns = slot.cpu_time_100ns;
                cpu_percent    = slot.cpu_percent;
                return true;
            }
            index = (index + 1) & mask_;
        }
        return false;
    }

    // Records `cpu_time_100ns` and `cpu_percent` for `key`, doubling the slot array first
    // when the insert would push the load factor above 0.5. Throws std::bad_alloc only from
    // that growth, in which case the table is left unchanged.
    void insert(const ProcessKey& key, uint64_t cpu_time_100ns, float cpu_percent) {
        if ((live_ + 1) * 2 > slots_.size()) grow();
        place(key, cpu_time_100ns, cpu_percent);
    }

private:
    void place(const ProcessKey& key, uint64_t cpu_time_100ns, float cpu_percent) noexcept {
        size_t index = hash_key(key) & mask_;
        for (;;) {
            HistorySlot& slot = slots_[index];
            if (slot.generation != generation_) {
                slot.create_time    = key.create_time;
                slot.cpu_time_100ns = cpu_time_100ns;
                slot.generation     = generation_;
                slot.pid            = key.pid;
                slot.cpu_percent    = cpu_percent;
                ++live_;
                return;
            }
            if (slot.pid == key.pid && slot.create_time == key.create_time) {
                slot.cpu_time_100ns = cpu_time_100ns;
                slot.cpu_percent    = cpu_percent;
                return;
            }
            index = (index + 1) & mask_;
        }
    }

    void grow() {
        const size_t new_count = slots_.empty() ? kInitialHistorySlots : slots_.size() * 2;
        std::vector<HistorySlot> fresh(new_count);  // allocate before mutating any state
        fresh.swap(slots_);
        mask_ = new_count - 1;
        live_ = 0;
        for (const HistorySlot& slot : fresh) {
            if (slot.generation == generation_) {
                place(ProcessKey{slot.pid, slot.create_time}, slot.cpu_time_100ns, slot.cpu_percent);
            }
        }
    }

    std::vector<HistorySlot> slots_;
    size_t   mask_       = 0;
    size_t   live_       = 0;
    uint64_t generation_ = 0;
};

// Selection record for the top list. The entry is re-read from the buffer when the output
// struct is filled, so the record stays small enough for partial_sort to move cheaply.
struct Candidate {
    uint64_t working_set_bytes;
    float    cpu_percent;
    uint32_t entry_offset;  // byte offset of a validated entry inside the buffer
};

bool candidate_before(const Candidate& a, const Candidate& b) noexcept {
    if (a.cpu_percent != b.cpu_percent) return a.cpu_percent > b.cpu_percent;
    if (a.working_set_bytes != b.working_set_bytes) return a.working_set_bytes > b.working_set_bytes;
    return a.entry_offset < b.entry_offset;  // buffer order keeps full ties deterministic
}

uint32_t handle_to_pid(HANDLE h) noexcept {
    return static_cast<uint32_t>(reinterpret_cast<uintptr_t>(h));
}

// Converts a QPC tick delta to 100 ns units without overflowing the intermediate product.
uint64_t ticks_to_100ns(uint64_t ticks, uint64_t frequency) noexcept {
    const uint64_t whole = ticks / frequency;
    const uint64_t rem   = ticks % frequency;
    return whole * kTicks100nsPerSecond + (rem * kTicks100nsPerSecond) / frequency;
}

}  // namespace

struct ProcessSampler::Impl {
    std::vector<std::byte> buffer;                    // SystemProcessInformation result; never shrinks
    size_t                 valid_bytes        = 0;    // bytes the kernel wrote in the last query
    uint64_t               query_qpc          = 0;    // QPC midpoint of the last successful query
    int64_t                query_systime      = 0;    // system time just before that query
    HistoryTable           tables[2];                 // tables[write_index] is built by the current scan
    unsigned               write_index        = 0;
    uint64_t               generation         = 0;
    std::vector<Candidate> candidates;                // non-idle processes of the current scan
    uint32_t               top_count          = 0;
    uint32_t               machine_processors = 0;    // active logical processors, all groups, at init()
    uint64_t               previous_qpc       = 0;    // query_qpc of the last measuring scan
    int64_t                previous_systime   = 0;    // query_systime of the last measuring scan
    bool                   previous_complete  = false;  // that scan's walk reached the last entry
    bool                   have_previous      = false;

    bool query();
    void walk(double capacity_100ns, bool measure, TelProcessSummary& summary, bool& complete);
    void fill(TelProcess& out, const Candidate& candidate) const;
    bool sample(TelProcessSummary& summary, TelProcess* top, uint32_t& written, uint32_t total_cores);
};

// Fills `buffer` with the current process list, growing it on STATUS_INFO_LENGTH_MISMATCH.
bool ProcessSampler::Impl::query() {
    if (g_nt.query == nullptr) {
        set_error("NtQuerySystemInformation is not resolved");
        return false;
    }
    for (int attempt = 0; attempt < kMaxQueryAttempts; ++attempt) {
        const ULONG capacity = static_cast<ULONG>(buffer.size());
        ULONG returned = 0;
        // The system time only backs the create-time rule that follows a truncated walk.
        // It is read before the call, so a process created during the call but after the
        // kernel captured the list is still later than this scan's start.
        const int64_t  before_systime = system_time_now();
        const uint64_t before         = qpc_now();
        const nt::NTSTATUS status =
            g_nt.query(nt::SystemProcessInformation, buffer.data(), capacity, &returned);
        if (nt::success(status)) {
            // The kernel captures the list somewhere inside the call; the midpoint keeps
            // the scan interval free of the call's own duration jitter.
            query_qpc     = before + (qpc_now() - before) / 2;
            query_systime = before_systime;
            // ReturnLength bounds the walk. A zero or oversized value degrades to the
            // whole buffer, which the walk validates entry by entry anyway.
            valid_bytes = (returned == 0 || returned > capacity) ? buffer.size()
                                                                 : static_cast<size_t>(returned);
            return true;
        }
        if (status != nt::STATUS_INFO_LENGTH_MISMATCH && status != nt::STATUS_BUFFER_TOO_SMALL) {
            set_nt_error("NtQuerySystemInformation(SystemProcessInformation)", status);
            return false;
        }
        if (buffer.size() >= kMaxBufferBytes) {
            set_error("SystemProcessInformation exceeds the %llu MiB buffer limit",
                      static_cast<unsigned long long>(kMaxBufferBytes >> 20));
            return false;
        }
        // Grow to what the kernel asked for plus slack for processes created before the
        // retry. A stale or zero ReturnLength falls back to doubling so every attempt
        // makes progress. The old contents are discarded, so a fresh vector avoids a copy.
        size_t wanted = static_cast<size_t>(returned) + kGrowSlackBytes;
        if (wanted <= buffer.size()) wanted = buffer.size() * 2;
        std::vector<std::byte> larger(std::min(wanted, kMaxBufferBytes));
        buffer.swap(larger);
    }
    set_error("SystemProcessInformation did not fit after %d attempts", kMaxQueryAttempts);
    return false;
}

// Walks the entry chain, accumulating summary counts and the candidate list for this scan.
// A measuring walk derives each process's share of `capacity_100ns` against the previous
// table and records this scan's observations in the current table. A non-measuring walk
// republishes the share recorded by the last measuring scan and leaves both tables
// untouched. Every entry is bounds-checked before any of its fields are read; a malformed
// entry, or the end of the returned bytes, ends the walk with the totals gathered so far.
// `complete` is set only when the walk ends at the entry whose NextEntryOffset is zero,
// that is, when every process the kernel returned was visited.
void ProcessSampler::Impl::walk(double capacity_100ns, bool measure, TelProcessSummary& summary,
                                bool& complete) {
    complete = false;
    candidates.clear();
    const HistoryTable& previous = tables[write_index ^ 1u];
    HistoryTable&       current  = tables[write_index];
    if (measure) current.begin(generation);

    const std::byte* base        = buffer.data();
    const uintptr_t  range_begin = reinterpret_cast<uintptr_t>(base);
    const uintptr_t  range_end   = range_begin + valid_bytes;

    size_t offset = 0;
    for (;;) {
        if (offset % kEntryAlignment != 0) break;
        if (offset > valid_bytes || valid_bytes - offset < kEntryHeaderBytes) break;
        const auto* entry = reinterpret_cast<const nt::SYSTEM_PROCESS_INFORMATION*>(base + offset);

        // The inline thread array must fit, and the next entry must not overlap this one.
        const uint64_t thread_bytes = static_cast<uint64_t>(entry->NumberOfThreads) * kThreadRecordBytes;
        const uint64_t body_bytes   = kEntryHeaderBytes + thread_bytes;
        if (body_bytes > static_cast<uint64_t>(valid_bytes - offset)) break;
        if (entry->NextEntryOffset != 0 && entry->NextEntryOffset < body_bytes) break;

        // The image name, when present, must point inside the returned bytes.
        const nt::UNICODE_STRING& image = entry->ImageName;
        if (image.Buffer != nullptr && image.Length != 0) {
            const uintptr_t name_begin = reinterpret_cast<uintptr_t>(image.Buffer);
            const uintptr_t name_end   = name_begin + image.Length;
            if (name_begin < range_begin || name_end > range_end || name_end < name_begin) break;
        }

        summary.handle_count += entry->HandleCount;
        const uint32_t pid = handle_to_pid(entry->UniqueProcessId);
        if (pid != kIdlePid) {
            summary.process_count += 1;

            const auto* threads = reinterpret_cast<const nt::SYSTEM_THREAD_INFORMATION*>(
                base + offset + kEntryHeaderBytes);
            for (ULONG i = 0; i < entry->NumberOfThreads; ++i) {
                switch (threads[i].ThreadState) {
                    case nt::KTHREAD_STATE::Running:
                        ++summary.threads.running;
                        break;
                    case nt::KTHREAD_STATE::Ready:
                    case nt::KTHREAD_STATE::DeferredReady:
                    case nt::KTHREAD_STATE::Standby:
                        ++summary.threads.ready;
                        break;
                    case nt::KTHREAD_STATE::Waiting:
                        ++summary.threads.waiting;
                        break;
                    default:
                        ++summary.threads.other;
                        break;
                }
            }

            const ProcessKey key{pid, entry->CreateTime.QuadPart};
            const uint64_t cpu_time = static_cast<uint64_t>(entry->KernelTime.QuadPart) +
                                      static_cast<uint64_t>(entry->UserTime.QuadPart);
            uint64_t   previous_time    = 0;
            float      previous_percent = 0.0f;
            const bool known            = previous.find(key, previous_time, previous_percent);

            float cpu_percent = 0.0f;
            if (!measure) {
                // A process first seen since the last measuring scan reports 0.
                cpu_percent = previous_percent;
            } else if (capacity_100ns > 0.0) {
                // capacity_100ns is zero on the first scan after init(), which has no
                // previous table, so every process reports 0 there and this branch is only
                // reached with a committed previous table.
                double delta = -1.0;
                if (known) {
                    if (cpu_time >= previous_time) delta = static_cast<double>(cpu_time - previous_time);
                } else if (previous_complete || key.create_time >= previous_systime) {
                    // Missing from the previous table. After a complete previous walk the
                    // process was not in the list then, so its whole CPU time accrued inside
                    // the interval regardless of any wall-clock step since. After a walk that
                    // ended before the last entry (a malformed entry or the end of the
                    // returned bytes) the key may belong to an older process the walk never
                    // reached, so only a create time at or after the previous
                    // scan's start qualifies and any other process reports 0.
                    delta = static_cast<double>(cpu_time);
                }
                if (delta >= 0.0) {
                    cpu_percent = static_cast<float>(std::clamp(delta * 100.0 / capacity_100ns, 0.0, 100.0));
                }
            }
            if (measure) current.insert(key, cpu_time, cpu_percent);

            candidates.push_back(Candidate{static_cast<uint64_t>(entry->WorkingSetSize),
                                           cpu_percent, static_cast<uint32_t>(offset)});
        }

        if (entry->NextEntryOffset == 0) {
            complete = true;
            break;
        }
        offset += entry->NextEntryOffset;
    }

    summary.threads.total = summary.threads.running + summary.threads.ready +
                            summary.threads.waiting + summary.threads.other;
}

void ProcessSampler::Impl::fill(TelProcess& out, const Candidate& candidate) const {
    const auto* entry =
        reinterpret_cast<const nt::SYSTEM_PROCESS_INFORMATION*>(buffer.data() + candidate.entry_offset);

    out.pid               = handle_to_pid(entry->UniqueProcessId);
    out.parent_pid        = handle_to_pid(entry->InheritedFromUniqueProcessId);
    out.thread_count      = entry->NumberOfThreads;
    out.handle_count      = entry->HandleCount;
    out.working_set_bytes = static_cast<uint64_t>(entry->WorkingSetSize);
    out.private_bytes     = static_cast<uint64_t>(entry->PrivatePageCount);
    out.cpu_percent       = candidate.cpu_percent;
    out._pad0             = 0;

    // Zero the whole name first so truncation and the terminator need no extra writes.
    std::memset(out.name, 0, sizeof(out.name));
    const nt::UNICODE_STRING& image = entry->ImageName;
    const size_t full = (image.Buffer != nullptr) ? image.Length / sizeof(WCHAR) : 0;
    if (full != 0) {
        size_t units = std::min<size_t>(full, TEL_PROCESS_NAME_CHARS - 1);
        // A cut between the halves of a surrogate pair would leave a lone high surrogate,
        // which strict UTF-8 encoders reject; the whole pair is dropped instead.
        if (units < full && is_high_surrogate(image.Buffer[units - 1])) --units;
        std::memcpy(out.name, image.Buffer, units * sizeof(wchar_t));
    } else if (out.pid == kSystemPid) {
        std::memcpy(out.name, kSystemName, sizeof(kSystemName));
    } else {
        std::memcpy(out.name, kUnknownName, sizeof(kUnknownName));
    }
}

bool ProcessSampler::Impl::sample(TelProcessSummary& summary, TelProcess* top, uint32_t& written,
                                  uint32_t total_cores) {
    if (!query()) return false;

    // Interval since the last measuring scan, in 100 ns units to match the kernel's
    // per-process times. Zero on the first scan, which yields 0 % for every process.
    const uint64_t frequency = qpc_frequency();
    uint64_t dt_100ns = 0;
    if (have_previous && frequency != 0 && query_qpc > previous_qpc) {
        dt_100ns = ticks_to_100ns(query_qpc - previous_qpc, frequency);
    }
    // A window shorter than kMinCpuWindow100ns (a forced scan soon after the last one, or
    // a process interval of a few ticks) is not measured. Its scan republishes the shares
    // of the last measured window, and that window's table and timestamps remain the
    // baseline, so the next scan measures over the full, longer interval.
    const bool measure = !have_previous || dt_100ns >= kMinCpuWindow100ns;

    // Per-process times accrue on every active logical processor, while the caller's count
    // may be clamped to TEL_MAX_CORES. Both are lower bounds of the machine's capacity.
    const uint32_t cores = std::max({machine_processors, total_cores, 1u});
    // Machine capacity over the interval in 100 ns units; zero on the first scan.
    const double capacity_100ns = static_cast<double>(dt_100ns) * static_cast<double>(cores);

    // Only a measuring walk that returns commits the new table, whether it reached the last
    // entry, and the timestamps. If growth throws midway, the previous table,
    // previous_complete, previous_qpc and previous_systime still describe the same scan, so
    // the next scan differences a consistent set over the longer interval.
    TelProcessSummary result{};
    bool complete = false;
    if (measure) ++generation;
    walk(capacity_100ns, measure, result, complete);
    if (measure) {
        write_index ^= 1u;
        previous_qpc      = query_qpc;
        previous_systime  = query_systime;
        previous_complete = complete;
        have_previous     = true;
    }

    // Only the top `count` candidates are ordered; the remainder stay unsorted.
    const size_t count = std::min<size_t>(top_count, candidates.size());
    if (count != 0) {
        std::partial_sort(candidates.begin(), candidates.begin() + static_cast<std::ptrdiff_t>(count),
                          candidates.end(), candidate_before);
        for (size_t i = 0; i < count; ++i) {
            fill(top[i], candidates[i]);
        }
    }
    if (top != nullptr && count < TEL_MAX_TOP_PROCESSES) {
        // The array always has TEL_MAX_TOP_PROCESSES slots; clearing the unused tail keeps
        // stale rows from a previous scan out of the published snapshot.
        std::memset(top + count, 0, (TEL_MAX_TOP_PROCESSES - count) * sizeof(TelProcess));
    }
    written = static_cast<uint32_t>(count);

    // scan_cost_us and scan_age_ms stay zero; the caller fills them.
    summary = result;
    return true;
}

ProcessSampler::ProcessSampler() = default;

ProcessSampler::~ProcessSampler() {
    shutdown();
}

// init() takes no baseline scan. Shares need an interval of at least kMinCpuWindow100ns,
// which a baseline taken moments before the first scan cannot provide, so the first scan
// after init() reports cpu_percent 0 for every process and orders the top list by working
// set; later scans measure against it.
bool ProcessSampler::init(uint32_t top_count) {
    shutdown();
    try {
        auto impl = std::make_unique<Impl>();
        impl->buffer.resize(kInitialBufferBytes);
        impl->tables[0].allocate(kInitialHistorySlots);
        impl->tables[1].allocate(kInitialHistorySlots);
        impl->candidates.reserve(kInitialCandidates);
        impl->top_count = std::min<uint32_t>(top_count, TEL_MAX_TOP_PROCESSES);
        // Zero on failure; sample() then falls back to the caller's count.
        impl->machine_processors =
            static_cast<uint32_t>(GetActiveProcessorCount(static_cast<WORD>(ALL_PROCESSOR_GROUPS)));
        impl_ = std::move(impl);
        return true;
    } catch (const std::bad_alloc&) {
        set_error("ProcessSampler::init: out of memory");
    } catch (...) {
        set_error("ProcessSampler::init: unexpected exception");
    }
    return false;
}

void ProcessSampler::shutdown() {
    impl_.reset();
}

bool ProcessSampler::sample(TelProcessSummary& summary, TelProcess* top, uint32_t& written,
                            uint32_t total_cores) {
    if (!impl_) {
        set_error("ProcessSampler::sample: not initialised");
        return false;
    }
    if (top == nullptr && impl_->top_count != 0) {
        set_error("ProcessSampler::sample: top array is null");
        return false;
    }
    try {
        return impl_->sample(summary, top, written, total_cores);
    } catch (const std::bad_alloc&) {
        set_error("ProcessSampler::sample: out of memory");
    } catch (...) {
        set_error("ProcessSampler::sample: unexpected exception");
    }
    return false;
}

}  // namespace tel
