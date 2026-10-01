// Private NTDLL declarations used by the samplers. These mirror the kernel's
// information-class structures for 64-bit Windows 10/11. winternl.h is deliberately
// not included: it declares partial versions of the same structures under the same
// names.
#pragma once

#include <windows.h>

#include <cstddef>
#include <cstdint>

namespace tel::nt {

using NTSTATUS = LONG;

constexpr NTSTATUS STATUS_SUCCESS              = 0x00000000L;
constexpr NTSTATUS STATUS_INFO_LENGTH_MISMATCH = static_cast<NTSTATUS>(0xC0000004L);
constexpr NTSTATUS STATUS_BUFFER_TOO_SMALL     = static_cast<NTSTATUS>(0xC0000023L);

inline bool success(NTSTATUS s) { return s >= 0; }

// SYSTEM_INFORMATION_CLASS values.
constexpr ULONG SystemBasicInformation                = 0;
constexpr ULONG SystemPerformanceInformation          = 2;
constexpr ULONG SystemTimeOfDayInformation            = 3;
constexpr ULONG SystemProcessInformation              = 5;
constexpr ULONG SystemProcessorPerformanceInformation = 8;
constexpr ULONG SystemInterruptInformation            = 23;

using PFN_NtQuerySystemInformation = NTSTATUS(NTAPI*)(
    ULONG SystemInformationClass,
    PVOID SystemInformation,
    ULONG SystemInformationLength,
    PULONG ReturnLength);

// Windows 7+. InputBuffer selects a processor group (USHORT) for per-processor classes.
using PFN_NtQuerySystemInformationEx = NTSTATUS(NTAPI*)(
    ULONG SystemInformationClass,
    PVOID InputBuffer,
    ULONG InputBufferLength,
    PVOID SystemInformation,
    ULONG SystemInformationLength,
    PULONG ReturnLength);

struct UNICODE_STRING {
    USHORT Length;         // bytes, excluding terminator
    USHORT MaximumLength;  // bytes
    PWSTR  Buffer;
};

struct CLIENT_ID {
    HANDLE UniqueProcess;
    HANDLE UniqueThread;
};

struct SYSTEM_BASIC_INFORMATION {
    ULONG     Reserved;
    ULONG     TimerResolution;
    ULONG     PageSize;
    ULONG     NumberOfPhysicalPages;
    ULONG     LowestPhysicalPageNumber;
    ULONG     HighestPhysicalPageNumber;
    ULONG     AllocationGranularity;
    ULONG_PTR MinimumUserModeAddress;
    ULONG_PTR MaximumUserModeAddress;
    ULONG_PTR ActiveProcessorsAffinityMask;
    CCHAR     NumberOfProcessors;
};

struct SYSTEM_TIMEOFDAY_INFORMATION {
    LARGE_INTEGER BootTime;
    LARGE_INTEGER CurrentTime;
    LARGE_INTEGER TimeZoneBias;
    ULONG         TimeZoneId;
    ULONG         Reserved;
    ULONGLONG     BootTimeBias;
    ULONGLONG     SleepTimeBias;
};

// One entry per logical processor in the queried group. Times are 100 ns units.
// KernelTime includes IdleTime, DpcTime and InterruptTime.
struct SYSTEM_PROCESSOR_PERFORMANCE_INFORMATION {
    LARGE_INTEGER IdleTime;
    LARGE_INTEGER KernelTime;
    LARGE_INTEGER UserTime;
    LARGE_INTEGER DpcTime;
    LARGE_INTEGER InterruptTime;
    ULONG         InterruptCount;
    ULONG         Reserved;
};
static_assert(sizeof(SYSTEM_PROCESSOR_PERFORMANCE_INFORMATION) == 48);

// Stable prefix of SYSTEM_PERFORMANCE_INFORMATION. The kernel appends fields in newer
// releases; callers pass a larger buffer and read only this prefix. Counts are in pages
// unless the name says otherwise.
struct SYSTEM_PERFORMANCE_INFORMATION {
    LARGE_INTEGER IdleProcessTime;
    LARGE_INTEGER IoReadTransferCount;
    LARGE_INTEGER IoWriteTransferCount;
    LARGE_INTEGER IoOtherTransferCount;
    ULONG IoReadOperationCount;
    ULONG IoWriteOperationCount;
    ULONG IoOtherOperationCount;
    ULONG AvailablePages;
    ULONG CommittedPages;
    ULONG CommitLimit;
    ULONG PeakCommitment;
    ULONG PageFaultCount;
    ULONG CopyOnWriteCount;
    ULONG TransitionCount;
    ULONG CacheTransitionCount;
    ULONG DemandZeroCount;
    ULONG PageReadCount;
    ULONG PageReadIoCount;
    ULONG CacheReadCount;
    ULONG CacheIoCount;
    ULONG DirtyPagesWriteCount;
    ULONG DirtyWriteIoCount;
    ULONG MappedPagesWriteCount;
    ULONG MappedWriteIoCount;
    ULONG PagedPoolPages;
    ULONG NonPagedPoolPages;
    ULONG PagedPoolAllocs;
    ULONG PagedPoolFrees;
    ULONG NonPagedPoolAllocs;
    ULONG NonPagedPoolFrees;
    ULONG FreeSystemPtes;
    ULONG ResidentSystemCodePage;
    ULONG TotalSystemDriverPages;
    ULONG TotalSystemCodePages;
    ULONG NonPagedPoolLookasideHits;
    ULONG PagedPoolLookasideHits;
    ULONG AvailablePagedPoolPages;
    ULONG ResidentSystemCachePage;
    ULONG ResidentPagedPoolPage;
    ULONG ResidentSystemDriverPage;
    ULONG CcFastReadNoWait;
    ULONG CcFastReadWait;
    ULONG CcFastReadResourceMiss;
    ULONG CcFastReadNotPossible;
    ULONG CcFastMdlReadNoWait;
    ULONG CcFastMdlReadWait;
    ULONG CcFastMdlReadResourceMiss;
    ULONG CcFastMdlReadNotPossible;
    ULONG CcMapDataNoWait;
    ULONG CcMapDataWait;
    ULONG CcMapDataNoWaitMiss;
    ULONG CcMapDataWaitMiss;
    ULONG CcPinMappedDataCount;
    ULONG CcPinReadNoWait;
    ULONG CcPinReadWait;
    ULONG CcPinReadNoWaitMiss;
    ULONG CcPinReadWaitMiss;
    ULONG CcCopyReadNoWait;
    ULONG CcCopyReadWait;
    ULONG CcCopyReadNoWaitMiss;
    ULONG CcCopyReadWaitMiss;
    ULONG CcMdlReadNoWait;
    ULONG CcMdlReadWait;
    ULONG CcMdlReadNoWaitMiss;
    ULONG CcMdlReadWaitMiss;
    ULONG CcReadAheadIos;
    ULONG CcLazyWriteIos;
    ULONG CcLazyWritePages;
    ULONG CcDataFlushes;
    ULONG CcDataPages;
    ULONG ContextSwitches;
    ULONG FirstLevelTbFills;
    ULONG SecondLevelTbFills;
    ULONG SystemCalls;
};
static_assert(offsetof(SYSTEM_PERFORMANCE_INFORMATION, PageFaultCount) == 0x3C);
static_assert(offsetof(SYSTEM_PERFORMANCE_INFORMATION, ContextSwitches) == 0x128);
static_assert(offsetof(SYSTEM_PERFORMANCE_INFORMATION, SystemCalls) == 0x134);
static_assert(sizeof(SYSTEM_PERFORMANCE_INFORMATION) == 0x138);
// Generous query size: newer kernels append fields after SystemCalls.
constexpr ULONG SYSTEM_PERFORMANCE_INFORMATION_QUERY_SIZE = 4096;

enum KTHREAD_STATE : ULONG {
    Initialized = 0,
    Ready = 1,
    Running = 2,
    Standby = 3,
    Terminated = 4,
    Waiting = 5,
    Transition = 6,
    DeferredReady = 7,
    GateWaitObsolete = 8,
    WaitingForProcessInSwap = 9
};

struct SYSTEM_THREAD_INFORMATION {
    LARGE_INTEGER KernelTime;
    LARGE_INTEGER UserTime;
    LARGE_INTEGER CreateTime;
    ULONG         WaitTime;
    PVOID         StartAddress;
    CLIENT_ID     ClientId;
    LONG          Priority;
    LONG          BasePriority;
    ULONG         ContextSwitches;
    ULONG         ThreadState;  // KTHREAD_STATE
    ULONG         WaitReason;
};
static_assert(sizeof(SYSTEM_THREAD_INFORMATION) == 80);

struct SYSTEM_PROCESS_INFORMATION {
    ULONG          NextEntryOffset;  // 0 for the last entry
    ULONG          NumberOfThreads;
    LARGE_INTEGER  WorkingSetPrivateSize;
    ULONG          HardFaultCount;
    ULONG          NumberOfThreadsHighWatermark;
    ULONGLONG      CycleTime;
    LARGE_INTEGER  CreateTime;
    LARGE_INTEGER  UserTime;
    LARGE_INTEGER  KernelTime;
    UNICODE_STRING ImageName;
    LONG           BasePriority;
    HANDLE         UniqueProcessId;
    HANDLE         InheritedFromUniqueProcessId;
    ULONG          HandleCount;
    ULONG          SessionId;
    ULONG_PTR      UniqueProcessKey;
    SIZE_T         PeakVirtualSize;
    SIZE_T         VirtualSize;
    ULONG          PageFaultCount;
    SIZE_T         PeakWorkingSetSize;
    SIZE_T         WorkingSetSize;
    SIZE_T         QuotaPeakPagedPoolUsage;
    SIZE_T         QuotaPagedPoolUsage;
    SIZE_T         QuotaPeakNonPagedPoolUsage;
    SIZE_T         QuotaNonPagedPoolUsage;
    SIZE_T         PagefileUsage;
    SIZE_T         PeakPagefileUsage;
    SIZE_T         PrivatePageCount;
    LARGE_INTEGER  ReadOperationCount;
    LARGE_INTEGER  WriteOperationCount;
    LARGE_INTEGER  OtherOperationCount;
    LARGE_INTEGER  ReadTransferCount;
    LARGE_INTEGER  WriteTransferCount;
    LARGE_INTEGER  OtherTransferCount;
    SYSTEM_THREAD_INFORMATION Threads[1];  // NumberOfThreads entries follow
};
static_assert(offsetof(SYSTEM_PROCESS_INFORMATION, ImageName) == 0x38);
static_assert(offsetof(SYSTEM_PROCESS_INFORMATION, UniqueProcessId) == 0x50);
static_assert(offsetof(SYSTEM_PROCESS_INFORMATION, WorkingSetSize) == 0x90);
static_assert(offsetof(SYSTEM_PROCESS_INFORMATION, Threads) == 0x100);

}  // namespace tel::nt
