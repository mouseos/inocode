//! Release-mode measurements for the protocol.
//!
//! Measurements are not assertions: they print numbers for the report. Run them
//! single-threaded in a release build so the timing is not distorted by other
//! tests sharing the CPU:
//!
//! ```text
//! cargo test --release --test measure -- --ignored --nocapture --test-threads=1
//! ```

mod common;

use std::io;
use std::pin::Pin;
use std::task::{Context, Poll};
use std::time::Instant;

use common::*;
use tokio::io::AsyncWrite;

/// Receives a transfer without keeping it, so measurement reflects the
/// transport rather than hashing or copying work.
#[derive(Default)]
struct CountingSink {
    bytes: u64,
}

impl AsyncWrite for CountingSink {
    fn poll_write(
        mut self: Pin<&mut Self>,
        _cx: &mut Context<'_>,
        buf: &[u8],
    ) -> Poll<io::Result<usize>> {
        self.bytes += buf.len() as u64;
        Poll::Ready(Ok(buf.len()))
    }

    fn poll_flush(self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        Poll::Ready(Ok(()))
    }

    fn poll_shutdown(self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        Poll::Ready(Ok(()))
    }
}

/// Transfers `total` bytes in `chunk`-sized Binary frames and waits until the
/// receiver has read the End frame. Returns the wall time and byte count.
async fn transfer(total: u64, chunk: usize) -> f64 {
    let (client, server) = connect_pair(move |mut socket| async move {
        let (stream_id, start): (u64, TestMessage) = socket.0.read_json().await.unwrap();
        assert_eq!(start, start_message(stream_id, total));
        let mut sink = CountingSink::default();
        let bytes = socket.0.read_binary_to(stream_id, &mut sink).await.unwrap();
        assert_eq!(bytes, total);
        assert_eq!(sink.bytes, total);
        let ack = Ack {
            ok: true,
            stream_id,
            bytes,
        };
        socket.1.write_json(stream_id, &ack).await.unwrap();
    })
    .await;

    let (mut reader, mut writer) = client;
    writer
        .write_json(1, &start_message(1, total))
        .await
        .unwrap();
    let mut buffer = vec![0u8; chunk];
    for (index, slot) in buffer.iter_mut().enumerate() {
        *slot = (index % 251) as u8;
    }

    let started = Instant::now();
    let mut sent = 0u64;
    while sent < total {
        let take = (total - sent).min(chunk as u64) as usize;
        writer.write_binary_chunk(1, &buffer[..take]).await.unwrap();
        sent += take as u64;
    }
    writer.write_binary_end(1).await.unwrap();
    let (_, ack): (u64, Ack) = reader.read_json().await.unwrap();
    let elapsed = started.elapsed().as_secs_f64();
    assert!(ack.ok);
    assert_eq!(ack.bytes, total);
    server.await.unwrap();
    elapsed
}

fn mib(bytes: f64) -> f64 {
    bytes / (1024.0 * 1024.0)
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "measurement: run with --release --ignored --nocapture --test-threads=1"]
async fn small_json_round_trip() {
    with_timeout(async {
        const WARMUP: u64 = 200;
        const SAMPLES: u64 = 2000;
        let (client, server) = connect_pair(|mut socket| async move {
            loop {
                let (stream_id, message): (u64, TestMessage) =
                    match socket.0.read_json().await {
                        Ok(value) => value,
                        Err(_) => break,
                    };
                socket.1.write_json(stream_id, &message).await.unwrap();
            }
        })
        .await;

        let (mut reader, mut writer) = client;
        for round in 0..WARMUP {
            writer
                .write_json(1, &TestMessage::new(round))
                .await
                .unwrap();
            let _: (u64, TestMessage) = reader.read_json().await.unwrap();
        }

        let payload_len = serde_json::to_vec(&TestMessage::new(0)).unwrap().len();
        let mut samples = Vec::with_capacity(SAMPLES as usize);
        let started = Instant::now();
        for round in 0..SAMPLES {
            let begin = Instant::now();
            writer
                .write_json(1, &TestMessage::new(round))
                .await
                .unwrap();
            let (_, echoed): (u64, TestMessage) = reader.read_json().await.unwrap();
            samples.push(begin.elapsed().as_secs_f64());
            assert_eq!(echoed.id, round);
        }
        let total = started.elapsed().as_secs_f64();
        drop(writer);
        drop(reader);
        server.await.unwrap();

        samples.sort_by(|a, b| a.partial_cmp(b).unwrap());
        let mean = samples.iter().sum::<f64>() / samples.len() as f64;
        println!("\n== small JSON round trip ==");
        println!("json payload            : {payload_len} bytes (client->server), echo on the same TCP connection");
        println!("samples                 : {SAMPLES} (after {WARMUP} warmup round trips)");
        println!("mean                    : {:.2} us", mean * 1e6);
        println!("min                     : {:.2} us", samples[0] * 1e6);
        println!(
            "p50 / p95 / p99         : {:.2} / {:.2} / {:.2} us",
            samples[samples.len() / 2] * 1e6,
            samples[samples.len() * 95 / 100] * 1e6,
            samples[samples.len() * 99 / 100] * 1e6
        );
        println!(
            "max                     : {:.2} us",
            samples[samples.len() - 1] * 1e6
        );
        println!(
            "round trips per second  : {:.0}",
            SAMPLES as f64 / total
        );
    })
    .await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "measurement: run with --release --ignored --nocapture --test-threads=1"]
async fn large_binary_throughput() {
    with_timeout(async {
        const TOTAL: u64 = 512 * 1024 * 1024;
        transfer(4 * 1024 * 1024, 64 * 1024).await;

        const RUNS: usize = 5;
        println!("\n== large binary transfer (loopback TCP, payload generated on the fly) ==");
        println!(
            "transfer                : {} MiB per run, {RUNS} runs per chunk size",
            mib(TOTAL as f64)
        );
        for chunk in [8 * 1024, 64 * 1024, 256 * 1024, 1024 * 1024] {
            let mut rates = Vec::with_capacity(RUNS);
            for _ in 0..RUNS {
                rates.push(mib(TOTAL as f64) / transfer(TOTAL, chunk).await);
            }
            let run_rates = rates.clone();
            rates.sort_by(|a, b| a.partial_cmp(b).unwrap());
            let runs: Vec<String> = run_rates.iter().map(|rate| format!("{rate:.0}")).collect();
            println!(
                "chunk {chunk:>8} bytes    : median {:7.1} MiB/s  (best {:7.1}, worst {:7.1}, runs {} MiB/s, {} frames per run)",
                rates[RUNS / 2],
                rates[RUNS - 1],
                rates[0],
                runs.join(" / "),
                TOTAL as usize / chunk
            );
        }
    })
    .await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "measurement: run with --release --ignored --nocapture --test-threads=1"]
async fn streaming_memory_and_cpu() {
    with_timeout(async {
        let baseline = process_peak_working_set();
        let cpu_before = process_cpu_seconds();
        let wall_started = Instant::now();
        let small = transfer(4 * 1024 * 1024, 64 * 1024).await;
        let small_peak = process_peak_working_set();

        let cpu_before_large = process_cpu_seconds();
        let large = transfer(512 * 1024 * 1024, 64 * 1024).await;
        let large_peak = process_peak_working_set();
        let cpu_after = process_cpu_seconds();
        let wall = wall_started.elapsed().as_secs_f64();

        println!("\n== streaming memory and CPU (64 KiB frames) ==");
        println!(
            "process                 : pid {} on {} {}",
            std::process::id(),
            std::env::consts::OS,
            std::env::consts::ARCH
        );
        println!("peak working set before : {}", describe_bytes(baseline));
        println!(
            "after 4 MiB transfer    : {} ({:.3} s)",
            describe_bytes(small_peak),
            small
        );
        println!(
            "after 512 MiB transfer  : {} ({:.3} s)",
            describe_bytes(large_peak),
            large
        );
        println!(
            "peak growth 4 MiB->512 MiB: {}",
            match (small_peak, large_peak) {
                (Some(a), Some(b)) => describe_bytes(Some(b.saturating_sub(a))),
                _ => "unavailable".to_string(),
            }
        );
        println!(
            "CPU seconds for 512 MiB : {} (whole test process: client+server)",
            match (cpu_before_large, cpu_after) {
                (Some(a), Some(b)) => format!("{:.3}", b - a),
                _ => "unavailable".to_string(),
            }
        );
        println!(
            "CPU seconds / wall time : {}",
            match (cpu_before_large, cpu_after) {
                (Some(a), Some(b)) => format!("{:.1} % of one core", (b - a) / large * 100.0),
                _ => "unavailable".to_string(),
            }
        );
        println!(
            "total CPU seconds       : {}",
            match (cpu_before, cpu_after) {
                (Some(a), Some(b)) => format!("{:.3} over {:.3} s wall", b - a, wall),
                _ => "unavailable".to_string(),
            }
        );
    })
    .await;
}

fn describe_bytes(value: Option<u64>) -> String {
    match value {
        Some(bytes) => format!("{:.1} MiB", bytes as f64 / (1024.0 * 1024.0)),
        None => "unavailable".to_string(),
    }
}

#[cfg(windows)]
fn process_peak_working_set() -> Option<u64> {
    let output = std::process::Command::new("powershell")
        .args([
            "-NoProfile",
            "-Command",
            &format!("(Get-Process -Id {}).PeakWorkingSet64", std::process::id()),
        ])
        .output()
        .ok()?;
    String::from_utf8_lossy(&output.stdout).trim().parse().ok()
}

#[cfg(windows)]
fn process_cpu_seconds() -> Option<f64> {
    let output = std::process::Command::new("powershell")
        .args([
            "-NoProfile",
            "-Command",
            &format!("(Get-Process -Id {}).CPU", std::process::id()),
        ])
        .output()
        .ok()?;
    String::from_utf8_lossy(&output.stdout).trim().parse().ok()
}

#[cfg(target_os = "linux")]
fn process_peak_working_set() -> Option<u64> {
    let status = std::fs::read_to_string("/proc/self/status").ok()?;
    status.lines().find_map(|line| {
        let value = line.strip_prefix("VmHWM:")?.trim();
        let kilobytes: u64 = value.trim_end_matches("kB").trim().parse().ok()?;
        Some(kilobytes * 1024)
    })
}

#[cfg(target_os = "linux")]
fn process_cpu_seconds() -> Option<f64> {
    let ticks_per_second: f64 = std::process::Command::new("getconf")
        .arg("CLK_TCK")
        .output()
        .ok()
        .and_then(|output| String::from_utf8_lossy(&output.stdout).trim().parse().ok())
        .unwrap_or(100.0);
    let stat = std::fs::read_to_string("/proc/self/stat").ok()?;
    let fields: Vec<&str> = stat.rsplit_once(')')?.1.split_whitespace().collect();
    let user: u64 = fields.get(11)?.parse().ok()?;
    let system: u64 = fields.get(12)?.parse().ok()?;
    Some((user + system) as f64 / ticks_per_second)
}

#[cfg(not(any(windows, target_os = "linux")))]
fn process_peak_working_set() -> Option<u64> {
    None
}

#[cfg(not(any(windows, target_os = "linux")))]
fn process_cpu_seconds() -> Option<f64> {
    None
}
