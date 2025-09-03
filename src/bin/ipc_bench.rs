use clap::{Parser, Subcommand};
use ipc_ring::{RingReader, RingWriter};
use std::io::Read;
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};
use std::{env, thread};

#[derive(Parser, Debug)]
#[command(name="ipc_bench")]
#[command(about="Throughput benchmark for ipc_ring (macOS + Linux)")]
struct Cli {
    /// Path to the ring file (/dev/shm on Linux, /tmp on macOS)
    #[arg(long, default_value = default_ring_path())]
    ring: String,

    /// Total messages to send
    #[arg(long, default_value_t = 5_000_000)]
    messages: usize,

    /// Message size in bytes (>= 8; first 8 bytes hold a seq id)
    #[arg(long, default_value_t = 256)]
    msg_size: usize,

    /// Ring capacity in bytes (power of two; driver rounds up if needed)
    #[arg(long, default_value_t = 64 * 1024 * 1024)]
    cap: usize,

    /// Mode: driver (default), reader, writer
    #[command(subcommand)]
    mode: Option<Mode>,
}

#[derive(Subcommand, Debug)]
enum Mode {
    Driver,
    Reader,
    Writer,
}

fn main() -> anyhow::Result<()> {
    let mut cli = Cli::parse();
    let mode = cli.mode.take().unwrap_or(Mode::Driver);

    match mode {
        Mode::Driver => run_driver(cli),
        Mode::Reader => run_reader(cli),
        Mode::Writer => run_writer(cli),
    }
}

fn run_driver(cli: Cli) -> anyhow::Result<()> {
    let cap = round_up_pow2(cli.cap);
    let exepath = env::current_exe()?;

    // Spawn reader first (it will loop trying to open until the writer creates the ring).
    let mut child_reader = Command::new(&exepath)
        .arg("--ring").arg(&cli.ring)
        .arg("--messages").arg(cli.messages.to_string())
        .arg("--msg-size").arg(cli.msg_size.to_string())
        .arg("--cap").arg(cap.to_string())
        .arg("reader")
        .stdout(Stdio::piped())
        .spawn()?;

    // Spawn writer second (creates the ring)
    let mut child_writer = Command::new(&exepath)
        .arg("--ring").arg(&cli.ring)
        .arg("--messages").arg(cli.messages.to_string())
        .arg("--msg-size").arg(cli.msg_size.to_string())
        .arg("--cap").arg(cap.to_string())
        .arg("writer")
        .stdout(Stdio::null())
        .spawn()?;

    // Wait for writer to exit (safety net; reader will usually exit last)
    let _ = child_writer.wait()?;

    // Read reader's JSON result
    let mut out = String::new();
    if let Some(mut rdr) = child_reader.stdout.take() {
        rdr.read_to_string(&mut out)?;
    }
    let status = child_reader.wait()?;

    if !status.success() {
        eprintln!("Reader failed; raw output:\n{}", out);
        anyhow::bail!("reader exited with status {:?}", status);
    }

    // Print reader's summary (already JSON + human string).
    print!("{}", out);
    Ok(())
}

fn run_writer(cli: Cli) -> anyhow::Result<()> {
    assert!(cli.msg_size >= 8, "msg_size must be >= 8 (u64 seq id)");
    let cap = round_up_pow2(cli.cap);

    // Create ring (writer owns creation)
    let mut ring = RingWriter::create(&cli.ring, cap)?;
    let mut buf = vec![0u8; cli.msg_size];

    // Pre-fill payload beyond seq id; not strictly necessary
    for i in 8..buf.len() { buf[i] = (i & 0xFF) as u8; }

    for i in 0..cli.messages {
        // write seq id LE in the first 8 bytes
        buf[0..8].copy_from_slice(&(i as u64).to_le_bytes());
        if ring.try_push(&buf).is_err() {
            ring.push(&buf, None)?;
        }
    }
    Ok(())
}

fn run_reader(cli: Cli) -> anyhow::Result<()> {
    assert!(cli.msg_size >= 8, "msg_size must be >= 8 (u64 seq id)");

    // Open ring with retry loop until writer has created it
    let mut reader = loop {
        match RingReader::open(&cli.ring) {
            Ok(r) => break r,
            Err(_) => thread::sleep(Duration::from_millis(10)),
        }
    };

    let mut buf = Vec::with_capacity(cli.msg_size);
    let mut count = 0usize;
    let mut t0 = None;

    // Receive N messages and verify sequence IDs
    while count < cli.messages {
        let _n = reader.pop(&mut buf, None)?; // blocking
        if buf.len() != cli.msg_size {
            // tolerate any size; for the bench we expect fixed size
        }
        let mut id_bytes = [0u8; 8];
        id_bytes.copy_from_slice(&buf[0..8]);
        let seq = u64::from_le_bytes(id_bytes);

        if count == 0 { t0 = Some(Instant::now()); }
        if (seq as usize) != count {
            eprintln!("Out of order or drop at count={} got seq={}", count, seq);
            // Keep going, but correctness anomaly will skew numbers.
        }
        count += 1;
    }

    let elapsed = t0.unwrap().elapsed();
    let secs = elapsed.as_secs_f64();
    let total_bytes = cli.messages as f64 * cli.msg_size as f64;
    let msgs_per_sec = cli.messages as f64 / secs;
    let mib_per_sec = total_bytes / (1024.0 * 1024.0) / secs;

    println!(
        "{{\"messages\":{},\"msg_size\":{},\"elapsed_sec\":{:.6},\"msgs_per_sec\":{:.0},\"MiB_per_sec\":{:.2}}}",
        cli.messages, cli.msg_size, secs, msgs_per_sec, mib_per_sec
    );
    println!(
        "ipc_ring throughput: {:.0} msgs/s, {:.2} MiB/s ({} msgs × {} B in {:.3}s)",
        msgs_per_sec, mib_per_sec, cli.messages, cli.msg_size, secs
    );

    Ok(())
}

fn round_up_pow2(mut x: usize) -> usize {
    if x < 2 { return 2; }
    if x.is_power_of_two() { return x; }
    x -= 1;
    x |= x >> 1;
    x |= x >> 2;
    x |= x >> 4;
    x |= x >> 8;
    x |= x >> 16;
    if usize::BITS == 64 { x |= x >> 32; }
    x + 1
}

const fn default_ring_path() -> &'static str {
    // A sensible default; override with --ring
    // On Linux prefer /dev/shm; on macOS /tmp works well
    "/tmp/ipc_ring.bench"
}

