# ipc_ring

Memory-mapped SPSC ring buffer for high-performance inter-process communication on Unix systems.

## Performance

**MacBook Pro 2021 M1 Pro, 32GB RAM, 1TB NVMe**  
- `cap=64 MiB`: 14.6M msgs/s, 3.5 GiB/s (256B messages)  
  ```
  {"messages":1000000,"msg_size":256,"elapsed_sec":0.068287,"msgs_per_sec":14644147,"MiB_per_sec":3575.23}
  ```
- `cap=8 MiB`: 10.5M msgs/s, 2.5 GiB/s (256B messages)  
  ```
  {"messages":1000000,"msg_size":256,"elapsed_sec":0.095021,"msgs_per_sec":10523999,"MiB_per_sec":2569.34}
  ```
- `cap=1 MiB` (heavy wrap pressure, higher message count): 12.3M msgs/s, 3.0 GiB/s (256B messages)  
  ```
  {"messages":5000000,"msg_size":256,"elapsed_sec":0.407814,"msgs_per_sec":12260504,"MiB_per_sec":2993.29}
  ```
- `cap=1 MiB` (heavy wrap pressure, 500M messages): 12.6M msgs/s, 3.1 GiB/s (256B messages)  
  ```
  {"messages":500000000,"msg_size":256,"elapsed_sec":39.682753,"msgs_per_sec":12599932,"MiB_per_sec":3076.16}
  ```

### Benchmark Comparison

Tested against [ipmpsc](https://github.com/dicej/ipmpsc) (serialized MPSC ring buffer) using 1M messages, 256-byte payloads, **64MB ring capacity** on `/tmp` filesystem:

| Library | Messages/sec | Throughput | Ratio |
|---------|-------------|-----------|-------|
| **ipc_ring** | 14.6M | 3.5 GiB/s | **18.5x** |
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
# 64 MiB ring (matches comparison table)
cargo run --release --bin ipc_bench -- --ring /dev/shm/bench --cap 67108864 --messages 1000000 --msg-size 256

# 8 MiB ring (more wrap pressure)
cargo run --release --bin ipc_bench -- --ring /dev/shm/bench --cap 8388608 --messages 1000000 --msg-size 256

# 1 MiB ring (heavy wrap pressure, higher message count)
cargo run --release --bin ipc_bench -- --ring /dev/shm/bench --cap 1048576 --messages 5000000 --msg-size 256

# 1 MiB ring (heavy wrap pressure, 500M messages)
cargo run --release --bin ipc_bench -- --ring /dev/shm/bench --cap 1048576 --messages 500000000 --msg-size 256
```

## Technical

- **SPSC**: Single producer, single consumer only
- **Lock-free**: Atomic operations, no mutexes
- **mmap-backed**: Shared memory via file mapping
- **Power-of-two capacity**: Required for efficient masking
- **Unix-only**: Linux + macOS/other Unix (manual-reset events; futex-backed on Linux, spin/sleep poller on macOS)

## Requirements

- Unix system (Linux/macOS)  
- Rust 1.70+
