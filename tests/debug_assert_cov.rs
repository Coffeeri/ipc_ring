use ipc_ring::RingWriter;

#[test]
fn test_try_push_hits_debug_assert_line() {
    // Minimal push to ensure the debug_assert! in try_push is executed in debug/test builds.
    let path = format!("/tmp/ipc_ring_dbgassert_{}_{}", std::process::id(), 1);
    let _ = std::fs::remove_file(&path);
    let mut writer = RingWriter::create(&path, 1024).expect("create ring");
    let payload = b"hi there"; // small message
    writer.try_push(payload).expect("try_push ok");
    let _ = std::fs::remove_file(&path);
}
