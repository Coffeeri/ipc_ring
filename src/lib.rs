//! `ipc_ring`: mmap-backed SPSC shared-memory ring for Unix (Linux + macOS).
//! - Generic: carries raw bytes
//! - No compression, no JSON assumptions
//! - One writer process <-> one reader process

#![cfg(unix)]
#![deny(clippy::all)]
#![deny(clippy::pedantic)]
#![allow(clippy::cast_ptr_alignment)] // Necessary for low-level memory mapping
#![allow(clippy::cast_possible_truncation)] // File sizes and offsets are validated
#![allow(clippy::multiple_crate_versions)] // Dependency issue, not our code

use memmap2::{MmapMut, MmapOptions};
use raw_sync::events::{EventInit, EventState};
use raw_sync::Timeout;
use std::fs::OpenOptions;
use std::io;
use std::mem::{align_of, size_of};
use std::os::unix::fs::OpenOptionsExt;
use std::path::Path;
use std::ptr;
use std::sync::atomic::{fence, AtomicU32, AtomicU64, Ordering};
use std::sync::OnceLock;
use std::time::Duration;
use thiserror::Error;

const MAGIC: u64 = 0x4950_4352_494E_4731; // "IPCRING1"
const READY: u32 = 1 << 31;
const HDR_ALIGN: usize = 64;
const DEFAULT_POLL_INTERVAL_MS: u64 = 2;
static POLL_INTERVAL_DEFAULT: OnceLock<Duration> = OnceLock::new();

#[cfg(feature = "failpoints")]
macro_rules! ring_fail_point {
    ($name:literal) => {
        fail::fail_point!($name);
    };
}

#[cfg(not(feature = "failpoints"))]
macro_rules! ring_fail_point {
    ($name:literal) => {
        let _ = $name;
    };
}

#[inline]
const fn align_up(x: usize, a: usize) -> usize {
    (x + a - 1) & !(a - 1)
}

#[repr(C)]
struct Header {
    magic: u64,
    cap: u64,          // ring capacity in bytes (power of two)
    write: AtomicU64,  // monotonically increasing
    read: AtomicU64,   // monotonically increasing
    commit: AtomicU64, // last published write index
    _pad: [u8; 64 - 8 - 8 - 8 - 8 - 8],
}

// Common mapping state, owned by both writer and reader.
// Layout in the mapping:
// [Header][Event(data_avail)][Event(space_avail)][padding to 64B][Ring bytes...]
struct RingMapping {
    // Own the mmap to guarantee the ring_ptr stays valid (we keep a raw pointer into it).
    // We never read this field directly, but it must be owned so Drop unmaps it.
    #[allow(dead_code)]
    map: MmapMut,
    hdr: *mut Header,
    data_avail: Box<dyn raw_sync::events::EventImpl>,
    space_avail: Box<dyn raw_sync::events::EventImpl>,
    ring_ptr: *mut u8,
    ring_cap: usize,
    mask: usize,
}

pub struct RingWriter {
    inner: RingMapping,
    last_read: u64,
    poll_interval: Duration,
}

pub struct RingReader {
    inner: RingMapping,
    last_commit: u64,
    poll_interval: Duration,
}

#[cfg(feature = "failpoints")]
unsafe impl Send for RingWriter {}
#[cfg(feature = "failpoints")]
unsafe impl Send for RingReader {}

#[must_use]
pub const fn failpoints_enabled() -> bool {
    cfg!(feature = "failpoints")
}

impl RingMapping {
    /// Initialize a brand new mapping: writes header, constructs events, aligns ring, etc.
    unsafe fn init_new(mut map: MmapMut, cap: usize) -> Result<Self, IpcError> {
        let base = map.as_mut_ptr();
        let mut off = 0usize;

        // Header
        let hdr_ptr = base.add(off).cast::<Header>();
        ptr::write(
            hdr_ptr,
            Header {
                magic: MAGIC,
                cap: cap as u64,
                write: AtomicU64::new(0),
                read: AtomicU64::new(0),
                commit: AtomicU64::new(0),
                _pad: [0; 64 - 40],
            },
        );
        off += align_up(size_of::<Header>(), HDR_ALIGN);

        // Events (manual-reset=true)
        let (data_evt, used1) = raw_sync::events::Event::new(base.add(off), true)
            .map_err(|e| IpcError::Event(to_send_sync_error(e.as_ref())))?;
        off += align_up(used1, align_of::<usize>());
        let (space_evt, used2) = raw_sync::events::Event::new(base.add(off), true)
            .map_err(|e| IpcError::Event(to_send_sync_error(e.as_ref())))?;
        off += align_up(used2, align_of::<usize>());
        off = align_up(off, HDR_ALIGN);

        let ring_ptr = base.add(off);
        let ring_cap = align_up(cap, HDR_ALIGN);

        Ok(Self {
            map,
            hdr: hdr_ptr,
            data_avail: data_evt,
            space_avail: space_evt,
            ring_ptr,
            ring_cap,
            mask: ring_cap - 1,
        })
    }

    /// Reconstruct an existing mapping: validates header, reopens events, derives ring.
    unsafe fn from_existing(mut map: MmapMut) -> Result<Self, IpcError> {
        let base = map.as_mut_ptr();
        let mut off = 0usize;

        let hdr = &*base.cast::<Header>();
        if hdr.magic != MAGIC {
            return Err(IpcError::Layout);
        }
        let cap = hdr.cap as usize;

        let hdr_ptr = base.cast::<Header>();
        off += align_up(size_of::<Header>(), HDR_ALIGN);

        let (data_evt, used1) = raw_sync::events::Event::from_existing(base.add(off))
            .map_err(|e| IpcError::Event(to_send_sync_error(e.as_ref())))?;
        off += align_up(used1, align_of::<usize>());
        let (space_evt, used2) = raw_sync::events::Event::from_existing(base.add(off))
            .map_err(|e| IpcError::Event(to_send_sync_error(e.as_ref())))?;
        off += align_up(used2, align_of::<usize>());
        off = align_up(off, HDR_ALIGN);

        let ring_ptr = base.add(off);
        let ring_cap = align_up(cap, HDR_ALIGN);

        Ok(Self {
            map,
            hdr: hdr_ptr,
            data_avail: data_evt,
            space_avail: space_evt,
            ring_ptr,
            ring_cap,
            mask: ring_cap - 1,
        })
    }

    #[inline]
    fn write_header(&self, w: usize, len: u32) {
        let pos = w & self.mask;
        debug_assert!(
            pos < self.ring_cap,
            "write position {} >= ring_cap {}",
            pos,
            self.ring_cap
        );
        let p = unsafe { self.ring_ptr.add(pos).cast::<AtomicU32>() };
        unsafe { (&*p).store(len, Ordering::Relaxed) };
    }

    #[inline]
    fn publish_header(&self, w: usize, len: u32) {
        fence(Ordering::Release);
        let pos = w & self.mask;
        debug_assert!(
            pos < self.ring_cap,
            "publish position {} >= ring_cap {}",
            pos,
            self.ring_cap
        );
        let p = unsafe { self.ring_ptr.add(pos).cast::<AtomicU32>() };
        unsafe { (&*p).store(len | READY, Ordering::Release) };
    }

    #[inline]
    fn write_payload(&self, w: usize, data: &[u8]) {
        unsafe {
            let pos = w & self.mask;
            let end = self.ring_cap;
            let first = (end - pos).min(data.len());
            ptr::copy_nonoverlapping(data.as_ptr(), self.ring_ptr.add(pos), first);
            if first < data.len() {
                ptr::copy_nonoverlapping(
                    data.as_ptr().add(first),
                    self.ring_ptr,
                    data.len() - first,
                );
            }
            // optional zero padding to 4B boundary
            let pad = align_up(data.len(), 4) - data.len();
            if pad != 0 {
                let zeros = [0u8; 3];
                let pad_start = (pos + data.len()) & self.mask;
                let end = self.ring_cap;
                let first_pad = (end - pad_start).min(pad);
                ptr::copy_nonoverlapping(zeros.as_ptr(), self.ring_ptr.add(pad_start), first_pad);
                if first_pad < pad {
                    ptr::copy_nonoverlapping(
                        zeros.as_ptr().add(first_pad),
                        self.ring_ptr,
                        pad - first_pad,
                    );
                }
            }
        }
    }

    #[inline]
    fn read_header(&self, r: usize) -> u32 {
        let pos = r & self.mask;
        let p = unsafe { self.ring_ptr.add(pos).cast::<AtomicU32>() };
        unsafe { (&*p).load(Ordering::Acquire) }
    }

    #[inline]
    fn read_payload(&self, r: usize, out: &mut [u8]) {
        unsafe {
            let pos = r & self.mask;
            let end = self.ring_cap;
            let first = (end - pos).min(out.len());
            ptr::copy_nonoverlapping(self.ring_ptr.add(pos), out.as_mut_ptr(), first);
            if first < out.len() {
                ptr::copy_nonoverlapping(
                    self.ring_ptr,
                    out.as_mut_ptr().add(first),
                    out.len() - first,
                );
            }
        }
    }
}

#[derive(Error, Debug)]
pub enum IpcError {
    #[error("io error: {0}")]
    Io(#[from] io::Error),
    #[error("invalid layout or magic")]
    Layout,
    #[error("insufficient space")]
    Full,
    #[error("message too large")]
    TooLarge,
    #[error("wait timed out")]
    Timeout,
    #[error("peer likely crashed or is unresponsive")]
    PeerStalled,
    #[error("event error: {0}")]
    Event(Box<dyn std::error::Error + Send + Sync>),
}

#[derive(Debug)]
struct SimpleError(String);

impl std::fmt::Display for SimpleError {
    fn fmt(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
        write!(f, "{}", self.0)
    }
}

impl std::error::Error for SimpleError {}

fn to_send_sync_error(err: &dyn std::error::Error) -> Box<dyn std::error::Error + Send + Sync> {
    Box::new(SimpleError(err.to_string()))
}

fn is_timeout_error(err: &dyn std::error::Error) -> bool {
    let msg = err.to_string();
    msg.contains("timed out")
        || msg.contains("Timed out")
        || msg.contains("Failed waiting for signal")
        || msg.contains("Failed waiting for event")
}

fn clamp_interval(interval: Duration) -> Duration {
    if interval.is_zero() {
        Duration::from_micros(1)
    } else {
        interval
    }
}

fn default_poll_interval() -> Duration {
    *POLL_INTERVAL_DEFAULT.get_or_init(|| {
        let from_env = std::env::var("IPC_RING_POLL_MS")
            .ok()
            .and_then(|val| val.parse::<u64>().ok())
            .filter(|&ms| ms > 0)
            .map(Duration::from_millis);
        clamp_interval(from_env.unwrap_or_else(|| Duration::from_millis(DEFAULT_POLL_INTERVAL_MS)))
    })
}

impl RingWriter {
    /// Create a new ring mapping at `path`. Fails if the file exists.
    ///
    /// # Panics
    ///
    /// Panics if `cap_pow2` is not a power of two.
    ///
    /// # Errors
    ///
    /// Returns an error if:
    /// - File already exists
    /// - Cannot create or write to the file
    /// - Memory mapping fails
    pub fn create<P: AsRef<Path>>(path: P, cap_pow2: usize) -> Result<Self, IpcError> {
        assert!(cap_pow2.is_power_of_two());
        let evt_sz = raw_sync::events::Event::size_of(None);
        let layout = align_up(size_of::<Header>(), HDR_ALIGN)
            + align_up(evt_sz, align_of::<usize>())
            + align_up(evt_sz, align_of::<usize>())
            + align_up(cap_pow2, HDR_ALIGN);

        let file = OpenOptions::new()
            .read(true)
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(path)?;

        file.set_len(layout as u64)?;

        let map = unsafe { MmapOptions::new().len(layout).map_mut(&file)? };
        // `file` can be dropped after mapping; mapping remains valid until unmapped.
        drop(file);
        let inner = unsafe { RingMapping::init_new(map, cap_pow2) }?;
        let read = unsafe { (&*inner.hdr).read.load(Ordering::Acquire) };
        let poll_interval = default_poll_interval();
        ring_fail_point!("ring_writer::create::after_init");
        Ok(Self {
            inner,
            last_read: read,
            poll_interval,
        })
    }

    /// Try to push without blocking.
    ///
    /// # Errors
    ///
    /// Returns an error if:
    /// - Message is too large for the ring buffer  
    /// - Ring buffer is full
    pub fn try_push(&mut self, payload: &[u8]) -> Result<(), IpcError> {
        if payload.len() + 4 > self.inner.ring_cap {
            return Err(IpcError::TooLarge);
        }

        debug_assert!(
            self.inner.ring_cap.is_power_of_two(),
            "ring_cap must be power of 2"
        );
        debug_assert_eq!(
            self.inner.mask,
            self.inner.ring_cap - 1,
            "mask must equal ring_cap - 1"
        );

        let hdr = unsafe { &*self.inner.hdr };
        let read = hdr.read.load(Ordering::Acquire);
        self.last_read = read;
        let mut cur_write = hdr.write.load(Ordering::Relaxed);
        let used = cur_write - read;
        let space = self.inner.ring_cap as u64 - used;
        let need = align_up(payload.len() + 4, 4) as u64;

        if space < need {
            return Err(IpcError::Full);
        }

        let mut w = cur_write as usize & self.inner.mask;

        // Ensure header is contiguous; if not, write a wrap marker (len=0|READY)
        if w + 4 > self.inner.ring_cap || w + 4 + payload.len() > self.inner.ring_cap {
            // Need to wrap; ensure we have space for the wrap marker (gap to end) + message
            let gap = (self.inner.ring_cap - w) as u64;
            if space < gap + need {
                return Err(IpcError::Full);
            }

            // Emit wrap marker and advance write pointer to start
            self.write_header(w, 0);
            self.publish_header(w, 0);
            ring_fail_point!("ring_writer::after_wrap_publish");
            cur_write = cur_write.wrapping_add(gap);
            hdr.write.store(cur_write, Ordering::Release);
            hdr.commit.store(cur_write, Ordering::Release);
            ring_fail_point!("ring_writer::after_wrap_advance");
            // Wake the reader so it can consume the wrap marker promptly
            let _ = self
                .inner
                .data_avail
                .set(EventState::Signaled)
                .map_err(|e| IpcError::Event(to_send_sync_error(e.as_ref())));
            ring_fail_point!("ring_writer::after_wrap_signal");

            w = 0; // After wrap marker, next write starts at beginning
        }

        // Write header (no READY), copy payload, then publish (set READY)
        self.write_header(w, payload.len() as u32);
        ring_fail_point!("ring_writer::after_write_header");
        self.write_payload(w + 4, payload);
        ring_fail_point!("ring_writer::after_write_payload");
        self.publish_header(w, payload.len() as u32);
        ring_fail_point!("ring_writer::after_publish_header");

        let bump = align_up(4 + payload.len(), 4) as u64;
        cur_write = cur_write.wrapping_add(bump);
        hdr.write.store(cur_write, Ordering::Release);
        hdr.commit.store(cur_write, Ordering::Release);
        ring_fail_point!("ring_writer::after_write_advance");

        // Signal data available
        self.inner
            .data_avail
            .set(EventState::Signaled)
            .map_err(|e| IpcError::Event(to_send_sync_error(e.as_ref())))?;
        ring_fail_point!("ring_writer::after_data_signal");
        Ok(())
    }

    /// Push with optional timeout (None = infinite).
    ///
    /// # Errors
    ///
    /// Returns an error if:
    /// - Message is too large for the ring buffer
    /// - Ring buffer is full and timeout expires
    /// - Event synchronization fails
    pub fn push(&mut self, payload: &[u8], timeout: Option<Duration>) -> Result<(), IpcError> {
        loop {
            match self.try_push(payload) {
                Ok(()) => return Ok(()),
                Err(IpcError::Full) => {
                    let hdr = unsafe { &*self.inner.hdr };
                    let prior_read = self.last_read;
                    let wait_timeout =
                        timeout.map_or_else(|| Timeout::Val(self.poll_interval), Timeout::Val);
                    ring_fail_point!("ring_writer::before_space_wait");
                    let wait_res = self.inner.space_avail.wait(wait_timeout);
                    ring_fail_point!("ring_writer::after_space_wait");
                    match wait_res {
                        Ok(()) => {}
                        Err(e) => {
                            if timeout.is_none() && is_timeout_error(e.as_ref()) {
                                let new_read = hdr.read.load(Ordering::Acquire);
                                if new_read == prior_read {
                                    return Err(IpcError::PeerStalled);
                                }
                                self.last_read = new_read;
                                continue;
                            }
                            if timeout.is_some() && is_timeout_error(e.as_ref()) {
                                return Err(IpcError::Timeout);
                            }
                            return Err(IpcError::Event(to_send_sync_error(e.as_ref())));
                        }
                    }
                    let new_read = hdr.read.load(Ordering::Acquire);
                    self.last_read = new_read;
                }
                Err(e) => return Err(e),
            }
        }
    }

    #[inline]
    fn write_header(&self, w: usize, len: u32) {
        self.inner.write_header(w, len);
    }
    #[inline]
    fn publish_header(&self, w: usize, len: u32) {
        self.inner.publish_header(w, len);
    }
    #[inline]
    fn write_payload(&self, w: usize, data: &[u8]) {
        self.inner.write_payload(w, data);
    }
}

impl RingWriter {
    /// Set the poll interval used when waiting for space without a timeout.
    pub fn set_poll_interval(&mut self, interval: Duration) {
        self.poll_interval = clamp_interval(interval);
    }

    /// Returns the current poll interval for unsignaled waits.
    #[must_use]
    pub fn poll_interval(&self) -> Duration {
        self.poll_interval
    }
}

impl RingReader {
    /// Open an existing ring mapping at `path`.
    ///
    /// # Errors
    ///
    /// Returns an error if:
    /// - File does not exist or cannot be read
    /// - File has invalid magic header or layout
    /// - Memory mapping fails
    pub fn open<P: AsRef<Path>>(path: P) -> Result<Self, IpcError> {
        let file = OpenOptions::new().read(true).write(true).open(path)?;
        let len = file.metadata()?.len() as usize;
        let map = unsafe { MmapOptions::new().len(len).map_mut(&file)? };
        // We don't need to retain the file descriptor after mapping.
        drop(file);

        // Reconstruct layout
        unsafe {
            let inner = RingMapping::from_existing(map)?;
            let commit = (&*inner.hdr).commit.load(Ordering::Acquire);
            let poll_interval = default_poll_interval();
            ring_fail_point!("ring_reader::open::after_map");
            Ok(Self {
                inner,
                last_commit: commit,
                poll_interval,
            })
        }
    }

    /// Try to pop without blocking. On success returns `Some(bytes_written_into_out)`.
    ///
    /// # Errors
    ///
    /// Returns an error if:
    /// - Event synchronization fails during space signaling
    pub fn try_pop(&mut self, out: &mut Vec<u8>) -> Result<Option<usize>, IpcError> {
        let hdr = unsafe { &*self.inner.hdr };
        let write = hdr.write.load(Ordering::Acquire);
        let read = hdr.read.load(Ordering::Relaxed);
        if write == read {
            return Ok(None);
        }

        let r = read as usize & self.inner.mask;
        // Read header with Acquire (ensures payload visibility).
        let h = self.inner.read_header(r);
        if (h & READY) == 0 {
            // Not published yet: let writer proceed.
            return Ok(None);
        }
        let size = (h & !READY) as usize;

        if size == 0 {
            // Wrap marker
            let bump = (self.inner.ring_cap - r) as u64;
            hdr.read.store(read + bump, Ordering::Release);
            ring_fail_point!("ring_reader::after_wrap_read_advance");
            self.inner
                .space_avail
                .set(EventState::Signaled)
                .map_err(|e| IpcError::Event(to_send_sync_error(e.as_ref())))?;
            ring_fail_point!("ring_reader::after_wrap_space_signal");
            return self.try_pop(out);
        }

        out.resize(size, 0);
        self.inner.read_payload(r + 4, &mut out[..]);
        ring_fail_point!("ring_reader::before_read_advance");

        let bump = align_up(4 + size, 4) as u64;
        hdr.read.store(read + bump, Ordering::Release);
        ring_fail_point!("ring_reader::after_read_advance");

        // Signal writer that there is room
        self.inner
            .space_avail
            .set(EventState::Signaled)
            .map_err(|e| IpcError::Event(to_send_sync_error(e.as_ref())))?;
        ring_fail_point!("ring_reader::after_space_signal");
        let commit = hdr.commit.load(Ordering::Acquire);
        self.last_commit = commit;
        Ok(Some(size))
    }

    /// Blocking pop with optional timeout (None = infinite).
    ///
    /// # Errors
    ///
    /// Returns an error if:
    /// - Timeout expires with no data available
    /// - Event synchronization fails
    pub fn pop(&mut self, out: &mut Vec<u8>, timeout: Option<Duration>) -> Result<usize, IpcError> {
        loop {
            if let Some(n) = self.try_pop(out)? {
                return Ok(n);
            }

            let hdr = unsafe { &*self.inner.hdr };
            let commit_now = hdr.commit.load(Ordering::Acquire);
            if commit_now != self.last_commit {
                // Writer advanced without signaling; re-check immediately.
                continue;
            }

            let wait_timeout =
                timeout.map_or_else(|| Timeout::Val(self.poll_interval), Timeout::Val);
            ring_fail_point!("ring_reader::before_data_wait");
            let wait_res = self.inner.data_avail.wait(wait_timeout);
            ring_fail_point!("ring_reader::after_data_wait");
            match wait_res {
                Ok(()) => {}
                Err(e) => {
                    if timeout.is_none() && is_timeout_error(e.as_ref()) {
                        continue;
                    }
                    if timeout.is_some() && is_timeout_error(e.as_ref()) {
                        return Err(IpcError::Timeout);
                    }
                    return Err(IpcError::Event(to_send_sync_error(e.as_ref())));
                }
            }
        }
    }
}

impl RingReader {
    /// Set the poll interval used when waiting without a timeout. Durations smaller than 1µs are clamped.
    pub fn set_poll_interval(&mut self, interval: Duration) {
        self.poll_interval = clamp_interval(interval);
    }

    /// Returns the current poll interval.
    #[must_use]
    pub fn poll_interval(&self) -> Duration {
        self.poll_interval
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use std::slice;

    fn test_ring_path() -> String {
        use std::sync::atomic::{AtomicU32, Ordering};
        static COUNTER: AtomicU32 = AtomicU32::new(0);
        format!(
            "/tmp/ipc_ring_test_{}_{}",
            std::process::id(),
            COUNTER.fetch_add(1, Ordering::SeqCst)
        )
    }

    fn cleanup_ring(path: &str) {
        let _ = fs::remove_file(path);
    }

    #[test]
    fn test_create_ring() {
        let path = test_ring_path();
        cleanup_ring(&path);

        let result = RingWriter::create(&path, 4096);
        assert!(result.is_ok());
        cleanup_ring(&path);
    }

    #[test]
    fn test_create_existing_ring_fails() {
        let path = test_ring_path();
        cleanup_ring(&path);

        let _writer1 = RingWriter::create(&path, 4096).unwrap();
        let result2 = RingWriter::create(&path, 4096);
        assert!(result2.is_err());
        cleanup_ring(&path);
    }

    #[test]
    fn test_open_nonexistent_ring_fails() {
        let path = test_ring_path();
        cleanup_ring(&path);

        let result = RingReader::open(&path);
        assert!(result.is_err());
    }

    #[test]
    fn test_open_invalid_magic_layout_error() {
        // Create a file with a valid size but wrong magic to trigger IpcError::Layout
        let path = test_ring_path();
        cleanup_ring(&path);
        let f = fs::OpenOptions::new()
            .create(true)
            .write(true)
            .read(true)
            .open(&path)
            .unwrap();
        // Write at least a header's worth of zeros so mapping and header read are safe
        let dummy_len = 4096u64;
        f.set_len(dummy_len).unwrap();
        // Try to open as reader: should fail with Layout
        let res = RingReader::open(&path);
        match res {
            Err(IpcError::Layout) => {}
            Ok(_) => panic!("expected Layout error, got Ok(_)"),
            Err(other) => panic!("expected Layout error, got {other:?}"),
        }
        cleanup_ring(&path);
    }

    #[test]
    fn test_basic_push_pop() {
        let path = test_ring_path();
        cleanup_ring(&path);

        let mut writer = RingWriter::create(&path, 4096).unwrap();
        let mut reader = RingReader::open(&path).unwrap();
        let mut buf = Vec::new();

        // Test basic message
        let msg = b"hello world";
        writer.try_push(msg).unwrap();

        let n = reader.try_pop(&mut buf).unwrap();
        assert_eq!(n, Some(msg.len()));
        assert_eq!(&buf[..], msg);

        cleanup_ring(&path);
    }

    #[test]
    fn test_multiple_messages() {
        let path = test_ring_path();
        cleanup_ring(&path);

        let mut writer = RingWriter::create(&path, 4096).unwrap();
        let mut reader = RingReader::open(&path).unwrap();
        let mut buf = Vec::new();

        let messages = [&b"first"[..], &b"second"[..], &b"third"[..]];

        // Push all messages
        for msg in &messages {
            writer.try_push(msg).unwrap();
        }

        // Pop all messages
        for expected in &messages {
            let n = reader.try_pop(&mut buf).unwrap();
            assert_eq!(n, Some(expected.len()));
            assert_eq!(&buf[..], *expected);
        }

        cleanup_ring(&path);
    }

    #[test]
    fn test_empty_pop_returns_none() {
        let path = test_ring_path();
        cleanup_ring(&path);

        let writer = RingWriter::create(&path, 4096).unwrap();
        let mut reader = RingReader::open(&path).unwrap();
        let mut buf = Vec::new();

        let result = reader.try_pop(&mut buf).unwrap();
        assert_eq!(result, None);

        drop(writer);
        cleanup_ring(&path);
    }

    #[test]
    fn test_fill_buffer() {
        let path = test_ring_path();
        cleanup_ring(&path);

        let mut writer = RingWriter::create(&path, 1024).unwrap(); // Small buffer
        let msg = vec![0u8; 256]; // Large message

        // Should be able to push a few messages
        assert!(writer.try_push(&msg).is_ok());
        assert!(writer.try_push(&msg).is_ok());

        // Eventually should fail with Full
        loop {
            match writer.try_push(&msg) {
                Ok(()) => continue,
                Err(IpcError::Full) => break,
                Err(e) => panic!("Unexpected error: {e:?}"),
            }
        }

        cleanup_ring(&path);
    }

    #[test]
    fn test_message_too_large() {
        let path = test_ring_path();
        cleanup_ring(&path);

        let mut writer = RingWriter::create(&path, 1024).unwrap();
        let huge_msg = vec![0u8; 2048]; // Larger than buffer

        let result = writer.try_push(&huge_msg);
        assert!(matches!(result, Err(IpcError::TooLarge)));

        cleanup_ring(&path);
    }

    #[test]
    fn test_wraparound() {
        let path = test_ring_path();
        cleanup_ring(&path);

        let mut writer = RingWriter::create(&path, 1024).unwrap();
        let mut reader = RingReader::open(&path).unwrap();
        let mut buf = Vec::new();

        let msg = vec![42u8; 200];

        // Fill buffer partially
        writer.try_push(&msg).unwrap();
        writer.try_push(&msg).unwrap();

        // Read one message to make space
        reader.try_pop(&mut buf).unwrap();

        // Should be able to push more (testing wraparound)
        writer.try_push(&msg).unwrap();

        // Verify we can read the remaining messages
        reader.try_pop(&mut buf).unwrap();
        assert_eq!(&buf[..], &msg[..]);

        reader.try_pop(&mut buf).unwrap();
        assert_eq!(&buf[..], &msg[..]);

        cleanup_ring(&path);
    }

    #[test]
    fn test_write_payload_wraps_data_across_end() {
        // Directly exercise write_payload's wrap copy (second segment at start of ring)
        let path = test_ring_path();
        cleanup_ring(&path);

        let writer = RingWriter::create(&path, 64).unwrap();
        // Choose a position near the end so the data crosses the boundary
        let w = 54; // pos = 54, end = 64, gap = 10
        let data: Vec<u8> = (0u8..16u8).collect(); // 16 bytes, will split 10 + 6

        // Perform raw payload write (bypassing header constraints for coverage)
        writer.write_payload(w, &data);

        unsafe {
            let ptr = writer.inner.ring_ptr;
            let end = writer.inner.ring_cap;
            let slice_all = slice::from_raw_parts(ptr, end);
            // Verify tail at [54..64)
            assert_eq!(&slice_all[54..64], &data[0..10]);
            // Verify wrap at [0..6)
            assert_eq!(&slice_all[0..6], &data[10..16]);
        }

        cleanup_ring(&path);
    }

    #[test]
    fn test_write_payload_padding_wraps_across_end() {
        // Exercise zero-padding wrap branch where pad spills into start of ring
        let path = test_ring_path();
        cleanup_ring(&path);

        let writer = RingWriter::create(&path, 64).unwrap();
        let w = 51; // pos=51; with len=13 (pad=3), pos+len=64, pad wraps 3 bytes to start
        let data: Vec<u8> = (0u8..13u8).collect();

        writer.write_payload(w, &data);

        unsafe {
            let ptr = writer.inner.ring_ptr;
            let end = writer.inner.ring_cap;
            let slice_all = slice::from_raw_parts(ptr, end);
            // Data ends exactly at ring end
            assert_eq!(&slice_all[51..64], &data[..]);
            // Padding wraps to start: first 3 bytes must be zero
            assert_eq!(&slice_all[0..3], &[0u8, 0u8, 0u8]);
        }

        cleanup_ring(&path);
    }

    #[test]
    fn test_read_payload_wraps_across_end() {
        // Write known bytes spanning the end, then read them back via read_payload
        let path = test_ring_path();
        cleanup_ring(&path);
        let writer = RingWriter::create(&path, 64).unwrap();
        let reader = RingReader::open(&path).unwrap();

        unsafe {
            let ptr = writer.inner.ring_ptr;
            let end = writer.inner.ring_cap;
            // Place 10 bytes at end and 6 at start
            let tail: [u8; 10] = [1, 2, 3, 4, 5, 6, 7, 8, 9, 10];
            let head: [u8; 6] = [11, 12, 13, 14, 15, 16];
            ptr::copy_nonoverlapping(tail.as_ptr(), ptr.add(end - 10), 10);
            ptr::copy_nonoverlapping(head.as_ptr(), ptr, 6);
        }
        let mut out = vec![0u8; 16];
        // Start reading at r=end-10 so it wraps
        reader
            .inner
            .read_payload(writer.inner.ring_cap - 10, &mut out);
        assert_eq!(
            out,
            vec![1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11, 12, 13, 14, 15, 16]
        );

        cleanup_ring(&path);
    }

    #[test]
    fn test_try_pop_not_ready_returns_none() {
        // Force write>read but header at read offset is not READY => Ok(None)
        let path = test_ring_path();
        cleanup_ring(&path);
        let writer = RingWriter::create(&path, 128).unwrap();
        let mut reader = RingReader::open(&path).unwrap();

        unsafe {
            let hdr = &*writer.inner.hdr;
            // Place read at 0, write at 8 so write>read
            hdr.read.store(0, Ordering::Relaxed);
            hdr.write.store(8, Ordering::Relaxed);
            // At r=0, write a header with len but WITHOUT READY bit
            writer.inner.write_header(0, 12); // 12 bytes
                                              // Do not publish (no READY); reader should see not READY and return None
        }

        let mut buf = Vec::new();
        let res = reader.try_pop(&mut buf).unwrap();
        assert!(res.is_none(), "expected None when header not READY");

        cleanup_ring(&path);
    }

    #[test]
    fn test_push_propagates_non_full_error() {
        // push() should propagate TooLarge directly (covers Err(e) arm)
        let path = test_ring_path();
        cleanup_ring(&path);
        let mut writer = RingWriter::create(&path, 1024).unwrap();
        let huge = vec![0u8; 4096];
        let err = writer
            .push(&huge, Some(Duration::from_millis(1)))
            .unwrap_err();
        assert!(matches!(err, IpcError::TooLarge));
        cleanup_ring(&path);
    }

    #[test]
    fn test_try_push_full_due_to_wrap_space_check() {
        // Craft hdr.read/write to force: space >= need but space < gap + need -> Full in wrap branch
        let path = test_ring_path();
        cleanup_ring(&path);
        let mut writer = RingWriter::create(&path, 64).unwrap();

        let payload = vec![0u8; 16]; // need = align_up(20,4)=20
        unsafe {
            let hdr = &*writer.inner.hdr;
            // Set write=56 (w=56), read=16 => used=40, space=24
            hdr.write.store(56, Ordering::Relaxed);
            hdr.read.store(16, Ordering::Relaxed);
        }
        let err = writer.try_push(&payload).unwrap_err();
        assert!(matches!(err, IpcError::Full));
        cleanup_ring(&path);
    }

    #[test]
    fn test_simple_error_and_converter() {
        // Cover Display and the to_send_sync_error adapter
        let e = SimpleError("hello".to_string());
        assert_eq!(format!("{}", e), "hello");
        let boxed: Box<dyn std::error::Error> = Box::new(SimpleError("x".to_string()));
        let send_sync = to_send_sync_error(boxed.as_ref());
        // Type assertion: must be Send + Sync
        fn assert_send_sync(_: &(dyn std::error::Error + Send + Sync)) {}
        assert_send_sync(&*send_sync);
    }

    #[test]
    fn test_capacity_power_of_two() {
        let path = test_ring_path();
        cleanup_ring(&path);

        // Should work with power of 2
        let _writer = RingWriter::create(&path, 4096).unwrap();
        cleanup_ring(&path);
    }

    #[test]
    #[should_panic]
    fn test_capacity_not_power_of_two_panics() {
        let path = test_ring_path();
        cleanup_ring(&path);

        // Should panic with non-power of 2
        let _writer = RingWriter::create(&path, 4097);
    }

    #[test]
    fn test_sequence_integrity() {
        let path = test_ring_path();
        cleanup_ring(&path);

        let mut writer = RingWriter::create(&path, 8192).unwrap();
        let mut reader = RingReader::open(&path).unwrap();
        let mut buf = Vec::new();

        // Send messages with sequence numbers
        for i in 0..100 {
            let msg = format!("message_{i}");
            writer.try_push(msg.as_bytes()).unwrap();
        }

        // Verify sequence integrity
        for i in 0..100 {
            let n = reader.try_pop(&mut buf).unwrap();
            assert!(n.is_some());
            let expected = format!("message_{i}");
            assert_eq!(&buf[..], expected.as_bytes());
        }

        cleanup_ring(&path);
    }
}
