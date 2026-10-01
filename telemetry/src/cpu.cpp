// Per-core utilization, DPC/interrupt time, interrupt rate and effective clock.
//
// Counters come from SystemProcessorPerformanceInformation: one
// SYSTEM_PROCESSOR_PERFORMANCE_INFORMATION per logical processor holding cumulative
// 100 ns times. NtQuerySystemInformation only reports the calling thread's processor
// group, so machines with several groups are read group by group through
// NtQuerySystemInformationEx and the per-group arrays are concatenated.
//
// The kernel charges processor time in whole clock ticks (about 15.6 ms), so two
// readings one 16 ms sample apart differ by a single tick and the busy share of that
// interval jumps between 0 and 100 %. Readings are therefore stored with their QPC
// timestamps in a fixed ring of kRingSlots entries, and percentages and interrupt rates
// are computed from the current reading against the newest stored reading that is at
// least kCpuWindowMs old (the oldest stored reading while the ring is filling). A reading
// is stored only when it is at least kCpuWindowMs / (kRingSlots - 1), rounded up to a
// QPC tick, newer than the newest stored one, so the kRingSlots - 1 gaps of a full ring
// span the window at any sample cadence, including back-to-back tel_sample_now() calls.
// Values are still published at the sample cadence, also for readings that are not
// stored; each one is a kCpuWindowMs trailing average. KernelTime already contains
// IdleTime, DpcTime and InterruptTime, so the busy share of an interval is
// (kernel + user - idle) / (kernel + user).
//
// The effective clock is the nominal (base) clock of each logical processor, MaxMhz from
// CallNtPowerInformation(ProcessorInformation), scaled by the Processor Information
// "% Processor Performance" counter, which is how Task Manager derives its speed. The
// CurrentMhz field of that call is not used: on current Windows it reports the nominal
// clock regardless of load. The counter is a ratio over the interval between two PDH
// collections, and one collection plus formatting costs a few hundred microseconds, so
// it is collected only when the caller asks for a refresh (the process-scan cadence) and
// at most once per kPerfMinIntervalMs; much shorter intervals yield unstable ratios.
// The clock is 0 (unknown) until the first interval completes, for processors the
// counter does not report, and whenever PDH is unavailable or a collection fails.
//
// Counter instances are named "<node>,<index>": the NUMA node number and the zero-based
// index of the processor within that node. init() rebuilds the processor order of every
// node from GetLogicalProcessorInformationEx (groups ascending, processor numbers
// ascending within a group) and maps each instance to a published core through it. When
// the topology cannot be read, only single-group machines are mapped, "0,<k>" to core k.
#include "sampler.h"

#include <windows.h>
#include <pdh.h>
#include <pdhmsg.h>
#include <powrprof.h>

#include <algorithm>
#include <bit>
#include <cstddef>
#include <cstdint>
#include <cstring>
#include <memory>
#include <utility>
#include <vector>

// PdhOpenQueryW and the other PDH entry points live in pdh.dll.
#pragma comment(lib, "pdh.lib")

namespace tel {

namespace {

using PerfInfo = nt::SYSTEM_PROCESSOR_PERFORMANCE_INFORMATION;
using PerfItem = PDH_FMT_COUNTERVALUE_ITEM_W;

constexpr ULONG kPerfEntryBytes = static_cast<ULONG>(sizeof(PerfInfo));

// Length of the trailing window behind every published percentage and rate.
constexpr uint32_t kCpuWindowMs = 250;
// Readings kept for the window: 250 ms at the default 16 ms cadence needs 16. Stored
// readings are at least 1 / (kRingSlots - 1) of the window apart, so a full ring always
// spans it.
constexpr uint32_t kRingSlots = 64;

// Minimum spacing of two "% Processor Performance" collections.
constexpr uint32_t kPerfMinIntervalMs = 100;
// Turbo clocks put the counter above 100 %, so the default cap must be lifted.
constexpr DWORD kPerfFormat = PDH_FMT_DOUBLE | PDH_FMT_NOCAP100;
// English path: localized counter names differ per display language.
constexpr wchar_t kPerfCounterPath[] = L"\\Processor Information(*)\\% Processor Performance";

// Layout of a SYSTEM_LOGICAL_PROCESSOR_INFORMATION_EX record holding a
// NUMA_NODE_RELATIONSHIP. Records are variable-length (Size bytes each), so fields are
// copied out at these offsets instead of being accessed through a structure pointer.
using TopologyRecord = SYSTEM_LOGICAL_PROCESSOR_INFORMATION_EX;
static_assert(sizeof(TopologyRecord::Relationship) == sizeof(DWORD));
constexpr size_t kRecordKindOffset  = offsetof(TopologyRecord, Relationship);
constexpr size_t kRecordSizeOffset  = offsetof(TopologyRecord, Size);
constexpr size_t kRecordHeaderBytes = kRecordSizeOffset + sizeof(DWORD);
constexpr size_t kNumaOffset        = offsetof(TopologyRecord, NumaNode);
constexpr size_t kNodeNumberOffset  = kNumaOffset + offsetof(NUMA_NODE_RELATIONSHIP, NodeNumber);
constexpr size_t kGroupCountOffset  = kNumaOffset + offsetof(NUMA_NODE_RELATIONSHIP, GroupCount);
constexpr size_t kGroupMasksOffset  = kNumaOffset + offsetof(NUMA_NODE_RELATIONSHIP, GroupMasks);
// Size probe plus query, repeated while processors are added between the two calls.
constexpr int kTopologyAttempts = 4;
// NUMA node numbers are USHORT throughout the Win32 NUMA API; larger values are
// treated as a malformed record.
constexpr DWORD kMaxNumaNode = 0xFFFF;

// Output element of CallNtPowerInformation(ProcessorInformation), one per logical
// processor. The user-mode SDK does not declare this structure (it is a WDK type),
// so the documented layout is mirrored here.
struct ProcessorPowerInformation {
    ULONG Number;
    ULONG MaxMhz;
    ULONG CurrentMhz;
    ULONG MhzLimit;
    ULONG MaxIdleState;
    ULONG CurrentIdleState;
};
static_assert(sizeof(ProcessorPowerInformation) == 24);

// Difference between two cumulative 100 ns counters. A decrease (counter reset after
// a processor was re-initialised) is reported as zero rather than as a huge value.
uint64_t time_delta(const LARGE_INTEGER& now, const LARGE_INTEGER& prev) {
    if (now.QuadPart <= prev.QuadPart) {
        return 0;
    }
    return static_cast<uint64_t>(now.QuadPart) - static_cast<uint64_t>(prev.QuadPart);
}

float clamp_percent(double value) {
    return static_cast<float>(std::clamp(value, 0.0, 100.0));
}

// `part` as a percentage of `whole`, clamped to 0..100. `whole` must be non-zero.
float percent(uint64_t part, uint64_t whole) {
    return clamp_percent(static_cast<double>(part) * 100.0 / static_cast<double>(whole));
}

// Rounds to the nearest 32-bit unsigned value for the TelCpuCore fields. NaN and
// negative values map to zero, values beyond the field's range saturate.
uint32_t round_to_u32(double value) {
    if (!(value > 0.0)) {
        return 0;
    }
    if (value >= 4294967295.0) {
        return UINT32_MAX;
    }
    return static_cast<uint32_t>(value + 0.5);
}

uint64_t ms_to_qpc_ticks(uint32_t ms) {
    return qpc_frequency() * ms / 1000u;
}

// Reads a decimal number of one to five digits at `p` and advances `p` past it.
bool parse_index(const wchar_t*& p, uint32_t& value) {
    uint32_t result = 0;
    uint32_t digits = 0;
    while (*p >= L'0' && *p <= L'9') {
        if (++digits > 5) {
            return false;
        }
        result = result * 10u + static_cast<uint32_t>(*p - L'0');
        ++p;
    }
    value = result;
    return digits != 0;
}

// Splits a Processor Information instance name of the form "<node>,<index>", where
// <node> is the NUMA node number and <index> the zero-based index of the processor
// within that node. Totals ("_Total", "<node>,_Total") and every other form are
// rejected.
bool parse_instance(const wchar_t* name, uint32_t& node, uint32_t& index) {
    if (name == nullptr) {
        return false;
    }
    const wchar_t* p = name;
    if (!parse_index(p, node) || *p != L',') {
        return false;
    }
    ++p;
    return parse_index(p, index) && *p == L'\0';
}

// Reads every `relation` record into `buffer`, sized by a probe call with no buffer.
// On success `bytes` is the number of bytes written; on failure `error` holds the
// Win32 error of the last call. Growing `buffer` may throw std::bad_alloc.
bool query_topology(LOGICAL_PROCESSOR_RELATIONSHIP relation, std::vector<std::byte>& buffer,
                    DWORD& bytes, DWORD& error) {
    buffer.clear();
    for (int attempt = 0; attempt < kTopologyAttempts; ++attempt) {
        const DWORD capacity = static_cast<DWORD>(buffer.size());
        DWORD length = capacity;
        auto* const records = buffer.empty()
            ? nullptr
            : reinterpret_cast<SYSTEM_LOGICAL_PROCESSOR_INFORMATION_EX*>(buffer.data());
        if (GetLogicalProcessorInformationEx(relation, records, &length)) {
            bytes = std::min(length, capacity);
            return true;
        }
        error = GetLastError();
        if (error != ERROR_INSUFFICIENT_BUFFER || length <= capacity) {
            return false;
        }
        buffer.resize(length);
    }
    return false;
}

}  // namespace

struct CpuSampler::Impl {
    Impl() = default;
    ~Impl();
    Impl(const Impl&) = delete;
    Impl& operator=(const Impl&) = delete;

    uint32_t core_count  = 0;      // logical processors exposed in TelCpu (<= TEL_MAX_CORES)
    uint32_t group_count = 0;      // active processor groups on the machine
    bool     per_group   = false;  // read each group through NtQuerySystemInformationEx

    // Prefix sums of the per-group entry counts inside `prev`/`curr`: group g occupies
    // [group_offset[g], group_offset[g + 1]). The counts are not clamped to
    // TEL_MAX_CORES so every group query has room for its complete result.
    std::vector<uint32_t> group_offset;

    // `curr` receives each new reading; `prev` holds the reading before it and supplies
    // the entries the kernel did not fill.
    std::vector<PerfInfo> prev;
    std::vector<PerfInfo> curr;

    // Window starts: the last kRingSlots stored readings of the published cores. Slot s
    // holds cores [s * core_count, (s + 1) * core_count) and was taken at ring_qpc[s].
    std::vector<PerfInfo> ring;
    std::vector<uint64_t> ring_qpc;
    uint32_t ring_newest       = 0;  // slot of the most recent stored reading
    uint32_t ring_used         = 0;  // valid slots, 1..kRingSlots once init() succeeded
    uint64_t window_ticks      = 0;  // kCpuWindowMs in QPC ticks
    uint64_t min_spacing_ticks = 0;  // smallest gap between two stored readings

    // Most recently published per-core values. Cores whose window carried no data
    // republish these instead of dividing by zero.
    std::vector<TelCpuCore> last;

    // Effective clock sources.
    std::vector<ProcessorPowerInformation> power;  // CallNtPowerInformation buffer
    std::vector<uint32_t> nominal_mhz;             // MaxMhz per core, 0 until known
    bool nominal_known = false;
    // Published core of every "% Processor Performance" instance: node_cores[n][i] is
    // the core index for instance "n,i", or UINT32_MAX for a processor that is not
    // published. Nodes without processors stay empty. Empty when the NUMA topology
    // could not be read.
    std::vector<std::vector<uint32_t>> node_cores;
    PDH_HQUERY   perf_query   = nullptr;           // null when PDH is unavailable
    PDH_HCOUNTER perf_counter = nullptr;           // owned by perf_query
    std::vector<PerfItem> perf_items;              // reusable PDH result buffer
    uint64_t perf_collect_qpc = 0;                 // time of the last collection
    uint64_t perf_min_ticks   = 0;                 // kPerfMinIntervalMs in QPC ticks
    std::vector<uint32_t> frequency_mhz;           // published clocks, 0 if unknown

    // Reads every sampled group into `dst`. Entries the kernel did not fill (a
    // processor went offline) are copied from `fallback` so their deltas are zero.
    bool read_counters(std::vector<PerfInfo>& dst, const std::vector<PerfInfo>& fallback);

    // Stores the published cores of `reading`, overwriting the oldest slot when full.
    // A reading less than min_spacing_ticks newer than the newest stored one is not
    // stored and the ring is left unchanged.
    void push_reading(const std::vector<PerfInfo>& reading, uint64_t qpc);

    // Slot of the newest stored reading at least window_ticks older than `now`, or the
    // oldest stored reading when none is that old. Requires ring_used >= 1.
    uint32_t window_start(uint64_t now) const;

    // Reads the per-core nominal clock. A failed query leaves nominal_known false so a
    // later refresh retries it.
    void query_nominal();

    // Opens the "% Processor Performance" query and takes its baseline collection. On
    // failure the query stays closed and every clock remains 0.
    void open_perf_query();
    void close_perf_query();

    // Grows perf_items to at least `bytes`. False when memory is exhausted.
    bool reserve_perf_items(DWORD bytes);

    // Collects the counter and recomputes frequency_mhz. Skipped, keeping the current
    // values, while the previous collection is less than kPerfMinIntervalMs old.
    void refresh_frequency(uint64_t now);

    // Builds node_cores from the NUMA topology. Requires group_offset and core_count.
    // Any failure leaves node_cores empty.
    void map_numa_nodes();

    // Appends the processors of every `relation` record in `records` to node_cores.
    // False when a record is malformed. Growing node_cores may throw std::bad_alloc.
    bool add_numa_records(const std::byte* records, size_t bytes,
                          LOGICAL_PROCESSOR_RELATIONSHIP relation);

    // Maps a counter instance name to the index of a published core. False for totals,
    // unparsable names and processors that are not published.
    bool core_from_instance(const wchar_t* name, uint32_t& core) const;
};

CpuSampler::Impl::~Impl() {
    close_perf_query();
}

bool CpuSampler::Impl::read_counters(std::vector<PerfInfo>& dst,
                                     const std::vector<PerfInfo>& fallback) {
    const size_t groups = group_offset.size() - 1;
    for (size_t g = 0; g < groups; ++g) {
        const uint32_t first = group_offset[g];
        const uint32_t room  = group_offset[g + 1] - first;
        if (room == 0) {
            continue;
        }

        PerfInfo* const target = dst.data() + first;
        const ULONG bytes = room * kPerfEntryBytes;
        ULONG returned = 0;

        if (per_group) {
            USHORT group = static_cast<USHORT>(g);
            const nt::NTSTATUS status = g_nt.query_ex(
                nt::SystemProcessorPerformanceInformation,
                &group, static_cast<ULONG>(sizeof(group)),
                target, bytes, &returned);
            if (!nt::success(status)) {
                set_nt_error("NtQuerySystemInformationEx(SystemProcessorPerformanceInformation)", status);
                return false;
            }
        } else {
            const nt::NTSTATUS status = g_nt.query(
                nt::SystemProcessorPerformanceInformation, target, bytes, &returned);
            if (!nt::success(status)) {
                set_nt_error("NtQuerySystemInformation(SystemProcessorPerformanceInformation)", status);
                return false;
            }
        }

        const uint32_t filled = std::min(returned, bytes) / kPerfEntryBytes;
        for (uint32_t i = first + filled; i < first + room; ++i) {
            dst[i] = fallback[i];
        }
    }
    return true;
}

void CpuSampler::Impl::push_reading(const std::vector<PerfInfo>& reading, uint64_t qpc) {
    if (ring_used != 0) {
        const uint64_t newest = ring_qpc[ring_newest];
        if (qpc > newest && qpc - newest < min_spacing_ticks) {
            return;
        }
    }
    const uint32_t slot = ring_used == 0 ? 0u : (ring_newest + 1u) % kRingSlots;
    std::copy_n(reading.data(), core_count, ring.data() + static_cast<size_t>(slot) * core_count);
    ring_qpc[slot] = qpc;
    ring_newest = slot;
    if (ring_used < kRingSlots) {
        ++ring_used;
    }
}

uint32_t CpuSampler::Impl::window_start(uint64_t now) const {
    // Walks from the newest reading towards older ones; after ring_used - 1 steps
    // without a match `slot` is the oldest stored reading.
    uint32_t slot = ring_newest;
    for (uint32_t step = 1; step < ring_used; ++step) {
        const uint64_t taken = ring_qpc[slot];
        if (now > taken && now - taken >= window_ticks) {
            break;
        }
        slot = slot == 0 ? kRingSlots - 1u : slot - 1u;
    }
    return slot;
}

void CpuSampler::Impl::query_nominal() {
    if (power.empty()) {
        return;
    }
    const ULONG bytes = static_cast<ULONG>(power.size() * sizeof(ProcessorPowerInformation));
    const nt::NTSTATUS status =
        CallNtPowerInformation(ProcessorInformation, nullptr, 0, power.data(), bytes);
    if (!nt::success(status)) {
        return;
    }
    const size_t n = std::min(nominal_mhz.size(), power.size());
    for (size_t i = 0; i < n; ++i) {
        nominal_mhz[i] = static_cast<uint32_t>(power[i].MaxMhz);
    }
    nominal_known = true;
}

void CpuSampler::Impl::open_perf_query() {
    if (PdhOpenQueryW(nullptr, 0, &perf_query) != ERROR_SUCCESS) {
        perf_query = nullptr;
        return;
    }
    if (PdhAddEnglishCounterW(perf_query, kPerfCounterPath, 0, &perf_counter) != ERROR_SUCCESS ||
        PdhCollectQueryData(perf_query) != ERROR_SUCCESS) {
        close_perf_query();
        return;
    }
    perf_collect_qpc = qpc_now();

    // Size the result buffer now so refreshes reuse it. One collection is not enough
    // for a rate, but the size probe already accounts for every instance. A failure
    // here is retried by refresh_frequency.
    DWORD bytes = 0;
    DWORD count = 0;
    const PDH_STATUS status =
        PdhGetFormattedCounterArrayW(perf_counter, kPerfFormat, &bytes, &count, nullptr);
    if (static_cast<DWORD>(status) == PDH_MORE_DATA) {
        (void)reserve_perf_items(bytes);
    }
}

void CpuSampler::Impl::close_perf_query() {
    if (perf_query != nullptr) {
        PdhCloseQuery(perf_query);  // also releases perf_counter
        perf_query = nullptr;
    }
    perf_counter = nullptr;
}

bool CpuSampler::Impl::reserve_perf_items(DWORD bytes) {
    constexpr size_t kItemBytes = sizeof(PerfItem);
    const size_t items = (static_cast<size_t>(bytes) + kItemBytes - 1) / kItemBytes;
    if (items <= perf_items.size()) {
        return true;
    }
    try {
        perf_items.resize(items);
    } catch (...) {
        return false;
    }
    return true;
}

void CpuSampler::Impl::map_numa_nodes() {
    node_cores.clear();
    try {
        std::vector<std::byte> buffer;
        DWORD bytes = 0;
        DWORD error = ERROR_SUCCESS;
        // RelationNumaNodeEx reports every group of a node; kernels that predate it
        // reject it with ERROR_INVALID_PARAMETER and only offer RelationNumaNode.
        LOGICAL_PROCESSOR_RELATIONSHIP relation = RelationNumaNodeEx;
        bool read = query_topology(relation, buffer, bytes, error);
        if (!read && error == ERROR_INVALID_PARAMETER) {
            relation = RelationNumaNode;
            read = query_topology(relation, buffer, bytes, error);
        }
        if (!read || !add_numa_records(buffer.data(), bytes, relation)) {
            node_cores.clear();
        }
    } catch (...) {
        node_cores.clear();
    }
}

bool CpuSampler::Impl::add_numa_records(const std::byte* records, size_t bytes,
                                        LOGICAL_PROCESSOR_RELATIONSHIP relation) {
    std::vector<GROUP_AFFINITY> masks;
    size_t pos = 0;
    while (pos < bytes) {
        if (bytes - pos < kRecordHeaderBytes) {
            return false;
        }
        const std::byte* const record = records + pos;
        DWORD kind = 0;
        DWORD size = 0;
        std::memcpy(&kind, record + kRecordKindOffset, sizeof(kind));
        std::memcpy(&size, record + kRecordSizeOffset, sizeof(size));
        if (size < kRecordHeaderBytes || size > bytes - pos) {
            return false;
        }
        pos += size;
        // Both queries tag their output records RelationNumaNode; RelationNumaNodeEx is
        // an input selector only. It is accepted too in case a build echoes it.
        if (kind != static_cast<DWORD>(RelationNumaNode) &&
            kind != static_cast<DWORD>(RelationNumaNodeEx)) {
            continue;
        }
        if (size < kGroupMasksOffset) {
            return false;
        }

        DWORD node = 0;
        WORD declared = 0;
        std::memcpy(&node, record + kNodeNumberOffset, sizeof(node));
        std::memcpy(&declared, record + kGroupCountOffset, sizeof(declared));
        if (node > kMaxNumaNode) {
            return false;
        }
        // RelationNumaNode carries a single GroupMask; kernels that predate GroupCount
        // leave it zero.
        size_t count = declared;
        if (relation == RelationNumaNode && count == 0) {
            count = 1;
        }
        count = std::min(count, (size - kGroupMasksOffset) / sizeof(GROUP_AFFINITY));

        masks.clear();
        for (size_t m = 0; m < count; ++m) {
            GROUP_AFFINITY mask{};
            std::memcpy(&mask, record + kGroupMasksOffset + m * sizeof(GROUP_AFFINITY), sizeof(mask));
            masks.push_back(mask);
        }
        std::sort(masks.begin(), masks.end(),
                  [](const GROUP_AFFINITY& a, const GROUP_AFFINITY& b) { return a.Group < b.Group; });

        if (node >= node_cores.size()) {
            node_cores.resize(static_cast<size_t>(node) + 1);
        }
        std::vector<uint32_t>& cores = node_cores[node];
        for (const GROUP_AFFINITY& mask : masks) {
            const size_t g = mask.Group;
            uint64_t bits = static_cast<uint64_t>(mask.Mask);
            while (bits != 0) {
                const uint32_t b = static_cast<uint32_t>(std::countr_zero(bits));
                bits &= bits - 1;
                uint32_t index = UINT32_MAX;
                if (g + 1 < group_offset.size()) {
                    const uint32_t first = group_offset[g];
                    if (b < group_offset[g + 1] - first && first + b < core_count) {
                        index = first + b;
                    }
                }
                cores.push_back(index);
            }
        }
    }
    return true;
}

bool CpuSampler::Impl::core_from_instance(const wchar_t* name, uint32_t& core) const {
    uint32_t node  = 0;
    uint32_t index = 0;
    if (!parse_instance(name, node, index)) {
        return false;
    }
    uint32_t mapped = UINT32_MAX;
    if (!node_cores.empty()) {
        if (node >= node_cores.size() || index >= node_cores[node].size()) {
            return false;
        }
        mapped = node_cores[node][index];
    } else if (group_count == 1 && node == 0) {
        // Without the topology the order within a node is only known for a single
        // group, where it is the processor number.
        mapped = index;
    } else {
        return false;
    }
    if (mapped >= core_count) {
        return false;
    }
    core = mapped;
    return true;
}

void CpuSampler::Impl::refresh_frequency(uint64_t now) {
    if (!nominal_known) {
        query_nominal();
    }
    if (perf_counter == nullptr) {
        return;
    }
    const uint64_t since = now > perf_collect_qpc ? now - perf_collect_qpc : 0;
    if (since < perf_min_ticks) {
        return;
    }

    if (PdhCollectQueryData(perf_query) != ERROR_SUCCESS) {
        std::fill(frequency_mhz.begin(), frequency_mhz.end(), 0u);
        return;
    }
    perf_collect_qpc = now;

    DWORD bytes = static_cast<DWORD>(perf_items.size() * sizeof(PerfItem));
    DWORD count = 0;
    PDH_STATUS status =
        PdhGetFormattedCounterArrayW(perf_counter, kPerfFormat, &bytes, &count, perf_items.data());
    if (static_cast<DWORD>(status) == PDH_MORE_DATA) {
        // The size reported for a non-empty buffer that was too small is not reliable,
        // so the requirement is queried again with an empty one.
        bytes = 0;
        count = 0;
        status = PdhGetFormattedCounterArrayW(perf_counter, kPerfFormat, &bytes, &count, nullptr);
        if (static_cast<DWORD>(status) == PDH_MORE_DATA && reserve_perf_items(bytes)) {
            bytes = static_cast<DWORD>(perf_items.size() * sizeof(PerfItem));
            count = 0;
            status = PdhGetFormattedCounterArrayW(perf_counter, kPerfFormat, &bytes, &count,
                                                  perf_items.data());
        }
    }

    // Processors without a valid reading in this interval are reported as unknown.
    std::fill(frequency_mhz.begin(), frequency_mhz.end(), 0u);
    if (status != ERROR_SUCCESS) {
        return;
    }
    const size_t items = std::min(static_cast<size_t>(count), perf_items.size());
    for (size_t k = 0; k < items; ++k) {
        const PerfItem& item = perf_items[k];
        const DWORD item_status = item.FmtValue.CStatus;
        if (item_status != PDH_CSTATUS_VALID_DATA && item_status != PDH_CSTATUS_NEW_DATA) {
            continue;
        }
        uint32_t core = 0;
        if (!core_from_instance(item.szName, core)) {
            continue;
        }
        frequency_mhz[core] = round_to_u32(static_cast<double>(nominal_mhz[core]) *
                                           item.FmtValue.doubleValue / 100.0);
    }
}

CpuSampler::CpuSampler() = default;

CpuSampler::~CpuSampler() {
    shutdown();
}

bool CpuSampler::init() {
    shutdown();

    if (g_nt.query == nullptr) {
        set_error("CpuSampler::init: NtQuerySystemInformation is not resolved");
        return false;
    }

    const WORD groups = GetActiveProcessorGroupCount();
    if (groups == 0) {
        set_win32_error("GetActiveProcessorGroupCount", GetLastError());
        return false;
    }
    const DWORD total = GetActiveProcessorCount(ALL_PROCESSOR_GROUPS);
    if (total == 0) {
        set_win32_error("GetActiveProcessorCount", GetLastError());
        return false;
    }

    try {
        auto impl = std::make_unique<Impl>();
        impl->group_count = groups;
        impl->per_group   = groups > 1 && g_nt.query_ex != nullptr;

        // Logical processors whose counters are published, before the TEL_MAX_CORES clamp.
        uint32_t reported = static_cast<uint32_t>(total);

        impl->group_offset.push_back(0);
        if (impl->per_group) {
            for (WORD g = 0; g < groups; ++g) {
                // Zero for a group without active processors; that group is skipped.
                const DWORD in_group = GetActiveProcessorCount(g);
                impl->group_offset.push_back(impl->group_offset.back() + static_cast<uint32_t>(in_group));
            }
        } else if (groups > 1) {
            // Without the Ex entry point only one group is reachable, and
            // NtQuerySystemInformation answers for the calling thread's group, which is
            // not necessarily group 0. The buffer is sized for the largest group so the
            // query always has room; the published cores are limited to group 0's count.
            DWORD largest = 0;
            for (WORD g = 0; g < groups; ++g) {
                largest = std::max(largest, GetActiveProcessorCount(g));
            }
            impl->group_offset.push_back(static_cast<uint32_t>(largest));
            reported = static_cast<uint32_t>(GetActiveProcessorCount(0));
        } else {
            impl->group_offset.push_back(static_cast<uint32_t>(total));
        }

        const uint32_t buffered = impl->group_offset.back();
        impl->core_count = std::min({reported, buffered, TEL_MAX_CORES});
        if (impl->core_count == 0) {
            set_error("CpuSampler::init: no active logical processors found");
            return false;
        }

        // CallNtPowerInformation fails with STATUS_BUFFER_TOO_SMALL unless the buffer
        // covers every processor on the machine, so this buffer is not clamped to
        // TEL_MAX_CORES and also covers processors that may be hot-added later.
        const DWORD max_processors = GetMaximumProcessorCount(ALL_PROCESSOR_GROUPS);
        const size_t power_entries = std::max(static_cast<size_t>(total), static_cast<size_t>(max_processors));

        impl->prev.assign(buffered, PerfInfo{});
        impl->curr.assign(buffered, PerfInfo{});
        impl->ring.assign(static_cast<size_t>(kRingSlots) * impl->core_count, PerfInfo{});
        impl->ring_qpc.assign(kRingSlots, 0);
        impl->last.assign(impl->core_count, TelCpuCore{});
        impl->power.assign(power_entries, ProcessorPowerInformation{});
        impl->nominal_mhz.assign(impl->core_count, 0u);
        impl->frequency_mhz.assign(impl->core_count, 0u);
        impl->window_ticks = ms_to_qpc_ticks(kCpuWindowMs);
        // Rounded up so kRingSlots - 1 gaps of this length cover the whole window.
        impl->min_spacing_ticks = (impl->window_ticks + kRingSlots - 2) / (kRingSlots - 1);
        impl->perf_min_ticks = ms_to_qpc_ticks(kPerfMinIntervalMs);

        // Baseline reading. `curr` is still zero, so an unfilled entry starts at zero.
        if (!impl->read_counters(impl->prev, impl->curr)) {
            return false;
        }
        impl->push_reading(impl->prev, qpc_now());

        impl->query_nominal();
        impl->map_numa_nodes();
        impl->open_perf_query();

        impl_ = std::move(impl);
        return true;
    } catch (...) {
        set_error("CpuSampler::init: out of memory");
        return false;
    }
}

void CpuSampler::shutdown() {
    impl_.reset();
}

bool CpuSampler::sample(TelCpu& out, bool refresh_frequency) {
    Impl* const s = impl_.get();
    if (s == nullptr) {
        set_error("CpuSampler::sample: not initialized");
        return false;
    }

    // Every call that can fail happens before `out` is touched.
    if (!s->read_counters(s->curr, s->prev)) {
        return false;
    }
    const uint64_t now = qpc_now();
    if (refresh_frequency) {
        s->refresh_frequency(now);
    }

    const uint32_t n     = s->core_count;
    const uint32_t start = s->window_start(now);
    const PerfInfo* const base = s->ring.data() + static_cast<size_t>(start) * n;

    // Window length for the per-second rates. Zero when the clock did not advance;
    // the rates then keep their previous values.
    const uint64_t start_qpc = s->ring_qpc[start];
    const uint64_t qpc_hz    = qpc_frequency();
    const uint64_t ticks     = now > start_qpc ? now - start_qpc : 0;
    const double dt_seconds =
        (qpc_hz != 0 && ticks != 0) ? static_cast<double>(ticks) / static_cast<double>(qpc_hz) : 0.0;

    double   sum_utilization   = 0.0;
    double   sum_kernel        = 0.0;
    double   sum_user          = 0.0;
    double   sum_dpc_interrupt = 0.0;
    uint64_t sum_interrupts    = 0;

    for (uint32_t i = 0; i < n; ++i) {
        const PerfInfo& c = s->curr[i];
        const PerfInfo& p = base[i];
        TelCpuCore& core  = s->last[i];

        const uint64_t d_idle      = time_delta(c.IdleTime, p.IdleTime);
        const uint64_t d_kernel    = time_delta(c.KernelTime, p.KernelTime);
        const uint64_t d_user      = time_delta(c.UserTime, p.UserTime);
        const uint64_t d_dpc       = time_delta(c.DpcTime, p.DpcTime);
        const uint64_t d_interrupt = time_delta(c.InterruptTime, p.InterruptTime);
        const uint64_t total       = d_kernel + d_user;

        // A window with no elapsed processor time (duplicate reading or a parked core
        // whose counters stopped) republishes the previous percentages.
        if (total != 0) {
            const uint64_t busy        = total > d_idle ? total - d_idle : 0;
            const uint64_t kernel_busy = d_kernel > d_idle ? d_kernel - d_idle : 0;
            core.utilization   = percent(busy, total);
            core.kernel        = percent(kernel_busy, total);
            core.user          = percent(d_user, total);
            core.dpc_interrupt = percent(d_dpc + d_interrupt, total);
        }

        if (dt_seconds > 0.0) {
            // Unsigned 32-bit subtraction handles wrap-around of the ULONG counter.
            const uint32_t d_count = static_cast<uint32_t>(c.InterruptCount - p.InterruptCount);
            core.interrupts_per_sec = round_to_u32(static_cast<double>(d_count) / dt_seconds);
        }
        core.frequency_mhz = s->frequency_mhz[i];

        sum_utilization   += core.utilization;
        sum_kernel        += core.kernel;
        sum_user          += core.user;
        sum_dpc_interrupt += core.dpc_interrupt;
        sum_interrupts    += core.interrupts_per_sec;

        out.cores[i] = core;
    }
    for (uint32_t i = n; i < TEL_MAX_CORES; ++i) {
        out.cores[i] = TelCpuCore{};
    }

    out.core_count  = n;
    out.group_count = s->group_count;

    const double inverse_count = n != 0 ? 1.0 / static_cast<double>(n) : 0.0;
    out.total_utilization   = clamp_percent(sum_utilization * inverse_count);
    out.total_kernel        = clamp_percent(sum_kernel * inverse_count);
    out.total_user          = clamp_percent(sum_user * inverse_count);
    out.total_dpc_interrupt = clamp_percent(sum_dpc_interrupt * inverse_count);
    out.interrupts_per_sec  = sum_interrupts;
    // context_switches_per_sec and syscalls_per_sec are owned by the memory sampler's
    // SystemRates and are filled by the caller.

    // The values above already used `curr`; push_reading may decline to store it.
    s->push_reading(s->curr, now);
    std::swap(s->prev, s->curr);
    return true;
}

uint32_t CpuSampler::core_count() const {
    return impl_ ? impl_->core_count : 0u;
}

}  // namespace tel
