use ipc_ring::{RingReader, RingWriter};
use std::sync::atomic::{AtomicU32, Ordering};
use std::thread;
use std::time::Duration;
use std::{fs, path::PathBuf};

static TEST_COUNTER: AtomicU32 = AtomicU32::new(0);

fn test_ring_path() -> PathBuf {
    let n = TEST_COUNTER.fetch_add(1, Ordering::SeqCst);
    PathBuf::from(format!(
        "/tmp/ipc_ring_correctness_{}_{}",
        std::process::id(),
        n
    ))
}

fn cleanup_ring(path: &PathBuf) {
    let _ = fs::remove_file(path);
}

#[test]
fn test_streaming_correctness_small_capacity() {
    // Ensure all messages arrive and in order, even when capacity << total bytes sent
    let path = test_ring_path();
    cleanup_ring(&path);

    let cap = 64 * 1024; // 64 KiB ring
    let msg_size = 256; // fixed size, includes 8-byte seq at start
    let messages = 20_000; // enough to force many wrap-arounds but fast in CI
    let op_timeout = Duration::from_secs(5);

    // Writer creates the ring
    let mut writer = RingWriter::create(&path, cap).expect("create ring");

    // Reader thread opens and consumes
    let reader_path = path.clone();
    let reader_handle = thread::spawn(move || {
        let mut reader = RingReader::open(&reader_path).expect("open reader");
        let mut buf = Vec::with_capacity(msg_size);
        for expected_seq in 0..messages {
            let _n = reader.pop(&mut buf, Some(op_timeout)).expect("pop ok");
            assert!(buf.len() >= 8, "message too small");
            let mut id = [0u8; 8];
            id.copy_from_slice(&buf[0..8]);
            let got = u64::from_le_bytes(id) as usize;
            assert_eq!(
                got, expected_seq,
                "out of order or drop: got {got} expected {expected_seq}"
            );
        }
    });

    // Writer thread (on main test thread)
    let mut payload = vec![0u8; msg_size];
    for i in 0..messages {
        payload[0..8].copy_from_slice(&(i as u64).to_le_bytes());
        // Fill remainder with a pattern (optional correctness)
        for (idx, byte) in payload.iter_mut().enumerate().skip(8) {
            *byte = (idx & 0xFF) as u8;
        }
        writer.push(&payload, Some(op_timeout)).expect("push ok");
    }

    // Join reader and cleanup
    reader_handle.join().expect("reader join");
    cleanup_ring(&path);
}

#[test]
fn test_wrap_marker_path_is_consumed() {
    // Forces wrap markers frequently and ensures reader continues across boundaries
    let path = test_ring_path();
    cleanup_ring(&path);

    let cap = 4096; // small to force frequent wraps
    let msg_size = 300; // deliberately not divisible by 4 to exercise padding
    let messages = 5_000;
    let op_timeout = Duration::from_secs(5);

    let mut writer = RingWriter::create(&path, cap).expect("create ring");
    let reader_path = path.clone();

    let reader = thread::spawn(move || {
        let mut reader = RingReader::open(&reader_path).expect("open reader");
        let mut buf = Vec::with_capacity(msg_size);
        for expected_seq in 0..messages {
            let _ = reader.pop(&mut buf, Some(op_timeout)).expect("pop ok");
            let mut id = [0u8; 8];
            id.copy_from_slice(&buf[0..8]);
            let got = u64::from_le_bytes(id) as usize;
            assert_eq!(
                got, expected_seq,
                "wrap marker traversal failed: got {got} expected {expected_seq}"
            );
        }
    });

    let mut payload = vec![0u8; msg_size];
    for i in 0..messages {
        payload[0..8].copy_from_slice(&(i as u64).to_le_bytes());
        writer.push(&payload, Some(op_timeout)).expect("push ok");
    }

    reader.join().expect("reader join");
    cleanup_ring(&path);
}
