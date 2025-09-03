//! cargo run --example reader -- /tmp/ipc_ring.demo

use ipc_ring::RingReader;
use std::{env, thread, time::Duration};

fn main() -> anyhow::Result<()> {
    let args: Vec<String> = env::args().collect();
    if args.len() < 2 {
        eprintln!("Usage: reader <ring_path>");
        std::process::exit(2);
    }
    let ring_path = &args[1];

    // Wait until the writer has created the ring.
    let mut reader = loop {
        match RingReader::open(ring_path) {
            Ok(r) => break r,
            Err(_) => {
                thread::sleep(Duration::from_millis(20));
            }
        }
    };

    let mut buf = Vec::new();
    for _ in 0..3 {
        let _n = reader.pop(&mut buf, None)?;
        println!("reader: {:?}", String::from_utf8_lossy(&buf));
    }
    Ok(())
}
