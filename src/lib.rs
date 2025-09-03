//! ipc_ring: mmap-backed SPSC shared-memory ring for Unix (Linux + macOS).
//! - Generic: carries raw bytes
//! - No compression, no JSON assumptions
//! - One writer process <-> one reader process

#![cfg(unix)]

use memmap2::{MmapMut, MmapOptions};
use raw_sync::events::{EventInit, EventState};
use raw_sync::Timeout;
use std::fs::{File, OpenOptions};
use std::io;
use std::mem::{align_of, size_of};
use std::os::unix::fs::OpenOptionsExt;
use std::path::Path;
use std::ptr;
use std::sync::atomic::{fence, AtomicU32, AtomicU64, Ordering};
use std::time::Duration;
use thiserror::Error;

const MAGIC: u64 = 0x49504352494E4731; // "IPCRING1"
const READY: u32 = 1 << 31;
const HDR_ALIGN: usize = 64;

#[inline]
fn align_up(x: usize, a: usize) -> usize {
    (x + a - 1) & !(a - 1)
}

#[repr(C)]
struct Header {
    magic: u64,
    cap: u64,         // ring capacity in bytes (power of two)
    write: AtomicU64, // monotonically increasing
    read: AtomicU64,  // monotonically increasing
    _pad: [u8; 64 - 8 - 8 - 8 - 8],
}

// Layout in the mapping:
// [Header][Event(data_avail)][Event(space_avail)][padding to 64B][Ring bytes...]
pub struct RingWriter {
    file: File,
    map: MmapMut,
    hdr: *mut Header,
    data_avail: Box<dyn raw_sync::events::EventImpl>,
    space_avail: Box<dyn raw_sync::events::EventImpl>,
    ring_ptr: *mut u8,
    ring_cap: usize,
    mask: usize,
}

pub struct RingReader {
    file: File,
    map: MmapMut,
    hdr: *mut Header,
    data_avail: Box<dyn raw_sync::events::EventImpl>,
    space_avail: Box<dyn raw_sync::events::EventImpl>,
    ring_ptr: *mut u8,
    ring_cap: usize,
    mask: usize,
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

fn to_send_sync_error(err: Box<dyn std::error::Error>) -> Box<dyn std::error::Error + Send + Sync> {
    Box::new(SimpleError(err.to_string()))
}

impl RingWriter {
    /// Create a new ring mapping at `path`. Fails if the file exists.
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
        unsafe { Self::init_mapping(file, map, cap_pow2) }
    }

    unsafe fn init_mapping(file: File, mut map: MmapMut, cap: usize) -> Result<Self, IpcError> {
        let base = map.as_mut_ptr();
        let mut off = 0usize;

        // Header
        let hdr_ptr = base.add(off) as *mut Header;
        ptr::write(
            hdr_ptr,
            Header {
                magic: MAGIC,
                cap: cap as u64,
                write: AtomicU64::new(0),
                read: AtomicU64::new(0),
                _pad: [0; 64 - 32],
            },
        );
        off += align_up(size_of::<Header>(), HDR_ALIGN);

        // Events (manual-reset=true)
        let (data_evt, used1) = raw_sync::events::Event::new(base.add(off), true)
            .map_err(|e| IpcError::Event(to_send_sync_error(e)))?;
        off += align_up(used1, align_of::<usize>());
        let (space_evt, used2) = raw_sync::events::Event::new(base.add(off), true)
            .map_err(|e| IpcError::Event(to_send_sync_error(e)))?;
        off += align_up(used2, align_of::<usize>());
        off = align_up(off, HDR_ALIGN);

        let ring_ptr = base.add(off);
        let ring_cap = align_up(cap, HDR_ALIGN);

        Ok(Self {
            file,
            map,
            hdr: hdr_ptr,
            data_avail: data_evt,
            space_avail: space_evt,
            ring_ptr,
            ring_cap,
            mask: ring_cap - 1,
        })
    }

    /// Try to push without blocking.
    pub fn try_push(&mut self, payload: &[u8]) -> Result<(), IpcError> {
        if payload.len() + 4 > self.ring_cap {
            return Err(IpcError::TooLarge);
        }
        
        debug_assert!(self.ring_cap.is_power_of_two(), "ring_cap must be power of 2");
        debug_assert_eq!(self.mask, self.ring_cap - 1, "mask must equal ring_cap - 1");

        let hdr = unsafe { &*self.hdr };
        let read = hdr.read.load(Ordering::Acquire);
        let mut cur_write = hdr.write.load(Ordering::Relaxed);
    let used = cur_write - read;
    let space = self.ring_cap as u64 - used;
        let need = align_up(payload.len() + 4, 4) as u64;

        if space < need {
            return Err(IpcError::Full);
        }

        let mut w = cur_write as usize & self.mask;

        // Ensure header is contiguous; if not, write a wrap marker (len=0|READY)
        if w + 4 > self.ring_cap || w + 4 + payload.len() > self.ring_cap {
            // Need to wrap; ensure we have space for the wrap marker (gap to end) + message
            let gap = (self.ring_cap - w) as u64;
            if space < gap + need {
                return Err(IpcError::Full);
            }

            // Emit wrap marker and advance write pointer to start
            self.write_header(w, 0);
            self.publish_header(w, 0);
            cur_write = cur_write.wrapping_add(gap);
            hdr.write.store(cur_write, Ordering::Release);
            // Wake the reader so it can consume the wrap marker promptly
            let _ = self
                .data_avail
                .set(EventState::Signaled)
                .map_err(|e| IpcError::Event(to_send_sync_error(e)));

            w = 0;  // After wrap marker, next write starts at beginning
        }

        // Write header (no READY), copy payload, then publish (set READY)
        self.write_header(w, payload.len() as u32);
        self.write_payload(w + 4, payload);
        self.publish_header(w, payload.len() as u32);

        let bump = align_up(4 + payload.len(), 4) as u64;
        cur_write = cur_write.wrapping_add(bump);
        hdr.write.store(cur_write, Ordering::Release);

        // Signal data available
        self.data_avail
            .set(EventState::Signaled)
            .map_err(|e| IpcError::Event(to_send_sync_error(e)))?;
        Ok(())
    }

    /// Push with optional timeout (None = infinite).
    pub fn push(&mut self, payload: &[u8], timeout: Option<Duration>) -> Result<(), IpcError> {
        loop {
            match self.try_push(payload) {
                Ok(()) => return Ok(()),
                Err(IpcError::Full) => {
                    let to = timeout.map(Timeout::Val).unwrap_or(Timeout::Infinite);
                    self.space_avail
                        .wait(to)
                        .map_err(|e| IpcError::Event(to_send_sync_error(e)))?;
                }
                Err(e) => return Err(e),
            }
        }
    }

    #[inline]
    fn write_header(&mut self, w: usize, len: u32) {
        let pos = w & self.mask;
        debug_assert!(pos < self.ring_cap, "write position {} >= ring_cap {}", pos, self.ring_cap);
        let p = unsafe { self.ring_ptr.add(pos) as *const AtomicU32 as *mut AtomicU32 };
        unsafe {
            (&*p).store(len, Ordering::Relaxed);
        }
    }
    #[inline]
    fn publish_header(&mut self, w: usize, len: u32) {
        fence(Ordering::Release);
        let pos = w & self.mask;
        debug_assert!(pos < self.ring_cap, "publish position {} >= ring_cap {}", pos, self.ring_cap);
        let p = unsafe { self.ring_ptr.add(pos) as *const AtomicU32 as *mut AtomicU32 };
        unsafe {
            (&*p).store(len | READY, Ordering::Release);
        }
    }
    #[inline]
    fn write_payload(&mut self, w: usize, data: &[u8]) {
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
                        pad - first_pad
                    );
                }
            }
        }
    }
}

impl RingReader {
    /// Open an existing ring mapping at `path`.
    pub fn open<P: AsRef<Path>>(path: P) -> Result<Self, IpcError> {
        let file = OpenOptions::new().read(true).write(true).open(path)?;
        let len = file.metadata()?.len() as usize;
        let map = unsafe { MmapOptions::new().len(len).map_mut(&file)? };

        // Reconstruct layout
        unsafe {
            let base = map.as_ptr() as *mut u8;
            let mut off = 0usize;

            let hdr = &*(base as *const Header);
            if hdr.magic != MAGIC {
                return Err(IpcError::Layout);
            }
            let cap = hdr.cap as usize;

            let hdr_ptr = base as *mut Header;
            off += align_up(size_of::<Header>(), HDR_ALIGN);

            let (data_evt, used1) = raw_sync::events::Event::from_existing(base.add(off))
                .map_err(|e| IpcError::Event(to_send_sync_error(e)))?;
            off += align_up(used1, align_of::<usize>());
            let (space_evt, used2) = raw_sync::events::Event::from_existing(base.add(off))
                .map_err(|e| IpcError::Event(to_send_sync_error(e)))?;
            off += align_up(used2, align_of::<usize>());
            off = align_up(off, HDR_ALIGN);

            let ring_ptr = base.add(off);
            let ring_cap = align_up(cap, HDR_ALIGN);

            Ok(Self {
                file,
                map,
                hdr: hdr_ptr,
                data_avail: data_evt,
                space_avail: space_evt,
                ring_ptr,
                ring_cap,
                mask: ring_cap - 1,
            })
        }
    }

    /// Try to pop without blocking. On success returns Some(bytes_written_into_out).
    pub fn try_pop(&mut self, out: &mut Vec<u8>) -> Result<Option<usize>, IpcError> {
        let hdr = unsafe { &*self.hdr };
        let write = hdr.write.load(Ordering::Acquire);
        let read = hdr.read.load(Ordering::Relaxed);
        if write == read {
            return Ok(None);
        }

        let r = read as usize & self.mask;
        // Read header with Acquire (ensures payload visibility).
        let h = self.read_header(r);
        if (h & READY) == 0 {
            // Not published yet: let writer proceed.
            return Ok(None);
        }
        let size = (h & !READY) as usize;

        if size == 0 {
            // Wrap marker
            let bump = (self.ring_cap - r) as u64;
            hdr.read.store(read + bump, Ordering::Release);
            self.space_avail
                .set(EventState::Signaled)
                .map_err(|e| IpcError::Event(to_send_sync_error(e)))?;
            return self.try_pop(out);
        }

        out.resize(size, 0);
        self.read_payload(r + 4, &mut out[..]);

        let bump = align_up(4 + size, 4) as u64;
        hdr.read.store(read + bump, Ordering::Release);

        // Signal writer that there is room
        self.space_avail
            .set(EventState::Signaled)
            .map_err(|e| IpcError::Event(to_send_sync_error(e)))?;
        Ok(Some(size))
    }

    /// Blocking pop with optional timeout (None = infinite).
    pub fn pop(&mut self, out: &mut Vec<u8>, timeout: Option<Duration>) -> Result<usize, IpcError> {
        loop {
            if let Some(n) = self.try_pop(out)? {
                return Ok(n);
            }
            let to = timeout.map(Timeout::Val).unwrap_or(Timeout::Infinite);
            self.data_avail
                .wait(to)
                .map_err(|e| IpcError::Event(to_send_sync_error(e)))?;
        }
    }

    #[inline]
    fn read_header(&self, r: usize) -> u32 {
        let pos = r & self.mask;
        let p = unsafe { self.ring_ptr.add(pos) as *const AtomicU32 };
        unsafe { (&*p).load(Ordering::Acquire) }
    }
    #[inline]
    fn read_payload(&mut self, r: usize, out: &mut [u8]) {
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

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

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
                Err(e) => panic!("Unexpected error: {:?}", e),
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
            let msg = format!("message_{}", i);
            writer.try_push(msg.as_bytes()).unwrap();
        }

        // Verify sequence integrity
        for i in 0..100 {
            let n = reader.try_pop(&mut buf).unwrap();
            assert!(n.is_some());
            let expected = format!("message_{}", i);
            assert_eq!(&buf[..], expected.as_bytes());
        }

        cleanup_ring(&path);
    }
}
