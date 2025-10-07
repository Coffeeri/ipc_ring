use ipc_ring::{RingReader, RingWriter};
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::{mpsc, Arc, Barrier};
use std::thread;
use std::time::Duration;

fn unique_path() -> String {
    static COUNTER: AtomicU32 = AtomicU32::new(0);
    format!(
        "/tmp/ipc_ring_stream_{}_{}",
        std::process::id(),
        COUNTER.fetch_add(1, Ordering::SeqCst)
    )
}

fn cleanup(path: &str) {
    let _ = std::fs::remove_file(path);
}

#[test]
fn reader_receives_stream_with_delays() {
    let path = unique_path();
    cleanup(&path);

    let mut writer = RingWriter::create(&path, 4096).expect("create writer");
    let reader_path = path.clone();

    let (tx, rx) = mpsc::channel();
    let barrier = Arc::new(Barrier::new(2));
    let reader_barrier = barrier.clone();

    let reader_handle = thread::spawn(move || {
        let mut reader = RingReader::open(&reader_path).expect("open reader");
        reader_barrier.wait();
        let mut buf = Vec::new();
        for _ in 0..3 {
            let n = reader.pop(&mut buf, None).expect("reader pop");
            tx.send(buf[..n].to_vec()).expect("send payload");
            buf.clear();
        }
    });

    let messages = [b"one".as_ref(), b"two".as_ref(), b"three".as_ref()];
    barrier.wait();
    for msg in messages.iter() {
        writer.push(msg, None).expect("writer push");
        thread::sleep(Duration::from_secs(1));
    }

    reader_handle.join().expect("reader join");
    let mut received = Vec::new();
    for _ in 0..3 {
        received.push(rx.recv().expect("receive payload"));
    }
    for (expected, got) in messages.iter().zip(received.iter()) {
        assert_eq!(expected.as_ref(), got.as_slice());
    }

    cleanup(&path);
}
