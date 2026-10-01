//! The speed test's I/O engine: sector-aligned buffers, a queue of overlapped unbuffered
//! reads or writes kept at a fixed depth on one I/O completion port, and the measurement loop.
//!
//! The loop only talks to an [`IoTarget`], so tests drive it with a scripted target in memory.

use std::alloc::{self, Layout};
use std::fmt;
use std::ptr::NonNull;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use windows::Win32::Foundation::{ERROR_IO_PENDING, ERROR_NOT_FOUND, HANDLE};
use windows::Win32::Storage::FileSystem::{ReadFile, WriteFile};
use windows::Win32::System::IO::{
    CancelIoEx, CreateIoCompletionPort, GetOverlappedResult, GetQueuedCompletionStatusEx,
    OVERLAPPED, OVERLAPPED_ENTRY,
};

use crate::storage::hash::random_fill;
use crate::win::handle::OwnedHandle;
use crate::win::is_win32;
use crate::{Error, Result};

/// How long [`IoTarget::drain`] waits for cancelled I/O before it gives up and leaks the
/// buffers the kernel may still write to.
const DRAIN_LIMIT: Duration = Duration::from_secs(30);
/// How long one wait of the measurement loop lasts at most.
const WAIT_STEP: Duration = Duration::from_millis(100);
/// Ends the text of a transfer that failed because the disk is full.
pub(crate) const DISK_FULL_MARK: &str = "(error 112)";

// ───────────────────────────── Buffers ─────────────────────────────

/// A zeroed heap buffer with a chosen alignment, as unbuffered I/O requires.
pub(crate) struct AlignedBuf {
    ptr: NonNull<u8>,
    len: usize,
    layout: Layout,
}

impl fmt::Debug for AlignedBuf {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("AlignedBuf")
            .field("len", &self.len)
            .field("align", &self.layout.align())
            .finish()
    }
}

// SAFETY: the buffer is plain bytes owned by this value; moving it to another thread moves
// the ownership with it.
unsafe impl Send for AlignedBuf {}
// SAFETY: shared access only reads the bytes (`as_slice`); writes need `&mut self` or go
// through the kernel into buffers that are only read after their I/O completed.
unsafe impl Sync for AlignedBuf {}

impl AlignedBuf {
    /// `len` zeroed bytes aligned to `align` (a power of two).
    pub(crate) fn new(len: usize, align: usize) -> Result<AlignedBuf> {
        let layout = Layout::from_size_align(len.max(1), align.max(1))
            .map_err(|e| Error::Other(format!("cannot lay out a {len} byte buffer: {e}")))?;
        // SAFETY: the layout has a non-zero size.
        let ptr = unsafe { alloc::alloc_zeroed(layout) };
        let ptr = NonNull::new(ptr)
            .ok_or_else(|| Error::Other(format!("cannot allocate a {len} byte buffer")))?;
        Ok(AlignedBuf { ptr, len, layout })
    }

    /// `len` random bytes aligned to `align`.
    pub(crate) fn random(len: usize, align: usize) -> Result<AlignedBuf> {
        let mut buf = AlignedBuf::new(len, align)?;
        random_fill(buf.as_mut_slice())?;
        Ok(buf)
    }

    pub(crate) fn len(&self) -> usize {
        self.len
    }

    pub(crate) fn as_ptr(&self) -> *mut u8 {
        self.ptr.as_ptr()
    }

    #[cfg(test)]
    pub(crate) fn as_slice(&self) -> &[u8] {
        // SAFETY: the allocation holds `len` initialized (zeroed or written) bytes for the
        // lifetime of `self`.
        unsafe { std::slice::from_raw_parts(self.ptr.as_ptr(), self.len) }
    }

    pub(crate) fn as_mut_slice(&mut self) -> &mut [u8] {
        // SAFETY: as above, and `&mut self` makes this the only reference.
        unsafe { std::slice::from_raw_parts_mut(self.ptr.as_ptr(), self.len) }
    }
}

impl Drop for AlignedBuf {
    fn drop(&mut self) {
        // SAFETY: allocated in `new` with exactly this layout and freed once, here.
        unsafe { alloc::dealloc(self.ptr.as_ptr(), self.layout) }
    }
}

/// xorshift64* generator for offsets; seeded from the system's random source.
#[derive(Debug, Clone)]
pub(crate) struct Rng(u64);

impl Rng {
    pub(crate) fn seeded(seed: u64) -> Rng {
        Rng(if seed == 0 {
            0x9E37_79B9_7F4A_7C15
        } else {
            seed
        })
    }

    pub(crate) fn from_system() -> Result<Rng> {
        let mut bytes = [0u8; 8];
        random_fill(&mut bytes)?;
        Ok(Rng::seeded(u64::from_le_bytes(bytes)))
    }

    pub(crate) fn next_u64(&mut self) -> u64 {
        let mut x = self.0;
        x ^= x >> 12;
        x ^= x << 25;
        x ^= x >> 27;
        self.0 = x;
        x.wrapping_mul(0x2545_F491_4F6C_DD1D)
    }

    /// A value in `0..n` (0 when `n` is 0).
    pub(crate) fn below(&mut self, n: u64) -> u64 {
        ((u128::from(self.next_u64()) * u128::from(n)) >> 64) as u64
    }
}

// ───────────────────────────── Target ─────────────────────────────

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum IoOp {
    Read,
    Write,
}

/// One finished I/O.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Completion {
    pub slot: usize,
    pub bytes: u32,
    pub error: Option<String>,
}

/// Where the measurement loop sends its I/O.
pub(crate) trait IoTarget {
    /// Number of I/Os that can be outstanding at once.
    fn slots(&self) -> usize;
    /// Queues one I/O on `slot`: a read lands in the slot's buffer; a write sends
    /// `pool[pool_offset..pool_offset + len]`.
    fn submit(
        &mut self,
        slot: usize,
        op: IoOp,
        offset: u64,
        len: u32,
        pool_offset: usize,
    ) -> Result<()>;
    /// Waits up to `timeout` and appends the I/Os that finished.
    fn wait(&mut self, timeout: Duration, out: &mut Vec<Completion>) -> Result<()>;
    fn outstanding(&self) -> usize;
    /// Cancels the outstanding I/O, then waits until nothing is outstanding.
    fn drain(&mut self) -> Result<()>;
}

/// One queued I/O; the OVERLAPPED comes first so a completion's pointer identifies it.
#[repr(C)]
struct Slot {
    ov: OVERLAPPED,
    busy: bool,
}

/// Overlapped unbuffered I/O on an open file through a completion port of its own.
///
/// Owns the file handle: dropping the target drains the I/O and closes the file (a file
/// opened with FILE_FLAG_DELETE_ON_CLOSE is deleted then). Memory the kernel may still
/// write to is never freed: when cancelled I/O does not finish within 30 s, the slots and
/// buffers are leaked instead.
pub(crate) struct OverlappedTarget {
    file: Option<OwnedHandle>,
    port: Option<OwnedHandle>,
    /// Allocated once in `new` as a boxed slice, which cannot grow, so every OVERLAPPED keeps
    /// its address while I/O is pending.
    slots: Box<[Slot]>,
    reads: Option<AlignedBuf>,
    pool: Option<Arc<AlignedBuf>>,
    outstanding: usize,
    entries: Vec<OVERLAPPED_ENTRY>,
}

impl fmt::Debug for OverlappedTarget {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("OverlappedTarget")
            .field("slots", &self.slots.len())
            .field("outstanding", &self.outstanding)
            .finish_non_exhaustive()
    }
}

impl OverlappedTarget {
    /// Binds `file` (opened with FILE_FLAG_OVERLAPPED) to a new completion port with `slots`
    /// I/O slots, a read buffer of `read_len` bytes aligned to `align`, and the write data
    /// `pool`.
    pub(crate) fn new(
        file: OwnedHandle,
        slots: usize,
        read_len: usize,
        align: usize,
        pool: Arc<AlignedBuf>,
    ) -> Result<OverlappedTarget> {
        // SAFETY: `file` is an open overlapped handle; a new port is created for it alone.
        let port = unsafe { CreateIoCompletionPort(file.raw(), None, 0, 1) }?;
        let port = OwnedHandle::new(port);
        let reads = AlignedBuf::new(read_len, align)?;
        Ok(OverlappedTarget {
            file: Some(file),
            port: Some(port),
            slots: (0..slots)
                .map(|_| Slot {
                    ov: OVERLAPPED::default(),
                    busy: false,
                })
                .collect(),
            reads: Some(reads),
            pool: Some(pool),
            outstanding: 0,
            entries: vec![OVERLAPPED_ENTRY::default(); slots.max(1)],
        })
    }

    fn file(&self) -> HANDLE {
        self.file.as_ref().map(|f| f.raw()).unwrap_or_default()
    }

    fn slot_of(&self, ov: *mut OVERLAPPED) -> Option<usize> {
        self.slots
            .iter()
            .position(|s| std::ptr::eq(&s.ov as *const OVERLAPPED, ov as *const OVERLAPPED))
    }

    /// Error text of a failed completion, read from its OVERLAPPED, with the Win32 code
    /// ("… (error 112)").
    fn failure_text(&self, slot: usize) -> String {
        let mut n = 0u32;
        // SAFETY: the OVERLAPPED belongs to a completed I/O on this file; `n` is a valid out
        // pointer and the call does not wait.
        let result =
            unsafe { GetOverlappedResult(self.file(), &self.slots[slot].ov, &mut n, false) };
        match result {
            Err(e) => {
                let code = e.code().0 as u32;
                if code & 0xFFFF_0000 == 0x8007_0000 {
                    format!("{} (error {})", e.message(), code & 0xFFFF)
                } else {
                    e.message()
                }
            }
            Ok(()) => "the transfer failed".to_string(),
        }
    }

    /// Never frees what the kernel may still write to.
    fn leak(&mut self) {
        tracing::error!(
            outstanding = self.outstanding,
            "cancelled speed-test I/O did not finish; its buffers are kept for the process's lifetime"
        );
        std::mem::forget(std::mem::take(&mut self.slots));
        if let Some(reads) = self.reads.take() {
            std::mem::forget(reads);
        }
        if let Some(pool) = self.pool.take() {
            std::mem::forget(pool);
        }
        // The file stays open too, so nothing is released under the pending I/O.
        if let Some(file) = self.file.take() {
            std::mem::forget(file);
        }
        if let Some(port) = self.port.take() {
            std::mem::forget(port);
        }
        self.outstanding = 0;
    }
}

impl IoTarget for OverlappedTarget {
    fn slots(&self) -> usize {
        self.slots.len()
    }

    fn submit(
        &mut self,
        slot: usize,
        op: IoOp,
        offset: u64,
        len: u32,
        pool_offset: usize,
    ) -> Result<()> {
        let len_usize = len as usize;
        if slot >= self.slots.len() || self.slots[slot].busy {
            return Err(Error::Other(format!("I/O slot {slot} is not free")));
        }
        let file = self.file();
        let data: *mut u8 = match op {
            IoOp::Read => {
                let reads = self.reads.as_ref().ok_or_else(released)?;
                let start = slot * len_usize;
                if start + len_usize > reads.len() {
                    return Err(Error::Other("the read buffer is too small".to_string()));
                }
                // SAFETY: `start + len` lies inside the read buffer (checked above).
                unsafe { reads.as_ptr().add(start) }
            }
            IoOp::Write => {
                let pool = self.pool.as_ref().ok_or_else(released)?;
                if pool_offset + len_usize > pool.len() {
                    return Err(Error::Other("the write data is too small".to_string()));
                }
                // SAFETY: `pool_offset + len` lies inside the pool (checked above).
                unsafe { pool.as_ptr().add(pool_offset) }
            }
        };
        let entry = &mut self.slots[slot];
        entry.ov = OVERLAPPED::default();
        entry.ov.Anonymous.Anonymous.Offset = offset as u32;
        entry.ov.Anonymous.Anonymous.OffsetHigh = (offset >> 32) as u32;
        let ov: *mut OVERLAPPED = &mut entry.ov;
        // SAFETY: `data` points to `len` bytes that stay allocated until this I/O completed
        // (buffers are only freed after a drain, or leaked); the OVERLAPPED lives in `slots`,
        // a boxed slice allocated once in `new` that never grows or moves, so its address is
        // stable while the I/O is pending, and it is not reused before its completion is
        // dequeued.
        let issued = unsafe {
            match op {
                IoOp::Read => ReadFile(
                    file,
                    Some(std::slice::from_raw_parts_mut(data, len_usize)),
                    None,
                    Some(ov),
                ),
                IoOp::Write => WriteFile(
                    file,
                    Some(std::slice::from_raw_parts(data, len_usize)),
                    None,
                    Some(ov),
                ),
            }
        };
        match issued {
            // A synchronous success still posts a completion to the port.
            Ok(()) => {}
            Err(e) if is_win32(&e, ERROR_IO_PENDING) => {}
            Err(e) => return Err(e.into()),
        }
        entry.busy = true;
        self.outstanding += 1;
        Ok(())
    }

    fn wait(&mut self, timeout: Duration, out: &mut Vec<Completion>) -> Result<()> {
        if self.outstanding == 0 {
            return Ok(());
        }
        let Some(port) = self.port.as_ref().map(|p| p.raw()) else {
            return Err(released());
        };
        let want = self.outstanding.min(self.entries.len());
        let mut removed = 0u32;
        let millis = u32::try_from(timeout.as_millis()).unwrap_or(u32::MAX);
        // SAFETY: `entries` is writable for `want` entries and `removed` is a valid out
        // pointer; the port belongs to this target.
        let dequeued = unsafe {
            GetQueuedCompletionStatusEx(
                port,
                &mut self.entries[..want],
                &mut removed,
                millis,
                false,
            )
        };
        if let Err(e) = dequeued {
            // WAIT_TIMEOUT: nothing finished within the timeout.
            if e.code() == windows::core::HRESULT::from_win32(258) {
                return Ok(());
            }
            return Err(e.into());
        }
        for i in 0..removed as usize {
            let entry = self.entries[i];
            let Some(slot) = self.slot_of(entry.lpOverlapped) else {
                continue;
            };
            self.slots[slot].busy = false;
            self.outstanding = self.outstanding.saturating_sub(1);
            let status = entry.Internal as u32 as i32;
            let error = (status < 0).then(|| self.failure_text(slot));
            out.push(Completion {
                slot,
                bytes: entry.dwNumberOfBytesTransferred,
                error,
            });
        }
        Ok(())
    }

    fn outstanding(&self) -> usize {
        self.outstanding
    }

    fn drain(&mut self) -> Result<()> {
        if self.outstanding == 0 {
            return Ok(());
        }
        // SAFETY: cancels this process's I/O on the target's own file handle.
        if let Err(e) = unsafe { CancelIoEx(self.file(), None) } {
            if !is_win32(&e, ERROR_NOT_FOUND) {
                tracing::warn!(error = %e, "CancelIoEx failed");
            }
        }
        let deadline = Instant::now() + DRAIN_LIMIT;
        let mut done = Vec::new();
        while self.outstanding > 0 {
            if Instant::now() >= deadline {
                self.leak();
                return Err(Error::Other(
                    "the drive did not finish the cancelled transfers".to_string(),
                ));
            }
            done.clear();
            if let Err(e) = self.wait(WAIT_STEP, &mut done) {
                tracing::warn!(error = %e, "waiting for cancelled I/O failed");
                self.leak();
                return Err(e);
            }
        }
        Ok(())
    }
}

impl Drop for OverlappedTarget {
    fn drop(&mut self) {
        let _ = self.drain();
    }
}

fn released() -> Error {
    Error::Other("the test file was already closed".to_string())
}

// ───────────────────────────── Measurement ─────────────────────────────

/// One measurement run.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct MeasureSpec {
    pub op: IoOp,
    /// Bytes per I/O.
    pub block: u32,
    /// Queue depth.
    pub qd: u32,
    pub random: bool,
    pub file_len: u64,
    pub duration: Duration,
    /// Stop issuing once this many bytes were issued.
    pub byte_limit: Option<u64>,
    /// Alignment of write data taken from the pool (the sector size).
    pub align: u32,
}

/// What one run transferred.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub(crate) struct RunStats {
    pub bytes: u64,
    pub ios: u64,
    pub elapsed: Duration,
    pub latency_sum: Duration,
}

impl RunStats {
    /// Decimal megabytes per second.
    pub(crate) fn mb_s(&self) -> f64 {
        let secs = self.elapsed.as_secs_f64();
        if secs <= 0.0 {
            0.0
        } else {
            self.bytes as f64 / secs / 1e6
        }
    }

    pub(crate) fn iops(&self) -> f64 {
        let secs = self.elapsed.as_secs_f64();
        if secs <= 0.0 {
            0.0
        } else {
            self.ios as f64 / secs
        }
    }

    /// Mean latency per I/O in microseconds.
    pub(crate) fn latency_us(&self) -> f64 {
        if self.ios == 0 {
            0.0
        } else {
            self.latency_sum.as_secs_f64() * 1e6 / self.ios as f64
        }
    }
}

/// Offsets of the I/Os of one run.
struct Offsets {
    block: u64,
    blocks: u64,
    random: bool,
    next: u64,
}

impl Offsets {
    fn next(&mut self, rng: &mut Rng) -> u64 {
        if self.random {
            return rng.below(self.blocks) * self.block;
        }
        let offset = self.next;
        self.next += self.block;
        if self.next + self.block > self.blocks * self.block {
            self.next = 0;
        }
        offset
    }
}

/// Offset in the pool of a write of `block` bytes, aligned to `align`.
fn pool_offset(rng: &mut Rng, pool_len: usize, block: u32, align: u32) -> usize {
    let align = u64::from(align.max(1));
    let room = (pool_len as u64).saturating_sub(u64::from(block));
    (rng.below(room / align + 1) * align) as usize
}

/// Issues the I/Os of one run and remembers when each slot's I/O was issued.
struct Issuer<'a> {
    spec: &'a MeasureSpec,
    offsets: Offsets,
    issued_at: Vec<Instant>,
    issued_bytes: u64,
    pool_len: usize,
    start: Instant,
}

impl Issuer<'_> {
    fn may_issue(&self, cancel: &AtomicBool) -> bool {
        let block = u64::from(self.spec.block);
        self.start.elapsed() < self.spec.duration
            && !cancel.load(Ordering::SeqCst)
            && self
                .spec
                .byte_limit
                .map_or(true, |limit| self.issued_bytes + block <= limit)
    }

    fn issue(&mut self, t: &mut dyn IoTarget, slot: usize, rng: &mut Rng) -> Result<()> {
        let offset = self.offsets.next(rng);
        let from = match self.spec.op {
            IoOp::Write => pool_offset(rng, self.pool_len, self.spec.block, self.spec.align),
            IoOp::Read => 0,
        };
        self.issued_at[slot] = Instant::now();
        t.submit(slot, self.spec.op, offset, self.spec.block, from)?;
        self.issued_bytes += u64::from(self.spec.block);
        Ok(())
    }
}

/// Runs one measurement: keeps `spec.qd` I/Os in flight until `spec.duration` passed, a stop
/// was requested, or `spec.byte_limit` bytes were issued; then waits for the rest. `live`
/// sees the running totals after each batch of completions. A failed or short transfer
/// drains the queue and fails.
pub(crate) fn measure(
    t: &mut dyn IoTarget,
    spec: &MeasureSpec,
    rng: &mut Rng,
    pool_len: usize,
    cancel: &AtomicBool,
    live: &mut dyn FnMut(&RunStats),
) -> Result<RunStats> {
    let block = u64::from(spec.block);
    let blocks = spec.file_len.checked_div(block).unwrap_or(0);
    if blocks == 0 {
        return Err(Error::Other(
            "the test file is smaller than one block".to_string(),
        ));
    }
    let qd = (spec.qd as usize).clamp(1, t.slots().max(1));
    let start = Instant::now();
    let mut issuer = Issuer {
        spec,
        offsets: Offsets {
            block,
            blocks,
            random: spec.random,
            next: 0,
        },
        issued_at: vec![start; qd],
        issued_bytes: 0,
        pool_len,
        start,
    };
    let mut stats = RunStats::default();

    for slot in 0..qd {
        if !issuer.may_issue(cancel) {
            break;
        }
        if let Err(e) = issuer.issue(t, slot, rng) {
            let _ = t.drain();
            return Err(e);
        }
    }
    let mut done = Vec::with_capacity(qd);
    while t.outstanding() > 0 {
        done.clear();
        if let Err(e) = t.wait(WAIT_STEP, &mut done) {
            let _ = t.drain();
            return Err(e);
        }
        let now = Instant::now();
        for c in &done {
            if let Some(error) = &c.error {
                let _ = t.drain();
                return Err(Error::Other(format!(
                    "the drive reported an I/O error: {error}"
                )));
            }
            if c.bytes != spec.block {
                let _ = t.drain();
                return Err(Error::Other(format!(
                    "the drive reported an I/O error: {} of {} bytes were transferred",
                    c.bytes, spec.block
                )));
            }
            stats.bytes += u64::from(c.bytes);
            stats.ios += 1;
            if let Some(at) = issuer.issued_at.get(c.slot) {
                stats.latency_sum += now.saturating_duration_since(*at);
            }
            if issuer.may_issue(cancel) {
                if let Err(e) = issuer.issue(t, c.slot, rng) {
                    let _ = t.drain();
                    return Err(e);
                }
            }
        }
        stats.elapsed = start.elapsed();
        if !done.is_empty() {
            live(&stats);
        }
    }
    stats.elapsed = start.elapsed();
    Ok(stats)
}

/// Writes the whole file once, sequentially, eight 1 MiB writes at a time (smaller files use
/// one block); `progress` sees the bytes written. Returns the bytes written, fewer when a
/// stop was requested.
pub(crate) fn prepare(
    t: &mut dyn IoTarget,
    file_len: u64,
    rng: &mut Rng,
    pool_len: usize,
    align: u32,
    cancel: &AtomicBool,
    progress: &mut dyn FnMut(u64),
) -> Result<u64> {
    let block = file_len.min(1 << 20) as u32;
    let spec = MeasureSpec {
        op: IoOp::Write,
        block,
        qd: 8,
        random: false,
        file_len,
        duration: Duration::from_secs(u64::from(u32::MAX)),
        byte_limit: Some(file_len),
        align,
    };
    let stats = measure(t, &spec, rng, pool_len, cancel, &mut |s| progress(s.bytes))?;
    Ok(stats.bytes)
}

#[cfg(test)]
pub(crate) mod testing {
    use super::*;
    use std::collections::VecDeque;

    /// An in-memory target: every submitted I/O completes on the next wait, in order, after
    /// `delay`; a scripted slot error or short transfer can be injected by I/O number.
    #[derive(Debug, Default)]
    pub(crate) struct FakeTarget {
        pub slots: usize,
        pub delay: Duration,
        pub queued: VecDeque<(usize, IoOp, u64, u32, usize)>,
        pub submitted: Vec<(usize, IoOp, u64, u32, usize)>,
        pub max_outstanding: usize,
        /// I/O number (0-based) that fails, and whether it is short rather than an error.
        pub fail_at: Option<(usize, bool)>,
        pub drained: bool,
        pub completed: usize,
    }

    impl FakeTarget {
        pub(crate) fn new(slots: usize) -> FakeTarget {
            FakeTarget {
                slots,
                ..FakeTarget::default()
            }
        }
    }

    impl IoTarget for FakeTarget {
        fn slots(&self) -> usize {
            self.slots
        }

        fn submit(
            &mut self,
            slot: usize,
            op: IoOp,
            offset: u64,
            len: u32,
            pool_offset: usize,
        ) -> Result<()> {
            assert!(slot < self.slots);
            assert!(
                self.queued.iter().all(|q| q.0 != slot),
                "slot {slot} reused while busy"
            );
            self.queued.push_back((slot, op, offset, len, pool_offset));
            self.submitted.push((slot, op, offset, len, pool_offset));
            self.max_outstanding = self.max_outstanding.max(self.queued.len());
            Ok(())
        }

        fn wait(&mut self, _timeout: Duration, out: &mut Vec<Completion>) -> Result<()> {
            if !self.delay.is_zero() {
                std::thread::sleep(self.delay);
            }
            if let Some((slot, _, _, len, _)) = self.queued.pop_front() {
                let n = self.completed;
                self.completed += 1;
                let (bytes, error) = match self.fail_at {
                    Some((at, true)) if at == n => (len / 2, None),
                    Some((at, false)) if at == n => (0, Some("scripted failure".to_string())),
                    _ => (len, None),
                };
                out.push(Completion { slot, bytes, error });
            }
            Ok(())
        }

        fn outstanding(&self) -> usize {
            self.queued.len()
        }

        fn drain(&mut self) -> Result<()> {
            self.drained = true;
            self.queued.clear();
            Ok(())
        }
    }
}

#[cfg(test)]
mod tests {
    use super::testing::FakeTarget;
    use super::*;

    const MIB: u64 = 1 << 20;

    fn spec(op: IoOp, block: u32, qd: u32, random: bool) -> MeasureSpec {
        MeasureSpec {
            op,
            block,
            qd,
            random,
            file_len: 8 * MIB,
            duration: Duration::from_millis(40),
            byte_limit: None,
            align: 4096,
        }
    }

    fn run(t: &mut FakeTarget, spec: &MeasureSpec) -> Result<RunStats> {
        let cancel = AtomicBool::new(false);
        let mut rng = Rng::seeded(7);
        measure(t, spec, &mut rng, 16 * MIB as usize, &cancel, &mut |_| {})
    }

    #[test]
    fn the_queue_stays_full_until_the_deadline() {
        let mut t = FakeTarget::new(32);
        t.delay = Duration::from_millis(1);
        let stats = run(&mut t, &spec(IoOp::Read, 4096, 32, true)).unwrap();
        assert_eq!(t.max_outstanding, 32);
        assert!(stats.ios > 32, "{stats:?}");
        assert_eq!(stats.bytes, stats.ios * 4096);
        assert_eq!(t.outstanding(), 0);
        // Every completion before the deadline was replaced at once: the first I/Os filled
        // every slot.
        let first: Vec<usize> = t.submitted.iter().take(32).map(|s| s.0).collect();
        assert_eq!(first, (0..32).collect::<Vec<_>>());
        assert!(stats.elapsed >= Duration::from_millis(40));
    }

    #[test]
    fn the_write_byte_limit_is_never_exceeded() {
        for qd in [1u32, 8] {
            let mut t = FakeTarget::new(8);
            let mut s = spec(IoOp::Write, 1 << 20, qd, false);
            s.duration = Duration::from_secs(60);
            s.byte_limit = Some(5 * MIB);
            let stats = run(&mut t, &s).unwrap();
            assert_eq!(stats.bytes, 5 * MIB, "qd {qd}");
            assert_eq!(t.submitted.len(), 5);
        }
        // A limit that is not a whole number of blocks stops before it.
        let mut t = FakeTarget::new(8);
        let mut s = spec(IoOp::Write, 1 << 20, 8, false);
        s.duration = Duration::from_secs(60);
        s.byte_limit = Some(5 * MIB / 2);
        assert_eq!(run(&mut t, &s).unwrap().bytes, 2 * MIB);
    }

    #[test]
    fn offsets_wrap_and_random_offsets_are_aligned_and_in_range() {
        let mut t = FakeTarget::new(8);
        let mut s = spec(IoOp::Write, 1 << 20, 8, false);
        s.duration = Duration::from_secs(60);
        s.byte_limit = Some(20 * MIB);
        run(&mut t, &s).unwrap();
        let offsets: Vec<u64> = t.submitted.iter().map(|s| s.2).collect();
        let expected: Vec<u64> = (0..20).map(|i| (i % 8) * MIB).collect();
        assert_eq!(offsets, expected);
        for sub in &t.submitted {
            assert_eq!(sub.4 % 4096, 0);
            assert!(sub.4 + (1 << 20) <= 16 << 20);
        }

        let mut t = FakeTarget::new(32);
        run(&mut t, &spec(IoOp::Read, 4096, 32, true)).unwrap();
        assert!(t.submitted.len() > 100);
        for sub in &t.submitted {
            assert_eq!(sub.2 % 4096, 0);
            assert!(sub.2 + 4096 <= 8 * MIB);
        }
        let distinct: std::collections::HashSet<u64> = t.submitted.iter().map(|s| s.2).collect();
        assert!(distinct.len() > 100, "{} distinct offsets", distinct.len());
    }

    #[test]
    fn a_failed_or_short_transfer_drains_and_fails() {
        for short in [false, true] {
            let mut t = FakeTarget::new(8);
            t.fail_at = Some((3, short));
            let err = run(&mut t, &spec(IoOp::Read, 1 << 20, 8, false)).unwrap_err();
            assert!(
                err.to_string()
                    .starts_with("the drive reported an I/O error"),
                "{err}"
            );
            assert!(t.drained);
            assert_eq!(t.outstanding(), 0);
        }
    }

    #[test]
    fn a_stop_ends_the_issuing() {
        let mut t = FakeTarget::new(8);
        let cancel = AtomicBool::new(false);
        let mut rng = Rng::seeded(1);
        let mut s = spec(IoOp::Read, 1 << 20, 8, false);
        s.duration = Duration::from_secs(60);
        let mut seen = 0;
        let stats = measure(&mut t, &s, &mut rng, 16 << 20, &cancel, &mut |_| {
            seen += 1;
            if seen == 5 {
                cancel.store(true, Ordering::SeqCst);
            }
        })
        .unwrap();
        // Before the stop every completion was replaced; afterwards only the queue drains.
        assert_eq!(t.submitted.len(), 8 + 5);
        assert_eq!(stats.ios, 13);
        assert_eq!(t.outstanding(), 0);
    }

    #[test]
    fn latency_is_averaged_per_io() {
        let mut t = FakeTarget::new(1);
        t.delay = Duration::from_millis(3);
        let stats = run(&mut t, &spec(IoOp::Read, 4096, 1, false)).unwrap();
        assert!(stats.ios >= 2);
        let per_io = stats.latency_us();
        assert!(per_io >= 3000.0, "{per_io}");
        assert!(per_io < stats.elapsed.as_secs_f64() * 1e6, "{per_io}");
        assert!(stats.mb_s() > 0.0 && stats.iops() > 0.0);
    }

    #[test]
    fn prepare_writes_the_file_once() {
        let mut t = FakeTarget::new(8);
        let cancel = AtomicBool::new(false);
        let mut rng = Rng::seeded(3);
        let mut last = 0;
        let written = prepare(
            &mut t,
            6 * MIB,
            &mut rng,
            16 << 20,
            4096,
            &cancel,
            &mut |b| last = b,
        )
        .unwrap();
        assert_eq!(written, 6 * MIB);
        assert_eq!(last, 6 * MIB);
        let offsets: Vec<u64> = t.submitted.iter().map(|s| s.2).collect();
        assert_eq!(offsets, (0..6).map(|i| i * MIB).collect::<Vec<_>>());
        assert_eq!(t.max_outstanding, 6);
    }

    #[test]
    fn rng_below_stays_in_range() {
        let mut rng = Rng::seeded(0);
        assert_eq!(rng.below(0), 0);
        for _ in 0..1000 {
            assert!(rng.below(10) < 10);
        }
        let buf = AlignedBuf::new(8192, 4096).unwrap();
        assert_eq!(buf.as_ptr() as usize % 4096, 0);
        assert!(buf.as_slice().iter().all(|&b| b == 0));
        let random = AlignedBuf::random(4096, 4096).unwrap();
        assert!(random.as_slice().iter().any(|&b| b != 0));
    }
}
