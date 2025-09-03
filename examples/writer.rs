//! cargo run --example writer -- /tmp/ipc_ring.demo 16777216
//! On Linux you can use /dev/shm/ipc_ring.demo to keep it purely in RAM.

use ipc_ring::RingWriter;
use std::env;

fn main() -> anyhow::Result<()> {
    let args: Vec<String> = env::args().collect();
    if args.len() < 3 {
        eprintln!("Usage: writer <ring_path> <cap_bytes_pow2>");
        std::process::exit(2);
    }
    let ring_path = &args[1];
    let cap: usize = args[2].parse().unwrap();

    let mut ring = RingWriter::create(ring_path, cap)?;
    let msgs: &[&[u8]] = &[b"hello", b"world", b"!"];
    for m in msgs {
        if ring.try_push(m).is_err() {
            ring.push(m, None)?;
        }
    }
    eprintln!("writer: sent 3 messages");
    Ok(())
}
