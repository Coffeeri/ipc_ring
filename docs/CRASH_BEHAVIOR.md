# Crash Behavior Notes

This document enumerates the observable state changes when either side crashes at specific points in the SPSC ring protocol. References use 1-based line numbers.

## Writer Crash Sites
- **During `RingWriter::create`** (`src/lib.rs:258-299`): a crash before `RingMapping::init_new` completes can leave events uninitialized. Subsequent `RingReader::open` fails with `IpcError::Layout`. Recreating the ring file is the only recovery path.
- **After `write_header` / before `publish_header`** (`src/lib.rs:356-359`): header length is written without READY. Reader keeps returning `Ok(None)` and state stays consistent; the message is lost but no deadlock occurs.
- **After `publish_header` but before `hdr.write.store`** (`src/lib.rs:359-363`): READY is visible, yet `hdr.write` still equals the previous value. Readers never see `write > read`, so data is silently dropped; future pushes overwrite the same slot.
- **After `hdr.write.store` but before `data_avail.set`** (`src/lib.rs:361-369`): indices already advanced, but the wake-up signal is missing. The reader polls the publish token (`commit`) using a configurable interval (default 2 ms) and drains the message even without a signal (`tests/failpoints.rs:340`).
- **Wrap-marker path** (`src/lib.rs:333-353`): the same checkpoints apply. In particular, a crash after updating `hdr.write` to the post-wrap value but before signaling `data_avail` leaves the reader waiting.
- **While blocked in `space_avail.wait`** (`src/lib.rs:379-399`): crashing here simply frees the shared memory; no additional cleanup is attempted. The reader continues normally.
- **Immediately before blocking in `space_avail.wait`** (`src/lib.rs:379-399` via `ring_writer::before_space_wait`): the writer panics before releasing ownership; the ring remains full and no new payload is written.
- **Immediately after `space_avail.wait` returns** (`src/lib.rs:379-399` via `ring_writer::after_space_wait`): capacity has been freed, but the message is not published; existing data stays intact for later readers.
- **After copying payload bytes but before publishing** (`src/lib.rs:356-378` via `ring_writer::after_write_payload`): the payload body resides in the ring, yet the header lacks READY. Reader ignores it and subsequent writes overwrite the slot.
- **During creation after mapping is initialized** (`src/lib.rs:258-299` via `ring_writer::create::after_init`): header/events are ready; panic leaves a reusable ring file behind.

## Reader Crash Sites
- **During `RingReader::open`** (`src/lib.rs:420-431`): a crash mid-open only affects the crashing process; the mapping remains untouched.
- **After header read but before advancing `read`** (`src/lib.rs:440-472`): the consumer may have copied bytes but `hdr.read` still points to the previous offset, so the writer sees the ring as full. A restarted reader can re-open and consume the message again (duplicate delivery).
- **After `hdr.read.store` but before `space_avail.set`** (`src/lib.rs:471-478`): write index is drained, yet the writer waiting in `space_avail.wait` never receives a signal and can stall indefinitely. The wrap-marker branch (“size == 0”) exhibits the same failure window at `src/lib.rs:457-465`.
- **While blocked in `data_avail.wait`** (`src/lib.rs:489-499`): the reader uses a short poll interval (2 ms) when waiting indefinitely; if the publisher crashed after committing, the reader rechecks and drains the message without a signal.
- **Immediately before blocking in `data_avail.wait`** (`src/lib.rs:489-499` via `ring_reader::before_data_wait`): panic occurs prior to sleeping; ring state is untouched and no data is consumed.
- **Immediately after `data_avail.wait` returns** (`src.lib.rs:489-499` via `ring_reader::after_data_wait`): the wake-up event fires but the reader crashes before taking ownership; message remains available for the next reader.
- **During open after reconstructing the mapping** (`src/lib.rs:420-431` via `ring_reader::open::after_map`): panic leaves the file intact; retry succeeds.

## Observations
- Both sides rely on manual-reset events for wake-ups. Writer commits are additionally recorded via `commit`, allowing the reader to self-wake if a signal is missing. The poll interval can be tuned via the `IPC_RING_POLL_MS` environment variable or `RingReader::set_poll_interval`.
- The current API lacks failpoints or dependency injection, so deterministic crash testing requires either process-level control (fork/kill) or adding feature-gated hooks specifically for tests.
- Recovery from a writer crash requires recreating the ring because there is no `RingWriter::open` to reattach; reader crashes allow re-opening and replay.
