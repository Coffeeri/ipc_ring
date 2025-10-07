# Crash Behavior Notes

This document enumerates the observable state changes when either side crashes at specific points in the SPSC ring protocol. References use 1-based line numbers.

## Writer Crash Sites
- **During `RingWriter::create`** (`src/lib.rs:258-299`): a crash before `RingMapping::init_new` completes can leave events uninitialized. Subsequent `RingReader::open` fails with `IpcError::Layout`. Recreating the ring file is the only recovery path.
- **After `write_header` / before `publish_header`** (`src/lib.rs:356-359`): header length is written without READY. Reader keeps returning `Ok(None)` and state stays consistent; the message is lost but no deadlock occurs.
- **After `publish_header` but before `hdr.write.store`** (`src/lib.rs:359-363`): READY is visible, yet `hdr.write` still equals the previous value. Readers never see `write > read`, so data is silently dropped; future pushes overwrite the same slot.
- **After `hdr.write.store` but before `data_avail.set`** (`src/lib.rs:361-369`): indices already advanced, but the wake-up signal is missing. A blocking reader stuck inside `pop()` (`src/lib.rs:489-499`) will wait forever unless it uses timeouts. This is a deadlock risk that needs mitigation.
- **Wrap-marker path** (`src/lib.rs:333-353`): the same checkpoints apply. In particular, a crash after updating `hdr.write` to the post-wrap value but before signaling `data_avail` leaves the reader waiting.
- **While blocked in `space_avail.wait`** (`src/lib.rs:379-399`): crashing here simply frees the shared memory; no additional cleanup is attempted. The reader continues normally.

## Reader Crash Sites
- **During `RingReader::open`** (`src/lib.rs:420-431`): a crash mid-open only affects the crashing process; the mapping remains untouched.
- **After header read but before advancing `read`** (`src/lib.rs:440-472`): the consumer may have copied bytes but `hdr.read` still points to the previous offset, so the writer sees the ring as full. A restarted reader can re-open and consume the message again (duplicate delivery).
- **After `hdr.read.store` but before `space_avail.set`** (`src/lib.rs:471-478`): write index is drained, yet the writer waiting in `space_avail.wait` never receives a signal and can stall indefinitely. The wrap-marker branch (“size == 0”) exhibits the same failure window at `src/lib.rs:457-465`.
- **While blocked in `data_avail.wait`** (`src/lib.rs:489-499`): no shared state changes; the writer continues.

## Observations
- Both sides rely on manual-reset events for wake-ups. Any crash after updating indices but before signaling its counterpart creates a hang until a timeout or manual intervention occurs.
- The current API lacks failpoints or dependency injection, so deterministic crash testing requires either process-level control (fork/kill) or adding feature-gated hooks specifically for tests.
- Recovery from a writer crash requires recreating the ring because there is no `RingWriter::open` to reattach; reader crashes allow re-opening and replay.
