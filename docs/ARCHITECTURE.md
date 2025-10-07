# Architecture

ipc_ring is a UNIX-only, mmap-backed single-producer single-consumer (SPSC) ring buffer. A writer process and a reader process open the same file-backed shared memory mapping. Layout:

```
[Header (64 B aligned)][data_avail event][space_avail event][padding][ring bytes...]
```

## Header and invariants
- `magic` identifies a valid mapping.
- `cap` is the usable ring capacity in bytes; it must be a power of two.
- `write` and `read` are monotonically increasing byte offsets in the unbounded sequence space (never wrap). Masking (`index & (cap - 1)`) maps them onto the ring.
- `commit` mirrors `write` after the writer publishes a message; it allows the reader to detect committed data even if a wake-up signal is missed.

The writer owns updates to `write`/`commit`, the reader owns `read`. Every message is serialized as:
```
[offset = write & mask]
header: u32 len (READY bit cleared initially)
payload bytes (aligned to 4)
```
A zero-length header acts as a wrap marker and forces the reader to jump to the base.

## Writer algorithm (push)
1. Load `read`, compute used space, and ensure the payload fits (`space >= align_up(len+4,4)`); otherwise return `IpcError::Full`.
2. If the header or payload would cross the end of the ring, write a wrap marker: zero header + publish, advance `write` to the start, signal `data_avail`, and update `commit`.
3. Write the real header with READY cleared, copy payload, fence, set READY bit, then advance `write` and `commit`.
4. Signal `data_avail` to wake the reader. Blocking pushes wait on `space_avail`; if they return because the reader never signaled but `read` advanced, the writer retries. If `read` stays unchanged with an infinite wait, it returns `IpcError::PeerStalled`; finite waits surface `IpcError::Timeout`.

## Reader algorithm (pop)
1. Load `write`/`read`; if equal, ring is empty.
2. Read the header. If READY is clear, the reader yields (`try_pop` => `Ok(None)`), letting the writer finish.
3. If the header length is zero, it is a wrap marker: advance `read` to the start, signal `space_avail`, recurse.
4. Otherwise resize the output buffer, copy the payload, and advance `read`.
5. Signal `space_avail`. Update `last_commit` with the writer’s `commit` value.
6. Blocking `pop` waits on `data_avail`; it polls `commit` at a configurable interval (default 2 ms) so it can detect committed data even if the writer crashed before signalling. Finite waits yield `IpcError::Timeout` when no data arrives.

## Synchronisation primitives
`raw_sync::events::Event` provides manual-reset events for `data_avail` and `space_avail`. Manual reset keeps a writer or reader from missing a signal if multiple units of work arrive before the opposite side wakes up.

## Crash semantics
- Writer may crash before publishing (header READY bit unset): reader ignores the half-written message.
- Writer crash after publishing but before signalling: reader detects `commit > last_commit`, retries immediately, and drains the message.
- Writer crash after advancing `write` but without a reader present: writer self-wake loops eventually raise `PeerStalled`.
- Reader crash after consuming data but before signalling: writer self-wake loops detect unchanged `read` and eventually return `PeerStalled`.
- Wrap markers and indices survive crashes because offsets are monotonically increasing and validated against the `magic`/`cap` header on reopen.

## Capacity and payload sizing
- `cap` must be a power of two; the effective usable payload per message is limited to `cap - 4` (header alignment). Try push returns `IpcError::TooLarge` if a payload plus header cannot fit even in an empty ring.
- Payloads are aligned to 4 bytes for header reuse and wrap marker simplicity.
- Large capacities reduce the probability of back-pressure but increase mapping size and cache range; writers should choose a capacity that fits expected burst size.

## Tuning & configuration
- Both reader and writer expose `set_poll_interval` / `poll_interval`; default is 2 ms (`IPC_RING_POLL_MS` environment variable override). Smaller intervals detect missed wake-ups faster at the cost of spin activity.
- `push(payload, Some(timeout))` and `pop(buffer, Some(timeout))` integrate with `raw_sync::Timeout`; zero or negative durations are not permitted.

## Summary flow
```
writer.create(path, cap)
reader.open(path)

loop writer:
    wait for space
    reserve region (wrap marker if needed)
    write header (READY=0)
    copy payload
    set READY, advance write/commit
    signal data_avail

loop reader:
    wait for data (poll commit if needed)
    read header
    handle wrap marker or payload
    advance read, signal space_avail
```

Crashes are tolerated because headers encode state transitions and indices are monotonically increasing; any committed data can be replayed, partially written entries are ignored, and stalled peers generate explicit `IpcError::PeerStalled`.
