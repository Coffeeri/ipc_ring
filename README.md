# ipc_ring

Memory-mapped SPSC ring buffer for high-performance inter-process communication on Unix systems.

## Performance

**M1 Pro (32GB RAM)**: 20.6M msgs/s, 5.0 GiB/s (256B messages)

```
{"messages":50000,"msg_size":256,"elapsed_sec":0.002429,"msgs_per_sec":20587069,"MiB_per_sec":5026.14}
```

## Build

```bash
cargo build --release
```

## Usage

**Writer creates ring:**
```rust
let mut writer = RingWriter::create("/tmp/ring", 64 * 1024 * 1024)?;
writer.push(b"data", None)?;  // None = block until space
```

**Reader opens existing ring:**
```rust  
let mut reader = RingReader::open("/tmp/ring")?;
let mut buf = Vec::new();
let n = reader.pop(&mut buf, None)?;  // None = block until data
```

## Benchmark

```bash
cargo run --release --bin ipc_bench -- --ring /dev/shm/bench --cap 67108864
```

## Technical

- **SPSC**: Single producer, single consumer only
- **Lock-free**: Atomic operations, no mutexes
- **mmap-backed**: Shared memory via file mapping
- **Power-of-two capacity**: Required for efficient masking
- **Unix-only**: Linux, macOS (uses raw_sync events)

## Requirements

- Unix system (Linux/macOS)  
- Rust 1.70+