# ipc_ring

Memory-mapped SPSC ring buffer for high-performance inter-process communication on Unix systems.

## Performance

**MacBook Pro 2021 M1 Pro, 32GB RAM, 1TB NVMe**: 16.3M msgs/s, 3.9 GiB/s (256B messages)

```
{"messages":1000000,"msg_size":256,"elapsed_sec":0.061254,"msgs_per_sec":16325531,"MiB_per_sec":3985.73}
```

### Benchmark Comparison

Tested against [ipmpsc](https://github.com/dicej/ipmpsc) (serialized MPSC ring buffer):

| Library | Messages/sec | Throughput | Ratio |
|---------|-------------|-----------|-------|
| **ipc_ring** | 16.3M | 3.9 GiB/s | **20.6x** |
| **ipmpsc** | 791K | 193 MiB/s | 1x |

**Use ipc_ring when:**
- Single producer/consumer pattern fits your use case
- Raw byte transfers (no serialization overhead needed)  
- Maximum throughput is critical
- Streaming pre-formatted data (JSON lines, log entries, etc.)

**Comparison limitations:**
- ipmpsc supports multiple producers; ipc_ring is SPSC only
- ipmpsc provides type-safe serialization; ipc_ring handles raw bytes
- ipmpsc is cross-platform; ipc_ring is Unix-only
- Different synchronization mechanisms (lock-free vs mutex-based)

**Perfect for:** Streaming JSON events to compression/storage processes, high-frequency logging, real-time data pipelines where serialization overhead is unwanted.

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