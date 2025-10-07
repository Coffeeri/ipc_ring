#![cfg(feature = "failpoints")]

use ipc_ring::{failpoints_enabled, RingReader, RingWriter};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU32, Ordering};
use std::{thread, time::Duration};

const PAYLOAD: &[u8] = b"failpoint message";

struct FailpointGuard {
    name: &'static str,
}

impl FailpointGuard {
    fn new(name: &'static str) -> Self {
        fail::cfg(name, "panic").unwrap();
        Self { name }
    }
}

impl Drop for FailpointGuard {
    fn drop(&mut self) {
        fail::remove(self.name);
    }
}

#[derive(Clone, Copy)]
struct WriterCase {
    name: &'static str,
    needs_wrap: bool,
    expect_message_visible: bool,
}

#[derive(Clone, Copy)]
struct ReaderCase {
    name: &'static str,
    needs_wrap: bool,
    message_survives: bool,
}

fn shared_tmp_dir() -> PathBuf {
    #[cfg(target_os = "linux")]
    {
        let shm = Path::new("/dev/shm");
        if shm.exists() && shm.is_dir() {
            return shm.to_path_buf();
        }
    }
    PathBuf::from("/tmp")
}

fn unique_ring_path() -> PathBuf {
    static COUNTER: AtomicU32 = AtomicU32::new(0);
    let dir = shared_tmp_dir();
    dir.join(format!(
        "ipc_ring_failpoint_{}_{}",
        std::process::id(),
        COUNTER.fetch_add(1, Ordering::SeqCst)
    ))
}

fn cleanup(path: &Path) {
    let _ = std::fs::remove_file(path);
}

fn prime_wrap(writer: &mut RingWriter, reader: &mut RingReader) {
    let mut buf = Vec::new();
    let prep = vec![0u8; 24];
    for _ in 0..2 {
        writer.try_push(&prep).expect("prime push");
        let observed = reader.try_pop(&mut buf).expect("prime pop");
        assert_eq!(observed, Some(prep.len()), "prime pop length mismatch");
        buf.clear();
    }
}

fn assert_no_payload_present(path: &Path) {
    let mut reader = RingReader::open(path).expect("verify reader open");
    let mut buf = Vec::new();
    while let Some(_n) = reader.try_pop(&mut buf).expect("verify pop") {
        assert_ne!(
            buf.as_slice(),
            PAYLOAD,
            "unexpected payload observed during verification"
        );
        buf.clear();
    }
}

fn run_writer_case(case: WriterCase) {
    assert!(
        failpoints_enabled(),
        "crate compiled without failpoints feature"
    );

    let path = unique_ring_path();
    cleanup(&path);

    let mut writer = RingWriter::create(&path, 64).expect("create ring");
    let mut reader = RingReader::open(&path).expect("open ring");

    if case.needs_wrap {
        prime_wrap(&mut writer, &mut reader);
    }

    {
        let _guard = FailpointGuard::new(case.name);
        let smoke = std::panic::catch_unwind(|| {
            fail::fail_point!(case.name);
        });
        assert!(smoke.is_err(), "failpoint {} did not panic", case.name);

        let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            writer.try_push(PAYLOAD).expect("writer push failed");
        }));
        assert!(
            result.is_err(),
            "failpoint {} did not trigger during push",
            case.name
        );
    }

    let mut buf = Vec::new();
    let observed = reader
        .try_pop(&mut buf)
        .expect("reader pop after failpoint");
    if case.expect_message_visible {
        assert_eq!(
            observed,
            Some(PAYLOAD.len()),
            "failpoint {} expected message visibility",
            case.name
        );
        assert_eq!(buf, PAYLOAD, "failpoint {} payload mismatch", case.name);
        buf.clear();
        assert!(
            reader.try_pop(&mut buf).expect("second pop").is_none(),
            "ring not empty after consuming message for {}",
            case.name
        );
    } else {
        assert!(
            observed.is_none(),
            "failpoint {} unexpectedly left readable data",
            case.name
        );
    }

    cleanup(&path);
}

fn run_reader_case(case: ReaderCase) {
    assert!(
        failpoints_enabled(),
        "crate compiled without failpoints feature"
    );

    let path = unique_ring_path();
    cleanup(&path);

    let mut writer = RingWriter::create(&path, 64).expect("create ring");
    let mut reader = RingReader::open(&path).expect("open ring");

    if case.needs_wrap {
        prime_wrap(&mut writer, &mut reader);
    }

    writer.try_push(PAYLOAD).expect("prepare payload");

    {
        let _guard = FailpointGuard::new(case.name);
        let smoke = std::panic::catch_unwind(|| {
            fail::fail_point!(case.name);
        });
        assert!(smoke.is_err(), "failpoint {} did not panic", case.name);

        let mut buf = Vec::new();
        let result =
            std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| reader.try_pop(&mut buf)));
        assert!(
            result.is_err(),
            "failpoint {} did not trigger during pop",
            case.name
        );
    }

    drop(reader);

    let mut reader = RingReader::open(&path).expect("reopen reader");
    let mut buf = Vec::new();
    let observed = reader
        .try_pop(&mut buf)
        .expect("reader pop after failpoint");
    if case.message_survives {
        assert_eq!(
            observed,
            Some(PAYLOAD.len()),
            "failpoint {} should leave payload readable",
            case.name
        );
        assert_eq!(buf, PAYLOAD, "failpoint {} payload mismatch", case.name);
        buf.clear();
        assert!(
            reader.try_pop(&mut buf).expect("second pop").is_none(),
            "ring not empty after replay for {}",
            case.name
        );
    } else {
        assert!(
            observed.is_none(),
            "failpoint {} should have consumed payload",
            case.name
        );
    }

    cleanup(&path);
}

#[test]
fn writer_failpoints_cover_crash_windows() {
    let _scenario = fail::FailScenario::setup();
    let cases = [
        WriterCase {
            name: "ring_writer::after_wrap_publish",
            needs_wrap: true,
            expect_message_visible: false,
        },
        WriterCase {
            name: "ring_writer::after_wrap_advance",
            needs_wrap: true,
            expect_message_visible: false,
        },
        WriterCase {
            name: "ring_writer::after_wrap_signal",
            needs_wrap: true,
            expect_message_visible: false,
        },
        WriterCase {
            name: "ring_writer::after_write_header",
            needs_wrap: false,
            expect_message_visible: false,
        },
        WriterCase {
            name: "ring_writer::after_write_payload",
            needs_wrap: false,
            expect_message_visible: false,
        },
        WriterCase {
            name: "ring_writer::after_publish_header",
            needs_wrap: false,
            expect_message_visible: false,
        },
        WriterCase {
            name: "ring_writer::after_write_advance",
            needs_wrap: false,
            expect_message_visible: true,
        },
        WriterCase {
            name: "ring_writer::after_data_signal",
            needs_wrap: false,
            expect_message_visible: true,
        },
    ];

    for case in cases {
        run_writer_case(case);
    }
}

#[test]
fn writer_wait_failpoints_handle_crash() {
    let _scenario = fail::FailScenario::setup();
    assert!(failpoints_enabled(), "failpoints feature disabled");

    // before_space_wait: panic before blocking; ring stays full, no new payload.
    {
        let path = unique_ring_path();
        cleanup(&path);
        let mut writer = RingWriter::create(&path, 64).expect("create ring");

        let filler = vec![0xAA; 28];
        while writer.try_push(&filler).is_ok() {}

        let reader = RingReader::open(&path).expect("open reader for verification");
        drop(reader); // ensure no outstanding borrow across panic.

        let guard = FailpointGuard::new("ring_writer::before_space_wait");
        let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            writer
                .push(PAYLOAD, Some(Duration::from_millis(50)))
                .expect("push should panic via failpoint");
        }));
        drop(guard);
        assert!(
            result.is_err(),
            "before_space_wait failpoint did not trigger panic"
        );

        drop(writer);
        assert_no_payload_present(&path);
        cleanup(&path);
    }

    // after_space_wait: panic immediately after wait succeeds; payload not committed.
    {
        let path = unique_ring_path();
        cleanup(&path);
        let mut writer = RingWriter::create(&path, 64).expect("create ring");

        let filler = vec![0xBB; 24];
        while writer.try_push(&filler).is_ok() {}

        let guard = FailpointGuard::new("ring_writer::after_space_wait");
        let path_for_reader = path.clone();
        let handle = thread::spawn(move || {
            let mut reader = RingReader::open(&path_for_reader).expect("spawn reader open");
            let mut buf = Vec::new();
            loop {
                if let Some(_n) = reader.try_pop(&mut buf).expect("spawn reader pop") {
                    break;
                }
                thread::sleep(Duration::from_millis(5));
            }
        });

        let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            writer
                .push(PAYLOAD, Some(Duration::from_millis(500)))
                .expect("push should panic via failpoint");
        }));
        drop(guard);
        assert!(
            result.is_err(),
            "after_space_wait failpoint did not trigger panic"
        );

        handle.join().expect("reader thread join");
        drop(writer);
        assert_no_payload_present(&path);
        cleanup(&path);
    }
}

#[test]
fn reader_failpoints_cover_crash_windows() {
    let _scenario = fail::FailScenario::setup();
    let cases = [
        ReaderCase {
            name: "ring_reader::after_wrap_read_advance",
            needs_wrap: true,
            message_survives: true,
        },
        ReaderCase {
            name: "ring_reader::after_wrap_space_signal",
            needs_wrap: true,
            message_survives: true,
        },
        ReaderCase {
            name: "ring_reader::before_read_advance",
            needs_wrap: false,
            message_survives: true,
        },
        ReaderCase {
            name: "ring_reader::after_read_advance",
            needs_wrap: false,
            message_survives: false,
        },
        ReaderCase {
            name: "ring_reader::after_space_signal",
            needs_wrap: false,
            message_survives: false,
        },
    ];

    for case in cases {
        run_reader_case(case);
    }
}

#[test]
fn reader_wait_failpoints_handle_crash() {
    let _scenario = fail::FailScenario::setup();
    assert!(failpoints_enabled(), "failpoints feature disabled");

    // before_data_wait: panic before blocking, ring unchanged.
    {
        let path = unique_ring_path();
        cleanup(&path);
        let writer = RingWriter::create(&path, 64).expect("create ring");
        drop(writer);

        let mut reader = RingReader::open(&path).expect("open reader");
        let guard = FailpointGuard::new("ring_reader::before_data_wait");
        let mut buf = Vec::new();
        let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            reader
                .pop(&mut buf, Some(Duration::from_millis(50)))
                .expect("pop should panic via failpoint");
        }));
        drop(guard);
        assert!(
            result.is_err(),
            "before_data_wait failpoint did not trigger panic"
        );
        drop(reader);

        let mut verify_reader = RingReader::open(&path).expect("verify reader open");
        assert!(
            verify_reader
                .try_pop(&mut buf)
                .expect("verify pop")
                .is_none(),
            "unexpected payload after before_data_wait panic"
        );
        cleanup(&path);
    }

    // after_data_wait: panic immediately after wait returns; payload remains for replay.
    {
        let path = unique_ring_path();
        cleanup(&path);
        let writer = RingWriter::create(&path, 64).expect("create ring");
        let mut reader = RingReader::open(&path).expect("open reader");

        let handle = thread::spawn(move || {
            let mut writer = writer;
            thread::sleep(Duration::from_millis(20));
            writer.push(PAYLOAD, None).expect("writer push");
        });

        let guard = FailpointGuard::new("ring_reader::after_data_wait");
        let mut buf = Vec::new();
        let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            reader
                .pop(&mut buf, Some(Duration::from_millis(500)))
                .expect("pop should panic via failpoint");
        }));
        drop(guard);
        assert!(
            result.is_err(),
            "after_data_wait failpoint did not trigger panic"
        );

        handle.join().expect("writer thread join");
        drop(reader);

        let mut verify_reader = RingReader::open(&path).expect("verify reader open");
        let mut verify_buf = Vec::new();
        let observed = verify_reader
            .try_pop(&mut verify_buf)
            .expect("verify pop after retry");
        assert_eq!(
            observed,
            Some(PAYLOAD.len()),
            "payload not preserved after after_data_wait panic"
        );
        assert_eq!(verify_buf.as_slice(), PAYLOAD);
        cleanup(&path);
    }
}

#[test]
fn writer_create_failpoint_leaves_valid_ring() {
    let _scenario = fail::FailScenario::setup();
    assert!(failpoints_enabled(), "failpoints feature disabled");

    let path = unique_ring_path();
    cleanup(&path);

    {
        let _guard = FailpointGuard::new("ring_writer::create::after_init");
        let result = std::panic::catch_unwind(|| {
            RingWriter::create(&path, 4096).expect("create should panic via failpoint");
        });
        assert!(
            result.is_err(),
            "create failpoint did not produce panic as expected"
        );
    }

    // After the panic, the ring layout is still initialized. Readers can reopen.
    let mut reader = RingReader::open(&path).expect("reader should open after writer crash");
    let mut buf = Vec::new();
    assert!(
        reader.try_pop(&mut buf).expect("post-crash pop").is_none(),
        "ring unexpectedly contains data after create failpoint"
    );

    cleanup(&path);
}

#[test]
fn reader_open_failpoint_allows_retry() {
    let _scenario = fail::FailScenario::setup();
    assert!(failpoints_enabled(), "failpoints feature disabled");

    let path = unique_ring_path();
    cleanup(&path);

    let writer = RingWriter::create(&path, 4096).expect("create ring");
    drop(writer);

    {
        let _guard = FailpointGuard::new("ring_reader::open::after_map");
        let result = std::panic::catch_unwind(|| {
            RingReader::open(&path).expect("open should panic via failpoint");
        });
        assert!(
            result.is_err(),
            "reader open failpoint did not produce panic as expected"
        );
    }

    // With the failpoint removed the reader should open cleanly.
    let mut reader = RingReader::open(&path).expect("retry reader open");
    let mut buf = Vec::new();
    assert!(
        reader.try_pop(&mut buf).expect("post-retry pop").is_none(),
        "ring unexpectedly contains data after reader open retry"
    );

    cleanup(&path);
}
