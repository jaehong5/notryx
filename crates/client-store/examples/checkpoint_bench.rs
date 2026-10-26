use notryx_client_store::ClientStore;
use serde_json::json;
use std::path::Path;
use std::time::{Duration, Instant};

fn proc_value(path: &str, key: &str) -> u64 {
    std::fs::read_to_string(path)
        .ok()
        .and_then(|text| {
            text.lines().find_map(|line| {
                line.strip_prefix(key)
                    .and_then(|value| value.split_whitespace().next())
                    .and_then(|value| value.parse().ok())
            })
        })
        .unwrap_or(0)
}

fn percentile(values: &[f64], fraction: f64) -> f64 {
    let mut values = values.to_vec();
    values.sort_by(f64::total_cmp);
    values[((values.len() as f64 * fraction).ceil() as usize).saturating_sub(1)]
}

fn file_bytes(path: &Path) -> u64 {
    path.metadata().map_or(0, |metadata| metadata.len())
}

async fn measure(
    size: usize,
    phase: &str,
) -> Result<serde_json::Value, Box<dyn std::error::Error>> {
    let directory = tempfile::tempdir()?;
    let path = directory.path().join("drafts.db");
    let mut store = ClientStore::open(&path).await?;
    let mut payload = vec![b'a'; size];
    let prefix = "# M0 합성 문서 🙂 e\u{301}\n\n".as_bytes();
    payload[..prefix.len()].copy_from_slice(prefix);
    let mut opened = store
        .open_document("document-a", &payload, "m0-base-a")
        .await?;
    let count = match phase {
        "typing" => 40,
        "paste" => 10,
        "switch" => 20,
        _ => unreachable!(),
    };
    let interval = Duration::from_millis(250);
    let before_writes = proc_value("/proc/self/io", "write_bytes:");
    let start = Instant::now();
    let mut durations = Vec::new();
    let mut max_queue_delay_ms = 0.0_f64;
    let mut min_available_kib = u64::MAX;
    let mut max_swap_used_kib = 0;
    let initial_swap_used_kib =
        proc_value("/proc/meminfo", "SwapTotal:") - proc_value("/proc/meminfo", "SwapFree:");

    for index in 0..count {
        if phase == "typing" {
            let due = start + interval * index;
            tokio::time::sleep_until(tokio::time::Instant::from_std(due)).await;
            max_queue_delay_ms = max_queue_delay_ms.max(due.elapsed().as_secs_f64() * 1000.0);
        }
        if phase == "paste" {
            payload.fill(b'a' + (index % 26) as u8);
            payload[..prefix.len()].copy_from_slice(prefix);
        } else {
            payload[size - 1] = b'a' + (index % 26) as u8;
        }
        let operation_start = Instant::now();
        store
            .save_checkpoint(
                &opened.document_id,
                &opened.edit_session_id,
                opened.generation + 1,
                &payload,
            )
            .await?;
        opened.generation += 1;
        if phase == "switch" {
            store.close_document(&opened.document_id, &opened.edit_session_id)?;
            let next = if opened.document_id == "document-a" {
                "document-b"
            } else {
                "document-a"
            };
            opened = store.open_document(next, &payload, "m0-base-a").await?;
        }
        durations.push(operation_start.elapsed().as_secs_f64() * 1000.0);
        min_available_kib = min_available_kib.min(proc_value("/proc/meminfo", "MemAvailable:"));
        let swap_used =
            proc_value("/proc/meminfo", "SwapTotal:") - proc_value("/proc/meminfo", "SwapFree:");
        max_swap_used_kib = max_swap_used_kib.max(swap_used);
    }
    if phase == "typing" {
        tokio::time::sleep_until(tokio::time::Instant::from_std(start + interval * count)).await;
    }
    let elapsed = start.elapsed().as_secs_f64();
    let writes = proc_value("/proc/self/io", "write_bytes:").saturating_sub(before_writes);
    let record = json!({
        "phase": phase,
        "document_bytes": size,
        "samples": count,
        "elapsed_seconds": elapsed,
        "latency_ms": {
            "p50": percentile(&durations, 0.5),
            "p95": percentile(&durations, 0.95),
            "max": durations.iter().copied().fold(0.0_f64, f64::max),
        },
        "max_queue_delay_ms": max_queue_delay_ms,
        "logical_payload_bytes": size as u64 * u64::from(count),
        "process_write_bytes": writes,
        "process_write_mib_per_second": writes as f64 / elapsed / 1_048_576.0,
        "database_bytes": file_bytes(&path),
        "wal_bytes": file_bytes(&path.with_file_name("drafts.db-wal")),
        "process_peak_rss_kib": proc_value("/proc/self/status", "VmHWM:"),
        "min_host_available_memory_kib": min_available_kib,
        "host_swap_growth_kib": max_swap_used_kib.saturating_sub(initial_swap_used_kib),
        "journal_mode": "wal",
        "synchronous": "FULL",
        "includes_ui": false,
    });
    store.close().await?;
    Ok(record)
}

#[tokio::main(flavor = "current_thread")]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let mut results = Vec::new();
    for size in [100 * 1024, 5 * 1024 * 1024] {
        for phase in ["typing", "paste", "switch"] {
            results.push(measure(size, phase).await?);
        }
    }
    println!(
        "{}",
        serde_json::to_string_pretty(
            &json!({"profile": "m0-full-checkpoint-v1", "results": results})
        )?
    );
    Ok(())
}
